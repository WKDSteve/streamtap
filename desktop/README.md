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
```

- Binds **127.0.0.1 only**; there is no flag to bind anything else.
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

Runs `ffmpeg -i sb1.p0.bin [-i sb2.p0.bin …] -c copy -movflags +faststart`.
Only part 0 of each SourceBuffer is muxed in this spike; the command warns
when more parts exist or when part 0 has no init segment (capture started
after the player had already appended it, which the extension avoids by
running at `document_start`).

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

Result on 2026-10-30: 8 segments written, 2 duplicates dropped, `mux: ok`,
ffprobe reports h264 640×360 + aac 48 kHz, 6.08 s.

## Deferred from the NEW_PROJECT gate (rules-vault, native)

Done: loopback bind, token before any write, hard size limits, workspace
lints, no shell for the ffmpeg child, no heredocs for docs, LICENSE.
Not yet (spike): `deny.toml` / `cargo audit` in CI, Windows CI runner (repo is
private), security tests as raw TCP bytes before the first route (`TST-01`),
child-process timeout for ffmpeg (`RS-04`), error enum (`RS-05`), the
Tauri shell and UI. These come with the MVP, not with the spike.
