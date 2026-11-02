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
no-op build counts as current. `scripts/build-install.ps1` builds the same way and
installs for the current user (see Install in the app section below).

Workspace lints: `unsafe_code = "forbid"`, clippy `all` as `deny`, `pedantic`
as `warn` (clean, tests included), rustfmt with Unix newlines.

## Run

```
target/release/streamtap-sink serve (--token-stdin | --token <secret>)
    [--port 17650] [--out captures]
    [--allow-origin chrome-extension://oggpddakdbffbjhkjmcakidgmhpgakia]
    [--max-session-gib 16] [--max-total-gib 64]
    [--hello-timeout-secs 10] [--idle-timeout-secs 3600] [--max-connections 16]
```

```
# by hand, without the token on the command line (PowerShell):
'<secret>' | target/release/streamtap-sink serve --token-stdin
```

- Binds **127.0.0.1 only**; there is no flag to bind anything else.
- The handshake's `Origin` must equal the allowed origin exactly (default:
  the streamtap extension, whose id the manifest `key` fixes). A web page,
  another extension or a client without `Origin` gets `403` before anything
  is written.
- A token is required, from exactly one of two places (both: usage error,
  exit 2):
  - `--token-stdin` (what the app uses): the sink reads the first line of
    its stdin before it binds, at most 4 KiB, with the LF or CRLF and a
    leading UTF-8 BOM removed (Windows PowerShell 5.1 adds one when
    `$OutputEncoding` is UTF-8); a last line without a line break counts when it
    is not empty, a longer line is refused, never cut. Anything after
    the first line is ignored, and stdin is not used again. Every other flag
    is checked first, so a bad flag ends the sink without waiting for stdin.
  - `--token <secret>`, for a quick manual run. **Warning:** a command line is
    readable by every other process of your user (Task Manager, `wmic`,
    `Get-CimInstance Win32_Process`), so the token is not secret from them
    while the sink runs. Prefer `--token-stdin`.

  The token must not be empty or blank (an unset variable in
  `--token "$VAR"`, an empty or closed stdin would let any local process in;
  exit 2), a longer line or one that is not UTF-8 is a usage error as well. The
  extension sends the same value in its first message (`hello`); a wrong token
  closes the socket before anything is written. The sink holds the token in a
  type without `PartialEq` whose `Debug` prints no value and compares it in
  constant time. A flag without a value, or followed by another flag
  (`--token --port 1`), is a usage error (exit 2) before anything is bound.
- Every connection becomes one capture directory
  `captures/<unix-seconds>_<session-prefix>[-n]/` (a suffix when the name is
  taken, e.g. a reconnect within the same second) with `session.json` (page
  URL from the browser, title, plus `state` and `sink_pid`; written atomically,
  see "Connection state" below), `sbN.pM.bin` (one file
  per SourceBuffer `N` and init-segment part `M`) and `index.jsonl` (one line
  per written segment, written in one call: offset, length, MediaSource,
  `tfdt`, the track ID it belongs to, timescale, duration, `timestampOffset`, sequence mode, MIME). The
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
  creating anything. Time and number: a connection has 10 s from accept to a
  valid `hello` (one deadline, trickled bytes do not restart it;
  `--hello-timeout-secs`), after it the sink tolerates an hour of silence
  (`--idle-timeout-secs`; a paused video sends nothing for minutes), and at
  most 16 connections are open at once, the next one is closed at once
  (`--max-connections`). Empty payloads are not written. A
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
- A media segment whose (`tfdt`, `timestampOffset`, duration, track ID) was already
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
target/release/streamtap-sink mux captures/<dir> [--out file.mp4] [--timeout-secs 3600] [--verbose]
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
  `mux --verbose` runs ffmpeg with `-loglevel warning` instead of `error`, so
  warnings such as "Non-monotonous DTS" appear in the `ffmpeg reported` tail
  (also bounded; the default stays quiet).
  ffmpeg writes to `out.part.mp4` next to the result and the file is renamed
  to `out.mp4` only when ffmpeg succeeded: a killed or failed mux leaves no
  truncated file and an older result in place. ffmpeg logs errors only,
  and what it reports on success is printed after the run.
- **Version and missing tools:** `mux` first runs `ffmpeg -version` and
  prints the version line (`mux: ffmpeg version …`); without ffmpeg it stops
  before any work with "ffmpeg not found on PATH". A missing ffprobe is a
  warning, said once (also when it exists but cannot be started): the parts are then not checked for decodability and
  are aligned by the capture's own timestamps (without B-frame offsets and
  edit lists). ffprobe timing out on a part is a warning for that part; after 3 timeouts in a row the remaining parts are not probed (one more warning).
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
- **Which part:** per SourceBuffer the best part with an init segment: first
  those whose bytes fit the media time their segments claim (between 100 B/s
  and 100 MB/s; padded or cut-off parts and inflated durations rank last),
  then the most claimed media time (per track, tracks are not added up), then
  the most bytes. Without durations only the bytes count. Adaptive players often start at a low quality and switch after
  the first segment, so part 0 can be a single segment. When that part
  cannot be muxed (its init is cut, it does not start like media, ffprobe
  cannot decode it, or a fallback part cannot be read), the next one in
  that order is used, which is said; at most 3 after the first, since each try
  may cost a sorted copy and an ffprobe run. The init is checked (complete
  `moov`, starts like media) before anything is sorted, and the sorted copy
  of a part that is left out is removed. A fallback part that fails only
  when the parts are laid out again (see below) is replaced by the next
  candidate of its SourceBuffer (within the same limit of 3 fallbacks); only
  when none is left, the SourceBuffer is left out, which is said. The other parts are
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
  after its parts) is left out, per track (the track ID of the append's first
  `traf` says which; gaps are reported per track too); partial overlaps are kept and counted (frames
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
| `segment` | `sb`, `seq`, `ms`, `mime`, `isInit`, `initCont`, `kind`, `tfdt` (number, or decimal string above 2^53), `timescale` (u32), `track` (track ID of the first fragment: the track `tfdt` and `dur` belong to; optional, older extensions send none), `dur` (ticks of the first track), `tso` (seconds), `seqMode`, `tsoSet` (`timestampOffset` set since the previous append that carried samples; an init, boxes in front of samples, a short head or a piece cut inside the head boxes, appended without samples do not use it up; the piece with the `moof`/`mdat` takes it) | the segment bytes exactly as passed to `appendBuffer` |
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

## Hardening: done and deferred

Done: loopback bind, exact origin check in the handshake, token before any
write, hard size limits, bounded tracks and bad messages, atomic
`session.json`, unit tests of the storage and mux logic, workspace lints, no
shell for the ffmpeg child, child-process frame for ffmpeg/ffprobe (deadline,
empty environment, byte limits, exit status and stderr checked), error enum
with one exit code per class, a missing ffprobe is reported, LICENSE.
Not yet (spike): `deny.toml` / `cargo audit` in CI, Windows CI runner, security
tests as raw TCP bytes against the running server,
read and handshake timeouts, the core/sink/testkit split, the Tauri
shell and UI. (The token on the command line is gone for the app: it uses
`--token-stdin`; `--token` stays for manual runs, see above.) The security tests, CI and the split come first, before new
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

## Install

Two ways to the same install, one installer. `scripts/build-install.ps1` (repo root) builds
with `scripts/build.ps1`, stages the build in the shape of the release zip
(`scripts/package-release.ps1 -NoZip`) and runs `scripts/install.ps1` on it; the release zip
(see "Release" below) carries that same `install.ps1`, run from the unpacked folder.
`scripts/uninstall.ps1` takes it away. Run them in a normal, not elevated PowerShell, never from
inside an MSIX-packaged app (a Claude session, a terminal it started): the package's container
redirects `%APPDATA%`, `%LOCALAPPDATA%` and `HKCU`, and everything would land only there. They
refuse when `CLAUDECODE` is set and when the session is elevated (`-AllowElevated` for a build
machine); `-BuildOnly` builds anywhere.

| What | Where | `uninstall.ps1` |
|---|---|---|
| `streamtap-app.exe`, `streamtap-sink.exe`, `streamtap-host.exe` | `%LOCALAPPDATA%\streamtap\bin` | removed (also from the root, where the earlier layout had them); `bin` only when empty |
| the extension, unpacked (`manifest.json`, `popup\`, `src\`; no tests, no README) | `%LOCALAPPDATA%\streamtap\extension` | removed with everything in it |
| a copy of `uninstall.ps1`, `LICENSE`, `README.md`, `VERSION` | `%LOCALAPPDATA%\streamtap` | removed; the folder only when nothing else is in it |
| Start menu entry `streamtap.lnk` | the user's Start menu programs folder | removed when it starts the installed app |
| *Settings > Apps* entry (runs the installed `uninstall.ps1` with the places of this install) | `HKCU\Software\Microsoft\Windows\CurrentVersion\Uninstall\streamtap` | removed last, when its `InstallLocation` is this folder |
| browser registrations (`NativeMessagingHosts`) | written by the app, not by the script | removed when the manifest names a host in the install folder; another copy's stays |
| settings, token, pairings, host manifests | `%APPDATA%\io.github.wkdsteve.streamtap` | kept; `-RemoveData` removes `config.json`, `pairings.json`, `bridge.json` and `native-messaging\*.json` |
| WebView cache | `%LOCALAPPDATA%\io.github.wkdsteve.streamtap\EBWebView` | kept; `-RemoveData` removes it |
| captures (default `captures\` in the settings folder) | wherever you set them | never touched |

The programs live in `bin\` (the layout the hub of the desktop apps looks for); the app finds the sink
and the host next to itself. An install of the earlier layout (programs directly in the install
folder) is moved to `bin\` by the next install, which names a browser that still starts the old host.
`install.ps1` refuses a folder that is not the shape of a release (a foreign file at its top or in
`bin\`, a missing program, a `VERSION` that is no version) before it writes anything.

Both scripts refuse while `streamtap-app`, `-sink` or `-host` runs from the install
folder and name the PIDs (`build.ps1` does the same for the checkout); the files are
first all copied to `.new`, then each old one is renamed to `.old` and its `.new` takes
its place. A copy or a rename that fails puts back what was swapped, so the install is
never a mix of an old and a new program; a file the new release no longer has in `extension\`
is removed after a successful swap; `uninstall.ps1` also removes `.new` and `.old`
files left behind. There is one *Settings > Apps* entry: an install into another folder
takes it over and names the install that lost it (its files stay). A second
`uninstall.ps1` says there is nothing to do; a file it cannot remove is an error, and the
*Settings > Apps* entry then stays for the next run. An empty install folder that cannot
go (a terminal whose current folder it is) stays with a note, the entry goes anyway.
`-RemoveData` while a streamtap app of another copy (the checkout's) runs: that app uses
the same settings folder and keeps its WebView cache open, so close every copy first.

**Extension.** Chrome loads it unpacked only (no store): `chrome://extensions`, *Developer mode*,
*Load unpacked*, `%LOCALAPPDATA%\streamtap\extension` (`install.ps1` ends by naming that folder).
The extension id stays the same (the manifest carries the public key), so the host's
`allowed_origins` fits. After an update press *Reload* on its card.

**ffmpeg** is a prerequisite and not part of the install (`scoop install ffmpeg` or ffmpeg on the
`PATH`); `install.ps1` says when it finds none, and the app shows "ffmpeg not found".

**Browsers after an install.** The app owns the registration (manifest, key,
rollback; see "Connect browser"), so the script does not write it. A browser
registered from the checkout keeps starting `desktop\target\release\streamtap-host.exe`;
`install.ps1` lists such browsers. Open the installed app from the Start menu,
*Connect browser*: it shows them as "registered for another location" (a manifest
that names another copy's host counts as stale), and *Register again* points them at
the installed host. (A non-interactive re-register flag in the app was not added: the
browser keeps working with the checkout's host until you press the button, and the
registration code stays in one place.)

A wrong `-InstallDir` costs nothing: `install.ps1` refuses a folder that holds files and no streamtap
install (no streamtap program, no `VERSION` with `uninstall.ps1`), and `uninstall.ps1` leaves such a
folder alone ("not an install of this program") instead of removing its `extension\`, `README.md`
or `LICENSE`.

Places for tests: `-InstallDir`, `-StartMenuDir`, `-RegistryRoot` (the key that stands
for `HKCU\Software`, below which the uninstall entry is written and the browser keys are
read), and for `uninstall.ps1` also `-DataDir`, `-LocalDataDir`; each defaults to
`STREAMTAP_INSTALL_DIR`, `STREAMTAP_START_MENU_DIR`, `STREAMTAP_REGISTRY_ROOT`,
`STREAMTAP_DATA_DIR`, `STREAMTAP_LOCAL_DATA_DIR` before the real place.
`build-install.ps1 -BuildDir <dir>` installs prebuilt exes without building;
`install.ps1 -PayloadDir <dir>` installs a staged or unpacked release folder.
`scripts/test-install.ps1` (CI, job `hygiene`) installs stand-ins (copies of `cmd.exe`)
into a temp folder with a throw-away registry root `HKCU:\Software\streamtap-test-install-<id>`
and checks files, shortcut, entry, the browser report, the refusals (Claude session, a
running sink, a locked file, a drive root, a registry root outside `HKCU`, a folder that is no
release), a copy and a rename that fail (all old again), the entry taken over by another
folder, the move from the earlier layout, uninstalling with the entry's command line as
Windows runs it (Windows PowerShell 5.1, places with a space and a non-ASCII letter, the
pause on an error), from inside the install folder, a second uninstall, and that
`-RemoveData` keeps captures and foreign files.

### Release

A release is a **zip with install scripts, not a setup.exe** (an unsigned setup is stopped by
SmartScreen like the programs, and the scripts already do the per-user install).
`scripts/package-release.ps1` packs `streamtap-<version>-x86_64-pc-windows-msvc.zip` from built
programs (`-BuildDir`, default `desktop\target\release`; it builds nothing unless `-Build`): `bin\` with
the three programs, `extension\`, `install.ps1`, `uninstall.ps1`, `LICENSE`, `README.md` (how to
install) and `VERSION`. The zip is reproducible (sorted entries, fixed timestamp): the same programs
give the same bytes when the same PowerShell/.NET packs them (the compressed bytes depend on the
runtime's deflate, so Windows PowerShell 5.1 and PowerShell 7 give different zips with the same
content). A `SHA256SUMS` with one line per zip of that folder is written next to it, and the version
must be the one of `streamtap-app/Cargo.toml`, `tauri.conf.json`, `streamtap-sink` and `streamtap-host`.
The script packs what is in the build folder and does not know whether it is fresh: build first. `-Rolling -Sha <commit>` packs the rolling build: the fixed name
`streamtap-windows-x86_64.zip` and the version label `<version>+<short commit>`.

`.github/workflows/release.yml` (tag `v*`): checks that the tag is the version of `Cargo.toml` and
`tauri.conf.json`, the commit is on `main`, its CI run is green and the release is not published
already; builds; packs; runs `scripts/test-release.ps1` on the zip; and makes a **draft** release
with the zip and `SHA256SUMS` through the publish action of appkit's release-kit. It never publishes;
a person opens the draft, installs from it and publishes it. `.github/workflows/rolling.yml`:
after a green `ci` on `main` the pre-release `latest` gets the zip of that commit (the tip of
`main` only) and `SHA256SUMS`, replaced each time. A newer build cancels an older one, but a running
publish is never cancelled (it would leave `latest` without a file); a publish that waited behind a newer one
publishes nothing when `main` has moved on. Both run on the self-hosted Windows runner
(the repository is private). The kit's own stamped `install.ps1` is not used (it needs a program
with an `install` command or a setup); `install.ps1` is in the zip. streamtap has no appkit crate,
so `tools/check-workflows.py` (CI, job `hygiene`; `--online` asks GitHub whether each action's tag
still names the pinned commit) takes the place of `check-appkit-pin.py`: every action is pinned by
a full commit with its tag in a comment (NREL-03), all of the kit's actions by the same one, every
job on `[self-hosted, windows]`, no pull request trigger, and a release stays a draft (NREL-07).
`scripts/test-release.ps1` (CI, job `hygiene`) packs stand-in programs and checks the zip's contents
(no tests, no keys, no README of the extension), checksums, reproducibility, the refusals (wrong
version, rolling without a commit, a key file or a file of an unexpected type in the extension, a
manifest naming a missing file), then installs the unpacked zip into temp places and uninstalls.

What it does:

- **Header:** sink state (stopped, running, crashed with its exit code), a
  Start/Stop button, and a Settings panel with port, captures folder, the
  token (masked, Show, Copy, New token), whether ffmpeg is on `PATH` and which
  sink exe is used. Paste port and token into the extension popup. Settings
  are locked while the sink runs. They live in `config.json` in the app config
  dir (`%APPDATA%\io.github.wkdsteve.streamtap`); the token is generated on
  first run. A config that does not parse is not overwritten: defaults run and
  the UI says so. When other accounts may read `config.json`, the panel warns
  (see Security).
- **Appearance** (in Settings): language *System* (default; German when the
  webview's language starts with `de`, else English), *Deutsch* or *English*,
  and theme *System* (default; follows the Windows light/dark mode live),
  *Light* or *Dark*. A change applies at once, without a reload and without
  losing open panels, open SourceBuffer tables, the pairing prompt or the delete
  dialog, and also while the sink runs (it is saved by its own command,
  `save_ui_prefs`, which accepts exactly `system`/`de`/`en` and
  `system`/`light`/`dark`). Both are stored as `language` and `theme` in
  `config.json`; a config without them (older app) loads with *System*, an
  unknown value there falls back to *System* without failing the rest of the
  file. While the stored config is unusable (see above) the choice applies to
  the running app only and is not written. All texts of the UI are translated
  (`ui/i18n.js`, one table per language, no dependency); error texts that come
  from the app's Rust side (failed commands, config, registry and bridge errors,
  the ffmpeg output) stay English, and page titles and URLs are shown as the
  page sent them. Mux failures carry a stable `reason` key next to the English
  `meaning`, so the UI shows the exit code's meaning in the chosen language.
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

- The webview may call the app's own sixteen commands and nothing else
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
  with time and pid) and is never logged (sink output is scrubbed of it). The
  app starts the sink as `serve --token-stdin --port <p> --out <dir>`, writes
  the token and a line break into the child's stdin and closes it, at every
  start and at the restart after a revoke; nothing token-related is on the
  command line. A sink that cannot take it is ended and the start fails.
- In the app the token (and the browser bridge key) is a `Token` type without
  `PartialEq`, whose `Debug` prints `Token(hidden)`; it is compared in constant
  time only. `config.json` keeps the token as a plain string under `token`, as
  before.
- `config.json` gets no ACL of its own: the rights of the user profile on
  `%APPDATA%` (you, SYSTEM, Administrators) are enough. At every start the app
  reads the file's DACL and checks that none of the broad groups *Everyone*
  (`S-1-1-0`), *Users* (`S-1-5-32-545`), *Authenticated Users* (`S-1-5-11`),
  *Interactive* (`S-1-5-4`), *Local* (`S-1-2-0`), *Guests* (`S-1-5-32-546`),
  *Domain Users* of any domain (`S-1-5-21-…-513`; without a domain the
  machine's *None* group, which holds every local account) or *All Application
  Packages* (`S-1-15-2-1`)
  may read it: an allow entry that grants reading the data, changing the DACL
  or taking ownership (the last two let the group grant itself reading;
  generic rights count by what they stand for), unless a deny of exactly that
  right for the group or for Everyone came before it. No DACL and a NULL DACL
  (`D:NO_ACCESS_CONTROL`) both mean everyone; group membership beyond that is
  not resolved, so the check errs on the side of a warning. If one may, the Settings panel shows a warning naming the groups
  (translated) and the app logs it (the sink output panel until the next sink
  start, and stderr); the fix is to remove those entries under *Properties >
  Security* of the file or its folder and restart the app. A DACL that cannot
  be read gives a warning as well. The check uses the `windows-permissions`
  crate (MIT, a safe wrapper; its `winapi` and `bitflags` 1 were already in the
  tree), so the workspace stays free of `unsafe`.

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
  up, the sink log says so (for a mux too, with the capture's id). Other platforms have no such guard (Windows-only
  for now).

## Connect browser (native messaging)

Instead of copying port and token into the extension popup, the app pairs with
the extension. Three parts:

- **`streamtap-host`** (`crates/streamtap-host`, its own small crate) is the
  native messaging host the browser starts. It is a separate console program,
  not the app with a flag, on purpose: the browser starts it on every
  `connectNative`, so it must start in milliseconds and without a webview; a
  Windows GUI-subsystem exe (the app) has no reliable stdio for the pipes; and
  it has no dependency but `serde`. It speaks the browser's framing on stdin and
  stdout (4 byte little-endian length + JSON) and a line protocol over a
  loopback socket to the app. The host knows **no secret of the sink** (only the
  bridge key, see below): it checks the origin
  the browser passes (only `chrome-extension://oggpddakdbffbjhkjmcakidgmhpgakia/`,
  else it answers `origin-not-allowed` and exits), forwards the extension's
  messages and hands the app's answers back. Port and token come only from the
  app, after you approved. (The task text said the host reads them from the app
  config; the app sends them instead, so that the approval is enforced in one
  place, the app, and the host needs no knowledge of the config file.)
- **The bridge** (`hub.rs`): when the app starts it listens on a random
  `127.0.0.1` port and writes `{"port":n,"key":"<64 hex>"}` into `bridge.json` in
  its config dir (`%APPDATA%\io.github.wkdsteve.streamtap`; temporary file and
  rename; removed on exit). The key is new at every app start (the same
  generator as the sink token). A host finds the app there and sends the key as
  the first line; the app reads at most 128 bytes for it within 2 s in total (a
  client that trickles in a byte now and then is cut off as well), compares in
  constant time and closes a connection without the right key before it takes
  one of the 16 connection slots or can raise a pairing prompt. So only a process
  that can read the app's config directory can talk to the bridge at all.
  Without the file or the app, the extension is told `app-not-running`.
- **Registration** (`registration.rs`): the *Connect browser* panel registers or
  removes the host for Chrome, Edge or Brave. That writes a manifest
  (`native-messaging/<browser>.json` in the config dir; `name`, `path` of
  `streamtap-host.exe` next to the app, `type: stdio`, `allowed_origins` with
  exactly the extension's origin, no wildcard) and points
  `HKCU\Software\<vendor>\<browser>\NativeMessagingHosts\io.github.wkdsteve.streamtap`
  at it (Chrome `Google\Chrome`, Edge `Microsoft\Edge`, Brave
  `BraveSoftware\Brave-Browser`). Per user, no administrator rights. Remove deletes
  key and manifest. The manifest is written to a temporary file and renamed before
  the key is set; when the key cannot be written, an earlier manifest is put back
  as it was (a working registration stays working). The host path is written in
  plain form (`\\?\C:\...` becomes `C:\...`, `\\?\UNC\server\share\...` becomes
  `\\server\share\...`), so the app may also live on a network share. A key that points elsewhere (an older install) shows as
  "registered for another location" with *Register again*. The registry access is
  the `winreg` crate (MIT, already in the tree through Tauri's build tooling)
  behind a trait: the tests use a fake, and one test uses a throw-away key under
  `HKCU\Software\streamtap-test-<pid>` that it removes, never the browsers' keys.
A key and manifest that are the app's but whose manifest names another host than
the one next to the running app (the checkout's, after an install) count as
"registered for another location" too.
  Start the app from Explorer or the Start menu when you register: an app
  launched by a packaged (MSIX) program, such as a terminal or agent inside
  one, inherits its container, and its HKCU key, manifest and `bridge.json`
  then exist only inside that package (`%LOCALAPPDATA%\Packages\<package>\LocalCache`,
  private registry hive). The app shows "registered", but the browser finds no
  host and never starts it (popup: "the connector is not registered").

Pairing: the extension connects and says hello (a random instance id it keeps per
browser profile, the browser it believes it is, its version). The app shows
*"Chrome (profile abcd1234) wants to pair"* with Approve and Deny. Only after
Approve it sends `credentials` (port and token) and the extension stores them. An
approval is kept in `pairings.json` (origin + instance, at most 32), so the same
profile pairs without a prompt next time and another profile asks again; the panel
lists the approvals and *Revoke* removes one after a confirmation (the extension is
told, drops the credentials it had been given and stops). Revoking also gives the
sink a **new token** (the same generator as *New token*), saves it and pushes it to
every browser that is still approved and connected; the revoked one was closed first
and gets nothing, so a copy it kept is useless at once. A running sink is restarted
with the new token, which ends captures in progress (the confirmation says so). A
browser set up by hand in the popup needs the new token there; the Settings panel
reads the token again after every revoke (also one that reports an error), so *Show*
and *Copy* give the new one. A new token or port
(Settings) is pushed to paired browsers. The browser name is what the extension reports (Edge and
Chrome are told apart by the browser's brands; Brave usually looks like Chrome); the
profile is the instance id, not the profile's name, which the protocol does not
show. An unknown origin is refused by the host and again by the app. A pairing the
user denies is asked again at the earliest after ten minutes (the extension's
rule) unless the user presses *pair now* in the popup.

Tabs: a paired and connected browser reports the tabs that hold a MediaSource
(title, host, icon, DRM flag, whether it is captured). The panel lists them with
*Capture* / *Stop*. Capture asks the extension to tap exactly that tab and reload it
once (a running stream has already appended its init segment); it needs a running
sink. A paired browser taps only chosen tabs; a browser set up by hand in the
popup taps every tab as before. No page previews: a screenshot of a tab needs
host access to all sites (or a click on the extension for that tab), which the
extension does not ask for; the icon comes from the browser's favicon service.

Schema and bounds (`proto.rs`, `framing.rs`): every message has `t` and `v: 1`,
unknown fields and types are refused; 512 KiB at most from the extension (16 KiB
to it), 32 tabs, titles cut to 200 characters with control and direction-override
characters removed, hosts only host-name characters, icons only small
`data:image/...;base64,` URLs whose bytes begin like the claimed type. A frame that
declares more than the limit is refused before anything is read. The token is
never printed (the `Token` type's `Debug` hides it; the tests check it).

Tests: schema and framing (unit), the host against a fake app on a real socket
(the key comes first), the bridge key (silent, wrong, missing and oversized keys get
no slot and no prompt; a key trickled in byte by byte is cut off after 2 s; a new
key per start), the hub (pending gets no credentials,
deny, remember, revoke, tap only for reported tabs, foreign origin, bounds), revoke
with token rotation (the revoked browser gets nothing, the others the new token, the
config is saved, a running real sink is restarted), the registry logic against a
fake (a failed register keeps a working registration; verbatim UNC paths), the
approvals file, and an end-to-end test that starts the real `streamtap-host.exe`
against the real bridge (also with a wrong key in `bridge.json`).

Limits: a browser or profile is identified by the random id the extension
generated, which the extension itself claims (any process of your user that can
read the app's config directory could claim another; you still approve every new
one, and such a process could read `config.json` anyway). A process that cannot
read `bridge.json` gets no slot and no prompt; one that only opens connections
without sending anything, or only a byte now and then, holds a thread for 2 s each,
not a slot. The hand run in a real
Chrome is still to do (see the task); Firefox is out of scope.

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
graph only (`targets` in `deny.toml`). `node desktop/scripts/test-app-ui.mjs`
checks the UI without a webview: both language tables have the same keys and
placeholders, every key `index.html` and `app.js` use exists, and both themes
in `style.css` keep WCAG contrast (4.5:1 for text, 3:1 for focus ring and
status dot) for every colour pair the UI uses; then it runs `app.js` against a
fake DOM and fake commands and checks that after a revoke (also one that fails
after the token changed) the token field and *Copy* give the new token, and
that the Settings panel shows the translated `config.json` access warning (or
none). The access decision is unit-tested on made-up DACLs (profile default,
each broad group with each read right, rights without reading, inherit-only and
audit entries, deny order, denies bit by bit, change-DACL and take-ownership
rights, no DACL); one test reads the real DACL of a temp file and finds
Everyone after `icacls /grant *S-1-1-0:(R)`, another gives a temp file a real
NULL DACL and gets all three groups. The token handover
is covered by the sink's stdin tests (first line, CRLF, BOM, EOF, 4 KiB bound
(also with a BOM, a cut line is refused),
blank, non-UTF-8, read error, `--token` and `--token-stdin` together, no read
of stdin before the other flags are checked) and by the end-to-end tests,
which start the real sink with `--token-stdin` and send it a `hello` with the
token.

Linux and macOS: the project is Windows-only for now and has no Linux CI. Two
advisories only exist in Tauri's Linux backend (GTK3): RUSTSEC-2024-0370
(`proc-macro-error`, unmaintained, build-time) and RUSTSEC-2024-0429 (`glib`
unsoundness). `cargo deny` never sees them (Windows graph); `cargo audit`
scans the whole `Cargo.lock` and ignores them in `.cargo/audit.toml`. A Linux
build needs a decision first (newer Tauri/GTK4 backend), not the ignore.