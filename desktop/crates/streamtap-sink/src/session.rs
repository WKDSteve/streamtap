//! One capture on disk: its tracks, the admission of segments, and the budgets.

use super::*;

/// Media segments remembered for dedupe, per session (all parts and tracks
/// together): beyond this the session takes no more media.
pub(crate) const MAX_SEEN_PER_SESSION: usize = 400_000;
/// Init bytes held until the init is complete (an init split across appends),
/// counting `PENDING_CHUNK_COST` for each chunk's header.
pub(crate) const MAX_PENDING_INIT: usize = 4 * 1024 * 1024;
/// Parts of a `SourceBuffer` tried after its largest one when that cannot be
/// muxed (`mux_inputs`): each try may cost a sorted copy and an ffprobe run.
pub(crate) const MAX_FALLBACK_PARTS: usize = 3;
pub(crate) const PENDING_CHUNK_COST: usize = 512;
/// Held init bytes of all tracks of one session together. Each track alone is
/// capped by `MAX_PENDING_INIT`; without this, 32 tracks could hold 128 MiB.
pub(crate) const MAX_PENDING_SESSION: usize = 16 * 1024 * 1024;
/// Part files per `SourceBuffer` (one per distinct init).
pub(crate) const MAX_PARTS_PER_TRACK: u32 = 64;
/// Budget charged for each part file (its directory entry and cluster).
pub(crate) const PART_FILE_COST: u64 = 4096;

/// Dedupe keys: (tfdt, timestampOffset in ms, duration, track ID), or in
/// sequence mode (payload hash, `i64::MIN`, length, 0); each with the offset
/// of its row in the part file (a sequence-mode duplicate points there,
/// `Admit::Mark`). The track keeps the appends of two tracks of one
/// `SourceBuffer` that share a start and duration apart (0: no track ID).
pub(crate) type SeenSet = HashMap<(u64, i64, u64, u32), u64>;

#[allow(
    clippy::struct_excessive_bools,
    reason = "independent per-track flags, not states of one machine"
)]
pub(crate) struct TrackState {
    pub(crate) part: u32,
    pub(crate) file: File,
    pub(crate) offset: u64,
    pub(crate) init_hash: Option<u64>,
    pub(crate) seen: SeenSet,
    /// Offset and dedupe set of the parts that are not current, so a switch
    /// back to an earlier init continues its part.
    pub(crate) saved: HashMap<u32, (u64, SeenSet)>,
    /// Init hash (of the complete init) -> its part.
    pub(crate) hash_part: HashMap<u64, u32>,
    /// Parts created so far.
    pub(crate) parts: u32,
    /// Chunks of an init still arriving (it may come in several appends): the
    /// decision (duplicate, switch back, new part) waits for the whole init.
    /// Header and length of each chunk; the bytes are `pending_whole`.
    pub(crate) pending: Vec<(SegmentHeader, usize)>,
    pub(crate) pending_whole: Vec<u8>,
    /// Where the completeness scan of `pending_whole` resumes, and whether it
    /// found the `moov` / `Tracks` (each byte is scanned once).
    pub(crate) pending_scan: usize,
    pub(crate) pending_done: bool,
    /// The pending init came right after an encrypted stretch.
    pub(crate) pending_after_refusal: bool,
    /// Container of the last init: continuation chunks have
    /// no kind of their own.
    pub(crate) init_kind: Container,
    /// The last timed chunk was a duplicate: the untimed chunks that follow
    /// it (its continuation) are duplicates too.
    pub(crate) skip_tail: bool,
    /// Boxes in front of the samples appended without them.
    pub(crate) segment_head: Option<SegmentHead>,
    /// The current init segment is encrypted (EME/DRM): nothing is written
    /// until a complete, clear init is decided (`flush_init`).
    pub(crate) refused: bool,
    /// A new init came beyond `MAX_PARTS_PER_TRACK`: its media is skipped.
    pub(crate) parts_full: bool,
    pub(crate) bytes: u64,
    pub(crate) segments: u64,
    pub(crate) dupes: u64,
    pub(crate) skipped: u64,
}

/// Disk budget shared by all sessions of one sink process.
pub(crate) struct Budget {
    pub(crate) per_session: u64,
    pub(crate) total_max: u64,
    pub(crate) total_used: AtomicU64,
}

impl Budget {
    /// Charges `n` bytes against the total; false (and nothing charged) when
    /// it would exceed the cap.
    pub(crate) fn charge(&self, n: u64) -> bool {
        let mut used = self.total_used.load(Ordering::Relaxed);
        loop {
            let next = used.saturating_add(n);
            if next > self.total_max {
                return false;
            }
            match self.total_used.compare_exchange_weak(
                used,
                next,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => return true,
                Err(now) => used = now,
            }
        }
    }
}

/// Tracks refused as DRM, shared by all connections of one sink process. A
/// reconnect opens a new session that may never see the encrypted init again,
/// and `WebM` media carries no marker of its own: the track stays refused
/// until a complete, clear init is decided (`flush_init`). Key: the extension's session id (one per
/// `MediaSource`, the same on every socket) and the `SourceBuffer`.
#[derive(Default)]
pub(crate) struct Refusals {
    pub(crate) keys: Mutex<VecDeque<(String, u32)>>,
}

impl Refusals {
    pub(crate) fn keys(&self) -> std::sync::MutexGuard<'_, VecDeque<(String, u32)>> {
        // A panic elsewhere leaves the list consistent: keep using it.
        self.keys
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    pub(crate) fn contains(&self, id: &str, sb: u32) -> bool {
        self.keys().iter().any(|(i, s)| i == id && *s == sb)
    }

    pub(crate) fn insert(&self, id: &str, sb: u32) {
        let mut keys = self.keys();
        if keys.iter().any(|(i, s)| i == id && *s == sb) {
            return;
        }
        if keys.len() >= MAX_REFUSALS {
            keys.pop_front();
        }
        keys.push_back((id.to_owned(), sb));
    }

    pub(crate) fn remove(&self, id: &str, sb: u32) {
        self.keys().retain(|(i, s)| !(i == id && *s == sb));
    }
}

pub(crate) struct Session {
    pub(crate) dir: PathBuf,
    /// Content of `session.json`; rewritten with `state: ended` when the
    /// session is dropped (the connection is over), see `mark_ended`.
    pub(crate) meta: serde_json::Value,
    /// The extension's session id (truncated); empty for a client that sends
    /// none, whose refusals are then not remembered (they would collide).
    pub(crate) id: String,
    pub(crate) refusals: Arc<Refusals>,
    pub(crate) index: File,
    pub(crate) tracks: HashMap<u32, TrackState>,
    pub(crate) track_limit_logged: bool,
    /// Bytes written in this session (payload and index lines).
    pub(crate) bytes_total: u64,
    pub(crate) budget: Arc<Budget>,
    /// The session or the process hit its budget: nothing more is written.
    pub(crate) stopped: bool,
    /// Dedupe entries of all tracks and parts of this session.
    pub(crate) seen_total: usize,
    pub(crate) seen_limit_logged: bool,
}

/// A page-controlled string for the terminal: printable ASCII only, at most
/// `max` characters (no escape sequences).
pub(crate) fn printable(s: &str, max: usize) -> String {
    s.chars()
        .take(max)
        .map(|c| {
            if c.is_ascii_graphic() || c == ' ' {
                c
            } else {
                '?'
            }
        })
        .collect()
}

/// The log line for a client event. The name is chosen by the client (any
/// holder of the token), so it goes through `printable` before it reaches the
/// terminal.
pub(crate) fn event_log_line(dir: &Path, name: &str, sb: Option<u32>) -> String {
    format!("{}: event {} sb={sb:?}", dir.display(), printable(name, 32))
}

/// `timestampOffset` in whole milliseconds; non-finite values count as 0.
pub(crate) fn tso_millis(tso: Option<f64>) -> i64 {
    let v = tso.filter(|v| v.is_finite()).unwrap_or(0.0) * 1000.0;
    // Clamped to about ±31 years, so the cast below cannot truncate.
    #[allow(clippy::cast_possible_truncation)]
    let ms = v.clamp(-1e12, 1e12).round() as i64;
    ms
}

pub(crate) fn fnv1a(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325_u64, |h, b| {
        (h ^ u64::from(*b)).wrapping_mul(0x0100_0000_01b3)
    })
}

pub(crate) fn sanitize(s: &str, max: usize) -> String {
    s.chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .take(max)
        .collect()
}

/// What `TrackState::admit_media` decided for one segment.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Admit {
    Skip,
    /// Write it; `as_init` marks init bytes.
    Write {
        as_init: bool,
    },
    /// A sequence-mode duplicate: not written again, but its place in the
    /// timeline is (an index line pointing at the row at `offset`). The
    /// browser placed it, and the next append continues after it.
    Mark {
        offset: u64,
    },
}

impl TrackState {
    pub(crate) fn new(file: File) -> Self {
        Self {
            part: 0,
            file,
            offset: 0,
            init_hash: None,
            seen: HashMap::new(),
            saved: HashMap::new(),
            hash_part: HashMap::new(),
            parts: 1,
            pending: Vec::new(),
            pending_whole: Vec::new(),
            pending_scan: 0,
            pending_done: false,
            pending_after_refusal: false,
            init_kind: Container::Other,
            skip_tail: false,
            segment_head: None,
            refused: false,
            parts_full: false,
            bytes: 0,
            segments: 0,
            dupes: 0,
            skipped: 0,
        }
    }

    /// Holds one more chunk of the pending init.
    pub(crate) fn push_pending(&mut self, h: &SegmentHeader, payload: &[u8]) {
        self.pending.push((h.clone(), payload.len()));
        self.pending_whole.extend_from_slice(payload);
        if !self.pending_done {
            let (done, next) = init_scan(self.init_kind, &self.pending_whole, self.pending_scan);
            self.pending_done = done;
            self.pending_scan = next;
        }
    }

    /// Memory the pending init holds: its bytes plus each chunk's header
    /// (a page can send it in 1-byte chunks).
    pub(crate) fn pending_cost(&self) -> usize {
        self.pending_whole.len() + self.pending.len() * PENDING_CHUNK_COST
    }

    /// Takes the pending chunks and their bytes, leaving nothing pending.
    pub(crate) fn take_pending(&mut self) -> (Vec<(SegmentHeader, usize)>, Vec<u8>) {
        self.pending_scan = 0;
        self.pending_done = false;
        (
            std::mem::take(&mut self.pending),
            std::mem::take(&mut self.pending_whole),
        )
    }

    /// Makes `part` the current part file, keeping the state of the old one.
    pub(crate) fn switch_part(&mut self, dir: &Path, sb: u32, part: u32) -> io::Result<()> {
        let seen = std::mem::take(&mut self.seen);
        self.saved.insert(self.part, (self.offset, seen));
        let (offset, seen) = self.saved.remove(&part).unwrap_or_default();
        self.file = Session::open_part(dir, sb, part)?;
        self.part = part;
        self.offset = offset;
        self.seen = seen;
        Ok(())
    }

    /// Decides about a complete init (hash over all its chunks): a duplicate
    /// or a switch back to an earlier part writes nothing; a new init writes
    /// into a new part (or the first, still empty one).
    pub(crate) fn decide_init(&mut self, hash: u64, dir: &Path, sb: u32) -> io::Result<bool> {
        self.skip_tail = false;
        if self.init_hash == Some(hash) {
            // Same init again (player re-appends after a seek).
            self.dupes += 1;
            return Ok(false);
        }
        self.init_hash = Some(hash);
        if let Some(&earlier) = self.hash_part.get(&hash) {
            // Back to an earlier init (an ABR switch back): continue its
            // part, whose file already starts with this init.
            self.switch_part(dir, sb, earlier)?;
            self.parts_full = false;
            println!("sb{sb} init seen before, continuing part {earlier}");
            self.dupes += 1;
            return Ok(false);
        }
        // Media that came before the first init (the hook attached late)
        // stays alone in its part.
        if !self.hash_part.is_empty() || self.offset > 0 {
            if self.parts >= MAX_PARTS_PER_TRACK {
                // Each distinct init opens a file; a page can forge them.
                // Its media would land under another init: skipped until a
                // known init comes back.
                if !self.parts_full {
                    eprintln!("sb{sb}: more than {MAX_PARTS_PER_TRACK} parts; skipping media under new inits");
                }
                self.parts_full = true;
                return Ok(false);
            }
            self.parts_full = false;
            let part = self.parts;
            self.parts += 1;
            self.switch_part(dir, sb, part)?;
            println!("sb{sb} new init segment, starting part {part}");
        }
        self.hash_part.insert(hash, self.part);
        Ok(true)
    }

    /// Decides about a media segment (timed) or an untimed continuation.
    pub(crate) fn admit_media(
        &mut self,
        h: &SegmentHeader,
        payload: &[u8],
        seen_room: bool,
    ) -> Admit {
        let Some(t) = h.tfdt else {
            // A segment cut before its tfdt: the sequence number of the moof makes the head
            // the same bytes only when it is the same segment, fetched again.
            if starts_a_numbered_moof(payload) {
                let key = (fnv1a(payload), i64::MIN + 1, payload.len() as u64, 0);
                self.skip_tail = self.seen.contains_key(&key);
                if self.skip_tail {
                    self.dupes += 1;
                    return Admit::Skip;
                }
                if !seen_room {
                    self.skipped += 1;
                    return Admit::Skip;
                }
                self.seen.insert(key, self.offset);
                return Admit::Write { as_init: false };
            }
            if self.skip_tail {
                self.dupes += 1;
                return Admit::Skip;
            }
            return Admit::Write { as_init: false };
        };
        self.skip_tail = false;
        // In sequence mode the browser places media one after the other and
        // rewrites timestampOffset itself; a later period may reuse tfdt
        // values. A re-append after a seek has the same bytes, a new period
        // other bytes: the key there is the payload.
        let key = if h.seq_mode {
            (fnv1a(payload), i64::MIN, payload.len() as u64, 0)
        } else {
            (
                t,
                tso_millis(h.tso),
                h.dur.unwrap_or(0),
                h.track.unwrap_or(0),
            )
        };
        if let Some(&offset) = self.seen.get(&key) {
            self.dupes += 1;
            self.skip_tail = true;
            return if h.seq_mode {
                Admit::Mark { offset }
            } else {
                Admit::Skip
            };
        }
        if !seen_room {
            self.skipped += 1;
            return Admit::Skip;
        }
        self.seen.insert(key, self.offset);
        Admit::Write { as_init: false }
    }
}

/// The chunk starts with a `moof` whose `mfhd` (and so its sequence number) arrived in full.
pub(crate) fn starts_a_numbered_moof(payload: &[u8]) -> bool {
    payload.len() >= 24 && &payload[4..8] == b"moof" && &payload[12..16] == b"mfhd"
}

/// Written atomically: a reader never sees half a file.
pub(crate) fn write_session_file(dir: &Path, bytes: &[u8]) -> io::Result<()> {
    let tmp = dir.join("session.json.tmp");
    fs::write(&tmp, bytes)?;
    fs::rename(&tmp, dir.join("session.json"))
}

/// Replaces an existing `session.json`. The rename fails while another
/// process (a virus scanner, a reader that did not share deletion) has the
/// file open; that is tried again a few times and then given up for an
/// in-place write, which is not atomic but is better than a capture that
/// stays `open` for as long as the sink runs.
pub(crate) fn replace_session_file(dir: &Path, bytes: &[u8]) -> io::Result<()> {
    const ATTEMPTS: u32 = 5;
    const PAUSE: Duration = Duration::from_millis(20);
    let mut last = None;
    for attempt in 0..ATTEMPTS {
        match write_session_file(dir, bytes) {
            Ok(()) => return Ok(()),
            Err(e) => last = Some(e),
        }
        if attempt + 1 < ATTEMPTS {
            std::thread::sleep(PAUSE);
        }
    }
    let _ = fs::remove_file(dir.join("session.json.tmp"));
    fs::write(dir.join("session.json"), bytes).map_err(|e| {
        let first = last.map_or_else(String::new, |l| format!(" (rename: {l})"));
        io::Error::new(e.kind(), format!("{e}{first}"))
    })
}

impl Drop for Session {
    /// Runs when the connection is over, also after a panic or a write error.
    fn drop(&mut self) {
        self.mark_ended();
    }
}

impl Session {
    /// Records on disk that this connection is over (`state: ended`, `ended`
    /// in unix seconds), so the desktop app can tell a finished capture from
    /// one that is only quiet. A sink killed hard writes nothing; readers
    /// treat a capture of a sink that is not running as finished.
    pub(crate) fn mark_ended(&mut self) {
        if self.meta["state"] == "ended" {
            return;
        }
        let secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        self.meta["state"] = "ended".into();
        self.meta["ended"] = secs.into();
        let written = serde_json::to_vec_pretty(&self.meta)
            .map_err(io::Error::from)
            .and_then(|bytes| replace_session_file(&self.dir, &bytes));
        if let Err(e) = written {
            eprintln!("{}: session end not recorded: {e}", self.dir.display());
        }
    }

    pub(crate) fn open(
        out: &Path,
        session_id: &str,
        url: &str,
        title: &str,
        budget: &Arc<Budget>,
        refusals: &Arc<Refusals>,
    ) -> io::Result<Option<Self>> {
        let secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        // `state` and `sink_pid` tell a reader whether this connection is
        // still open (`mark_ended` flips it); captures of older sinks have
        // neither.
        let meta = serde_json::json!({
            "url": url, "title": title, "session": session_id, "started": secs,
            "state": "open", "sink_pid": std::process::id(),
        });
        let meta_bytes = serde_json::to_vec_pretty(&meta)?;
        // The directory and session.json count against the budget too; it
        // is charged before anything is created, so a spent process budget
        // creates no further directories.
        // The 4096 bytes of slack also cover the longer `ended` rewrite.
        let open_cost = meta_bytes.len() as u64 + 4096;
        if open_cost > budget.per_session || !budget.charge(open_cost) {
            return Ok(None);
        }
        let short = sanitize(session_id, 8);
        let base = format!("{secs}_{}", if short.is_empty() { "anon" } else { &short });
        // A reconnect of the same session within the same second must not
        // append to the previous directory (its offsets would restart at 0).
        let mut n = 0u32;
        let dir = loop {
            let name = if n == 0 {
                base.clone()
            } else {
                format!("{base}-{n}")
            };
            let dir = out.join(name);
            match fs::create_dir(&dir) {
                Ok(()) => break dir,
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists && n < 1000 => n += 1,
                Err(e) => return Err(e),
            }
        };
        // The index first: a failure after `session.json` said "open" would
        // leave a connection that never ends.
        let index = OpenOptions::new()
            .create(true)
            .append(true)
            .open(dir.join("index.jsonl"))?;
        write_session_file(&dir, &meta_bytes)?;
        Ok(Some(Self {
            dir,
            meta,
            id: session_id.chars().take(MAX_SESSION_ID).collect(),
            refusals: Arc::clone(refusals),
            index,
            tracks: HashMap::new(),
            track_limit_logged: false,
            bytes_total: open_cost,
            budget: Arc::clone(budget),
            stopped: false,
            seen_total: 0,
            seen_limit_logged: false,
        }))
    }

    pub(crate) fn open_part(dir: &Path, sb: u32, part: u32) -> io::Result<File> {
        OpenOptions::new()
            .create(true)
            .append(true)
            .open(dir.join(format!("sb{sb}.p{part}.bin")))
    }

    /// Stops the session for good when `cost` more bytes do not fit the
    /// session or the process budget.
    pub(crate) fn within_budget(&mut self, cost: u64) -> bool {
        if self.stopped {
            return false;
        }
        let session_ok = self.bytes_total.saturating_add(cost) <= self.budget.per_session;
        if session_ok && self.budget.charge(cost) {
            self.bytes_total += cost;
            return true;
        }
        self.stopped = true;
        eprintln!(
            "{}: {} budget reached (--max-session-gib {} / --max-total-gib {}); nothing more is written in this session",
            self.dir.display(),
            if session_ok { "process" } else { "session" },
            self.budget.per_session >> 30,
            self.budget.total_max >> 30
        );
        false
    }

    /// Writes one row: payload into the track's current part, a line into
    /// the index, both against the budget.
    pub(crate) fn write_row(
        &mut self,
        sb: u32,
        h: &SegmentHeader,
        payload: &[u8],
        as_init: bool,
    ) -> io::Result<()> {
        let track = self.tracks.get_mut(&sb).expect("present");
        let line = IndexLine {
            sb,
            ms: h.ms,
            part: track.part,
            seq: h.seq,
            offset: track.offset,
            len: payload.len(),
            is_init: as_init,
            tfdt: if as_init { None } else { h.tfdt },
            track: if as_init { None } else { h.track },
            timescale: h.timescale,
            dur: if as_init { None } else { h.dur },
            tso: h.tso.filter(|v| v.is_finite()),
            seq_mode: h.seq_mode,
            tso_set: h.tso_set && !as_init,
            mark: false,
            mime: &h.mime,
            kind: &h.kind,
        };
        // Serialized first and written in one call: an interrupted sink
        // leaves at most a missing line, not half of one.
        let mut line_buf = serde_json::to_vec(&line)?;
        line_buf.push(b'\n');
        let cost = (payload.len() + line_buf.len()) as u64;
        if !self.within_budget(cost) {
            return Ok(());
        }
        let track = self.tracks.get_mut(&sb).expect("present");
        track.file.write_all(payload)?;
        self.index.write_all(&line_buf)?;
        track.offset += payload.len() as u64;
        track.bytes += payload.len() as u64;
        track.segments += 1;
        Ok(())
    }

    /// Decides about and writes a track's pending init (all its chunks).
    /// True when it was written: not for a duplicate, a switch back, a new
    /// init beyond the part limit, an encrypted one or one over the budget.
    pub(crate) fn flush_init(&mut self, sb: u32) -> io::Result<bool> {
        let dir = self.dir.clone();
        let Some(track) = self.tracks.get_mut(&sb) else {
            return Ok(false);
        };
        if track.pending.is_empty() {
            return Ok(false);
        }
        // Read before `take_pending` resets them.
        let after_refusal = std::mem::take(&mut track.pending_after_refusal);
        let done = track.pending_done;
        let (chunks, whole) = track.take_pending();
        // Each chunk was scanned on its own; markers split across chunks (a
        // WebM ID cut in two, its three IDs in three appends) only show in
        // the whole init.
        let flagged = if track.init_kind.is_webm() {
            webm_encrypted(&whole)
        } else {
            looks_encrypted(&whole)
        };
        // After a refusal only a complete, clear init lifts it. An
        // incomplete one (the connection ended, its continuation may carry
        // the markers) keeps the track refused, here and on the next
        // connection.
        if flagged || (after_refusal && !done) {
            if flagged && !track.refused {
                eprintln!(
                    "{}: sb{sb} carries encrypted media (DRM); not capturing this track",
                    dir.display()
                );
            }
            track.refused = true;
            track.skipped += chunks.len() as u64;
            self.remember_refusal(sb, true);
            return Ok(false);
        }
        if after_refusal {
            self.remember_refusal(sb, false);
        }
        let track = self.tracks.get_mut(&sb).expect("present");
        let parts_before = track.parts;
        let write = track.decide_init(fnv1a(&whole), &dir, sb)?;
        let new_file = track.parts != parts_before;
        // A new part file costs more than its bytes.
        if (new_file && !self.within_budget(PART_FILE_COST)) || !write {
            return Ok(false);
        }
        let mut at = 0;
        for (h, len) in &chunks {
            self.write_row(sb, h, &whole[at..at + len], true)?;
            at += len;
        }
        Ok(true)
    }

    /// The extension lost an init of `sb` (its backlog was full, or the kept
    /// init a new socket replays had to make room) and sends nothing of `sb`
    /// until the next init: recorded in `events.jsonl` for `mux`.
    pub(crate) fn note_lost_init(&mut self, sb: u32) -> io::Result<()> {
        let mut line = serde_json::to_vec(&serde_json::json!({ "sb": sb, "event": "initlost" }))?;
        line.push(b'\n');
        if !self.within_budget(line.len() as u64) {
            return Ok(());
        }
        OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.dir.join("events.jsonl"))?
            .write_all(&line)
    }

    /// Decides about every pending init (end of a connection or stream).
    pub(crate) fn finish(&mut self) {
        let ids: Vec<u32> = self.tracks.keys().copied().collect();
        for sb in ids {
            if let Err(e) = self.flush_init(sb) {
                eprintln!("{}: sb{sb}: init not written: {e}", self.dir.display());
            }
        }
    }

    /// Creates the track of `sb` on its first segment; false when it is over
    /// the track limit or the budget.
    pub(crate) fn ensure_track(&mut self, sb: u32) -> io::Result<bool> {
        if self.tracks.contains_key(&sb) {
            return Ok(true);
        }
        if self.tracks.len() >= MAX_TRACKS {
            if !self.track_limit_logged {
                eprintln!(
                    "{}: more than {MAX_TRACKS} SourceBuffers, ignoring sb{sb} and further new ones",
                    self.dir.display()
                );
                self.track_limit_logged = true;
            }
            return Ok(false);
        }
        if !self.within_budget(PART_FILE_COST) {
            return Ok(false);
        }
        let file = Self::open_part(&self.dir, sb, 0)?;
        let mut track = TrackState::new(file);
        // Refused on an earlier connection: stays refused until a complete,
        // clear init is decided (`flush_init`).
        if !self.id.is_empty() && self.refusals.contains(&self.id, sb) {
            eprintln!(
                "{}: sb{sb} was refused as DRM before this reconnect; not capturing it",
                self.dir.display()
            );
            track.refused = true;
        }
        self.tracks.insert(sb, track);
        Ok(true)
    }

    /// Records whether `sb` is refused as DRM beyond this connection.
    pub(crate) fn remember_refusal(&self, sb: u32, refused: bool) {
        if self.id.is_empty() {
            return;
        }
        if refused {
            self.refusals.insert(&self.id, sb);
        } else {
            self.refusals.remove(&self.id, sb);
        }
    }

    /// An init with its first segment in one append (`init_prefix_len` is
    /// `n`): two rows, so the init is decided (and deduped) on its own bytes.
    pub(crate) fn write_init_and_media(
        &mut self,
        h: &SegmentHeader,
        payload: &[u8],
        n: usize,
    ) -> io::Result<()> {
        let (init, media) = payload.split_at(n);
        let mut hi = h.clone();
        hi.tfdt = None;
        hi.dur = None;
        self.write_segment(&hi, init)?;
        // The init ends where the media starts: decide it now, so an untimed
        // media part (a moof without tfdt) is not taken for its continuation.
        self.flush_init(h.sb)?;
        // Only the boxes in front of the samples (the append ended after its
        // styp): held for the next media append.
        if !has_samples(h.container(), media) && self.hold_segment_head(h, media) {
            return Ok(());
        }
        let mut hm = h.clone();
        hm.is_init = false;
        hm.init_cont = false;
        self.write_segment(&hm, media)
    }

    /// Holds boxes that go in front of samples (`SegmentHead`) instead of
    /// writing a row of their own without samples or tfdt, which would end
    /// the sequence-mode group. Not when too large, or while an init is
    /// still incomplete (then they are its continuation).
    pub(crate) fn hold_segment_head(&mut self, h: &SegmentHeader, bytes: &[u8]) -> bool {
        let Some(track) = self.tracks.get_mut(&h.sb) else {
            return false;
        };
        if bytes.len() > MAX_SEGMENT_HEAD || (!track.pending.is_empty() && !track.pending_done) {
            return false;
        }
        track.segment_head = Some(SegmentHead {
            bytes: bytes.to_vec(),
            tso_set: h.tso_set,
            seq: h.seq,
        });
        true
    }

    /// The held segment head in front of this media append, which also takes
    /// its set offset. Only for the append right after it: an init, or an
    /// append in between that did not arrive, drops it. A row stays within
    /// one message: past that the head's bytes are left out, not the
    /// samples.
    pub(crate) fn join_segment_head<'a>(
        &mut self,
        h: &SegmentHeader,
        payload: &'a [u8],
    ) -> Option<(SegmentHeader, Cow<'a, [u8]>)> {
        let head = self.tracks.get_mut(&h.sb)?.segment_head.take()?;
        if h.is_init || h.init_cont || h.seq != head.seq.wrapping_add(1) {
            return None;
        }
        let mut hj = h.clone();
        hj.tso_set |= head.tso_set;
        if head.bytes.len() + payload.len() > MAX_MESSAGE {
            return Some((hj, Cow::Borrowed(payload)));
        }
        let mut joined = head.bytes;
        joined.extend_from_slice(payload);
        Some((hj, Cow::Owned(joined)))
    }

    /// Encrypted media on `sb` (DRM): the track is refused and nothing of it
    /// is captured; the first time says so on the terminal.
    pub(crate) fn refuse_encrypted(&mut self, sb: u32) {
        let track = self.tracks.get_mut(&sb).expect("present");
        if !track.refused {
            eprintln!(
                "{}: sb{sb} carries encrypted media (DRM); not capturing this track",
                self.dir.display()
            );
        }
        track.refused = true;
        track.take_pending();
        track.skipped += 1;
        self.remember_refusal(sb, true);
    }

    /// Held init bytes of all tracks but `sb` (see `MAX_PENDING_SESSION`).
    pub(crate) fn pending_cost_except(&self, sb: u32) -> usize {
        self.tracks
            .iter()
            .filter(|(k, _)| **k != sb)
            .map(|(_, t)| t.pending_cost())
            .sum()
    }

    pub(crate) fn write_segment(&mut self, h: &SegmentHeader, payload: &[u8]) -> io::Result<()> {
        if self.stopped || payload.is_empty() {
            return Ok(());
        }
        if let Some(n) = h
            .is_init
            .then(|| init_prefix_len(h.container(), payload))
            .flatten()
        {
            return self.write_init_and_media(h, payload, n);
        }
        if let Some((hj, joined)) = self.join_segment_head(h, payload) {
            return self.write_segment(&hj, &joined);
        }
        if !h.is_init
            && !h.init_cont
            && is_segment_head(payload)
            && self.hold_segment_head(h, payload)
        {
            return Ok(());
        }
        let dir = self.dir.clone();
        if !self.ensure_track(h.sb)? {
            return Ok(());
        }
        let others_pending = self.pending_cost_except(h.sb);
        let track = self.tracks.get_mut(&h.sb).expect("present");
        // An untimed chunk continues the init when the extension says so, or
        // while the pending init is still incomplete. After a complete init it
        // is media (a segment cut inside its moof), not init bytes.
        let continuation = !h.is_init
            && h.tfdt.is_none()
            && (h.init_cont
                || (!track.pending.is_empty()
                    && !track.pending_done
                    && !opens_like_media(payload)));

        if payload_encrypted(h, payload, continuation, track.init_kind) {
            self.refuse_encrypted(h.sb);
            return Ok(());
        }
        if track.refused && !h.is_init {
            track.skipped += 1;
            return Ok(());
        }

        if h.is_init {
            self.flush_init(h.sb)?;
            let track = self.tracks.get_mut(&h.sb).expect("present");
            // A clear init may end an encrypted stretch. The refusal is only
            // lifted once that init is complete and clear (`flush_init`):
            // its first chunk alone says nothing about the rest.
            track.pending_after_refusal = std::mem::take(&mut track.refused);
            track.init_kind = h.container();
            track.push_pending(h, payload);
            if track.pending_cost() > MAX_PENDING_INIT
                || others_pending + track.pending_cost() > MAX_PENDING_SESSION
            {
                self.flush_init(h.sb)?;
            }
            return Ok(());
        }
        if continuation && !track.pending.is_empty() {
            let cost = track.pending_cost() + payload.len() + PENDING_CHUNK_COST;
            if cost <= MAX_PENDING_INIT && others_pending + cost <= MAX_PENDING_SESSION {
                track.push_pending(h, payload);
                return Ok(());
            }
            // Too large to hold: decide on what came so far, write the rest
            // as init bytes of that part. Only when that init was written:
            // the rest of a duplicate or a switch back would land as a stray
            // init row after the part's media.
            if !self.flush_init(h.sb)? {
                // A new part file over the budget stopped the session: that
                // init and its rest are skipped, not duplicates.
                let stopped = self.stopped;
                let track = self.tracks.get_mut(&h.sb).expect("present");
                if track.refused || track.parts_full || stopped {
                    track.skipped += 1;
                } else {
                    // Its further untimed chunks are dropped too.
                    track.skip_tail = true;
                    track.dupes += 1;
                }
                return Ok(());
            }
            return self.write_row(h.sb, h, payload, true);
        }
        self.flush_init(h.sb)?;
        let track = self.tracks.get_mut(&h.sb).expect("present");
        // The pending init was refused (encrypted as a whole, or incomplete
        // after a refusal) or is beyond the part limit.
        if track.refused || track.parts_full {
            track.skipped += 1;
            return Ok(());
        }

        let seen_room = self.seen_total < MAX_SEEN_PER_SESSION;
        if !seen_room && !self.seen_limit_logged {
            eprintln!(
                "{}: {MAX_SEEN_PER_SESSION} media segments in this session; not taking more",
                dir.display()
            );
            self.seen_limit_logged = true;
        }
        let track = self.tracks.get_mut(&h.sb).expect("present");
        match track.admit_media(h, payload, seen_room) {
            Admit::Skip => Ok(()),
            Admit::Mark { offset } => self.write_mark(h.sb, h, offset, payload.len()),
            Admit::Write { as_init } => {
                if h.tfdt.is_some() || starts_a_numbered_moof(payload) {
                    self.seen_total += 1;
                }
                self.write_row(h.sb, h, payload, as_init)
            }
        }
    }

    /// Writes the index line of a sequence-mode duplicate (`Admit::Mark`):
    /// the bytes of the row at `offset`, appended again at this point.
    pub(crate) fn write_mark(
        &mut self,
        sb: u32,
        h: &SegmentHeader,
        offset: u64,
        len: usize,
    ) -> io::Result<()> {
        let track = self.tracks.get_mut(&sb).expect("present");
        let line = IndexLine {
            sb,
            ms: h.ms,
            part: track.part,
            seq: h.seq,
            offset,
            len,
            is_init: false,
            tfdt: h.tfdt,
            track: h.track,
            timescale: h.timescale,
            dur: h.dur,
            tso: h.tso.filter(|v| v.is_finite()),
            seq_mode: true,
            tso_set: h.tso_set,
            mark: true,
            mime: &h.mime,
            kind: &h.kind,
        };
        let mut line_buf = serde_json::to_vec(&line)?;
        line_buf.push(b'\n');
        if !self.within_budget(line_buf.len() as u64) {
            return Ok(());
        }
        self.index.write_all(&line_buf)
    }

    pub(crate) fn summary(&self) {
        let mut ids: Vec<_> = self.tracks.keys().copied().collect();
        ids.sort_unstable();
        for id in ids {
            let t = &self.tracks[&id];
            println!(
                "{}: sb{id} parts={} segments={} dupes={} skipped={} bytes={}",
                self.dir.display(),
                t.parts,
                t.segments,
                t.dupes,
                t.skipped,
                t.bytes
            );
        }
    }
}
