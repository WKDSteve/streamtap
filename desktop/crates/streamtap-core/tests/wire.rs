//! streamtap-core: `wire`.

use streamtap_core::wire::*;

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
