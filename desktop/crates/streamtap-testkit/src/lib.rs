//! streamtap-testkit: fixtures for the tests of streamtap, made in memory.
//!
//! Boxes, segments, wire headers and index rows as small as a test needs them; no file is read and no tool is run,
//! so the tests of `streamtap-core` stay free of I/O and the sink's tests share the same builders.

#![allow(
    clippy::must_use_candidate,
    clippy::missing_panics_doc,
    reason = "builders for tests: a fixture that cannot be built is a bug of the test"
)]

use streamtap_core::wire::{IndexRow, SegmentHeader};

/// One MP4 box of `kind` around `body` (32-bit size).
pub fn mp4_box(kind: [u8; 4], body: &[u8]) -> Vec<u8> {
    let mut v = u32::try_from(8 + body.len())
        .expect("small")
        .to_be_bytes()
        .to_vec();
    v.extend_from_slice(&kind);
    v.extend_from_slice(body);
    v
}

/// A minimal media segment: moof > traf > tfdt (version 0) with `tfdt`.
pub fn moof(tfdt: u32) -> Vec<u8> {
    let mut tfdt_body = vec![0, 0, 0, 0];
    tfdt_body.extend_from_slice(&tfdt.to_be_bytes());
    mp4_box(*b"moof", &mp4_box(*b"traf", &mp4_box(*b"tfdt", &tfdt_body)))
}

/// The `tfdt` of a segment made by [`moof`].
pub fn tfdt_of(seg: &[u8]) -> u32 {
    u32::from_be_bytes([seg[28], seg[29], seg[30], seg[31]])
}

/// An init segment with `traks` empty trak boxes.
pub fn init_with_traks(traks: usize) -> Vec<u8> {
    let mut moov = Vec::new();
    for _ in 0..traks {
        moov.extend(mp4_box(*b"trak", b""));
    }
    let mut v = mp4_box(*b"ftyp", b"iso6");
    v.extend(mp4_box(*b"moov", &moov));
    v
}

/// A row of `index.jsonl`: part 0 of `SourceBuffer` 1 at 1000 ticks/s, MP4.
pub fn row(offset: u64, len: u64, is_init: bool, tfdt: Option<u64>, tso: Option<f64>) -> IndexRow {
    IndexRow {
        sb: 1,
        ms: Some(1),
        part: 0,
        offset,
        len,
        is_init,
        tfdt,
        track: None,
        timescale: Some(1000),
        dur: None,
        tso,
        seq_mode: false,
        tso_set: false,
        mark: false,
        mime: "video/mp4".into(),
        kind: "mp4".into(),
    }
}

/// A row of a part for the selection tests: `len` bytes claiming `dur_ms`.
pub fn claim(part: u32, len: u64, dur_ms: Option<u64>) -> IndexRow {
    IndexRow {
        part,
        dur: dur_ms,
        ..row(0, len, false, Some(0), None)
    }
}

/// The init row of `part` for the selection tests.
pub fn init_row(part: u32) -> IndexRow {
    IndexRow {
        part,
        ..row(0, 10, true, None, None)
    }
}

/// The header of one append as the extension sends it: MP4 at 1000 ticks/s.
pub fn seg(sb: u32, is_init: bool, tfdt: Option<u64>, tso: Option<f64>) -> SegmentHeader {
    SegmentHeader {
        sb,
        seq: 1,
        ms: Some(1),
        mime: "video/mp4".into(),
        is_init,
        init_cont: false,
        kind: "mp4".into(),
        tfdt,
        track: None,
        timescale: Some(1000),
        dur: None,
        tso,
        seq_mode: false,
        tso_set: false,
    }
}

/// A moof of the given sequence number, cut before its tfdt: what a chunked fetch can deliver
/// as an append of its own.
pub fn cut_moof_head(seq: u32) -> Vec<u8> {
    let mut mfhd = vec![0, 0, 0, 0];
    mfhd.extend(seq.to_be_bytes());
    let mut head = 100u32.to_be_bytes().to_vec(); // the moof goes on in the next chunk
    head.extend(b"moof");
    head.extend(mp4_box(*b"mfhd", &mfhd));
    head
}
