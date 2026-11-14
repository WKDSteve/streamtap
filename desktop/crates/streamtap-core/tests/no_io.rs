//! streamtap-core does no I/O: its sources name no file, socket, process, environment, clock, thread or terminal.
//!
//! The crate is the part of streamtap that can be tested without a disk and reused where there is none; a
//! `std::fs` slipping in would not fail any other test. This reads the sources as text (the test may touch the
//! disk, the crate may not).

use std::fs;
use std::path::Path;

/// What the sources must not contain, each with what it would be.
const FORBIDDEN: [(&str, &str); 11] = [
    ("std::fs", "the file system"),
    ("std::net", "the network"),
    ("std::process", "a process"),
    ("std::env", "the environment"),
    ("std::io", "streams"),
    ("std::thread", "a thread"),
    ("std::time", "a clock"),
    ("std::os", "the operating system"),
    ("std::path", "a path on a disk"),
    // Also `eprintln!`, which contains it.
    ("println!", "the terminal"),
    ("dbg!", "the terminal"),
];

/// The lines of `text` that are code: comments say what the sink does around the crate and may name anything.
fn code_lines(text: &str) -> impl Iterator<Item = (usize, &str)> {
    text.lines()
        .enumerate()
        .filter(|(_, l)| !l.trim_start().starts_with("//"))
        .map(|(i, l)| (i + 1, l))
}

fn offences(name: &str, text: &str) -> Vec<String> {
    let mut found = Vec::new();
    for (no, line) in code_lines(text) {
        for (needle, what) in FORBIDDEN {
            if line.contains(needle) {
                found.push(format!("{name}:{no}: {needle} ({what})"));
            }
        }
    }
    found
}

#[test]
fn the_sources_name_no_io() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = 0;
    let mut found = Vec::new();
    for entry in fs::read_dir(&src).expect("src") {
        let path = entry.expect("entry").path();
        if path.extension().is_some_and(|e| e == "rs") {
            let name = path
                .file_name()
                .expect("name")
                .to_string_lossy()
                .into_owned();
            found.extend(offences(&name, &fs::read_to_string(&path).expect("read")));
            files += 1;
        }
    }
    assert!(files >= 6, "the sources were not found ({files} files)");
    assert!(
        found.is_empty(),
        "streamtap-core must stay free of I/O:\n{}",
        found.join("\n")
    );
}

/// The check itself: it finds each kind of I/O in code and leaves comments alone.
#[test]
fn the_check_finds_io_in_code_and_not_in_comments() {
    let code =
        "use std::fs::File;\n    let t = std::time::Instant::now();\n    eprintln!(\"x\");\n";
    assert_eq!(
        offences("a.rs", code),
        [
            "a.rs:1: std::fs (the file system)",
            "a.rs:2: std::time (a clock)",
            "a.rs:3: println! (the terminal)",
        ]
    );
    let none: Vec<String> = Vec::new();
    assert_eq!(
        offences(
            "a.rs",
            "/// The sink writes with std::fs.
    // println!
"
        ),
        none
    );
    assert_eq!(
        offences(
            "a.rs",
            "use std::collections::HashMap;
use std::fmt;
"
        ),
        none
    );
}
