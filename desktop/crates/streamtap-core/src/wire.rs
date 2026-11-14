//! The wire format: the headers of a message, and the lines of `index.jsonl` as they are written and read.

use std::fmt;

use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Header {
    Hello {
        /// The shared token of a local connection; a paired device (tailnet) sends none.
        #[serde(default)]
        token: String,
        #[serde(default)]
        session: String,
        #[serde(default)]
        url: String,
        #[serde(default)]
        title: String,
    },
    Segment(SegmentHeader),
    Event {
        name: String,
        #[serde(default)]
        sb: Option<u32>,
    },
    /// A phone asks to be paired (tailnet only): the one-time code, its name and its public key.
    Pair {
        #[serde(default)]
        v: u32,
        code: String,
        name: String,
        key: String,
    },
    /// A paired device asks for a challenge (tailnet only).
    Login {
        #[serde(default)]
        v: u32,
        device: String,
    },
    /// The device's signature of the challenge.
    Auth {
        signature: String,
    },
    /// The sink's own command line asks the running sink (loopback only, with the token).
    Control {
        token: String,
        op: String,
        #[serde(default)]
        device: String,
    },
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "wire format: independent flags of one append"
)]
pub struct SegmentHeader {
    pub sb: u32,
    pub seq: u64,
    /// `MediaSource` the `SourceBuffer` belongs to (null for `SourceBuffers` the
    /// hook only saw after they were created).
    #[serde(default)]
    pub ms: Option<u32>,
    #[serde(default)]
    pub mime: String,
    #[serde(default, rename = "isInit")]
    pub is_init: bool,
    /// The rest of an init segment that did not fit the previous append (the
    /// hook saw the init still open).
    #[serde(default, rename = "initCont")]
    pub init_cont: bool,
    #[serde(default)]
    pub kind: String,
    /// Decode time of the first fragment, in `timescale` ticks. Above 2^53 the
    /// extension sends it as a decimal string (JS numbers lose precision).
    #[serde(default, deserialize_with = "de_opt_u64")]
    pub tfdt: Option<u64>,
    /// Track ID (`tfhd`) of the first fragment: the track `tfdt` and `dur`
    /// belong to. An append may begin with another track than the one before.
    #[serde(default)]
    pub track: Option<u32>,
    #[serde(default)]
    pub timescale: Option<u32>,
    /// Duration of the append's first track in `timescale` ticks, if known.
    #[serde(default, deserialize_with = "de_opt_u64")]
    pub dur: Option<u64>,
    /// `SourceBuffer.timestampOffset` in seconds at the time of the append.
    /// Multi-period DASH and HLS discontinuities restart `tfdt` and shift the
    /// timeline with it, so dedupe and ordering must include it.
    #[serde(default)]
    pub tso: Option<f64>,
    /// The `SourceBuffer` is in "sequence" mode: the browser places media
    /// one after the other and rewrites `timestampOffset` itself.
    #[serde(default, rename = "seqMode")]
    pub seq_mode: bool,
    /// The player set `timestampOffset` since the previous append. In
    /// sequence mode the browser then starts this append at that offset.
    #[serde(default, rename = "tsoSet")]
    pub tso_set: bool,
}

/// `Option<u64>` from a JSON number or a decimal string.
fn de_opt_u64<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<u64>, D::Error> {
    use serde::de::Error;
    match Option::<serde_json::Value>::deserialize(d)? {
        None | Some(serde_json::Value::Null) => Ok(None),
        Some(serde_json::Value::Number(n)) => n
            .as_u64()
            .map(Some)
            .ok_or_else(|| D::Error::custom("not an unsigned 64-bit integer")),
        Some(serde_json::Value::String(s)) if s.len() <= 20 => {
            s.parse::<u64>().map(Some).map_err(D::Error::custom)
        }
        Some(_) => Err(D::Error::custom("expected a number or a decimal string")),
    }
}

#[derive(Debug, Serialize)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "index format: independent flags of one row"
)]
pub struct IndexLine<'a> {
    pub sb: u32,
    pub ms: Option<u32>,
    pub part: u32,
    pub seq: u64,
    pub offset: u64,
    pub len: usize,
    /// Init bytes: an init segment or the continuation of one.
    pub is_init: bool,
    pub tfdt: Option<u64>,
    /// Track ID the `tfdt` belongs to (`SegmentHeader::track`).
    pub track: Option<u32>,
    pub timescale: Option<u32>,
    pub dur: Option<u64>,
    pub tso: Option<f64>,
    pub seq_mode: bool,
    pub tso_set: bool,
    /// A sequence-mode duplicate: no bytes of its own, it points at the row
    /// it repeats and only holds its place in the timeline.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub mark: bool,
    pub mime: &'a str,
    pub kind: &'a str,
}

/// The JSON header of one message may be this long.
pub const MAX_HEADER: usize = 64 * 1024;

/// A message that is not in the wire format; the text says what was wrong with it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WireError(pub String);

impl fmt::Display for WireError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for WireError {}

/// Splits one binary message into its header and its payload.
///
/// # Errors
///
/// When the message is shorter than its length field says, the header is longer than [`MAX_HEADER`], or it is
/// not a header this format knows.
pub fn split_message(data: &[u8]) -> Result<(Header, &[u8]), WireError> {
    if data.len() < 4 {
        return Err(WireError("shorter than header length field".into()));
    }
    let hlen = u32::from_le_bytes([data[0], data[1], data[2], data[3]]) as usize;
    if hlen > MAX_HEADER || 4 + hlen > data.len() {
        return Err(WireError(format!("header length {hlen} out of range")));
    }
    let header: Header =
        serde_json::from_slice(&data[4..4 + hlen]).map_err(|e| WireError(e.to_string()))?;
    Ok((header, &data[4 + hlen..]))
}

#[derive(Debug, Deserialize)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "index format: independent flags of one row"
)]
pub struct IndexRow {
    pub sb: u32,
    #[serde(default)]
    pub ms: Option<u32>,
    pub part: u32,
    pub offset: u64,
    pub len: u64,
    pub is_init: bool,
    pub tfdt: Option<u64>,
    #[serde(default)]
    pub track: Option<u32>,
    pub timescale: Option<u32>,
    #[serde(default)]
    pub dur: Option<u64>,
    #[serde(default)]
    pub tso: Option<f64>,
    #[serde(default)]
    pub seq_mode: bool,
    #[serde(default)]
    pub tso_set: bool,
    /// Only for the sequence-mode layout (`IndexLine::mark`).
    #[serde(default)]
    pub mark: bool,
    pub mime: String,
    #[serde(default)]
    pub kind: String,
}
