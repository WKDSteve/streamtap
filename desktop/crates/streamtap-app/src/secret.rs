//! Secrets the app holds: the sink token and the bridge key (SRV-11).
//!
//! [`Token`] has no `PartialEq` (an `==` would compare in time that depends
//! on the content; the bridge key is proved with an HMAC instead,
//! `streamtap_host::auth`, and the sink compares its token itself) and its
//! `Debug` prints no value, so a `{:?}` in a log or an error message cannot
//! leak it. Serde reads and writes it as the plain string it always was
//! (`config.json`).

use std::fmt;

use serde::{Deserialize, Serialize};

#[derive(Clone, Serialize, Deserialize)]
#[serde(transparent)]
pub(crate) struct Token(String);

impl Token {
    /// A new random token (64 hex digits, see [`crate::config::random_token`]).
    pub(crate) fn generate() -> Self {
        Self(crate::config::random_token())
    }

    /// The value, for the places that must hand it on: the sink's stdin, the
    /// approved browsers, the settings view and the log scrubber.
    pub(crate) fn expose(&self) -> &str {
        &self.0
    }
}

impl From<String> for Token {
    fn from(s: String) -> Self {
        Self(s)
    }
}

impl From<&str> for Token {
    fn from(s: &str) -> Self {
        Self(s.to_owned())
    }
}

impl fmt::Debug for Token {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Token(hidden)")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECRET: &str = "0123456789abcdef0123456789abcdef";

    #[test]
    fn debug_hides_the_value() {
        let t = Token::from(SECRET);
        let shown = format!("{t:?} {:?}", Some(t.clone()));
        assert!(!shown.contains(SECRET), "{shown}");
        assert!(!shown.contains("0123"), "{shown}");
    }

    /// `config.json` keeps its format: the token is a plain JSON string.
    #[test]
    fn serde_reads_and_writes_a_plain_string() {
        let t: Token = serde_json::from_str(&format!("\"{SECRET}\"")).unwrap();
        assert_eq!(t.expose(), SECRET);
        assert_eq!(serde_json::to_string(&t).unwrap(), format!("\"{SECRET}\""));
        assert!(serde_json::from_str::<Token>("42").is_err());
    }

    #[test]
    fn generated_tokens_are_long_and_differ() {
        let (a, b) = (Token::generate(), Token::generate());
        assert_eq!(a.expose().len(), 64);
        assert_ne!(a.expose(), b.expose());
    }
}
