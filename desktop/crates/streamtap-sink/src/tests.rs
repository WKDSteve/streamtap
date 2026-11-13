use super::*;

const GIB: u64 = 1 << 30;

/// A fresh directory under the system temp directory, removed again when
/// the guard drops: the name carries the pid, so a leftover would pile up
/// with every run (CI on the author's machine included).
struct TestDir(PathBuf);

impl std::ops::Deref for TestDir {
    type Target = Path;
    fn deref(&self) -> &Path {
        &self.0
    }
}

impl AsRef<Path> for TestDir {
    fn as_ref(&self) -> &Path {
        &self.0
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn temp_dir(name: &str) -> TestDir {
    let d = std::env::temp_dir().join(format!("streamtap-test-{}-{name}", std::process::id()));
    let _ = fs::remove_dir_all(&d);
    fs::create_dir_all(&d).expect("temp dir");
    TestDir(d)
}

/// A session together with the directory it lives in; the session drops
/// first (field order), so its files are closed when the directory goes.
struct TestSession {
    s: Session,
    _dir: TestDir,
}

impl std::ops::Deref for TestSession {
    type Target = Session;
    fn deref(&self) -> &Session {
        &self.s
    }
}

impl std::ops::DerefMut for TestSession {
    fn deref_mut(&mut self) -> &mut Session {
        &mut self.s
    }
}

#[test]
fn test_dirs_are_removed_when_dropped() {
    let dir = temp_dir("dropped");
    fs::write(dir.join("f"), b"x").expect("write");
    let path = dir.to_path_buf();
    drop(dir);
    assert!(!path.exists(), "{} left behind", path.display());
    let s = session("dropped-session");
    let root = s.dir.parent().expect("parent").to_path_buf();
    drop(s);
    assert!(!root.exists(), "{} left behind", root.display());
}

fn mp4_box(kind: [u8; 4], body: &[u8]) -> Vec<u8> {
    let mut v = u32::try_from(8 + body.len())
        .expect("small")
        .to_be_bytes()
        .to_vec();
    v.extend_from_slice(&kind);
    v.extend_from_slice(body);
    v
}

/// A minimal media segment: moof > traf > tfdt (version 0) with `tfdt`.
fn moof(tfdt: u32) -> Vec<u8> {
    let mut tfdt_body = vec![0, 0, 0, 0];
    tfdt_body.extend_from_slice(&tfdt.to_be_bytes());
    mp4_box(*b"moof", &mp4_box(*b"traf", &mp4_box(*b"tfdt", &tfdt_body)))
}

fn tfdt_of(seg: &[u8]) -> u32 {
    u32::from_be_bytes([seg[28], seg[29], seg[30], seg[31]])
}

/// An init segment with `traks` empty trak boxes.
fn init_with_traks(traks: usize) -> Vec<u8> {
    let mut moov = Vec::new();
    for _ in 0..traks {
        moov.extend(mp4_box(*b"trak", b""));
    }
    let mut v = mp4_box(*b"ftyp", b"iso6");
    v.extend(mp4_box(*b"moov", &moov));
    v
}

fn row(offset: u64, len: u64, is_init: bool, tfdt: Option<u64>, tso: Option<f64>) -> IndexRow {
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

/// (bytes, `is_init`, tfdt, tso) of one test segment.
type TestSeg<'a> = (&'a [u8], bool, Option<u64>, Option<f64>);

/// Writes `segs` as sb1.p0.bin and returns the rows (with the given tfdt / tso).
fn write_part(dir: &Path, segs: &[TestSeg<'_>]) -> Vec<IndexRow> {
    let mut raw = Vec::new();
    let mut rows = Vec::new();
    for (bytes, is_init, tfdt, tso) in segs {
        rows.push(row(
            raw.len() as u64,
            bytes.len() as u64,
            *is_init,
            *tfdt,
            *tso,
        ));
        raw.extend_from_slice(bytes);
    }
    fs::write(dir.join("sb1.p0.bin"), raw).expect("write part");
    rows
}

fn order(dir: &Path, rows: &[IndexRow]) -> (Vec<u8>, PathBuf, Option<f64>) {
    let refs: Vec<&IndexRow> = rows.iter().collect();
    // As mux_inputs does: the sequence-mode layout over the SourceBuffer.
    let leads = seq_leads(dir, 1, &refs);
    let (moves, _) = seq_layout(&refs, &leads);
    let (path, start, _, _) = ordered_part(dir, 1, 0, &refs, Some(&moves), 0).expect("ordered");
    (fs::read(&path).expect("read"), path, start)
}

#[test]
fn tso_millis_handles_odd_values() {
    assert_eq!(tso_millis(None), 0);
    assert_eq!(tso_millis(Some(1.5)), 1500);
    assert_eq!(tso_millis(Some(-2.0)), -2000);
    assert_eq!(tso_millis(Some(f64::NAN)), 0);
    assert_eq!(tso_millis(Some(f64::INFINITY)), 0);
    assert_eq!(tso_millis(Some(1e300)), 1_000_000_000_000);
}

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
fn encrypted_payloads_are_recognised() {
    assert!(looks_encrypted(b"\0\0\0\x10moov....encv...."));
    assert!(!looks_encrypted(b"\0\0\0\x10moov....avc1...."));
    let mut h = seg(1, false, Some(0), None);
    let senc = mp4_box(*b"moof", &mp4_box(*b"traf", &mp4_box(*b"senc", b"")));
    assert!(
        payload_encrypted(&h, &senc, false, Container::Mp4),
        "senc in a moof"
    );
    assert!(
        !payload_encrypted(&h, &moof(0), false, Container::Mp4),
        "clear moof"
    );
    for aux in [*b"saiz", *b"saio"] {
        let cenc = mp4_box(*b"moof", &mp4_box(*b"traf", &mp4_box(aux, b"")));
        assert!(
            payload_encrypted(&h, &cenc, false, Container::Mp4),
            "auxiliary info without senc"
        );
    }
    let mut moov = mp4_box(*b"ftyp", b"iso6");
    moov.extend(mp4_box(*b"moov", b"..encv.."));
    assert!(
        payload_encrypted(&h, &moov, false, Container::Mp4),
        "moov in a chunk not flagged as init"
    );
    h.kind = "webm".into();
    h.is_init = true;
    assert!(payload_encrypted(
        &h,
        &[0x16, 0x54, 0x6d, 0x80, 0x50, 0x35, 0x47, 0xe2],
        false,
        Container::Webm
    ));
    assert!(!payload_encrypted(
        &h,
        &[0x16, 0x54, 0xae, 0x6b],
        false,
        Container::Mp4
    ));
}

#[test]
fn shift_tfdt_moves_and_refuses_underflow() {
    let mut seg = moof(1000);
    assert!(shift_tfdt(&mut seg, 500));
    assert_eq!(tfdt_of(&seg), 1500);
    let before = seg.clone();
    assert!(!shift_tfdt(&mut seg, -2000), "negative result is refused");
    assert_eq!(seg, before, "a refused shift leaves the segment unchanged");
    let mut no_moof = b"\0\0\0\x08mdat".to_vec();
    assert!(!shift_tfdt(&mut no_moof, 1), "nothing to shift");
}

#[test]
fn box_walker_survives_garbage_sizes() {
    for buf in [
        b"\0\0\0\x03moofxxxx".to_vec(),
        b"\0\0\x10\0moofxxxx".to_vec(),
        b"\0\0\0\x01moof\0\0".to_vec(),
    ] {
        assert_eq!(boxes_in(&buf, 0, buf.len()), Vec::new());
    }
}

#[test]
fn trak_count_reads_the_init() {
    assert_eq!(trak_count(&init_with_traks(1)), 1);
    assert_eq!(trak_count(&init_with_traks(2)), 2);
    assert_eq!(trak_count(b"INIT"), 0);
}

#[test]
fn ordered_part_sorts_by_tfdt_with_init_first() {
    let dir = temp_dir("sort");
    let (a, b) = (moof(2000), moof(1000));
    let rows = write_part(
        &dir,
        &[
            (b"INIT", true, None, None),
            (&a, false, Some(2000), None),
            (&b, false, Some(1000), None),
        ],
    );
    let (got, path, start) = order(&dir, &rows);
    assert!(path.ends_with("sb1.p0.sorted.bin"));
    assert_eq!(&got[..4], b"INIT");
    assert_eq!(tfdt_of(&got[4..]), 1000);
    assert_eq!(tfdt_of(&got[36..]), 2000);
    assert_eq!(start, Some(1.0), "first media at 1000 ticks of 1000/s");
}

/// A sorted copy left by an earlier layout round is removed when the raw
/// file serves now.
#[test]
fn ordered_part_removes_a_stale_sorted_copy_when_raw_serves() {
    let dir = temp_dir("stale");
    let (a, b) = (moof(1000), moof(2000));
    let rows = write_part(
        &dir,
        &[
            (b"INIT", true, None, None),
            (&a, false, Some(1000), None),
            (&b, false, Some(2000), None),
        ],
    );
    let stale = dir.join("sb1.p0.sorted.bin");
    fs::write(&stale, b"old").expect("stale");
    let (_, path, _) = order(&dir, &rows);
    assert!(path.ends_with("sb1.p0.bin"), "raw serves: {path:?}");
    assert!(!stale.exists(), "stale copy left");
}

#[test]
fn the_unmoved_notice_does_not_warn_of_a_jump_for_webm() {
    let webm = unmoved_notice((1, 0), 2, true);
    assert!(
        !webm.contains("may jump") && webm.contains("probed start"),
        "{webm}"
    );
    assert!(unmoved_notice((1, 0), 2, false).contains("may jump"));
}

#[test]
fn a_hanging_ffprobe_is_given_up_after_a_few_timeouts() {
    let tools = Tools::default();
    let mut prober = Prober {
        tools: &tools,
        missing: false,
        timeouts: 0,
        warnings: Vec::new(),
    };
    let timeout = SinkError::ToolTimeout {
        tool: Tool::Ffprobe,
        after: Duration::from_secs(60),
    };
    for i in 0..MAX_PROBE_TIMEOUTS {
        assert!(!prober.missing, "still asked before timeout {i}");
        prober.note_error(&timeout, &format!("sb1.p{i}"));
    }
    assert!(prober.missing, "not asked any more");
    let last = prober.warnings.last().expect("warning");
    assert!(last.contains("timed out 3 times in a row"), "{last}");
}

#[test]
fn a_single_ffprobe_timeout_stays_a_warning_for_that_part() {
    let tools = Tools::default();
    let mut prober = Prober {
        tools: &tools,
        missing: false,
        timeouts: 0,
        warnings: Vec::new(),
    };
    let timeout = SinkError::ToolTimeout {
        tool: Tool::Ffprobe,
        after: Duration::from_secs(60),
    };
    prober.note_error(&timeout, "sb1.p0");
    assert!(!prober.missing);
    assert_eq!(prober.warnings.len(), 1);
}

#[test]
fn a_prober_warning_is_kept_once() {
    let tools = Tools::default();
    let mut prober = Prober {
        tools: &tools,
        missing: false,
        timeouts: 0,
        warnings: Vec::new(),
    };
    prober.warn("sb1.p0: x".into());
    prober.warn("sb1.p0: x".into());
    prober.warn("sb1.p1: x".into());
    assert_eq!(prober.warnings.len(), 2);
}

#[test]
fn ordered_part_keeps_arrival_order_without_tfdt_or_in_sequence_mode() {
    let dir = temp_dir("untimed");
    let rows = write_part(
        &dir,
        &[
            (b"INIT", true, None, None),
            (b"AAAA", false, Some(1000), None),
            (b"BBBB", false, None, None),
            (b"CCCC", false, Some(500), None),
        ],
    );
    let (got, path, start) = order(&dir, &rows);
    assert!(path.ends_with("sb1.p0.bin"), "already in arrival order");
    assert_eq!(got, b"INITAAAABBBBCCCC");
    assert_eq!(start, None);

    // Sequence mode: sorted by tfdt, the (stale) timestampOffset ignored.
    let mut rows = write_part(
        &dir,
        &[
            (b"INIT", true, None, None),
            (b"AAAA", false, Some(1000), None),
            (b"CCCC", false, Some(500), Some(99.0)),
        ],
    );
    for r in &mut rows {
        r.seq_mode = true;
    }
    assert_eq!(
        order(&dir, &rows).0,
        b"INITAAAACCCC",
        "sequence mode: arrival order (the sink dropped duplicates)"
    );
}

#[test]
fn ordered_part_writes_every_init_row_first() {
    let dir = temp_dir("initrows");
    let rows = write_part(
        &dir,
        &[
            (b"IN", true, None, None),
            (b"IT", true, None, None),
            (b"BBBB", false, Some(2000), None),
            (b"AAAA", false, Some(1000), None),
        ],
    );
    assert_eq!(
        order(&dir, &rows).0,
        b"INITAAAABBBB",
        "a split init stays whole"
    );
}

/// An init row after the media (the overflow of a duplicate init, as an
/// older sink stored it) is not joined into the init.
#[test]
fn ordered_part_leaves_out_stray_init_rows() {
    let dir = temp_dir("strayinit");
    let m = moof(0);
    let rows = write_part(
        &dir,
        &[
            (b"IN", true, None, None),
            (b"IT", true, None, None),
            (&m, false, Some(0), None),
            (b"XX", true, None, None),
        ],
    );
    let (got, path, _) = order(&dir, &rows);
    assert!(path.ends_with("sb1.p0.sorted.bin"), "not the raw file");
    assert_eq!(got, [&b"INIT"[..], &m].concat());
}

#[test]
fn ordered_part_moves_init_to_the_front() {
    let dir = temp_dir("initlate");
    let rows = write_part(
        &dir,
        &[(b"MMMM", false, None, None), (b"INIT", true, None, None)],
    );
    assert_eq!(order(&dir, &rows).0, b"INITMMMM");
}

#[test]
fn ordered_part_applies_timestamp_offset() {
    let dir = temp_dir("tso");
    let (p1, p2) = (moof(5000), moof(0));
    let rows = write_part(
        &dir,
        &[
            (b"INIT", true, None, None),
            (&p1, false, Some(5000), None),
            (&p2, false, Some(0), Some(10.0)),
        ],
    );
    let got = order(&dir, &rows).0;
    assert_eq!(tfdt_of(&got[4..]), 5000);
    assert_eq!(
        tfdt_of(&got[36..]),
        10_000,
        "tfdt shifted by 10 s at timescale 1000"
    );
}

#[test]
fn ordered_part_does_not_shift_multi_track_buffers() {
    let dir = temp_dir("multitrak");
    let init = init_with_traks(2);
    let p = moof(0);
    let rows = write_part(
        &dir,
        &[(&init, true, None, None), (&p, false, Some(0), Some(10.0))],
    );
    let got = order(&dir, &rows).0;
    assert_eq!(
        tfdt_of(&got[init.len()..]),
        0,
        "two tracks: no single timescale to shift by"
    );

    // A second period (tfdt restarts, tso 10 s) is neither dropped as
    // covered nor sorted into the first one.
    let (a, b) = (moof(0), moof(0));
    let mut rows = write_part(
        &dir,
        &[
            (&init, true, None, None),
            (&b, false, Some(0), Some(10.0)),
            (&a, false, Some(0), None),
        ],
    );
    rows[1].dur = Some(1000);
    rows[2].dur = Some(1000);
    let got = order(&dir, &rows).0;
    assert_eq!(got.len(), init.len() + 64, "both periods kept");
}

#[test]
fn ordered_part_leaves_out_covered_segments() {
    let dir = temp_dir("overlap");
    let (part_a, part_b, full) = (moof(0), moof(1000), moof(0));
    let mut rows = write_part(
        &dir,
        &[
            (b"INIT", true, None, None),
            (&part_a, false, Some(0), None),
            (&part_b, false, Some(1000), None),
            (&full, false, Some(0), None),
        ],
    );
    rows[1].dur = Some(1000);
    rows[2].dur = Some(1000);
    rows[3].dur = Some(2000);
    let got = order(&dir, &rows).0;
    // The full segment sorts first (longer at the same start) and covers both parts.
    assert_eq!(got.len(), 4 + 32, "init + the full segment only");
}

/// Segments of two tracks that cover the same time are not each other's
/// duplicates: the track ID of the first traf keeps them apart.
#[test]
fn coverage_is_per_track() {
    let dir = temp_dir("pertrack");
    let (a, b) = (moof(0), moof(0));
    let mut rows = write_part(
        &dir,
        &[
            (b"INIT", true, None, None),
            (&a, false, Some(0), None),
            (&b, false, Some(0), None),
        ],
    );
    rows[1].dur = Some(1000);
    rows[2].dur = Some(1000);
    // Without a track ID the second one looks like a re-fetch.
    assert_eq!(order(&dir, &rows).0.len(), 4 + 32, "same track: covered");
    rows[1].track = Some(1);
    rows[2].track = Some(2);
    assert_eq!(order(&dir, &rows).0.len(), 4 + 64, "two tracks: both kept");
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

/// A row of a part for the selection tests: `len` bytes claiming `dur_ms`.
fn claim(part: u32, len: u64, dur_ms: Option<u64>) -> IndexRow {
    IndexRow {
        part,
        dur: dur_ms,
        ..row(0, len, false, Some(0), None)
    }
}

fn init_row(part: u32) -> IndexRow {
    IndexRow {
        part,
        ..row(0, 10, true, None, None)
    }
}

fn ranked_parts(rows: &[IndexRow]) -> Vec<u32> {
    let refs: Vec<&IndexRow> = rows.iter().collect();
    pick_parts(&refs)[0].ranked.iter().map(|r| r.0).collect()
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

/// Appends of two tracks that share a start and duration are not each
/// other's re-fetch; the same track again is.
#[test]
fn dedupe_keeps_the_tracks_of_one_sourcebuffer_apart() {
    let mut s = session("dedupetrack");
    write(&mut s, &seg(1, true, None, None), &init_with_traks(2));
    let media = |track: Option<u32>| {
        let mut h = seg(1, false, Some(1000), None);
        h.dur = Some(500);
        h.track = track;
        h
    };
    write(&mut s, &media(Some(1)), &moof(1000));
    write(&mut s, &media(Some(2)), &moof(1000));
    write(&mut s, &media(Some(1)), &moof(1000)); // re-fetch of the first
    let media_rows = index_rows(&s.dir)
        .into_iter()
        .filter(|r| r["is_init"] == false)
        .count();
    assert_eq!(media_rows, 2, "one per track, the re-fetch dropped");
}

#[test]
fn gap_report_survives_huge_steps() {
    let untimed = |starts: &[i128]| -> Vec<(i128, Option<u64>)> {
        starts.iter().map(|s| (*s, None)).collect()
    };
    report_gaps(1, 0, &untimed(&[0, 10, i128::from(u64::MAX)]), 1000);
    report_gaps(
        1,
        0,
        &untimed(&[0, i128::from(u64::MAX), i128::from(u64::MAX) * 2]),
        1,
    );
    let huge = [
        (0, Some(u64::MAX)),
        (i128::from(u64::MAX) * 3, Some(u64::MAX)),
    ];
    report_gaps(1, 0, &huge, 1);
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

#[test]
fn container_is_parsed_once_from_the_wire_kind() {
    assert_eq!(Container::from_kind("mp4"), Container::Mp4);
    assert_eq!(Container::from_kind("webm"), Container::Webm);
    // Anything else is `Other`: MP4 code paths, but no input format.
    for other in ["", "MP4", "ogg", "mp4 "] {
        assert_eq!(Container::from_kind(other), Container::Other, "{other:?}");
    }
    assert_eq!(Container::default(), Container::Other);
    assert!(Container::Webm.is_webm() && !Container::Mp4.is_webm() && !Container::Other.is_webm());

    let mp4_head = *b"\0\0\0\x18ftyp";
    let webm_head = [0x1a, 0x45, 0xdf, 0xa3, 0, 0, 0, 0];
    assert_eq!(Container::Mp4.head_format(mp4_head), Some("mp4"));
    assert_eq!(Container::Webm.head_format(webm_head), Some("matroska"));
    assert_eq!(
        Container::Mp4.head_format(webm_head),
        None,
        "kind and magic must agree"
    );
    assert_eq!(Container::Webm.head_format(mp4_head), None);
    assert_eq!(Container::Other.head_format(mp4_head), None);

    let mut h = seg(0, true, None, None);
    h.kind = "webm".into();
    assert_eq!(h.container(), Container::Webm);
    h.kind = "mp4".into();
    assert_eq!(h.container(), Container::Mp4);
}

#[test]
fn mux_inputs_refuses_parts_that_are_not_media() {
    let dir = temp_dir("magic");
    let mpd = b"<?xml version=\"1.0\"?><MPD/>";
    fs::write(dir.join("sb1.p0.bin"), mpd).expect("write");
    let mut r = row(0, mpd.len() as u64, true, None, None);
    r.kind = "mp4".into();
    assert!(
        matches!(
            mux_inputs(&dir, &[r], &Tools::default()),
            Err(SinkError::Capture(_))
        ),
        "an MPD is not an mp4 part"
    );
    assert_eq!(input_format(Container::Mp4, &dir.join("sb1.p0.bin")), None);

    let init = init_with_traks(1);
    fs::write(dir.join("sb1.p0.bin"), &init).expect("write");
    assert_eq!(
        input_format(Container::Mp4, &dir.join("sb1.p0.bin")),
        Some("mp4")
    );
    assert_eq!(
        input_format(Container::Webm, &dir.join("sb1.p0.bin")),
        None,
        "kind and magic must agree"
    );
}

#[test]
fn inputs_are_aligned_by_their_start() {
    let inputs = [
        MuxInput {
            path: PathBuf::from("v"),
            format: "mp4",
            start: Some(2.08),
        },
        MuxInput {
            path: PathBuf::from("a"),
            format: "mp4",
            start: Some(0.0),
        },
    ];
    let args = input_args(&inputs);
    assert!(args[0]
        .windows(2)
        .any(|w| w[0] == "-itsoffset" && w[1] == "2.080000"));
    assert!(
        !args[1].iter().any(|a| a == "-itsoffset"),
        "the earliest input is not moved"
    );
    assert!(args[0].windows(2).any(|w| w[0] == "-f" && w[1] == "mp4"));
}

#[test]
fn split_message_rejects_malformed_frames() {
    assert!(split_message(b"\x01\0").is_err());
    assert!(
        split_message(b"\xff\xff\0\0{}").is_err(),
        "header longer than the message"
    );
    let mut bad = 5u32.to_le_bytes().to_vec();
    bad.extend_from_slice(b"{bad}");
    assert!(split_message(&bad).is_err());
    let frame = |json: &[u8]| {
        let mut f = u32::try_from(json.len())
            .expect("small")
            .to_le_bytes()
            .to_vec();
        f.extend_from_slice(json);
        f
    };
    assert!(
        split_message(&frame(br#"{"type":"segment","sb":"x","seq":1}"#)).is_err(),
        "a string sb id is rejected"
    );
    let big = frame(br#"{"type":"segment","sb":1,"seq":1,"tfdt":"18446744073709551615"}"#);
    match split_message(&big).expect("tfdt as a decimal string").0 {
        Header::Segment(h) => assert_eq!(h.tfdt, Some(u64::MAX)),
        _ => panic!("not a segment"),
    }
}

fn seg(sb: u32, is_init: bool, tfdt: Option<u64>, tso: Option<f64>) -> SegmentHeader {
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

fn budget(per_session: u64, total_max: u64) -> Arc<Budget> {
    Arc::new(Budget {
        per_session,
        total_max,
        total_used: AtomicU64::new(0),
    })
}

fn session(name: &str) -> TestSession {
    let dir = temp_dir(name);
    let s = Session::open(
        &dir,
        "test",
        "u",
        "t",
        &budget(16 * GIB, 64 * GIB),
        &refusals(),
    )
    .expect("session")
    .expect("within budget");
    TestSession { s, _dir: dir }
}

fn refusals() -> Arc<Refusals> {
    Arc::new(Refusals::default())
}

/// A session with the extension's session id `id`, sharing `refusals`
/// with the others (one sink process).
fn session_with(name: &str, id: &str, refusals: &Arc<Refusals>) -> TestSession {
    let dir = temp_dir(name);
    let s = Session::open(&dir, id, "u", "t", &budget(16 * GIB, 64 * GIB), refusals)
        .expect("session")
        .expect("within budget");
    TestSession { s, _dir: dir }
}

fn written(s: &Session, sb: u32) -> usize {
    fs::read(s.dir.join(format!("sb{sb}.p0.bin"))).map_or(0, |b| b.len())
}

fn index_lines(dir: &Path) -> usize {
    fs::read_to_string(dir.join("index.jsonl"))
        .expect("index")
        .lines()
        .count()
}

fn write(s: &mut Session, h: &SegmentHeader, bytes: &[u8]) {
    s.write_segment(h, bytes).expect("write");
}

#[test]
fn dedupe_includes_timestamp_offset_and_duration() {
    let mut s = session("dedupe");
    write(&mut s, &seg(1, true, None, None), b"INIT");
    write(&mut s, &seg(1, false, Some(0), None), b"P1");
    write(&mut s, &seg(1, false, Some(0), None), b"P1");
    write(&mut s, &seg(1, false, Some(0), Some(10.0)), b"P2");
    let mut longer = seg(1, false, Some(0), None);
    longer.dur = Some(4000);
    write(&mut s, &longer, b"FULL");
    assert_eq!(
        index_lines(&s.dir),
        4,
        "one dupe dropped; other offset and duration kept"
    );
}

#[test]
fn continuation_of_a_duplicate_is_dropped() {
    let mut s = session("tail");
    write(&mut s, &seg(1, true, None, None), b"INIT");
    write(&mut s, &seg(1, false, Some(0), None), b"HEAD");
    write(&mut s, &seg(1, false, None, None), b"TAIL");
    write(&mut s, &seg(1, false, Some(0), None), b"HEAD");
    write(&mut s, &seg(1, false, None, None), b"TAIL");
    write(&mut s, &seg(1, false, Some(1000), None), b"NEXT");
    assert_eq!(index_lines(&s.dir), 4, "init, head, tail, next");
    assert_eq!(
        fs::read(s.dir.join("sb1.p0.bin")).expect("part"),
        b"INITHEADTAILNEXT"
    );
}

#[test]
fn split_init_is_one_init_and_its_duplicate_is_dropped_whole() {
    let mut s = session("splitinit");
    write(&mut s, &seg(1, true, None, None), b"IN");
    write(&mut s, &seg(1, false, None, None), b"IT");
    write(&mut s, &seg(1, false, Some(0), None), b"M0");
    // Seek back: the player appends both init chunks again.
    write(&mut s, &seg(1, true, None, None), b"IN");
    write(&mut s, &seg(1, false, None, None), b"IT");
    write(&mut s, &seg(1, false, Some(1000), None), b"M1");
    assert_eq!(
        fs::read(s.dir.join("sb1.p0.bin")).expect("part"),
        b"INITM0M1"
    );
    let rows: Vec<serde_json::Value> = fs::read_to_string(s.dir.join("index.jsonl"))
        .expect("index")
        .lines()
        .map(|l| serde_json::from_str(l).expect("json"))
        .collect();
    assert_eq!(rows[1]["is_init"], true, "the continuation is init bytes");
}

/// An init over `MAX_PENDING_INIT`: a first chunk and 1 MiB `initCont`
/// chunks, the last of which no longer fits the pending init.
fn send_big_init(s: &mut Session, tag: u8) {
    write(s, &seg(1, true, None, None), &[tag; 8]);
    let mut cont = seg(1, false, None, None);
    cont.init_cont = true;
    for _ in 0..4 {
        write(s, &cont, &vec![tag; 1 << 20]);
    }
}

/// Init rows of `part` that come after one of its media rows.
fn init_rows_after_media(dir: &Path, part: u64) -> usize {
    let rows: Vec<serde_json::Value> = index_rows(dir)
        .into_iter()
        .filter(|r| r["part"] == part)
        .collect();
    let first_media = rows
        .iter()
        .position(|r| r["is_init"] == false)
        .unwrap_or(rows.len());
    rows[first_media..]
        .iter()
        .filter(|r| r["is_init"] == true)
        .count()
}

/// The overflow chunk of a re-appended init is not written as an init
/// row after the media: the init it continues was a duplicate.
#[test]
fn overflow_of_a_duplicate_init_is_not_written() {
    let mut s = session("overflowdup");
    send_big_init(&mut s, b'A');
    write(&mut s, &seg(1, false, Some(0), None), &moof(0));
    let before = written(&s, 1);
    send_big_init(&mut s, b'A');
    assert_eq!(written(&s, 1), before, "nothing of the duplicate");
    write(&mut s, &seg(1, false, Some(1000), None), &moof(1000));
    assert_eq!(
        init_rows_after_media(&s.dir, 0),
        0,
        "{:?}",
        index_rows(&s.dir)
    );
}

/// The overflow of a new init whose part file no longer fits the budget
/// is counted as skipped (the session stops), not as a duplicate.
#[test]
fn overflow_of_an_init_over_the_budget_counts_as_skipped() {
    // What the first init and its media cost.
    let mut probe = session("overflowcost");
    send_big_init(&mut probe, b'A');
    write(&mut probe, &seg(1, false, Some(0), None), &moof(0));
    let used = probe.bytes_total;
    let dir = temp_dir("overflowbudget");
    let shared = budget(used + PART_FILE_COST - 1, 64 * GIB);
    let s = Session::open(&dir, "test", "u", "t", &shared, &refusals())
        .expect("session")
        .expect("within budget");
    let mut s = TestSession { s, _dir: dir };
    send_big_init(&mut s, b'A');
    write(&mut s, &seg(1, false, Some(0), None), &moof(0));
    assert!(!s.stopped, "the first init fits");
    send_big_init(&mut s, b'B');
    assert!(s.stopped, "no room for the new part file");
    let track = &s.tracks[&1];
    assert_eq!((track.dupes, track.skipped), (0, 1));
}

/// The same for a switch back: nothing is appended to the earlier part.
#[test]
fn overflow_of_a_switch_back_init_is_not_written() {
    let mut s = session("overflowback");
    send_big_init(&mut s, b'A');
    write(&mut s, &seg(1, false, Some(0), None), &moof(0));
    write(&mut s, &seg(1, true, None, None), &init_with_traks(1));
    write(&mut s, &seg(1, false, Some(1000), None), &moof(1000));
    let before = written(&s, 1);
    send_big_init(&mut s, b'A');
    assert_eq!(written(&s, 1), before, "nothing appended to part 0");
    write(&mut s, &seg(1, false, Some(2000), None), &moof(2000));
    assert_eq!(
        init_rows_after_media(&s.dir, 0),
        0,
        "{:?}",
        index_rows(&s.dir)
    );
    assert_eq!(s.tracks[&1].part, 0, "back in part 0");
}

#[test]
fn sequence_mode_dedupes_by_tfdt_and_duration() {
    let mut s = session("seqmode");
    write(&mut s, &seg(1, true, None, None), b"INIT");
    let mut h = seg(1, false, Some(0), Some(3.0));
    h.seq_mode = true;
    write(&mut s, &h, b"A");
    h.tso = Some(7.0); // rewritten by the browser, not a new period
    h.tso_set = true;
    write(&mut s, &h, b"A");
    assert_eq!(
        fs::read(s.dir.join("sb1.p0.bin")).expect("part"),
        b"INITA",
        "a re-append after a seek back is a duplicate"
    );
    let rows = index_rows(&s.dir);
    assert_eq!(rows.len(), 3, "init, A, and the mark of the re-append");
    assert_eq!(rows[2]["mark"], true, "it holds its place: {rows:?}");
    assert_eq!(rows[2]["offset"], rows[1]["offset"], "pointing at A");
    assert_eq!(rows[2]["tso_set"], true, "with its timestampOffset set");
    assert!(rows[1].get("mark").is_none(), "stored rows carry no mark");
}

fn index_rows(dir: &Path) -> Vec<serde_json::Value> {
    fs::read_to_string(dir.join("index.jsonl"))
        .expect("index")
        .lines()
        .map(|l| serde_json::from_str(l).expect("json"))
        .collect()
}

#[test]
fn switching_back_to_an_earlier_init_continues_its_part() {
    let mut s = session("abr");
    write(&mut s, &seg(1, true, None, None), b"LOW-");
    write(&mut s, &seg(1, false, Some(0), None), b"l0");
    write(&mut s, &seg(1, true, None, None), b"HIGH");
    write(&mut s, &seg(1, false, Some(1000), None), b"h1");
    write(&mut s, &seg(1, true, None, None), b"LOW-");
    write(&mut s, &seg(1, false, Some(2000), None), b"l2");
    assert_eq!(fs::read(s.dir.join("sb1.p0.bin")).expect("p0"), b"LOW-l0l2");
    assert_eq!(fs::read(s.dir.join("sb1.p1.bin")).expect("p1"), b"HIGHh1");
    assert!(!s.dir.join("sb1.p2.bin").exists(), "no third part");
}

#[test]
fn media_before_the_first_init_gets_its_own_part() {
    let mut s = session("preinit");
    write(&mut s, &seg(1, false, Some(0), None), b"OLD");
    write(&mut s, &seg(1, true, None, None), b"INIT");
    write(&mut s, &seg(1, false, Some(1000), None), b"NEW");
    assert_eq!(fs::read(s.dir.join("sb1.p0.bin")).expect("p0"), b"OLD");
    assert_eq!(fs::read(s.dir.join("sb1.p1.bin")).expect("p1"), b"INITNEW");
}

#[test]
fn encrypted_track_is_not_written() {
    let mut s = session("drm");
    write(&mut s, &seg(1, true, None, None), b"....encv....");
    write(&mut s, &seg(1, false, Some(0), None), b"CIPHER");
    assert_eq!(index_lines(&s.dir), 0);
    assert_eq!(fs::read(s.dir.join("sb1.p0.bin")).expect("part").len(), 0);
    // A clear (complete) init afterwards is captured again.
    write(&mut s, &seg(1, true, None, None), &init_with_traks(1));
    write(&mut s, &seg(1, false, Some(0), None), b"MEDIA");
    assert_eq!(index_lines(&s.dir), 2);
}

/// The first chunk of an init after a refusal looks clear, but its
/// continuation (with `encv`) never arrives: the connection ends. The
/// refusal must hold, on this connection and the next one, whose media
/// (moofs without `senc`, `WebM` clusters) carries no marker.
#[test]
fn an_incomplete_init_after_a_refusal_keeps_it() {
    let shared = refusals();
    let mut enc = mp4_box(*b"ftyp", b"iso6");
    enc.extend(mp4_box(*b"moov", b"..encv.."));
    // ftyp plus a moov whose size runs past the chunk: no marker yet.
    let mut cut = mp4_box(*b"ftyp", b"iso6");
    cut.extend_from_slice(&4096u32.to_be_bytes());
    cut.extend_from_slice(b"moov");
    cut.extend(mp4_box(*b"mvhd", &[0; 16]));

    let mut first = session_with("cutlift-a", "tab-1", &shared);
    write(&mut first, &seg(1, true, None, None), &enc);
    write(&mut first, &seg(1, true, None, None), &cut);
    assert!(shared.contains("tab-1", 1), "not lifted by a first chunk");
    first.finish();
    assert_eq!(index_lines(&first.dir), 0, "the cut init is not written");
    assert!(first.tracks[&1].refused);
    drop(first);

    let mut again = session_with("cutlift-b", "tab-1", &shared);
    for t in 0..3u32 {
        let h = seg(1, false, Some(u64::from(t) * 1000), None);
        write(&mut again, &h, &moof(t * 1000));
    }
    let mut w = seg(1, false, Some(5000), None);
    w.kind = "webm".into();
    write(&mut again, &w, b"\x1f\x43\xb6\x75cluster");
    assert_eq!(index_lines(&again.dir), 0, "no row after the reconnect");
    assert_eq!(written(&again, 1), 0, "no bytes after the reconnect");
}

/// The same within one connection: media after an incomplete init that
/// followed a refusal stays refused.
#[test]
fn media_after_an_incomplete_init_after_a_refusal_is_refused() {
    let shared = refusals();
    let mut enc = mp4_box(*b"ftyp", b"iso6");
    enc.extend(mp4_box(*b"moov", b"..encv.."));
    let mut s = session_with("cutlift-same", "tab-1", &shared);
    write(&mut s, &seg(1, true, None, None), &enc);
    write(&mut s, &seg(1, true, None, None), b"\0\0\0\x08ftyp");
    for t in 0..3u32 {
        let h = seg(1, false, Some(u64::from(t) * 1000), None);
        write(&mut s, &h, &moof(t * 1000));
    }
    assert_eq!(index_lines(&s.dir), 0);
    assert!(s.tracks[&1].refused);
    assert!(shared.contains("tab-1", 1));
    // A complete clear init still lifts it.
    write(&mut s, &seg(1, true, None, None), &init_with_traks(1));
    write(&mut s, &seg(1, false, Some(9000), None), &moof(9000));
    assert_eq!(index_lines(&s.dir), 2);
    assert!(!shared.contains("tab-1", 1));
}

/// The three `WebM` encryption IDs in three appends, one of them cut in
/// two: no chunk shows all three, the whole init does.
#[test]
fn webm_markers_split_across_chunks_are_refused() {
    let shared = refusals();
    let webm = |is_init: bool, tfdt: Option<u64>| {
        let mut h = seg(1, is_init, tfdt, None);
        h.kind = "webm".into();
        h.mime = "video/webm".into();
        h
    };
    let init = [
        &[0x1a, 0x45, 0xdf, 0xa3, 0x80][..],
        &[
            0x18, 0x53, 0x80, 0x67, 0x01, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
        ],
        &[0x15, 0x49, 0xa9, 0x66, 0x80],
        &[
            0x16, 0x54, 0xae, 0x6b, 0x86, 0x6d, 0x80, 0x50, 0x35, 0x47, 0xe2,
        ],
    ]
    .concat();
    // Cut inside 0x6d80, between 0x5035 and 0x47e2.
    let (a, rest) = init.split_at(init.len() - 5);
    let (b, c) = rest.split_at(3);
    let mut s = session_with("webmsplit", "tab-1", &shared);
    write(&mut s, &webm(true, None), a);
    let mut cont = webm(false, None);
    cont.init_cont = true;
    write(&mut s, &cont, b);
    write(&mut s, &cont, c);
    for t in 0..3 {
        write(
            &mut s,
            &webm(false, Some(t * 1000)),
            b"\x1f\x43\xb6\x75cluster",
        );
    }
    assert_eq!(index_lines(&s.dir), 0, "nothing written");
    assert!(s.tracks[&1].refused);
    assert!(shared.contains("tab-1", 1), "remembered for a reconnect");
}

/// A reconnect opens a fresh session that never sees the (encrypted)
/// init: its media must still be refused from the `senc` in the moof.
/// Seen live on 2026-10-31, when an older sink wrote such media to disk.
#[test]
fn encrypted_media_without_init_after_reconnect_is_refused() {
    // senc inside the traf, as CENC media carries it, or only the
    // auxiliary info (saiz/saio) that points at the IVs.
    for marker in [*b"senc", *b"saiz"] {
        let mut s = session("drmreconnect");
        for t in 0..3u32 {
            let mut m = moof(t * 1000);
            m = mp4_box(
                *b"moof",
                &mp4_box(*b"traf", &[&m[16..], &mp4_box(marker, b"")[..]].concat()),
            );
            write(&mut s, &seg(1, false, Some(u64::from(t) * 1000), None), &m);
        }
        assert_eq!(index_lines(&s.dir), 0, "no row for encrypted media");
        assert_eq!(
            fs::read(s.dir.join("sb1.p0.bin")).map_or(0, |b| b.len()),
            0,
            "no bytes written"
        );
        assert!(s.tracks[&1].refused, "track refused without an init");
    }
}

/// `WebM` media carries no encryption marker: after a reconnect only the
/// refusal remembered from the first connection keeps it off the disk.
#[test]
fn refused_webm_track_stays_refused_after_reconnect() {
    let shared = refusals();
    let webm = |is_init: bool, tfdt: Option<u64>| {
        let mut h = seg(1, is_init, tfdt, None);
        h.kind = "webm".into();
        h.mime = "video/webm".into();
        h
    };
    // EBML header, Segment, Info, Tracks with ContentEncodings,
    // ContentEncryption and ContentEncKeyID.
    let init = [
        &[0x1a, 0x45, 0xdf, 0xa3, 0x80][..],
        &[
            0x18, 0x53, 0x80, 0x67, 0x01, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
        ],
        &[0x15, 0x49, 0xa9, 0x66, 0x80],
        &[
            0x16, 0x54, 0xae, 0x6b, 0x86, 0x6d, 0x80, 0x50, 0x35, 0x47, 0xe2,
        ],
    ]
    .concat();
    let cluster = b"\x1f\x43\xb6\x75cluster";

    let mut first = session_with("webmdrm-a", "tab-1", &shared);
    write(&mut first, &webm(true, None), &init);
    assert!(first.tracks[&1].refused, "encrypted init refused");
    first.finish();
    drop(first);

    // Reconnect: same browser session, no init, only clusters.
    let mut again = session_with("webmdrm-b", "tab-1", &shared);
    for t in 0..3 {
        write(&mut again, &webm(false, Some(t * 1000)), cluster);
    }
    assert_eq!(index_lines(&again.dir), 0, "no row after the reconnect");
    assert_eq!(written(&again, 1), 0, "no bytes after the reconnect");

    // Another browser session is not affected.
    let mut other = session_with("webmdrm-c", "tab-2", &shared);
    write(&mut other, &webm(false, Some(0)), cluster);
    assert_eq!(index_lines(&other.dir), 1, "other session captured");
}

/// The same for fMP4, with clear-looking moofs (no `senc`) after the
/// reconnect: only the remembered refusal holds them back.
#[test]
fn refused_mp4_track_stays_refused_after_reconnect() {
    let shared = refusals();
    let mut init = mp4_box(*b"ftyp", b"iso6");
    init.extend(mp4_box(*b"moov", b"..encv.."));

    let mut first = session_with("mp4drm-a", "tab-1", &shared);
    write(&mut first, &seg(1, true, None, None), &init);
    assert!(first.tracks[&1].refused, "encrypted init refused");
    drop(first);

    let mut again = session_with("mp4drm-b", "tab-1", &shared);
    for t in 0..3u32 {
        write(
            &mut again,
            &seg(1, false, Some(u64::from(t) * 1000), None),
            &moof(t * 1000),
        );
    }
    assert_eq!(index_lines(&again.dir), 0, "no row after the reconnect");
    assert_eq!(written(&again, 1), 0, "no bytes after the reconnect");
    // Another SourceBuffer of the same session is not affected.
    write(&mut again, &seg(2, false, Some(0), None), &moof(0));
    assert_eq!(index_lines(&again.dir), 1, "sb2 captured");
}

/// A clear init ends the refusal, on this connection and the next ones.
#[test]
fn clear_init_after_reconnect_lifts_the_refusal() {
    let shared = refusals();
    let mut enc = mp4_box(*b"ftyp", b"iso6");
    enc.extend(mp4_box(*b"moov", b"..encv.."));

    let mut first = session_with("lift-a", "tab-1", &shared);
    write(&mut first, &seg(1, true, None, None), &enc);
    drop(first);

    let mut again = session_with("lift-b", "tab-1", &shared);
    assert!(!again.tracks.contains_key(&1));
    write(&mut again, &seg(1, true, None, None), &init_with_traks(1));
    write(&mut again, &seg(1, false, Some(0), None), &moof(0));
    assert_eq!(index_lines(&again.dir), 2, "clear init and media written");
    drop(again);

    let mut third = session_with("lift-c", "tab-1", &shared);
    write(&mut third, &seg(1, false, Some(1000), None), &moof(1000));
    assert_eq!(index_lines(&third.dir), 1, "refusal forgotten");
}

#[test]
fn refusals_are_bounded_and_skip_anonymous_sessions() {
    let r = Refusals::default();
    for sb in 0..=u32::try_from(MAX_REFUSALS).expect("small") {
        r.insert("s", sb);
    }
    assert!(!r.contains("s", 0), "oldest forgotten");
    assert!(r.contains("s", 1));
    assert_eq!(r.keys().len(), MAX_REFUSALS);

    let shared = refusals();
    let mut enc = mp4_box(*b"ftyp", b"iso6");
    enc.extend(mp4_box(*b"moov", b"..encv.."));
    let mut anon = session_with("anon-a", "", &shared);
    write(&mut anon, &seg(1, true, None, None), &enc);
    assert!(anon.tracks[&1].refused);
    assert!(shared.keys().is_empty(), "no id, nothing remembered");
}

#[test]
fn encrypted_continuation_of_a_split_init_is_refused() {
    let mut s = session("drmsplit");
    write(&mut s, &seg(1, true, None, None), b"\0\0\0\x08ftyp");
    write(&mut s, &seg(1, false, None, None), b"..encv..");
    write(&mut s, &seg(1, false, Some(0), None), b"CIPHER");
    assert_eq!(
        index_lines(&s.dir),
        0,
        "a split init is decided whole: nothing written"
    );
}

#[test]
fn piff_sample_encryption_is_refused() {
    let mut uuid = PIFF_SAMPLE_ENCRYPTION.to_vec();
    uuid.extend_from_slice(b"rest");
    let piff = mp4_box(*b"moof", &mp4_box(*b"traf", &mp4_box(*b"uuid", &uuid)));
    assert!(payload_encrypted(
        &seg(1, false, Some(0), None),
        &piff,
        false,
        Container::Mp4
    ));
}

#[test]
fn session_budget_counts_index_lines_and_stops_for_good() {
    let out = temp_dir("cap");
    let mut s = Session::open(&out, "test", "u", "t", &budget(9000, 64 * GIB), &refusals())
        .expect("session")
        .expect("within budget");
    write(&mut s, &seg(1, true, None, None), b"INIT");
    write(&mut s, &seg(1, false, Some(0), None), b"");
    write(&mut s, &seg(1, false, Some(1), None), b"X");
    assert_eq!(index_lines(&s.dir), 2, "an empty payload is not written");
    for t in 1..20 {
        write(&mut s, &seg(1, false, Some(t), None), b"X");
    }
    let lines = index_lines(&s.dir);
    assert!(
        lines < 20,
        "index lines count against the cap ({lines} written)"
    );
    assert!(s.stopped);
    let before = index_lines(&s.dir);
    write(&mut s, &seg(1, false, Some(999), None), b"Y");
    assert_eq!(index_lines(&s.dir), before, "stopped stays stopped");
}

#[test]
fn process_budget_is_shared_by_sessions() {
    // Two directories and two part files (about 4 KiB each) fit, then a
    // few rows.
    let shared = budget(16 * GIB, 20_000);
    let open = |name: &str| {
        let dir = temp_dir(name);
        let s = Session::open(&dir, name, "u", "t", &shared, &refusals())
            .expect("session")
            .expect("within budget");
        TestSession { s, _dir: dir }
    };
    let mut a = open("tot-a");
    let mut b = open("tot-b");
    write(&mut a, &seg(1, true, None, None), b"INIT");
    write(&mut b, &seg(1, true, None, None), b"INIT");
    for t in 0..10 {
        write(&mut a, &seg(1, false, Some(t), None), b"X");
        write(&mut b, &seg(1, false, Some(t), None), b"X");
    }
    assert!(shared.total_used.load(Ordering::Relaxed) <= 20_000);
    assert!(a.stopped || b.stopped);
    // A spent process budget opens no further directory.
    let out = temp_dir("tot-c");
    let c = Session::open(&out, "c", "u", "t", &shared, &refusals()).expect("no io error");
    assert!(c.is_none());
    assert_eq!(
        fs::read_dir(&out).expect("dir").count(),
        0,
        "nothing created"
    );
}

#[test]
fn combined_init_and_media_appends_switch_back_into_their_part() {
    // video.js VHS: the init is appended together with a segment.
    let mut s = session("vhs");
    let init_a = init_with_traks(1);
    let mut init_b = init_with_traks(1);
    init_b.extend(mp4_box(*b"free", b"B"));
    let with = |init: &[u8], t: u32| {
        let mut v = init.to_vec();
        v.extend(moof(t));
        v
    };
    write(&mut s, &seg(1, true, Some(0), None), &with(&init_a, 0));
    write(&mut s, &seg(1, false, Some(1000), None), &moof(1000));
    write(
        &mut s,
        &seg(1, true, Some(2000), None),
        &with(&init_b, 2000),
    );
    write(
        &mut s,
        &seg(1, true, Some(3000), None),
        &with(&init_a, 3000),
    );
    write(&mut s, &seg(1, false, Some(4000), None), &moof(4000));
    s.finish();
    let p0 = fs::read(s.dir.join("sb1.p0.bin")).expect("p0");
    assert_eq!(
        p0.len(),
        init_a.len() + 4 * 32,
        "A init once, A media 0, 1, 3, 4"
    );
    assert!(
        !s.dir.join("sb1.p2.bin").exists(),
        "the switch back made no third part"
    );
}

#[test]
fn split_inits_are_compared_whole() {
    let mut s = session("splitdiff");
    // Same first chunk, different rest: two renditions, two parts.
    write(&mut s, &seg(1, true, None, None), b"HEAD");
    write(&mut s, &seg(1, false, None, None), b"-A");
    write(&mut s, &seg(1, false, Some(0), None), b"a0");
    write(&mut s, &seg(1, true, None, None), b"HEAD");
    write(&mut s, &seg(1, false, None, None), b"-B");
    write(&mut s, &seg(1, false, Some(1000), None), b"b1");
    assert_eq!(fs::read(s.dir.join("sb1.p0.bin")).expect("p0"), b"HEAD-Aa0");
    assert_eq!(fs::read(s.dir.join("sb1.p1.bin")).expect("p1"), b"HEAD-Bb1");
}

#[test]
fn sequence_mode_keeps_a_new_period_with_reused_tfdt() {
    let mut s = session("seqperiod");
    write(&mut s, &seg(1, true, None, None), b"INIT");
    let mut h = seg(1, false, Some(0), None);
    h.seq_mode = true;
    write(&mut s, &h, b"period-1");
    write(&mut s, &h, b"period-2"); // discontinuity: tfdt restarts
    write(&mut s, &h, b"period-1"); // seek back: same bytes
    assert_eq!(
        fs::read(s.dir.join("sb1.p0.bin")).expect("part"),
        b"INITperiod-1period-2",
        "init, both periods; the re-append not stored again"
    );
    let rows = index_rows(&s.dir);
    assert_eq!(rows.len(), 4, "plus the re-append's mark: {rows:?}");
    assert_eq!(rows[3]["mark"], true);
    assert_eq!(rows[3]["offset"], rows[1]["offset"]);
}

/// Segments mode places by tfdt and offset: a duplicate needs no mark.
#[test]
fn segments_mode_duplicate_writes_no_mark() {
    let mut s = session("segdupe");
    write(&mut s, &seg(1, true, None, None), b"INIT");
    write(&mut s, &seg(1, false, Some(0), None), b"A");
    write(&mut s, &seg(1, false, Some(0), None), b"A");
    assert_eq!(index_lines(&s.dir), 2);
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

/// A chosen part that mux then leaves out (here: no complete moov) does
/// not move the others either.
#[test]
fn a_skipped_part_does_not_lift_the_others() {
    let dir = temp_dir("liftskipped");
    let init = init_with_traks(1);
    let mut good = init.clone();
    good.extend(moof(2000));
    fs::write(dir.join("sb1.p0.bin"), &good).expect("write");
    let broken = init_with_traks(0);
    let mut bad = broken.clone();
    bad.extend(moof(0));
    fs::write(dir.join("sb2.p0.bin"), &bad).expect("write");
    let mut rows = vec![
        row(0, init.len() as u64, true, None, None),
        row(init.len() as u64, 32, false, Some(2000), None),
        row(0, broken.len() as u64, true, None, None),
        row(broken.len() as u64, 32, false, Some(0), Some(-5.0)),
    ];
    rows[2].sb = 2;
    rows[3].sb = 2;
    let plan = mux_inputs(&dir, &rows, &tools_without(Tool::Ffprobe)).expect("plan");
    assert_eq!(plan.inputs.len(), 1, "sb2 left out: {plan:?}");
    let bytes = fs::read(&plan.inputs[0].path).expect("read");
    assert_eq!(tfdt_of(&bytes[init.len()..]), 2000, "sb1 not moved");
}

/// A mark repeats bytes stored once: a looping teaser does not outweigh
/// the main video.
#[test]
fn marks_do_not_count_for_the_media_source_pick() {
    let mut rows = vec![row(0, 100, false, Some(0), None)];
    let mut teaser = row(0, 10, false, Some(0), None);
    teaser.ms = Some(2);
    teaser.sb = 3;
    for _ in 0..20 {
        let mut m = row(0, 10, false, Some(0), None);
        m.ms = Some(2);
        m.sb = 3;
        m.mark = true;
        rows.push(m);
    }
    rows.push(teaser);
    keep_main_media_source(&mut rows);
    assert!(rows.iter().all(|r| r.ms == Some(1)), "the video is kept");
}

/// An init+media append that ends after its styp: the styp goes with the
/// next append (no row without samples), and so does the set offset.
#[test]
fn a_lone_styp_goes_with_the_next_append() {
    let mut s = session("stypnext");
    let init = init_with_traks(1);
    let styp = mp4_box(*b"styp", b"msdh");
    let mut first = init.clone();
    first.extend_from_slice(&styp);
    let mut h = seg(1, true, None, Some(4.0));
    h.seq_mode = true;
    h.tso_set = true;
    write(&mut s, &h, &first);
    let mut m = seg(1, false, Some(0), Some(4.0));
    m.seq_mode = true;
    m.seq = h.seq + 1;
    write(&mut s, &m, &moof(0));
    let rows = index_rows(&s.dir);
    assert_eq!(rows.len(), 2, "init and one media row: {rows:?}");
    assert_eq!(rows[1]["tso_set"], true, "the media takes the set offset");
    let mut want = init;
    want.extend_from_slice(&styp);
    want.extend(moof(0));
    assert_eq!(fs::read(s.dir.join("sb1.p0.bin")).expect("part"), want);
}

/// A styp appended on its own (no samples, no tfdt) is held the same way:
/// no row that would end the sequence-mode group.
#[test]
fn a_styp_on_its_own_goes_with_the_next_append() {
    let mut s = session("stypalone");
    let init = init_with_traks(1);
    write(&mut s, &seg(1, true, None, None), &init);
    let styp = mp4_box(*b"styp", b"msdh");
    let mut h = seg(1, false, None, Some(4.0));
    h.seq = 2;
    h.seq_mode = true;
    write(&mut s, &h, &styp);
    let mut m = seg(1, false, Some(0), Some(4.0));
    m.seq = 3;
    m.seq_mode = true;
    write(&mut s, &m, &moof(0));
    let rows = index_rows(&s.dir);
    assert_eq!(rows.len(), 2, "init and one media row: {rows:?}");
    assert_eq!(rows[1]["len"], styp.len() + moof(0).len());
}

/// The head goes only to the append right after it: when that one did
/// not arrive, a later one neither gets its bytes nor its set offset.
#[test]
fn a_held_head_is_dropped_when_the_next_append_is_missing() {
    let mut s = session("stypgap");
    let mut first = init_with_traks(1);
    first.extend(mp4_box(*b"styp", b"msdh"));
    let mut h = seg(1, true, None, Some(4.0));
    h.seq_mode = true;
    h.tso_set = true;
    write(&mut s, &h, &first);
    let mut m = seg(1, false, Some(1000), Some(-0.08));
    m.seq_mode = true;
    m.seq = h.seq + 2;
    write(&mut s, &m, &moof(1000));
    let rows = index_rows(&s.dir);
    assert_eq!(rows[1]["tso_set"], false, "{rows:?}");
    assert_eq!(rows[1]["len"], moof(1000).len());
}

/// A row stays within one message: next to samples that fill it, the
/// held head's bytes are left out (its set offset still counts).
#[test]
fn a_held_head_never_makes_a_row_too_large() {
    let mut s = session("stypbig");
    let mut first = init_with_traks(1);
    first.extend(mp4_box(*b"styp", b"msdh"));
    let mut h = seg(1, true, None, None);
    h.tso_set = true;
    write(&mut s, &h, &first);
    let mut m = seg(1, false, Some(0), None);
    m.seq = h.seq + 1;
    let big = vec![0u8; MAX_MESSAGE];
    let (hj, joined) = s.join_segment_head(&m, &big).expect("joined");
    assert_eq!(joined.len(), MAX_MESSAGE);
    assert!(hj.tso_set);
}

/// A part where no media row has a tfdt (every append cut inside its
/// moof) moves no bytes, so it does not carry the lift.
#[test]
fn an_all_untimed_part_does_not_carry_the_lift() {
    let dir = temp_dir("liftallcut");
    let init = init_with_traks(1);
    let rows = write_part(
        &dir,
        &[
            (&init, true, None, None),
            (b"piece", false, None, None),
            (b"other", false, None, None),
        ],
    );
    let refs: Vec<&IndexRow> = rows.iter().collect();
    let lifted = ordered_part(&dir, 1, 0, &refs, Some(&SeqMoves::new()), 1_000_000_000)
        .expect("ordered")
        .3;
    assert!(!lifted, "nothing moved");
}

/// The lift is in the bytes when the first segment with a tfdt moved; a
/// piece without one in front of it (nothing to move) does not count.
#[test]
fn an_untimed_first_piece_does_not_hide_the_lift() {
    let dir = temp_dir("liftuntimed");
    let init = init_with_traks(1);
    let rows = write_part(
        &dir,
        &[
            (&init, true, None, None),
            (b"piece", false, None, None),
            (&moof(0), false, Some(0), None),
        ],
    );
    let refs: Vec<&IndexRow> = rows.iter().collect();
    let lifted = ordered_part(&dir, 1, 0, &refs, Some(&SeqMoves::new()), 1_000_000_000)
        .expect("ordered")
        .3;
    assert!(lifted, "the moof moved");
}

/// A part whose bytes cannot take the lift (no tfdt to rewrite: `WebM`)
/// says so, so mux aligns its probed start as if they had.
#[test]
fn ordered_part_says_whether_the_bytes_carry_the_lift() {
    let lifted = |name: &str, media: &[u8]| {
        let dir = temp_dir(name);
        let init = init_with_traks(1);
        let rows = write_part(
            &dir,
            &[(&init, true, None, None), (media, false, Some(0), None)],
        );
        let refs: Vec<&IndexRow> = rows.iter().collect();
        ordered_part(&dir, 1, 0, &refs, Some(&SeqMoves::new()), 1_000_000_000)
            .expect("ordered")
            .3
    };
    assert!(lifted("liftmp4", &moof(0)), "an MP4 segment is moved");
    assert!(
        !lifted("liftwebm", b"\x1f\x43\xb6\x75cluster"),
        "nothing to move"
    );
}

/// A moof of the given sequence number, cut before its tfdt: what a chunked fetch can deliver
/// as an append of its own.
fn cut_moof_head(seq: u32) -> Vec<u8> {
    let mut mfhd = vec![0, 0, 0, 0];
    mfhd.extend(seq.to_be_bytes());
    let mut head = 100u32.to_be_bytes().to_vec(); // the moof goes on in the next chunk
    head.extend(b"moof");
    head.extend(mp4_box(*b"mfhd", &mfhd));
    head
}

#[test]
fn a_refetched_segment_cut_before_its_tfdt_is_dropped_whole() {
    let mut s = session("cuthead");
    write(&mut s, &seg(1, true, None, None), b"INIT");
    let first = cut_moof_head(1);
    let second = cut_moof_head(2);
    write(&mut s, &seg(1, false, None, None), &first);
    write(&mut s, &seg(1, false, None, None), b"TAIL");
    // Seek back: the player fetches the first segment again, then goes on.
    write(&mut s, &seg(1, false, None, None), &first);
    write(&mut s, &seg(1, false, None, None), b"TAIL");
    write(&mut s, &seg(1, false, None, None), &second);
    write(&mut s, &seg(1, false, None, None), b"TAIL");
    let mut want = b"INIT".to_vec();
    want.extend(&first);
    want.extend(b"TAIL");
    want.extend(&second);
    want.extend(b"TAIL");
    assert_eq!(fs::read(s.dir.join("sb1.p0.bin")).expect("part"), want);
    assert_eq!(
        index_lines(&s.dir),
        5,
        "init, then head and tail of two segments"
    );
}

#[test]
fn equal_untimed_chunks_that_start_no_moof_are_all_kept() {
    // Silence and repeated WebM blocks hold equal bytes at different times: only a moof
    // head has a sequence number that says it is the same segment.
    let mut s = session("equaltails");
    write(&mut s, &seg(1, true, None, None), b"INIT");
    write(&mut s, &seg(1, false, Some(0), None), b"HEAD");
    write(&mut s, &seg(1, false, None, None), b"SAME");
    write(&mut s, &seg(1, false, None, None), b"SAME");
    assert_eq!(
        fs::read(s.dir.join("sb1.p0.bin")).expect("part"),
        b"INITHEADSAMESAME"
    );
}

#[test]
fn the_lead_is_found_when_the_append_was_cut_inside_the_moof() {
    // tfdt in the first chunk, the trun with the composition offset in the second.
    let mut trun = vec![0, 0, 0x09, 0x00];
    trun.extend(1u32.to_be_bytes());
    trun.extend(40u32.to_be_bytes());
    trun.extend(80u32.to_be_bytes());
    let segment = mp4_box(*b"moof", &mp4_box(*b"traf", &mp4_box(*b"trun", &trun)));
    let (head, tail) = segment.split_at(segment.len() - 6);
    let dir = temp_dir("cutlead");
    let init = init_with_traks(1);
    let mut rows = write_part(
        &dir,
        &[
            (&init, true, None, None),
            (head, false, Some(1000), Some(4.0)),
            (tail, false, None, Some(4.0)),
        ],
    );
    rows[1].seq_mode = true;
    rows[1].tso_set = true;
    rows[2].seq_mode = true;
    let refs: Vec<&IndexRow> = rows.iter().collect();
    let leads = seq_leads(&dir, 1, &refs);
    assert_eq!(leads[&(0, rows[1].offset)], 80, "read across the cut");
}

#[test]
fn first_cto_reads_version_0_offsets_signed() {
    let mut trun = vec![0, 0, 0x08, 0x00]; // version 0, cto present
    trun.extend_from_slice(&1u32.to_be_bytes());
    trun.extend_from_slice(&(-1024i32).to_be_bytes());
    let seg = mp4_box(*b"moof", &mp4_box(*b"traf", &mp4_box(*b"trun", &trun)));
    assert_eq!(first_cto(&seg), -1024);
}

#[test]
fn elst_media_time_skips_an_empty_first_edit() {
    let mut elst = vec![0, 0, 0, 0];
    elst.extend_from_slice(&2u32.to_be_bytes());
    for (dur, media_time) in [(100u32, -1i32), (500, 1024)] {
        elst.extend_from_slice(&dur.to_be_bytes());
        elst.extend_from_slice(&media_time.to_be_bytes());
        elst.extend_from_slice(&0x0001_0000u32.to_be_bytes());
    }
    let edts = mp4_box(*b"edts", &mp4_box(*b"elst", &elst));
    let mut init = mp4_box(*b"ftyp", b"iso6");
    init.extend(mp4_box(*b"moov", &mp4_box(*b"trak", &edts)));
    assert_eq!(elst_media_time(&init), 1024);
}

/// A combined append cut right after the per-segment styp (or emsg) is
/// split there; an init+index range (ftyp moov sidx) stays init.
#[test]
fn init_prefix_len_splits_at_a_lone_styp() {
    let init = init_with_traks(1);
    for kind in [*b"styp", *b"emsg", *b"prft"] {
        let mut p = init.clone();
        p.extend(mp4_box(kind, b"msdh"));
        assert_eq!(
            init_prefix_len(Container::Mp4, &p),
            Some(init.len()),
            "{kind:?}"
        );
    }
    let mut p = init.clone();
    p.extend(mp4_box(*b"sidx", b"index"));
    assert_eq!(
        init_prefix_len(Container::Mp4, &p),
        None,
        "single-file index"
    );
}

#[test]
fn media_bytes_never_count_as_init_markers() {
    let mut s = session("nofalsedrm");
    write(&mut s, &seg(1, true, None, None), &init_with_traks(1));
    write(&mut s, &seg(1, false, Some(0), None), &moof(0));
    // An untimed media continuation that happens to hold "encv".
    write(&mut s, &seg(1, false, None, None), b"xxxxencvxxxx");
    write(&mut s, &seg(1, false, Some(1000), None), &moof(1000));
    assert_eq!(index_lines(&s.dir), 4, "nothing refused");
    assert!(!s.tracks[&1].refused);
}

#[test]
fn untimed_media_of_a_combined_append_is_not_init_bytes() {
    // Init and a segment in one append, the moof without a tfdt: the
    // media part is not a continuation of the init, so its mdat bytes
    // are not scanned for init markers and not hashed into the init.
    let mut s = session("vhsnotfdt");
    let mut append = init_with_traks(1);
    append.extend(mp4_box(*b"moof", &mp4_box(*b"traf", b"")));
    append.extend(mp4_box(*b"mdat", b"xxxxencvxxxx"));
    write(&mut s, &seg(1, true, None, None), &append);
    write(&mut s, &seg(1, false, Some(1000), None), &moof(1000));
    assert!(!s.tracks[&1].refused, "no false DRM from mdat bytes");
    let rows: Vec<serde_json::Value> = fs::read_to_string(s.dir.join("index.jsonl"))
        .expect("index")
        .lines()
        .map(|l| serde_json::from_str(l).expect("json"))
        .collect();
    let inits: Vec<bool> = rows.iter().map(|r| r["is_init"] == true).collect();
    assert_eq!(
        inits,
        [true, false, false],
        "init row, media row, media row"
    );
}

#[test]
fn init_prefix_is_found_in_mp4_and_webm() {
    let init = init_with_traks(1);
    let mut combined = init.clone();
    combined.extend(moof(0));
    assert_eq!(init_prefix_len(Container::Mp4, &combined), Some(init.len()));
    assert_eq!(init_prefix_len(Container::Mp4, &init), None);
    // EBML header, Segment (unknown size) > Info, Cluster.
    let webm = [
        0x1a, 0x45, 0xdf, 0xa3, 0x81, 0x00, 0x18, 0x53, 0x80, 0x67, 0x01, 0xff, 0xff, 0xff, 0xff,
        0xff, 0xff, 0xff, 0x15, 0x49, 0xa9, 0x66, 0x80, 0x1f, 0x43, 0xb6, 0x75, 0x80,
    ];
    assert_eq!(init_prefix_len(Container::Webm, &webm), Some(23));
}

#[test]
fn multi_track_segments_shift_each_traf_by_its_timescale() {
    // tkhd v0: fullbox(4) ctime(4) mtime(4) track_ID(4); mdhd v0: fullbox, ctime, mtime, timescale.
    let trak = |id: u32, ts: u32| {
        let mut tkhd = vec![0u8; 16];
        tkhd[12..16].copy_from_slice(&id.to_be_bytes());
        let mut mdhd = vec![0u8; 16];
        mdhd[12..16].copy_from_slice(&ts.to_be_bytes());
        mp4_box(
            *b"trak",
            &[
                mp4_box(*b"tkhd", &tkhd),
                mp4_box(*b"mdia", &mp4_box(*b"mdhd", &mdhd)),
            ]
            .concat(),
        )
    };
    let mut init = mp4_box(*b"ftyp", b"iso6");
    init.extend(mp4_box(
        *b"moov",
        &[trak(1, 90_000), trak(2, 48_000)].concat(),
    ));
    let ts = trak_timescales(&init);
    assert_eq!(ts.get(&1), Some(&90_000));
    assert_eq!(ts.get(&2), Some(&48_000));
    let frag = |id: u32| {
        let mut tfhd = vec![0u8; 8];
        tfhd[4..8].copy_from_slice(&id.to_be_bytes());
        mp4_box(
            *b"traf",
            &[mp4_box(*b"tfhd", &tfhd), mp4_box(*b"tfdt", &[0u8; 8])].concat(),
        )
    };
    let mut seg = mp4_box(*b"moof", &[frag(1), frag(2)].concat());
    let by_track =
        |tid: Option<u32>| shift_ticks(Some(10.0), tid.and_then(|t| ts.get(&t)).copied());
    assert!(shift_tfdt_by(&mut seg, &by_track));
    let tfdt_at = |n: usize| {
        let at = seg
            .windows(4)
            .enumerate()
            .filter(|(_, w)| w == b"tfdt")
            .nth(n)
            .expect("tfdt")
            .0
            + 8;
        u32::from_be_bytes([seg[at], seg[at + 1], seg[at + 2], seg[at + 3]])
    };
    assert_eq!(tfdt_at(0), 900_000, "video: 10 s at 90 kHz");
    assert_eq!(tfdt_at(1), 480_000, "audio: 10 s at 48 kHz");
}

/// The largest part of a `SourceBuffer` has a cut init: the smaller one
/// that decodes is muxed instead of none.
#[test]
fn a_smaller_part_is_used_when_the_largest_cannot_be_muxed() {
    let dir = temp_dir("fallback");
    let init = init_with_traks(1);
    let mut good = init.clone();
    good.extend(moof(0));
    fs::write(dir.join("sb1.p0.bin"), &good).expect("write");
    let cut = init_with_traks(0);
    let mut big = cut.clone();
    big.extend(vec![0u8; 4096]);
    fs::write(dir.join("sb1.p1.bin"), &big).expect("write");
    let mut rows = vec![
        row(0, init.len() as u64, true, None, None),
        row(init.len() as u64, 32, false, Some(0), None),
        row(0, cut.len() as u64, true, None, None),
        row(cut.len() as u64, 4096, false, Some(1000), None),
    ];
    rows[2].part = 1;
    rows[3].part = 1;
    let plan = mux_inputs(&dir, &rows, &tools_without(Tool::Ffprobe)).expect("plan");
    assert_eq!(plan.inputs.len(), 1, "{plan:?}");
    assert!(
        plan.inputs[0].path.ends_with("sb1.p0.bin"),
        "the smaller part: {plan:?}"
    );
}

/// Writes `init` and `media` as sb1.p`part`.bin and returns its rows.
/// Media is (bytes, len, tfdt); bytes `None` is a row past the end of the
/// file (a truncated part).
fn write_part_rows(
    dir: &Path,
    part: u32,
    init: &[u8],
    media: &[(Option<&[u8]>, u64, u64)],
) -> Vec<IndexRow> {
    let mut raw = init.to_vec();
    let mut rows = vec![row(0, init.len() as u64, true, None, None)];
    for &(bytes, len, tfdt) in media {
        let offset = match bytes {
            Some(b) => {
                raw.extend_from_slice(b);
                raw.len() - b.len()
            }
            None => raw.len() + (1 << 20),
        };
        rows.push(row(offset as u64, len, false, Some(tfdt), None));
    }
    fs::write(dir.join(format!("sb1.p{part}.bin")), raw).expect("write");
    for r in &mut rows {
        r.part = part;
    }
    rows
}

/// A fallback part that cannot be read (an index row past the end of its
/// file) is left out like one that does not decode: the next one is
/// tried, the mux goes on, and no partial sorted copy stays behind.
#[test]
fn an_unreadable_fallback_part_is_skipped() {
    let dir = temp_dir("fallbackio");
    let (good, cut) = (init_with_traks(1), init_with_traks(0));
    let m1 = moof(1000);
    let mut rows = write_part_rows(&dir, 1, &cut, &[(Some(&[0u8; 64]), 64, 0)]);
    rows[1].len = 20_000;
    rows.extend(write_part_rows(
        &dir,
        0,
        &good,
        &[(Some(&m1), m1.len() as u64, 1000), (None, 10_000, 0)],
    ));
    rows.extend(write_part_rows(&dir, 2, &good, &[(Some(&moof(0)), 32, 0)]));
    let plan = mux_inputs(&dir, &rows, &tools_without(Tool::Ffprobe)).expect("plan");
    assert_eq!(plan.inputs.len(), 1, "{plan:?}");
    assert!(plan.inputs[0].path.ends_with("sb1.p2.bin"), "{plan:?}");
    assert!(!dir.join("sb1.p0.sorted.bin").exists(), "partial copy left");
}

/// A fallback that only fails once the others are laid out again (a
/// truncated row the raw file served unread at the first lift) is left
/// out there too, and the lift then follows the parts actually muxed.
#[test]
fn a_fallback_failing_in_the_relayout_is_skipped_and_moves_nothing() {
    let dir = temp_dir("relayoutio");
    let cut = init_with_traks(0);
    let largest = |dir: &Path, tso: Option<f64>| {
        let mut r = write_part_rows(dir, 1, &cut, &[(Some(&[0u8; 64]), 64, 0)]);
        r[1].len = 20_000;
        r[1].tso = tso;
        r
    };
    // sb2: the largest part has a cut init; the fallback needs no move
    // at the first lift (10 s against its -10 s offset), so its raw file
    // serves and its truncated row goes unread, but it sets a lift of
    // 8 s for the second layout.
    let m2 = moof(2000);
    let mut rows = largest(&dir, None);
    rows.extend(write_part_rows(
        &dir,
        0,
        &init_with_traks(1),
        &[(Some(&m2), m2.len() as u64, 2000), (None, 10_000, 3000)],
    ));
    for r in &mut rows {
        r.sb = 2;
        if r.part == 0 && !r.is_init {
            r.tso = Some(-10.0);
        }
    }
    for part in 0..2 {
        fs::rename(
            dir.join(format!("sb1.p{part}.bin")),
            dir.join(format!("sb2.p{part}.bin")),
        )
        .expect("rename");
    }
    // sb1: the largest part (cut init) sets the first lift of 10 s.
    rows.extend(largest(&dir, Some(-10.0)));
    rows.extend(write_part_rows(
        &dir,
        0,
        &init_with_traks(1),
        &[(Some(&moof(0)), 32, 0)],
    ));
    let plan = mux_inputs(&dir, &rows, &tools_without(Tool::Ffprobe)).expect("plan");
    assert_eq!(plan.inputs.len(), 1, "sb2 left out: {plan:?}");
    assert!(
        plan.inputs[0].path.ends_with("sb1.p0.bin"),
        "sb1 not moved by the parts left out: {plan:?}"
    );
}

/// The fallback that fails in the relayout does not take its `SourceBuffer`
/// down: the next candidate (small, clean) is tried with the new lift.
#[test]
fn a_relayout_failure_tries_the_next_candidate_of_the_sourcebuffer() {
    let dir = temp_dir("relayoutnext");
    let cut = init_with_traks(0);
    let largest = |dir: &Path, tso: Option<f64>| {
        let mut r = write_part_rows(dir, 1, &cut, &[(Some(&[0u8; 64]), 64, 0)]);
        r[1].len = 20_000;
        r[1].tso = tso;
        r
    };
    let m2 = moof(2000);
    let mut rows = largest(&dir, None);
    rows.extend(write_part_rows(
        &dir,
        0,
        &init_with_traks(1),
        &[(Some(&m2), m2.len() as u64, 2000), (None, 10_000, 3000)],
    ));
    // A small, clean part of the same SourceBuffer, ranked last.
    rows.extend(write_part_rows(
        &dir,
        2,
        &init_with_traks(1),
        &[(Some(&moof(0)), 32, 0)],
    ));
    for r in &mut rows {
        r.sb = 2;
        if r.part == 0 && !r.is_init {
            r.tso = Some(-10.0);
        }
    }
    for part in 0..3 {
        fs::rename(
            dir.join(format!("sb1.p{part}.bin")),
            dir.join(format!("sb2.p{part}.bin")),
        )
        .expect("rename");
    }
    rows.extend(largest(&dir, Some(-10.0)));
    rows.extend(write_part_rows(
        &dir,
        0,
        &init_with_traks(1),
        &[(Some(&moof(0)), 32, 0)],
    ));
    let plan = mux_inputs(&dir, &rows, &tools_without(Tool::Ffprobe)).expect("plan");
    let names: Vec<_> = plan
        .inputs
        .iter()
        .map(|i| {
            i.path
                .file_name()
                .expect("name")
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    assert!(names.contains(&"sb1.p0.bin".to_string()), "{names:?}");
    assert!(
        names.iter().any(|n| n.starts_with("sb2.p2")),
        "sb2 muxed through its small part: {names:?}"
    );
}

/// The cheap checks on the init come before the sort: a part with a cut
/// init is skipped without reading (or copying) its media, so a
/// truncated media row does not fail the mux either.
#[test]
fn a_cut_init_is_rejected_before_the_sort() {
    let dir = temp_dir("cheapfirst");
    let m1 = moof(1000);
    let rows = write_part_rows(
        &dir,
        0,
        &init_with_traks(0),
        &[(Some(&m1), m1.len() as u64, 1000), (None, 10_000, 0)],
    );
    assert!(
        matches!(
            mux_inputs(&dir, &rows, &tools_without(Tool::Ffprobe)),
            Err(SinkError::Capture(_))
        ),
        "skipped, not an I/O error"
    );
    assert!(!dir.join("sb1.p0.sorted.bin").exists());
}

/// At most `MAX_FALLBACK_PARTS` parts are tried after the largest one.
#[test]
fn fallback_parts_are_capped() {
    let dir = temp_dir("fallbackcap");
    let cut = init_with_traks(0);
    let mut rows = write_part_rows(&dir, 0, &init_with_traks(1), &[(Some(&moof(0)), 32, 0)]);
    let last = u32::try_from(MAX_FALLBACK_PARTS).expect("small") + 1;
    for part in 1..=last {
        let mut r = write_part_rows(&dir, part, &cut, &[(Some(&[0u8; 64]), 64, 0)]);
        // Larger than part 0, the lower part number the larger.
        r[1].len = 10_000 - u64::from(part);
        rows.extend(r);
    }
    assert!(
        matches!(
            mux_inputs(&dir, &rows, &tools_without(Tool::Ffprobe)),
            Err(SinkError::Capture(_))
        ),
        "part 0 is beyond the cap"
    );
    rows.retain(|r| r.part != last);
    let plan = mux_inputs(&dir, &rows, &tools_without(Tool::Ffprobe)).expect("plan");
    assert!(plan.inputs[0].path.ends_with("sb1.p0.bin"), "{plan:?}");
}

/// A part rejected after its sort (it does not start like media) leaves
/// no sorted copy behind.
#[test]
fn a_rejected_part_leaves_no_sorted_copy() {
    let dir = temp_dir("rejectsorted");
    // A WebM "init" too short to check before the sort.
    let mut rows = write_part_rows(
        &dir,
        0,
        b"XYZ",
        &[(Some(b"CLUSTER1"), 8, 1000), (Some(b"CLUSTER0"), 8, 0)],
    );
    for r in &mut rows {
        r.kind = "webm".into();
        r.mime = "video/webm".into();
    }
    assert!(matches!(
        mux_inputs(&dir, &rows, &tools_without(Tool::Ffprobe)),
        Err(SinkError::Capture(_))
    ));
    assert!(!dir.join("sb1.p0.sorted.bin").exists());
}

#[test]
fn a_part_with_a_cut_init_is_left_out() {
    let dir = temp_dir("cutinit");
    let full = init_with_traks(1);
    let cut = &full[..full.len() - 4];
    fs::write(dir.join("sb1.p0.bin"), cut).expect("write");
    let r = row(0, cut.len() as u64, true, None, None);
    assert!(
        matches!(
            mux_inputs(&dir, &[r], &Tools::default()),
            Err(SinkError::Capture(_))
        ),
        "no complete moov: skipped"
    );
}

fn tools_without(tool: Tool) -> Tools {
    let mut tools = Tools::default();
    let missing = std::ffi::OsString::from("streamtap-no-such-tool-4711");
    match tool {
        Tool::Ffmpeg => tools.ffmpeg = missing,
        Tool::Ffprobe => tools.ffprobe = missing,
    }
    tools
}

/// ENG-13: a missing ffprobe is said once, the parts are kept with the
/// capture's own start instead of silently counting as "start unknown".
#[test]
fn a_missing_ffprobe_is_reported_not_swallowed() {
    let warnings = plan_two_parts_with("noprobe", &tools_without(Tool::Ffprobe));
    assert!(warnings[0].starts_with("ffprobe not found"), "{warnings:?}");
}

/// ENG-13 also for an ffprobe that exists but cannot be started: one
/// warning, not one (and one spawn attempt) per part.
#[test]
fn an_unstartable_ffprobe_is_reported_once() {
    let dir = temp_dir("badprobe");
    // A directory, not a text file named .exe: starting a non-PE .exe makes
    // Windows pop up the "16-bit application" dialog on every test run,
    // while a directory fails the spawn with "access denied" silently.
    let bad = dir.join("ffprobe-not-a-program.exe");
    fs::create_dir(&bad).expect("create dir");
    let tools = Tools {
        ffprobe: bad.into(),
        ..Tools::default()
    };
    let warnings = plan_two_parts_with("badprobe-parts", &tools);
    assert!(
        warnings[0].starts_with("cannot run ffprobe"),
        "{warnings:?}"
    );
}

/// Plans two MP4 parts with `tools` (ffprobe unusable); expects both kept
/// with the capture's start and exactly one warning, which it returns.
fn plan_two_parts_with(name: &str, tools: &Tools) -> Vec<String> {
    let dir = temp_dir(name);
    let init = init_with_traks(1);
    let mut seg = init.clone();
    seg.extend(moof(2000));
    let mut rows = Vec::new();
    for sb in [1, 2] {
        fs::write(dir.join(format!("sb{sb}.p0.bin")), &seg).expect("write");
        let mut i = row(0, init.len() as u64, true, None, None);
        let mut m = row(
            init.len() as u64,
            (seg.len() - init.len()) as u64,
            false,
            Some(2000),
            None,
        );
        i.sb = sb;
        m.sb = sb;
        rows.push(i);
        rows.push(m);
    }
    let plan = mux_inputs(&dir, &rows, tools).expect("plan");
    assert_eq!(plan.inputs.len(), 2, "parts are kept: {plan:?}");
    assert!(
        plan.inputs.iter().all(|i| i.start == Some(2.0)),
        "the capture's start is used: {plan:?}"
    );
    assert_eq!(plan.warnings.len(), 1, "said once: {:?}", plan.warnings);
    plan.warnings
}

#[test]
fn the_part_file_keeps_the_extension() {
    assert_eq!(
        part_path(Path::new("cap/out.mp4")),
        Path::new("cap/out.part.mp4")
    );
    assert_eq!(part_path(Path::new("cap/out")), Path::new("cap/out.part"));
}

/// A mux killed at its deadline (or the output limit) leaves neither a
/// truncated `out` nor its part file, and an older `out` stays intact.
#[test]
fn a_killed_mux_leaves_no_truncated_output() {
    let dir = temp_dir("killedmux");
    let out = dir.join("out.mp4");
    fs::write(&out, b"older result").expect("write");
    let err = write_through_part(&out, |part| -> Result<(), SinkError> {
        fs::write(part, b"truncated, no moov").expect("write part");
        Err(SinkError::ToolTimeout {
            tool: Tool::Ffmpeg,
            after: Duration::from_secs(1),
        })
    })
    .unwrap_err();
    assert_eq!(err.exit_code(), 5);
    assert_eq!(fs::read(&out).expect("out"), b"older result");
    assert!(!part_path(&out).exists(), "part file removed");
}

#[test]
fn a_finished_mux_replaces_the_output() {
    let dir = temp_dir("finishedmux");
    let out = dir.join("out.mp4");
    fs::write(&out, b"older result").expect("write");
    write_through_part(&out, |part| {
        fs::write(part, b"new result").expect("write part");
        Ok(())
    })
    .expect("mux");
    assert_eq!(fs::read(&out).expect("out"), b"new result");
    assert!(!part_path(&out).exists());
}

#[test]
fn a_missing_ffmpeg_fails_the_mux_with_its_own_code() {
    let dir = temp_dir("noffmpeg");
    fs::write(dir.join("index.jsonl"), "").expect("write");
    let args = [dir.to_string_lossy().into_owned()];
    let err = run_mux(&args, &tools_without(Tool::Ffmpeg)).unwrap_err();
    assert!(
        matches!(err, SinkError::ToolMissing { tool: Tool::Ffmpeg }),
        "{err}"
    );
    assert_eq!(err.exit_code(), 4);
}

#[test]
fn mux_usage_errors_exit_with_2() {
    for args in [
        vec![],
        vec![
            "dir".to_owned(),
            "--timeout-secs".to_owned(),
            "0".to_owned(),
        ],
        vec![
            "dir".to_owned(),
            "--out".to_owned(),
            "--timeout-secs".to_owned(),
            "5".to_owned(),
        ],
    ] {
        let err = run_mux(&args, &Tools::default()).unwrap_err();
        assert_eq!(err.exit_code(), 2, "{err}");
    }
}

/// An empty token would let any local process in; a flag value that is
/// the next flag is a slip. Both stop `serve` before it binds.
#[test]
fn serve_usage_errors_exit_with_2_before_binding() {
    let a = |v: &[&str]| v.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();
    for args in [
        a(&["--token", ""]),
        a(&["--token", " \t"]),
        a(&["--token", "--port", "1"]),
        a(&["--token"]),
        a(&["--token", "t", "--port"]),
        a(&["--token", "t", "--out", "--port", "1"]),
        a(&["--port", "1"]),
        a(&["--token-stdin", "--port", "x"]),
        a(&["--token", "t", "--token-stdin"]),
        a(&["--token-stdin", "--token", "t"]),
        a(&["--token", "t", "--hello-timeout-secs", "0"]),
        a(&["--token", "t", "--idle-timeout-secs", "x"]),
        a(&["--token", "t", "--max-connections", "0"]),
        a(&["--token", "t", "--max-connections", "5000"]),
    ] {
        let err = serve_config(&args, NoStdin).unwrap_err();
        assert_eq!(err.exit_code(), 2, "{args:?}: {err}");
    }
    let ok = serve_config(&a(&["--port", "0", "--token", "secret"]), NoStdin).expect("valid");
    assert_eq!((ok.token.expose(), ok.port), (&b"secret"[..], 0));
    assert_eq!(ok.out, PathBuf::from("captures"));
    assert_eq!(
        ok.limits.hello,
        Duration::from_secs(DEFAULT_HELLO_TIMEOUT_SECS)
    );
    assert_eq!(ok.limits.max_connections, DEFAULT_MAX_CONNECTIONS);
}

/// A stdin that must not be read: `serve` reads it only for
/// `--token-stdin`, and only after every other flag was checked.
struct NoStdin;

impl Read for NoStdin {
    fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
        panic!("stdin was read");
    }
}

fn token_from_stdin(input: &[u8]) -> Result<Secret, SinkError> {
    serve_config(
        &[
            "--token-stdin".to_owned(),
            "--port".to_owned(),
            "0".to_owned(),
        ],
        input,
    )
    .map(|c| c.token)
}

#[test]
fn token_stdin_takes_the_first_line_without_its_line_break() {
    for (input, want) in [
        (&b"secret\n"[..], &b"secret"[..]),
        (b"secret\r\n", b"secret"),
        (b"secret", b"secret"),
        (b"secret\nsecond line\n", b"secret"),
        (b" sp ace \n", b" sp ace "),
        (b"\xef\xbb\xbfsecret\r\n", b"secret"),
        (b"\xef\xbb\xbfsecret", b"secret"),
    ] {
        let token = token_from_stdin(input).expect("valid");
        assert_eq!(token.expose(), want, "{input:?}");
    }
}

#[test]
fn token_stdin_refuses_an_empty_a_blank_and_a_non_utf8_line() {
    for input in [
        &b""[..],
        b"\n",
        b"\r\n",
        b" \t\r\n",
        b"\nsecret\n",
        b"\xff\xfe\n",
        b"\xef\xbb\xbf\r\n",
        b"\xef\xbb\xbf",
    ] {
        let err = token_from_stdin(input).unwrap_err();
        assert_eq!(err.exit_code(), 2, "{input:?}: {err}");
    }
}

#[test]
fn token_stdin_reads_at_most_4_kib() {
    let longest = "t".repeat(MAX_TOKEN_LINE);
    for tail in ["\n", "\r\n", ""] {
        for bom in ["", "\u{feff}"] {
            let token = token_from_stdin(format!("{bom}{longest}{tail}").as_bytes()).expect("fits");
            assert_eq!(token.expose().len(), MAX_TOKEN_LINE, "{bom:?} {tail:?}");
        }
    }
    // A cut line is refused, never shortened into a token (also after
    // its BOM is removed).
    for input in [
        format!("{longest}t\n"),
        format!("{longest}t"),
        "t".repeat(100_000),
        format!("\u{feff}{longest}t\r\n"),
        format!("\u{feff}{}", "t".repeat(100_000)),
    ] {
        let err = token_from_stdin(input.as_bytes()).unwrap_err();
        assert_eq!(err.exit_code(), 2, "{} bytes: {err}", input.len());
    }
}

#[test]
fn a_failing_stdin_is_an_io_error() {
    struct Broken;
    impl Read for Broken {
        fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
            Err(io::Error::other("pipe broken"))
        }
    }
    let args = ["--token-stdin".to_owned()];
    assert_eq!(serve_config(&args, Broken).unwrap_err().exit_code(), 1);
}

#[test]
fn the_token_is_compared_by_content_and_never_printed() {
    let config = token_from_stdin(b"hunter2-secret\n").unwrap();
    assert!(config.matches(b"hunter2-secret"));
    assert!(!config.matches(b"hunter2-secreT"));
    assert!(!config.matches(b"hunter2-secre"));
    assert!(!config.matches(b""));
    let full = serve_config(
        &["--token".to_owned(), "hunter2-secret".to_owned()],
        NoStdin,
    )
    .unwrap();
    let shown = format!("{full:?}");
    assert!(!shown.contains("hunter2"), "{shown}");
    assert!(shown.contains("hidden"), "{shown}");
}

#[test]
fn version_line_drops_the_copyright() {
    assert_eq!(
            version_line("ffmpeg version 7.1-full_build-www.gyan.dev Copyright (c) 2000-2024 the FFmpeg developers
built with gcc"),
            "ffmpeg version 7.1-full_build-www.gyan.dev"
        );
    assert!(version_line("").contains("unknown"));
}

#[test]
fn verbose_asks_ffmpeg_for_warnings() {
    let inputs = [MuxInput {
        path: PathBuf::from("v"),
        format: "mp4",
        start: None,
    }];
    let level = |verbose| -> String {
        let args: Vec<String> = ffmpeg_args(&inputs, Path::new("out.mp4"), verbose)
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        let i = args.iter().position(|a| a == "-loglevel").expect("level");
        args[i + 1].clone()
    };
    assert_eq!(level(false), "error");
    assert_eq!(level(true), "warning");
}

#[test]
fn ffmpeg_runs_quiet_and_maps_every_input() {
    let inputs = [MuxInput {
        path: PathBuf::from("v"),
        format: "mp4",
        start: None,
    }];
    let args: Vec<String> = ffmpeg_args(&inputs, Path::new("out.mp4"), false)
        .iter()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    assert!(args
        .windows(2)
        .any(|w| w[0] == "-loglevel" && w[1] == "error"));
    assert!(args.iter().any(|a| a == "-nostats"));
    assert!(args.windows(2).any(|w| w[0] == "-map" && w[1] == "0"));
    assert_eq!(args.last().map(String::as_str), Some("out.mp4"));
}

#[test]
fn printable_strips_terminal_escapes() {
    assert_eq!(
        printable("video/mp4\x1b]0;pwned\x07", 64),
        "video/mp4?]0;pwned?"
    );
    assert_eq!(printable("abc", 2), "ab");
}

#[test]
fn track_count_is_bounded() {
    let mut s = session("tracks");
    let limit = u32::try_from(MAX_TRACKS).expect("small constant");
    for sb in 0..(limit + 10) {
        write(&mut s, &seg(sb, true, None, None), b"I");
    }
    assert_eq!(s.tracks.len(), MAX_TRACKS);
}

#[test]
fn sessions_never_share_a_directory() {
    let out = temp_dir("dirs");
    let a = Session::open(&out, "same", "u", "t", &budget(GIB, GIB), &refusals())
        .expect("a")
        .expect("within budget");
    let b = Session::open(&out, "same", "u", "t", &budget(GIB, GIB), &refusals())
        .expect("b")
        .expect("within budget");
    assert_ne!(a.dir, b.dir);
    assert!(a.dir.join("session.json").is_file());
}

#[test]
fn session_json_says_open_until_the_session_is_dropped() {
    let out = temp_dir("ended");
    let read = |dir: &Path| -> serde_json::Value {
        serde_json::from_slice(&fs::read(dir.join("session.json")).expect("read")).expect("json")
    };
    let s = Session::open(
        &out,
        "sid",
        "https://x/v",
        "T",
        &budget(GIB, GIB),
        &refusals(),
    )
    .expect("open")
    .expect("within budget");
    let dir = s.dir.clone();
    let open = read(&dir);
    assert_eq!(open["state"], "open");
    assert_eq!(open["sink_pid"], std::process::id());
    assert!(open.get("ended").is_none());
    drop(s);
    let done = read(&dir);
    assert_eq!(done["state"], "ended");
    assert!(done["ended"]
        .as_u64()
        .is_some_and(|t| t >= done["started"].as_u64().unwrap()));
    // Everything else is kept, and no temp file is left behind.
    assert_eq!(done["url"], "https://x/v");
    assert_eq!(done["title"], "T");
    assert!(!dir.join("session.json.tmp").exists());
}

/// A reader that keeps `session.json` open without sharing deletion (a
/// virus scanner does the same for a moment) makes the rename fail.
#[cfg(windows)]
#[test]
fn the_end_is_recorded_although_a_reader_blocks_the_rename() {
    use std::os::windows::fs::OpenOptionsExt;
    let out = temp_dir("endblocked");
    let s = Session::open(
        &out,
        "sid",
        "https://x/v",
        "T",
        &budget(GIB, GIB),
        &refusals(),
    )
    .expect("open")
    .expect("within budget");
    let dir = s.dir.clone();
    // FILE_SHARE_READ | FILE_SHARE_WRITE, but not FILE_SHARE_DELETE.
    let reader = OpenOptions::new()
        .read(true)
        .share_mode(3)
        .open(dir.join("session.json"))
        .expect("hold the file");
    fs::write(dir.join("probe.tmp"), b"x").expect("probe");
    assert!(
        fs::rename(dir.join("probe.tmp"), dir.join("session.json")).is_err(),
        "sanity: replacing the held file by rename does not work"
    );
    fs::remove_file(dir.join("probe.tmp")).expect("remove probe");
    drop(s);
    drop(reader);
    let done: serde_json::Value =
        serde_json::from_slice(&fs::read(dir.join("session.json")).expect("read")).expect("json");
    assert_eq!(done["state"], "ended", "the end was lost: {done}");
    assert!(!dir.join("session.json.tmp").exists());
}

#[test]
fn per_segment_boxes_are_not_init_bytes() {
    // DASH packagers put styp and a per-segment sidx in front of the
    // moof; with the init in the same append they belong to the media.
    let init = init_with_traks(1);
    let mut append = init.clone();
    append.extend(mp4_box(*b"styp", b"msdh"));
    append.extend(mp4_box(*b"sidx", b"1234"));
    append.extend(moof(0));
    assert_eq!(init_prefix_len(Container::Mp4, &append), Some(init.len()));
    let mut free = init.clone();
    free.extend(mp4_box(*b"free", b"x"));
    let mut with_free = free.clone();
    with_free.extend(moof(0));
    assert_eq!(
        init_prefix_len(Container::Mp4, &with_free),
        Some(free.len()),
        "a free box after moov stays init"
    );

    // A switch back with a different sidx each time continues part 0.
    let mut s = session("sidx");
    let init_b = init_with_traks(2);
    let with = |init: &[u8], t: u32| {
        let mut v = init.to_vec();
        v.extend(mp4_box(*b"styp", b"msdh"));
        v.extend(mp4_box(*b"sidx", &t.to_be_bytes()));
        v.extend(moof(t));
        v
    };
    write(&mut s, &seg(1, true, Some(0), None), &with(&init, 0));
    write(
        &mut s,
        &seg(1, true, Some(1000), None),
        &with(&init_b, 1000),
    );
    write(&mut s, &seg(1, true, Some(2000), None), &with(&init, 2000));
    s.finish();
    assert!(!s.dir.join("sb1.p2.bin").exists(), "no third part");
    assert_eq!(s.tracks[&1].part, 0, "back in part 0");
}

#[test]
fn untimed_media_after_a_complete_init_is_media() {
    // The init alone, then a segment cut inside its moof (the first
    // piece has no tfdt): the piece is media, so the same init after a
    // seek is a duplicate and opens no part.
    let mut s = session("cutmoof");
    let init = init_with_traks(1);
    for t in [0u32, 1000] {
        write(&mut s, &seg(1, true, None, None), &init);
        let m = moof(t);
        write(&mut s, &seg(1, false, None, None), &m[..10]);
        write(&mut s, &seg(1, false, None, None), &m[10..]);
    }
    s.finish();
    assert!(!s.dir.join("sb1.p1.bin").exists(), "one part");
    let rows: Vec<serde_json::Value> = fs::read_to_string(s.dir.join("index.jsonl"))
        .expect("index")
        .lines()
        .map(|l| serde_json::from_str(l).expect("json"))
        .collect();
    let inits = rows.iter().filter(|r| r["is_init"] == true).count();
    assert_eq!(inits, 1, "one init row, the pieces are media");
}

#[test]
fn init_complete_needs_moov_or_tracks() {
    let init = init_with_traks(1);
    assert!(init_complete(Container::Mp4, &init));
    assert!(!init_complete(Container::Mp4, &init[..init.len() - 1]));
    assert!(!init_complete(Container::Mp4, &mp4_box(*b"ftyp", b"iso6")));
    // EBML header, Segment (unknown size), Info, Tracks.
    let ebml = [0x1a, 0x45, 0xdf, 0xa3, 0x80];
    let segment = [
        0x18, 0x53, 0x80, 0x67, 0x01, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
    ];
    let info = [0x15, 0x49, 0xa9, 0x66, 0x80];
    let tracks = [0x16, 0x54, 0xae, 0x6b, 0x80];
    let mut head = [&ebml[..], &segment, &info].concat();
    assert!(!init_complete(Container::Webm, &head));
    head.extend_from_slice(&tracks);
    assert!(init_complete(Container::Webm, &head));
}

#[test]
fn parts_per_track_are_capped() {
    let mut s = session("parts");
    for i in 0..(MAX_PARTS_PER_TRACK + 6) {
        let mut init = init_with_traks(1);
        init.extend(mp4_box(*b"free", &i.to_be_bytes()));
        write(&mut s, &seg(1, true, None, None), &init);
        write(&mut s, &seg(1, false, Some(u64::from(i)), None), &moof(i));
    }
    s.finish();
    let files = fs::read_dir(&s.dir)
        .expect("dir")
        .filter(|e| {
            e.as_ref()
                .is_ok_and(|e| e.file_name().to_string_lossy().starts_with("sb1.p"))
        })
        .count();
    assert_eq!(files, MAX_PARTS_PER_TRACK as usize);
    assert!(s.tracks[&1].parts_full);
    assert!(s.tracks[&1].skipped >= 6, "media under new inits skipped");
}

#[test]
fn sequence_mode_lays_out_jumps_like_the_browser() {
    // Period 1 (0, 1000), then period 2 restarting tfdt (0, 1000), then a
    // seek back fetching 500 again: placed one after the other.
    let dir = temp_dir("seqlayout");
    let init = init_with_traks(1);
    let segs = [moof(0), moof(1000), moof(0), moof(1000), moof(500)];
    let mut rows = write_part(
        &dir,
        &[
            (&init, true, None, None),
            (&segs[0], false, Some(0), None),
            (&segs[1], false, Some(1000), None),
            (&segs[2], false, Some(0), None),
            (&segs[3], false, Some(1000), None),
            (&segs[4], false, Some(500), None),
        ],
    );
    for r in &mut rows[1..] {
        r.seq_mode = true;
        r.dur = Some(1000);
    }
    let got = order(&dir, &rows).0;
    let media = &got[init.len()..];
    let tfdts: Vec<u32> = media.chunks(32).map(tfdt_of).collect();
    assert_eq!(tfdts, [0, 1000, 2000, 3000, 4000]);

    // Without durations the jumps cannot be laid out: arrival order.
    for r in &mut rows[1..] {
        r.dur = None;
    }
    let (got, path, _) = order(&dir, &rows);
    assert_eq!(path, dir.join("sb1.p0.bin"), "unchanged");
    let tfdts: Vec<u32> = got[init.len()..].chunks(32).map(tfdt_of).collect();
    assert_eq!(tfdts, [0, 1000, 0, 1000, 500]);
}

#[test]
fn sequence_mode_follows_a_set_timestamp_offset() {
    // Shaka in sequence mode: 0, 1 s, a seek to 4 s (timestampOffset set
    // to 4), 5 s (the browser left the offset at 0), a seek back to 2 s
    // (set to 2), 3 s. The browser places each at its own time.
    let dir = temp_dir("seqtso");
    let init = init_with_traks(1);
    let segs = [
        moof(0),
        moof(1000),
        moof(4000),
        moof(5000),
        moof(2000),
        moof(3000),
    ];
    let set = [false, false, true, false, true, false];
    let tso = [0.0, 0.0, 4.0, 0.0, 2.0, 0.0];
    let mut list: Vec<TestSeg<'_>> = vec![(&init, true, None, None)];
    for (i, s) in segs.iter().enumerate() {
        list.push((s, false, Some(u64::from(tfdt_of(s))), Some(tso[i])));
    }
    let mut rows = write_part(&dir, &list);
    for (i, r) in rows[1..].iter_mut().enumerate() {
        r.seq_mode = true;
        r.dur = Some(1000);
        r.tso_set = set[i];
    }
    let got = order(&dir, &rows).0;
    let tfdts: Vec<u32> = got[init.len()..].chunks(32).map(tfdt_of).collect();
    assert_eq!(tfdts, [0, 1000, 2000, 3000, 4000, 5000], "in time order");
    let refs: Vec<&IndexRow> = rows.iter().collect();
    let (moves, jumps) = seq_layout(&refs, &HashMap::new());
    assert_eq!(jumps, 0, "no discontinuity");
    assert!(
        moves.values().all(|m| *m == 0),
        "each at its own time, none moved: {moves:?}"
    );
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
fn init_index_range_stays_one_init() {
    // SegmentBase: ftyp moov sidx with no moof is all init.
    let mut v = init_with_traks(1);
    v.extend(mp4_box(*b"sidx", b"1234"));
    assert_eq!(init_prefix_len(Container::Mp4, &v), None);
}

#[test]
fn init_scan_resumes_where_it_stopped() {
    let init = init_with_traks(1);
    let (done, at) = init_scan(Container::Mp4, &init[..20], 0);
    assert!(!done);
    assert_eq!(at, 12, "after the complete ftyp");
    assert!(init_scan(Container::Mp4, &init, at).0);
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
fn a_cut_moof_still_splits_a_combined_append() {
    let init = init_with_traks(1);
    let mut v = init.clone();
    v.extend(mp4_box(*b"styp", b"msdh"));
    v.extend(&moof(0)[..12]); // header says 40 bytes, 12 arrived
    assert_eq!(init_prefix_len(Container::Mp4, &v), Some(init.len()));
}

/// A cut `moof` without `tfdt` in its own append, arriving while an init
/// is still incomplete, is media: it must not join the init (and its
/// hash, and the DRM scan of the init).
#[test]
fn an_untimed_moof_does_not_join_an_incomplete_init() {
    let mut s = session("moofnotinit");
    write(&mut s, &seg(1, true, None, None), b"IN");
    let cut = moof(0)[..12].to_vec(); // header says 40 bytes, 12 arrived
    write(&mut s, &seg(1, false, None, None), &cut);
    let rows = index_rows(&s.dir);
    assert_eq!(rows[0]["is_init"], true);
    assert_eq!(rows[0]["len"], 2, "the init is just its own bytes");
    assert_eq!(rows[1]["is_init"], false, "the cut moof is media");
}

#[test]
fn pending_chunks_count_their_header() {
    // An ftyp-only init, then 1-byte chunks: each costs its header too.
    let mut s = session("pendcost");
    write(
        &mut s,
        &seg(1, true, None, None),
        &mp4_box(*b"ftyp", b"iso6"),
    );
    for _ in 0..(MAX_PENDING_INIT / PENDING_CHUNK_COST + 10) {
        write(&mut s, &seg(1, false, None, None), b"x");
    }
    assert!(s.tracks[&1].pending_cost() <= MAX_PENDING_INIT);
}

#[test]
fn rescale_rounds_between_timescales() {
    assert_eq!(rescale(1000, Some(1000), 90_000), 90_000);
    assert_eq!(rescale(-3, Some(2), 1), -2);
    assert_eq!(rescale(5, None, 1000), 0);
}

#[test]
fn an_event_name_with_escapes_is_defanged_and_cut() {
    let name = format!("\x1b[2J\x1b]0;owned\x07{}", "a".repeat(100));
    let line = event_log_line(Path::new("d"), &name, Some(1));
    assert!(!line.contains('\x1b') && !line.contains('\x07'));
    assert!(line.starts_with("d: event ?[2J?]0;owned?"));
    assert!(line.len() < 60);
}

#[test]
fn open_inits_of_many_tracks_stay_under_the_session_cap() {
    // Each track holds an unfinished init (a stub, then 1 MiB `initCont`
    // chunks): alone below the track cap, together far over the session cap.
    let mut s = session("pendsession");
    let mut cont = seg(0, false, None, None);
    cont.init_cont = true;
    for sb in 0..12u32 {
        write(&mut s, &seg(sb, true, None, None), b"IN");
        cont.sb = sb;
        for _ in 0..3 {
            write(&mut s, &cont, &vec![7u8; 1 << 20]);
        }
    }
    let total: usize = s.tracks.values().map(TrackState::pending_cost).sum();
    assert!(total > 0, "the inits must still be pending");
    assert!(total <= MAX_PENDING_SESSION, "{total}");
}

// --- replay (a recording saved on a phone, extension/src/file-target.js) ---------------------------------------

/// One message as the extension sends it: header length, JSON header, payload.
fn wire_message(header: &serde_json::Value, payload: &[u8]) -> Vec<u8> {
    let h = serde_json::to_vec(header).expect("json");
    let mut v = u32::try_from(h.len())
        .expect("small")
        .to_le_bytes()
        .to_vec();
    v.extend(h);
    v.extend_from_slice(payload);
    v
}

/// A recording of `messages`, as file-target.js saves it.
fn recording(messages: &[Vec<u8>]) -> Vec<u8> {
    let mut v = RECORDING_MAGIC.to_vec();
    for m in messages {
        v.extend(u32::try_from(m.len()).expect("small").to_le_bytes());
        v.extend(m);
    }
    v
}

fn rec_segment(seq: u64, is_init: bool, tfdt: Option<u64>, bytes: &[u8]) -> Vec<u8> {
    wire_message(
        &serde_json::json!({ "type": "segment", "sb": 1, "seq": seq, "ms": 1, "mime": "video/mp4",
            "isInit": is_init, "kind": "mp4", "tfdt": tfdt, "timescale": 1000 }),
        bytes,
    )
}

fn rec_hello() -> Vec<u8> {
    wire_message(
        &serde_json::json!({ "type": "hello", "token": "", "session": "phone",
            "url": "https://www.ardmediathek.de/video/x", "title": "On the phone" }),
        &[],
    )
}

#[test]
fn a_recording_is_replayed_into_a_capture_directory_as_a_live_connection_writes_it() {
    let dir = temp_dir("replay");
    let init = init_with_traks(1);
    let (m1, m2) = (moof(0), moof(1000));
    let rec = recording(&[
        rec_hello(),
        rec_segment(1, true, None, &init),
        rec_segment(2, false, Some(0), &m1),
        rec_segment(3, false, Some(1000), &m2),
        wire_message(
            &serde_json::json!({ "type": "event", "name": "recordfull", "sb": null }),
            &[],
        ),
    ]);
    let done =
        replay_from(&rec[..], &dir, &budget(16 * GIB, 64 * GIB), &refusals()).expect("replayed");
    assert_eq!((done.bad, done.cut), (0, false));
    let bin = fs::read(done.dir.join("sb1.p0.bin")).expect("part");
    assert_eq!(bin, [init.clone(), m1, m2].concat());
    assert_eq!(index_lines(&done.dir), 3);
    let session = fs::read_to_string(done.dir.join("session.json")).expect("session");
    assert!(
        session.contains("ardmediathek") && session.contains("On the phone"),
        "{session}"
    );
}

#[test]
fn a_recording_cut_short_keeps_what_came_before() {
    let dir = temp_dir("replay-cut");
    let init = init_with_traks(1);
    let mut rec = recording(&[
        rec_hello(),
        rec_segment(1, true, None, &init),
        rec_segment(2, false, Some(0), &moof(0)),
    ]);
    // The last message loses its tail (a save cut short).
    rec.truncate(rec.len() - 3);
    let done =
        replay_from(&rec[..], &dir, &budget(16 * GIB, 64 * GIB), &refusals()).expect("replayed");
    assert!(done.cut);
    assert_eq!(fs::read(done.dir.join("sb1.p0.bin")).expect("part"), init);
}

#[test]
fn what_is_no_recording_is_refused_and_writes_nothing() {
    let dir = temp_dir("replay-bad");
    let not_magic = b"STREAMTAP-RECORDING/2\n".to_vec();
    let no_hello = recording(&[rec_segment(1, true, None, &init_with_traks(1))]);
    let empty = recording(&[]);
    for (name, bytes) in [
        ("magic", not_magic),
        ("no hello", no_hello),
        ("empty", empty),
    ] {
        let got = replay_from(&bytes[..], &dir, &budget(16 * GIB, 64 * GIB), &refusals());
        assert!(got.is_err(), "{name}");
    }
    assert_eq!(
        fs::read_dir(&*dir).expect("dir").count(),
        0,
        "nothing written"
    );
}
