//! Phones over the tailnet: what the app asks the running sink.
//!
//! The sink keeps the paired phones and makes the pairing codes; the app only asks it, the way the sink's own
//! command line (`streamtap-sink devices`) does, but in its own process: a WebSocket to the sink on loopback with the
//! extension's origin and one `control` message that carries the token. No child process is started for a question.
//! The answer is one message of JSON, read up to a limit and checked field by field before the page sees it: a phone
//! chose its own name.

use std::net::TcpStream;
use std::time::{Duration, Instant};

use serde::Serialize;

use crate::secret::Token;

/// How long one question may take: the connection, the handshake and the answer together.
const ASK_TIMEOUT: Duration = Duration::from_secs(10);
/// The most of the answer that is read; 16 devices are a few KiB.
const MAX_ANSWER: usize = 64 * 1024;
/// The most devices a sink keeps (appkit's limit); more in an answer is not an answer of the sink.
const MAX_PHONES: usize = 16;
const MAX_NAME: usize = 60;
/// The origin the sink takes on loopback (its `--allow-origin`, which the app never changes): the extension's.
fn sink_origin() -> &'static str {
    streamtap_host::proto::ALLOWED_ORIGIN.trim_end_matches('/')
}

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

/// Asks the sink on `port` (loopback) and returns its answer, an answer with `ok` true.
pub(crate) fn ask(port: u16, token: &Token, op: &Op) -> Result<serde_json::Value, String> {
    let (op_word, device) = match op {
        Op::Pair => ("pair", ""),
        Op::List => ("devices", ""),
        Op::Revoke(id) => {
            // Checked before it is sent: nothing but an id goes to the sink.
            if !is_id(id) {
                return Err("not a device id".into());
            }
            ("revoke", id.as_str())
        }
    };
    let request = serde_json::json!({ "type": "control", "token": token.expose(), "op": op_word, "device": device });
    let answer = exchange(port, &request)?;
    if answer["ok"] != true {
        return Err(answer["error"]
            .as_str()
            .unwrap_or("the sink refused")
            .chars()
            .take(300)
            .collect());
    }
    if matches!(op, Op::Revoke(_)) && answer["revoked"] != true {
        return Err(format!("no paired device has the id {device}"));
    }
    Ok(answer)
}

/// One `control` message to the sink and its answer. Every step is bounded by what is left of [`ASK_TIMEOUT`].
fn exchange(port: u16, request: &serde_json::Value) -> Result<serde_json::Value, String> {
    use tungstenite::client::IntoClientRequest;
    let until = Instant::now() + ASK_TIMEOUT;
    let left = || {
        until
            .checked_duration_since(Instant::now())
            .filter(|d| !d.is_zero())
            .ok_or_else(|| "the sink did not answer in time".to_owned())
    };
    let stream = TcpStream::connect_timeout(&([127, 0, 0, 1], port).into(), left()?)
        .map_err(|_| format!("no sink answers on 127.0.0.1:{port}"))?;
    let set_timeouts = |stream: &TcpStream| -> Result<(), String> {
        let d = left()?;
        stream
            .set_read_timeout(Some(d))
            .and_then(|()| stream.set_write_timeout(Some(d)))
            .map_err(|e| format!("cannot set a timeout: {e}"))
    };
    set_timeouts(&stream)?;
    let mut req = format!("ws://127.0.0.1:{port}/")
        .into_client_request()
        .map_err(|e| e.to_string())?;
    req.headers_mut().insert(
        "origin",
        tungstenite::http::HeaderValue::from_static(sink_origin()),
    );
    let config = tungstenite::protocol::WebSocketConfig::default()
        .max_message_size(Some(MAX_ANSWER))
        .max_frame_size(Some(MAX_ANSWER));
    let (mut ws, _) = tungstenite::client::client_with_config(req, stream, Some(config))
        .map_err(|e| format!("the sink on 127.0.0.1:{port} refused the connection ({e})"))?;
    // The sink's framing: the length of the JSON header (u32, little endian), the header, no payload.
    let header = request.to_string().into_bytes();
    let mut frame = u32::try_from(header.len())
        .map_err(|_| "request too large".to_owned())?
        .to_le_bytes()
        .to_vec();
    frame.extend_from_slice(&header);
    ws.send(tungstenite::Message::binary(frame))
        .map_err(|e| format!("cannot ask the sink: {e}"))?;
    loop {
        set_timeouts(ws.get_ref())?;
        match ws.read() {
            Ok(tungstenite::Message::Text(t)) => {
                let _ = ws.close(None);
                return serde_json::from_str(t.as_str())
                    .map_err(|_| "the sink's answer is not JSON".to_owned());
            }
            Ok(tungstenite::Message::Close(_)) => return Err(
                "the sink closed without an answer: a wrong token, or a sink older than this app"
                    .into(),
            ),
            Ok(_) => {}
            Err(tungstenite::Error::Io(e))
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                return Err("the sink did not answer in time".into())
            }
            Err(_) => return Err(
                "the sink closed without an answer: a wrong token, or a sink older than this app"
                    .into(),
            ),
        }
    }
}

/// The modules of the QR code of `text`, one string of `1` (dark) and `0` per row, without a quiet zone (as the
/// sink's `devices pair --json` makes them).
fn qr_rows(text: &str) -> Result<Vec<String>, String> {
    let code = qrcode::QrCode::with_error_correction_level(text.as_bytes(), qrcode::EcLevel::M)
        .map_err(|_| "the pairing link is too long for a QR code".to_owned())?;
    Ok(code
        .to_colors()
        .chunks(code.width())
        .map(|row| {
            row.iter()
                .map(|c| if *c == qrcode::Color::Dark { '1' } else { '0' })
                .collect()
        })
        .collect())
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
    // Made here from the checked parts, not taken from the answer.
    let link = format!("https://{tailnet}/#streamtap-pair=1&code={code}");
    let qr = qr_rows(&link)?;
    Ok(Pairing {
        tailnet: tailnet.to_owned(),
        link,
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

    #[test]
    fn a_pairing_is_checked_and_its_link_and_qr_code_are_made_here() {
        let code = "0123456789abcdef0123456789abcdef";
        let good = json!({ "ok": true, "tailnet": "pc.example.ts.net", "code": code, "ttl": 60,
            "qr": ["1"], "link": "https://evil.example/#streamtap-pair=1&code=x" });
        let p = pairing_of(&good, "pc.example.ts.net").expect("a pairing");
        assert_eq!(
            p.link,
            format!("https://pc.example.ts.net/#streamtap-pair=1&code={code}")
        );
        assert_eq!(p.qr, qr_rows(&p.link).unwrap());
        assert!(p.qr.len() >= 21 && p.qr.iter().all(|r| r.len() == p.qr.len()));
        assert_eq!(p.ttl, 60);
        assert_eq!(format!("{p:?}"), "Pairing(pc.example.ts.net, code hidden)");
        assert!(pairing_of(&good, "other.example.ts.net").is_err());
        for (what, bad) in [
            ("no code", json!({ "tailnet": "pc.example.ts.net" })),
            (
                "a code of another form",
                json!({ "tailnet": "pc.example.ts.net", "code": "<b>" }),
            ),
        ] {
            assert!(pairing_of(&bad, "pc.example.ts.net").is_err(), "{what}");
        }
    }

    /// A stand-in for the running sink on loopback: takes one WebSocket, hands its `Origin` and its first message
    /// to `seen`, and answers with `answer` (or closes without one).
    // The handshake callback's error type is tungstenite's, not ours.
    #[allow(clippy::result_large_err)]
    fn fake_sink(
        answer: Option<serde_json::Value>,
    ) -> (u16, std::thread::JoinHandle<(String, serde_json::Value)>) {
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let handle = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut origin = String::new();
            let mut ws = tungstenite::accept_hdr(
                stream,
                |req: &tungstenite::handshake::server::Request,
                 res: tungstenite::handshake::server::Response| {
                    origin = req.headers()["origin"].to_str().unwrap().to_owned();
                    Ok(res)
                },
            )
            .unwrap();
            let data = loop {
                if let tungstenite::Message::Binary(b) = ws.read().unwrap() {
                    break b;
                }
            };
            let len = u32::from_le_bytes(data[..4].try_into().unwrap()) as usize;
            let header: serde_json::Value = serde_json::from_slice(&data[4..4 + len]).unwrap();
            match answer {
                Some(a) => ws.send(tungstenite::Message::text(a.to_string())).unwrap(),
                None => {
                    let _ = ws.close(None);
                }
            }
            let _ = ws.flush();
            // Until the app is done with the connection.
            while ws.read().is_ok() {}
            (origin, header)
        });
        (port, handle)
    }

    #[test]
    fn the_app_asks_the_sink_itself_over_loopback_with_the_token_in_the_message() {
        let token = Token::from("0123456789abcdef0123");
        let (port, sink) = fake_sink(Some(
            json!({ "ok": true, "tailnet": "pc.example.ts.net", "devices": [] }),
        ));
        let answer = ask(port, &token, &Op::List).expect("an answer");
        assert_eq!(answer["devices"], json!([]));
        let (origin, header) = sink.join().unwrap();
        assert_eq!(
            origin,
            "chrome-extension://oggpddakdbffbjhkjmcakidgmhpgakia"
        );
        assert_eq!(
            header,
            json!({ "type": "control", "token": "0123456789abcdef0123", "op": "devices", "device": "" })
        );

        let id = "ab".repeat(16);
        let (port, sink) = fake_sink(Some(json!({ "ok": true, "revoked": false })));
        let err = ask(port, &token, &Op::Revoke(id.clone())).unwrap_err();
        assert_eq!(err, format!("no paired device has the id {id}"));
        assert_eq!(sink.join().unwrap().1["op"], "revoke");

        let (port, sink) = fake_sink(Some(
            json!({ "ok": false, "error": "this sink was started without --tailnet" }),
        ));
        assert_eq!(
            ask(port, &token, &Op::Pair).unwrap_err(),
            "this sink was started without --tailnet"
        );
        assert_eq!(sink.join().unwrap().1["op"], "pair");

        let (port, sink) = fake_sink(None);
        assert!(ask(port, &token, &Op::List)
            .unwrap_err()
            .contains("closed without an answer"));
        sink.join().unwrap();
    }

    #[test]
    fn no_sink_on_the_port_is_an_error_not_a_wait() {
        let free = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = free.local_addr().unwrap().port();
        drop(free);
        let token = Token::from("0123456789abcdef0123");
        let err = ask(port, &token, &Op::List).unwrap_err();
        assert!(err.contains("no sink answers"), "{err}");
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
            let err = ask(1, &token, &Op::Revoke(id.into()));
            assert_eq!(err.unwrap_err(), "not a device id", "{id:?}");
        }
    }
}
