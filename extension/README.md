# Extension: Spike 1 + 2 – MSE hook logger and segment streamer

Manifest V3 extension that patches the Media Source Extensions prototypes in
the page's main world and, for every `SourceBuffer.appendBuffer()` call,
records which SourceBuffer, MIME type, byte size, container kind, top-level
boxes, `tfdt` start time and duration (spike 1), and streams a copy of the
segment bytes to the local sink over a loopback WebSocket (spike 2, see
[../desktop/README.md](../desktop/README.md)). Without a configured sink the
extension stays in metadata-only mode.

The **service worker owns the socket**, not the content script: the sink
then sees the extension's own origin and compares it exactly, and no page
CSP (`connect-src`) can block it. The manifest's `key` pins the extension id
to `oggpddakdbffbjhkjmcakidgmhpgakia` on every machine, so the origin is
`chrome-extension://oggpddakdbffbjhkjmcakidgmhpgakia`.

The concept and the DRM primer live in the nvim-config vault under
`docs/ROADMAP/IDEAS/video-capture/`.

## Files

| File | Runs where | Purpose |
|---|---|---|
| `src/hook.js` | page main world, `document_start`, all frames | Patches `MediaSource`, `SourceBuffer`, `URL.createObjectURL`, `Navigator.requestMediaKeySystemAccess`; parses fMP4 / WebM headers; posts records via `window.postMessage`. |
| `src/content.js` | isolated world | Forwards the records to the service worker. On the first segment it opens a runtime port `vc-sink` and posts `hello`, then every segment as a `data:` URL (extension messaging is JSON, it cannot carry an `ArrayBuffer`) in append order. At most 64 MiB wait for encoding; beyond that segments are counted as dropped. |
| `src/background.js` | service worker | Per-tab registry (in memory), badge with the number of SourceBuffers that received media. Red badge when a DRM key system was requested. Loads `sink-link.js`. |
| `src/sink-link.js` | service worker | One WebSocket to `ws://127.0.0.1:<port>/` per port: `hello` with the token and the frame URL from the browser (not from the page), then one binary message per segment. Queues at most 64 MiB, only while connecting; when the sink is down segments are dropped and counted, never queued. Answers the popup's link status. |
| `popup/` | action popup | Shows the registry for the current tab. |

## Load it (Chrome / Edge / Brave)

1. Open `chrome://extensions`, enable **Developer mode**.
2. **Load unpacked** → select this `extension/` folder.
3. Open a page with a video (e.g. ARD Mediathek), play it.
4. Open DevTools → Console, filter `vc-hook`. Each append logs one line:

   ```
   [vc-hook] #1 seq 12 t=120.0s +10s 1071790 B mp4 [moof mdat] video/mp4; codecs="avc1.64001f"
   ```

5. Click the extension icon for the per-tab summary (SourceBuffers, init /
   media segment counts, bytes, covered time range).
6. To capture: start `streamtap-sink serve --token <secret>` (see
   `../desktop/README.md`), enter port and token in the popup, **save**, reload
   the video page. The popup's sink line shows connected / sent / dropped.
   After the video ended (or you close the tab) run
   `streamtap-sink mux captures/<dir>` and open the MP4 in mpv.

After editing a file, hit the reload icon on `chrome://extensions` and reload
the page.

Updating from 0.1 (before the `key`): the id changes, so Chrome treats it as
a new extension. Remove the old entry, load the folder again and enter port
and token again (the settings belong to the old id).

Chrome ≥ 116 (an open WebSocket keeps the service worker alive).

Firefox: `world: "MAIN"` needs Firefox ≥ 128; load via
`about:debugging#/runtime/this-firefox` → Load Temporary Add-on → `manifest.json`.
Firefox ignores `key` and gives the extension a random `moz-extension://`
origin; start the sink with `--allow-origin <that origin>` to capture there.

## What a successful run looks like

- Two SourceBuffers per MediaSource on DASH / most HLS-via-hls.js sites: one
  `video/mp4`, one `audio/mp4` (or `video/webm` + `audio/webm` on YouTube).
- First append per SourceBuffer is the init segment (`[ftyp moov]`, `INIT`),
  followed by media segments (`[moof mdat]`) with monotonically increasing
  `t=` values and a stable duration (typically 2–10 s).
- A seek shows up as a jump in `t=`; `remove` events show the player evicting
  old ranges (ignored for capture purposes).
- `DRM requested: com.widevine.alpha` means the site uses EME; the badge turns
  red and the capture concept stops there by design.

## Result of the first live run (2026-10-30)

Injected into a running ARD Mediathek page (hls.js → fMP4), 7 s after a seek:

- 68 appends on two SourceBuffers, strictly alternating video / audio.
- Video: ~0.5–1.7 MB per segment, 500 samples each, `tfdt` delta 900 000 →
  timescale 90 000 → exact 10 s segments at 50 fps.
- Audio: ~168 KB per segment, 468/469 samples, `tfdt` delta ≈ 480 000 →
  48 kHz → 10 s segments.
- The seek showed as a 120 s jump in both tracks, consistent with
  `video.buffered`.
- `startSec` / `durationSec` were `null` because the init segment (and thus the
  `mdhd` timescale) was appended before the hook was injected. With the
  extension installed (hook runs at `document_start`) the init segment is seen
  and the values are filled in. The "pre-hook" path registers such
  SourceBuffers lazily so late injection still works.

## Spike 2 result (2026-10-31)

- Sink and wire format verified end to end with `desktop/scripts/test-sink.mjs`
  (fragmented test MP4 → sink → `mux` → ffprobe OK).
- **Real Chrome, ARD Mediathek:** one session, about 10.5 minutes of video
  with several forward seeks and one 408 s backward seek. `mux` produced an
  MP4 with H.264 1280×720 and AAC 48 kHz, 630 s; a full decode
  (`ffmpeg -f null`) reports no errors.
- The console showed one `WebSocket connection … failed` right after page
  load; the reconnect succeeded and all segments arrived. No CSP message on
  this site. Since then the socket lives in the service worker (decided
  2026-10-31), so a page `connect-src` no longer matters.
- Two things the real run taught the mux (see `../desktop/README.md`):
  hls.js starts at a low quality and switches after the first segment (new
  init segment → new part), and after seeks segments arrive out of order.
- The popup escapes every value it shows: any page script can forge the
  hook's `postMessage` records.

## Known limitations of the spike

- Timescale is taken from the first `mdhd` in the init segment; muxed
  multi-track init segments would take the first track's timescale.
- MPEG-TS segments are detected (`ts`) but not parsed; hls.js transmuxes them to
  fMP4 before `appendBuffer`, so this rarely matters in practice.
- The service worker state is in memory only; a suspended worker forgets the
  registry (the badge resets). Fine for a spike, not for the real thing.
- `sender.tab.url` in the registry is only available where the content script
  has host access; it is informational only.

- Any page script can post records that look like the hook's. They only
  reach this page's own capture and the popup (escaped); they gain nothing,
  but a capture of a hostile page can contain junk.
