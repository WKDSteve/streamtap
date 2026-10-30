# streamtap

Save videos that play in your browser, tapped at the point where they are
already in the clear: a browser extension hooks `SourceBuffer.appendBuffer()`
(Media Source Extensions) and hands the segments to a desktop app that writes
them to disk and muxes them into a playable file. Bit-exact, no re-encoding.

Works for anything that does not use DRM (public broadcasters' media
libraries, YouTube, Vimeo, Twitch, news sites, lecture platforms). Content
protected by EME/DRM (Widevine, PlayReady, FairPlay) is detected and left
alone, by design.

## Status

- Spike 1 (done): the extension logs what players feed into MSE. See
  [extension/README.md](extension/README.md).
- Spike 2 (sink done, browser run pending): `desktop/streamtap-sink` receives
  segments over a loopback WebSocket, writes them per SourceBuffer and remuxes
  with `ffmpeg -c copy`. See [desktop/README.md](desktop/README.md).

Layout:

```
streamtap/
  extension/   MV3 extension (hook, content script, service worker, popup)
  desktop/     Rust workspace: streamtap-sink (WebSocket receiver, segment store, mux);
               later the Tauri 2 app
  LICENSE
  README.md
```

## Roadmap (short)

1. Spike 1: hook sees segments, timeline parses (done, see extension README).
2. Spike 2: segments streamed binary to a local Rust server, written per
   SourceBuffer, `ffmpeg -c copy` yields a playable MP4 (sink verified with a
   test client; first real-browser run pending).
3. MVP: desktop app with a tab/video list, background capture mode, DRM flag.
4. Later: download modes (yt-dlp, manifest replay), tab capture, live ring
   buffer, Firefox.

The full concept, the mode matrix and the DRM/CDM primer are kept outside this
repository (vault notes).

## Legal note

This is a tool for personal copies of DRM-free streams. It does not and will
not circumvent technical protection measures (AT § 90c UrhG, DE § 95a UrhG,
EU 2001/29/EC Art. 6). Respect the terms of the sites you use it on.

## License

MIT, see [LICENSE](LICENSE).
