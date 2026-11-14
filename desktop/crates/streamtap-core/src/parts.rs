//! Which `MediaSource` of a capture and which part of each `SourceBuffer` is muxed.

use std::collections::HashMap;

use crate::wire::IndexRow;

/// The parts of one `SourceBuffer` that may be muxed ([`rank_parts`]).
pub struct Candidates {
    pub sb: u32,
    /// All its parts, with or without an init.
    pub total: usize,
    /// (part, bytes, MIME) of the parts with an init segment, best first
    /// (plausible, most claimed media time, most bytes).
    pub ranked: Vec<(u32, u64, String)>,
}

/// What [`rank_parts`] knows about one part.
#[derive(Default)]
pub struct PartStat {
    pub bytes: u64,
    pub has_init: bool,
    pub mime: String,
    /// Media time the segments claim (ms), per track: the tracks of a
    /// `SourceBuffer` run in parallel, so they are not added up.
    pub ms_by_track: HashMap<Option<u32>, u64>,
}

/// Bounds for bytes per second of claimed media time: below the lowest a
/// real stream reaches (a few kbit/s audio) the time was inflated or the
/// bytes cut off, above the highest (800 Mbit/s) the bytes were padded.
pub const MIN_BYTES_PER_SEC: u64 = 100;

pub const MAX_BYTES_PER_SEC: u64 = 100_000_000;

impl PartStat {
    pub fn media_ms(&self) -> u64 {
        self.ms_by_track.values().copied().max().unwrap_or(0)
    }

    /// No claimed time (nothing to check) or one that fits the bytes.
    pub fn plausible(&self) -> bool {
        let ms = self.media_ms();
        if ms == 0 {
            return true;
        }
        let per_sec = u128::from(self.bytes) * 1000 / u128::from(ms);
        (u128::from(MIN_BYTES_PER_SEC)..=u128::from(MAX_BYTES_PER_SEC)).contains(&per_sec)
    }
}

/// What [`rank_parts`] found.
pub struct Ranked {
    /// Per `SourceBuffer` with at least one part that has an init: the parts to try, best first.
    pub candidates: Vec<Candidates>,
    /// The `SourceBuffer`s without any part with an init segment (nothing of them can be muxed).
    pub without_init: Vec<u32>,
}

/// Per `SourceBuffer` the parts to try, best first. Adaptive players often start at a low quality and switch after
/// the first segment, which starts a new part: the parts with an init segment, the most plausible first (see
/// [`PartStat`]). The first one the mux can use is taken.
pub fn rank_parts(stored: &[&IndexRow]) -> Ranked {
    let mut parts: HashMap<(u32, u32), PartStat> = HashMap::new();
    for row in stored {
        let e = parts.entry((row.sb, row.part)).or_insert_with(|| PartStat {
            mime: row.mime.clone(),
            ..PartStat::default()
        });
        e.bytes += row.len;
        e.has_init |= row.is_init;
        if let (false, Some(dur), Some(ts)) = (row.is_init, row.dur, row.timescale) {
            if ts > 0 {
                let ms = u64::try_from(u128::from(dur) * 1000 / u128::from(ts)).unwrap_or(u64::MAX);
                let t = e.ms_by_track.entry(row.track).or_insert(0);
                *t = t.saturating_add(ms);
            }
        }
    }
    let mut ids: Vec<u32> = parts.keys().map(|(sb, _)| *sb).collect();
    ids.sort_unstable();
    ids.dedup();
    let mut found = Ranked {
        candidates: Vec::new(),
        without_init: Vec::new(),
    };
    for id in ids {
        let total = parts.keys().filter(|(sb, _)| *sb == id).count();
        let mut stats: Vec<(u32, &PartStat)> = parts
            .iter()
            .filter(|((sb, _), st)| *sb == id && st.has_init)
            .map(|((_, part), st)| (*part, st))
            .collect();
        // Plausible parts first, then the most media time, then the most
        // bytes, the later part at equal size. Bytes alone can be padded or
        // cut short by the page; the time the segments claim has to fit them.
        stats.sort_unstable_by_key(|(part, st)| {
            (
                !st.plausible(),
                std::cmp::Reverse(st.media_ms()),
                std::cmp::Reverse(st.bytes),
                std::cmp::Reverse(*part),
            )
        });
        let ranked: Vec<(u32, u64, String)> = stats
            .iter()
            .map(|(part, st)| (*part, st.bytes, st.mime.clone()))
            .collect();
        if ranked.is_empty() {
            found.without_init.push(id);
            continue;
        }
        found.candidates.push(Candidates {
            sb: id,
            total,
            ranked,
        });
    }
    found
}

/// The `MediaSource` a capture is muxed from, and the ones left out.
#[derive(Debug, PartialEq, Eq)]
pub struct MainSource {
    pub chosen: Option<u32>,
    /// (`MediaSource`, bytes) of the others, in order of their id (`None` first).
    pub others: Vec<(Option<u32>, u64)>,
}

/// One `MediaSource` is one video. A preroll ad, the next video of a single-page app or a hover preview adds more
/// `MediaSource`s to the same session: the one with the most bytes is the video (the lower id at equal bytes).
/// `None` without rows. A mark repeats bytes stored once: counting it would let a looping teaser outweigh the
/// video.
pub fn main_media_source(rows: &[IndexRow]) -> Option<MainSource> {
    let mut per_ms: HashMap<Option<u32>, u64> = HashMap::new();
    for r in rows.iter().filter(|r| !r.mark) {
        *per_ms.entry(r.ms).or_default() += r.len;
    }
    let (&chosen, _) = per_ms
        .iter()
        .max_by_key(|(ms, bytes)| (**bytes, std::cmp::Reverse(**ms)))?;
    let mut others: Vec<(Option<u32>, u64)> = per_ms
        .iter()
        .filter(|(ms, _)| **ms != chosen)
        .map(|(ms, b)| (*ms, *b))
        .collect();
    others.sort_unstable();
    Some(MainSource { chosen, others })
}
