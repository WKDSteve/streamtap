//! Registers the native messaging host for a browser, per user.
//!
//! A browser finds a native messaging host through a registry key
//! (`HKCU\Software\<vendor>\<browser>\NativeMessagingHosts\<name>`) whose
//! default value is the path of a manifest file. No administrator rights are
//! needed for the current-user hive. The manifest names the host executable
//! and `allowed_origins`: exactly the streamtap extension, no wildcard.
//!
//! The registry is behind a trait so that the logic is tested against a fake;
//! the real one (`WinRegistry`) takes a root key path, which the one test
//! that touches the registry points at a throw-away key.

use std::fs;
use std::path::{Path, PathBuf};

use serde::Serialize;
use streamtap_host::proto::{ALLOWED_ORIGIN, HOST_NAME};

/// A Chromium based browser the host can be registered for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Browser {
    Chrome,
    Edge,
    Brave,
}

impl Browser {
    pub(crate) const ALL: [Self; 3] = [Self::Chrome, Self::Edge, Self::Brave];

    pub(crate) fn parse(text: &str) -> Result<Self, String> {
        Self::ALL
            .into_iter()
            .find(|b| b.id() == text)
            .ok_or_else(|| "unknown browser".to_string())
    }

    pub(crate) fn id(self) -> &'static str {
        match self {
            Self::Chrome => "chrome",
            Self::Edge => "edge",
            Self::Brave => "brave",
        }
    }

    /// The key under HKCU that the browser reads its hosts from.
    fn hosts_key(self) -> &'static str {
        match self {
            Self::Chrome => r"Software\Google\Chrome\NativeMessagingHosts",
            Self::Edge => r"Software\Microsoft\Edge\NativeMessagingHosts",
            Self::Brave => r"Software\BraveSoftware\Brave-Browser\NativeMessagingHosts",
        }
    }

    fn key_path(self) -> String {
        format!(r"{}\{HOST_NAME}", self.hosts_key())
    }
}

/// What a browser's key says.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum State {
    NotRegistered,
    Registered,
    /// A key exists but does not lead to this app's current manifest (an
    /// older install, the manifest file is gone, or it names the host of
    /// another copy of the app): register again.
    Stale,
}

pub(crate) trait Registry {
    /// The default value of `key`, or none when the key does not exist.
    fn get_default(&self, key: &str) -> Result<Option<String>, String>;
    /// Creates `key` when needed and sets its default value.
    fn set_default(&mut self, key: &str, value: &str) -> Result<(), String>;
    /// Removes `key` with everything below it; a missing key is fine.
    fn delete_key(&mut self, key: &str) -> Result<(), String>;
}

/// Where the manifest of `browser` lives: in the app's own directory.
pub(crate) fn manifest_path(dir: &Path, browser: Browser) -> PathBuf {
    dir.join("native-messaging")
        .join(format!("{}.json", browser.id()))
}

/// The host manifest: one allowed origin, the extension's, written out in full.
pub(crate) fn manifest_json(host_exe: &Path) -> Result<String, String> {
    let manifest = serde_json::json!({
        "name": HOST_NAME,
        "description": "streamtap: pairs the browser extension with the streamtap app",
        "path": host_exe.to_str().ok_or("the host path is not valid text")?,
        "type": "stdio",
        "allowed_origins": [ALLOWED_ORIGIN],
    });
    serde_json::to_string_pretty(&manifest).map_err(|e| e.to_string())
}

/// `\\?\C:\x` becomes `C:\x` and `\\?\UNC\server\share\x` becomes
/// `\\server\share\x`: browsers do not all read the verbatim form.
fn plain(path: &Path) -> PathBuf {
    let text = path.to_string_lossy();
    if let Some(unc) = text.strip_prefix(r"\\?\UNC\") {
        return PathBuf::from(format!(r"\\{unc}"));
    }
    text.strip_prefix(r"\\?\")
        .map_or_else(|| path.to_path_buf(), PathBuf::from)
}

/// Writes `text` to `path` through a temporary file and a rename.
fn write_atomic(path: &Path, text: &[u8]) -> Result<(), String> {
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, text).map_err(|e| format!("cannot write {}: {e}", tmp.display()))?;
    fs::rename(&tmp, path).map_err(|e| {
        let _ = fs::remove_file(&tmp);
        format!("cannot replace {}: {e}", path.display())
    })
}

/// Whether the manifest at `manifest` names `host_exe` as its program (letter
/// case aside, as Windows paths). An unreadable manifest names nothing.
fn manifest_names(manifest: &Path, host_exe: &Path) -> bool {
    let Ok(text) = fs::read_to_string(manifest) else {
        return false;
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) else {
        return false;
    };
    value["path"]
        .as_str()
        .is_some_and(|p| p.eq_ignore_ascii_case(&plain(host_exe).to_string_lossy()))
}

/// `host_exe` is the host next to this app, when there is one: a manifest that
/// names another host (the checkout's, after the app was installed elsewhere)
/// is `Stale`, so *Register again* points the browser at this app's host.
pub(crate) fn state(
    reg: &impl Registry,
    dir: &Path,
    browser: Browser,
    host_exe: Option<&Path>,
) -> Result<State, String> {
    let manifest = manifest_path(dir, browser);
    match reg.get_default(&browser.key_path())? {
        None => Ok(State::NotRegistered),
        Some(v)
            if Path::new(&v) == manifest
                && manifest.is_file()
                && host_exe.is_none_or(|h| manifest_names(&manifest, h)) =>
        {
            Ok(State::Registered)
        }
        Some(_) => Ok(State::Stale),
    }
}

/// Writes the manifest (temporary file, then rename), then points the
/// browser's key at it. When that fails the earlier state stays: a manifest
/// that was there before is put back as it was, a new one is removed.
pub(crate) fn register(
    reg: &mut impl Registry,
    dir: &Path,
    browser: Browser,
    host_exe: &Path,
) -> Result<(), String> {
    let host_exe = plain(host_exe);
    if !host_exe.is_absolute() || !host_exe.is_file() {
        return Err(
            "the host program was not found next to the app (build it: scripts/build.ps1)".into(),
        );
    }
    let manifest = manifest_path(dir, browser);
    let folder = manifest.parent().ok_or("manifest path has no directory")?;
    fs::create_dir_all(folder).map_err(|e| format!("cannot create {}: {e}", folder.display()))?;
    let value = manifest
        .to_str()
        .ok_or("the manifest path is not valid text")?;
    let text = manifest_json(&host_exe)?;
    let before = match fs::read(&manifest) {
        Ok(bytes) => Some(bytes),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(format!("cannot read {}: {e}", manifest.display())),
    };
    write_atomic(&manifest, text.as_bytes())?;
    if let Err(e) = reg.set_default(&browser.key_path(), value) {
        let undo = match &before {
            Some(old) => write_atomic(&manifest, old),
            None => fs::remove_file(&manifest).map_err(|e| e.to_string()),
        };
        return Err(match undo {
            Ok(()) => e,
            Err(u) => format!("{e}; the earlier manifest could not be put back: {u}"),
        });
    }
    Ok(())
}

/// Removes the browser's key and the manifest file.
pub(crate) fn unregister(
    reg: &mut impl Registry,
    dir: &Path,
    browser: Browser,
) -> Result<(), String> {
    reg.delete_key(&browser.key_path())?;
    match fs::remove_file(manifest_path(dir, browser)) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(format!("cannot remove the manifest: {e}")),
    }
}

/// A registry that lives in a map; for tests.
#[cfg(test)]
#[derive(Default)]
pub(crate) struct FakeRegistry {
    pub(crate) keys: std::collections::HashMap<String, String>,
}

#[cfg(test)]
impl Registry for FakeRegistry {
    fn get_default(&self, key: &str) -> Result<Option<String>, String> {
        Ok(self.keys.get(key).cloned())
    }
    fn set_default(&mut self, key: &str, value: &str) -> Result<(), String> {
        self.keys.insert(key.to_string(), value.to_string());
        Ok(())
    }
    fn delete_key(&mut self, key: &str) -> Result<(), String> {
        self.keys.remove(key);
        Ok(())
    }
}

/// The current user's registry hive.
#[cfg(windows)]
pub(crate) struct WinRegistry {
    /// Prepended to every key; empty for the real keys, a throw-away path in tests.
    root: String,
}

#[cfg(windows)]
impl WinRegistry {
    pub(crate) fn real() -> Self {
        Self {
            root: String::new(),
        }
    }

    #[cfg(test)]
    fn under(root: &str) -> Self {
        Self { root: root.into() }
    }

    fn path(&self, key: &str) -> String {
        if self.root.is_empty() {
            key.to_string()
        } else {
            format!(r"{}\{key}", self.root)
        }
    }
}

#[cfg(windows)]
impl Registry for WinRegistry {
    fn get_default(&self, key: &str) -> Result<Option<String>, String> {
        use winreg::enums::HKEY_CURRENT_USER;
        use winreg::RegKey;
        let hkcu = RegKey::predef(HKEY_CURRENT_USER);
        match hkcu.open_subkey(self.path(key)) {
            Ok(k) => match k.get_value::<String, _>("") {
                Ok(v) => Ok(Some(v)),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Some(String::new())),
                Err(e) => Err(format!("cannot read the registry key: {e}")),
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(format!("cannot open the registry key: {e}")),
        }
    }

    fn set_default(&mut self, key: &str, value: &str) -> Result<(), String> {
        use winreg::enums::HKEY_CURRENT_USER;
        use winreg::RegKey;
        let hkcu = RegKey::predef(HKEY_CURRENT_USER);
        let (k, _) = hkcu
            .create_subkey(self.path(key))
            .map_err(|e| format!("cannot create the registry key: {e}"))?;
        k.set_value("", &value)
            .map_err(|e| format!("cannot write the registry key: {e}"))
    }

    fn delete_key(&mut self, key: &str) -> Result<(), String> {
        use winreg::enums::HKEY_CURRENT_USER;
        use winreg::RegKey;
        let hkcu = RegKey::predef(HKEY_CURRENT_USER);
        match hkcu.delete_subkey_all(self.path(key)) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(format!("cannot remove the registry key: {e}")),
        }
    }
}

/// No registry off Windows (the app ships for Windows only).
#[cfg(not(windows))]
pub(crate) struct WinRegistry;

#[cfg(not(windows))]
impl WinRegistry {
    pub(crate) fn real() -> Self {
        Self
    }
}

#[cfg(not(windows))]
impl Registry for WinRegistry {
    fn get_default(&self, _: &str) -> Result<Option<String>, String> {
        Err("registration needs Windows".into())
    }
    fn set_default(&mut self, _: &str, _: &str) -> Result<(), String> {
        Err("registration needs Windows".into())
    }
    fn delete_key(&mut self, _: &str) -> Result<(), String> {
        Err("registration needs Windows".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("streamtap-reg-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    fn fake_host(d: &Path) -> PathBuf {
        let p = d.join("streamtap-host.exe");
        fs::write(&p, b"x").unwrap();
        p
    }

    #[test]
    fn each_browser_has_its_own_key() {
        let keys: Vec<String> = Browser::ALL.iter().map(|b| b.key_path()).collect();
        assert!(keys[0].starts_with(r"Software\Google\Chrome\NativeMessagingHosts\"));
        assert!(keys[1].starts_with(r"Software\Microsoft\Edge\NativeMessagingHosts\"));
        assert!(keys[2].starts_with(r"Software\BraveSoftware\Brave-Browser\NativeMessagingHosts\"));
        assert!(keys.iter().all(|k| k.ends_with(HOST_NAME)));
        assert_eq!(Browser::parse("edge").unwrap(), Browser::Edge);
        assert!(Browser::parse("firefox").is_err());
        assert!(Browser::parse("Chrome").is_err());
    }

    #[test]
    fn the_manifest_allows_the_extension_and_nothing_else() {
        let json = manifest_json(Path::new(r"C:\Apps\streamtap\streamtap-host.exe")).unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["name"], HOST_NAME);
        assert_eq!(v["type"], "stdio");
        assert_eq!(v["path"], r"C:\Apps\streamtap\streamtap-host.exe");
        let origins = v["allowed_origins"].as_array().unwrap();
        assert_eq!(origins.len(), 1);
        assert_eq!(
            origins[0],
            "chrome-extension://oggpddakdbffbjhkjmcakidgmhpgakia/"
        );
        assert!(!json.contains('*'), "no wildcard anywhere");
    }

    #[test]
    fn register_writes_the_manifest_and_the_key_and_unregister_removes_both() {
        let d = dir("roundtrip");
        let host = fake_host(&d);
        let mut reg = FakeRegistry::default();
        assert_eq!(
            state(&reg, &d, Browser::Chrome, Some(&host)).unwrap(),
            State::NotRegistered
        );
        register(&mut reg, &d, Browser::Chrome, &host).unwrap();
        let manifest = manifest_path(&d, Browser::Chrome);
        assert!(manifest.is_file());
        assert_eq!(
            reg.keys
                .get(&Browser::Chrome.key_path())
                .map(String::as_str),
            manifest.to_str()
        );
        assert_eq!(
            state(&reg, &d, Browser::Chrome, Some(&host)).unwrap(),
            State::Registered
        );
        // Only the chosen browser.
        assert_eq!(
            state(&reg, &d, Browser::Edge, Some(&host)).unwrap(),
            State::NotRegistered
        );
        assert_eq!(reg.keys.len(), 1);

        unregister(&mut reg, &d, Browser::Chrome).unwrap();
        assert!(reg.keys.is_empty());
        assert!(!manifest.exists());
        unregister(&mut reg, &d, Browser::Chrome).unwrap();
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn unregistering_one_browser_keeps_the_others() {
        let d = dir("others");
        let host = fake_host(&d);
        let mut reg = FakeRegistry::default();
        for b in [Browser::Chrome, Browser::Brave] {
            register(&mut reg, &d, b, &host).unwrap();
        }
        unregister(&mut reg, &d, Browser::Chrome).unwrap();
        assert_eq!(
            state(&reg, &d, Browser::Brave, Some(&host)).unwrap(),
            State::Registered
        );
        assert!(manifest_path(&d, Browser::Brave).is_file());
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn a_missing_or_relative_host_is_refused_and_nothing_is_written() {
        let d = dir("nohost");
        let mut reg = FakeRegistry::default();
        assert!(register(&mut reg, &d, Browser::Edge, &d.join("missing.exe")).is_err());
        assert!(register(&mut reg, &d, Browser::Edge, Path::new("streamtap-host.exe")).is_err());
        assert!(reg.keys.is_empty());
        assert!(!manifest_path(&d, Browser::Edge).exists());
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn a_key_that_leads_elsewhere_or_to_a_missing_manifest_is_stale() {
        let d = dir("stale");
        let host = fake_host(&d);
        let mut reg = FakeRegistry::default();
        reg.keys
            .insert(Browser::Chrome.key_path(), r"C:\old\manifest.json".into());
        assert_eq!(
            state(&reg, &d, Browser::Chrome, Some(&host)).unwrap(),
            State::Stale
        );
        register(&mut reg, &d, Browser::Chrome, &host).unwrap();
        fs::remove_file(manifest_path(&d, Browser::Chrome)).unwrap();
        assert_eq!(
            state(&reg, &d, Browser::Chrome, Some(&host)).unwrap(),
            State::Stale
        );
        fs::remove_dir_all(&d).unwrap();
    }

    /// After an install the key and the manifest are this app's, but the
    /// manifest still names the checkout's host: that is not "registered".
    #[test]
    fn a_manifest_that_names_another_host_is_stale() {
        let d = dir("otherhost");
        let checkout = d.join("checkout");
        let installed = d.join("installed");
        fs::create_dir_all(&checkout).unwrap();
        fs::create_dir_all(&installed).unwrap();
        let old_host = fake_host(&checkout);
        let new_host = fake_host(&installed);
        let mut reg = FakeRegistry::default();
        register(&mut reg, &d, Browser::Edge, &old_host).unwrap();
        assert_eq!(
            state(&reg, &d, Browser::Edge, Some(&old_host)).unwrap(),
            State::Registered
        );
        assert_eq!(
            state(&reg, &d, Browser::Edge, Some(&new_host)).unwrap(),
            State::Stale
        );
        // Without a host next to the app only the key and the file count.
        assert_eq!(
            state(&reg, &d, Browser::Edge, None).unwrap(),
            State::Registered
        );
        // Letter case of a Windows path does not make it another host.
        let upper = PathBuf::from(old_host.to_string_lossy().to_uppercase());
        assert_eq!(
            state(&reg, &d, Browser::Edge, Some(&upper)).unwrap(),
            State::Registered
        );
        // A manifest that cannot be read names no host.
        fs::write(manifest_path(&d, Browser::Edge), b"{ not json").unwrap();
        assert_eq!(
            state(&reg, &d, Browser::Edge, Some(&old_host)).unwrap(),
            State::Stale
        );
        // Register again points it at the new host.
        register(&mut reg, &d, Browser::Edge, &new_host).unwrap();
        assert_eq!(
            state(&reg, &d, Browser::Edge, Some(&new_host)).unwrap(),
            State::Registered
        );
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn a_failing_registry_leaves_no_manifest_behind() {
        struct Failing;
        impl Registry for Failing {
            fn get_default(&self, _: &str) -> Result<Option<String>, String> {
                Ok(None)
            }
            fn set_default(&mut self, _: &str, _: &str) -> Result<(), String> {
                Err("denied".into())
            }
            fn delete_key(&mut self, _: &str) -> Result<(), String> {
                Ok(())
            }
        }
        let d = dir("failing");
        let host = fake_host(&d);
        assert!(register(&mut Failing, &d, Browser::Chrome, &host).is_err());
        assert!(!manifest_path(&d, Browser::Chrome).exists());
        fs::remove_dir_all(&d).unwrap();
    }

    /// A registry whose writes fail but which still shows the key an earlier
    /// registration set.
    struct ReadOnly(FakeRegistry);
    impl Registry for ReadOnly {
        fn get_default(&self, key: &str) -> Result<Option<String>, String> {
            self.0.get_default(key)
        }
        fn set_default(&mut self, _: &str, _: &str) -> Result<(), String> {
            Err("denied".into())
        }
        fn delete_key(&mut self, _: &str) -> Result<(), String> {
            Err("denied".into())
        }
    }

    #[test]
    fn a_failed_register_keeps_a_working_registration() {
        let d = dir("keep");
        let host = fake_host(&d);
        let mut reg = FakeRegistry::default();
        register(&mut reg, &d, Browser::Edge, &host).unwrap();
        let manifest = manifest_path(&d, Browser::Edge);
        let before = fs::read(&manifest).unwrap();
        // Register again for another host location; the key cannot be written.
        let other = d.join("other");
        fs::create_dir_all(&other).unwrap();
        let mut ro = ReadOnly(reg);
        assert!(register(&mut ro, &d, Browser::Edge, &fake_host(&other)).is_err());
        assert_eq!(
            fs::read(&manifest).unwrap(),
            before,
            "the old manifest is back"
        );
        assert_eq!(
            state(&ro, &d, Browser::Edge, Some(&host)).unwrap(),
            State::Registered
        );
        assert!(
            !manifest.with_extension("json.tmp").exists(),
            "no temporary file is left"
        );
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn a_verbatim_unc_path_becomes_a_plain_unc_path() {
        assert_eq!(
            plain(Path::new(r"\\?\UNC\server\share\apps\h.exe")),
            PathBuf::from(r"\\server\share\apps\h.exe")
        );
        assert_eq!(
            plain(Path::new(r"\\server\share\apps\h.exe")),
            PathBuf::from(r"\\server\share\apps\h.exe")
        );
        #[cfg(windows)]
        assert!(plain(Path::new(r"\\?\UNC\server\share\apps\h.exe")).is_absolute());
        let json = manifest_json(&plain(Path::new(r"\\?\UNC\server\share\h.exe"))).unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["path"], r"\\server\share\h.exe");
    }

    #[test]
    fn the_verbatim_prefix_is_dropped() {
        assert_eq!(
            plain(Path::new(r"\\?\C:\Apps\h.exe")),
            PathBuf::from(r"C:\Apps\h.exe")
        );
        assert_eq!(
            plain(Path::new(r"C:\Apps\h.exe")),
            PathBuf::from(r"C:\Apps\h.exe")
        );
    }

    /// The only test that touches the registry: a throw-away key of this
    /// process under HKCU (never the browsers' keys), removed afterwards.
    #[cfg(windows)]
    #[test]
    fn the_real_registry_round_trips_under_a_throwaway_root() {
        let root = format!(r"Software\streamtap-test-{}", std::process::id());
        let d = dir("winreg");
        let host = fake_host(&d);
        let mut reg = WinRegistry::under(&root);
        register(&mut reg, &d, Browser::Brave, &host).unwrap();
        assert_eq!(
            state(&reg, &d, Browser::Brave, Some(&host)).unwrap(),
            State::Registered
        );
        assert_eq!(
            state(&reg, &d, Browser::Chrome, Some(&host)).unwrap(),
            State::NotRegistered
        );
        unregister(&mut reg, &d, Browser::Brave).unwrap();
        assert_eq!(
            state(&reg, &d, Browser::Brave, Some(&host)).unwrap(),
            State::NotRegistered
        );
        // The throw-away root itself goes too.
        let mut cleanup = WinRegistry::under("");
        cleanup.delete_key(&root).unwrap();
        fs::remove_dir_all(&d).unwrap();
    }
}
