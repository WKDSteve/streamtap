//! The wire format: the headers of a message and the lines of `index.jsonl`.

use super::*;
use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub(crate) enum Header {
    Hello {
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
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "wire format: independent flags of one append"
)]
pub(crate) struct SegmentHeader {
    pub(crate) sb: u32,
    pub(crate) seq: u64,
    /// `MediaSource` the `SourceBuffer` belongs to (null for `SourceBuffers` the
    /// hook only saw after they were created).
    #[serde(default)]
    pub(crate) ms: Option<u32>,
    #[serde(default)]
    pub(crate) mime: String,
    #[serde(default, rename = "isInit")]
    pub(crate) is_init: bool,
    /// The rest of an init segment that did not fit the previous append (the
    /// hook saw the init still open).
    #[serde(default, rename = "initCont")]
    pub(crate) init_cont: bool,
    #[serde(default)]
    pub(crate) kind: String,
    /// Decode time of the first fragment, in `timescale` ticks. Above 2^53 the
    /// extension sends it as a decimal string (JS numbers lose precision).
    #[serde(default, deserialize_with = "de_opt_u64")]
    pub(crate) tfdt: Option<u64>,
    /// Track ID (`tfhd`) of the first fragment: the track `tfdt` and `dur`
    /// belong to. An append may begin with another track than the one before.
    #[serde(default)]
    pub(crate) track: Option<u32>,
    #[serde(default)]
    pub(crate) timescale: Option<u32>,
    /// Duration of the append's first track in `timescale` ticks, if known.
    #[serde(default, deserialize_with = "de_opt_u64")]
    pub(crate) dur: Option<u64>,
    /// `SourceBuffer.timestampOffset` in seconds at the time of the append.
    /// Multi-period DASH and HLS discontinuities restart `tfdt` and shift the
    /// timeline with it, so dedupe and ordering must include it.
    #[serde(default)]
    pub(crate) tso: Option<f64>,
    /// The `SourceBuffer` is in "sequence" mode: the browser places media
    /// one after the other and rewrites `timestampOffset` itself.
    #[serde(default, rename = "seqMode")]
    pub(crate) seq_mode: bool,
    /// The player set `timestampOffset` since the previous append. In
    /// sequence mode the browser then starts this append at that offset.
    #[serde(default, rename = "tsoSet")]
    pub(crate) tso_set: bool,
}

/// `Option<u64>` from a JSON number or a decimal string.
pub(crate) fn de_opt_u64<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<u64>, D::Error> {
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
pub(crate) struct IndexLine<'a> {
    pub(crate) sb: u32,
    pub(crate) ms: Option<u32>,
    pub(crate) part: u32,
    pub(crate) seq: u64,
    pub(crate) offset: u64,
    pub(crate) len: usize,
    /// Init bytes: an init segment or the continuation of one.
    pub(crate) is_init: bool,
    pub(crate) tfdt: Option<u64>,
    /// Track ID the `tfdt` belongs to (`SegmentHeader::track`).
    pub(crate) track: Option<u32>,
    pub(crate) timescale: Option<u32>,
    pub(crate) dur: Option<u64>,
    pub(crate) tso: Option<f64>,
    pub(crate) seq_mode: bool,
    pub(crate) tso_set: bool,
    /// A sequence-mode duplicate: no bytes of its own, it points at the row
    /// it repeats and only holds its place in the timeline.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub(crate) mark: bool,
    pub(crate) mime: &'a str,
    pub(crate) kind: &'a str,
}

pub(crate) fn split_message(data: &[u8]) -> Result<(Header, &[u8]), SinkError> {
    if data.len() < 4 {
        return Err(SinkError::Protocol(
            "shorter than header length field".into(),
        ));
    }
    let hlen = u32::from_le_bytes([data[0], data[1], data[2], data[3]]) as usize;
    if hlen > MAX_HEADER || 4 + hlen > data.len() {
        return Err(SinkError::Protocol(format!(
            "header length {hlen} out of range"
        )));
    }
    let header: Header = serde_json::from_slice(&data[4..4 + hlen])
        .map_err(|e| SinkError::Protocol(e.to_string()))?;
    Ok((header, &data[4 + hlen..]))
}
