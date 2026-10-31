#!/usr/bin/env node
// Runs extension/src/hook.js in Node with a minimal window / MediaSource /
// SourceBuffer stand-in and checks what it posts:
//   - a clear init + media segment are posted with their bytes
//   - an encrypted init (encv in moov) marks the SourceBuffer: no bytes, an
//     "encrypted" record, and a clear init clears the mark again
//   - the tap switch from content.js stops the copies
//   - timestampOffset travels with each append
//   - a WebM cluster with a corrupt timecode size returns at once
// Usage: node extension/scripts/test-hook.mjs   (exit 0 = all checks passed)
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";
import vm from "node:vm";

const here = dirname(fileURLToPath(import.meta.url));
const hookSrc = readFileSync(join(here, "..", "src", "hook.js"), "utf8");
const failures = [];
const check = (ok, what) => {
  console.log(`${ok ? "ok  " : "FAIL"} ${what}`);
  if (!ok) failures.push(what);
};

// --- environment -----------------------------------------------------------
const posted = [];
const listeners = [];
globalThis.window = globalThis;
globalThis.location = { href: "https://example.test/watch" };
globalThis.postMessage = (data, _origin, transfer) => {
  posted.push({ data, transfer });
  for (const l of listeners) l({ source: globalThis, data });
};
globalThis.addEventListener = (type, fn) => {
  if (type === "message") listeners.push(fn);
};
class SourceBuffer {
  constructor() {
    this._tso = 0;
  }
  appendBuffer() {}
  get timestampOffset() {
    return this._tso;
  }
  set timestampOffset(v) {
    this._tso = v;
  }
}
class MediaSource {
  addSourceBuffer() {
    return new SourceBuffer();
  }
  endOfStream() {}
}
globalThis.SourceBuffer = SourceBuffer;
globalThis.MediaSource = MediaSource;
console.debug = () => {};

vm.runInThisContext(hookSrc, { filename: "hook.js" });

// --- segments ----------------------------------------------------------------
const box = (type, ...children) => {
  const body = Buffer.concat(children.map((c) => Buffer.from(c)));
  const head = Buffer.alloc(8);
  head.writeUInt32BE(8 + body.length, 0);
  head.write(type, 4, "ascii");
  return Buffer.concat([head, body]);
};
const mdhd = () => {
  const b = Buffer.alloc(24); // version 0, flags, ctime, mtime, timescale, duration
  b.writeUInt32BE(90000, 12);
  return box("mdhd", b);
};
const init = (entry) => {
  const moov = box("moov", box("trak", box("mdia", mdhd(), box("minf", box(entry)))));
  return Buffer.concat([box("ftyp", Buffer.from("iso6")), moov]);
};
const media = (tfdt) => {
  const t = Buffer.alloc(8);
  t.writeUInt32BE(tfdt, 4);
  return Buffer.concat([box("moof", box("traf", box("tfdt", t))), box("mdat", Buffer.from("xxxx"))]);
};
const appends = () => posted.map((p) => p.data).filter((d) => d.type === "append");
const last = () => appends().at(-1);
const ab = (buf) => buf.buffer.slice(buf.byteOffset, buf.byteOffset + buf.byteLength);

const ms = new MediaSource();
const sb = ms.addSourceBuffer('video/mp4; codecs="avc1.64001f"');

sb.appendBuffer(ab(init("avc1")));
check(last().isInit && last().payload instanceof ArrayBuffer && !last().encrypted, "clear init is posted with its bytes");
sb.timestampOffset = 12.5;
sb.appendBuffer(ab(media(180000)));
check(last().payload instanceof ArrayBuffer && last().tfdt === 180000, "clear media is posted with its bytes");
check(last().tso === 12.5, "timestampOffset travels with the append");

sb.appendBuffer(ab(init("encv")));
check(last().encrypted === true && last().payload === undefined, "encrypted init: no bytes");
check(posted.some((p) => p.data.type === "encrypted"), "an encrypted record is posted");
sb.appendBuffer(ab(media(270000)));
check(last().encrypted === true && last().payload === undefined, "media after an encrypted init: no bytes");
sb.appendBuffer(ab(init("avc1")));
sb.appendBuffer(ab(media(360000)));
check(last().payload instanceof ArrayBuffer, "a clear init clears the mark again");

globalThis.postMessage({ __vcCtl: 1, tap: false });
sb.appendBuffer(ab(media(450000)));
check(last().payload === undefined, "tap off: metadata only");
globalThis.postMessage({ __vcCtl: 1, tap: true });

// WebM: EBML Cluster (1F 43 B6 75, size 0x88 = 8) whose Timecode (E7) claims 2^48 bytes.
const webm = new Uint8Array([0x1f, 0x43, 0xb6, 0x75, 0x88, 0xe7, 0x02, 0x01, 0, 0, 0, 0, 0, 0]);
const sbw = ms.addSourceBuffer('video/webm; codecs="vp9"');
const t0 = Date.now();
sbw.appendBuffer(webm.buffer);
check(Date.now() - t0 < 200 && last().kind === "webm", "corrupt WebM timecode size returns at once");

console.log(failures.length ? `FAILED: ${failures.length} check(s)` : "ok: all checks passed");
process.exit(failures.length ? 1 : 0);
