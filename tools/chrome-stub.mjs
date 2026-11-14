// The stand-in for `chrome.*` of the extension's popup, for the check that opens popup.html without the extension
// (tools/page-check.mjs puts it in as an init script): what the service worker answers, with something in every part of
// every page. It runs in the page, so it uses nothing from outside.

/** Sets `window.chrome` before the popup's scripts run. `target`: the stored target ("" is the sink on the PC). */
export function chromeStub({ target }) {
  const buffer = (sbId, mime) => ({ sbId, mime, mimes: ["video/webm"], kind: "mp4", inits: 1, media: 12, bytes: 3 << 20, firstSec: 0, lastEndSec: 48, lastSec: 44 });
  const state = {
    drm: [{ keySystem: "com.example.drm", robustness: ["SW_SECURE_DECODE"] }],
    events: 31,
    frames: [1],
    mediaSources: [
      {
        msId: 1,
        ended: false,
        frameUrl: "https://video.example.invalid/watch/a-long-path-that-has-to-wrap-on-a-narrow-screen/and-goes-on",
        blobUrl: "blob:https://video.example.invalid/0123-4567",
        sourceBuffers: [buffer(1, 'video/mp4; codecs="avc1.64001f"'), buffer(2, 'audio/mp4; codecs="mp4a.40.2"')],
      },
    ],
  };
  const file = {
    available: true,
    recording: 1,
    media: 12,
    bytes: 3 << 20,
    full: false,
    saved: [
      { name: "streamtap-20260101-120000-video.example.invalid.mp4", bytes: 9 << 20, video: true, report: { gaps: 1, gapSeconds: 2.5 } },
      { name: "streamtap-20260101-110000-video.example.invalid.streamtap", bytes: 4 << 20, fallback: "the source is WebM", reason: "webm" },
      { error: "download", name: "streamtap-20260101-100000-video.example.invalid.streamtap" },
    ],
  };
  const answers = {
    "vc-get": state,
    "vc-source-of": { verdict: { state: "locked", groups: ["social"] }, on: true, unlocked: ["obfuscated"], own: ["tv.home.example"], learning: [] },
    "vc-pair-status": { connected: true, pairing: "approved", tapped: 1 },
    "vc-link-status": { links: 1, connected: true, sent: 40, sentBytes: 6 << 20, dropped: 0, file },
    "vc-tailnet-status": { available: true, paired: true, host: "pc.example.ts.net", device: "0123456789abcdef0123456789abcdef", state: "unreachable", offer: "pc.example.ts.net" },
    "vc-tailnet-pair": { ok: false, reason: "code" },
  };
  window.chrome = {
    runtime: {
      lastError: undefined,
      getManifest: () => ({ version: "0.0.0" }),
      sendMessage: (msg, cb) => cb && cb(answers[msg.kind]),
    },
    tabs: { query: async () => [{ id: 1 }] },
    storage: { local: { get: (wanted, cb) => cb({ ...wanted, ...("sinkTarget" in wanted ? { sinkTarget: target } : {}) }), set: (_values, cb) => cb && cb() } },
  };
}
