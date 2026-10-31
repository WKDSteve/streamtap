//! Secrets the app holds: the sink token and the bridge key (SRV-11).
//!
//! [`Token`] has no `PartialEq` (an `==` would compare in time that depends
//! on the content; [`Token::matches`] does not) and its `Debug` prints no
//! value, so a `{:?}` in a log or an error message cannot leak it. Serde reads
//! and writes it as the plain string it always was (`config.json`).

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

    /// Whether `other` is this token, in time that depends on the lengths only.
    pub(crate) fn matches(&self, other: &[u8]) -> bool {
        constant_time_eq(self.0.as_bytes(), other)
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

/// Compares two byte strings in time that depends on their length only.
pub(crate) fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let diff = a
        .iter()
        .zip(b)
        .fold(0u8, |acc, (x, y)| std::hint::black_box(acc | (x ^ y)));
    diff == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECRET: &str = "0123456789abcdef0123456789abcdef";

    #[test]
    fn bytes_compare_by_content_and_length() {
        assert!(constant_time_eq(b"abcd", b"abcd"));
        assert!(!constant_time_eq(b"abcd", b"abce"));
        assert!(!constant_time_eq(b"abcd", b"abc"));
        assert!(!constant_time_eq(b"", b"a"));
        assert!(constant_time_eq(b"", b""));
    }

    #[test]
    fn a_token_matches_its_value_only() {
        let t = Token::from(SECRET);
        assert!(t.matches(SECRET.as_bytes()));
        assert!(!t.matches(SECRET.to_uppercase().as_bytes()));
        assert!(!t.matches(&SECRET.as_bytes()[1..]));
        assert!(!t.matches(b""));
    }

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
        assert!(!a.matches(b.expose().as_bytes()));
    }
}
