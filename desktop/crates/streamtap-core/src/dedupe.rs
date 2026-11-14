//! Has this media segment been written before: the keys and the decision.

use std::collections::HashMap;

use crate::wire::SegmentHeader;

/// Dedupe keys: (tfdt, timestampOffset in ms, duration, track ID), or in
/// sequence mode (payload hash, `i64::MIN`, length, 0); each with the offset
/// of its row in the part file (a sequence-mode duplicate points there,
/// [`Admission::Mark`]). The track keeps the appends of two tracks of one
/// `SourceBuffer` that share a start and duration apart (0: no track ID).
pub type SeenSet = HashMap<(u64, i64, u64, u32), u64>;

/// `timestampOffset` in whole milliseconds; non-finite values count as 0.
pub fn tso_millis(tso: Option<f64>) -> i64 {
    let v = tso.filter(|v| v.is_finite()).unwrap_or(0.0) * 1000.0;
    // Clamped to about ±31 years, so the cast below cannot truncate.
    #[allow(clippy::cast_possible_truncation)]
    let ms = v.clamp(-1e12, 1e12).round() as i64;
    ms
}

pub fn fnv1a(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325_u64, |h, b| {
        (h ^ u64::from(*b)).wrapping_mul(0x0100_0000_01b3)
    })
}

/// The chunk starts with a `moof` whose `mfhd` (and so its sequence number) arrived in full.
pub fn starts_a_numbered_moof(payload: &[u8]) -> bool {
    payload.len() >= 24 && &payload[4..8] == b"moof" && &payload[12..16] == b"mfhd"
}

/// What [`Dedupe::admit`] decided for one media append.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Admission {
    /// New: write it.
    Write,
    /// A sequence-mode duplicate: not written again, but its place in the timeline is (an index line pointing at
    /// the row at `offset`). The browser placed it, and the next append continues after it.
    Mark { offset: u64 },
    /// Written before (or the continuation of such a segment): left out.
    Duplicate,
    /// New, but the session remembers as many segments as it may: left out.
    NoRoom,
}

/// The media segments of one part file that were written, and whether the untimed chunks arriving now continue a
/// duplicate.
#[derive(Debug, Default)]
pub struct Dedupe {
    pub seen: SeenSet,
    /// The last timed chunk was a duplicate: the untimed chunks that follow it (its continuation) are duplicates
    /// too.
    pub skip_tail: bool,
}

impl Dedupe {
    /// Decides about a media segment (timed) or an untimed continuation. `offset` is where the row would be
    /// written in the part file; `room` says whether one more segment may be remembered.
    ///
    /// The key of a timed segment is (`tfdt`, `timestampOffset`, duration, track ID): players re-fetch ranges
    /// after a seek, multi-period streams restart `tfdt` and move the timeline with the offset, and two tracks of
    /// one `SourceBuffer` may share start and duration. In sequence mode the browser places media one after the
    /// other and a later period may reuse `tfdt` values: a re-append has the same bytes, a new period other
    /// bytes, so the key there is the payload.
    pub fn admit(
        &mut self,
        h: &SegmentHeader,
        payload: &[u8],
        offset: u64,
        room: bool,
    ) -> Admission {
        let Some(t) = h.tfdt else {
            // A segment cut before its tfdt: the sequence number of the moof makes the head the same bytes only
            // when it is the same segment, fetched again.
            if starts_a_numbered_moof(payload) {
                let key = (fnv1a(payload), i64::MIN + 1, payload.len() as u64, 0);
                self.skip_tail = self.seen.contains_key(&key);
                if self.skip_tail {
                    return Admission::Duplicate;
                }
                if !room {
                    return Admission::NoRoom;
                }
                self.seen.insert(key, offset);
                return Admission::Write;
            }
            return if self.skip_tail {
                Admission::Duplicate
            } else {
                Admission::Write
            };
        };
        self.skip_tail = false;
        let key = if h.seq_mode {
            (fnv1a(payload), i64::MIN, payload.len() as u64, 0)
        } else {
            (
                t,
                tso_millis(h.tso),
                h.dur.unwrap_or(0),
                h.track.unwrap_or(0),
            )
        };
        if let Some(&offset) = self.seen.get(&key) {
            self.skip_tail = true;
            return if h.seq_mode {
                Admission::Mark { offset }
            } else {
                Admission::Duplicate
            };
        }
        if !room {
            return Admission::NoRoom;
        }
        self.seen.insert(key, offset);
        Admission::Write
    }

    /// Whether an append [`Dedupe::admit`] let through took a place in the set (an untimed continuation does
    /// not).
    pub fn remembers(h: &SegmentHeader, payload: &[u8]) -> bool {
        h.tfdt.is_some() || starts_a_numbered_moof(payload)
    }
}
