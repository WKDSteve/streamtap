//! The app's settings (`config.json` in the app config dir): sink port,
//! the shared token, the captures directory and the UI preferences
//! (language and theme).

use std::collections::hash_map::RandomState;
use std::fmt::Write as _;
use std::fs;
use std::hash::{BuildHasher, Hasher};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Deserializer, Serialize};

use crate::secret::Token;

/// The extension popup's default port.
pub(crate) const DEFAULT_PORT: u16 = 17650;
const MIN_PORT: u16 = 1024;
const MAX_CONFIG_BYTES: u64 = 64 * 1024;
const MIN_TOKEN_LEN: usize = 16;
const MAX_TOKEN_LEN: usize = 256;

/// No `PartialEq`: the token has none (see [`Token`]).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct Settings {
    pub(crate) port: u16,
    pub(crate) token: Token,
    pub(crate) captures_dir: PathBuf,
    /// UI language; missing in configs written before it existed.
    #[serde(default, deserialize_with = "lenient")]
    pub(crate) language: Language,
    /// UI theme; missing in configs written before it existed.
    #[serde(default, deserialize_with = "lenient")]
    pub(crate) theme: Theme,
}

/// The UI language: `system` follows the webview's language (German for
/// `de*`, else English).
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Language {
    #[default]
    System,
    De,
    En,
}

impl FromStr for Language {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, String> {
        match s {
            "system" => Ok(Self::System),
            "de" => Ok(Self::De),
            "en" => Ok(Self::En),
            _ => Err("language must be one of: system, de, en".into()),
        }
    }
}

/// The UI theme: `system` follows the operating system's light or dark mode.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Theme {
    #[default]
    System,
    Light,
    Dark,
}

impl FromStr for Theme {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, String> {
        match s {
            "system" => Ok(Self::System),
            "light" => Ok(Self::Light),
            "dark" => Ok(Self::Dark),
            _ => Err("theme must be one of: system, light, dark".into()),
        }
    }
}

/// Reads a UI preference without failing the whole config: an unknown or
/// mistyped value (a newer app's option, a hand edit) gives the default, so
/// the token and the other settings still load.
fn lenient<'de, D, T>(d: D) -> Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: FromStr + Default,
{
    let value = serde_json::Value::deserialize(d)?;
    Ok(value
        .as_str()
        .and_then(|s| s.parse().ok())
        .unwrap_or_default())
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
            token: Token::generate(),
            captures_dir: data_dir.join("captures"),
            language: Language::default(),
            theme: Theme::default(),
        }
    }

    /// Checks what the sink would reject or what would be unsafe to hand it.
    pub(crate) fn validate(&self) -> Result<(), String> {
        if self.port < MIN_PORT {
            return Err(format!("port must be {MIN_PORT} or higher"));
        }
        let token = self.token.expose();
        if token.len() < MIN_TOKEN_LEN
            || token.len() > MAX_TOKEN_LEN
            || !token.chars().all(|c| c.is_ascii_graphic())
        {
            return Err(format!(
                "token must be {MIN_TOKEN_LEN} to {MAX_TOKEN_LEN} visible ASCII characters"
            ));
        }
        // The app hands the token over on stdin; a token that starts with `-`
        // would still be read as a flag by `serve --token <token>` (manual use).
        if token.starts_with('-') {
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

    /// Field by field, the token in constant time (tests only: the app never
    /// needs to compare whole settings).
    #[cfg(test)]
    pub(crate) fn same_as(&self, other: &Self) -> bool {
        self.port == other.port
            && self.token.matches(other.token.expose().as_bytes())
            && self.captures_dir == other.captures_dir
            && self.language == other.language
            && self.theme == other.theme
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
            language: Language::System,
            theme: Theme::System,
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
        assert!(first.same_as(&second), "the token must survive a restart");
        fs::write(&path, b"{ not json").unwrap();
        assert!(
            load_or_create(&path, &dir).is_err(),
            "a broken file is not replaced"
        );
        assert_eq!(fs::read(&path).unwrap(), b"{ not json");
        fs::remove_dir_all(&dir).unwrap();
    }

    fn with_prefs(language: &str, theme: &str) -> String {
        format!(
            r#"{{"port":17651,"token":"0123456789abcdef0123","captures_dir":"C:/x/captures","language":{language},"theme":{theme}}}"#
        )
    }

    #[test]
    fn a_config_without_ui_preferences_still_loads_with_system_defaults() {
        let old = r#"{"port":17650,"token":"0123456789abcdef0123","captures_dir":"C:/x/captures"}"#;
        let s: Settings = serde_json::from_str(old).unwrap();
        assert_eq!(s.language, Language::System);
        assert_eq!(s.theme, Theme::System);
        assert_eq!(s.token.expose(), "0123456789abcdef0123");
    }

    /// The token type changes nothing in `config.json`: an old file loads,
    /// and a saved one has the token as a plain string under the same key.
    #[test]
    fn the_config_file_format_is_unchanged_by_the_token_type() {
        let old = r#"{"port":17650,"token":"0123456789abcdef0123","captures_dir":"C:/x/captures","language":"de","theme":"dark"}"#;
        let s: Settings = serde_json::from_str(old).unwrap();
        let back: serde_json::Value = serde_json::to_value(&s).unwrap();
        let want: serde_json::Value = serde_json::from_str(old).unwrap();
        assert_eq!(back, want);
        assert!(!format!("{s:?}").contains("0123456789abcdef0123"));
    }

    #[test]
    fn ui_preferences_round_trip_as_lowercase_names() {
        let mut s = base();
        s.language = Language::De;
        s.theme = Theme::Dark;
        let json = serde_json::to_value(&s).unwrap();
        assert_eq!(json["language"], "de");
        assert_eq!(json["theme"], "dark");
        let back: Settings = serde_json::from_value(json).unwrap();
        assert!(back.same_as(&s));
        let s: Settings = serde_json::from_str(&with_prefs(r#""en""#, r#""light""#)).unwrap();
        assert_eq!((s.language, s.theme), (Language::En, Theme::Light));
    }

    #[test]
    fn unknown_ui_preferences_in_the_file_fall_back_without_losing_the_rest() {
        for (lang, theme) in [
            (r#""fr""#, r#""sepia""#),
            ("42", "null"),
            (r#""DE""#, r#""Dark""#),
            ("{}", "[]"),
        ] {
            let s: Settings = serde_json::from_str(&with_prefs(lang, theme)).unwrap();
            assert_eq!(s.language, Language::System, "{lang}");
            assert_eq!(s.theme, Theme::System, "{theme}");
            assert_eq!(s.port, 17651);
            assert_eq!(s.token.expose(), "0123456789abcdef0123");
        }
    }

    #[test]
    fn ui_preference_names_are_parsed_strictly() {
        assert_eq!("system".parse::<Language>(), Ok(Language::System));
        assert_eq!("de".parse::<Language>(), Ok(Language::De));
        assert_eq!("en".parse::<Language>(), Ok(Language::En));
        assert_eq!("system".parse::<Theme>(), Ok(Theme::System));
        assert_eq!("light".parse::<Theme>(), Ok(Theme::Light));
        assert_eq!("dark".parse::<Theme>(), Ok(Theme::Dark));
        for bad in ["", "DE", "de-DE", " en", "english", "fr"] {
            assert!(bad.parse::<Language>().is_err(), "{bad:?}");
        }
        for bad in ["", "Dark", "auto", "light ", "hell"] {
            assert!(bad.parse::<Theme>().is_err(), "{bad:?}");
        }
    }

    #[test]
    fn ui_preferences_survive_a_save_and_load() {
        let dir = std::env::temp_dir().join(format!("streamtap-app-cfg-ui-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let path = dir.join("config.json");
        let mut s = load_or_create(&path, &dir).unwrap();
        assert_eq!((s.language, s.theme), (Language::System, Theme::System));
        s.language = Language::En;
        s.theme = Theme::Light;
        save(&path, &s).unwrap();
        assert!(load_or_create(&path, &dir).unwrap().same_as(&s));
        fs::remove_dir_all(&dir).unwrap();
    }
}
