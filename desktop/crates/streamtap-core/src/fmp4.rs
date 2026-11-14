//! What the bytes are: container, init segment, media head, encryption; and the boxes the mux reads or rewrites.

use std::collections::HashMap;

use crate::wire::SegmentHeader;

/// PIFF sample encryption box (`uuid` A2394F52-5A9B-4F14-A244-6C427C648DF4).
pub const PIFF_SAMPLE_ENCRYPTION: [u8; 16] = [
    0xa2, 0x39, 0x4f, 0x52, 0x5a, 0x9b, 0x4f, 0x14, 0xa2, 0x44, 0x6c, 0x42, 0x7c, 0x64, 0x8d, 0xf4,
];

/// The container of a stream's segments, parsed once from the wire `kind`
/// string so no later code compares strings. `Other` is any kind this sink does
/// not know: it takes the MP4 code paths, as the string comparisons always did
/// (everything that was not `"webm"`), and matches no input format.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Container {
    Mp4,
    Webm,
    #[default]
    Other,
}

impl Container {
    pub fn from_kind(kind: &str) -> Self {
        match kind {
            "mp4" => Self::Mp4,
            "webm" => Self::Webm,
            _ => Self::Other,
        }
    }

    pub fn is_webm(self) -> bool {
        self == Self::Webm
    }

    /// The ffmpeg input format of a part starting with `head`, `None` when the
    /// bytes do not look like this container (so the part is not muxed).
    pub fn head_format(self, head: [u8; 8]) -> Option<&'static str> {
        match self {
            Self::Mp4 if matches!(&head[4..8], b"ftyp" | b"moov" | b"styp") => Some("mp4"),
            Self::Webm if head[..4] == [0x1a, 0x45, 0xdf, 0xa3] => Some("matroska"),
            _ => None,
        }
    }
}

impl SegmentHeader {
    pub fn container(&self) -> Container {
        Container::from_kind(&self.kind)
    }
}

/// True when a segment carries protected media. Init bytes (an init or a
/// continuation of one, `init_part`; `init_kind` is the init's container) are
/// scanned for the markers: `encv`/`enca`/`tenc`/`pssh` (MP4), or the
/// `ContentEncodings`/`ContentEncryption`/`ContentEncKeyID` IDs (`WebM`).
/// Media is only read as structure: a top-level `moov` or `pssh`, or `senc` /
/// PIFF sample encryption in a `moof`, or `saiz`/`saio` there (CENC media may
/// carry its IVs as auxiliary info without a `senc`; clear media has no
/// auxiliary info in practice). Random bytes in media never count.
pub fn payload_encrypted(
    h: &SegmentHeader,
    payload: &[u8],
    init_part: bool,
    init_kind: Container,
) -> bool {
    let webm = h.container().is_webm() || (init_part && !h.is_init && init_kind.is_webm());
    if h.is_init || init_part {
        return if webm {
            webm_encrypted(payload)
        } else {
            looks_encrypted(payload)
        };
    }
    if webm {
        return false;
    }
    boxes_in(payload, 0, payload.len())
        .iter()
        .any(|(kind, s, e)| match kind {
            b"moov" => looks_encrypted(&payload[*s..*e]),
            b"pssh" => true,
            b"moof" => {
                let m = &payload[*s..*e];
                m.windows(4)
                    .any(|w| w == b"senc" || w == b"saiz" || w == b"saio")
                    || m.windows(16).any(|w| w == PIFF_SAMPLE_ENCRYPTION)
            }
            _ => false,
        })
}

/// An EBML variable-size integer at `off`: (value, length). `keep_marker`
/// keeps the length marker (element IDs).
pub fn read_vint(b: &[u8], off: usize, keep_marker: bool) -> Option<(u64, usize)> {
    let first = *b.get(off)?;
    let len = first.leading_zeros() as usize + 1;
    if len > 8 || off + len > b.len() {
        return None;
    }
    let mask = if len == 8 { 0 } else { 0xffu8 >> len };
    let mut v = u64::from(if keep_marker { first } else { first & mask });
    for &byte in &b[off + 1..off + len] {
        v = (v << 8) | u64::from(byte);
    }
    Some((v, len))
}

/// Top-level MP4 boxes that belong to a media segment, not to its init.
pub const MEDIA_BOXES: [&[u8; 4]; 7] = [
    b"styp", b"sidx", b"ssix", b"emsg", b"prft", b"moof", b"mdat",
];

/// Of those, the ones only a media segment has (`sidx`/`ssix` also form the
/// index range of a single-file stream, right after its `moov`).
pub const SEGMENT_BOXES: [&[u8; 4]; 5] = [b"styp", b"emsg", b"prft", b"moof", b"mdat"];

/// Where the init part of an init+media append ends (video.js VHS appends the
/// init together with the first segment): the first top-level media box
/// (`styp`, `sidx`, `emsg`, `moof`, ...; MP4) or the first Cluster (`WebM`).
/// Per-segment boxes in front of the `moof` (a `sidx` differs per segment)
/// must not count as init bytes, or every combined append would look like a
/// new init. `None` when there is no media in it.
pub fn init_prefix_len(kind: Container, payload: &[u8]) -> Option<usize> {
    if kind.is_webm() {
        let mut off = 0;
        while off < payload.len() {
            let (id, il) = read_vint(payload, off, true)?;
            let (size, sl) = read_vint(payload, off + il, false)?;
            if id == 0x1f43_b675 {
                return (off > 0).then_some(off);
            }
            let body = off + il + sl;
            let unknown = sl < 8 && size == (1u64 << (7 * sl)) - 1;
            if id == 0x1853_8067 || unknown {
                off = body; // descend into Segment
                continue;
            }
            off = body.checked_add(usize::try_from(size).ok()?)?;
        }
        return None;
    }
    // Only with a box after it that only a segment has (`SEGMENT_BOXES`): an
    // init+index range of a single-file stream (ftyp moov sidx) is all init.
    let starts = box_starts(payload);
    let first_media = starts.iter().position(|(k, _)| MEDIA_BOXES.contains(&k))?;
    if !starts[first_media..]
        .iter()
        .any(|(k, _)| SEGMENT_BOXES.contains(&k))
    {
        return None;
    }
    let start = starts[first_media].1;
    (start > 0).then_some(start)
}

/// (type, start) of every top-level MP4 box, one after the other (headers
/// may be 8 or 16 bytes). A last box cut off (a chunked fetch) counts by its
/// type, as in the hook.
pub fn box_starts(payload: &[u8]) -> Vec<([u8; 4], usize)> {
    let complete = boxes_in(payload, 0, payload.len());
    let mut starts: Vec<([u8; 4], usize)> = Vec::with_capacity(complete.len() + 1);
    let mut at = 0;
    for (k, _, end) in &complete {
        starts.push((*k, at));
        at = *end;
    }
    if let Some(t) = payload.get(at + 4..at + 8) {
        starts.push(([t[0], t[1], t[2], t[3]], at));
    }
    starts
}

/// The media part of a split init+media append holds samples (a `moof` or
/// `mdat`, a `WebM` Cluster), not only boxes in front of them (`styp`,
/// `emsg`, `prft`, `sidx`), which an append can end with (a chunked fetch).
pub fn has_samples(kind: Container, media: &[u8]) -> bool {
    kind.is_webm()
        || box_starts(media)
            .iter()
            .any(|(k, _)| k == b"moof" || k == b"mdat")
}

/// Segment boxes without samples held for the next media append (bytes).
pub const MAX_SEGMENT_HEAD: usize = 64 * 1024;

/// Boxes that go in front of a segment's samples.
pub const HEAD_BOXES: [&[u8; 4]; 5] = [b"styp", b"sidx", b"ssix", b"emsg", b"prft"];

/// A media append made only of complete boxes that go in front of samples
/// (a `styp` appended on its own).
/// The payload opens like media, not like the rest of an init: a `moof`,
/// `styp` or `mdat` box, or a `WebM` `Cluster`.
pub fn opens_like_media(payload: &[u8]) -> bool {
    matches!(payload.get(4..8), Some(b"moof" | b"styp" | b"mdat"))
        || payload.starts_with(&[0x1f, 0x43, 0xb6, 0x75])
}

pub fn is_segment_head(payload: &[u8]) -> bool {
    let boxes = boxes_in(payload, 0, payload.len());
    boxes
        .last()
        .is_some_and(|(_, _, end)| *end == payload.len())
        && boxes.iter().all(|(k, _, _)| HEAD_BOXES.contains(&k))
}

/// Boxes in front of the samples appended without them, held for the next
/// media append of the `SourceBuffer` (`Session::join_segment_head`).
pub struct SegmentHead {
    pub bytes: Vec<u8>,
    /// The append came after a set offset (older extensions flag it there).
    pub tso_set: bool,
    pub seq: u64,
}

/// The init bytes so far hold a complete `moov` (MP4) or `Tracks` (`WebM`):
/// nothing more of this init is to come.
pub fn init_complete(kind: Container, bytes: &[u8]) -> bool {
    init_scan(kind, bytes, 0).0
}

/// Scans init bytes for a complete `moov` (MP4) or `Tracks` (`WebM`) from
/// `from`, the start of the first box or element not yet complete. Returns
/// whether it was found and where the next scan resumes, so bytes arriving
/// in many chunks are each walked once.
pub fn init_scan(kind: Container, bytes: &[u8], from: usize) -> (bool, usize) {
    let mut off = from;
    if kind.is_webm() {
        while off < bytes.len() {
            let Some((id, il)) = read_vint(bytes, off, true) else {
                return (false, off);
            };
            let Some((size, sl)) = read_vint(bytes, off + il, false) else {
                return (false, off);
            };
            let body = off + il + sl;
            let unknown = sl < 8 && size == (1u64 << (7 * sl)) - 1;
            if id == 0x1853_8067 || unknown {
                off = body; // descend into Segment
                continue;
            }
            let Some(end) = usize::try_from(size).ok().and_then(|s| body.checked_add(s)) else {
                return (false, off);
            };
            if end > bytes.len() {
                return (false, off);
            }
            if id == 0x1654_ae6b {
                return (true, end);
            }
            off = end;
        }
        return (false, off);
    }
    while off + 8 <= bytes.len() {
        let size32 =
            u32::from_be_bytes([bytes[off], bytes[off + 1], bytes[off + 2], bytes[off + 3]]);
        let (size, header) = match size32 {
            // Up to the end of the stream: never complete before it ends.
            0 => return (false, off),
            1 => {
                let Some(b) = bytes.get(off + 8..off + 16) else {
                    return (false, off);
                };
                let mut v = [0u8; 8];
                v.copy_from_slice(b);
                match usize::try_from(u64::from_be_bytes(v)) {
                    Ok(s) => (s, 16),
                    Err(_) => return (false, off),
                }
            }
            s => (s as usize, 8),
        };
        let Some(end) = off
            .checked_add(size)
            .filter(|e| size >= header && *e <= bytes.len())
        else {
            return (false, off);
        };
        if &bytes[off + 4..off + 8] == b"moov" {
            return (true, end);
        }
        off = end;
    }
    (false, off)
}

/// `WebM`/Matroska encryption in init bytes: the element IDs
/// `ContentEncodings` (0x6D80), `ContentEncryption` (0x5035) and
/// `ContentEncKeyID` (0x47E2) all present (clear data practically never has
/// all three; the extension reads the structure where it can).
pub fn webm_encrypted(bytes: &[u8]) -> bool {
    let has = |a: u8, b: u8| bytes.windows(2).any(|w| w[0] == a && w[1] == b);
    has(0x6d, 0x80) && has(0x50, 0x35) && has(0x47, 0xe2)
}

/// True when an init segment declares protected tracks (ISO BMFF `encv`,
/// `enca`, `sinf`/`tenc` or a `pssh` box). Such content is DRM: streamtap
/// leaves it alone instead of storing ciphertext.
pub fn looks_encrypted(init: &[u8]) -> bool {
    const MARKERS: [&[u8; 4]; 4] = [b"encv", b"enca", b"tenc", b"pssh"];
    init.windows(4)
        .any(|w| MARKERS.iter().any(|m| w == m.as_slice()))
}

/// Top-level ISO BMFF boxes in `buf[start..end]` as (type, body start, end).
pub fn boxes_in(buf: &[u8], start: usize, end: usize) -> Vec<([u8; 4], usize, usize)> {
    let mut out = Vec::new();
    let mut off = start;
    while off + 8 <= end {
        let size32 = u32::from_be_bytes([buf[off], buf[off + 1], buf[off + 2], buf[off + 3]]);
        let kind = [buf[off + 4], buf[off + 5], buf[off + 6], buf[off + 7]];
        let (size, header) = match size32 {
            0 => (end - off, 8),
            1 => {
                if off + 16 > end {
                    break;
                }
                let mut b = [0u8; 8];
                b.copy_from_slice(&buf[off + 8..off + 16]);
                match usize::try_from(u64::from_be_bytes(b)) {
                    Ok(s) => (s, 16),
                    Err(_) => break,
                }
            }
            s => (s as usize, 8),
        };
        if size < header || off.saturating_add(size) > end {
            break;
        }
        out.push((kind, off + header, off + size));
        off += size;
    }
    out
}

/// Adds `delta` to every `moof/traf/tfdt` of one media segment. Returns
/// false (and leaves the segment unchanged) when a result would be negative
/// or not fit a version-0 box.
pub fn shift_tfdt(seg: &mut [u8], delta: i64) -> bool {
    shift_tfdt_by(seg, &|_| delta)
}

/// Like `shift_tfdt`, with the delta per `traf` from its `tfhd` track ID:
/// each track counts in its own timescale.
pub fn shift_tfdt_by(seg: &mut [u8], delta_of: &dyn Fn(Option<u32>) -> i64) -> bool {
    let mut fields = Vec::new(); // (offset, is_64bit, delta)
    for (kind, s, e) in boxes_in(seg, 0, seg.len()) {
        if &kind != b"moof" {
            continue;
        }
        for (kind, s, e) in boxes_in(seg, s, e) {
            if &kind != b"traf" {
                continue;
            }
            let children = boxes_in(seg, s, e);
            let track = children
                .iter()
                .find(|(kind, s, e)| kind == b"tfhd" && *e >= s + 8)
                .map(|(_, s, _)| {
                    u32::from_be_bytes([seg[s + 4], seg[s + 5], seg[s + 6], seg[s + 7]])
                });
            let delta = delta_of(track);
            for (kind, s, e) in children {
                if &kind == b"tfdt" && e >= s + 8 {
                    let v1 = seg[s] == 1;
                    if !v1 || e >= s + 12 {
                        fields.push((s + 4, v1, delta));
                    }
                }
            }
        }
    }
    let mut writes = Vec::new();
    for &(off, v1, delta) in &fields {
        let old = if v1 {
            let mut b = [0u8; 8];
            b.copy_from_slice(&seg[off..off + 8]);
            i128::from(u64::from_be_bytes(b))
        } else {
            i128::from(u32::from_be_bytes([
                seg[off],
                seg[off + 1],
                seg[off + 2],
                seg[off + 3],
            ]))
        };
        let new = old + i128::from(delta);
        let fits = if v1 {
            u64::try_from(new).is_ok()
        } else {
            u32::try_from(new).is_ok()
        };
        if !fits {
            return false;
        }
        writes.push((off, v1, new));
    }
    for (off, v1, new) in writes {
        if v1 {
            let v = u64::try_from(new).expect("checked above");
            seg[off..off + 8].copy_from_slice(&v.to_be_bytes());
        } else {
            let v = u32::try_from(new).expect("checked above");
            seg[off..off + 4].copy_from_slice(&v.to_be_bytes());
        }
    }
    !fields.is_empty()
}

/// Track ID -> `mdhd` timescale of every `trak` in an init's `moov`.
pub fn trak_timescales(init: &[u8]) -> HashMap<u32, u32> {
    let be32 = |b: &[u8], o: usize| -> Option<u32> {
        b.get(o..o + 4)
            .map(|x| u32::from_be_bytes([x[0], x[1], x[2], x[3]]))
    };
    let mut out = HashMap::new();
    for (kind, s, e) in boxes_in(init, 0, init.len()) {
        if &kind != b"moov" {
            continue;
        }
        for (kind, s, e) in boxes_in(init, s, e) {
            if &kind != b"trak" {
                continue;
            }
            let mut id = None;
            let mut ts = None;
            for (kind, s, e) in boxes_in(init, s, e) {
                if &kind == b"tkhd" {
                    // fullbox; v0: ctime, mtime (4+4); v1: (8+8); then track_ID
                    let at = if init.get(s) == Some(&1) {
                        s + 20
                    } else {
                        s + 12
                    };
                    id = be32(&init[..e], at);
                } else if &kind == b"mdia" {
                    for (kind, s, e) in boxes_in(init, s, e) {
                        if &kind == b"mdhd" {
                            let at = if init.get(s) == Some(&1) {
                                s + 20
                            } else {
                                s + 12
                            };
                            ts = be32(&init[..e], at);
                        }
                    }
                }
            }
            if let (Some(id), Some(ts)) = (id, ts) {
                out.insert(id, ts);
            }
        }
    }
    out
}

/// Number of `trak` boxes in the `moov` of an init segment.
pub fn trak_count(init: &[u8]) -> usize {
    boxes_in(init, 0, init.len())
        .iter()
        .filter(|(kind, _, _)| kind == b"moov")
        .map(|(_, s, e)| {
            boxes_in(init, *s, *e)
                .iter()
                .filter(|(kind, _, _)| kind == b"trak")
                .count()
        })
        .sum()
}

/// Big-endian integer of `n` bytes (at most 8) at `at`.
pub fn be_uint(b: &[u8], at: usize, n: usize) -> Option<u64> {
    b.get(at..at + n)
        .map(|x| x.iter().fold(0u64, |v, byte| (v << 8) | u64::from(*byte)))
}

/// Composition offset of the first sample of a media segment (first `trun`
/// of the first `traf`), in its track's ticks; 0 when it has none.
pub fn first_cto(seg: &[u8]) -> i64 {
    let Some((_, ms, me)) = boxes_in(seg, 0, seg.len())
        .into_iter()
        .find(|(k, _, _)| k == b"moof")
    else {
        return 0;
    };
    let Some((_, ts, te)) = boxes_in(seg, ms, me)
        .into_iter()
        .find(|(k, _, _)| k == b"traf")
    else {
        return 0;
    };
    let Some((_, s, e)) = boxes_in(seg, ts, te)
        .into_iter()
        .find(|(k, _, _)| k == b"trun")
    else {
        return 0;
    };
    let trun = &seg[s..e];
    let Some(flags) = be_uint(trun, 1, 3) else {
        return 0;
    };
    if flags & 0x800 == 0 {
        return 0;
    }
    // sample_count, then optional data_offset and first_sample_flags, then
    // the first sample's duration, size and flags before its offset.
    let mut at = 8;
    for (bit, len) in [(0x1, 4), (0x4, 4), (0x100, 4), (0x200, 4), (0x400, 4)] {
        if flags & bit != 0 {
            at += len;
        }
    }
    let Some(raw) = be_uint(trun, at, 4) else {
        return 0;
    };
    // Signed in both versions, as Chrome and ffmpeg read it (a version-0 box
    // with a negative offset is out of spec but written by some muxers).
    let raw = u32::try_from(raw).expect("4 bytes");
    i64::from(i32::from_be_bytes(raw.to_be_bytes()))
}

/// `media_time` of the first non-empty edit of the first track's edit list
/// (`moov/trak/edts/elst`), in its media ticks; 0 without one.
pub fn elst_media_time(init: &[u8]) -> i64 {
    let find = |s: usize, e: usize, kind: &[u8; 4]| {
        boxes_in(init, s, e)
            .into_iter()
            .find(|(k, _, _)| k == kind)
            .map(|(_, s, e)| (s, e))
    };
    let Some((s, e)) = find(0, init.len(), b"moov")
        .and_then(|(s, e)| find(s, e, b"trak"))
        .and_then(|(s, e)| find(s, e, b"edts"))
        .and_then(|(s, e)| find(s, e, b"elst"))
    else {
        return 0;
    };
    let elst = &init[s..e];
    let v1 = elst.first() == Some(&1);
    let count = be_uint(elst, 4, 4).unwrap_or(0);
    // segment_duration, media_time, media_rate (16.16).
    let entry = if v1 { 20 } else { 12 };
    for i in 0..count.min(16) {
        let at = 8 + usize::try_from(i).expect("small") * entry;
        let media_time = if v1 {
            be_uint(elst, at + 8, 8).map(|v| i64::from_be_bytes(v.to_be_bytes()))
        } else {
            be_uint(elst, at + 4, 4).map(|v| {
                i64::from(i32::from_be_bytes(
                    u32::try_from(v).expect("4 bytes").to_be_bytes(),
                ))
            })
        };
        match media_time {
            Some(-1) => {} // an empty edit
            Some(t) => return t,
            None => return 0,
        }
    }
    0
}
