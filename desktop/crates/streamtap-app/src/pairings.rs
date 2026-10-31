//! The browsers the user approved (`pairings.json` in the app config dir).
//!
//! An approval names the extension's origin and the random instance id the
//! extension keeps per browser profile, so "once per Chrome profile" holds and
//! another profile asks again. The file is small and bounded. A file that
//! cannot be read approves nobody (fail closed); the next approval replaces it.

use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Approved browsers kept at most; the user revokes one to make room.
pub(crate) const MAX_APPROVALS: usize = 32;
const MAX_FILE_BYTES: u64 = 64 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Approval {
    pub(crate) origin: String,
    pub(crate) instance: String,
    /// What the browser called itself when it was approved (display only).
    pub(crate) browser: String,
    /// Unix seconds.
    pub(crate) approved_at: u64,
}

#[derive(Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileContent {
    approvals: Vec<Approval>,
}

pub(crate) struct Store {
    path: PathBuf,
    list: Vec<Approval>,
    /// Why the stored file could not be used.
    pub(crate) error: Option<String>,
}

impl Store {
    pub(crate) fn load(path: &Path) -> Self {
        let mut store = Self {
            path: path.to_path_buf(),
            list: Vec::new(),
            error: None,
        };
        match fs::File::open(path) {
            Ok(f) => {
                let mut text = String::new();
                let read = f.take(MAX_FILE_BYTES).read_to_string(&mut text);
                let parsed = read
                    .map_err(|e| e.to_string())
                    .and_then(|_| {
                        serde_json::from_str::<FileContent>(&text).map_err(|e| e.to_string())
                    })
                    .and_then(|c| {
                        if c.approvals.len() > MAX_APPROVALS {
                            Err("too many approvals".to_string())
                        } else {
                            Ok(c.approvals)
                        }
                    });
                match parsed {
                    Ok(list) => store.list = list,
                    Err(e) => {
                        store.error = Some(format!(
                            "{} cannot be used, nobody is approved: {e}",
                            path.display()
                        ));
                    }
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => store.error = Some(format!("cannot open {}: {e}", path.display())),
        }
        store
    }

    pub(crate) fn contains(&self, origin: &str, instance: &str) -> bool {
        self.list
            .iter()
            .any(|a| a.origin == origin && a.instance == instance)
    }

    pub(crate) fn list(&self) -> &[Approval] {
        &self.list
    }

    /// Adds `approval` (or refreshes the same browser) and saves. The list in
    /// memory only changes when the file was written.
    pub(crate) fn add(&mut self, approval: Approval) -> Result<(), String> {
        let mut next: Vec<Approval> = self
            .list
            .iter()
            .filter(|a| !(a.origin == approval.origin && a.instance == approval.instance))
            .cloned()
            .collect();
        if next.len() >= MAX_APPROVALS {
            return Err("too many approved browsers; revoke one first".into());
        }
        next.push(approval);
        self.save(&next)?;
        self.list = next;
        self.error = None;
        Ok(())
    }

    /// Removes every approval of `instance`; whether there was one.
    pub(crate) fn remove(&mut self, instance: &str) -> Result<bool, String> {
        let next: Vec<Approval> = self
            .list
            .iter()
            .filter(|a| a.instance != instance)
            .cloned()
            .collect();
        if next.len() == self.list.len() {
            return Ok(false);
        }
        self.save(&next)?;
        self.list = next;
        Ok(true)
    }

    fn save(&self, list: &[Approval]) -> Result<(), String> {
        let dir = self.path.parent().ok_or("pairings path has no directory")?;
        fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
        let text = serde_json::to_vec_pretty(&FileContent {
            approvals: list.to_vec(),
        })
        .map_err(|e| e.to_string())?;
        let tmp = self.path.with_extension("json.tmp");
        fs::write(&tmp, text).map_err(|e| format!("cannot write {}: {e}", tmp.display()))?;
        fs::rename(&tmp, &self.path)
            .map_err(|e| format!("cannot replace {}: {e}", self.path.display()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("streamtap-pair-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    fn approval(instance: &str) -> Approval {
        Approval {
            origin: "chrome-extension://x/".into(),
            instance: instance.into(),
            browser: "chrome".into(),
            approved_at: 1,
        }
    }

    #[test]
    fn approvals_survive_a_restart_and_are_per_instance() {
        let d = tmp("persist");
        let path = d.join("pairings.json");
        let mut s = Store::load(&path);
        assert!(!s.contains("chrome-extension://x/", "profile-a"));
        s.add(approval("profile-a")).unwrap();
        let again = Store::load(&path);
        assert!(again.contains("chrome-extension://x/", "profile-a"));
        assert!(
            !again.contains("chrome-extension://x/", "profile-b"),
            "another profile asks again"
        );
        assert!(
            !again.contains("chrome-extension://y/", "profile-a"),
            "another origin is another browser"
        );
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn revoking_removes_the_approval_for_good() {
        let d = tmp("revoke");
        let path = d.join("pairings.json");
        let mut s = Store::load(&path);
        s.add(approval("profile-a")).unwrap();
        s.add(approval("profile-b")).unwrap();
        assert!(s.remove("profile-a").unwrap());
        assert!(!s.remove("profile-a").unwrap(), "second time: nothing left");
        let again = Store::load(&path);
        assert_eq!(again.list().len(), 1);
        assert!(!again.contains("chrome-extension://x/", "profile-a"));
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn a_broken_file_approves_nobody() {
        let d = tmp("broken");
        let path = d.join("pairings.json");
        for bad in [
            "{ not json".to_string(),
            r#"{"approvals":[{"origin":"o","instance":"i","browser":"b","approved_at":1,"admin":true}]}"#
                .to_string(),
            "x".repeat(70 * 1024),
        ] {
            fs::write(&path, &bad).unwrap();
            let s = Store::load(&path);
            assert!(s.list().is_empty(), "{bad:.30}");
            assert!(s.error.is_some());
        }
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn the_list_is_bounded_and_a_repeat_does_not_grow_it() {
        let d = tmp("bound");
        let mut s = Store::load(&d.join("pairings.json"));
        for i in 0..MAX_APPROVALS {
            s.add(approval(&format!("profile-{i}"))).unwrap();
        }
        assert!(s.add(approval("one-too-many")).is_err());
        s.add(approval("profile-0")).unwrap();
        assert_eq!(s.list().len(), MAX_APPROVALS);
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn a_failed_save_changes_nothing_in_memory() {
        let d = tmp("fail");
        // The file's place is a directory: the rename cannot succeed.
        let path = d.join("pairings.json");
        fs::create_dir(&path).unwrap();
        let mut s = Store::load(&path);
        assert!(s.add(approval("profile-a")).is_err());
        assert!(!s.contains("chrome-extension://x/", "profile-a"));
        fs::remove_dir_all(&d).unwrap();
    }
}
