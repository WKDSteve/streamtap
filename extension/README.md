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
| `src/hook.js` | page main world, `document_start`, all frames | Patches `MediaSource`, `SourceBuffer`, `URL.createObjectURL`, `Navigator.requestMediaKeySystemAccess`, `HTMLMediaElement.setMediaKeys`; parses fMP4 / WebM headers; posts records (first `tfdt`, exact above 2^53 as a string; duration of the first track; `timestampOffset`; sequence mode; MediaSource id; `initLen` for an init+media append; `initCont` for the rest of a split init) via `window.postMessage`, after the native append succeeded. **Init assembly:** an append that starts an init (`ftyp`/`moov`, EBML/Segment, even cut off) opens it; appends that do not start a new init continue it until its `moov` / `Tracks` is complete, and so does a `moov` after an init that holds only `ftyp`, or a Segment after one that holds only the EBML header. An append of fewer than 8 bytes cannot be told apart: it is held until the next one; when both together start an init they are posted as that one init record, otherwise each on its own. The init part of an init+media append ends at the first media box (`styp`, `sidx`, `emsg`, `prft`, `moof`, `mdat`), but only with a box after it that only a media segment has (`styp`, `emsg`, `prft`, `moof`, `mdat`; an `ftyp moov sidx` init+index range is all init). A record with samples (a `moof` or `mdat`, also cut off; WebM media; an init+media append included) says whether the player set `timestampOffset` (successfully) since the previous append with samples (`tsoSet`; in sequence mode the browser starts those samples at the offset; an init, or boxes in front of samples such as a `styp`, appended without samples in between do not use it up). Then protection and timescale (`mdhd`, WebM `TimecodeScale`, 1 ms when the init has none) are read from its structure; the head of an open init carries no timescale; while it is still open, its bytes (init bytes only, never media) are scanned for the markers. **DRM:** once `setMediaKeys` is called or an `encrypted` event fires, the frame copies nothing any more; a SourceBuffer whose init declares protection or whose media carries `pssh`/`senc` (or only `saiz`/`saio`) is marked for good. Copies only while the content script taps. Reports `msclose` when a MediaSource closes. |
| `src/content.js` | isolated world | Forwards bounded, type-checked copies of the records to the service worker (pages can forge records) and tells the hook whether to tap. Opens one runtime port `vc-sink` per MediaSource (one sink session per video, ended by `msclose`; at most 8 at a time) and posts every segment as a `data:` URL (extension messaging is JSON, it cannot carry an `ArrayBuffer`) in append order. Until it has read whether a sink token is set, appends are held (at most 64 MiB) instead of dropped; a full queue drops media, not small init bytes, and of an init+media append it still sends the init part. Init bytes dropped anyway (full queue, a post that fails, held beyond 64 MiB) are noted at their place in the stream: the kept init is dropped, the service worker gets `lost` with `drop` (it says `initlost`), and that SourceBuffer's media is not sent until its next init. Replays the init in effect (only the init part of an init+media append, with its continuation chunks) on a new port (restarted service worker); an init too large to keep (over 1 MiB, also when its continuation chunks take it there) drops the kept one instead of leaving a stale one, and so does one beyond the 16 kept per MediaSource; for such a SourceBuffer the new port is told first (`lost`), and the service worker says `initlost` and takes nothing of it until its next init. |
| `src/background.js` | service worker | Per-tab registry (in memory, bounded in entries and string sizes, frame URLs from the browser), badge with the number of SourceBuffers that received media (the rest of a split init counts with its init, not as media). SourceBuffer ids count per frame, so a `changetype` renames only the sending frame's SourceBuffer. Red badge when DRM was seen. Loads `sink-link.js`. |
| `src/sink-link.js` | service worker | One WebSocket to `ws://127.0.0.1:<port>/` per port: `hello` with the token and the frame URL from the browser (not from the page), then one binary message per segment, header fields whitelisted. Every socket, the first one included, first sends the init segments in effect at that point of the stream (the init part only, with continuation chunks), because the sink starts a new capture directory. Queues at most 64 MiB per link and 128 MiB in all while connecting; at most 128 MiB of received payloads wait for decoding in the whole worker; on an open socket media is dropped beyond 64 MiB unsent; small init bytes may go to twice the decoding, the open-socket and the connecting-queue limits, and of an init+media append dropped on an open socket or while connecting the init part (at most 1 MiB) still goes out or is queued and is kept, so that rendition's next media is not lost. When init bytes are dropped anyway (here, also when too much waits for decoding, or in the content script, which says `lost` with `drop`), that SourceBuffer takes nothing until its next init (its media would be filed under the previous one), its kept init is not replayed, and the sink gets an `initlost` event (mux names the SourceBuffer): on an open socket at once, past the limit; while connecting behind what is already queued of that SourceBuffer, which still goes out under its own init; with nothing of it in flight (no socket yet, retry window) on the next socket. Only init bytes this worker decoded and then dropped with nothing of that SourceBuffer in flight are the init in effect: they are kept for the next socket instead. Kept init bytes are bounded worker-wide (64 MiB, parked links included); when full, the largest init of the frame that keeps the most bytes within the tab that keeps the most is dropped first, the keeping tab and frame counted with the init they are about to keep (never that init itself), so one page forging inits loses its own and cannot keep another tab's current init out, nor an ad frame the player frame's; a link whose init was dropped or not kept that way, or was too large to keep (over 1 MiB), tells the sink `initlost` on its next socket, drops what it had queued for that SourceBuffer up to the next queued init, and takes everything from that init on. A link whose port has gone keeps no new init; what it posted is still handled like before once decoded, and a socket that was still connecting or that such a late item starts (after the retry window) first replays the inits kept so far, then sends its queue and closes; those inits are dropped once no socket of the link is being set up. A link takes segments and `lost` messages of at most 32 SourceBuffer ids (as many as the registry keeps per MediaSource); those of further ids are dropped and counted, so forged ids cannot make it keep or announce (`initlost`) more. Backoff resets only after a socket stayed open 5 s (a wrong token is not hammered). At most 16 tapping links per worker: a port beyond that is parked without a socket (it keeps its init bytes, drops media) and admitted on a later segment once a slot frees up. Answers the popup's link status. |
| `src/native-pairing.js` | service worker | Pairing with the desktop app over native messaging (`connectNative` to `io.github.wkdsteve.streamtap`, started by the app's *Connect browser*): says hello with a random per-profile instance id, stores the port and token the app sends **after you approved there** exactly like the popup does (plus `sinkPaired: true`, `tapMode: "selected"`), reports the tabs that hold a MediaSource (title cleaned and cut, host only, favicon as a small PNG data URL from the browser's `_favicon` service, DRM flag, whether tapped, whether visible) and switches capture per tab when the app asks (the tab is reloaded once when switched on, so the capture sees the init segment). Every message from the host is checked field by field; credentials are accepted only in the approved state; a denial or revocation pauses asking for ten minutes (the popup's *pair now* overrides), a revocation removes credentials the pairing gave (a token typed into the popup stays). Reconnects back off from 5 s to 5 min. The token is never logged and never in the status the popup reads. |
| `popup/` | action popup | Shows the registry for the current tab, the pairing state with *pair now*, and the manual port/token fields (fallback; saving them taps every tab as before). |

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
6. To capture, either pair with the desktop app (below) or: start
   `streamtap-sink serve --token <secret>` (see `../desktop/README.md`), enter
   port and token in the popup, **save**, reload the video page. The popup's sink line shows connected / sent / dropped.
   After the video ended (or you close the tab) run
   `streamtap-sink mux captures/<dir>` and open the MP4 in mpv.

## Pair with the desktop app (no copy and paste)

1. Build and start the app (`..\scripts\build.ps1`), press **Connect browser**, **Register**
   for your browser (Chrome, Edge or Brave; per user, no admin) and **Start** the sink.
2. **Reload the extension** on `chrome://extensions` (0.3 adds the `nativeMessaging` and
   `favicon` permissions; Chrome asks again or disables the extension until you accept).
3. The app shows *"<browser> (profile …) wants to pair"*: **Approve**. The extension stores
   port and token itself; the popup shows `app pairing: paired`.
4. The app lists the tabs that hold a video; **Capture** one. That tab is reloaded once and
   from then on tapped; the other tabs are not (mode `selected`). **Stop** ends it.

Notes: a browser paired this way taps only the tabs you chose; entering port and token in the
popup (**save**) returns to the old behaviour (every tab). Tabs are listed from what the
content scripts report (`sender.tab`), so the extension needs no `tabs` permission and no
access to all sites for this; for the same reason there are **no page previews** (a screenshot
needs `activeTab` on a click or host access to all sites), only the favicon (permission
`favicon`) and the title. An open native port keeps the service worker alive while the app
runs. Firefox is not supported.

After editing a file, hit the reload icon on `chrome://extensions` and reload
the page.

Updating from 0.1 (before the `key`): the id changes, so Chrome treats it as
a new extension. Remove the old entry, load the folder again and enter port
and token again (the settings belong to the old id). Reload the tabs that
were open: the old build's content scripts keep knocking with the page
origin until then (the sink logs them as `origin rejected: https://…`).

Chrome ≥ 116 (an open WebSocket keeps the service worker alive).

Firefox: not supported yet. Its MV3 needs `background.scripts` instead of a
service worker, so the sink link does not run there; it also ignores `key`
(the sink would need `--allow-origin <moz-extension origin>`).

## What a successful run looks like

- Two SourceBuffers per MediaSource on DASH / most HLS-via-hls.js sites: one
  `video/mp4`, one `audio/mp4` (or `video/webm` + `audio/webm` on YouTube).
- First append per SourceBuffer is the init segment (`[ftyp moov]`, `INIT`),
  followed by media segments (`[moof mdat]`) with monotonically increasing
  `t=` values and a stable duration (typically 2–10 s).
- A seek shows up as a jump in `t=`; `remove` events show the player evicting
  old ranges (ignored for capture purposes).
- `DRM (not captured): com.widevine.alpha` means the site asked for EME; the
  badge turns red. `setMediaKeys` or `encrypted event` means EME is in use in
  that frame: from then on nothing in the frame is copied. `encrypted media
  in SourceBuffer #N` means the segments themselves carry protection markers:
  those are never copied or sent, and the sink refuses protected init and
  media segments as a second line. A site that only probes EME but plays
  clear media without MediaKeys is still captured. The capture concept stops
  at DRM by design.
- Without a sink token in the popup nothing is sent: the content script
  drops what it held once it has read the setting and switches the hook to
  metadata only.
- Test the hook without a browser: `node scripts/test-hook.mjs`.
- Test what the content script sends a new port (restarted service worker)
  and after a dropped init, and the registry's counts:
  `node scripts/test-content.mjs`.
- Test the pairing module (fake native port, storage and timers) and the
  per-tab tapping gate of the content script: `node scripts/test-pairing.mjs`.
- Test the sink link's lost-init and drain paths with a fake WebSocket (no
  sink needed): `node ../desktop/scripts/test-sink-link.mjs` without
  arguments.

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
- **Service-worker path, real Chrome, 2026-10-31:** the sink accepted the
  extension origin and wrote one session (47 segments, audio + video with a
  quality switch after the start, URL taken from `port.sender.url`); `mux`
  gave H.264 1280×720 + AAC, 156 s, full decode without errors. At the same
  time it rejected `https://www.ardmediathek.de` and
  `https://www.paramountplus.com` with 403: content scripts of the removed
  0.1 build keep running in tabs that were open before (orphaned scripts can
  still open sockets) until those tabs are reloaded.
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
