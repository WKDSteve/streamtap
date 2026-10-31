//! Who besides the user can read `config.json` (SRV-14).
//!
//! The file holds the sink token. The app sets no ACL of its own: the
//! profile's rights on `%APPDATA%` (the user, SYSTEM, Administrators) are
//! enough (decision of 2026-10-31). At start the app reads the file's DACL
//! and warns when one of the broad groups *Everyone*, *Users* or
//! *Authenticated Users* may read it, e.g. after the folder was moved or its
//! rights were widened by hand.
//!
//! The decision ([`broad_readers`]) works on plain [`AceView`]s, so it is
//! tested with made-up DACLs; [`read_dacl`] fills them from the real file
//! through the `windows-permissions` crate.

use std::path::Path;

use serde::Serialize;

/// The broad groups by SID, with the stable id the UI translates.
const BROAD_GROUPS: [(&str, &str); 3] = [
    ("S-1-1-0", "everyone"),
    ("S-1-5-32-545", "users"),
    ("S-1-5-11", "authenticated-users"),
];
const EVERYONE: &str = "S-1-1-0";

/// `FILE_READ_DATA`, `GENERIC_ALL` and `GENERIC_READ`: rights that include
/// reading the file's content.
const READ_RIGHTS: u32 = 0x0000_0001 | 0x1000_0000 | 0x8000_0000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AceKind {
    Allow,
    Deny,
    /// Audit, label and other entries that grant nothing.
    Other,
}

/// One entry of a DACL, as far as the decision needs it.
#[derive(Debug, Clone)]
pub(crate) struct AceView {
    pub(crate) kind: AceKind,
    /// Applies to children only, not to the file itself.
    pub(crate) inherit_only: bool,
    /// String form, `S-1-...`.
    pub(crate) sid: String,
    pub(crate) mask: u32,
}

/// What the start-up check found; the UI shows a warning for both variants.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(tag = "state", rename_all = "lowercase")]
pub(crate) enum ConfigAccess {
    /// These broad groups can read the file (stable ids, see `BROAD_GROUPS`).
    Exposed { groups: Vec<&'static str> },
    /// The rights could not be read.
    Unknown { error: String },
}

impl ConfigAccess {
    /// The line for the app's log.
    pub(crate) fn log_line(&self, path: &Path) -> String {
        match self {
            Self::Exposed { groups } => format!(
                "warning: {} can be read by {}; it holds the sink token",
                path.display(),
                groups.join(", ")
            ),
            Self::Unknown { error } => format!(
                "warning: cannot check who can read {}: {error}",
                path.display()
            ),
        }
    }
}

/// The broad groups that can read a file with this DACL, in `BROAD_GROUPS`
/// order. `None` is no DACL at all: everyone has full access.
///
/// Entries are evaluated in order like Windows does: an allow for a group
/// counts unless a deny of reading for that group, or for *Everyone*, came
/// before it. Membership beyond that is not resolved, so the answer errs on
/// the side of a warning.
pub(crate) fn broad_readers(dacl: Option<&[AceView]>) -> Vec<&'static str> {
    let Some(aces) = dacl else {
        return BROAD_GROUPS.iter().map(|(_, id)| *id).collect();
    };
    let mut denied: Vec<&str> = Vec::new();
    let mut readable: Vec<&'static str> = Vec::new();
    for ace in aces
        .iter()
        .filter(|a| !a.inherit_only && a.mask & READ_RIGHTS != 0)
    {
        match ace.kind {
            AceKind::Deny => denied.push(&ace.sid),
            AceKind::Allow => {
                let blocked = denied.iter().any(|d| *d == ace.sid || *d == EVERYONE);
                if let Some((_, id)) = BROAD_GROUPS.iter().find(|(sid, _)| *sid == ace.sid) {
                    if !blocked && !readable.contains(id) {
                        readable.push(id);
                    }
                }
            }
            AceKind::Other => {}
        }
    }
    BROAD_GROUPS
        .iter()
        .map(|(_, id)| *id)
        .filter(|id| readable.contains(id))
        .collect()
}

/// Checks `path`: `None` when no broad group can read it.
pub(crate) fn check(path: &Path) -> Option<ConfigAccess> {
    // The crate panics on a few malformed descriptors (e.g. a present but
    // null DACL); that must not end the app.
    let read = std::panic::catch_unwind(|| read_dacl(path))
        .unwrap_or_else(|_| Err("the security descriptor could not be read".into()));
    match read {
        Ok(dacl) => {
            let groups = broad_readers(dacl.as_deref());
            (!groups.is_empty()).then_some(ConfigAccess::Exposed { groups })
        }
        Err(error) => Some(ConfigAccess::Unknown { error }),
    }
}

/// The DACL of `path`; `Ok(None)` when it has none.
#[cfg(windows)]
pub(crate) fn read_dacl(path: &Path) -> Result<Option<Vec<AceView>>, String> {
    use windows_permissions::constants::{AceFlags, AceType, SeObjectType, SecurityInformation};
    use windows_permissions::wrappers::GetNamedSecurityInfo;

    let sd = GetNamedSecurityInfo(
        path.as_os_str(),
        SeObjectType::SE_FILE_OBJECT,
        SecurityInformation::Dacl,
    )
    .map_err(|e| e.to_string())?;
    let Some(acl) = sd.dacl() else {
        return Ok(None);
    };
    let mut aces = Vec::new();
    for i in 0..acl.len() {
        let Some(ace) = acl.get_ace(i) else {
            break;
        };
        let kind = match ace.ace_type() {
            AceType::ACCESS_ALLOWED_ACE_TYPE
            | AceType::ACCESS_ALLOWED_CALLBACK_ACE_TYPE
            | AceType::ACCESS_ALLOWED_CALLBACK_OBJECT_ACE_TYPE
            | AceType::ACCESS_ALLOWED_OBJECT_ACE_TYPE => AceKind::Allow,
            AceType::ACCESS_DENIED_ACE_TYPE
            | AceType::ACCESS_DENIED_CALLBACK_ACE_TYPE
            | AceType::ACCESS_DENIED_CALLBACK_OBJECT_ACE_TYPE
            | AceType::ACCESS_DENIED_OBJECT_ACE_TYPE => AceKind::Deny,
            _ => AceKind::Other,
        };
        aces.push(AceView {
            kind,
            inherit_only: ace.flags().contains(AceFlags::InheritOnly),
            sid: ace.sid().map(ToString::to_string).unwrap_or_default(),
            mask: ace.mask().bits(),
        });
    }
    Ok(Some(aces))
}

/// Other platforms have no DACL; the app is built for Windows only.
#[cfg(not(windows))]
pub(crate) fn read_dacl(_path: &Path) -> Result<Option<Vec<AceView>>, String> {
    Ok(Some(Vec::new()))
}

#[cfg(test)]
mod tests {
    use super::*;

    const ME: &str = "S-1-5-21-1111111111-2222222222-3333333333-1001";
    const SYSTEM: &str = "S-1-5-18";
    const ADMINS: &str = "S-1-5-32-544";
    const FULL: u32 = 0x001F_01FF;
    const READ: u32 = 0x0012_00A9;
    const NONE: [&str; 0] = [];

    fn ace(kind: AceKind, sid: &str, mask: u32) -> AceView {
        AceView {
            kind,
            inherit_only: false,
            sid: sid.into(),
            mask,
        }
    }

    fn allow(sid: &str, mask: u32) -> AceView {
        ace(AceKind::Allow, sid, mask)
    }

    /// The profile's default: the user, SYSTEM and Administrators.
    fn profile() -> Vec<AceView> {
        vec![allow(SYSTEM, FULL), allow(ADMINS, FULL), allow(ME, FULL)]
    }

    #[test]
    fn the_profile_default_reaches_no_broad_group() {
        assert_eq!(broad_readers(Some(&profile())), NONE);
        assert_eq!(broad_readers(Some(&[])), NONE, "an empty DACL: nobody");
    }

    #[test]
    fn each_broad_group_with_read_rights_is_found() {
        for (sid, id) in BROAD_GROUPS {
            for mask in [READ, FULL, 0x0000_0001, 0x8000_0000, 0x1000_0000] {
                let mut dacl = profile();
                dacl.push(allow(sid, mask));
                assert_eq!(broad_readers(Some(&dacl)), [id], "{sid} {mask:#x}");
            }
        }
        let mut all = profile();
        all.extend(BROAD_GROUPS.iter().rev().map(|(sid, _)| allow(sid, READ)));
        assert_eq!(
            broad_readers(Some(&all)),
            ["everyone", "users", "authenticated-users"]
        );
    }

    #[test]
    fn no_dacl_means_everyone() {
        assert_eq!(
            broad_readers(None),
            ["everyone", "users", "authenticated-users"]
        );
    }

    #[test]
    fn rights_without_reading_the_content_do_not_count() {
        // READ_CONTROL | SYNCHRONIZE | FILE_READ_ATTRIBUTES; FILE_WRITE_DATA.
        for mask in [0x0012_0080, 0x0000_0002, 0] {
            let mut dacl = profile();
            dacl.push(allow("S-1-1-0", mask));
            assert_eq!(broad_readers(Some(&dacl)), NONE, "{mask:#x}");
        }
    }

    #[test]
    fn inherit_only_and_non_access_entries_do_not_count() {
        let mut dacl = profile();
        dacl.push(AceView {
            inherit_only: true,
            ..allow("S-1-5-32-545", READ)
        });
        dacl.push(ace(AceKind::Other, "S-1-1-0", FULL));
        assert_eq!(broad_readers(Some(&dacl)), NONE);
    }

    #[test]
    fn a_deny_before_the_allow_blocks_it_and_one_after_does_not() {
        let users = "S-1-5-32-545";
        let before = vec![ace(AceKind::Deny, users, READ), allow(users, READ)];
        assert_eq!(broad_readers(Some(&before)), NONE);
        let after = vec![allow(users, READ), ace(AceKind::Deny, users, READ)];
        assert_eq!(broad_readers(Some(&after)), ["users"]);
        // A deny of Everyone covers the other groups; one of another group not.
        let everyone = vec![
            ace(AceKind::Deny, "S-1-1-0", READ),
            allow(users, READ),
            allow("S-1-5-11", READ),
        ];
        assert_eq!(broad_readers(Some(&everyone)), NONE);
        let other = vec![ace(AceKind::Deny, ME, READ), allow(users, READ)];
        assert_eq!(broad_readers(Some(&other)), ["users"]);
        // A deny of writing only blocks no reading.
        let write_only = vec![ace(AceKind::Deny, users, 0x0000_0002), allow(users, READ)];
        assert_eq!(broad_readers(Some(&write_only)), ["users"]);
    }

    #[test]
    fn log_lines_name_the_file_and_the_groups() {
        let path = Path::new("C:/x/config.json");
        let exposed = ConfigAccess::Exposed {
            groups: vec!["everyone", "users"],
        };
        let line = exposed.log_line(path);
        assert!(
            line.contains("config.json") && line.contains("everyone, users"),
            "{line}"
        );
        let unknown = ConfigAccess::Unknown {
            error: "denied".into(),
        };
        assert!(unknown.log_line(path).ends_with("denied"));
        let json = serde_json::to_value(&exposed).unwrap();
        assert_eq!(
            json,
            serde_json::json!({"state": "exposed", "groups": ["everyone", "users"]})
        );
    }

    /// The real file system: a fresh file in a fresh folder is checked, then
    /// Everyone is granted read with `icacls` (by SID, independent of the
    /// Windows language) and the check finds it.
    #[cfg(windows)]
    #[test]
    fn the_real_dacl_of_a_file_is_read_and_a_grant_to_everyone_is_found() {
        let dir = std::env::temp_dir().join(format!("streamtap-acl-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("config.json");
        std::fs::write(&file, b"{}").unwrap();
        let dacl = read_dacl(&file).unwrap().expect("a file has a DACL");
        assert!(!dacl.is_empty());
        assert!(dacl.iter().all(|a| a.sid.starts_with("S-1-")), "{dacl:?}");
        let before = check(&file);
        assert!(
            !matches!(&before, Some(ConfigAccess::Exposed { groups }) if groups.contains(&"everyone")),
            "{before:?}"
        );
        let status = std::process::Command::new("icacls")
            .arg(&file)
            .args(["/grant", "*S-1-1-0:(R)"])
            .stdout(std::process::Stdio::null())
            .status()
            .unwrap();
        assert!(status.success());
        match check(&file) {
            Some(ConfigAccess::Exposed { groups }) => assert!(groups.contains(&"everyone")),
            other => panic!("Everyone may read the file now: {other:?}"),
        }
        let missing = check(&dir.join("missing.json"));
        assert!(
            matches!(missing, Some(ConfigAccess::Unknown { .. })),
            "{missing:?}"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
