#!/usr/bin/env node
// Test client for streamtap-sink: splits a fragmented MP4 into its init
// segment (ftyp+moov) and media segments (moof+mdat), then streams them to a
// running sink exactly like the extension would, including a duplicate
// segment to exercise the tfdt dedupe. Usage:
//
//   node scripts/test-sink.mjs <fragmented.mp4> --token <token> [--port 17650]
//
// Needs Node >= 22 (global WebSocket).
import { readFileSync } from "node:fs";

const args = process.argv.slice(2);
const file = args.find((a) => !a.startsWith("--"));
const flag = (n, d) => {
  const i = args.indexOf(n);
  return i >= 0 ? args[i + 1] : d;
};
const token = flag("--token");
const port = Number(flag("--port", "17650"));
if (!file || !token) {
  console.error("usage: test-sink.mjs <fragmented.mp4> --token <token> [--port 17650]");
  process.exit(2);
}

const buf = readFileSync(file);
const u8 = new Uint8Array(buf.buffer, buf.byteOffset, buf.byteLength);
const dv = new DataView(u8.buffer, u8.byteOffset, u8.byteLength);
const type = (off) => String.fromCharCode(u8[off + 4], u8[off + 5], u8[off + 6], u8[off + 7]);

// Top-level boxes -> [{type, start, end}]
const boxes = [];
for (let off = 0; off + 8 <= u8.byteLength; ) {
  let size = dv.getUint32(off);
  if (size === 1) size = Number(dv.getBigUint64(off + 8));
  else if (size === 0) size = u8.byteLength - off;
  boxes.push({ type: type(off), start: off, end: off + size });
  off += size;
}

// Init = everything up to and including moov; then pairs of moof+mdat.
const moovIdx = boxes.findIndex((b) => b.type === "moov");
const init = u8.subarray(0, boxes[moovIdx].end);
const segments = [];
for (let i = moovIdx + 1; i < boxes.length; i++) {
  if (boxes[i].type === "moof" && boxes[i + 1] && boxes[i + 1].type === "mdat") {
    segments.push(u8.subarray(boxes[i].start, boxes[i + 1].end));
    i++;
  }
}

// tfdt of a moof+mdat segment (walk moof > traf > tfdt), plus mdhd timescale from init.
const findBox = (start, end, path) => {
  let [want, ...rest] = path;
  for (let off = start; off + 8 <= end; ) {
    const size = dv.getUint32(off);
    if (size < 8) return null;
    if (type(off) === want) return rest.length ? findBox(off + 8, off + size, rest) : off;
    off += size;
  }
  return null;
};
const tfdtOf = (seg) => {
  const base = seg.byteOffset - u8.byteOffset;
  const off = findBox(base, base + seg.byteLength, ["moof", "traf", "tfdt"]);
  if (off === null) return null;
  const version = u8[off + 8];
  return version === 1 ? Number(dv.getBigUint64(off + 12)) : dv.getUint32(off + 12);
};
const mdhdOff = findBox(0, init.byteLength, ["moov", "trak", "mdia", "mdhd"]);
const timescale = mdhdOff === null ? null : dv.getUint32(mdhdOff + 8 + (u8[mdhdOff + 8] === 1 ? 20 : 12));

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

const ws = new WebSocket(`ws://127.0.0.1:${port}/`);
ws.binaryType = "arraybuffer";
ws.onopen = () => {
  ws.send(frame({ type: "hello", token, session: "test-client", url: "file://" + file, title: "test-sink.mjs" }));
  const mime = 'video/mp4; codecs="avc1.64001e, mp4a.40.2"';
  let seq = 1;
  const seg = (payload, isInit) =>
    ws.send(frame({ type: "segment", sb: 1, seq: seq++, mime, isInit, kind: "mp4", tfdt: isInit ? null : tfdtOf(payload), timescale }, payload));
  seg(init, true);
  for (const s of segments) seg(s, false);
  // Re-send init and the second media segment: both must be deduped.
  seg(init, true);
  if (segments[1]) seg(segments[1], false);
  ws.send(frame({ type: "event", name: "endofstream", sb: null }));
  setTimeout(() => ws.close(), 300);
};
ws.onclose = (e) => {
  console.log(`closed code=${e.code}; sent ${segments.length + 1} unique + 2 duplicate segments`);
};
ws.onerror = (e) => {
  console.error("websocket error", e.message || e);
  process.exit(1);
};
