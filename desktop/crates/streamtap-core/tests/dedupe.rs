//! streamtap-core: `dedupe`.

use streamtap_core::dedupe::*;
use streamtap_core::wire::SegmentHeader;
use streamtap_testkit::*;

#[test]
fn tso_millis_handles_odd_values() {
    assert_eq!(tso_millis(None), 0);
    assert_eq!(tso_millis(Some(1.5)), 1500);
    assert_eq!(tso_millis(Some(-2.0)), -2000);
    assert_eq!(tso_millis(Some(f64::NAN)), 0);
    assert_eq!(tso_millis(Some(f64::INFINITY)), 0);
    assert_eq!(tso_millis(Some(1e300)), 1_000_000_000_000);
}

/// A timed media header at `tfdt` with a duration of 500 ticks.
fn media(tfdt: u64) -> SegmentHeader {
    let mut h = seg(1, false, Some(tfdt), None);
    h.dur = Some(500);
    h
}

#[test]
fn a_refetched_segment_is_a_duplicate() {
    let mut d = Dedupe::default();
    assert_eq!(
        d.admit(&media(1000), &moof(1000), 0, true),
        Admission::Write
    );
    assert_eq!(
        d.admit(&media(1500), &moof(1500), 40, true),
        Admission::Write
    );
    assert_eq!(
        d.admit(&media(1000), &moof(1000), 80, true),
        Admission::Duplicate,
        "same start, offset, duration and track: a re-fetch after a seek"
    );
    assert_eq!(d.seen.len(), 2, "a duplicate takes no place");
}

#[test]
fn the_key_holds_offset_duration_and_track() {
    let mut d = Dedupe::default();
    assert_eq!(
        d.admit(&media(1000), &moof(1000), 0, true),
        Admission::Write
    );
    // A new period restarts tfdt and moves the timeline with timestampOffset.
    let mut period = media(1000);
    period.tso = Some(30.0);
    assert_eq!(d.admit(&period, &moof(1000), 40, true), Admission::Write);
    // Another cut of the same range (an LL-HLS part after the full segment).
    let mut cut = media(1000);
    cut.dur = Some(250);
    assert_eq!(d.admit(&cut, &moof(1000), 80, true), Admission::Write);
    // The other track of the SourceBuffer shares start and duration.
    let mut track = media(1000);
    track.track = Some(2);
    assert_eq!(d.admit(&track, &moof(1000), 120, true), Admission::Write);
    assert_eq!(
        d.admit(&track, &moof(1000), 160, true),
        Admission::Duplicate
    );
    // An offset that differs by less than a millisecond is the same place.
    let mut near = period.clone();
    near.tso = Some(30.0004);
    assert_eq!(d.admit(&near, &moof(1000), 200, true), Admission::Duplicate);
}

#[test]
fn the_untimed_tail_of_a_duplicate_is_a_duplicate() {
    let mut d = Dedupe::default();
    let untimed = seg(1, false, None, None);
    assert_eq!(
        d.admit(&media(1000), &moof(1000), 0, true),
        Admission::Write
    );
    assert_eq!(
        d.admit(&untimed, b"rest of the mdat", 40, true),
        Admission::Write,
        "the continuation of a new segment"
    );
    assert_eq!(
        d.admit(&media(1000), &moof(1000), 56, true),
        Admission::Duplicate
    );
    assert_eq!(
        d.admit(&untimed, b"rest of the mdat", 56, true),
        Admission::Duplicate,
        "or it would land as orphan bytes"
    );
    assert_eq!(
        d.admit(&media(1500), &moof(1500), 56, true),
        Admission::Write
    );
    assert_eq!(
        d.admit(&untimed, b"rest of the mdat", 96, true),
        Admission::Write,
        "a new timed segment ends the tail"
    );
}

#[test]
fn sequence_mode_keys_on_the_payload_and_marks_its_duplicates() {
    let mut d = Dedupe::default();
    let mut h = media(0);
    h.seq_mode = true;
    assert_eq!(d.admit(&h, b"first period", 0, true), Admission::Write);
    assert_eq!(
        d.admit(&h, b"other period", 12, true),
        Admission::Write,
        "a new period may reuse tfdt values: other bytes are other media"
    );
    assert_eq!(
        d.admit(&h, b"first period", 24, true),
        Admission::Mark { offset: 0 },
        "the same bytes again: its place is kept, pointing at the row it repeats"
    );
    assert_eq!(
        d.admit(&h, b"other period", 24, true),
        Admission::Mark { offset: 12 }
    );
}

#[test]
fn a_head_cut_before_its_tfdt_is_keyed_by_its_bytes() {
    let mut d = Dedupe::default();
    let untimed = seg(1, false, None, None);
    assert!(starts_a_numbered_moof(&cut_moof_head(7)));
    assert_eq!(
        d.admit(&untimed, &cut_moof_head(7), 0, true),
        Admission::Write
    );
    assert_eq!(d.admit(&untimed, b"its rest", 24, true), Admission::Write);
    assert_eq!(
        d.admit(&untimed, &cut_moof_head(7), 32, true),
        Admission::Duplicate,
        "the same sequence number: the same segment, fetched again"
    );
    assert_eq!(
        d.admit(&untimed, b"its rest", 32, true),
        Admission::Duplicate
    );
    assert_eq!(
        d.admit(&untimed, &cut_moof_head(8), 32, true),
        Admission::Write,
        "the next segment"
    );
    assert_eq!(d.admit(&untimed, b"its rest", 56, true), Admission::Write);
}

#[test]
fn a_full_set_takes_no_new_segment_but_still_knows_the_old_ones() {
    let mut d = Dedupe::default();
    assert_eq!(
        d.admit(&media(1000), &moof(1000), 0, true),
        Admission::Write
    );
    assert_eq!(
        d.admit(&media(1500), &moof(1500), 40, false),
        Admission::NoRoom
    );
    assert_eq!(d.seen.len(), 1, "nothing remembered without room");
    assert_eq!(
        d.admit(&media(1000), &moof(1000), 40, false),
        Admission::Duplicate
    );
    assert_eq!(
        d.admit(&seg(1, false, None, None), &cut_moof_head(1), 40, false),
        Admission::NoRoom
    );
}

#[test]
fn only_keyed_appends_take_a_place() {
    assert!(Dedupe::remembers(&media(0), b""));
    assert!(Dedupe::remembers(
        &seg(1, false, None, None),
        &cut_moof_head(1)
    ));
    assert!(!Dedupe::remembers(&seg(1, false, None, None), b"a tail"));
}
