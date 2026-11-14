//! streamtap-core: `parts`.

use streamtap_core::parts::*;
use streamtap_core::wire::IndexRow;
use streamtap_testkit::*;

fn ranked_parts(rows: &[IndexRow]) -> Vec<u32> {
    let refs: Vec<&IndexRow> = rows.iter().collect();
    rank_parts(&refs).candidates[0]
        .ranked
        .iter()
        .map(|r| r.0)
        .collect()
}

/// Bytes alone do not decide: a padded part (many bytes, little time) and a
/// part whose claimed time the bytes cannot carry rank after a real one.
#[test]
fn the_part_selection_checks_bytes_against_time() {
    let rows = [
        init_row(0),
        claim(0, 500_000, Some(10_000)), // 50 kB/s: plausible
        init_row(1),
        claim(1, 900_000_000, Some(1_000)), // padded: 900 MB/s
        init_row(2),
        claim(2, 1_000, Some(3_600_000)), // an hour in 1 kB: inflated time
    ];
    let ranked = ranked_parts(&rows);
    assert_eq!(ranked[0], 0, "the real part first");
    assert_eq!(ranked.len(), 3, "the others stay available as fallbacks");
}

/// Among plausible parts the one with more media time wins, even with
/// fewer bytes (a low quality that ran longer).
#[test]
fn the_part_selection_prefers_more_media_time() {
    let rows = [
        init_row(0),
        claim(0, 4_000_000, Some(20_000)),
        init_row(1),
        claim(1, 1_000_000, Some(60_000)),
    ];
    assert_eq!(ranked_parts(&rows), vec![1, 0]);
}

/// Without durations nothing can be checked: the most bytes, as before.
#[test]
fn the_part_selection_falls_back_to_bytes() {
    let rows = [
        init_row(0),
        claim(0, 1_000, None),
        init_row(1),
        claim(1, 9_000, None),
    ];
    assert_eq!(ranked_parts(&rows), vec![1, 0]);
}

/// The tracks of a `SourceBuffer` run in parallel: their times are not
/// added up.
#[test]
fn the_part_selection_does_not_add_up_tracks() {
    let track = |part: u32, t: u32, len: u64, ms: u64| IndexRow {
        track: Some(t),
        ..claim(part, len, Some(ms))
    };
    let rows = [
        init_row(0),
        track(0, 1, 500_000, 10_000),
        track(0, 2, 500_000, 10_000),
        init_row(1),
        track(1, 1, 700_000, 12_000),
    ];
    assert_eq!(ranked_parts(&rows), vec![1, 0], "12 s beats 10 s, not 20 s");
}

/// A media row of `len` bytes that belongs to `MediaSource` `ms`.
fn of_source(ms: Option<u32>, len: u64) -> IndexRow {
    IndexRow {
        ms,
        ..row(0, len, false, Some(0), None)
    }
}

#[test]
fn the_media_source_with_the_most_bytes_is_the_video() {
    assert_eq!(main_media_source(&[]), None);
    let rows = [
        of_source(Some(1), 100), // a preroll
        of_source(Some(2), 4000),
        of_source(Some(2), 4000),
        of_source(None, 50),
    ];
    let main = main_media_source(&rows).expect("rows");
    assert_eq!(main.chosen, Some(2));
    assert_eq!(main.others, [(None, 50), (Some(1), 100)]);
    // Equal bytes: the earlier one.
    let tie = [of_source(Some(3), 10), of_source(Some(1), 10)];
    assert_eq!(main_media_source(&tie).expect("rows").chosen, Some(1));
}

#[test]
fn a_mark_does_not_count_as_bytes_of_its_media_source() {
    // A looping teaser repeats one segment: stored once, marked many times.
    let mut rows = vec![of_source(Some(1), 1000), of_source(Some(2), 3000)];
    for _ in 0..10 {
        rows.push(IndexRow {
            mark: true,
            ..of_source(Some(1), 1000)
        });
    }
    assert_eq!(main_media_source(&rows).expect("rows").chosen, Some(2));
}

#[test]
fn a_sourcebuffer_without_an_init_is_named_not_ranked() {
    let rows = [
        init_row(0),
        claim(0, 1000, Some(1000)),
        IndexRow {
            sb: 2,
            ..claim(0, 1000, Some(1000))
        },
    ];
    let refs: Vec<&IndexRow> = rows.iter().collect();
    let ranked = rank_parts(&refs);
    assert_eq!(ranked.without_init, [2]);
    assert_eq!(ranked.candidates.len(), 1);
    assert_eq!(ranked.candidates[0].sb, 1);
    assert_eq!(ranked.candidates[0].total, 1);
}
