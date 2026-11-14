//! Where a row lies on the timeline: offsets, the sequence-mode layout, order, coverage and gaps.

use std::collections::HashMap;

use crate::fmp4::Container;
use crate::wire::IndexRow;

/// `timestampOffset` (seconds) as ticks of `timescale`, rounded once from
/// the exact product (no millisecond step in between).
pub fn shift_ticks(tso: Option<f64>, timescale: Option<u32>) -> i64 {
    match (tso.filter(|v| v.is_finite()), timescale) {
        (Some(o), Some(ts)) if ts > 0 => {
            // Clamped well inside i64, so the cast cannot truncate.
            #[allow(clippy::cast_possible_truncation)]
            let ticks = (o * f64::from(ts)).round().clamp(-9e15, 9e15) as i64;
            ticks
        }
        _ => 0,
    }
}

/// Ticks of `from` as ticks of `to`, rounded (0 without a source timescale).
pub fn rescale(ticks: i64, from: Option<u32>, to: u32) -> i64 {
    let Some(src) = from.filter(|s| *s > 0) else {
        return 0;
    };
    let num = i128::from(ticks) * i128::from(to);
    let den = i128::from(src);
    let quot = (num + num.signum() * den / 2) / den;
    i64::try_from(quot.clamp(i128::from(i64::MIN), i128::from(i64::MAX))).expect("clamped")
}

/// Moves of sequence-mode media rows, per (part, offset), in ticks of each
/// row's timescale.
pub type SeqMoves = HashMap<(u32, u64), i64>;

/// Sequence mode: where the browser places each append (MSE coded frame
/// processing), per `SourceBuffer` over all its parts in arrival order (an
/// ABR switch and back keeps one timeline). After the player set
/// `timestampOffset` (`tso_set`: a seek, a resync) the append starts at that
/// offset, the group start. Otherwise it starts at its `tfdt` plus the offset
/// in effect, unless that does not continue within 100 ms of where the
/// previous append ended (a discontinuity, a new period restarting `tfdt`):
/// then the browser starts it right there. The browser puts the first
/// frame's presentation time at the group start: `leads` holds, per row, how
/// far that lies after its `tfdt` (composition offset of the first sample
/// minus the edit list's `media_time`). A row without `tfdt`, duration or
/// timescale gets no move and ends the group (the next one is not checked
/// against it). A duplicate the sink did not store again (`mark`) is placed
/// like any append, so the next one continues after it, but keeps the move
/// of the row it repeats. Returns the moves and the number of
/// discontinuities.
pub fn seq_layout(rows: &[&IndexRow], leads: &SeqMoves) -> (SeqMoves, usize) {
    const NS: i128 = 1_000_000_000;
    let mut moves = HashMap::new();
    let mut group_end: Option<i128> = None;
    let mut jumps = 0;
    for r in rows.iter().filter(|r| !r.is_init && r.seq_mode) {
        let (Some(t), Some(d), Some(ts)) = (
            r.tfdt,
            r.dur.filter(|d| *d > 0),
            r.timescale.filter(|t| *t > 0),
        ) else {
            group_end = None;
            continue;
        };
        let ts = i128::from(ts);
        let t_ns = i128::from(t) * NS / ts;
        let tso_ns = i128::from(shift_ticks(r.tso, Some(1_000_000_000)));
        let start = if r.tso_set {
            let lead = leads.get(&(r.part, r.offset)).copied().unwrap_or(0);
            tso_ns - i128::from(lead) * NS / ts
        } else {
            let s = t_ns + tso_ns;
            match group_end {
                Some(e) if (s - e).abs() > NS / 10 => {
                    jumps += 1;
                    e
                }
                _ => s,
            }
        };
        group_end = Some(start + i128::from(d) * NS / ts);
        if r.mark {
            continue;
        }
        let num = (start - t_ns) * ts;
        let ticks = (num + num.signum() * NS / 2) / NS;
        let clamped = ticks.clamp(i128::from(i64::MIN), i128::from(i64::MAX));
        moves.insert((r.part, r.offset), i64::try_from(clamped).expect("clamped"));
    }
    (moves, jumps)
}

/// The init rows of a part: its first run of consecutive init rows (an init
/// split across appends). An init row after media is a stray piece of
/// another copy (an older sink stored the overflow of a duplicate init so):
/// joined in, it would break the init, and a page could grow it without
/// bound.
pub fn init_run<'a>(rows: &[&'a IndexRow]) -> Vec<&'a IndexRow> {
    rows.iter()
        .copied()
        .skip_while(|r| !r.is_init)
        .take_while(|r| r.is_init)
        .collect()
}

/// The lift (nanoseconds) for all `SourceBuffer`s. A frame placed by its
/// offset may need a decode time before 0 (B-frames: at a set offset the
/// browser puts the presentation time there; a negative `timestampOffset`),
/// which an MP4 cannot hold: then every `SourceBuffer` moves later by the
/// same amount, so their alignment stays. Only the `muxed` (`SourceBuffer`,
/// part) pairs count: a part left out (another rendition, a forged or
/// undecodable one) moves nothing.
pub fn seq_lift(rows: &[IndexRow], layouts: &HashMap<u32, SeqMoves>, muxed: &[(u32, u32)]) -> i64 {
    const NS: i128 = 1_000_000_000;
    let no_moves = SeqMoves::new();
    let mut earliest: i128 = 0;
    for r in rows.iter().filter(|r| !r.is_init && !r.mark) {
        if !muxed.contains(&(r.sb, r.part)) {
            continue;
        }
        let (Some(t), Some(ts)) = (r.tfdt, r.timescale.filter(|t| *t > 0)) else {
            continue;
        };
        let m = if r.seq_mode {
            let moves = layouts.get(&r.sb).unwrap_or(&no_moves);
            moves.get(&(r.part, r.offset)).copied().unwrap_or(0)
        } else {
            shift_ticks(r.tso, Some(ts))
        };
        earliest = earliest.min((i128::from(t) + i128::from(m)) * NS / i128::from(ts));
    }
    i64::try_from((-earliest).clamp(0, i128::from(i64::MAX))).expect("clamped")
}

/// `ns` nanoseconds as ticks of `ts`, rounded up (0 without a timescale).
pub fn ns_ticks(ns: i64, ts: Option<u32>) -> i64 {
    let Some(ts) = ts.filter(|t| *t > 0) else {
        return 0;
    };
    let ticks = (i128::from(ns) * i128::from(ts) + 999_999_999) / 1_000_000_000;
    i64::try_from(ticks.clamp(i128::from(i64::MIN), i128::from(i64::MAX))).expect("clamped")
}

/// Ranges skipped by a forward seek in the kept segments, sorted by start,
/// as (start, duration) in track ticks: (count, missing ticks, exact).
///
/// With a duration on every segment the gaps are exact: a start past the
/// end of everything before it (a running end, so a longer earlier segment
/// causes no false gap), beyond a tolerance for rounding. Without, a step
/// well above the typical (lower median) step counts, measured against it;
/// gaps that make up most of the steps are missed there.
pub fn find_gaps(segs: &[(i128, Option<u64>)], ts: u32) -> (usize, u128, bool) {
    let mut durs: Vec<u64> = segs.iter().filter_map(|s| s.1.filter(|d| *d > 0)).collect();
    if !segs.is_empty() && durs.len() == segs.len() {
        durs.sort_unstable();
        // 50 ms, or 1 % of a typical segment, absorbs frame rounding.
        let median_dur = durs[(durs.len() - 1) / 2];
        let tolerance = i128::from((ts / 20).max(1)).max(i128::from(median_dur / 100));
        let (mut count, mut missing) = (0, 0u128);
        let mut covered_end = segs[0].0;
        for &(start, dur) in segs {
            let gap = start - covered_end;
            if gap > tolerance {
                count += 1;
                missing = missing.saturating_add(gap.unsigned_abs());
            }
            covered_end = covered_end.max(start + i128::from(dur.unwrap_or(0)));
        }
        return (count, missing, true);
    }
    let steps: Vec<u128> = segs
        .windows(2)
        .filter_map(|w| u128::try_from(w[1].0 - w[0].0).ok())
        .collect();
    let mut typical = steps.clone();
    typical.sort_unstable();
    let Some(&median) = typical.get(typical.len().saturating_sub(1) / 2) else {
        return (0, 0, false);
    };
    let threshold = median.saturating_add(median / 2);
    let big: Vec<u128> = steps.into_iter().filter(|s| *s > threshold).collect();
    let missing = big
        .iter()
        .fold(0u128, |acc, s| acc.saturating_add(s - median));
    (big.len(), missing, false)
}

/// The rows split by the track their `tfdt` belongs to, each group in the
/// given order; groups in order of first appearance. Rows without a track ID
/// (older captures, `WebM`) share one group.
pub fn by_track(rows: Vec<&IndexRow>) -> Vec<(Option<u32>, Vec<&IndexRow>)> {
    let mut groups: Vec<(Option<u32>, Vec<&IndexRow>)> = Vec::new();
    for r in rows {
        match groups.iter_mut().find(|(t, _)| *t == r.track) {
            Some((_, g)) => g.push(r),
            None => groups.push((r.track, vec![r])),
        }
    }
    groups
}

/// Leaves out segments (sorted by start) that earlier ones fully cover, when
/// every segment has a duration. Returns the kept ones and the counts of
/// covered and partially overlapping segments.
pub fn drop_covered<'a>(
    order: Vec<&'a IndexRow>,
    pts: &dyn Fn(&IndexRow) -> i128,
) -> (Vec<&'a IndexRow>, usize, usize) {
    if !order.iter().all(|r| r.dur.is_some_and(|d| d > 0)) {
        return (order, 0, 0);
    }
    let (mut covered, mut partial) = (0, 0);
    let mut kept = Vec::with_capacity(order.len());
    let mut covered_end = i128::MIN;
    for r in order {
        let start = pts(r);
        let end = start + i128::from(r.dur.unwrap_or(0));
        if end <= covered_end {
            covered += 1;
            continue;
        }
        if start < covered_end {
            partial += 1;
        }
        covered_end = covered_end.max(end);
        kept.push(r);
    }
    (kept, covered, partial)
}

/// Whether the part's init is `WebM`.
pub fn is_webm(rows: &[&IndexRow]) -> bool {
    rows.iter()
        .find(|r| r.is_init)
        .is_some_and(|r| Container::from_kind(&r.kind) == Container::Webm)
}

/// The same rows (not equal ones) in the same order.
pub fn same_rows(a: &[&IndexRow], b: &[&IndexRow]) -> bool {
    a.len() == b.len() && a.iter().zip(b).all(|(x, y)| std::ptr::eq(*x, *y))
}

/// Ranges skipped by a forward seek in one track of a part ([`find_gaps`]), in ticks of `scale`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Gaps {
    pub count: usize,
    pub missing: u128,
    /// From durations; otherwise estimated from the typical step.
    pub exact: bool,
    pub scale: u32,
}

impl Gaps {
    /// The missing time in seconds, for a message (precision far beyond what it shows).
    pub fn seconds(&self) -> f64 {
        #[allow(clippy::cast_precision_loss)]
        let seconds = self.missing as f64 / f64::from(self.scale);
        seconds
    }
}

/// What [`cover_per_track`] found.
pub struct Covered<'a> {
    /// The kept rows, sorted by start (the longer one first at the same start).
    pub rows: Vec<&'a IndexRow>,
    /// Segments left out because earlier ones cover them.
    pub covered: usize,
    /// Segments kept although they overlap an earlier one in part.
    pub partial: usize,
    /// The gaps of every track that has some, in order of the tracks' first appearance.
    pub gaps: Vec<Gaps>,
}

/// Drops the segments earlier ones cover and finds the gaps, per track: an append that begins with another track
/// of the `SourceBuffer` carries that track's time and duration. `order` is sorted by start; the result is too.
/// `track_ts` holds the timescale of each track of the init, `ts` the one of the part's rows (for rows without a
/// track ID).
pub fn cover_per_track<'a>(
    order: Vec<&'a IndexRow>,
    pts: &dyn Fn(&IndexRow) -> i128,
    track_ts: &HashMap<u32, u32>,
    ts: Option<u32>,
) -> Covered<'a> {
    let mut out = Covered {
        rows: Vec::with_capacity(order.len()),
        covered: 0,
        partial: 0,
        gaps: Vec::new(),
    };
    for (track, group) in by_track(order) {
        let (kept, c, p) = drop_covered(group, pts);
        out.covered += c;
        out.partial += p;
        let scale = track.and_then(|t| track_ts.get(&t).copied()).or(ts);
        if let Some(scale) = scale {
            let segs: Vec<(i128, Option<u64>)> = kept.iter().map(|r| (pts(r), r.dur)).collect();
            let (count, missing, exact) = find_gaps(&segs, scale);
            if count > 0 {
                out.gaps.push(Gaps {
                    count,
                    missing,
                    exact,
                    scale,
                });
            }
        }
        out.rows.extend(kept);
    }
    out.rows
        .sort_by_key(|r| (pts(r), std::cmp::Reverse(r.dur.unwrap_or(0))));
    out
}

/// What the mux knows of one part before it orders it: how far each of its rows moves on the timeline.
#[derive(Debug, Clone, Copy)]
pub struct PartClock<'a> {
    pub part: u32,
    /// The timescale of the part's rows (the media rows carry the one of their `tfdt`).
    pub ts: Option<u32>,
    /// How far every `SourceBuffer` moves later so that none decodes before 0 ([`seq_lift`]).
    pub lift_ns: i64,
    /// The sequence-mode layout of the part's `SourceBuffer` ([`seq_layout`]).
    pub seq_moves: Option<&'a SeqMoves>,
    /// `trak` boxes in the part's init (0: none found).
    pub raw_tracks: usize,
    /// Track ID -> timescale of the init's tracks.
    pub track_ts: &'a HashMap<u32, u32>,
}

impl PartClock<'_> {
    /// Tracks the part counts with: at least one.
    pub fn tracks(&self) -> usize {
        self.raw_tracks.max(1)
    }

    /// How far `r` moves, in ticks of the part's timescale: the lift plus its place in the sequence-mode layout,
    /// or its `timestampOffset`.
    pub fn offset(&self, r: &IndexRow) -> i64 {
        ns_ticks(self.lift_ns, self.ts)
            + if r.seq_mode {
                self.seq_moves
                    .and_then(|m| m.get(&(self.part, r.offset)))
                    .copied()
                    .unwrap_or(0)
            } else {
                shift_ticks(r.tso, self.ts)
            }
    }

    /// The `tfdt` can be rewritten: one timescale holds for a whole segment, or each track's is known.
    pub fn can_shift(&self) -> bool {
        self.tracks() == 1 || (self.raw_tracks > 0 && self.track_ts.len() == self.raw_tracks)
    }

    /// Where `r` starts on the timeline, in ticks of the part's timescale. Order and coverage always follow it,
    /// also where the `tfdt` cannot be rewritten.
    pub fn pts(&self, r: &IndexRow) -> i128 {
        i128::from(r.tfdt.unwrap_or(0)) + i128::from(self.offset(r))
    }

    /// The `tfdt` shift of one `traf` of `r`: per track (`tid`) in a multi-track part, in that track's
    /// timescale; the row's own otherwise (`tid` `None`: the row as a whole). 0 for init rows and where nothing
    /// can be shifted.
    pub fn shift_for(&self, r: &IndexRow, tid: Option<u32>) -> i64 {
        if r.is_init {
            return 0;
        }
        match (r.seq_mode, tid.and_then(|t| self.track_ts.get(&t))) {
            (false, Some(&t)) if self.tracks() > 1 => {
                shift_ticks(r.tso, Some(t)) + ns_ticks(self.lift_ns, Some(t))
            }
            (true, Some(&t)) if self.tracks() > 1 => rescale(self.offset(r), self.ts, t),
            _ if self.can_shift() => self.offset(r),
            _ => 0,
        }
    }
}

/// The media rows of one part in the order they are muxed ([`order_media`]).
pub struct Ordered<'a> {
    pub rows: Vec<&'a IndexRow>,
    /// Ordered by presentation time; otherwise the arrival order was kept (a row without `tfdt`, or sequence
    /// mode without a layout for every row).
    pub timed: bool,
    pub covered: usize,
    pub partial: usize,
    pub gaps: Vec<Gaps>,
    /// Every media row has a duration (only then can covered segments be found).
    pub durations_known: bool,
    /// Presentation time (seconds) of the first media row, when ordered by time and the timescale is known.
    pub start: Option<f64>,
    /// Some row moves on the timeline (an offset, a layout move or the lift).
    pub wants_shift: bool,
}

/// Orders the media rows of one part by presentation time (`tfdt` plus what `clock` moves them by), the longer
/// one first at the same start, and leaves out what earlier segments cover. Players fetch out of order after a
/// backward seek, and the sink writes in arrival order.
///
/// Without a `tfdt` on every media row (`WebM` blocks, continuation chunks), or in sequence mode without a
/// layout move for every row (no durations), the arrival order is kept.
pub fn order_media<'a>(clock: &PartClock<'_>, media: &[&'a IndexRow]) -> Ordered<'a> {
    let seq_ok = media.iter().filter(|r| r.seq_mode).all(|r| {
        clock
            .seq_moves
            .is_some_and(|m| m.contains_key(&(clock.part, r.offset)))
    });
    let timed = !media.is_empty() && media.iter().all(|r| r.tfdt.is_some()) && seq_ok;
    let mut out = Ordered {
        rows: media.to_vec(),
        timed,
        covered: 0,
        partial: 0,
        gaps: Vec::new(),
        durations_known: media.iter().all(|r| r.dur.is_some_and(|d| d > 0)),
        start: None,
        wants_shift: media.iter().any(|r| clock.offset(r) != 0),
    };
    if !timed {
        return out;
    }
    let pts = |r: &IndexRow| clock.pts(r);
    // Longer first at the same start, so the shorter one is the covered one.
    out.rows
        .sort_by_key(|r| (pts(r), std::cmp::Reverse(r.dur.unwrap_or(0))));
    let covered = cover_per_track(
        std::mem::take(&mut out.rows),
        &pts,
        clock.track_ts,
        clock.ts,
    );
    out.rows = covered.rows;
    out.covered = covered.covered;
    out.partial = covered.partial;
    out.gaps = covered.gaps;
    if let (Some(ts), Some(first)) = (clock.ts, out.rows.first()) {
        // Presentation times are far below 2^52 ticks in practice; a rounding error here only moves the
        // alignment by a tick.
        #[allow(clippy::cast_precision_loss)]
        let secs = pts(first) as f64 / f64::from(ts);
        out.start = Some(secs);
    }
    out
}
