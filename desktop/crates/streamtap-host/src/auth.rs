//! The bridge's handshake: host and app prove to each other that they know the
//! key of `bridge.json`. The key itself never crosses the socket.
//!
//! `bridge.json` names a port and a key; only a process that can read the
//! app's config directory knows the key. When the app is gone (a crash, a
//! stale file) another local process can listen on that port. A host that
//! sent the key as its first line would hand it to that process, and a host
//! that asked for nothing in return would take that process for the app and
//! pass its "credentials" on to the extension. So both ends prove the key
//! over fresh nonces, each before it is told anything:
//!
//! ```text
//! host -> app   streamtap-bridge/2 <host nonce>
//! app  -> host  <app nonce> <HMAC-SHA256(key, "streamtap-bridge/2\napp\n<host nonce>\n<app nonce>")>
//! host -> app   <HMAC-SHA256(key, "streamtap-bridge/2\nhost\n<host nonce>\n<app nonce>")>
//! ```
//!
//! Nonces are 32 random bytes as hex, proofs 32 bytes as hex, one line each.
//! The host checks the app's proof before it sends its own; the app checks
//! the host's before the connection gets a slot. The role is part of what is
//! signed, so neither proof can be played back as the other, and each proof
//! covers the nonce of the end that checks it, so one from an earlier
//! connection proves nothing. Proofs are compared in constant time.
//!
//! Nothing here sets a time limit: the caller bounds the reads and writes of
//! the stream it hands in.

use std::io::{self, Read, Write};

use hmac::{Hmac, KeyInit, Mac};
use sha2::Sha256;

/// First word of the host's first line: the bridge protocol and its version.
/// An app that reads anything else there closes the connection.
pub const GREETING: &str = "streamtap-bridge/2";
/// Hex digits of a nonce and of a proof (32 bytes each).
pub const HEX_LEN: usize = 64;
/// Longest line of the handshake: the app's answer (nonce, space, proof), with room for a line break.
pub const MAX_AUTH_LINE: usize = 2 * HEX_LEN + 1 + 2;

/// Which end a proof speaks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// The native messaging host (the end that connects).
    Host,
    /// The app (the end that listens).
    App,
}

impl Role {
    fn label(self) -> &'static str {
        match self {
            Role::Host => "host",
            Role::App => "app",
        }
    }
}

/// A fresh nonce: 32 bytes from the system's random source, as hex.
///
/// # Errors
/// When the system gives no randomness; there is no fallback.
pub fn nonce() -> io::Result<String> {
    let mut bytes = [0u8; HEX_LEN / 2];
    getrandom::fill(&mut bytes).map_err(|e| io::Error::other(format!("no randomness: {e}")))?;
    Ok(to_hex(&bytes))
}

fn to_hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(out, "{b:02x}");
    }
    out
}

/// `text` as the 32 bytes it spells in lower-case hex; nothing else passes
/// (no upper case, no other length), so a nonce has exactly one spelling.
fn from_hex(text: &str) -> Option<[u8; HEX_LEN / 2]> {
    let digits = text.as_bytes();
    if digits.len() != HEX_LEN {
        return None;
    }
    let value = |d: u8| match d {
        b'0'..=b'9' => Some(d - b'0'),
        b'a'..=b'f' => Some(d - b'a' + 10),
        _ => None,
    };
    let mut out = [0u8; HEX_LEN / 2];
    for (byte, [high, low]) in out.iter_mut().zip(digits.as_chunks::<2>().0) {
        *byte = (value(*high)? << 4) | value(*low)?;
    }
    Some(out)
}

/// Whether `text` is a nonce as this protocol writes one.
#[must_use]
pub fn is_nonce(text: &str) -> bool {
    from_hex(text).is_some()
}

fn mac(key: &str, role: Role, host_nonce: &str, app_nonce: &str) -> Hmac<Sha256> {
    // HMAC takes a key of any length; `new_from_slice` has no failing case for it.
    let mut mac =
        Hmac::<Sha256>::new_from_slice(key.as_bytes()).expect("HMAC takes a key of any length");
    for part in [
        GREETING,
        "\n",
        role.label(),
        "\n",
        host_nonce,
        "\n",
        app_nonce,
    ] {
        mac.update(part.as_bytes());
    }
    mac
}

/// The proof of `role` for this pair of nonces, as hex.
#[must_use]
pub fn proof(key: &str, role: Role, host_nonce: &str, app_nonce: &str) -> String {
    to_hex(
        &mac(key, role, host_nonce, app_nonce)
            .finalize()
            .into_bytes(),
    )
}

/// Whether `claimed` is the proof of `role` for this pair of nonces. Compared
/// in constant time; anything that is not 64 lower-case hex digits is no proof.
#[must_use]
pub fn proves(key: &str, role: Role, host_nonce: &str, app_nonce: &str, claimed: &str) -> bool {
    from_hex(claimed).is_some_and(|bytes| {
        mac(key, role, host_nonce, app_nonce)
            .verify_slice(&bytes)
            .is_ok()
    })
}

/// The host's first line for `host_nonce`.
#[must_use]
pub fn greeting_line(host_nonce: &str) -> String {
    format!("{GREETING} {host_nonce}\n")
}

/// The host nonce of a first line (without its line break), when it is this
/// protocol's greeting with a well-formed nonce.
#[must_use]
pub fn parse_greeting(line: &[u8]) -> Option<&str> {
    let text = std::str::from_utf8(line).ok()?;
    let nonce = text.strip_prefix(GREETING)?.strip_prefix(' ')?;
    is_nonce(nonce).then_some(nonce)
}

/// The app's answer to a greeting: a nonce of its own and its proof.
#[must_use]
pub fn answer_line(key: &str, host_nonce: &str, app_nonce: &str) -> String {
    format!(
        "{app_nonce} {}\n",
        proof(key, Role::App, host_nonce, app_nonce)
    )
}

/// Reads one line of at most `max` bytes, byte by byte: nothing beyond the
/// line break is taken from the stream, so whoever reads after the handshake
/// misses nothing.
fn read_short_line(stream: &mut impl Read, max: usize) -> io::Result<Vec<u8>> {
    let mut line = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        if stream.read(&mut byte)? == 0 {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        if byte[0] == b'\n' {
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            return Ok(line);
        }
        if line.len() >= max {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "handshake line longer than the limit",
            ));
        }
        line.push(byte[0]);
    }
}

fn not_the_app() -> io::Error {
    io::Error::new(
        io::ErrorKind::PermissionDenied,
        "the listener on the bridge port did not prove the bridge key",
    )
}

/// The host's side of the handshake on a fresh connection to the bridge port.
/// Sends a nonce, checks that the listener proves the key over it, and only
/// then proves the key in return. On an error nothing derived from the key
/// has been sent.
///
/// # Errors
/// When the stream fails or ends, or the listener's answer is not a
/// well-formed nonce with the right proof (`PermissionDenied`: whoever
/// listens there is not the app that wrote this key).
pub fn host_handshake<S: Read + Write>(stream: &mut S, key: &str) -> io::Result<()> {
    let host_nonce = nonce()?;
    stream.write_all(greeting_line(&host_nonce).as_bytes())?;
    stream.flush()?;
    let line = read_short_line(stream, MAX_AUTH_LINE)?;
    let text = std::str::from_utf8(&line).map_err(|_| not_the_app())?;
    let (app_nonce, app_proof) = text.split_once(' ').ok_or_else(not_the_app)?;
    if !is_nonce(app_nonce) || !proves(key, Role::App, &host_nonce, app_nonce, app_proof) {
        return Err(not_the_app());
    }
    let mut line = proof(key, Role::Host, &host_nonce, app_nonce);
    line.push('\n');
    stream.write_all(line.as_bytes())?;
    stream.flush()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    const KEY: &str = "0f1e2d3c4b5a69788796a5b4c3d2e1f00f1e2d3c4b5a69788796a5b4c3d2e1f0";
    const OTHER_KEY: &str = "ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff";
    const H: &str = "1111111111111111111111111111111111111111111111111111111111111111";
    const A: &str = "2222222222222222222222222222222222222222222222222222222222222222";

    /// A stream that plays `answer` back and records what is written.
    struct Wire {
        answer: Cursor<Vec<u8>>,
        written: Vec<u8>,
    }

    impl Wire {
        fn answering(answer: impl Into<Vec<u8>>) -> Self {
            Self {
                answer: Cursor::new(answer.into()),
                written: Vec::new(),
            }
        }

        fn lines(&self) -> Vec<String> {
            String::from_utf8(self.written.clone())
                .expect("text")
                .lines()
                .map(str::to_owned)
                .collect()
        }
    }

    impl Read for Wire {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            self.answer.read(buf)
        }
    }

    impl Write for Wire {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.written.extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    /// RFC 4231, test case 2: the MAC is HMAC-SHA256, not something home-made.
    #[test]
    fn the_mac_is_hmac_sha256() {
        let mut mac = Hmac::<Sha256>::new_from_slice(b"Jefe").unwrap();
        mac.update(b"what do ya want for nothing?");
        assert_eq!(
            to_hex(&mac.finalize().into_bytes()),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
    }

    #[test]
    fn a_proof_holds_for_its_key_role_and_nonces_only() {
        let p = proof(KEY, Role::App, H, A);
        assert_eq!(p.len(), HEX_LEN);
        assert!(proves(KEY, Role::App, H, A, &p));
        assert!(!proves(OTHER_KEY, Role::App, H, A, &p), "another key");
        assert!(!proves(KEY, Role::Host, H, A, &p), "the other role");
        assert!(!proves(KEY, Role::App, A, H, &p), "the nonces swapped");
        assert!(!proves(KEY, Role::App, A, A, &p), "another host nonce");
        assert!(!proves(KEY, Role::App, H, H, &p), "another app nonce");
        assert_ne!(p, proof(KEY, Role::Host, H, A));
        // Not a proof at all.
        for bad in [
            "",
            &p[..63],
            &p.to_uppercase(),
            &format!("{p}0"),
            &format!(" {}", &p[1..]),
        ] {
            assert!(!proves(KEY, Role::App, H, A, bad), "{bad:?}");
        }
    }

    #[test]
    fn nonces_are_fresh_and_have_one_spelling() {
        let (a, b) = (nonce().unwrap(), nonce().unwrap());
        assert!(is_nonce(&a) && is_nonce(&b));
        assert_ne!(a, b);
        assert!(!is_nonce(
            "ABCDEF1111111111111111111111111111111111111111111111111111111111"
        ));
        assert!(!is_nonce(&a[..62]));
        assert!(!is_nonce(""));
    }

    #[test]
    fn the_greeting_is_parsed_strictly() {
        assert_eq!(
            parse_greeting(greeting_line(H).trim_end().as_bytes()),
            Some(H)
        );
        for bad in [
            format!("streamtap-bridge/1 {H}"),
            format!("streamtap-bridge/2  {H}"),
            format!("streamtap-bridge/2{H}"),
            format!("streamtap-bridge/2 {H} "),
            format!("streamtap-bridge/2 {}", &H[..63]),
            format!(" streamtap-bridge/2 {H}"),
            KEY.to_owned(),
            String::new(),
        ] {
            assert_eq!(parse_greeting(bad.as_bytes()), None, "{bad:?}");
        }
        assert_eq!(parse_greeting(&[0xff, 0xfe]), None);
    }

    /// The whole exchange against an app that knows the key: three lines, and
    /// the key is in none of them.
    #[test]
    fn host_and_app_prove_the_key_to_each_other() {
        // The host's nonce is random: the app's side answers what the host wrote.
        struct App {
            key: &'static str,
            inbox: Vec<u8>,
            outbox: Cursor<Vec<u8>>,
            host_nonce: String,
            host_proof_ok: Option<bool>,
        }
        impl Read for App {
            fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
                self.outbox.read(buf)
            }
        }
        impl Write for App {
            fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
                self.inbox.extend_from_slice(buf);
                while let Some(end) = self.inbox.iter().position(|b| *b == b'\n') {
                    let line: Vec<u8> = self.inbox.drain(..=end).collect();
                    let line = &line[..line.len() - 1];
                    if self.host_nonce.is_empty() {
                        self.host_nonce = parse_greeting(line).expect("a greeting").to_owned();
                        self.outbox =
                            Cursor::new(answer_line(self.key, &self.host_nonce, A).into_bytes());
                    } else {
                        let claimed = std::str::from_utf8(line).unwrap();
                        self.host_proof_ok =
                            Some(proves(self.key, Role::Host, &self.host_nonce, A, claimed));
                    }
                }
                Ok(buf.len())
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let mut app = App {
            key: KEY,
            inbox: Vec::new(),
            outbox: Cursor::new(Vec::new()),
            host_nonce: String::new(),
            host_proof_ok: None,
        };
        host_handshake(&mut app, KEY).expect("both know the key");
        assert_eq!(app.host_proof_ok, Some(true));

        // An app with another key (a stale bridge.json, another app's port).
        let mut other = App {
            key: OTHER_KEY,
            inbox: Vec::new(),
            outbox: Cursor::new(Vec::new()),
            host_nonce: String::new(),
            host_proof_ok: None,
        };
        let err = host_handshake(&mut other, KEY).expect_err("another key");
        assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);
        assert_eq!(other.host_proof_ok, None, "the host proved nothing to it");
    }

    /// A listener that does not know the key learns nothing from the host: one
    /// line with a random nonce, whatever it answers.
    #[test]
    fn a_listener_without_the_key_gets_a_nonce_and_nothing_else() {
        let replayed = answer_line(KEY, H, A);
        let answers: Vec<Vec<u8>> = vec![
            Vec::new(),
            b"\n".to_vec(),
            format!("{KEY}\n").into_bytes(),
            format!("{A} {}\n", proof(OTHER_KEY, Role::App, H, A)).into_bytes(),
            // What the real app answered on an earlier connection (another host nonce).
            replayed.into_bytes(),
            format!("{A}\n").into_bytes(),
            format!("{A} \n").into_bytes(),
            vec![b'a'; 4096],
            vec![0xff; 40],
            b"{\"t\":\"state\",\"v\":1,\"pairing\":\"approved\"}\n".to_vec(),
        ];
        for answer in answers {
            let mut wire = Wire::answering(answer.clone());
            let err = host_handshake(&mut wire, KEY).expect_err("no proof");
            assert!(
                matches!(
                    err.kind(),
                    io::ErrorKind::PermissionDenied
                        | io::ErrorKind::UnexpectedEof
                        | io::ErrorKind::InvalidData
                ),
                "{err:?}"
            );
            let lines = wire.lines();
            assert_eq!(lines.len(), 1, "only the greeting: {lines:?}");
            let sent = parse_greeting(lines[0].as_bytes()).expect("a greeting");
            let written = String::from_utf8(wire.written.clone()).unwrap();
            assert!(!written.contains(KEY), "the key was sent");
            assert_ne!(sent, H, "the nonce is fresh");
            // Nothing in the line depends on the key: no proof of the host's is in it.
            assert_eq!(written.len(), GREETING.len() + 1 + HEX_LEN + 1);
        }
    }

    #[test]
    fn a_short_line_is_read_without_taking_more() {
        let mut stream = Cursor::new(b"abc\r\nrest".to_vec());
        assert_eq!(read_short_line(&mut stream, 8).unwrap(), b"abc");
        let mut rest = String::new();
        stream.read_to_string(&mut rest).unwrap();
        assert_eq!(rest, "rest", "nothing beyond the line break was taken");
        let mut long = Cursor::new(vec![b'x'; 100]);
        assert_eq!(
            read_short_line(&mut long, 8).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
        assert_eq!(long.position(), 9, "reading stops at the limit");
        let mut cut = Cursor::new(b"no end".to_vec());
        assert_eq!(
            read_short_line(&mut cut, 8).unwrap_err().kind(),
            io::ErrorKind::UnexpectedEof
        );
    }
}
