//! Phones over the tailnet: what the app asks the running sink (`streamtap-sink devices ... --json`).
//!
//! The sink keeps the paired phones and makes the pairing codes; the app only asks it, through the sink's own
//! command line: a short-lived child process that talks to the running sink over loopback with the token, which it
//! gets on stdin like the sink itself (never on a command line). Its answer is one line of JSON, read up to a limit
//! and checked field by field before the page sees it: a phone chose its own name.

use std::io::Read;
use std::path::Path;
use std::process::Stdio;
use std::time::{Duration, Instant};

use serde::Serialize;

use crate::secret::Token;
use crate::sink;

/// How long one question may take (the command line itself gives up after 10 s).
const ASK_TIMEOUT: Duration = Duration::from_secs(15);
const WAIT_POLL: Duration = Duration::from_millis(25);
/// The most of the answer that is read; 16 devices and a QR code are a few KiB.
const MAX_ANSWER: u64 = 64 * 1024;
const MAX_STDERR: u64 = 4 * 1024;
/// The most devices a sink keeps (appkit's limit); more in an answer is not an answer of the sink.
const MAX_PHONES: usize = 16;
/// The largest QR code has 177 modules a side.
const MAX_QR: usize = 177;
const MAX_NAME: usize = 60;

/// Whether `name` is a machine's name in a tailnet as the sink's `--tailnet` takes it: a plain host name in lower
/// case with a dot, no scheme, no port (`pc.example.ts.net`; with Headscale `pc.<base_domain>`).
pub(crate) fn tailnet_name_ok(name: &str) -> bool {
    let label_ok = |l: &str| {
        !l.is_empty()
            && l.len() <= 63
            && !l.starts_with('-')
            && !l.ends_with('-')
            && l.bytes()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-')
    };
    name.len() <= 253 && name.contains('.') && name.split('.').all(label_ok)
}

fn is_id(s: &str) -> bool {
    s.len() == 32 && s.bytes().all(|c| matches!(c, b'0'..=b'9' | b'a'..=b'f'))
}

/// What to ask the sink.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Op {
    Pair,
    List,
    Revoke(String),
}

/// A pairing code as the page shows it: the link, its QR code and the code to type. Good for `ttl` seconds, once.
/// Its `Debug` prints none of the three (SRV-11): a `{:?}` in a log or an error cannot leak the code.
#[derive(Clone, Serialize)]
pub(crate) struct Pairing {
    pub(crate) tailnet: String,
    pub(crate) link: String,
    pub(crate) code: String,
    pub(crate) ttl: u64,
    /// The modules of the QR code of `link`: one string of `1` (dark) and `0` per row.
    pub(crate) qr: Vec<String>,
}

impl std::fmt::Debug for Pairing {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Pairing({}, code hidden)", self.tailnet)
    }
}

/// A paired phone.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub(crate) struct Phone {
    pub(crate) id: String,
    pub(crate) name: String,
    /// Seconds since 1970.
    pub(crate) paired: Option<u64>,
    pub(crate) last_used: Option<u64>,
    /// Its connections that are open right now.
    pub(crate) open: u64,
}

/// A name a phone gave itself, as one line of plain text of bounded length.
fn clean_name(name: &str) -> String {
    name.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .take(MAX_NAME)
        .collect::<String>()
        .trim()
        .to_owned()
}

/// Runs `exe devices ... --json` against the sink on `port` and returns its answer.
pub(crate) fn ask(
    exe: &Path,
    port: u16,
    token: &Token,
    op: &Op,
) -> Result<serde_json::Value, String> {
    let mut args: Vec<String> = vec!["devices".into()];
    match op {
        Op::Pair => args.push("pair".into()),
        Op::List => {}
        Op::Revoke(id) => {
            // Checked before it is an argument: nothing but an id goes into the command line.
            if !is_id(id) {
                return Err("not a device id".into());
            }
            args.extend(["revoke".into(), id.clone()]);
        }
    }
    args.extend([
        "--json".into(),
        "--token-stdin".into(),
        "--port".into(),
        port.to_string(),
    ]);
    let mut child = sink::command(exe)
        .args(&args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("cannot start streamtap-sink: {e}"))?;
    let handed_over = child
        .stdin
        .take()
        .ok_or_else(|| std::io::Error::other("no stdin pipe"))
        .and_then(|stdin| sink::hand_over_token(stdin, token));
    // Both pipes are read on threads of their own, so a child that writes much never blocks, and a child that
    // hangs is ended by the deadline below.
    let read = |pipe: Option<Box<dyn Read + Send>>, limit: u64| {
        std::thread::spawn(move || {
            let mut text = String::new();
            if let Some(p) = pipe {
                let _ = p.take(limit).read_to_string(&mut text);
            }
            text
        })
    };
    let out = read(
        child
            .stdout
            .take()
            .map(|p| Box::new(p) as Box<dyn Read + Send>),
        MAX_ANSWER,
    );
    let err = read(
        child
            .stderr
            .take()
            .map(|p| Box::new(p) as Box<dyn Read + Send>),
        MAX_STDERR,
    );
    let until = Instant::now() + ASK_TIMEOUT;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) if handed_over.is_ok() && Instant::now() < until => {
                std::thread::sleep(WAIT_POLL);
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
        }
    };
    let (out, err) = (
        out.join().unwrap_or_default(),
        err.join().unwrap_or_default(),
    );
    if let Err(e) = handed_over {
        return Err(format!("cannot hand the token to streamtap-sink: {e}"));
    }
    let Some(status) = status else {
        return Err("the sink did not answer in time".into());
    };
    if !status.success() {
        // The command line says why on stderr (`devices: ...`); the token is never part of it.
        let why = err.trim().trim_start_matches("devices: ");
        return Err(if why.is_empty() {
            "the sink did not answer".into()
        } else {
            why.chars().take(300).collect()
        });
    }
    serde_json::from_str(out.trim()).map_err(|_| "the sink's answer is not JSON".to_owned())
}

/// The pairing in the sink's answer, for the tailnet name the app started the sink with.
pub(crate) fn pairing_of(answer: &serde_json::Value, tailnet: &str) -> Result<Pairing, String> {
    let bad = || "the sink's answer is not a pairing".to_owned();
    let code = answer["code"]
        .as_str()
        .filter(|c| is_id(c))
        .ok_or_else(bad)?;
    if answer["tailnet"].as_str() != Some(tailnet) {
        return Err(
            "the sink answers for another tailnet name than the one in the settings".into(),
        );
    }
    let qr: Vec<String> = answer["qr"]
        .as_array()
        .ok_or_else(bad)?
        .iter()
        .filter_map(|r| r.as_str().map(str::to_owned))
        .collect();
    let square = (21..=MAX_QR).contains(&qr.len())
        && qr
            .iter()
            .all(|r| r.len() == qr.len() && r.bytes().all(|c| c == b'0' || c == b'1'));
    if !square {
        return Err(bad());
    }
    Ok(Pairing {
        tailnet: tailnet.to_owned(),
        // Made here from the checked parts, not taken from the answer.
        link: format!("https://{tailnet}/#streamtap-pair=1&code={code}"),
        code: code.to_owned(),
        ttl: answer["ttl"].as_u64().unwrap_or(60).min(600),
        qr,
    })
}

/// The phones in the sink's answer. An entry of another form is left out.
pub(crate) fn phones_of(answer: &serde_json::Value) -> Result<Vec<Phone>, String> {
    let list = answer["devices"]
        .as_array()
        .ok_or("the sink's answer is not a list of devices")?;
    Ok(list
        .iter()
        .filter_map(|d| {
            let id = d["id"].as_str().filter(|id| is_id(id))?;
            Some(Phone {
                id: id.to_owned(),
                name: clean_name(d["name"].as_str().unwrap_or("")),
                paired: d["paired"].as_u64(),
                last_used: d["last_used"].as_u64(),
                open: d["open"].as_u64().unwrap_or(0),
            })
        })
        .take(MAX_PHONES)
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn the_tailnet_name_is_a_plain_lower_case_host() {
        for ok in [
            "pc.example.ts.net",
            "my-pc.tail1234.ts.net",
            "pc.hs.example.org",
        ] {
            assert!(tailnet_name_ok(ok), "{ok}");
        }
        for bad in [
            "",
            "pc",
            "PC.example.ts.net",
            "https://pc.example.ts.net",
            "pc.example.ts.net:443",
            "pc.example.ts.net/",
            "pc..ts.net",
            "-pc.ts.net",
            "--tailnet.x",
            "pc .ts.net",
        ] {
            assert!(!tailnet_name_ok(bad), "{bad:?}");
        }
    }

    fn qr(n: usize) -> Vec<String> {
        vec!["10".repeat(n).chars().take(n).collect(); n]
    }

    #[test]
    fn a_pairing_is_checked_and_its_link_is_made_here() {
        let code = "0123456789abcdef0123456789abcdef";
        let good = json!({ "ok": true, "tailnet": "pc.example.ts.net", "code": code, "ttl": 60, "qr": qr(33),
            "link": "https://evil.example/#streamtap-pair=1&code=x" });
        let p = pairing_of(&good, "pc.example.ts.net").expect("a pairing");
        assert_eq!(
            p.link,
            format!("https://pc.example.ts.net/#streamtap-pair=1&code={code}")
        );
        assert_eq!((p.ttl, p.qr.len()), (60, 33));
        assert_eq!(format!("{p:?}"), "Pairing(pc.example.ts.net, code hidden)");
        assert!(pairing_of(&good, "other.example.ts.net").is_err());
        for (what, bad) in [
            (
                "no code",
                json!({ "tailnet": "pc.example.ts.net", "qr": qr(33) }),
            ),
            (
                "a code of another form",
                json!({ "tailnet": "pc.example.ts.net", "code": "<b>", "qr": qr(33) }),
            ),
            (
                "no QR code",
                json!({ "tailnet": "pc.example.ts.net", "code": code }),
            ),
            (
                "a QR code that is not square",
                json!({ "tailnet": "pc.example.ts.net", "code": code, "qr": ["101", "010"] }),
            ),
            (
                "a QR code with other characters",
                json!({ "tailnet": "pc.example.ts.net", "code": code, "qr": vec!["x".repeat(21); 21] }),
            ),
            (
                "a QR code larger than any",
                json!({ "tailnet": "pc.example.ts.net", "code": code, "qr": qr(178) }),
            ),
        ] {
            assert!(pairing_of(&bad, "pc.example.ts.net").is_err(), "{what}");
        }
    }

    #[test]
    fn phones_are_checked_and_their_names_are_one_line() {
        let id = "ab".repeat(16);
        let answer = json!({ "devices": [
            { "id": id, "name": "  my\nphone\u{7}  ", "paired": 5, "last_used": null, "open": 2, "level": "create" },
            { "id": "not-an-id", "name": "x" },
            { "name": "no id" },
            { "id": "cd".repeat(16), "name": "n".repeat(500) },
        ] });
        let phones = phones_of(&answer).expect("a list");
        assert_eq!(phones.len(), 2);
        assert_eq!(
            phones[0],
            Phone {
                id,
                name: "my phone".into(),
                paired: Some(5),
                last_used: None,
                open: 2
            }
        );
        assert_eq!(phones[1].name.chars().count(), MAX_NAME);
        assert!(phones_of(&json!({ "ok": true })).is_err());
        let many: Vec<_> = (0..40)
            .map(|i| json!({ "id": format!("{i:032x}"), "name": "p" }))
            .collect();
        assert_eq!(
            phones_of(&json!({ "devices": many }))
                .expect("a list")
                .len(),
            MAX_PHONES
        );
    }

    #[test]
    fn only_an_id_becomes_an_argument() {
        let token = Token::from("0123456789abcdef0123");
        for id in ["--port", "x", "", "AB".repeat(16).as_str()] {
            let err = ask(
                Path::new("no-such-program"),
                1,
                &token,
                &Op::Revoke(id.into()),
            );
            assert_eq!(err.unwrap_err(), "not a device id", "{id:?}");
        }
    }
}
