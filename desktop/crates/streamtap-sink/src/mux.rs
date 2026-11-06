//! `mux`: a capture directory to one playable file.

use super::*;
use serde::Deserialize;

#[derive(Debug, Deserialize)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "index format: independent flags of one row"
)]
pub(crate) struct IndexRow {
    pub(crate) sb: u32,
    #[serde(default)]
    pub(crate) ms: Option<u32>,
    pub(crate) part: u32,
    pub(crate) offset: u64,
    pub(crate) len: u64,
    pub(crate) is_init: bool,
    pub(crate) tfdt: Option<u64>,
    #[serde(default)]
    pub(crate) track: Option<u32>,
    pub(crate) timescale: Option<u32>,
    #[serde(default)]
    pub(crate) dur: Option<u64>,
    #[serde(default)]
    pub(crate) tso: Option<f64>,
    #[serde(default)]
    pub(crate) seq_mode: bool,
    #[serde(default)]
    pub(crate) tso_set: bool,
    /// Only for the sequence-mode layout (`IndexLine::mark`).
    #[serde(default)]
    pub(crate) mark: bool,
    pub(crate) mime: String,
    #[serde(default)]
    pub(crate) kind: String,
}

/// Top-level ISO BMFF boxes in `buf[start..end]` as (type, body start, end).
pub(crate) fn boxes_in(buf: &[u8], start: usize, end: usize) -> Vec<([u8; 4], usize, usize)> {
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
#[cfg(test)]
pub(crate) fn shift_tfdt(seg: &mut [u8], delta: i64) -> bool {
    shift_tfdt_by(seg, &|_| delta)
}

/// Like `shift_tfdt`, with the delta per `traf` from its `tfhd` track ID:
/// each track counts in its own timescale.
pub(crate) fn shift_tfdt_by(seg: &mut [u8], delta_of: &dyn Fn(Option<u32>) -> i64) -> bool {
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
pub(crate) fn trak_timescales(init: &[u8]) -> HashMap<u32, u32> {
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
pub(crate) fn trak_count(init: &[u8]) -> usize {
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

/// `timestampOffset` (seconds) as ticks of `timescale`, rounded once from
/// the exact product (no millisecond step in between).
pub(crate) fn shift_ticks(tso: Option<f64>, timescale: Option<u32>) -> i64 {
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
pub(crate) fn rescale(ticks: i64, from: Option<u32>, to: u32) -> i64 {
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
pub(crate) type SeqMoves = HashMap<(u32, u64), i64>;

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
pub(crate) fn seq_layout(rows: &[&IndexRow], leads: &SeqMoves) -> (SeqMoves, usize) {
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

/// Big-endian integer of `n` bytes (at most 8) at `at`.
pub(crate) fn be_uint(b: &[u8], at: usize, n: usize) -> Option<u64> {
    b.get(at..at + n)
        .map(|x| x.iter().fold(0u64, |v, byte| (v << 8) | u64::from(*byte)))
}

/// Composition offset of the first sample of a media segment (first `trun`
/// of the first `traf`), in its track's ticks; 0 when it has none.
pub(crate) fn first_cto(seg: &[u8]) -> i64 {
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
pub(crate) fn elst_media_time(init: &[u8]) -> i64 {
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

/// The init rows of a part: its first run of consecutive init rows (an init
/// split across appends). An init row after media is a stray piece of
/// another copy (an older sink stored the overflow of a duplicate init so):
/// joined in, it would break the init, and a page could grow it without
/// bound.
pub(crate) fn init_run<'a>(rows: &[&'a IndexRow]) -> Vec<&'a IndexRow> {
    rows.iter()
        .copied()
        .skip_while(|r| !r.is_init)
        .take_while(|r| r.is_init)
        .collect()
}

/// `init_run` of one part, saying what it leaves out; also whether nothing
/// was left out (only then can the raw file serve as it is).
pub(crate) fn part_init<'a>(
    sb: u32,
    part: u32,
    rows: &[&'a IndexRow],
) -> (Vec<&'a IndexRow>, bool) {
    let init = init_run(rows);
    let stray = rows.iter().filter(|r| r.is_init).count() - init.len();
    if stray > 0 {
        eprintln!("mux: sb{sb}.p{part}: {stray} stray init row(s) after media left out");
    }
    (init, stray == 0)
}

/// How much of a segment cut into pieces is read for its first `trun`: a moof is far smaller,
/// and the pieces that follow can be the whole media data.
pub(crate) const MAX_MOOF_FOR_LEAD: usize = 1024 * 1024;

/// The presentation lead (`seq_layout`) of every sequence-mode row appended
/// right after a set `timestampOffset`: read from its segment and its part's
/// init. Unreadable rows get none.
pub(crate) fn seq_leads(dir: &Path, sb: u32, rows: &[&IndexRow]) -> SeqMoves {
    let mut leads = HashMap::new();
    let mut parts: Vec<u32> = rows
        .iter()
        .filter(|r| r.seq_mode && r.tso_set && !r.is_init)
        .map(|r| r.part)
        .collect();
    parts.sort_unstable();
    parts.dedup();
    let mut buf = Vec::new();
    for part in parts {
        let Ok(mut src) = File::open(dir.join(format!("sb{sb}.p{part}.bin"))) else {
            continue;
        };
        let part_rows: Vec<&IndexRow> = rows.iter().copied().filter(|r| r.part == part).collect();
        let mut init = Vec::new();
        for r in init_run(&part_rows) {
            if read_segment(&mut src, r, &mut buf).is_ok() {
                init.extend_from_slice(&buf);
            }
        }
        let media_time = elst_media_time(&init);
        for (i, r) in part_rows.iter().enumerate() {
            if !r.seq_mode || !r.tso_set || r.is_init {
                continue;
            }
            if read_segment(&mut src, r, &mut buf).is_err() {
                continue;
            }
            // Cut inside the moof, the rest of it (and the trun) is in the pieces that follow.
            let mut seg = std::mem::take(&mut buf);
            for next in part_rows[i + 1..]
                .iter()
                .take_while(|n| n.tfdt.is_none() && !n.is_init)
            {
                if seg.len() >= MAX_MOOF_FOR_LEAD || read_segment(&mut src, next, &mut buf).is_err()
                {
                    break;
                }
                seg.extend_from_slice(&buf);
            }
            leads.insert((part, r.offset), first_cto(&seg) - media_time);
            buf = seg;
        }
    }
    leads
}

/// The sequence-mode layout (`seq_layout`) of the `SourceBuffer` of every
/// `chosen` (`SourceBuffer`, part) pair.
pub(crate) fn seq_layouts(
    dir: &Path,
    rows: &[IndexRow],
    chosen: &[(u32, u32)],
) -> HashMap<u32, SeqMoves> {
    let mut layouts = HashMap::new();
    for &(id, _) in chosen {
        let sb_rows: Vec<&IndexRow> = rows.iter().filter(|r| r.sb == id).collect();
        let leads = seq_leads(dir, id, &sb_rows);
        let (moves, jumps) = seq_layout(&sb_rows, &leads);
        if jumps > 0 {
            eprintln!(
                "mux: sb{id}: sequence mode: {jumps} discontinuity(ies) without a timestampOffset set; placed right after the previous append, as the browser does"
            );
        }
        layouts.insert(id, moves);
    }
    layouts
}

/// The lift (nanoseconds) for all `SourceBuffer`s. A frame placed by its
/// offset may need a decode time before 0 (B-frames: at a set offset the
/// browser puts the presentation time there; a negative `timestampOffset`),
/// which an MP4 cannot hold: then every `SourceBuffer` moves later by the
/// same amount, so their alignment stays. Only the `muxed` (`SourceBuffer`,
/// part) pairs count: a part left out (another rendition, a forged or
/// undecodable one) moves nothing.
pub(crate) fn seq_lift(
    rows: &[IndexRow],
    layouts: &HashMap<u32, SeqMoves>,
    muxed: &[(u32, u32)],
) -> i64 {
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
pub(crate) fn ns_ticks(ns: i64, ts: Option<u32>) -> i64 {
    let Some(ts) = ts.filter(|t| *t > 0) else {
        return 0;
    };
    let ticks = (i128::from(ns) * i128::from(ts) + 999_999_999) / 1_000_000_000;
    i64::try_from(ticks.clamp(i128::from(i64::MIN), i128::from(i64::MAX))).expect("clamped")
}

pub(crate) fn read_segment(src: &mut File, r: &IndexRow, buf: &mut Vec<u8>) -> io::Result<()> {
    let len = usize::try_from(r.len)
        .ok()
        .filter(|l| *l <= MAX_MESSAGE)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "segment too large"))?;
    buf.resize(len, 0);
    src.seek(io::SeekFrom::Start(r.offset))?;
    src.read_exact(buf)
}

/// Ranges skipped by a forward seek in the kept segments, sorted by start,
/// as (start, duration) in track ticks: (count, missing ticks, exact).
///
/// With a duration on every segment the gaps are exact: a start past the
/// end of everything before it (a running end, so a longer earlier segment
/// causes no false gap), beyond a tolerance for rounding. Without, a step
/// well above the typical (lower median) step counts, measured against it;
/// gaps that make up most of the steps are missed there.
pub(crate) fn find_gaps(segs: &[(i128, Option<u64>)], ts: u32) -> (usize, u128, bool) {
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

/// Reports ranges skipped by a forward seek (`find_gaps`).
pub(crate) fn report_gaps(sb: u32, part: u32, segs: &[(i128, Option<u64>)], ts: u32) {
    let (count, missing, exact) = find_gaps(segs, ts);
    if count > 0 {
        // Seconds for a message; precision far beyond what it shows.
        #[allow(clippy::cast_precision_loss)]
        let seconds = missing as f64 / f64::from(ts);
        eprintln!(
            "mux: sb{sb}.p{part}: {count} gap(s), {}{seconds:.1} s not captured (seeked past them)",
            if exact { "" } else { "about " }
        );
    }
}

/// The rows split by the track their `tfdt` belongs to, each group in the
/// given order; groups in order of first appearance. Rows without a track ID
/// (older captures, `WebM`) share one group.
pub(crate) fn by_track(rows: Vec<&IndexRow>) -> Vec<(Option<u32>, Vec<&IndexRow>)> {
    let mut groups: Vec<(Option<u32>, Vec<&IndexRow>)> = Vec::new();
    for r in rows {
        match groups.iter_mut().find(|(t, _)| *t == r.track) {
            Some((_, g)) => g.push(r),
            None => groups.push((r.track, vec![r])),
        }
    }
    groups
}

/// Drops the segments earlier ones cover and reports gaps, per track: an
/// append that begins with another track of the `SourceBuffer` carries that
/// track's time and duration. `order` is sorted by start; the result is too.
/// Returns the kept rows and the counts of covered and partial overlaps.
pub(crate) fn cover_and_gaps<'a>(
    order: Vec<&'a IndexRow>,
    pts: &dyn Fn(&IndexRow) -> i128,
    (sb, part): (u32, u32),
    track_ts: &HashMap<u32, u32>,
    ts: Option<u32>,
) -> (Vec<&'a IndexRow>, usize, usize) {
    let (mut covered, mut partial) = (0, 0);
    let mut merged = Vec::with_capacity(order.len());
    for (track, group) in by_track(order) {
        let (kept, c, p) = drop_covered(group, pts);
        (covered, partial) = (covered + c, partial + p);
        let scale = track.and_then(|t| track_ts.get(&t).copied()).or(ts);
        if let Some(scale) = scale {
            let segs: Vec<(i128, Option<u64>)> = kept.iter().map(|r| (pts(r), r.dur)).collect();
            report_gaps(sb, part, &segs, scale);
        }
        merged.extend(kept);
    }
    merged.sort_by_key(|r| (pts(r), std::cmp::Reverse(r.dur.unwrap_or(0))));
    (merged, covered, partial)
}

/// Leaves out segments (sorted by start) that earlier ones fully cover, when
/// every segment has a duration. Returns the kept ones and the counts of
/// covered and partially overlapping segments.
pub(crate) fn drop_covered<'a>(
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

/// One part prepared for ffmpeg: its file, the demuxer to force, and the
/// presentation time (seconds) of its first media segment, if known.
#[derive(Debug)]
pub(crate) struct MuxInput {
    pub(crate) path: PathBuf,
    pub(crate) format: &'static str,
    pub(crate) start: Option<f64>,
}

/// Returns a file for `sbN.pM` with the init segment first and the media
/// segments in presentation order (`tfdt` plus `timestampOffset`), plus the
/// start time of its first media segment. Players fetch out of order after
/// a backward seek, and the sink writes in arrival order. If the raw part
/// already is in that order it is returned unchanged; otherwise
/// `sbN.pM.sorted.bin` is written.
///
/// - Segments moved by a `timestampOffset` get their `tfdt` rewritten so
///   the timeline stays monotonic; not for a `SourceBuffer` with several
///   tracks (each `traf` counts in its own timescale), which is warned.
/// - With durations on every segment, a segment fully covered by earlier
///   ones (an LL-HLS full segment after its parts, a re-fetch with another
///   cut) is left out; partial overlaps are kept and counted.
/// - Sequence mode: laid out one after the other in arrival order, as the
///   browser plays it (`seq_layout`); a jump (a seek, a new period) gets its
///   `tfdt` rewritten.
/// - Without a `tfdt` on every media segment (`WebM` blocks, continuation
///   chunks), or in sequence mode without durations, the arrival order is
///   kept.
///
/// The last value says whether the file's bytes carry the lift (`lift_ns`):
/// not for a part that cannot be shifted (`WebM`, several tracks without
/// per-track timescales), whose probed start then lacks it.
pub(crate) fn ordered_part(
    dir: &Path,
    sb: u32,
    part: u32,
    rows: &[&IndexRow],
    seq_moves: Option<&SeqMoves>,
    lift_ns: i64,
) -> io::Result<(PathBuf, Option<f64>, usize, bool)> {
    let raw = dir.join(format!("sb{sb}.p{part}.bin"));
    let (init, no_stray) = part_init(sb, part, rows);
    let media: Vec<&IndexRow> = rows.iter().copied().filter(|r| !r.is_init).collect();
    // The media rows carry the timescale of their tfdt; the first row of a
    // split init may still carry the previous rendition's.
    let ts = media
        .iter()
        .chain(init.iter())
        .find_map(|r| r.timescale)
        .filter(|t| *t > 0);
    let mut src = File::open(&raw)?;
    let mut buf = Vec::new();

    // The init may be several rows (an init split across appends).
    let mut init_bytes = Vec::new();
    for r in &init {
        read_segment(&mut src, r, &mut buf)?;
        init_bytes.extend_from_slice(&buf);
    }
    let raw_tracks = trak_count(&init_bytes);
    let tracks = raw_tracks.max(1);
    // Per-track timescales let a multi-track segment be shifted traf by traf.
    let track_ts = trak_timescales(&init_bytes);
    // Sequence mode: placed where the browser placed it (`seq_layout`, over
    // all parts of the SourceBuffer); without that layout the arrival order.
    let seq_ok = media
        .iter()
        .filter(|r| r.seq_mode)
        .all(|r| seq_moves.is_some_and(|m| m.contains_key(&(part, r.offset))));
    // Every SourceBuffer moves by the same lift, so none decodes before 0.
    let lift = ns_ticks(lift_ns, ts);
    let offset = |r: &IndexRow| {
        lift + if r.seq_mode {
            seq_moves
                .and_then(|m| m.get(&(part, r.offset)))
                .copied()
                .unwrap_or(0)
        } else {
            shift_ticks(r.tso, ts)
        }
    };
    let wants_shift = media.iter().any(|r| offset(r) != 0);
    let can_shift = tracks == 1 || (raw_tracks > 0 && track_ts.len() == raw_tracks);
    if wants_shift && !can_shift {
        eprintln!(
            "mux: sb{sb}.p{part}: {tracks} tracks in one SourceBuffer without per-track timescales; timestampOffset is not written into the segments, the timeline may jump"
        );
    }
    // Order and coverage always follow the presentation time; the tfdt is
    // only rewritten where one timescale holds for the whole segment.
    let delta = |r: &IndexRow| if can_shift { offset(r) } else { 0 };
    let pts = |r: &IndexRow| i128::from(r.tfdt.unwrap_or(0)) + i128::from(offset(r));
    // Sequence mode needs durations to lay out the jumps; without them the
    // arrival order (duplicates already removed by the sink) is kept.
    let timed = !media.is_empty() && media.iter().all(|r| r.tfdt.is_some()) && seq_ok;
    // The raw file only serves when it holds nothing left out.
    let init_first = no_stray && rows.iter().take(init.len()).all(|r| r.is_init);

    let mut order: Vec<&IndexRow> = media.clone();
    if timed {
        // Longer first at the same start, so the shorter one is the covered one.
        order.sort_by_key(|r| (pts(r), std::cmp::Reverse(r.dur.unwrap_or(0))));
        let (covered, partial);
        (order, covered, partial) = cover_and_gaps(order, &pts, (sb, part), &track_ts, ts);
        if covered > 0 || partial > 0 {
            eprintln!(
                "mux: sb{sb}.p{part}: {covered} segment(s) fully covered by others left out, {partial} partial overlap(s) kept (frames may repeat there)"
            );
        }
        if !media.iter().all(|r| r.dur.is_some_and(|d| d > 0)) {
            eprintln!(
                "mux: sb{sb}.p{part}: segment durations unknown; overlapping re-fetches cannot be detected"
            );
        }
    } else {
        warn_arrival_order(sb, part, &media);
    }

    // Start time of the first media segment, for aligning the inputs.
    let start = match (timed, ts, order.first()) {
        (true, Some(ts), Some(r)) => {
            // Presentation times are far below 2^52 ticks in practice; a
            // rounding error here only moves the alignment by a tick.
            #[allow(clippy::cast_precision_loss)]
            let secs = pts(r) as f64 / f64::from(ts);
            Some(secs)
        }
        _ => None,
    };

    let same_order = same_rows(&order, &media);
    let shifted = can_shift && wants_shift;
    // ffprobe reads the start from the first media segment: did it move?
    let carries_lift =
        |first_moved: bool| lift_ns == 0 || (ts.is_some() && can_shift && first_moved);
    if init_first && !shifted && same_order {
        discard_sorted(&dir.join(format!("sb{sb}.p{part}.sorted.bin"))); // an earlier round's
        return Ok((raw, start, raw_tracks, carries_lift(true)));
    }

    // The tfdt shift of a row's traf: per track in a multi-track part, the
    // row's own otherwise (tid None: the row as a whole).
    let shift_for = |r: &IndexRow, tid: Option<u32>| -> i64 {
        if r.is_init {
            return 0;
        }
        match (r.seq_mode, tid.and_then(|t| track_ts.get(&t))) {
            (false, Some(&t)) if tracks > 1 => {
                shift_ticks(r.tso, Some(t)) + ns_ticks(lift_ns, Some(t))
            }
            (true, Some(&t)) if tracks > 1 => rescale(offset(r), ts, t),
            _ => delta(r),
        }
    };
    let rows_out: Vec<&IndexRow> = init.iter().chain(order.iter()).copied().collect();
    let (sorted, first_moved) =
        write_sorted(dir, (sb, part), &mut src, &rows_out, &shift_for, shifted)?;
    Ok((sorted, start, raw_tracks, carries_lift(first_moved)))
}

/// Whether the part's init is `WebM`.
pub(crate) fn is_webm(rows: &[&IndexRow]) -> bool {
    rows.iter()
        .find(|r| r.is_init)
        .is_some_and(|r| Container::from_kind(&r.kind) == Container::Webm)
}

/// The same rows (not equal ones) in the same order.
pub(crate) fn same_rows(a: &[&IndexRow], b: &[&IndexRow]) -> bool {
    a.len() == b.len() && a.iter().zip(b).all(|(x, y)| std::ptr::eq(*x, *y))
}

/// Says why a part keeps its arrival order.
pub(crate) fn warn_arrival_order(sb: u32, part: u32, media: &[&IndexRow]) {
    if media.is_empty() {
        return;
    }
    let untimed = media.iter().filter(|r| r.tfdt.is_none()).count();
    if untimed > 0 {
        eprintln!(
            "mux: sb{sb}.p{part}: {untimed} media segment(s) without tfdt; keeping arrival order"
        );
    } else {
        eprintln!(
            "mux: sb{sb}.p{part}: sequence mode without segment durations; keeping arrival order, a seek or a new period may jump back in time"
        );
    }
}

/// Says that `unshifted` segments keep their own timestamps. `WebM` has no
/// `tfdt` to rewrite: the part is aligned by its probed start instead, so
/// nothing is said about a jump there.
pub(crate) fn unmoved_notice((sb, part): (u32, u32), unshifted: usize, webm: bool) -> String {
    if webm {
        format!(
            "mux: sb{sb}.p{part}: {unshifted} WebM segment(s) keep their own timestamps; the part is aligned by its probed start instead of a rewritten timestampOffset"
        )
    } else {
        format!(
            "mux: sb{sb}.p{part}: {unshifted} segment(s) could not be moved by their timestampOffset; the timeline may jump"
        )
    }
}

/// Writes `sbN.pM.sorted.bin` from `rows` in that order, shifting each
/// media segment's tfdt by `shift_for`. Also returns whether the first media
/// segment with a tfdt got its shift (or needed none).
pub(crate) fn write_sorted(
    dir: &Path,
    (sb, part): (u32, u32),
    src: &mut File,
    rows: &[&IndexRow],
    shift_for: &dyn Fn(&IndexRow, Option<u32>) -> i64,
    shifted: bool,
) -> io::Result<(PathBuf, bool)> {
    let sorted = dir.join(format!("sb{sb}.p{part}.sorted.bin"));
    let mut dst = io::BufWriter::new(File::create(&sorted)?);
    let mut buf = Vec::new();
    let mut unshifted = 0usize;
    let mut first_moved = None;
    for r in rows {
        read_segment(src, r, &mut buf)?;
        let moved = shift_for(r, None) == 0 || shift_tfdt_by(&mut buf, &|tid| shift_for(r, tid));
        if !moved {
            unshifted += 1;
        }
        // A row without tfdt (a cut piece) has nothing to move.
        if !r.is_init && r.tfdt.is_some() && first_moved.is_none() {
            first_moved = Some(moved);
        }
        dst.write_all(&buf)?;
    }
    dst.flush()?;
    if unshifted > 0 {
        eprintln!("{}", unmoved_notice((sb, part), unshifted, is_webm(rows)));
    }
    eprintln!(
        "mux: sb{sb}.p{part}: reordered{}, wrote {}",
        if shifted {
            " and shifted by timestampOffset"
        } else {
            ""
        },
        sorted.display()
    );
    Ok((sorted, first_moved.unwrap_or(false)))
}

pub(crate) fn run_mux(args: &[String], tools: &Tools) -> Result<(), SinkError> {
    let Some(dir) = args.first().filter(|a| !a.starts_with("--")) else {
        return Err(SinkError::Usage("capture directory required".into()));
    };
    let dir = PathBuf::from(dir);
    let out = flag(args, "--out")?.map_or_else(|| dir.join("out.mp4"), PathBuf::from);
    let timeout = match flag(args, "--timeout-secs")?.map(str::parse::<u64>) {
        None => Duration::from_secs(DEFAULT_MUX_TIMEOUT_SECS),
        Some(Ok(s)) if (1..=86_400).contains(&s) => Duration::from_secs(s),
        Some(_) => {
            return Err(SinkError::Usage(
                "--timeout-secs must be a whole number from 1 to 86400".into(),
            ))
        }
    };

    let index_path = dir.join("index.jsonl");
    let index = fs::read_to_string(&index_path)
        .map_err(|e| SinkError::io("cannot read", Some(index_path), e))?;

    // Fail before any work when ffmpeg is missing, and name the version the
    // mux ran with.
    println!("mux: {}", ffmpeg_version(tools)?);

    // A bad line (an interrupted sink, a forged record) costs that line only.
    let mut rows: Vec<IndexRow> = Vec::new();
    let mut bad_lines = 0usize;
    for line in index.lines().filter(|l| !l.trim().is_empty()) {
        match serde_json::from_str(line) {
            Ok(r) => rows.push(r),
            Err(e) => {
                bad_lines += 1;
                if bad_lines <= 3 {
                    eprintln!("mux: bad index line skipped: {e}");
                }
            }
        }
    }
    if bad_lines > 3 {
        eprintln!("mux: {bad_lines} bad index lines skipped in total");
    }

    keep_main_media_source(&mut rows);
    report_lost_inits(&dir);
    let plan = mux_inputs(&dir, &rows, tools)?;
    for w in &plan.warnings {
        eprintln!("mux: {w}");
    }
    run_ffmpeg(
        tools,
        &plan.inputs,
        &out,
        timeout,
        args.iter().any(|a| a == "--verbose"),
    )
}

/// Names the `SourceBuffer`s whose capture ends early because the extension
/// dropped one of their inits (`events.jsonl`, written by the sink).
pub(crate) fn report_lost_inits(dir: &Path) {
    let Ok(events) = fs::read_to_string(dir.join("events.jsonl")) else {
        return;
    };
    let mut lost: Vec<u64> = events
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .filter(|v| v["event"] == "initlost")
        .filter_map(|v| v["sb"].as_u64())
        .collect();
    lost.sort_unstable();
    lost.dedup();
    for sb in lost {
        eprintln!(
            "mux: sb{sb}: the extension lost an init (its queue was full, or the init could not be kept for a new connection); the media between it and the next init of sb{sb} is not captured"
        );
    }
}

/// The first line of `ffmpeg -version`, e.g. `ffmpeg version 7.1 ...`.
pub(crate) fn ffmpeg_version(tools: &Tools) -> Result<String, SinkError> {
    let out = proc::run(&Spec {
        tool: Tool::Ffmpeg,
        program: tools.program(Tool::Ffmpeg),
        args: vec!["-hide_banner".into(), "-version".into()],
        extra_env: Vec::new(),
        timeout: VERSION_TIMEOUT,
    })?;
    Ok(version_line(&String::from_utf8_lossy(&out.stdout)))
}

/// `ffmpeg version <v>` from the version output, without the copyright.
pub(crate) fn version_line(text: &str) -> String {
    let first = text.lines().next().unwrap_or("").trim();
    let cut = first.find(" Copyright").map_or(first, |i| &first[..i]);
    if cut.is_empty() {
        "ffmpeg version unknown (empty -version output)".to_owned()
    } else {
        printable(cut, 200)
    }
}

/// One `MediaSource` is one video. A preroll ad, the next video of a
/// single-page app or a hover preview adds more `MediaSources` to the same
/// session; keep the one with the most bytes and name the others.
pub(crate) fn keep_main_media_source(rows: &mut Vec<IndexRow>) {
    let mut per_ms: HashMap<Option<u32>, u64> = HashMap::new();
    // A mark repeats bytes stored once: counting it would let a looping
    // teaser outweigh the video.
    for r in rows.iter().filter(|r| !r.mark) {
        *per_ms.entry(r.ms).or_default() += r.len;
    }
    let Some((&chosen, _)) = per_ms
        .iter()
        .max_by_key(|(ms, bytes)| (**bytes, std::cmp::Reverse(**ms)))
    else {
        return;
    };
    let name = |ms: Option<u32>| ms.map_or("?".to_owned(), |m| m.to_string());
    if per_ms.len() > 1 {
        let mut others: Vec<String> = per_ms
            .iter()
            .filter(|(ms, _)| **ms != chosen)
            .map(|(ms, b)| format!("{} ({b} bytes)", name(*ms)))
            .collect();
        others.sort();
        eprintln!(
            "mux: {} MediaSources in this capture; using #{}, leaving out {}",
            per_ms.len(),
            name(chosen),
            others.join(", ")
        );
    }
    rows.retain(|r| r.ms == chosen);
}

/// The ffmpeg demuxer for a part, after checking that the part starts like
/// one: the bytes come from the page, and ffmpeg's format probing would
/// otherwise follow whatever they claim to be (an MPD or a playlist pulls in
/// other files).
pub(crate) fn input_format(kind: Container, path: &Path) -> Option<&'static str> {
    let mut head = [0u8; 8];
    File::open(path).ok()?.read_exact(&mut head).ok()?;
    kind.head_format(head)
}

/// What `mux_inputs` hands to ffmpeg, plus what the caller must tell the
/// user about ffprobe (ENG-13: a missing ffprobe is said, not swallowed).
#[derive(Debug)]
pub(crate) struct MuxPlan {
    pub(crate) inputs: Vec<MuxInput>,
    pub(crate) warnings: Vec<String>,
}

/// The parts of one `SourceBuffer` that may be muxed (`pick_parts`).
pub(crate) struct Candidates {
    pub(crate) sb: u32,
    /// All its parts, with or without an init.
    pub(crate) total: usize,
    /// (part, bytes, MIME) of the parts with an init segment, best first
    /// (plausible, most claimed media time, most bytes).
    pub(crate) ranked: Vec<(u32, u64, String)>,
}

/// Per `SourceBuffer` the parts to try, best first. Adaptive players often
/// start at a low quality and switch after the first segment, which starts
/// a new part: the parts with an init segment, the most plausible first (see
/// `PartStat`). The first one `part_input` can mux is used.
pub(crate) fn pick_parts(stored: &[&IndexRow]) -> Vec<Candidates> {
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
    let mut picked = Vec::new();
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
            eprintln!("mux: sb{id} has no part with an init segment; skipped. Capture with the hook active from page load.");
            continue;
        }
        picked.push(Candidates {
            sb: id,
            total,
            ranked,
        });
    }
    picked
}

/// What `pick_parts` knows about one part.
#[derive(Default)]
pub(crate) struct PartStat {
    pub(crate) bytes: u64,
    pub(crate) has_init: bool,
    pub(crate) mime: String,
    /// Media time the segments claim (ms), per track: the tracks of a
    /// `SourceBuffer` run in parallel, so they are not added up.
    pub(crate) ms_by_track: HashMap<Option<u32>, u64>,
}

/// Bounds for bytes per second of claimed media time: below the lowest a
/// real stream reaches (a few kbit/s audio) the time was inflated or the
/// bytes cut off, above the highest (800 Mbit/s) the bytes were padded.
pub(crate) const MIN_BYTES_PER_SEC: u64 = 100;
pub(crate) const MAX_BYTES_PER_SEC: u64 = 100_000_000;

impl PartStat {
    pub(crate) fn media_ms(&self) -> u64 {
        self.ms_by_track.values().copied().max().unwrap_or(0)
    }

    /// No claimed time (nothing to check) or one that fits the bytes.
    pub(crate) fn plausible(&self) -> bool {
        let ms = self.media_ms();
        if ms == 0 {
            return true;
        }
        let per_sec = u128::from(self.bytes) * 1000 / u128::from(ms);
        (u128::from(MIN_BYTES_PER_SEC)..=u128::from(MAX_BYTES_PER_SEC)).contains(&per_sec)
    }
}

/// The ffmpeg input of one chosen (`SourceBuffer`, part) in timeline order,
/// moved by `lift`; `None` when it cannot be muxed (said why).
pub(crate) fn part_input(
    dir: &Path,
    stored: &[&IndexRow],
    layouts: &HashMap<u32, SeqMoves>,
    (id, part): (u32, u32),
    lift: i64,
    prober: &mut Prober<'_>,
) -> Result<Option<MuxInput>, SinkError> {
    let no_moves = SeqMoves::new();
    let seq_moves = layouts.get(&id).unwrap_or(&no_moves);
    let part_rows: Vec<&IndexRow> = stored
        .iter()
        .copied()
        .filter(|r| r.sb == id && r.part == part)
        .collect();
    let kind = part_rows
        .iter()
        .find(|r| r.is_init)
        .map_or("", |r| r.kind.as_str());
    let container = Container::from_kind(kind);
    let not_media = || {
        eprintln!(
            "mux: sb{id}.p{part} does not start like {} media; skipped",
            if kind.is_empty() {
                "known".to_owned()
            } else {
                printable(kind, 16)
            }
        );
    };
    let raw = dir.join(format!("sb{id}.p{part}.bin"));
    let sorted = dir.join(format!("sb{id}.p{part}.sorted.bin"));
    let cannot_reorder =
        |e| SinkError::io(format!("sb{id}.p{part}: cannot reorder segments"), None, e);
    // The cheap checks on the init first: a part they reject costs no sorted
    // copy and no ffprobe run. The sorted file starts with this init.
    let init = read_init(&raw, &part_rows).map_err(cannot_reorder)?;
    // An MP4 init without a complete moov (cut short) cannot be decoded, and
    // ffmpeg would fail the whole mux with it.
    if container == Container::Mp4 && trak_count(&init) == 0 {
        eprintln!("mux: sb{id}.p{part}: its init has no complete moov; skipped");
        return Ok(None);
    }
    if let Some(head) = init.first_chunk::<8>() {
        if container.head_format(*head).is_none() {
            not_media();
            return Ok(None);
        }
    }
    let (path, start, _, lifted) =
        match ordered_part(dir, id, part, &part_rows, Some(seq_moves), lift) {
            Ok(v) => v,
            Err(e) => {
                discard_sorted(&sorted);
                return Err(cannot_reorder(e));
            }
        };
    let Some(format) = input_format(container, &path) else {
        not_media();
        discard_sorted(&sorted);
        return Ok(None);
    };
    // The first presentation time as ffmpeg sees it (B-frame composition
    // offsets and edit lists included): that is what ffmpeg subtracts. A part
    // ffprobe cannot read is left out instead of failing the mux. Without
    // ffprobe (or when it fails to run) the part is kept with the capture's
    // own start, and that is said (ENG-13).
    let start = match prober.probe(format, &path, &format!("sb{id}.p{part}")) {
        Some(Probe::Undecodable) => {
            eprintln!("mux: sb{id}.p{part} cannot be decoded; skipped");
            discard_sorted(&sorted);
            return Ok(None);
        }
        // A part whose bytes could not take the lift is aligned as if they
        // had.
        Some(Probe::Start(s)) if !lifted => {
            Some(s + Duration::from_nanos(lift.unsigned_abs()).as_secs_f64())
        }
        Some(Probe::Start(s)) => Some(s),
        Some(Probe::Unknown) | None => start,
    };
    Ok(Some(MuxInput {
        path,
        format,
        start,
    }))
}

/// The bytes of a part's init (`init_run` of its rows) from its raw file.
pub(crate) fn read_init(raw: &Path, part_rows: &[&IndexRow]) -> io::Result<Vec<u8>> {
    let mut src = File::open(raw)?;
    let (mut init, mut buf) = (Vec::new(), Vec::new());
    for r in init_run(part_rows) {
        read_segment(&mut src, r, &mut buf)?;
        init.extend_from_slice(&buf);
    }
    Ok(init)
}

/// Removes the sorted copy of a part that is not muxed (none is fine).
pub(crate) fn discard_sorted(sorted: &Path) {
    if let Err(e) = fs::remove_file(sorted) {
        if e.kind() != io::ErrorKind::NotFound {
            eprintln!("mux: cannot remove {}: {e}", sorted.display());
        }
    }
}

/// The ffmpeg inputs: per `SourceBuffer` the chosen part in timeline order.
pub(crate) fn mux_inputs(
    dir: &Path,
    rows: &[IndexRow],
    tools: &Tools,
) -> Result<MuxPlan, SinkError> {
    // Marks only hold a duplicate's place in the sequence-mode layout; they
    // have no bytes of their own.
    let stored: Vec<&IndexRow> = rows.iter().filter(|r| !r.mark).collect();
    if stored.is_empty() {
        return Err(SinkError::Capture("index is empty".into()));
    }
    let mut prober = Prober {
        tools,
        missing: false,
        timeouts: 0,
        warnings: Vec::new(),
    };
    let picked = pick_parts(&stored);
    // The layout covers all parts of a SourceBuffer, so a fallback part has
    // its own; the lift starts from each one's best part.
    let best: Vec<(u32, u32)> = picked.iter().map(|c| (c.sb, c.ranked[0].0)).collect();
    let layouts = seq_layouts(dir, rows, &best);
    let mut lift = seq_lift(rows, &layouts, &best);
    let chooser = PartPicker {
        dir,
        stored: &stored,
        layouts: &layouts,
    };
    let mut inputs = Vec::new();
    let mut muxed = Vec::new();
    for c in &picked {
        if let Some((i, input)) = chooser.select(c, 0, lift, &mut prober)? {
            inputs.push(input);
            muxed.push((c.sb, c.ranked[i].0));
        }
    }
    // A part left out above must not move the others: once more without it.
    // A part can fail only now (the new lift makes it read rows the raw file
    // served before); the next candidate of its SourceBuffer is tried then,
    // and the lift is worked out again until it holds for the parts actually
    // muxed. Every round either ends or moves a SourceBuffer to a later
    // candidate (or drops it), so this ends.
    loop {
        let muxed_lift = seq_lift(rows, &layouts, &muxed);
        if muxed_lift == lift {
            break;
        }
        eprintln!(
            "mux: a part left out set how far to move the others; laid out again without it (this replaces the lines above)"
        );
        lift = muxed_lift;
        inputs.clear();
        let mut kept = Vec::new();
        for &p in &muxed {
            let c = picked.iter().find(|c| c.sb == p.0).expect("picked");
            let at = c.ranked.iter().position(|r| r.0 == p.1).expect("ranked");
            match chooser.select(c, at, lift, &mut prober) {
                Ok(Some((i, input))) => {
                    inputs.push(input);
                    kept.push((p.0, c.ranked[i].0));
                }
                Ok(None) => eprintln!("mux: sb{} left out after the new layout", p.0),
                Err(e) => return Err(e),
            }
        }
        muxed = kept;
    }
    if lift > 0 {
        eprintln!(
            "mux: the earliest frame would decode {:.3} s before 0; every SourceBuffer is moved later by that",
            Duration::from_nanos(lift.unsigned_abs()).as_secs_f64()
        );
    }
    if inputs.is_empty() {
        return Err(SinkError::Capture("nothing decodable to mux".into()));
    }
    Ok(MuxPlan {
        inputs,
        warnings: prober.warnings,
    })
}

/// What `mux_inputs` needs to turn one part into an ffmpeg input.
pub(crate) struct PartPicker<'a> {
    pub(crate) dir: &'a Path,
    pub(crate) stored: &'a [&'a IndexRow],
    pub(crate) layouts: &'a HashMap<u32, SeqMoves>,
}

impl PartPicker<'_> {
    /// The first part of `c` from candidate `from` on that can be muxed, with
    /// its place in `c.ranked`. The largest part may not decode (a cut init,
    /// garbled bytes): the next one by size then, rather than none of this
    /// `SourceBuffer`. Only a few: each try may cost a sorted copy and an
    /// ffprobe run. A fallback that cannot be read (a truncated part) is left
    /// out like one that does not decode; the largest part's error still ends
    /// the mux.
    pub(crate) fn select(
        &self,
        c: &Candidates,
        from: usize,
        lift: i64,
        prober: &mut Prober<'_>,
    ) -> Result<Option<(usize, MuxInput)>, SinkError> {
        let sb = c.sb;
        let tries = c.ranked.len().min(1 + MAX_FALLBACK_PARTS);
        for (i, (part, bytes, mime)) in c.ranked.iter().enumerate().take(tries).skip(from) {
            let outcome = match part_input(
                self.dir,
                self.stored,
                self.layouts,
                (sb, *part),
                lift,
                prober,
            ) {
                Ok(v) => v,
                Err(e) if i > 0 => {
                    eprintln!("mux: {e}; skipped");
                    None
                }
                Err(e) => return Err(e),
            };
            let Some(input) = outcome else {
                if i + 1 == tries && tries < c.ranked.len() {
                    eprintln!(
                        "mux: sb{sb}: none of its {tries} largest parts can be muxed; {} smaller one(s) not tried",
                        c.ranked.len() - tries
                    );
                }
                continue;
            };
            if i > 0 {
                eprintln!(
                    "mux: sb{sb}.p{} skipped; using sb{sb}.p{part} ({bytes} bytes) instead",
                    c.ranked[0].0
                );
            }
            if c.total > 1 {
                eprintln!(
                    "mux: sb{sb} has {} parts (codec/quality changes); using part {part} ({bytes} bytes, {})",
                    c.total,
                    printable(mime, 120)
                );
            }
            return Ok(Some((i, input)));
        }
        Ok(None)
    }
}

/// Runs ffprobe per part and collects what went wrong with ffprobe itself.
pub(crate) struct Prober<'a> {
    pub(crate) tools: &'a Tools,
    /// Set once ffprobe turned out to be missing or not startable, or kept
    /// timing out: not asked again per part.
    pub(crate) missing: bool,
    /// ffprobe timeouts in a row; any answer resets it.
    pub(crate) timeouts: u32,
    pub(crate) warnings: Vec<String>,
}

/// After this many ffprobe timeouts in a row the remaining parts are not
/// probed any more: a hanging ffprobe would cost `PROBE_TIMEOUT` per part.
pub(crate) const MAX_PROBE_TIMEOUTS: u32 = 3;

impl Prober<'_> {
    /// Records a warning once: a relayout round probes the same part again.
    pub(crate) fn warn(&mut self, msg: String) {
        if !self.warnings.contains(&msg) {
            self.warnings.push(msg);
        }
    }

    /// `None` when ffprobe could not tell (a warning is recorded).
    pub(crate) fn probe(&mut self, format: &str, path: &Path, name: &str) -> Option<Probe> {
        if self.missing {
            return None;
        }
        match probe_start(self.tools, format, path) {
            Ok(p) => {
                self.timeouts = 0;
                Some(p)
            }
            Err(e) => {
                self.note_error(&e, name);
                None
            }
        }
    }

    /// Records a failed probe; decides whether ffprobe is asked again.
    pub(crate) fn note_error(&mut self, e: &SinkError, name: &str) {
        match e {
            // Missing or not startable: the same for every part, so said
            // once and not tried again (ENG-13).
            SinkError::ToolMissing { .. } | SinkError::ToolSpawn { .. } => {
                self.missing = true;
                self.warn(format!(
                    "{e}. Without it the parts are not checked for decodability and are aligned by the capture's own timestamps (B-frame offsets and edit lists not counted)."
                ));
            }
            SinkError::ToolTimeout { .. } => {
                self.timeouts += 1;
                self.warn(format!(
                    "{name}: {e}; aligned by the capture's own timestamps"
                ));
                if self.timeouts >= MAX_PROBE_TIMEOUTS {
                    self.missing = true;
                    self.warn(format!(
                        "ffprobe timed out {MAX_PROBE_TIMEOUTS} times in a row; the remaining parts are not probed and are aligned by the capture's own timestamps"
                    ));
                }
            }
            _ => self.warn(format!(
                "{name}: {e}; aligned by the capture's own timestamps"
            )),
        }
    }
}

/// What ffprobe says about a part it could run on.
#[derive(Debug, PartialEq)]
pub(crate) enum Probe {
    /// First presentation time in seconds.
    Start(f64),
    /// Readable, no start time reported.
    Unknown,
    /// ffprobe failed on it.
    Undecodable,
}

/// `Err` only when ffprobe itself could not do its job (missing, not
/// startable, timed out, too much output); a part it rejects is
/// `Probe::Undecodable`.
pub(crate) fn probe_start(tools: &Tools, format: &str, path: &Path) -> Result<Probe, SinkError> {
    let mut args: Vec<std::ffi::OsString> = [
        "-v",
        "error",
        "-f",
        format,
        "-protocol_whitelist",
        "file",
        "-show_entries",
        "stream=codec_type:format=start_time",
        "-of",
        "default=noprint_wrappers=1",
    ]
    .iter()
    .map(Into::into)
    .collect();
    args.push(path.into());
    let out = proc::run(&Spec {
        tool: Tool::Ffprobe,
        program: tools.program(Tool::Ffprobe),
        args,
        extra_env: Vec::new(),
        timeout: PROBE_TIMEOUT,
    });
    match out {
        Err(SinkError::ToolFailed { .. }) => Ok(Probe::Undecodable),
        Err(e) => Err(e),
        Ok(o) => {
            let text = String::from_utf8_lossy(&o.stdout);
            // No audio or video stream (an init cut short reads "fine" but
            // has nothing to decode): ffmpeg would fail the whole mux.
            let has_stream = text
                .lines()
                .any(|l| l == "codec_type=video" || l == "codec_type=audio");
            if !has_stream {
                return Ok(Probe::Undecodable);
            }
            Ok(text
                .lines()
                .find_map(|l| l.strip_prefix("start_time="))
                .and_then(|v| v.trim().parse::<f64>().ok())
                .filter(|v| v.is_finite())
                .map_or(Probe::Unknown, Probe::Start))
        }
    }
}

/// Input options of every input: the forced demuxer, files only, and an
/// offset that keeps the inputs aligned. ffmpeg moves every input to start
/// at 0, so a part that starts later (the video after a quality switch)
/// would otherwise slide against the others.
pub(crate) fn input_args(inputs: &[MuxInput]) -> Vec<Vec<String>> {
    let earliest = inputs
        .iter()
        .filter_map(|i| i.start)
        .fold(f64::INFINITY, f64::min);
    inputs
        .iter()
        .map(|i| {
            let mut a = vec![
                "-f".to_owned(),
                i.format.to_owned(),
                "-protocol_whitelist".to_owned(),
                "file".to_owned(),
            ];
            if let Some(s) = i.start.filter(|_| earliest.is_finite()) {
                let offset = s - earliest;
                if offset > 0.0 {
                    a.push("-itsoffset".to_owned());
                    a.push(format!("{offset:.6}"));
                }
            }
            a.push("-i".to_owned());
            a.push(i.path.to_string_lossy().into_owned());
            a
        })
        .collect()
}

/// The whole ffmpeg argv of the mux. Errors only (no progress, no
/// per-packet warnings) unless `verbose` asks for warnings too (such as
/// "Non-monotonous DTS"): stderr keeps its tail either way
/// (`proc::PIPE_LIMIT`), so a chatty ffmpeg is not killed for it.
pub(crate) fn ffmpeg_args(
    inputs: &[MuxInput],
    out: &Path,
    verbose: bool,
) -> Vec<std::ffi::OsString> {
    let level = if verbose { "warning" } else { "error" };
    let mut args: Vec<std::ffi::OsString> = ["-hide_banner", "-nostats", "-loglevel", level, "-y"]
        .iter()
        .map(Into::into)
        .collect();
    args.extend(input_args(inputs).into_iter().flatten().map(Into::into));
    // Every stream of every input, not ffmpeg's "one best video, one best
    // audio" pick across inputs.
    for n in 0..inputs.len() {
        args.push("-map".into());
        args.push(n.to_string().into());
    }
    args.extend(["-c", "copy", "-movflags", "+faststart"].map(Into::into));
    args.push(out.into());
    args
}

pub(crate) fn run_ffmpeg(
    tools: &Tools,
    inputs: &[MuxInput],
    out: &Path,
    timeout: Duration,
    verbose: bool,
) -> Result<(), SinkError> {
    if inputs.iter().any(|i| i.start.is_none()) && inputs.len() > 1 {
        eprintln!("mux: some inputs have no start time; their alignment is ffmpeg's guess");
    }
    println!("mux: {} input(s) -> {}", inputs.len(), out.display());
    let done = write_through_part(out, |part| {
        proc::run(&Spec {
            tool: Tool::Ffmpeg,
            program: tools.program(Tool::Ffmpeg),
            args: ffmpeg_args(inputs, part, verbose),
            extra_env: Vec::new(),
            timeout,
        })
    })?;
    // Errors ffmpeg reported but survived are passed on, not dropped (RS-06).
    let notes = proc::quote_tail(&done.stderr, proc::STDERR_QUOTE);
    if !notes.is_empty() {
        eprintln!("mux: ffmpeg reported:\n{notes}");
    }
    println!("mux: ok");
    Ok(())
}

/// `out` with `.part` before its extension (`out.part.mp4`), next to it:
/// ffmpeg still picks the container from the extension.
pub(crate) fn part_path(out: &Path) -> PathBuf {
    let mut name = out.file_stem().unwrap_or_default().to_os_string();
    name.push(".part");
    if let Some(ext) = out.extension() {
        name.push(".");
        name.push(ext);
    }
    out.with_file_name(name)
}

/// Runs `write` on a sibling part file and renames it to `out` only on
/// success. A killed or failed ffmpeg leaves a truncated file (with
/// `+faststart` the moov comes last, so it does not play); that file is
/// removed, and an older `out` stays untouched.
pub(crate) fn write_through_part<T>(
    out: &Path,
    write: impl FnOnce(&Path) -> Result<T, SinkError>,
) -> Result<T, SinkError> {
    let part = part_path(out);
    let result = write(&part).and_then(|v| {
        fs::rename(&part, out)
            .map(|()| v)
            .map_err(|e| SinkError::io("cannot rename", Some(part.clone()), e))
    });
    if result.is_err() {
        if let Err(e) = fs::remove_file(&part) {
            if e.kind() != std::io::ErrorKind::NotFound {
                eprintln!("mux: cannot remove {}: {e}", part.display());
            }
        }
    }
    result
}
