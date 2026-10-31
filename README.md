# streamtap

Save videos that play in your browser, tapped at the point where they are
already in the clear: a browser extension hooks `SourceBuffer.appendBuffer()`
(Media Source Extensions) and hands the segments to a desktop app that writes
them to disk and muxes them into a playable file. Bit-exact, no re-encoding.

Aimed at players that do not use DRM, for personal copies where the site
allows them (public broadcasters' media libraries, lecture platforms, your own
content); verified so far in Chrome on the ARD Mediathek (hls.js). Content
protected by EME/DRM (Widevine, PlayReady, FairPlay) is detected and left
alone, by design. Respect the terms of the sites you use it on; see the
[legal note](#legal-note).

## Status

- Spike 1 (done): the extension logs what players feed into MSE. See
  [extension/README.md](extension/README.md).
- Spike 2 (done, verified in Chrome on 2026-10-31): `desktop/streamtap-sink`
  receives segments over a loopback WebSocket, writes them per SourceBuffer
  and remuxes with `ffmpeg -c copy`, reordering after seeks. See
  [desktop/README.md](desktop/README.md).
- MVP, first step (done): desktop/crates/streamtap-app, a Tauri 2 app that starts and stops the sink,
  lists the captures live and muxes, opens or deletes them. See the app section of
  [desktop/README.md](desktop/README.md).
- Browser pairing (code done, hand run in Chrome pending): the app registers a native
  messaging host for Chrome, Edge or Brave (per user, no admin), the extension pairs with
  the app after you approve it there and receives port and token by itself, and the app
  lists the tabs that hold a video so you pick the one to capture. See "Connect browser"
  in [desktop/README.md](desktop/README.md) and [extension/README.md](extension/README.md).

Layout:

```
streamtap/
  extension/   MV3 extension (hook, content script, service worker, popup)
  desktop/     Rust workspace: streamtap-sink (WebSocket receiver, segment store, mux),
               streamtap-app (Tauri 2 shell + static UI in crates/streamtap-app/ui),
               streamtap-host (native messaging host: browser <-> app relay)
  tools/       repository checks (control-character guard)
  LICENSE
  README.md
```

## Checks

CI (`.github/workflows/ci.yml`, the author's self-hosted Windows runner, on
pushes to `main` only) runs, and locally the same commands do:

```
cd desktop
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
cargo audit --deny warnings     # cargo install cargo-audit
cargo deny check                # cargo install cargo-deny; policy in desktop/deny.toml
cd ..
git ls-files -z 'extension/*.js' 'extension/*.mjs' 'desktop/scripts/*.mjs' 'desktop/crates/streamtap-app/ui/*.js' | xargs -0 -n1 node --check
node extension/scripts/test-hook.mjs
node extension/scripts/test-content.mjs
node extension/scripts/test-pairing.mjs
node desktop/scripts/test-sink-link.mjs
python -m unittest discover tools
python tools/check-control-chars.py
```

The Rust toolchain is pinned in `desktop/rust-toolchain.toml` (rustup
installs it on first use).

## Roadmap (short)

1. Spike 1: hook sees segments, timeline parses (done, see extension README).
2. Spike 2: segments streamed binary to a local Rust server, written per
   SourceBuffer, `ffmpeg -c copy` yields a playable MP4 (done).
3. MVP: desktop app with a capture list, DRM flag, mux and open (started: sink control,
   live list, mux/open/delete, browser pairing with a tab list; still to do: background
   capture mode, installer).
4. Later: more capture modes for DRM-free streams, tab capture, live ring
   buffer, Firefox.

## Legal note

This is a tool for personal copies of DRM-free streams. It does not and will
not circumvent technical protection measures (AT § 90c UrhG, DE § 95a UrhG,
EU 2001/29/EC Art. 6). Respect the terms of the sites you use it on.

## License

MIT, see [LICENSE](LICENSE).
