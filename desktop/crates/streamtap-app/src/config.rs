//! The app's settings (`config.json` in the app config dir): sink port,
//! the shared token and the captures directory.

use std::collections::hash_map::RandomState;
use std::fmt::Write as _;
use std::fs;
use std::hash::{BuildHasher, Hasher};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

/// The extension popup's default port.
pub(crate) const DEFAULT_PORT: u16 = 17650;
const MIN_PORT: u16 = 1024;
const MAX_CONFIG_BYTES: u64 = 64 * 1024;
const MIN_TOKEN_LEN: usize = 16;
const MAX_TOKEN_LEN: usize = 256;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct Settings {
    pub(crate) port: u16,
    pub(crate) token: String,
    pub(crate) captures_dir: PathBuf,
}

/// A random token of 64 hex digits.
///
/// No dependency for this: `RandomState` keys come from the operating
/// system's random source (a per-process seed), and the time and a counter
/// are mixed in so two calls never repeat. 128 bits of the secret are the
/// hasher keys; the token only keeps other local processes out of a
/// loopback socket, it is not a long-term credential.
pub(crate) fn random_token() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let mut out = String::with_capacity(64);
    for i in 0u64..4 {
        let mut h = RandomState::new().build_hasher();
        h.write_u64(i);
        h.write_u128(nanos);
        h.write_u32(std::process::id());
        let _ = write!(out, "{:016x}", h.finish());
    }
    out
}

impl Settings {
    pub(crate) fn defaults(data_dir: &Path) -> Self {
        Self {
            port: DEFAULT_PORT,
            token: random_token(),
            captures_dir: data_dir.join("captures"),
        }
    }

    /// Checks what the sink would reject or what would be unsafe to hand it.
    pub(crate) fn validate(&self) -> Result<(), String> {
        if self.port < MIN_PORT {
            return Err(format!("port must be {MIN_PORT} or higher"));
        }
        if self.token.len() < MIN_TOKEN_LEN
            || self.token.len() > MAX_TOKEN_LEN
            || !self.token.chars().all(|c| c.is_ascii_graphic())
        {
            return Err(format!(
                "token must be {MIN_TOKEN_LEN} to {MAX_TOKEN_LEN} visible ASCII characters"
            ));
        }
        // A token that starts with `--` would be read as a flag by the sink.
        if self.token.starts_with('-') {
            return Err("token must not start with '-'".into());
        }
        if !self.captures_dir.is_absolute() {
            return Err("captures directory must be an absolute path".into());
        }
        if self.captures_dir.parent().is_none() {
            return Err("captures directory must not be a drive or filesystem root".into());
        }
        if self.captures_dir.to_string_lossy().starts_with('-') {
            return Err("captures directory must not start with '-'".into());
        }
        Ok(())
    }
}

/// Loads `path`; a missing file gives the defaults (which are saved), a file
/// that does not parse or validate is an error (the user's token is not
/// silently replaced).
pub(crate) fn load_or_create(path: &Path, data_dir: &Path) -> Result<Settings, String> {
    match fs::File::open(path) {
        Ok(f) => {
            let mut text = String::new();
            f.take(MAX_CONFIG_BYTES)
                .read_to_string(&mut text)
                .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
            let s: Settings = serde_json::from_str(&text)
                .map_err(|e| format!("{} is not valid: {e}", path.display()))?;
            s.validate()
                .map_err(|e| format!("{}: {e}", path.display()))?;
            Ok(s)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let s = Settings::defaults(data_dir);
            save(path, &s)?;
            Ok(s)
        }
        Err(e) => Err(format!("cannot open {}: {e}", path.display())),
    }
}

/// Writes `s` atomically (temporary file, then rename).
pub(crate) fn save(path: &Path, s: &Settings) -> Result<(), String> {
    let dir = path.parent().ok_or("config path has no directory")?;
    fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    let text = serde_json::to_vec_pretty(s).map_err(|e| e.to_string())?;
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, text).map_err(|e| format!("cannot write {}: {e}", tmp.display()))?;
    fs::rename(&tmp, path).map_err(|e| format!("cannot replace {}: {e}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> Settings {
        let dir = std::env::temp_dir().join("streamtap-app-cfg-captures");
        Settings {
            port: 17650,
            token: "0123456789abcdef0123".into(),
            captures_dir: dir,
        }
    }

    #[test]
    fn tokens_are_long_hex_and_differ() {
        let a = random_token();
        let b = random_token();
        assert_eq!(a.len(), 64);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, b);
    }

    #[test]
    fn validate_accepts_the_base_and_refuses_bad_values() {
        assert!(base().validate().is_ok());
        let mut s = base();
        s.port = 80;
        assert!(s.validate().is_err(), "privileged port");
        let mut s = base();
        s.token = "short".into();
        assert!(s.validate().is_err(), "short token");
        let mut s = base();
        s.token = "--port-looks-like-flag".into();
        assert!(s.validate().is_err(), "token read as a flag by the sink");
        let mut s = base();
        s.token = "has space in it 12345".into();
        assert!(s.validate().is_err(), "whitespace in token");
        let mut s = base();
        s.captures_dir = PathBuf::from("relative/dir");
        assert!(s.validate().is_err(), "relative captures dir");
        let mut s = base();
        s.captures_dir = std::env::temp_dir()
            .ancestors()
            .last()
            .unwrap()
            .to_path_buf();
        assert!(s.validate().is_err(), "filesystem root");
    }

    #[test]
    fn load_creates_defaults_once_and_keeps_them() {
        let dir = std::env::temp_dir().join(format!("streamtap-app-cfg-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let path = dir.join("config.json");
        let first = load_or_create(&path, &dir).unwrap();
        let second = load_or_create(&path, &dir).unwrap();
        assert_eq!(first, second, "the token must survive a restart");
        fs::write(&path, b"{ not json").unwrap();
        assert!(
            load_or_create(&path, &dir).is_err(),
            "a broken file is not replaced"
        );
        assert_eq!(fs::read(&path).unwrap(), b"{ not json");
        fs::remove_dir_all(&dir).unwrap();
    }
}
