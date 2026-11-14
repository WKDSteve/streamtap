//! streamtap-core: `fmp4`.

use streamtap_core::fmp4::*;
use streamtap_core::timeline::shift_ticks;
use streamtap_testkit::*;

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
fn a_cut_moof_still_splits_a_combined_append() {
    let init = init_with_traks(1);
    let mut v = init.clone();
    v.extend(mp4_box(*b"styp", b"msdh"));
    v.extend(&moof(0)[..12]); // header says 40 bytes, 12 arrived
    assert_eq!(init_prefix_len(Container::Mp4, &v), Some(init.len()));
}
