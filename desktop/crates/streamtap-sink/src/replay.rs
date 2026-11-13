//! `replay`: a recording the extension saved on a phone (target "file on the device",
//! `extension/src/file-target.js`) into a capture directory, through the same session code a live connection
//! takes (`Session::write_segment`, `note_lost_init`): DRM refusal, init assembly, duplicates and limits are the
//! sink's, never the phone's. `mux` then makes the video as for a live capture.
//!
//! A recording is the line `STREAMTAP-RECORDING/1\n`, then per message a 32-bit little-endian length and the message
//! as the sink takes it over the WebSocket (`split_message`). The first message is the `hello`; its token is not
//! checked (the file is the person's own), everything after it is handled like a live connection's.

use super::*;

/// The first line of a recording.
pub(crate) const RECORDING_MAGIC: &[u8] = b"STREAMTAP-RECORDING/1\n";

/// What `replay` made of a recording: the capture directory and how many messages were skipped.
#[derive(Debug)]
pub(crate) struct Replayed {
    pub(crate) dir: PathBuf,
    pub(crate) bad: u32,
    /// The recording ends inside a message (a save cut short): what came before is kept.
    pub(crate) cut: bool,
}

/// One message of the recording, or `None` at its end (`Some(Err)` for a cut or oversized one).
fn next_message(r: &mut impl Read) -> Option<Result<Vec<u8>, String>> {
    let mut len = [0u8; 4];
    match r.read_exact(&mut len) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return None,
        Err(e) => return Some(Err(e.to_string())),
    }
    let len = u32::from_le_bytes(len) as usize;
    if len > MAX_MESSAGE {
        return Some(Err(format!("message of {len} bytes exceeds the limit")));
    }
    let mut data = vec![0u8; len];
    match r.read_exact(&mut data) {
        Ok(()) => Some(Ok(data)),
        Err(e) => Some(Err(e.to_string())),
    }
}

/// Replays the recording `r` into a new capture directory under `out`.
pub(crate) fn replay_from(
    mut r: impl Read,
    out: &Path,
    budget: &Arc<Budget>,
    refusals: &Arc<Refusals>,
) -> Result<Replayed, SinkError> {
    let mut magic = [0u8; RECORDING_MAGIC.len()];
    if r.read_exact(&mut magic).is_err() || magic != RECORDING_MAGIC {
        return Err(SinkError::Usage(
            "not a streamtap recording (it starts with STREAMTAP-RECORDING/1)".into(),
        ));
    }
    let mut session: Option<Session> = None;
    let mut bad = 0u32;
    let mut cut = false;
    while let Some(data) = next_message(&mut r) {
        let data = match data {
            Ok(d) => d,
            Err(e) => {
                eprintln!("recording ends inside a message ({e}); what came before is kept");
                cut = true;
                break;
            }
        };
        let (header, payload) = match split_message(&data) {
            Ok(x) => x,
            Err(e) if session.is_none() => {
                return Err(SinkError::Usage(format!(
                    "the recording does not start with a hello: {e}"
                )));
            }
            Err(e) => {
                bad += 1;
                eprintln!("bad message skipped ({bad}): {e}");
                if bad > MAX_BAD_MESSAGES {
                    eprintln!("too many bad messages, stopping");
                    break;
                }
                continue;
            }
        };
        match (header, session.as_mut()) {
            (
                Header::Hello {
                    session: id,
                    url,
                    title,
                    ..
                },
                None,
            ) => {
                let Some(s) = Session::open(out, &id, &url, &title, budget, refusals)
                    .map_err(|e| SinkError::io("open session", None, e))?
                else {
                    return Err(SinkError::Usage("--max-total-gib reached".into()));
                };
                println!("session {} ({})", s.dir.display(), printable(&url, 300));
                session = Some(s);
            }
            (Header::Hello { .. }, Some(_)) => eprintln!("second hello, ignoring"),
            (_, None) => {
                return Err(SinkError::Usage(
                    "the recording does not start with a hello".into(),
                ));
            }
            (Header::Segment(h), Some(s)) => s
                .write_segment(&h, payload)
                .map_err(|e| SinkError::io("write", Some(s.dir.clone()), e))?,
            (Header::Event { name, sb }, Some(s)) => {
                println!("{}", event_log_line(&s.dir, &name, sb));
                if let ("initlost", Some(sb)) = (name.as_str(), sb) {
                    s.note_lost_init(sb)
                        .map_err(|e| SinkError::io("write", Some(s.dir.clone()), e))?;
                }
                if name == "recordfull" {
                    eprintln!(
                        "{}: the phone's memory was full here: later media of this video is not in the recording",
                        s.dir.display()
                    );
                }
            }
        }
    }
    let Some(mut s) = session else {
        return Err(SinkError::Usage("the recording holds no hello".into()));
    };
    s.finish();
    s.summary();
    Ok(Replayed {
        dir: s.dir.clone(),
        bad,
        cut,
    })
}

pub(crate) fn run_replay(args: &[String]) -> Result<(), SinkError> {
    let Some(file) = args.first().filter(|a| !a.starts_with("--")) else {
        return Err(SinkError::Usage(
            "replay needs a recording: streamtap-sink replay <file.streamtap> [--out captures]"
                .into(),
        ));
    };
    let out = PathBuf::from(flag(args, "--out")?.unwrap_or("captures"));
    let path = PathBuf::from(file);
    let f = File::open(&path).map_err(|e| SinkError::io("cannot open", Some(path.clone()), e))?;
    fs::create_dir_all(&out).map_err(|e| SinkError::io("cannot create", Some(out.clone()), e))?;
    let budget = Arc::new(Budget {
        per_session: DEFAULT_MAX_SESSION_GIB << 30,
        total_max: DEFAULT_MAX_TOTAL_GIB << 30,
        total_used: AtomicU64::new(0),
    });
    let refusals = Arc::new(Refusals::default());
    let done = replay_from(io::BufReader::new(f), &out, &budget, &refusals)?;
    println!(
        "{}: replayed{}{}; next: streamtap-sink mux {}",
        done.dir.display(),
        if done.bad > 0 {
            format!(", {} bad message(s) skipped", done.bad)
        } else {
            String::new()
        },
        if done.cut {
            ", the recording was cut short"
        } else {
            ""
        },
        done.dir.display()
    );
    Ok(())
}
