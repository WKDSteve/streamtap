#!/usr/bin/env node
// Test client for streamtap-sink: splits a fragmented MP4 into its init
// segment (ftyp+moov) and media segments (moof+mdat), then streams them to a
// running sink exactly like the extension would, including a duplicate
// segment to exercise the tfdt dedupe. Usage:
//
//   node scripts/test-sink.mjs <fragmented.mp4> --token <token> [--port 17650]
//        [--origin chrome-extension://oggpddakdbffbjhkjmcakidgmhpgakia] [--hold <ms>]
//
// --hold keeps the connection open that long after the last segment (a paused video), to watch the app show it live.
//
// The sink only accepts the extension's origin, so the client sends it (the
// non-standard `headers` option of Node's undici WebSocket). Needs Node >= 22.
import { splitFmp4 } from "./fmp4-split.mjs";

const args = process.argv.slice(2);
const file = args.find((a) => !a.startsWith("--"));
const flag = (n, d) => {
  const i = args.indexOf(n);
  return i >= 0 ? args[i + 1] : d;
};
const token = flag("--token");
const port = Number(flag("--port", "17650"));
const hold = Number(flag("--hold", "300"));
const origin = flag("--origin", "chrome-extension://oggpddakdbffbjhkjmcakidgmhpgakia");
if (!file || !token) {
  console.error("usage: test-sink.mjs <fragmented.mp4> --token <token> [--port 17650] [--origin <origin>]");
  process.exit(2);
}

const { init, segments, tfdtOf, durOf, timescale } = splitFmp4(file);
console.log(`init ${init.byteLength} B, ${segments.length} media segments, timescale ${timescale}`);

const enc = new TextEncoder();
const frame = (header, payload) => {
  const h = enc.encode(JSON.stringify(header));
  const len = new Uint8Array(4);
  new DataView(len.buffer).setUint32(0, h.byteLength, true);
  const out = new Uint8Array(4 + h.byteLength + (payload ? payload.byteLength : 0));
  out.set(len, 0);
  out.set(h, 4);
  if (payload) out.set(payload, 4 + h.byteLength);
  return out;
};

const ws = new WebSocket(`ws://127.0.0.1:${port}/`, { headers: { Origin: origin } });
ws.binaryType = "arraybuffer";
ws.onopen = () => {
  ws.send(frame({ type: "hello", token, session: "test-client", url: "file://" + file, title: "test-sink.mjs" }));
  const mime = 'video/mp4; codecs="avc1.64001e, mp4a.40.2"';
  let seq = 1;
  // `dur` (ticks of `timescale`) is what the app's list sums up as the duration, like the extension sends it.
  const seg = (payload, isInit, dur = null) =>
    ws.send(frame({ type: "segment", sb: 1, seq: seq++, mime, isInit, kind: "mp4", tfdt: isInit ? null : tfdtOf(payload), timescale, dur: isInit ? null : dur }, payload));
  seg(init, true);
  segments.forEach((s, i) => seg(s, false, durOf(i)));
  // Re-send init and the second media segment: both must be deduped.
  seg(init, true);
  if (segments[1]) seg(segments[1], false, durOf(1));
  ws.send(frame({ type: "event", name: "endofstream", sb: null }));
  setTimeout(() => ws.close(), hold);
};
ws.onclose = (e) => {
  console.log(`closed code=${e.code}; sent ${segments.length + 1} unique + 2 duplicate segments`);
};
ws.onerror = (e) => {
  console.error("websocket error", e.message || e);
  process.exit(1);
};
