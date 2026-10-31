# desktop: streamtap-sink (spike 2)

A small Rust binary that receives the segments the extension taps, writes them
to disk and remuxes them with ffmpeg. It is the seed of the later desktop app;
for now it is a CLI with two subcommands.

## Build

```
..\scripts\build.ps1        # or: cargo build --release --locked
cargo test --release         # unit tests of dedupe, ordering, limits, DRM refusal
```

`scripts/build.ps1` (repo root) is what the nvim-config's `:MyNative build`
runs: it refuses while a sink from this checkout is running (the exe is
locked) and gives the exe the current time after a successful build, so a
no-op build counts as current.

Workspace lints: `unsafe_code = "forbid"`, clippy `all` as `deny`, `pedantic`
as `warn` (clean, tests included), rustfmt with Unix newlines.

## Run

```
target/release/streamtap-sink serve --token <secret> [--port 17650] [--out captures]
    [--allow-origin chrome-extension://oggpddakdbffbjhkjmcakidgmhpgakia]
    [--max-session-gib 16] [--max-total-gib 64]
```

- Binds **127.0.0.1 only**; there is no flag to bind anything else.
- The handshake's `Origin` must equal the allowed origin exactly (default:
  the streamtap extension, whose id the manifest `key` fixes). A web page,
  another extension or a client without `Origin` gets `403` before anything
  is written.
- `--token` is required and must not be empty or blank (an unset variable
  in `--token "$VAR"` would let any local process in). The extension sends
  the same value in its first message (`hello`); a wrong token closes the
  socket before anything is written. Set it in the extension popup (port +
  token). A flag without a value, or followed by another flag
  (`--token --port 1`), is a usage error (exit 2) before anything is bound.
- Every connection becomes one capture directory
  `captures/<unix-seconds>_<session-prefix>[-n]/` (a suffix when the name is
  taken, e.g. a reconnect within the same second) with `session.json` (page
  URL from the browser, title, plus `state` and `sink_pid`; written atomically,
  see "Connection state" below), `sbN.pM.bin` (one file
  per SourceBuffer `N` and init-segment part `M`) and `index.jsonl` (one line
  per written segment, written in one call: offset, length, MediaSource,
  `tfdt`, timescale, duration, `timestampOffset`, sequence mode, MIME). The
  extension opens one connection per MediaSource, so one directory is one
  video.
- Limits: 64 KiB JSON header, 64 MiB per message and per WebSocket frame
  (larger close the connection), 32 SourceBuffers per session, 64 part files
  per SourceBuffer, 400 000 media segments per session. Disk: the directory,
  each part file (4 KiB), payload and index lines count against 16 GiB per
  session (`--max-session-gib`) and 64 GiB per sink process
  (`--max-total-gib`); when either is reached the session stops for good
  (logged once). The directory is charged before it is created: once the
  process budget cannot hold another one, a new connection is closed without
  creating anything. Empty payloads are not written. A
  malformed message after `hello` is skipped and logged (a page can forge the
  hook's records); more than 100 close the connection. Before `hello` any bad
  message closes it. Every way a connection ends prints the per-SourceBuffer
  summary. Page strings (URL, MIME) are printed as printable ASCII only.
- **DRM:** a segment that carries protection is not written, nor anything
  after it on that SourceBuffer until a complete, clear init segment arrives.
  Init bytes (an init and its continuation chunks) are scanned for `encv`/
  `enca`/`tenc`/`pssh` (MP4) or the `ContentEncodings`, `ContentEncryption`
  and `ContentEncKeyID` IDs together (WebM): each chunk as it comes, and the
  whole init once more when it is decided, so markers split across appends
  count too. That whole-init scan covers what is held of an init, up to
  4 MiB; chunks of a larger init beyond that are scanned one at a time
  (real inits carry their markers in a `moov` or `Tracks` well below that). An init after a refusal that is still incomplete when it is
  decided (media came, or the connection ended) keeps the track refused.
  Media is only read as structure: a top-level `moov` or `pssh`, sample
  encryption (`senc` or PIFF) or auxiliary info (`saiz`/`saio`, CENC
  without `senc`) in a `moof`; random bytes in media never
  count. The extension does not send protected
  segments in the first place; this is the second line.
- **DRM across reconnects:** a reconnect opens a new session that may never
  see the encrypted init again, and WebM media carries no marker of its own.
  The sink process therefore remembers each refused track by the extension's
  session id (one per MediaSource, the same on every socket) and
  SourceBuffer: on the next connection that track stays refused until a
  complete, clear init is decided, which lifts it for later connections too
  (its first chunk alone does not). The list lives in
  memory (at most 4096 tracks, oldest forgotten first); a client that sends
  no session id gets no such memory.

### Dedupe and parts

- An init appended together with media (video.js VHS) is stored as two rows:
  the init part and the media part. The init part ends at the first media box
  (`styp`, `sidx`, `emsg`, `prft`, `moof`, `mdat`; a `sidx` differs per
  segment and must not make every combined append a new init), but only with
  a box after it that only a media segment has (`styp`, `emsg`, `prft`,
  `moof`, `mdat`; also when it is cut off, or the append ends right after
  it): an init+index range of a single-file stream (`ftyp moov sidx`) stays
  one init. The init is
  decided right there, so the media part never counts as its continuation
  (also without a `tfdt`). Boxes in front of the samples appended without
  them (a media part that ends after its `styp`, or an append of complete
  `styp`/`sidx`/`ssix`/`emsg`/`prft` boxes only; at most 64 KiB, and not
  while an init is still incomplete) are not a row of their own: they go in
  front of the next media append of that SourceBuffer, which also takes
  their append's `tsoSet`. Only the append right after them (`seq` + 1)
  takes them; an init or a missing append in between drops them, and next
  to samples that already fill a message (64 MiB) their bytes are left
  out.
- An init is decided once it is complete: its chunks (an init split across
  appends: chunks the extension flags `initCont`, or untimed chunks while the
  init has no complete `moov` / `Tracks` yet) are held, up to 4 MiB with
  512 bytes counted per chunk for its header, until
  the next media, then compared as a whole. After a complete init an untimed
  chunk (a segment cut inside its `moof`) is media. A chunk that no longer
  fits decides the init on what came so far and is written as its init
  bytes; when that init was a duplicate or a switch back, the chunk and the
  untimed chunks after it are dropped instead.
- A repeated, identical init is skipped. A **different** init (codec or
  quality change, `changeType`) starts a new part file `sbN.p(M+1).bin`; a
  switch **back** to an init seen before continues that earlier part. The
  first init when media came before it (the hook attached late) starts a new
  part too: that media stays alone in part 0. Beyond 64 parts a new init's
  media is skipped until a known init comes back (a page can forge inits).
- A media segment whose (`tfdt`, `timestampOffset`, duration) was already
  written for that part is skipped (players re-fetch ranges after seeks). The
  offset matters: multi-period DASH and HLS discontinuities restart `tfdt` and
  move the timeline with `timestampOffset` instead. In sequence mode
  (`seqMode`) the browser places media itself and a new period may reuse
  `tfdt` values, so the key there is the payload (hash and length): a
  re-append after a seek is dropped, a new period kept.
- The untimed continuation chunks (no `moof`) of a skipped duplicate are
  skipped too, or they would land as orphan bytes.

## Mux

```
target/release/streamtap-sink mux captures/<dir> [--out file.mp4] [--timeout-secs 3600]
```

Runs `ffmpeg -nostats -loglevel error -f mp4 -protocol_whitelist file [-itsoffset <s>] -i <part> … -map 0 -map 1 … -c copy -movflags +faststart`.

- **ffmpeg and ffprobe as children:** started without a shell, with an
  empty environment plus `PATH`, `PATHEXT`, `SystemRoot`, `windir`, `TEMP`,
  `TMP`, `TMPDIR`, `HOME`, `USERPROFILE`, stdin null and no console window.
  One deadline covers exit and pipes: `ffmpeg -version` 20 s, ffprobe 60 s
  per part, the mux `--timeout-secs` (default 3600); then the child is
  killed. stdout is capped at 64 KiB (more kills the child); stderr is read
  to its end and only its last 64 KiB are kept, so a long mux that logs many
  errors is not killed for it; a failure quotes the last 4 KiB of stderr.
  ffmpeg writes to `out.part.mp4` next to the result and the file is renamed
  to `out.mp4` only when ffmpeg succeeded: a killed or failed mux leaves no
  truncated file and an older result in place. ffmpeg logs errors only,
  and what it reports on success is printed after the run.
- **Version and missing tools:** `mux` first runs `ffmpeg -version` and
  prints the version line (`mux: ffmpeg version …`); without ffmpeg it stops
  before any work with "ffmpeg not found on PATH". A missing ffprobe is a
  warning, said once (also when it exists but cannot be started): the parts are then not checked for decodability and
  are aligned by the capture's own timestamps (without B-frame offsets and
  edit lists). ffprobe timing out on a part is a warning for that part.
- **Exit codes:** `0` ok, `1` file or socket error, `2` usage, `3` the capture
  holds nothing to mux, `4` ffmpeg missing or not startable (a missing ffprobe is only a warning), `5` a tool
  timed out or exceeded its output limit, `6` ffmpeg failed (its stderr tail is
  printed).

- **Inputs are checked and forced:** a part must start like its kind (`ftyp`,
  `moov` or `styp` for MP4, the EBML magic for WebM) and ffmpeg gets the
  demuxer explicitly (`-f mp4` / `-f matroska`) with files only. The bytes come
  from the page; probing would follow whatever they claim to be (an MPD or a
  playlist pulls in other files). A part that fails the check, whose MP4 init
  has no complete `moov`, or in which ffprobe finds no audio or video stream,
  is skipped instead of failing the whole mux.
- **Alignment:** ffmpeg moves every input to start at 0. Each input gets
  `-itsoffset` = its first presentation time (from ffprobe, so B-frame
  composition offsets count) minus the earliest one: a video part that
  starts later (after a quality switch) stays in sync with the audio.
- **Which MediaSource:** one MediaSource is one video. A preroll ad, the next
  video of a single-page app or a hover preview add more to the same
  session; `mux` takes the one with the most bytes and names the others.
- **Which part:** per SourceBuffer the part with an init segment and the
  most bytes. Adaptive players often start at a low quality and switch after
  the first segment, so part 0 can be a single segment. When that part
  cannot be muxed (its init is cut, it does not start like media, ffprobe
  cannot decode it, or a fallback part cannot be read), the next one by
  size is used, which is said; at most 3 after the largest, since each try
  may cost a sorted copy and an ffprobe run. The init is checked (complete
  `moov`, starts like media) before anything is sorted, and the sorted copy
  of a part that is left out is removed. A fallback part that fails only
  when the parts are laid out again (see below) leaves its SourceBuffer out,
  which is said; no further part of it is tried then. The other parts are
  named in a warning and left out (spike limit).
- **Order:** the sink writes in arrival order. Unless the part already is
  init-first and in timeline order, `mux` writes `sbN.pM.sorted.bin`: the
  init rows first (only its first run of them: an init row after media is a
  stray piece of another copy and left out, which is said), then media
  segments by presentation time (`tfdt` plus
  `timestampOffset` in ticks, computed exactly). Segments moved by a
  `timestampOffset` get their `tfdt` rewritten so the timeline stays
  monotonic; in a SourceBuffer with several tracks each `traf` is moved in
  its own track's timescale (from the init's `tkhd`/`mdhd`). With durations
  on all segments, one that earlier ones fully cover (an LL-HLS full segment
  after its parts) is left out; partial overlaps are kept and counted (frames
  may repeat there). Without durations that check cannot run, which is
  warned. Without a `tfdt` on every media segment (WebM blocks, continuation
  chunks, a segment cut inside its `moof`) the arrival order is kept, and
  re-fetches cannot be told apart there.
- **Sequence mode:** `mux` places each append where the browser placed it
  (MSE coded frame processing), per SourceBuffer over all its parts in
  arrival order, so an ABR switch and back keeps one timeline. After the
  player set `timestampOffset` (`tsoSet`, a seek or a resync as Shaka does
  it) the append's first presentation time goes to that offset (its `tfdt`
  plus the first sample's composition offset, minus the edit list's
  `media_time`); otherwise it starts at its `tfdt` plus the
  offset in effect, unless that does not continue within 100 ms of where the
  previous append ended (a discontinuity, a new period restarting `tfdt`):
  then right there, as the browser does, and these are counted. A seek
  without a set offset therefore plays after what came before it, as in the
  browser; a seek with one keeps its place and its gap is reported. A
  re-appended segment the sink did not store again (same bytes) still holds
  its place: the sink writes an index line that points at the stored copy
  (`mark`), so what follows it continues there; the copy itself plays once,
  at its first place. Moved segments get their `tfdt` rewritten. Edit lists
  with an empty first edit and signed composition offsets in version-0
  `trun` boxes are read as the browser reads them. A segment without `tfdt`, duration or
  timescale ends the group (the next one is not checked against it); a part
  with such a segment keeps its arrival order, which is warned. When a frame
  would decode before 0 (B-frames at a set offset, a negative
  `timestampOffset`), every SourceBuffer is moved later by the same amount,
  which is said, so they stay aligned. Only the parts that are muxed count
  for that amount (not one left out for a missing `moov`, bytes that do not
  start like media, one ffprobe cannot decode, or a fallback that cannot be
  read: then the parts are laid out again without it, until the amount
  holds, which is said); a part whose bytes cannot be moved (WebM,
  several tracks without per-track timescales) is aligned as if they had
  been. Index lines of duplicates (`mark`) never count as bytes, also not
  when picking the main MediaSource. The first append keeps its own time (the browser may start
  it at 0; the alignment between SourceBuffers is relative anyway).
- **Gaps:** ranges skipped by a forward seek are reported. With durations on
  every segment they are exact: a start past the end of everything before it
  by more than 50 ms or 1 % of the typical (lower median) segment duration,
  whichever is larger. Without durations a step well above
  the typical step (the lower median) counts, with an approximate length;
  there, gaps that make up most of the steps (a capture sampled by seeking
  again and again) are not found.
- **Lost inits:** when the extension had to drop an init (its queue was
  full), or a new connection cannot replay one (its kept copy was dropped to
  stay within the kept-init budget, where the tab and frame keeping the most
  lose first, or it could not be kept at all), it takes nothing more of that
  SourceBuffer until the next
  init and sends `initlost`; the sink notes it in `events.jsonl` and `mux` says that
  the media of that SourceBuffer between the lost init and its next init is
  not captured.
- A bad `index.jsonl` line (an interrupted sink) is skipped, not fatal.
- A SourceBuffer without any init segment is skipped (capture started after
  the player had appended it; the extension avoids this by running at
  `document_start`, and resends the init in effect, with its continuation
  chunks, on every new connection).

## Wire format

Each WebSocket **binary** message:

```
u32 little-endian header length | JSON header | payload bytes
```

Headers:

| `type` | fields | payload |
|---|---|---|
| `hello` | `token`, `session`, `url`, `title` | none; must be the first message |
| `segment` | `sb`, `seq`, `ms`, `mime`, `isInit`, `initCont`, `kind`, `tfdt` (number, or decimal string above 2^53), `timescale` (u32), `dur` (ticks of the first track), `tso` (seconds), `seqMode`, `tsoSet` (`timestampOffset` set since the previous append that carried samples; an init, or boxes in front of samples, appended without samples do not use it up) | the segment bytes exactly as passed to `appendBuffer` |
| `event` | `name` (`endofstream`, `changetype`, `initlost`), `sb` | none |
## Test without a browser

`cargo test --release` runs the unit tests (dedupe, split inits, ABR
switch-back, parts, DRM refusal incl. PIFF, split inits and reconnects,
budgets,
ordering, shifting, overlap, input checks, alignment arguments, printable
output).

`scripts/test-sink.mjs` splits a fragmented MP4 into init + `moof`/`mdat`
segments and streams them like the extension would, including one duplicate
init and one duplicate media segment:

```
ffmpeg -f lavfi -i testsrc2=size=640x360:rate=25 -f lavfi -i sine=frequency=440:sample_rate=48000 \
  -t 6 -c:v libx264 -g 25 -c:a aac -movflags frag_keyframe+empty_moov+default_base_moof \
  -frag_duration 1000000 test-frag.mp4
node scripts/test-sink.mjs test-frag.mp4 --token <secret>
target/release/streamtap-sink mux captures/<dir>
ffprobe captures/<dir>/out.mp4
```

`scripts/test-sink-link.mjs <file> --token <t> --captures <sink --out dir>`
runs the extension's service-worker link (`extension/src/sink-link.js`) in
Node with a mocked `chrome` against a running sink and checks what the sink
wrote: a reconnect (the second directory starts with the replayed init), a
first socket that fails (the dropped init still arrives), a quality switch
during a reconnect (old init before old media, new init before new media),
a split init (both chunks replayed), and the worker-wide link limit (a
parked 17th port connects once a slot frees up).
Without arguments, `node scripts/test-sink-link.mjs` runs only its mock
scenarios against a fake WebSocket (e.g. inits dropped on an open, a
connecting or a failed socket, lost inits, a port that closes while its
socket connects; the full list is in the script's header comment); they
need no sink.
Both clients send the extension origin (`--origin` to change it).
`../extension/scripts/test-hook.mjs` checks the page hook in Node (DRM
marking and EME stop, inits split anywhere incl. WebM Tracks, no false DRM
from mdat bytes, tap switch, `timestampOffset`, first `tfdt`, 64-bit
`tfdt`, sequence mode, rejected appends, WebM TimecodeScale, corrupt WebM).

Result on 2026-10-30: 8 segments written, 2 duplicates dropped, `mux: ok`,
ffprobe reports h264 640×360 + aac 48 kHz, 6.08 s.

First real-browser run (Chrome, ARD Mediathek, 2026-10-31): one session,
H.264 1280×720 + AAC, 630 s after reordering, full decode without errors.

## Deferred from the NEW_PROJECT gate (rules-vault, native)

Done: loopback bind, exact origin check in the handshake, token before any
write, hard size limits, bounded tracks and bad messages, atomic
`session.json`, unit tests of the storage and mux logic, workspace lints, no
shell for the ffmpeg child, child-process frame for ffmpeg/ffprobe (deadline,
empty environment, byte limits, exit status and stderr checked: `RS-04`,
`RS-06`, `ENG-01`..`ENG-03`), error enum with one exit code per class
(`RS-05`), a missing ffprobe is reported (`ENG-13`), no heredocs for docs,
LICENSE.
Not yet (spike): `deny.toml` / `cargo audit` in CI, Windows CI runner (repo is
private), security tests as raw TCP bytes against the running server (`TST-01`),
read and handshake timeouts, the token on the command line (visible to other
processes; pairing replaces it), the core/sink/testkit split, the Tauri
shell and UI. The security tests, CI and the split come first, before new
features.

# desktop: streamtap-app

A Tauri 2 desktop app (`crates/streamtap-app`) around the sink. The sink stays
a separate process: the app starts and stops it, reads its capture directory
and runs `streamtap-sink mux` on request. The UI is plain static HTML, CSS and
JS in `crates/streamtap-app/ui/` (no npm, no bundler, no network).

```
cargo run -p streamtap-app            # development (needs the sink built: cargo build --workspace)
..\scripts\build.ps1                  # release build of both exes
target\release\streamtap-app.exe
```

The app finds the sink next to its own exe. A debug build (`cargo run`) also
looks in `target/release` and `target/debug` of the cargo target dir; a
release build never runs a program from another directory. Running it needs WebView2 (part of
Windows 11); building it needs nothing extra.

What it does:

- **Header:** sink state (stopped, running, crashed with its exit code), a
  Start/Stop button, and a Settings panel with port, captures folder, the
  token (masked, Show, Copy, New token), whether ffmpeg is on `PATH` and which
  sink exe is used. Paste port and token into the extension popup. Settings
  are locked while the sink runs. They live in `config.json` in the app config
  dir (`%APPDATA%\io.github.wkdsteve.streamtap`); the token is generated on
  first run. A config that does not parse is not overwritten: defaults run and
  the UI says so.
- **Captures:** every capture directory in the captures folder (named like the
  sink names them, `<unix seconds>_<session>[-n]`; other folders are ignored),
  newest first, refreshed
  every 2 s: title, page host, start time, duration, size, segments,
  SourceBuffers (kind, MIME, size), and badges: `live` (the connection that
  writes the capture is open; exact, a paused video stays `live`, see
  "Connection state"), `DRM refused` (see below), `muxed` (an
  `out.mp4` exists). `index.jsonl` is read incrementally (only the lines
  appended since the last poll).
- **Per capture:** Mux (runs `streamtap-sink mux <dir>`, shows running, ok or
  failed with the exit code's meaning (0..6, as in the Mux section) and the last
  4 KiB of stderr), Open result, Open folder, Delete (asks first).
- **Empty and error states** say what to do: sink stopped, waiting for the
  extension (copy the token, set it in the popup), sink missing, ffmpeg
  missing, sink crashed (often the port is taken), captures folder unreadable.
- **DRM badge:** the sink only logs refusals to stderr. The app reads that log
  and writes `app-drm.flag` into the capture, so the badge survives a restart.
  A refusal logged before the app started the sink is not seen.

Security:

- The webview may call the app's own ten commands and nothing else
  (`build.rs` + `capabilities/main.json`; no fs, shell or opener plugin). CSP is
  `default-src 'none'` with scripts, styles and images from the app only and
  IPC as the one connection; the UI sets every disk or page string with
  `textContent`; titles and URLs from the page are also cut to a bounded
  length and stripped of control and bidirectional characters on the Rust side.
- Commands take a capture **id** (the directory name), never a path. An id has
  the sink's shape `<digits>_<[A-Za-z0-9_-]>`, so an ordinary subfolder
  (`src`, `Documents`) of a captures folder that was pointed at the wrong place
  is no capture and cannot be deleted; it is joined to the
  captures folder, must be a real directory (no symlink or junction) and its
  canonical form must lie directly inside the canonical captures folder.
  Delete additionally refuses a live capture or one that is being muxed.
- Children are started without a shell and, on Windows, without a console
  window (`CREATE_NO_WINDOW`). Open result / Open folder start `explorer.exe`
  with the path as one argument. The sink binds 127.0.0.1 only (its own rule).
- The token is random per install (std `RandomState`, seeded by the OS, mixed
  with time and pid), is never logged (sink output is scrubbed of it) and is
  passed to the sink on its command line, like for a hand-started sink: other
  local processes of the same user can read it there. That limit stays until
  the sink takes it another way (pairing).

- **Connection state:** the sink writes `"state":"open"` and its own `sink_pid`
  into `session.json` when a connection starts and rewrites it atomically with
  `"state":"ended"` and `"ended"` (unix seconds) when the connection is over
  (end of stream, closed socket, write error, even a panic: it runs in `Drop`).
  The app lists a capture as `live` only while it is `open`, written by the
  sink process the app runs (`sink_pid`) and that sink is running. A sink that
  was killed hard cannot write the end; its captures stay `open` on disk but
  are not live, because that process is gone. Windows reuses process ids, so a
  capture must also have begun after the app started the sink it belongs to
  (`started` is not older than that start). When the rename that writes the end
  fails (a virus scanner holding the file), it is tried again and then
  replaced in place: not atomic, but the capture is not left `open`. Captures of older sinks have no
  `state`: for those the app keeps the old rule (live = the sink runs and wrote
  into the capture within 10 s). Readers that do not know the fields ignore them.
- **Children die with the app:** on Windows the app puts the sink and every
  mux (with the ffmpeg it starts) into a job object with kill-on-close
  (`win32job` crate, a safe wrapper; this crate stays `unsafe`-free). Windows
  ends them when the app exits, also when the app is killed hard
  (Task Manager, crash). On a normal exit the app stops the sink and aborts
  running muxes: the process tree is killed and the half-written
  `out.part.mp4` is removed (a mux killed by the OS after a hard kill of the
  app cannot clean it up; the next mux overwrites it). If the job cannot be set
  up, the sink log says so. Other platforms have no such guard (Windows-only
  for now).

Limits: a hard kill of the app leaves a stale `out.part.mp4` in a capture
whose mux was running (harmless, replaced by the next mux).

Checks in CI and locally: the unit tests cover the capture scanner (incremental
index reading, partial lines, replaced files, state flags), id and path
validation (traversal, separators, drive syntax, junction), settings and token,
DRM line parsing, exit-code mapping and the log scrubbing. Two tests start the
real sink: one sends an encrypted init (`encv`/`tenc`/`pssh`) over a
WebSocket and checks that the app's scanner flags the capture `DRM refused`
and that `live` ends with the connection; the other kills an owner process and
checks that its job-tied child is gone. `cargo deny` checks the Windows target
graph only (`targets` in `deny.toml`).

Linux and macOS: the project is Windows-only for now and has no Linux CI. Two
advisories only exist in Tauri's Linux backend (GTK3): RUSTSEC-2024-0370
(`proc-macro-error`, unmaintained, build-time) and RUSTSEC-2024-0429 (`glib`
unsoundness). `cargo deny` never sees them (Windows graph); `cargo audit`
scans the whole `Cargo.lock` and ignores them in `.cargo/audit.toml`. A Linux
build needs a decision first (newer Tauri/GTK4 backend), not the ignore.