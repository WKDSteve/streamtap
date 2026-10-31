# desktop: streamtap-sink (spike 2)

A small Rust binary that receives the segments the extension taps, writes them
to disk and remuxes them with ffmpeg. It is the seed of the later desktop app;
for now it is a CLI with two subcommands.

## Build

```
cargo build --release
```

Workspace lints: `unsafe_code = "forbid"`, clippy `all` as `deny`, `pedantic`
as `warn` (clean as of spike 2), rustfmt with Unix newlines.

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
  `captures/<unix-seconds>_<session-prefix>/` with `session.json` (page URL,
  title), `sbN.pM.bin` (one file per SourceBuffer `N` and init-segment part
  `M`) and `index.jsonl` (one line per written segment: offset, length,
  `tfdt`, timescale, MIME).
- Limits: 64 KiB JSON header, 64 MiB per message; larger messages close the
  connection.

### Dedupe and parts

- A media segment whose `tfdt` was already written for that SourceBuffer is
  skipped (players re-fetch ranges after seeks).
- A repeated, byte-identical init segment is skipped.
- A **different** init segment (codec or quality change, `changeType`) starts
  a new part file `sbN.p(M+1).bin`.

## Mux

```
target/release/streamtap-sink mux captures/<dir> [--out file.mp4]
```

Runs `ffmpeg -i <video part> -i <audio part> -c copy -movflags +faststart`.

- **Which part:** per SourceBuffer the part with an init segment and the
  most bytes. Adaptive players often start at a low quality and switch after
  the first segment, so part 0 can be a single segment. The other parts are
  named in a warning and left out (spike limit).
- **Order:** the sink writes in arrival order. After a backward seek that is
  not the timeline order, so `mux` writes `sbN.pM.sorted.bin` (init segment
  first, then media segments by ascending `tfdt`) and muxes that instead.
- **Gaps:** ranges skipped by a forward seek are reported with their
  approximate length.
- A SourceBuffer without any init segment is skipped (capture started after
  the player had appended it; the extension avoids this by running at
  `document_start`).

## Wire format

Each WebSocket **binary** message:

```
u32 little-endian header length | JSON header | payload bytes
```

Headers:

| `type` | fields | payload |
|---|---|---|
| `hello` | `token`, `session`, `url`, `title` | none; must be the first message |
| `segment` | `sb`, `seq`, `mime`, `isInit`, `kind`, `tfdt`, `timescale` | the segment bytes exactly as passed to `appendBuffer` |
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

`scripts/test-sink-link.mjs` (same arguments) runs the extension's
service-worker link (`extension/src/sink-link.js`) in Node with a mocked
`chrome`, posting segments as `data:` URLs right after `hello` like the
content script; it exits 0 when every message was sent and none dropped.
Both clients send the extension origin (`--origin` to change it).

Result on 2026-10-30: 8 segments written, 2 duplicates dropped, `mux: ok`,
ffprobe reports h264 640×360 + aac 48 kHz, 6.08 s.

First real-browser run (Chrome, ARD Mediathek, 2026-10-31): one session,
H.264 1280×720 + AAC, 630 s after reordering, full decode without errors.

## Deferred from the NEW_PROJECT gate (rules-vault, native)

Done: loopback bind, exact origin check in the handshake, token before any
write, hard size limits, workspace lints, no shell for the ffmpeg child, no
heredocs for docs, LICENSE.
Not yet (spike): `deny.toml` / `cargo audit` in CI, Windows CI runner (repo is
private), security tests as raw TCP bytes before the first route (`TST-01`),
read and handshake timeouts, child-process timeout for ffmpeg (`RS-04`),
error enum (`RS-05`), the token on the command line (visible to other
processes; pairing replaces it), the core/sink/testkit split, the Tauri
shell and UI. The security tests, CI and the split come first, before new
features.
