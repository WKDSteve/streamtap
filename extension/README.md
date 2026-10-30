# Extension: Spike 1 + 2 – MSE hook logger and segment streamer

Manifest V3 extension that patches the Media Source Extensions prototypes in
the page's main world and, for every `SourceBuffer.appendBuffer()` call,
records which SourceBuffer, MIME type, byte size, container kind, top-level
boxes, `tfdt` start time and duration (spike 1), and streams a copy of the
segment bytes to the local sink over a loopback WebSocket (spike 2, see
[../desktop/README.md](../desktop/README.md)). Without a configured sink the
extension stays in metadata-only mode.

The concept and the DRM primer live in the nvim-config vault under
`docs/ROADMAP/IDEAS/video-capture/`.

## Files

| File | Runs where | Purpose |
|---|---|---|
| `src/hook.js` | page main world, `document_start`, all frames | Patches `MediaSource`, `SourceBuffer`, `URL.createObjectURL`, `Navigator.requestMediaKeySystemAccess`; parses fMP4 / WebM headers; posts records via `window.postMessage`. |
| `src/content.js` | isolated world | Forwards the records to the service worker; opens `ws://127.0.0.1:<port>/` to the sink, sends `hello` with the token, then one binary message per segment. Never buffers more than 64 MiB and only while connecting; when the sink is down segments are counted as dropped, not queued. |
| `src/background.js` | service worker | Per-tab registry (in memory), badge with the number of SourceBuffers that received media. Red badge when a DRM key system was requested. |
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

Firefox: `world: "MAIN"` needs Firefox ≥ 128; load via
`about:debugging#/runtime/this-firefox` → Load Temporary Add-on → `manifest.json`.

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

## Spike 2 status (2026-10-30)

- Sink and wire format verified end to end with `desktop/scripts/test-sink.mjs`
  (fragmented test MP4 → sink → `mux` → ffprobe OK).
- Browser → sink **not yet verified**: the Claude desktop app's built-in
  browser pane cannot reach loopback servers started from the shell (all
  `ws://127.0.0.1:17650` attempts failed without a CSP message and without
  reaching the listener). The hook + content script injected on the hls.js
  demo page did produce segments (18 appends in 12 s, 1920×1080) and the
  content script counted them as dropped, so the page-side path works; only
  the socket needs a real Chrome. Open question from the concept remains:
  does the page CSP (`connect-src`) apply to a content script's WebSocket in
  Chromium? First real-Chrome run answers it.

## Known limitations of the spike

- Timescale is taken from the first `mdhd` in the init segment; muxed
  multi-track init segments would take the first track's timescale.
- MPEG-TS segments are detected (`ts`) but not parsed; hls.js transmuxes them to
  fMP4 before `appendBuffer`, so this rarely matters in practice.
- The service worker state is in memory only; a suspended worker forgets the
  registry (the badge resets). Fine for a spike, not for the real thing.
- `sender.tab.url` in the registry is only available where the content script
  has host access; it is informational only.

## Next: Spike 2

Copy each segment once (`data.slice()`), transfer it to the content script,
stream it binary over a local WebSocket to a small Rust server that writes one
file per SourceBuffer, then `ffmpeg -c copy` the result into a playable MP4.
