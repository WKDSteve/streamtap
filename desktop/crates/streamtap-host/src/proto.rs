//! The messages of the browser link, with a strict schema.
//!
//! Three hops carry them:
//!
//! - extension -> host (`ExtMsg`) and host -> extension (`HostMsg`) over the
//!   browser's native messaging pipe (4 byte length + JSON, see `framing`);
//! - host <-> app (`BridgeMsg` towards the app, `HostMsg` back) as one JSON
//!   object per line over a loopback socket.
//!
//! Every message is an object with a `"t"` type and `"v": 1`; unknown fields
//! and unknown types are refused (`deny_unknown_fields`), every string is
//! bounded and checked. Strings that a web page controls (a tab's title, its
//! host) are untrusted: titles are cleaned and cut here, hosts must look like
//! a host name, a favicon must be a small `data:` URL of an image type.
//! Nothing here logs: the credentials message never prints its token (`Debug`
//! is written by hand).

use std::fmt;

use serde::{Deserialize, Serialize};

/// Name of the native messaging host (the key under `NativeMessagingHosts`).
pub const HOST_NAME: &str = "io.github.wkdsteve.streamtap";
/// The one origin that may talk to the host: the streamtap extension, whose id
/// the manifest `key` pins. Native messaging origins end in a slash.
pub const ALLOWED_ORIGIN: &str = "chrome-extension://oggpddakdbffbjhkjmcakidgmhpgakia/";
/// The app's identifier (Tauri names the config directory after it).
pub const APP_IDENTIFIER: &str = "io.github.wkdsteve.streamtap";
/// File in the app's config directory that names the bridge port.
pub const BRIDGE_FILE: &str = "bridge.json";
/// The protocol version in every message.
pub const PROTOCOL_VERSION: u8 = 1;

/// Largest message the host takes from the extension (a tab list with icons).
pub const MAX_FROM_EXTENSION: usize = 512 * 1024;
/// Largest message the host sends to the extension (the browser's own limit is 1 MiB).
pub const MAX_TO_EXTENSION: usize = 16 * 1024;
/// Largest line on the bridge in either direction.
pub const MAX_BRIDGE_LINE: usize = MAX_FROM_EXTENSION + 4096;

/// Tabs in one list.
pub const MAX_TABS: usize = 32;
/// Characters of a tab title kept.
pub const MAX_TITLE_CHARS: usize = 200;
/// Longest host name (DNS limit).
pub const MAX_HOST_LEN: usize = 253;
/// Longest favicon `data:` URL.
pub const MAX_FAVICON_LEN: usize = 8 * 1024;

const MIN_PORT: u16 = 1024;
const MIN_TOKEN_LEN: usize = 16;
const MAX_TOKEN_LEN: usize = 256;

/// Why a message was refused. The text never contains message content.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProtoError(pub &'static str);

impl fmt::Display for ProtoError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.0)
    }
}

impl std::error::Error for ProtoError {}

/// One tab that holds a `MediaSource`, as the extension reports it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TabInfo {
    pub id: u32,
    pub title: String,
    pub host: String,
    /// A small `data:image/...;base64,` URL, or none.
    #[serde(default)]
    pub favicon: Option<String>,
    pub drm: bool,
    pub tapping: bool,
    pub active: bool,
}

/// Extension -> host.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "lowercase", deny_unknown_fields)]
pub enum ExtMsg {
    /// First message: who is asking. `instance` is a random id the extension
    /// keeps per browser profile.
    Hello {
        v: u8,
        instance: String,
        browser: String,
        version: String,
    },
    Tabs {
        v: u8,
        tabs: Vec<TabInfo>,
    },
    Ping {
        v: u8,
    },
}

/// Host -> app: the extension's messages, with the origin the browser named
/// added by the host to the hello.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "lowercase", deny_unknown_fields)]
pub enum BridgeMsg {
    Hello {
        v: u8,
        origin: String,
        instance: String,
        browser: String,
        version: String,
    },
    Tabs {
        v: u8,
        tabs: Vec<TabInfo>,
    },
    Ping {
        v: u8,
    },
}

/// Where a pairing stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Pairing {
    Pending,
    Approved,
    Denied,
    Revoked,
}

/// App -> host -> extension.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "lowercase", deny_unknown_fields)]
pub enum HostMsg {
    State {
        v: u8,
        pairing: Pairing,
    },
    /// Port and token of the sink; only ever sent after the user approved.
    Credentials {
        v: u8,
        port: u16,
        token: String,
    },
    /// Switch capture of one tab on or off.
    Tap {
        v: u8,
        tab: u32,
        on: bool,
    },
    Error {
        v: u8,
        code: String,
    },
}

// By hand: the token must not end up in a log or a panic message.
impl fmt::Debug for HostMsg {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::State { pairing, .. } => write!(f, "State({pairing:?})"),
            Self::Credentials { port, .. } => write!(f, "Credentials(port {port}, token hidden)"),
            Self::Tap { tab, on, .. } => write!(f, "Tap(tab {tab}, {on})"),
            Self::Error { code, .. } => write!(f, "Error({code})"),
        }
    }
}

impl HostMsg {
    #[must_use]
    pub fn state(pairing: Pairing) -> Self {
        Self::State {
            v: PROTOCOL_VERSION,
            pairing,
        }
    }

    #[must_use]
    pub fn credentials(port: u16, token: &str) -> Self {
        Self::Credentials {
            v: PROTOCOL_VERSION,
            port,
            token: token.to_string(),
        }
    }

    #[must_use]
    pub fn tap(tab: u32, on: bool) -> Self {
        Self::Tap {
            v: PROTOCOL_VERSION,
            tab,
            on,
        }
    }

    #[must_use]
    pub fn error(code: &str) -> Self {
        Self::Error {
            v: PROTOCOL_VERSION,
            code: code.chars().take(32).collect(),
        }
    }

    /// Parses and checks one message from the app.
    ///
    /// # Errors
    /// When the bytes are no valid message of the schema.
    pub fn parse(bytes: &[u8]) -> Result<Self, ProtoError> {
        let msg: Self =
            serde_json::from_slice(bytes).map_err(|_| ProtoError("not a valid host message"))?;
        match &msg {
            Self::State { v, .. } | Self::Tap { v, .. } => version(*v)?,
            Self::Credentials { v, port, token } => {
                version(*v)?;
                if *port < MIN_PORT {
                    return Err(ProtoError("credentials: port out of range"));
                }
                if token.len() < MIN_TOKEN_LEN
                    || token.len() > MAX_TOKEN_LEN
                    || !token.chars().all(|c| c.is_ascii_graphic())
                    || token.starts_with('-')
                {
                    return Err(ProtoError("credentials: token has a bad shape"));
                }
            }
            Self::Error { v, code } => {
                version(*v)?;
                if code.is_empty()
                    || code.len() > 32
                    || !code.bytes().all(|b| b.is_ascii_lowercase() || b == b'-')
                {
                    return Err(ProtoError("error: bad code"));
                }
            }
        }
        Ok(msg)
    }

    /// The JSON bytes, within the browser-facing limit.
    ///
    /// # Errors
    /// When the message is larger than `MAX_TO_EXTENSION`.
    pub fn to_bytes(&self) -> Result<Vec<u8>, ProtoError> {
        let bytes =
            serde_json::to_vec(self).map_err(|_| ProtoError("cannot serialise the message"))?;
        if bytes.len() > MAX_TO_EXTENSION {
            return Err(ProtoError("message to the extension is too large"));
        }
        Ok(bytes)
    }
}

fn version(v: u8) -> Result<(), ProtoError> {
    if v == PROTOCOL_VERSION {
        Ok(())
    } else {
        Err(ProtoError("unsupported protocol version"))
    }
}

fn check_hello(instance: &str, browser: &str, version_text: &str) -> Result<(), ProtoError> {
    let plain = |s: &str, min: usize, max: usize, extra: &[u8]| {
        s.len() >= min
            && s.len() <= max
            && s.bytes()
                .all(|b| b.is_ascii_alphanumeric() || extra.contains(&b))
    };
    if !plain(instance, 8, 64, b"-_") {
        return Err(ProtoError("hello: bad instance id"));
    }
    if !matches!(browser, "chrome" | "edge" | "brave" | "other") {
        return Err(ProtoError("hello: unknown browser"));
    }
    if !plain(version_text, 1, 32, b"._-") {
        return Err(ProtoError("hello: bad version"));
    }
    Ok(())
}

/// A title cleaned for display: control, line-break and direction-override
/// characters are dropped, runs of blanks collapse, the length is cut.
#[must_use]
pub fn clean_title(raw: &str) -> String {
    let mut out = String::new();
    let mut blank = true;
    for c in raw.chars() {
        let hidden = matches!(c,
            '\u{200b}'..='\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2060}'..='\u{2064}'
            | '\u{2066}'..='\u{2069}' | '\u{feff}');
        if hidden {
            continue;
        }
        if c.is_control() || c.is_whitespace() {
            if !blank {
                out.push(' ');
                blank = true;
            }
        } else {
            out.push(c);
            blank = false;
        }
        if out.chars().count() >= MAX_TITLE_CHARS {
            break;
        }
    }
    out.trim_end().to_string()
}

fn check_host(host: &str) -> Result<(), ProtoError> {
    let ok = host.len() <= MAX_HOST_LEN
        && host.bytes().all(|b| {
            b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_' | b':' | b'[' | b']')
        });
    if ok {
        Ok(())
    } else {
        Err(ProtoError("tab: bad host"))
    }
}

/// Base64 starts of the image types a favicon may have; the claimed type in
/// the URL must match what the bytes begin with.
const FAVICON_KINDS: [(&str, &str); 5] = [
    ("data:image/png;base64,", "iVBORw0KGgo"),
    ("data:image/jpeg;base64,", "/9j/"),
    ("data:image/gif;base64,", "R0lGOD"),
    ("data:image/webp;base64,", "UklG"),
    ("data:image/x-icon;base64,", "AAABAA"),
];

fn check_favicon(url: &str) -> Result<(), ProtoError> {
    if url.len() > MAX_FAVICON_LEN {
        return Err(ProtoError("tab: favicon too large"));
    }
    let Some((_, magic)) = FAVICON_KINDS.iter().find(|(p, _)| url.starts_with(p)) else {
        return Err(ProtoError("tab: favicon is no data URL of an image type"));
    };
    let body = &url[url.find(',').map_or(0, |i| i + 1)..];
    let b64 = body
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'/' | b'='));
    if !b64 || !body.starts_with(magic) {
        return Err(ProtoError("tab: favicon does not match its image type"));
    }
    Ok(())
}

fn check_tabs(tabs: &mut [TabInfo]) -> Result<(), ProtoError> {
    if tabs.len() > MAX_TABS {
        return Err(ProtoError("tabs: too many tabs"));
    }
    for t in tabs.iter_mut() {
        t.title = clean_title(&t.title);
        check_host(&t.host)?;
        if let Some(f) = &t.favicon {
            check_favicon(f)?;
        }
    }
    Ok(())
}

impl ExtMsg {
    /// Parses and checks one message from the extension; titles come back cleaned.
    ///
    /// # Errors
    /// When the bytes are no valid message of the schema.
    pub fn parse(bytes: &[u8]) -> Result<Self, ProtoError> {
        let mut msg: Self = serde_json::from_slice(bytes)
            .map_err(|_| ProtoError("not a valid extension message"))?;
        match &mut msg {
            Self::Hello {
                v,
                instance,
                browser,
                version: ver,
            } => {
                version(*v)?;
                check_hello(instance, browser, ver)?;
            }
            Self::Tabs { v, tabs } => {
                version(*v)?;
                check_tabs(tabs)?;
            }
            Self::Ping { v } => version(*v)?,
        }
        Ok(msg)
    }

    /// The same message for the bridge; a hello gets the browser's origin.
    #[must_use]
    pub fn into_bridge(self, origin: &str) -> BridgeMsg {
        match self {
            Self::Hello {
                v,
                instance,
                browser,
                version,
            } => BridgeMsg::Hello {
                v,
                origin: origin.to_string(),
                instance,
                browser,
                version,
            },
            Self::Tabs { v, tabs } => BridgeMsg::Tabs { v, tabs },
            Self::Ping { v } => BridgeMsg::Ping { v },
        }
    }
}

impl BridgeMsg {
    /// Parses and checks one line from the host. The origin is *not* judged
    /// here: the app decides on it (`ALLOWED_ORIGIN`).
    ///
    /// # Errors
    /// When the bytes are no valid message of the schema.
    pub fn parse(bytes: &[u8]) -> Result<Self, ProtoError> {
        let mut msg: Self =
            serde_json::from_slice(bytes).map_err(|_| ProtoError("not a valid bridge message"))?;
        match &mut msg {
            Self::Hello {
                v,
                origin,
                instance,
                browser,
                version: ver,
            } => {
                version(*v)?;
                if origin.len() > 128 {
                    return Err(ProtoError("hello: origin too long"));
                }
                check_hello(instance, browser, ver)?;
            }
            Self::Tabs { v, tabs } => {
                version(*v)?;
                check_tabs(tabs)?;
            }
            Self::Ping { v } => version(*v)?,
        }
        Ok(msg)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hello() -> String {
        r#"{"t":"hello","v":1,"instance":"abcd1234-ef","browser":"chrome","version":"0.3.0"}"#
            .to_string()
    }

    #[test]
    fn a_valid_hello_parses() {
        let m = ExtMsg::parse(hello().as_bytes()).unwrap();
        assert!(matches!(m, ExtMsg::Hello { ref browser, .. } if browser == "chrome"));
    }

    #[test]
    fn unknown_fields_types_and_versions_are_refused() {
        let extra = hello().replace("\"v\":1", "\"v\":1,\"admin\":true");
        assert!(ExtMsg::parse(extra.as_bytes()).is_err(), "unknown field");
        let kind = hello().replace("hello", "shell");
        assert!(ExtMsg::parse(kind.as_bytes()).is_err(), "unknown type");
        let ver = hello().replace("\"v\":1", "\"v\":2");
        assert!(ExtMsg::parse(ver.as_bytes()).is_err(), "other version");
        assert!(ExtMsg::parse(b"[]").is_err());
        assert!(ExtMsg::parse(b"{\"t\":\"ping\"}").is_err(), "no version");
        assert!(ExtMsg::parse(&[0xff, 0xfe]).is_err(), "not UTF-8");
    }

    #[test]
    fn hello_fields_are_checked() {
        for bad in [
            hello().replace("abcd1234-ef", "short"),
            hello().replace("abcd1234-ef", "has space 1234"),
            hello().replace("chrome", "firefox"),
            hello().replace("0.3.0", "<script>"),
        ] {
            assert!(ExtMsg::parse(bad.as_bytes()).is_err(), "{bad}");
        }
    }

    #[test]
    fn titles_are_cleaned_and_cut() {
        assert_eq!(clean_title("a\u{0}b\r\n  c\u{202e}d"), "a b cd");
        assert_eq!(clean_title("  x  "), "x");
        assert_eq!(
            clean_title(&"y".repeat(500)).chars().count(),
            MAX_TITLE_CHARS
        );
        assert_eq!(clean_title("\u{200b}\u{feff}"), "");
    }

    fn tab(host: &str, favicon: Option<&str>) -> String {
        let fav = favicon.map_or("null".to_string(), |f| format!("\"{f}\""));
        format!(
            r#"{{"t":"tabs","v":1,"tabs":[{{"id":7,"title":"T\u0007itle","host":"{host}","favicon":{fav},"drm":false,"tapping":false,"active":true}}]}}"#
        )
    }

    #[test]
    fn tabs_are_validated_and_titles_come_back_clean() {
        let ok = ExtMsg::parse(tab("video.example", None).as_bytes()).unwrap();
        let ExtMsg::Tabs { tabs, .. } = ok else {
            panic!("tabs")
        };
        assert_eq!(tabs[0].title, "T itle");
        assert!(ExtMsg::parse(tab("evil host/<x>", None).as_bytes()).is_err());
        let many = format!(
            r#"{{"t":"tabs","v":1,"tabs":[{}]}}"#,
            vec![
                r#"{"id":1,"title":"t","host":"h","drm":false,"tapping":false,"active":false}"#;
                MAX_TABS + 1
            ]
            .join(",")
        );
        assert!(
            ExtMsg::parse(many.as_bytes()).is_err(),
            "more tabs than allowed"
        );
    }

    #[test]
    fn favicons_must_be_small_images_that_match_their_type() {
        let png = "data:image/png;base64,iVBORw0KGgoAAAANSUhEUg==";
        assert!(ExtMsg::parse(tab("h", Some(png)).as_bytes()).is_ok());
        for bad in [
            "https://tracker.example/pixel.png",
            "data:text/html;base64,PHNjcmlwdD4=",
            "data:image/svg+xml;base64,PHN2Zz4=",
            "data:image/png;base64,PHNjcmlwdD4=",
            "data:image/png;base64,iVBORw0KGgo<>",
            &format!(
                "data:image/png;base64,iVBORw0KGgo{}",
                "A".repeat(MAX_FAVICON_LEN)
            ),
        ] {
            assert!(
                ExtMsg::parse(tab("h", Some(bad)).as_bytes()).is_err(),
                "{bad:.40}"
            );
        }
    }

    #[test]
    fn credentials_are_checked_and_never_printed() {
        let secret = "0123456789abcdef0123456789abcdef";
        let msg = HostMsg::credentials(17650, secret);
        let text = format!("{msg:?} {msg:#?}");
        assert!(!text.contains(secret), "Debug must hide the token");
        let bytes = msg.to_bytes().unwrap();
        assert_eq!(HostMsg::parse(&bytes).unwrap(), msg);
        for bad in [
            HostMsg::credentials(80, secret),
            HostMsg::credentials(17650, "short"),
            HostMsg::credentials(17650, "--looks-like-a-flag-0123456789"),
            HostMsg::credentials(17650, "has space in it 0123456789"),
        ] {
            let b = serde_json::to_vec(&bad).unwrap();
            assert!(HostMsg::parse(&b).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn host_messages_round_trip_and_refuse_junk() {
        for m in [
            HostMsg::state(Pairing::Pending),
            HostMsg::state(Pairing::Revoked),
            HostMsg::tap(5, true),
            HostMsg::error("origin-not-allowed"),
        ] {
            assert_eq!(HostMsg::parse(&m.to_bytes().unwrap()).unwrap(), m);
        }
        assert!(HostMsg::parse(br#"{"t":"tap","v":1,"tab":-1,"on":true}"#).is_err());
        assert!(HostMsg::parse(br#"{"t":"error","v":1,"code":"Bad Code"}"#).is_err());
        assert!(HostMsg::parse(br#"{"t":"state","v":1,"pairing":"maybe"}"#).is_err());
    }

    #[test]
    fn the_hello_gains_the_origin_on_the_way_to_the_app() {
        let m = ExtMsg::parse(hello().as_bytes())
            .unwrap()
            .into_bridge(ALLOWED_ORIGIN);
        let line = serde_json::to_vec(&m).unwrap();
        let BridgeMsg::Hello { origin, .. } = BridgeMsg::parse(&line).unwrap() else {
            panic!("hello")
        };
        assert_eq!(origin, ALLOWED_ORIGIN);
        // The extension itself may not name an origin.
        let forged = hello().replace("\"v\":1", "\"v\":1,\"origin\":\"x\"");
        assert!(ExtMsg::parse(forged.as_bytes()).is_err());
    }
}
