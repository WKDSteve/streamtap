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
- `--token` is required. The extension sends the same value in its first
  message (`hello`); a wrong token closes the socket before anything is
  written. Set it in the extension popup (port + token).
- Every connection becomes one capture directory
  `captures/<unix-seconds>_<session-prefix>[-n]/` (a suffix when the name is
  taken, e.g. a reconnect within the same second) with `session.json` (page
  URL from the browser, title; written atomically), `sbN.pM.bin` (one file
  per SourceBuffer `N` and init-segment part `M`) and `index.jsonl` (one line
  per written segment, written in one call: offset, length, MediaSource,
  `tfdt`, timescale, duration, `timestampOffset`, sequence mode, MIME). The
  extension opens one connection per MediaSource, so one directory is one
  video.
- Limits: 64 KiB JSON header, 64 MiB per message and per WebSocket frame
  (larger close the connection), 32 SourceBuffers per session, 400 000 media
  segments per session. Disk: the directory, payload and index lines count
  against 16 GiB per session (`--max-session-gib`) and 64 GiB per sink
  process (`--max-total-gib`); when either is reached the session stops for
  good (logged once), and once the process budget is spent a new connection
  is closed without creating a directory. Empty payloads are not written. A
  malformed message after `hello` is skipped and logged (a page can forge the
  hook's records); more than 100 close the connection. Before `hello` any bad
  message closes it. Every way a connection ends prints the per-SourceBuffer
  summary. Page strings (URL, MIME) are printed as printable ASCII only.
- **DRM:** a segment that carries protection is not written, nor anything
  after it on that SourceBuffer until a clear init segment arrives. Init bytes
  (an init and its continuation chunks) are scanned for `encv`/`enca`/`tenc`/
  `pssh` (MP4) or the `ContentEncodings`, `ContentEncryption` and
  `ContentEncKeyID` IDs together (WebM). Media is only read as structure: a
  top-level `moov` or `pssh`, sample encryption (`senc` or PIFF) in a `moof`;
  random bytes in media never count. The extension does not send protected
  segments in the first place; this is the second line.

### Dedupe and parts

- An init appended together with media (video.js VHS) is stored as two rows:
  the init part and the media part. The init is decided right there, so the
  media part never counts as its continuation (also without a `tfdt`).
- An init is decided once it is complete: its chunks (an init split across
  appends: the untimed chunks right after it, or chunks the extension flags
  `initCont`) are held, up to 4 MiB, until the next media, then compared as a
  whole.
- A repeated, identical init is skipped. A **different** init (codec or
  quality change, `changeType`) starts a new part file `sbN.p(M+1).bin`; a
  switch **back** to an init seen before continues that earlier part. The
  first init when media came before it (the hook attached late) starts a new
  part too: that media stays alone in part 0.
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
target/release/streamtap-sink mux captures/<dir> [--out file.mp4]
```

Runs `ffmpeg -f mp4 -protocol_whitelist file [-itsoffset <s>] -i <part> … -map 0 -map 1 … -c copy -movflags +faststart`.

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
  the first segment, so part 0 can be a single segment. The other parts are
  named in a warning and left out (spike limit).
- **Order:** the sink writes in arrival order. Unless the part already is
  init-first and in timeline order, `mux` writes `sbN.pM.sorted.bin`: all init
  bytes first, then media segments by presentation time (`tfdt` plus
  `timestampOffset` in ticks, computed exactly). Segments moved by a
  `timestampOffset` get their `tfdt` rewritten so the timeline stays
  monotonic; in a SourceBuffer with several tracks each `traf` is moved in
  its own track's timescale (from the init's `tkhd`/`mdhd`). With durations
  on all segments, one that earlier ones fully cover (an LL-HLS full segment
  after its parts) is left out; partial overlaps are kept and counted (frames
  may repeat there). Without durations that check cannot run, which is
  warned. Without a `tfdt` on every media segment (WebM blocks, continuation
  chunks), and in sequence mode (the browser placed the appends one after the
  other; the sink already dropped re-appends), the arrival order is kept.
- **Gaps:** ranges skipped by a forward seek are reported with their
  approximate length.
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
| `segment` | `sb`, `seq`, `ms`, `mime`, `isInit`, `initCont`, `kind`, `tfdt` (number, or decimal string above 2^53), `timescale` (u32), `dur` (ticks of the first track), `tso` (seconds), `seqMode` | the segment bytes exactly as passed to `appendBuffer` |
| `event` | `name` (`endofstream`, `changetype`), `sb` | none |
## Test without a browser

`cargo test --release` runs the unit tests (dedupe, split inits, ABR
switch-back, parts, DRM refusal incl. PIFF and split inits, budgets,
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
shell for the ffmpeg child, no heredocs for docs, LICENSE.
Not yet (spike): `deny.toml` / `cargo audit` in CI, Windows CI runner (repo is
private), security tests as raw TCP bytes against the running server (`TST-01`),
read and handshake timeouts, child-process timeout for ffmpeg (`RS-04`),
error enum (`RS-05`), the token on the command line (visible to other
processes; pairing replaces it), the core/sink/testkit split, the Tauri
shell and UI. The security tests, CI and the split come first, before new
features.
