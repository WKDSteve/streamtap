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
  `tfdt`, timescale, `timestampOffset`, MIME).
- Limits: 64 KiB JSON header, 64 MiB per message (larger messages close the
  connection), 32 SourceBuffers per session. A malformed message after
  `hello` is skipped and logged (a page can forge the hook's records); more
  than 100 close the connection. Before `hello` any bad message closes it.
- **DRM:** an init segment that declares protection (`encv`, `enca`, `tenc`,
  `pssh`) is not written, nor anything after it on that SourceBuffer until a
  clear init segment arrives. The extension does not send such segments in
  the first place; this is the second line.

### Dedupe and parts

- A media segment whose (`tfdt`, `timestampOffset`) was already written for
  that SourceBuffer part is skipped (players re-fetch ranges after seeks).
  The offset matters: multi-period DASH and HLS discontinuities restart
  `tfdt` and move the timeline with `timestampOffset` instead.
- A repeated, byte-identical init segment is skipped.
- A **different** init segment (codec or quality change, `changeType`) starts
  a new part file `sbN.p(M+1).bin`.

## Mux

```
target/release/streamtap-sink mux captures/<dir> [--out file.mp4]
```

Runs `ffmpeg -i <video part> -i <audio part> -map 0 -map 1 -c copy -movflags +faststart`
(explicit `-map`: every stream of every input).

- **Which MediaSource:** one MediaSource is one video. A preroll ad, the next
  video of a single-page app or a hover preview add more to the same
  session; `mux` takes the one with the most bytes and names the others.
- **Which part:** per SourceBuffer the part with an init segment and the
  most bytes. Adaptive players often start at a low quality and switch after
  the first segment, so part 0 can be a single segment. The other parts are
  named in a warning and left out (spike limit).
- **Order:** the sink writes in arrival order. Unless the part already is
  init-first and in timeline order, `mux` writes `sbN.pM.sorted.bin`: init
  segment first, then media segments by presentation time (`tfdt` plus
  `timestampOffset`). Segments moved by a `timestampOffset` get their `tfdt`
  rewritten so the timeline stays monotonic. Without a `tfdt` on every media
  segment (WebM blocks, continuation chunks) the arrival order is kept.
- **Gaps:** ranges skipped by a forward seek are reported with their
  approximate length.
- A bad `index.jsonl` line (an interrupted sink) is skipped, not fatal.
- A SourceBuffer without any init segment is skipped (capture started after
  the player had appended it; the extension avoids this by running at
  `document_start`, and resends kept init segments after a reconnect).

## Wire format

Each WebSocket **binary** message:

```
u32 little-endian header length | JSON header | payload bytes
```

Headers:

| `type` | fields | payload |
|---|---|---|
| `hello` | `token`, `session`, `url`, `title` | none; must be the first message |
| `segment` | `sb`, `seq`, `ms`, `mime`, `isInit`, `kind`, `tfdt`, `timescale` (u32), `tso` (seconds) | the segment bytes exactly as passed to `appendBuffer` |
| `event` | `name` (`endofstream`, `changetype`), `sb` | none |

## Test without a browser

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
Node with a mocked `chrome`, posting segments as `data:` URLs right after
`hello` like the content script. Halfway it closes the socket as if the sink
had restarted. It checks what the sink wrote: two capture directories, the
second starting with the replayed init segment, nothing dropped.
Both clients send the extension origin (`--origin` to change it).
`../extension/scripts/test-hook.mjs` checks the page hook in Node (DRM
marking, tap switch, `timestampOffset`, corrupt WebM).

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
