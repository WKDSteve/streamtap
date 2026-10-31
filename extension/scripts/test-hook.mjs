#!/usr/bin/env node
// Runs extension/src/hook.js in Node with a minimal window / document /
// MediaSource / SourceBuffer / HTMLMediaElement stand-in and checks what it
// posts. Usage: node extension/scripts/test-hook.mjs   (exit 0 = all passed)
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
const docListeners = {};
globalThis.window = globalThis;
globalThis.location = { href: "https://example.test/watch" };
globalThis.postMessage = (data, _origin, transfer) => {
  posted.push({ data, transfer });
  for (const l of listeners) l({ source: globalThis, data });
};
globalThis.addEventListener = (type, fn) => {
  if (type === "message") listeners.push(fn);
};
globalThis.document = {
  addEventListener: (type, fn) => {
    (docListeners[type] ||= []).push(fn);
  },
};
class SourceBuffer {
  constructor() {
    this._tso = 0;
    this.mode = "segments";
    this.fail = false;
  }
  appendBuffer() {
    if (this.fail) throw new Error("QuotaExceededError");
  }
  get timestampOffset() {
    return this._tso;
  }
  set timestampOffset(v) {
    this._tso = v;
  }
}
class MediaSource {
  constructor() {
    this.handlers = {};
  }
  addSourceBuffer() {
    return new SourceBuffer();
  }
  endOfStream() {}
  addEventListener(type, fn) {
    this.handlers[type] = fn;
  }
}
class HTMLMediaElement {
  setMediaKeys() {
    return Promise.resolve();
  }
}
globalThis.SourceBuffer = SourceBuffer;
globalThis.MediaSource = MediaSource;
globalThis.HTMLMediaElement = HTMLMediaElement;
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
const tfhd = (track) => {
  const b = Buffer.alloc(8); // fullbox, track_ID
  b.writeUInt32BE(track, 4);
  return box("tfhd", b);
};
const tfdt = (v, version = 0) => {
  const b = Buffer.alloc(version ? 12 : 8);
  b[0] = version;
  if (version) b.writeBigUInt64BE(BigInt(v), 4);
  else b.writeUInt32BE(v, 4);
  return box("tfdt", b);
};
const media = (t, version = 0) =>
  Buffer.concat([box("moof", box("traf", tfhd(1), tfdt(t, version))), box("mdat", Buffer.from("xxxx"))]);
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

// Two trafs (two tracks): the first tfdt is the append's start.
const twoTrafs = Buffer.concat([
  box("moof", box("traf", tfhd(1), tfdt(1000)), box("traf", tfhd(2), tfdt(5))),
  box("mdat", Buffer.from("x")),
]);
sb.appendBuffer(ab(twoTrafs));
check(last().tfdt === 1000, "the first traf's tfdt is kept");

// Above 2^53 the tfdt goes as an exact decimal string.
sb.appendBuffer(ab(media("18446744073709551615", 1)));
check(last().tfdt === "18446744073709551615", "a 64-bit tfdt stays exact as a string");

sb.mode = "sequence";
sb.appendBuffer(ab(media(200000)));
check(last().seqMode === true, "sequence mode is reported");
sb.mode = "segments";

// A rejected append is not recorded.
const before = appends().length;
sb.fail = true;
try {
  sb.appendBuffer(ab(media(210000)));
} catch (e) {
  // expected
}
sb.fail = false;
check(appends().length === before, "an append the browser rejects is not recorded");

globalThis.postMessage({ __vcCtl: 1, tap: false });
sb.appendBuffer(ab(media(450000)));
check(last().payload === undefined, "tap off: metadata only");
globalThis.postMessage({ __vcCtl: 1, tap: true });

// Encrypted init on a second SourceBuffer: marked for good.
const sb2 = ms.addSourceBuffer('video/mp4; codecs="avc1.64001f"');
sb2.appendBuffer(ab(init("encv")));
check(last().encrypted === true && last().payload === undefined, "encrypted init: no bytes");
check(posted.some((p) => p.data.type === "encrypted"), "an encrypted record is posted");
sb2.appendBuffer(ab(media(270000)));
check(last().payload === undefined, "media after an encrypted init: no bytes");
sb2.appendBuffer(ab(init("avc1")));
sb2.appendBuffer(ab(media(360000)));
check(last().payload === undefined, "the mark is sticky: a later clear init does not clear it");

// An encrypted init split across two appends; the encv marker is in the second half.
const sb3 = ms.addSourceBuffer('video/mp4; codecs="avc1.64001f"');
const enc = init("encv");
sb3.appendBuffer(ab(enc.subarray(0, enc.length - 12)));
sb3.appendBuffer(ab(enc.subarray(enc.length - 12)));
check(last().encrypted === true && last().payload === undefined, "split encrypted init: the second half is caught");
sb3.appendBuffer(ab(media(100)));
check(last().payload === undefined, "split encrypted init: media after it is not tapped");

// The MediaSource closing is reported for the content script.
ms.handlers.sourceclose();
check(posted.some((p) => p.data.type === "msclose"), "sourceclose posts msclose");

// WebM: Cluster whose Timecode claims 2^40 bytes.
const webm = new Uint8Array([0x1f, 0x43, 0xb6, 0x75, 0x88, 0xe7, 0x02, 0x01, 0, 0, 0, 0, 0, 0]);
const sbw = ms.addSourceBuffer('video/webm; codecs="vp9"');
const t0 = Date.now();
sbw.appendBuffer(webm.buffer);
check(Date.now() - t0 < 200 && last().kind === "webm", "corrupt WebM timecode size returns at once");

// EME in the frame: nothing is tapped any more, anywhere.
new HTMLMediaElement().setMediaKeys({});
sb.appendBuffer(ab(media(900000)));
check(last().payload === undefined && last().encrypted === true, "after setMediaKeys nothing is tapped");
check(posted.some((p) => p.data.type === "drm" && p.data.keySystem === "setMediaKeys"), "setMediaKeys is reported as DRM");
check((docListeners.encrypted || []).length === 1, "the encrypted event is watched");

console.log(failures.length ? `FAILED: ${failures.length} check(s)` : "ok: all checks passed");
process.exit(failures.length ? 1 : 0);
