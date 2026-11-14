//! streamtap-core: `timeline`.

use std::collections::HashMap;

use streamtap_core::fmp4::first_cto;
use streamtap_core::timeline::*;
use streamtap_core::wire::IndexRow;
use streamtap_testkit::*;

#[test]
fn shift_ticks_is_exact_not_millisecond_rounded() {
    // tso = -tfdt/ts for tfdt 1001 at 30000. Rounding tso to whole
    // milliseconds first (-33 ms) would give -990 ticks instead of -1001.
    let tso = -1001.0 / 30000.0;
    assert_eq!(shift_ticks(Some(tso), Some(30000)), -1001);
    assert_eq!(shift_ticks(None, Some(1000)), 0);
    assert_eq!(shift_ticks(Some(1.0), None), 0);
}

#[test]
fn by_track_keeps_order_inside_and_first_seen_between_groups() {
    let mk = |track: Option<u32>, tfdt: u64| IndexRow {
        track,
        ..row(0, 1, false, Some(tfdt), None)
    };
    let rows = [mk(Some(2), 0), mk(Some(1), 5), mk(Some(2), 9), mk(None, 1)];
    let groups = by_track(rows.iter().collect());
    let shape: Vec<(Option<u32>, Vec<u64>)> = groups
        .iter()
        .map(|(t, g)| (*t, g.iter().filter_map(|r| r.tfdt).collect()))
        .collect();
    assert_eq!(
        shape,
        vec![(Some(2), vec![0, 9]), (Some(1), vec![5]), (None, vec![1])]
    );
}

/// Steps and durations at the edge of what the types hold: counted and turned into seconds without a panic.
#[test]
fn gaps_survive_huge_steps() {
    let untimed = |starts: &[i128]| -> Vec<(i128, Option<u64>)> {
        starts.iter().map(|s| (*s, None)).collect()
    };
    let seconds = |segs: &[(i128, Option<u64>)], scale: u32| {
        let (count, missing, exact) = find_gaps(segs, scale);
        let gaps = Gaps {
            count,
            missing,
            exact,
            scale,
        };
        (count, gaps.seconds())
    };
    let (count, secs) = seconds(&untimed(&[0, 10, i128::from(u64::MAX)]), 1000);
    assert_eq!(count, 1);
    assert!(secs.is_finite() && secs > 1e15, "{secs}");
    let (count, _) = seconds(
        &untimed(&[0, i128::from(u64::MAX), i128::from(u64::MAX) * 2]),
        1,
    );
    assert_eq!(count, 0, "equal steps are the typical step, not a gap");
    let huge = [
        (0, Some(u64::MAX)),
        (i128::from(u64::MAX) * 3, Some(u64::MAX)),
    ];
    let (count, secs) = seconds(&huge, 1);
    assert_eq!(count, 1);
    assert!(secs.is_finite() && secs > 1e19, "{secs}");
}

/// (start, 6 s) segments at 1000 ticks/s with these start steps (s).
fn timed_steps(steps: &[i128]) -> Vec<(i128, Option<u64>)> {
    let mut start = 0;
    let mut segs = vec![(0, Some(6000))];
    for s in steps {
        start += s * 1000;
        segs.push((start, Some(6000)));
    }
    segs
}

/// One played segment, a seek to 10:00, two more: the gap is the seek,
/// not the typical step (the upper median was the gap itself).
#[test]
fn a_gap_is_found_from_durations() {
    assert_eq!(find_gaps(&timed_steps(&[600, 6]), 1000), (1, 594_000, true));
    assert_eq!(
        find_gaps(&timed_steps(&[6, 6, 300, 300]), 1000),
        (2, 588_000, true),
        "sampling by seeking: gaps make up half the steps"
    );
    assert_eq!(find_gaps(&timed_steps(&[60]), 1000), (1, 54_000, true));
    assert_eq!(
        find_gaps(&timed_steps(&[6, 6, 6]), 1000),
        (0, 0, true),
        "back to back"
    );
    // A longer earlier segment covers the next start: no false gap.
    let segs = [(0, Some(20_000)), (6000, Some(6000)), (20_000, Some(6000))];
    assert_eq!(find_gaps(&segs, 1000), (0, 0, true));
    // Rounding of a few ticks is no gap.
    let segs = [(0, Some(6000)), (6010, Some(6000))];
    assert_eq!(find_gaps(&segs, 1000), (0, 0, true));
}

/// Without durations the lower median is the typical step.
#[test]
fn a_gap_without_durations_uses_the_lower_median() {
    let segs: Vec<(i128, Option<u64>)> = [0, 6000, 606_000].iter().map(|s| (*s, None)).collect();
    assert_eq!(find_gaps(&segs, 1000), (1, 594_000, false));
    let segs: Vec<(i128, Option<u64>)> = [0, 6000].iter().map(|s| (*s, None)).collect();
    assert_eq!(
        find_gaps(&segs, 1000),
        (0, 0, false),
        "one step: no typical"
    );
}

/// Sequence mode: a re-append the sink did not store again still holds
/// its place, so the next append continues after it (not after the end
/// of the stretch before the seek), and the row it repeats keeps its own
/// place.
#[test]
fn seq_layout_continues_after_a_marked_duplicate() {
    let mut rows = Vec::new();
    let mut add = |offset: u64, tfdt: u64, tso: f64, tso_set: bool, mark: bool| {
        let mut r = row(offset, 1, false, Some(tfdt), Some(tso));
        r.seq_mode = true;
        r.dur = Some(1000);
        r.tso_set = tso_set;
        r.mark = mark;
        rows.push(r);
    };
    add(0, 0, 0.0, true, false); // A at 0
    add(1, 1000, 0.0, false, false); // B at 1
    add(2, 2000, 0.0, false, false); // C at 2
    add(3, 4000, 4.0, true, false); // seek forward: E at 4
    add(4, 5000, 0.0, false, false); // F at 5
    add(2, 2000, 10.0, true, true); // seek: C again, at 10
    add(5, 3000, 8.0, false, false); // D right after it, at 11
    let refs: Vec<&IndexRow> = rows.iter().collect();
    let (moves, jumps) = seq_layout(&refs, &SeqMoves::new());
    assert_eq!(jumps, 0, "D continues the re-append: {moves:?}");
    assert_eq!(moves[&(0, 5)], 8000, "D where the browser played it");
    assert_eq!(moves[&(0, 2)], 0, "C keeps its first place");
}

/// The lift counts only the parts that are muxed: a part left out (here
/// one with a negative offset) moves nothing.
#[test]
fn lift_counts_only_the_chosen_parts() {
    let mut low = row(0, 1, false, Some(0), Some(-5.0));
    low.part = 0;
    let mut high = row(0, 1, false, Some(0), None);
    high.part = 1;
    let rows = vec![low, high];
    let lift = seq_lift(&rows, &HashMap::new(), &[(1, 1)]);
    assert_eq!(lift, 0, "part 0 is not muxed");
    let lift = seq_lift(&rows, &HashMap::new(), &[(1, 0)]);
    assert_eq!(lift, 5_000_000_000, "part 0 muxed: lifted by 5 s");
}

#[test]
fn sequence_mode_layout_spans_all_parts() {
    // An ABR switch and back: part 0 has 0, 1 s and 4, 5 s, part 1 the
    // 2, 3 s in between. One timeline: nothing moves.
    let init = init_with_traks(1);
    let mut rows = Vec::new();
    for (i, (part, t)) in [
        (0, 0),
        (0, 1000),
        (1, 2000),
        (1, 3000),
        (0, 4000),
        (0, 5000),
    ]
    .into_iter()
    .enumerate()
    {
        let mut r = row(i as u64 * 100, 32, false, Some(t), Some(0.0));
        r.part = part;
        r.seq_mode = true;
        r.dur = Some(1000);
        rows.push(r);
    }
    let mut first = row(9999, init.len() as u64, true, None, None);
    first.seq_mode = true;
    rows.insert(0, first);
    let refs: Vec<&IndexRow> = rows.iter().collect();
    let (moves, jumps) = seq_layout(&refs, &HashMap::new());
    assert_eq!(jumps, 0);
    assert!(moves.values().all(|m| *m == 0), "{moves:?}");
}

#[test]
fn a_set_offset_places_the_first_presentation_time() {
    // trun with duration and composition offset: the first frame shows
    // 80 ticks after its decode time.
    let mut trun = vec![0, 0, 0x09, 0x00]; // v0, duration + cto
    trun.extend(1u32.to_be_bytes());
    trun.extend(40u32.to_be_bytes());
    trun.extend(80u32.to_be_bytes());
    let seg = mp4_box(*b"moof", &mp4_box(*b"traf", &mp4_box(*b"trun", &trun)));
    assert_eq!(first_cto(&seg), 80);
    assert_eq!(first_cto(&moof(0)), 0, "no trun");

    let mut r = row(0, 32, false, Some(1000), Some(4.0));
    r.seq_mode = true;
    r.tso_set = true;
    r.dur = Some(1000);
    let leads: SeqMoves = HashMap::from([((0, 0), 80)]);
    let (moves, _) = seq_layout(&[&r], &leads);
    // PTS (1000 + 80) at 4 s: tfdt goes to 3920.
    assert_eq!(moves[&(0, 0)], 2920);
}

#[test]
fn rescale_rounds_between_timescales() {
    assert_eq!(rescale(1000, Some(1000), 90_000), 90_000);
    assert_eq!(rescale(-3, Some(2), 1), -2);
    assert_eq!(rescale(5, None, 1000), 0);
}

/// A media row of part 0 at `tfdt` lasting `dur` ticks (1000 ticks/s), stored at `offset`.
fn timed(offset: u64, tfdt: u64, dur: u64) -> IndexRow {
    IndexRow {
        dur: Some(dur),
        ..row(offset, 10, false, Some(tfdt), None)
    }
}

/// A clock of a single-track part at 1000 ticks/s that moves nothing.
fn plain_clock(track_ts: &HashMap<u32, u32>) -> PartClock<'_> {
    PartClock {
        part: 0,
        ts: Some(1000),
        lift_ns: 0,
        seq_moves: None,
        raw_tracks: 1,
        track_ts,
    }
}

fn offsets(rows: &[&IndexRow]) -> Vec<u64> {
    rows.iter().map(|r| r.offset).collect()
}

#[test]
fn media_is_ordered_by_presentation_time() {
    // Arrival order after a backward seek: 4 s, 6 s, then 0 s and 2 s.
    let rows = [
        timed(0, 4000, 2000),
        timed(10, 6000, 2000),
        timed(20, 0, 2000),
        timed(30, 2000, 2000),
    ];
    let refs: Vec<&IndexRow> = rows.iter().collect();
    let none = HashMap::new();
    let ordered = order_media(&plain_clock(&none), &refs);
    assert!(ordered.timed);
    assert_eq!(offsets(&ordered.rows), [20, 30, 0, 10]);
    assert_eq!(ordered.start, Some(0.0));
    assert_eq!((ordered.covered, ordered.partial), (0, 0));
    assert!(ordered.gaps.is_empty() && ordered.durations_known && !ordered.wants_shift);
    assert!(!same_rows(&ordered.rows, &refs), "the order changed");
}

#[test]
fn the_timestamp_offset_counts_for_the_order() {
    // A second period restarts tfdt at 0 and is moved 10 s later by its offset.
    let mut later = timed(0, 0, 2000);
    later.tso = Some(10.0);
    let rows = [later, timed(10, 2000, 2000)];
    let refs: Vec<&IndexRow> = rows.iter().collect();
    let none = HashMap::new();
    let clock = plain_clock(&none);
    let ordered = order_media(&clock, &refs);
    assert_eq!(
        offsets(&ordered.rows),
        [10, 0],
        "tfdt alone would keep the arrival order"
    );
    assert!(ordered.wants_shift);
    assert_eq!(ordered.start, Some(2.0));
    assert_eq!(clock.pts(&rows[0]), 10_000);
    assert_eq!(clock.shift_for(&rows[0], None), 10_000);
    assert_eq!(clock.shift_for(&rows[1], None), 0);
    assert_eq!(
        ordered.gaps,
        [Gaps {
            count: 1,
            missing: 6000,
            exact: true,
            scale: 1000
        }],
        "4 s to 10 s were never fetched"
    );
}

#[test]
fn a_covered_segment_is_left_out_and_a_partial_overlap_kept() {
    let rows = [
        timed(0, 0, 4000),
        timed(10, 0, 2000), // a part of the first, fetched again with another cut
        timed(20, 3000, 2000), // overlaps the first by a second
        timed(30, 5000, 2000),
    ];
    let refs: Vec<&IndexRow> = rows.iter().collect();
    let none = HashMap::new();
    let ordered = order_media(&plain_clock(&none), &refs);
    assert_eq!(offsets(&ordered.rows), [0, 20, 30]);
    assert_eq!((ordered.covered, ordered.partial), (1, 1));
}

#[test]
fn coverage_needs_a_duration_on_every_row() {
    let mut unknown = timed(10, 0, 2000);
    unknown.dur = None;
    let rows = [timed(0, 0, 4000), unknown];
    let refs: Vec<&IndexRow> = rows.iter().collect();
    let none = HashMap::new();
    let ordered = order_media(&plain_clock(&none), &refs);
    assert!(ordered.timed && !ordered.durations_known);
    assert_eq!(
        offsets(&ordered.rows),
        [0, 10],
        "the longer one first at the same start"
    );
    assert_eq!((ordered.covered, ordered.partial), (0, 0));
}

#[test]
fn a_row_without_tfdt_keeps_the_arrival_order() {
    let mut cut = timed(10, 0, 2000);
    cut.tfdt = None;
    let rows = [timed(0, 4000, 2000), cut, timed(20, 0, 2000)];
    let refs: Vec<&IndexRow> = rows.iter().collect();
    let none = HashMap::new();
    let ordered = order_media(&plain_clock(&none), &refs);
    assert!(!ordered.timed);
    assert!(same_rows(&ordered.rows, &refs));
    assert_eq!(ordered.start, None);
    let empty = order_media(&plain_clock(&none), &[]);
    assert!(!empty.timed && empty.rows.is_empty() && empty.start.is_none());
}

#[test]
fn sequence_mode_is_ordered_only_with_a_layout_for_every_row() {
    let mut a = timed(0, 5000, 2000);
    a.seq_mode = true;
    let mut b = timed(10, 0, 2000);
    b.seq_mode = true;
    let rows = [a, b];
    let refs: Vec<&IndexRow> = rows.iter().collect();
    let none = HashMap::new();
    let without = order_media(&plain_clock(&none), &refs);
    assert!(
        !without.timed,
        "no layout: the arrival order is all there is"
    );
    // The browser placed the first append at 0 and the second right after it, at 2 s, not at its tfdt of 0.
    let moves: SeqMoves = HashMap::from([((0, 0), -5000), ((0, 10), 2000)]);
    let clock = PartClock {
        seq_moves: Some(&moves),
        ..plain_clock(&none)
    };
    let with = order_media(&clock, &refs);
    assert!(with.timed && with.wants_shift);
    assert_eq!(offsets(&with.rows), [0, 10]);
    assert_eq!(with.start, Some(0.0));
    let half: SeqMoves = HashMap::from([((0, 0), -5000)]);
    let clock = PartClock {
        seq_moves: Some(&half),
        ..plain_clock(&none)
    };
    assert!(
        !order_media(&clock, &refs).timed,
        "a row the layout does not hold"
    );
}

#[test]
fn the_lift_moves_every_row_and_is_counted_per_track_timescale() {
    let rows = [timed(0, 0, 2000)];
    let none = HashMap::new();
    let lifted = PartClock {
        lift_ns: 500_000_000,
        ..plain_clock(&none)
    };
    assert_eq!(lifted.offset(&rows[0]), 500);
    assert_eq!(lifted.shift_for(&rows[0], None), 500);
    assert_eq!(
        lifted.shift_for(&init_row(0), None),
        0,
        "an init has no tfdt"
    );
    // Two tracks, each in its own timescale: the traf of track 2 counts in 48000.
    let two = HashMap::from([(1, 1000), (2, 48_000)]);
    let multi = PartClock {
        raw_tracks: 2,
        track_ts: &two,
        ..lifted
    };
    assert!(multi.can_shift());
    assert_eq!(multi.shift_for(&rows[0], Some(1)), 500);
    assert_eq!(multi.shift_for(&rows[0], Some(2)), 24_000);
    // Two tracks whose timescales the init does not give: nothing can be rewritten, the order still follows.
    let unknown = PartClock {
        raw_tracks: 2,
        ..lifted
    };
    assert!(!unknown.can_shift());
    assert_eq!(unknown.shift_for(&rows[0], None), 0);
    assert_eq!(unknown.pts(&rows[0]), 500);
}
