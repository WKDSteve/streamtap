//! Reads the sink's capture directories: `session.json` (page URL, title,
//! start time) and `index.jsonl` (one row per written segment) are
//! summarised for the list. `index.jsonl` grows while a capture is live, so
//! each directory keeps an accumulator that only reads what was appended
//! since the last scan.

use std::collections::{BTreeMap, HashMap};
use std::fs::{self, File};
use std::io::{self, Read, Seek, SeekFrom};
use std::path::Path;
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};

use crate::paths;

/// A capture that was written to within this window, while the sink runs,
/// counts as live (the sink keeps no "connection open" flag on disk).
pub(crate) const LIVE_WINDOW: Duration = Duration::from_secs(10);
/// Directories listed at most (newest first).
const MAX_CAPTURES: usize = 1000;
/// New index bytes parsed per capture and scan; the rest follows next poll.
const MAX_INDEX_CHUNK: u64 = 32 * 1024 * 1024;
const MAX_SESSION_BYTES: u64 = 64 * 1024;
/// The sink accepts titles and URLs up to 64 KiB; the list shows far less
/// (1000 such captures would make every 2 s snapshot huge).
const MAX_TITLE_CHARS: usize = 300;
const MAX_URL_CHARS: usize = 1000;
/// File the app writes into a capture when the sink logged a DRM refusal.
pub(crate) const DRM_FLAG: &str = "app-drm.flag";

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct SessionFile {
    url: String,
    title: String,
    started: u64,
}

/// The fields of the sink's `IndexLine` the list needs.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct Row {
    sb: u32,
    len: u64,
    is_init: bool,
    mark: bool,
    dur: Option<u64>,
    timescale: Option<u32>,
    mime: String,
    kind: String,
}

#[derive(Debug, Default, Clone, Serialize)]
pub(crate) struct SbSummary {
    pub(crate) sb: u32,
    pub(crate) kind: String,
    pub(crate) mime: String,
    pub(crate) bytes: u64,
    pub(crate) segments: u64,
    #[serde(skip)]
    dur_ms: u64,
}

/// Running totals of one `index.jsonl`.
#[derive(Debug, Default)]
pub(crate) struct IndexAcc {
    /// Bytes of the file consumed (always at a line start).
    pos: u64,
    inits: u64,
    segments: u64,
    bytes: u64,
    bad_lines: u64,
    sbs: BTreeMap<u32, SbSummary>,
}

impl IndexAcc {
    /// Adds the complete lines of `data`; returns the bytes consumed (a
    /// trailing line without a newline is left for the next call). Works on
    /// the raw bytes so the count is the file's, whatever the lines hold.
    fn feed(&mut self, data: &[u8]) -> usize {
        let end = data.iter().rposition(|b| *b == b'\n').map_or(0, |i| i + 1);
        for raw in data[..end].split(|b| *b == b'\n') {
            // Index lines are ASCII JSON; a lossy conversion only touches a
            // forged byte, which the line parser then rejects.
            let line = String::from_utf8_lossy(raw);
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str::<Row>(&line) {
                Ok(row) => self.add(row),
                Err(_) => self.bad_lines += 1,
            }
        }
        end
    }

    fn add(&mut self, row: Row) {
        // A `mark` row only holds a place in the timeline: no bytes, no segment.
        if row.mark {
            return;
        }
        let sb = self.sbs.entry(row.sb).or_insert_with(|| SbSummary {
            sb: row.sb,
            ..SbSummary::default()
        });
        sb.bytes = sb.bytes.saturating_add(row.len);
        if !row.mime.is_empty() {
            sb.mime = row.mime;
        }
        if !row.kind.is_empty() {
            sb.kind = row.kind;
        }
        self.bytes = self.bytes.saturating_add(row.len);
        if row.is_init {
            self.inits += 1;
            return;
        }
        sb.segments += 1;
        self.segments += 1;
        if let (Some(dur), Some(ts)) = (row.dur, row.timescale.filter(|t| *t > 0)) {
            let ms = u128::from(dur) * 1000 / u128::from(ts);
            sb.dur_ms = sb
                .dur_ms
                .saturating_add(u64::try_from(ms).unwrap_or(u64::MAX));
        }
    }

    /// Reads what `path` gained since the last call. A file that shrank
    /// (replaced) starts over.
    fn update(&mut self, path: &Path) -> io::Result<()> {
        let mut f = match File::open(path) {
            Ok(f) => f,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(e),
        };
        let len = f.metadata()?.len();
        if len < self.pos {
            *self = Self::default();
        }
        if len == self.pos {
            return Ok(());
        }
        f.seek(SeekFrom::Start(self.pos))?;
        let mut buf = Vec::new();
        f.take((len - self.pos).min(MAX_INDEX_CHUNK))
            .read_to_end(&mut buf)?;
        let mut used = self.feed(&buf) as u64;
        if used == 0 && buf.len() as u64 >= MAX_INDEX_CHUNK {
            // A "line" longer than a whole chunk is no index row: count it
            // as one bad line and move on instead of re-reading it forever.
            self.bad_lines += 1;
            used = buf.len() as u64;
        }
        self.pos += used;
        Ok(())
    }

    /// The longest `SourceBuffer`: one capture is one video, its tracks run in parallel.
    fn duration_ms(&self) -> u64 {
        self.sbs.values().map(|s| s.dur_ms).max().unwrap_or(0)
    }
}

#[derive(Debug, Serialize)]
pub(crate) struct CaptureInfo {
    pub(crate) id: String,
    pub(crate) title: String,
    pub(crate) url: String,
    pub(crate) host: String,
    /// Unix seconds, 0 when unknown.
    pub(crate) started: u64,
    pub(crate) duration_ms: u64,
    pub(crate) bytes: u64,
    pub(crate) segments: u64,
    pub(crate) inits: u64,
    pub(crate) bad_lines: u64,
    pub(crate) source_buffers: Vec<SbSummary>,
    pub(crate) live: bool,
    pub(crate) drm: bool,
    pub(crate) muxed: bool,
    pub(crate) result_bytes: u64,
}

/// The host of a page URL (`https://user@Example.com:8080/x` gives `example.com`).
pub(crate) fn host_of(url: &str) -> String {
    let rest = url.split_once("://").map_or(url, |(_, r)| r);
    let authority = rest.split(['/', '\\', '?', '#']).next().unwrap_or("");
    let host = authority.rsplit('@').next().unwrap_or("");
    let host = if host.starts_with('[') {
        host.split(']')
            .next()
            .map_or(host, str::trim)
            .trim_start_matches('[')
    } else {
        host.split(':').next().unwrap_or("")
    };
    host.chars()
        .filter(char::is_ascii_graphic)
        .collect::<String>()
        .to_ascii_lowercase()
}

/// Page-controlled text made safe to show: control characters and the
/// bidirectional or invisible formatting characters (which can reorder or
/// hide text) are dropped, and the length is bounded.
pub(crate) fn display_text(text: &str, max_chars: usize) -> String {
    text.chars()
        .filter(|c| {
            !c.is_control()
                && !matches!(
                    *c,
                    '\u{200b}'..='\u{200f}'
                        | '\u{2028}'..='\u{202e}'
                        | '\u{2060}'..='\u{2069}'
                        | '\u{feff}'
                )
        })
        .take(max_chars)
        .collect()
}

fn read_session(dir: &Path) -> SessionFile {
    let Ok(f) = File::open(dir.join("session.json")) else {
        return SessionFile::default();
    };
    let mut text = String::new();
    if f.take(MAX_SESSION_BYTES).read_to_string(&mut text).is_err() {
        return SessionFile::default();
    }
    serde_json::from_str(&text).unwrap_or_default()
}

/// Whether the sink is running and wrote into `dir` within `LIVE_WINDOW`.
pub(crate) fn is_live(dir: &Path, sink_running: bool, now: SystemTime) -> bool {
    if !sink_running {
        return false;
    }
    ["index.jsonl", "session.json"]
        .iter()
        .filter_map(|n| fs::metadata(dir.join(n)).ok()?.modified().ok())
        .max()
        .is_some_and(|m| now.duration_since(m).unwrap_or_default() <= LIVE_WINDOW)
}
/// Lists the capture directories of `root`, newest first. `cache` holds one
/// accumulator per directory and is pruned of directories that are gone.
pub(crate) fn scan(
    root: &Path,
    cache: &mut HashMap<String, IndexAcc>,
    sink_running: bool,
    now: SystemTime,
) -> io::Result<Vec<CaptureInfo>> {
    let mut names = Vec::new();
    for entry in fs::read_dir(root)? {
        let Ok(entry) = entry else { continue };
        let Ok(ft) = entry.file_type() else { continue };
        // file_type() does not follow links: a symlink or junction is no capture.
        if !ft.is_dir() || ft.is_symlink() {
            continue;
        }
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if paths::validate_id(&name).is_ok() {
            names.push(name);
        }
    }
    // Names start with the unix start time; longer (later) numbers sort last.
    names.sort_by(|a, b| a.len().cmp(&b.len()).then_with(|| a.cmp(b)));
    names.reverse();
    names.truncate(MAX_CAPTURES);
    cache.retain(|k, _| names.contains(k));

    let mut out = Vec::with_capacity(names.len());
    for name in names {
        let dir = root.join(&name);
        let session = read_session(&dir);
        let index_path = dir.join("index.jsonl");
        let acc = cache.entry(name.clone()).or_default();
        // An unreadable index leaves the totals as they were.
        let _ = acc.update(&index_path);
        let live = is_live(&dir, sink_running, now);
        let result = paths::result_file(&dir);
        let drm = fs::symlink_metadata(dir.join(DRM_FLAG)).is_ok_and(|m| m.is_file());
        let mut source_buffers: Vec<SbSummary> = acc.sbs.values().cloned().collect();
        source_buffers.sort_by_key(|s| s.sb);
        out.push(CaptureInfo {
            host: host_of(&session.url),
            id: name,
            title: display_text(&session.title, MAX_TITLE_CHARS),
            url: display_text(&session.url, MAX_URL_CHARS),
            started: session.started,
            duration_ms: acc.duration_ms(),
            bytes: acc.bytes,
            segments: acc.segments,
            inits: acc.inits,
            bad_lines: acc.bad_lines,
            source_buffers,
            live,
            drm,
            muxed: result.is_some(),
            result_bytes: result.map_or(0, |r| r.1),
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn row(sb: u32, len: u64, init: bool, dur: u64, ts: u32) -> String {
        format!(
            "{{\"sb\":{sb},\"ms\":1,\"part\":0,\"seq\":1,\"offset\":0,\"len\":{len},\"is_init\":{init},\"tfdt\":0,\"timescale\":{ts},\"dur\":{dur},\"tso\":0.0,\"seq_mode\":false,\"tso_set\":false,\"mime\":\"video/mp4\",\"kind\":\"video\"}}\n"
        )
    }

    fn tmp(name: &str) -> std::path::PathBuf {
        let p =
            std::env::temp_dir().join(format!("streamtap-app-cap-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&p);
        fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn accumulates_rows_per_source_buffer() {
        let mut acc = IndexAcc::default();
        let text = [
            row(1, 100, true, 0, 1000),
            row(1, 5000, false, 4000, 1000),
            row(1, 6000, false, 4000, 1000),
            row(2, 700, false, 2_000, 48_000),
            "{\"sb\":1,\"len\":999,\"mark\":true,\"is_init\":false}\n".into(),
            "not json\n".into(),
        ]
        .concat();
        assert_eq!(acc.feed(text.as_bytes()), text.len());
        assert_eq!(acc.inits, 1);
        assert_eq!(acc.segments, 3);
        assert_eq!(acc.bytes, 100 + 5000 + 6000 + 700);
        assert_eq!(acc.bad_lines, 1);
        assert_eq!(
            acc.duration_ms(),
            8000,
            "the longest SourceBuffer, marks ignored"
        );
        assert_eq!(acc.sbs[&1].segments, 2);
        assert_eq!(acc.sbs[&2].mime, "video/mp4");
    }

    #[test]
    fn a_partial_last_line_waits_for_its_newline() {
        let mut acc = IndexAcc::default();
        let full = row(1, 10, false, 1000, 1000);
        let (a, b) = full.split_at(full.len() - 5);
        assert_eq!(acc.feed(a.as_bytes()), 0);
        assert_eq!(acc.segments, 0);
        assert_eq!(acc.feed(format!("{a}{b}").as_bytes()), full.len());
        assert_eq!(acc.segments, 1);
    }

    #[test]
    fn zero_timescale_does_not_divide_by_zero() {
        let mut acc = IndexAcc::default();
        acc.feed(row(1, 10, false, 1000, 0).as_bytes());
        assert_eq!(acc.duration_ms(), 0);
        assert_eq!(acc.segments, 1);
    }

    #[test]
    fn update_reads_only_appended_lines_and_survives_a_replaced_file() {
        let dir = tmp("update");
        let path = dir.join("index.jsonl");
        fs::write(&path, row(1, 10, false, 1000, 1000)).unwrap();
        let mut acc = IndexAcc::default();
        acc.update(&path).unwrap();
        assert_eq!(acc.segments, 1);
        let mut f = fs::OpenOptions::new().append(true).open(&path).unwrap();
        f.write_all(row(1, 10, false, 1000, 1000).as_bytes())
            .unwrap();
        // A line cut in the middle is not counted yet.
        f.write_all(b"{\"sb\":1,\"len\":5").unwrap();
        drop(f);
        acc.update(&path).unwrap();
        assert_eq!(acc.segments, 2, "counted once, not re-read from the start");
        assert_eq!(acc.duration_ms(), 2000);
        fs::write(&path, row(3, 1, false, 500, 1000)).unwrap();
        acc.update(&path).unwrap();
        assert_eq!(acc.segments, 1, "a shorter file starts over");
        assert!(acc.sbs.contains_key(&3) && !acc.sbs.contains_key(&1));
        // A missing index is not an error (the directory was just created).
        let mut empty = IndexAcc::default();
        empty.update(&dir.join("nothing.jsonl")).unwrap();
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn an_invalid_byte_does_not_shift_the_file_position() {
        // A lossy conversion turns one bad byte into three (U+FFFD); the
        // consumed count must still be the file's, or every later line is
        // read from the wrong place.
        let dir = tmp("badbyte");
        let path = dir.join("index.jsonl");
        let mut data = b"garbage \xff\xfe\xfd line\n".to_vec();
        data.extend_from_slice(row(1, 10, false, 1000, 1000).as_bytes());
        fs::write(&path, &data).unwrap();
        let mut acc = IndexAcc::default();
        acc.update(&path).unwrap();
        assert_eq!(acc.bad_lines, 1);
        assert_eq!(acc.segments, 1);
        assert_eq!(acc.pos, data.len() as u64);
        let mut f = fs::OpenOptions::new().append(true).open(&path).unwrap();
        f.write_all(row(1, 10, false, 1000, 1000).as_bytes())
            .unwrap();
        drop(f);
        acc.update(&path).unwrap();
        assert_eq!(
            acc.segments, 2,
            "the appended row is read from the right offset"
        );
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn page_text_is_bounded_and_stripped() {
        assert_eq!(display_text("a\u{202e}b\u{200b}c\nd\te", 100), "abcde");
        assert_eq!(display_text(&"x".repeat(5000), 300).len(), 300);
        assert_eq!(display_text("\u{e4}\u{6f22}", 100), "\u{e4}\u{6f22}");
    }

    #[test]
    fn scan_bounds_a_huge_title_and_url() {
        let root = tmp("hugetitle");
        let d = root.join("1_x");
        fs::create_dir(&d).unwrap();
        let big = "t\u{202e}".repeat(7_000);
        fs::write(
            d.join("session.json"),
            format!("{{\"url\":\"{big}\",\"title\":\"{big}\",\"started\":1}}"),
        )
        .unwrap();
        let list = scan(&root, &mut HashMap::new(), false, SystemTime::now()).unwrap();
        assert_eq!(list[0].title.chars().count(), MAX_TITLE_CHARS);
        assert_eq!(list[0].url.chars().count(), MAX_URL_CHARS);
        assert!(!list[0].title.contains('\u{202e}'));
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn host_of_reads_a_backslash_as_the_end_of_the_host() {
        // Browsers treat `\` like `/` in special URLs: this host is evil.example.
        assert_eq!(
            host_of(r"https://evil.example\@good.example/x"),
            "evil.example"
        );
    }

    #[test]
    fn host_of_handles_the_odd_cases() {
        assert_eq!(
            host_of("https://User:pw@Example.COM:8080/a?b#c"),
            "example.com"
        );
        assert_eq!(host_of("http://[::1]:80/x"), "::1");
        assert_eq!(host_of("example.org/path"), "example.org");
        assert_eq!(host_of(""), "");
        assert_eq!(host_of("https://a.b/\u{202e}x"), "a.b");
    }

    #[test]
    fn scan_lists_captures_newest_first_with_state() {
        let root = tmp("scan");
        // Two captures, a stray file, a name that is no valid id.
        for (name, url, secs) in [
            ("1730000000_aaaa", "https://old.example/v", 1_730_000_000u64),
            ("1730000500_bbbb", "https://new.example/v", 1_730_000_500),
        ] {
            let d = root.join(name);
            fs::create_dir(&d).unwrap();
            fs::write(
                d.join("session.json"),
                format!("{{\"url\":\"{url}\",\"title\":\"T {name}\",\"session\":\"s\",\"started\":{secs}}}"),
            )
            .unwrap();
            fs::write(d.join("index.jsonl"), row(1, 2048, false, 3000, 1000)).unwrap();
        }
        fs::write(root.join("stray.txt"), b"x").unwrap();
        fs::create_dir(root.join("bad name")).unwrap();
        fs::write(root.join("1730000000_aaaa").join("out.mp4"), b"12345").unwrap();
        fs::write(root.join("1730000000_aaaa").join(DRM_FLAG), b"").unwrap();

        let mut cache = HashMap::new();
        let list = scan(&root, &mut cache, true, SystemTime::now()).unwrap();
        let ids: Vec<_> = list.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(ids, ["1730000500_bbbb", "1730000000_aaaa"]);
        let new = &list[0];
        assert_eq!(new.host, "new.example");
        assert!(new.live, "just written, sink running");
        assert!(!new.muxed && !new.drm);
        assert_eq!((new.segments, new.bytes, new.duration_ms), (1, 2048, 3000));
        let old = &list[1];
        assert!(old.muxed && old.drm);
        assert_eq!(old.result_bytes, 5);

        // The sink is not running: nothing is live. Old files: nothing is live.
        let stopped = scan(&root, &mut cache, false, SystemTime::now()).unwrap();
        assert!(stopped.iter().all(|c| !c.live));
        let later = SystemTime::now() + LIVE_WINDOW + Duration::from_secs(5);
        let aged = scan(&root, &mut cache, true, later).unwrap();
        assert!(aged.iter().all(|c| !c.live));

        // A removed capture leaves the cache.
        fs::remove_dir_all(root.join("1730000000_aaaa")).unwrap();
        let after = scan(&root, &mut cache, true, SystemTime::now()).unwrap();
        assert_eq!(after.len(), 1);
        assert_eq!(cache.len(), 1);
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_capture_with_broken_files_still_lists() {
        let root = tmp("broken");
        let d = root.join("1_x");
        fs::create_dir(&d).unwrap();
        fs::write(d.join("session.json"), b"{ truncated").unwrap();
        let mut cache = HashMap::new();
        let list = scan(&root, &mut cache, false, SystemTime::now()).unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].title, "");
        assert_eq!(list[0].segments, 0);
        fs::remove_dir_all(&root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn scan_skips_symlinked_directories() {
        let root = tmp("scanlink");
        let outside = tmp("scanlink-out");
        std::os::unix::fs::symlink(&outside, root.join("1_link")).unwrap();
        let list = scan(&root, &mut HashMap::new(), false, SystemTime::now()).unwrap();
        assert!(list.is_empty());
        fs::remove_dir_all(&root).unwrap();
        fs::remove_dir_all(&outside).unwrap();
    }
}
