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

// Split inside the moov's own 8-byte header: the first chunk has no moov at all.
const sb4 = ms.addSourceBuffer('video/mp4; codecs="avc1.64001f"');
const enc4 = init("encv");
const ftypLen = 8 + 4;
sb4.appendBuffer(ab(enc4.subarray(0, ftypLen + 4)));
sb4.appendBuffer(ab(enc4.subarray(ftypLen + 4)));
check(last().initCont === true, "the rest of a split init is flagged initCont");
check(last().encrypted === true && last().payload === undefined, "init split inside the moov header: caught");

// A clear init together with media whose mdat happens to contain "encv": not DRM.
const sb5 = ms.addSourceBuffer('video/mp4; codecs="avc1.64001f"');
const combined = Buffer.concat([init("avc1"), box("moof", box("traf", tfhd(1), tfdt(0))), box("mdat", Buffer.from("..encv.."))]);
sb5.appendBuffer(ab(combined));
check(last().encrypted === false && last().payload instanceof ArrayBuffer, "encv inside mdat of a clear init+media append is not DRM");

// WebM: an encrypted Tracks element split across two appends.
const ebml = (id, body) => {
  const idBytes = [];
  for (let v = id; v > 0; v = Math.floor(v / 256)) idBytes.unshift(v & 0xff);
  return Buffer.concat([Buffer.from(idBytes), Buffer.from([0x80 | body.length]), body]);
};
const webmInit = Buffer.concat([
  ebml(0x1a45dfa3, Buffer.from([0x42, 0x82, 0x84, 0x77, 0x65, 0x62, 0x6d])),
  ebml(0x1549a966, ebml(0x2ad7b1, Buffer.from([0x0f, 0x42, 0x40]))), // TimecodeScale 1,000,000 ns
  ebml(0x1654ae6b, ebml(0xae, ebml(0x6d80, ebml(0x6240, ebml(0x5035, ebml(0x47e2, Buffer.from([1, 2, 3, 4]))))))),
]);
const sbw2 = ms.addSourceBuffer('video/webm; codecs="vp9"');
sbw2.appendBuffer(ab(webmInit.subarray(0, webmInit.length - 9)));
sbw2.appendBuffer(ab(webmInit.subarray(webmInit.length - 9)));
check(last().encrypted === true && last().payload === undefined, "split encrypted WebM Tracks: caught");
const sbw3 = ms.addSourceBuffer('video/webm; codecs="vp9"');
sbw3.appendBuffer(ab(webmInit));
check(last().encrypted === true, "whole encrypted WebM Tracks: caught by structure");
const clearWebm = Buffer.concat([
  ebml(0x1a45dfa3, Buffer.from([0x42, 0x82, 0x84, 0x77, 0x65, 0x62, 0x6d])),
  ebml(0x1549a966, ebml(0x2ad7b1, Buffer.from([0x0f, 0x42, 0x40]))),
  ebml(0x1654ae6b, ebml(0xae, Buffer.from([0x86, 0x82, 0x56, 0x39]))),
]);
const sbw4 = ms.addSourceBuffer('video/webm; codecs="vp9"');
sbw4.appendBuffer(ab(clearWebm));
check(last().encrypted === false && last().timescale === 1000, "clear WebM init: tapped, TimecodeScale read (1 ms -> 1000/s)");

// WebM init split at an element boundary: the second append starts with Tracks.
const tracksAt = webmInit.indexOf(Buffer.from([0x16, 0x54, 0xae, 0x6b]));
const sbw5 = ms.addSourceBuffer('video/webm; codecs="vp9"');
sbw5.appendBuffer(ab(webmInit.subarray(0, tracksAt)));
sbw5.appendBuffer(ab(webmInit.subarray(tracksAt)));
check(last().initCont === true && last().encrypted === true, "WebM split before Tracks: continuation, encryption read");

// TimecodeScale 500 us survives a later Cluster-only append.
const ts500 = Buffer.concat([
  ebml(0x1a45dfa3, Buffer.from([0x42, 0x82, 0x84, 0x77, 0x65, 0x62, 0x6d])),
  ebml(0x1549a966, ebml(0x2ad7b1, Buffer.from([0x07, 0xa1, 0x20]))), // 500,000 ns
  ebml(0x1654ae6b, ebml(0xae, Buffer.from([0x86, 0x82, 0x56, 0x39]))),
]);
const sbw6 = ms.addSourceBuffer('video/webm; codecs="vp9"');
sbw6.appendBuffer(ab(ts500));
sbw6.appendBuffer(ab(Buffer.from([0x1f, 0x43, 0xb6, 0x75, 0x84, 0xe7, 0x82, 0x07, 0xd0])));
check(last().timescale === 2000 && last().startSec === 1, "WebM TimecodeScale kept for Cluster appends (2000 ticks = 1 s)");

// Split inside ftyp: the first chunk has not even a complete box.
const sb6 = ms.addSourceBuffer('video/mp4; codecs="avc1.64001f"');
const enc6 = init("encv");
const before6 = appends().length;
sb6.appendBuffer(ab(enc6.subarray(0, 6)));
check(appends().length === before6, "6 bytes cannot be told apart yet; they are held, nothing posted");
sb6.appendBuffer(ab(enc6.subarray(6)));
check(
  last().isInit === true && last().bytes === enc6.length && last().encrypted === true,
  "init split inside the ftyp header: posted joined as the init, and caught"
);
const sb6b = ms.addSourceBuffer('video/mp4; codecs="avc1.64001f"');
sb6b.appendBuffer(ab(enc6.subarray(0, 10)));
check(last().isInit === true, "a cut-off ftyp box still starts an init");
sb6b.appendBuffer(ab(enc6.subarray(10)));
check(last().initCont === true && last().encrypted === true, "init split inside the ftyp body: the rest is caught");

// After a complete init, a media chunk without box header that contains "encv" is not DRM.
const sb7 = ms.addSourceBuffer('video/mp4; codecs="avc1.64001f"');
sb7.appendBuffer(ab(init("avc1")));
sb7.appendBuffer(ab(Buffer.from("xxxxencvxxxxxxxxxxxx")));
check(last().encrypted === false && last().initCont === false, "media bytes after a complete init are never scanned as init");

// A split clear init: its timescale is read once it is complete.
const sb8 = ms.addSourceBuffer('video/mp4; codecs="avc1.64001f"');
const clear8 = init("avc1");
sb8.appendBuffer(ab(clear8.subarray(0, 20)));
sb8.appendBuffer(ab(clear8.subarray(20)));
sb8.appendBuffer(ab(media(90000)));
check(last().timescale === 90000 && last().startSec === 1, "a split init still gives the timescale");

// A combined init + media append reports where the init ends.
const sb9 = ms.addSourceBuffer('video/mp4; codecs="avc1.64001f"');
const ini9 = init("avc1");
sb9.appendBuffer(ab(Buffer.concat([ini9, media(0)])));
check(last().isInit === true && last().initLen === ini9.length, "initLen marks the init part of an init+media append");

// A clear init whose first append is a short head: one init record with all bytes.
const sb10 = ms.addSourceBuffer('video/mp4; codecs="avc1.64001f"');
const ini10 = init("avc1");
const before10 = appends().length;
sb10.appendBuffer(ab(ini10.subarray(0, 5)));
sb10.appendBuffer(ab(ini10.subarray(5)));
const recs10 = appends().slice(before10);
check(
  recs10.length === 1 && recs10[0].isInit === true && recs10[0].payload.byteLength === ini10.length,
  "a short init head is posted together with its rest, as the init"
);

// Init split right after ftyp: the moov is its continuation, not a new init.
const sb11 = ms.addSourceBuffer('video/mp4; codecs="avc1.64001f"');
const ini11 = init("avc1");
const ftyp11 = ini11.readUInt32BE(0);
sb11.appendBuffer(ab(ini11.subarray(0, ftyp11)));
sb11.appendBuffer(ab(ini11.subarray(ftyp11)));
check(last().isInit === false && last().initCont === true, "init split after ftyp: the moov continues it");

// WebM init split right after the EBML header: the Segment continues it.
const segWebm = Buffer.concat([
  Buffer.from([0x18, 0x53, 0x80, 0x67, 0x01, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff]),
  ebml(0x1549a966, ebml(0x4d80, Buffer.from("x"))), // Info without TimecodeScale
  ebml(0x1654ae6b, ebml(0xae, Buffer.from([0x86, 0x82, 0x56, 0x39]))),
]);
const ebmlHead = ebml(0x1a45dfa3, Buffer.from([0x42, 0x82, 0x84, 0x77, 0x65, 0x62, 0x6d]));
const sbw7 = ms.addSourceBuffer('video/webm; codecs="vp9"');
sbw7.appendBuffer(ab(ts500));
sbw7.appendBuffer(ab(ebmlHead));
check(last().isInit === true, "an EBML header alone starts an init");
sbw7.appendBuffer(ab(segWebm));
check(last().isInit === false && last().initCont === true, "WebM split after the EBML header: the Segment continues it");
// That init has no TimecodeScale: 1 ms, not the 500 us of the init before.
sbw7.appendBuffer(ab(Buffer.from([0x1f, 0x43, 0xb6, 0x75, 0x84, 0xe7, 0x82, 0x07, 0xd0])));
check(last().timescale === 1000 && last().startSec === 2, "a WebM init without TimecodeScale resets to 1 ms");

// Per-segment boxes in front of the moof are not init bytes.
const sb12 = ms.addSourceBuffer('video/mp4; codecs="avc1.64001f"');
const ini12 = init("avc1");
sb12.appendBuffer(ab(Buffer.concat([ini12, box("styp", Buffer.from("msdh")), box("sidx", Buffer.alloc(4)), media(0)])));
check(last().initLen === ini12.length, "initLen ends before styp/sidx");

// The head of a new split init does not carry the previous init's timescale.
sb8.appendBuffer(ab(clear8.subarray(0, 20)));
check(last().isInit === true && last().timescale === null, "the head of a split init has no stale timescale");

// A set timestampOffset is flagged on the next append only.
const sb13 = ms.addSourceBuffer('video/mp4; codecs="avc1.64001f"');
sb13.appendBuffer(ab(init("avc1")));
sb13.timestampOffset = 4;
sb13.appendBuffer(ab(media(0)));
check(last().tsoSet === true && last().tso === 4, "a set timestampOffset is flagged");
sb13.appendBuffer(ab(media(90000)));
check(last().tsoSet === false, "only on the append right after it");
sb13.timestampOffset = 8;
sb13.appendBuffer(ab(init("avc1")));
check(last().isInit === true && last().tsoSet === false, "an init append does not use it up");
sb13.appendBuffer(ab(media(0)));
check(last().tsoSet === true, "the media after the init carries it");

// A short tail of media, then an init: both posted on their own, the init intact.
const sb14 = ms.addSourceBuffer('video/mp4; codecs="avc1.64001f"');
const ini14 = init("avc1");
const before14 = appends().length;
sb14.appendBuffer(ab(Buffer.from([1, 2, 3])));
sb14.appendBuffer(ab(ini14));
const recs14 = appends().slice(before14);
check(
  recs14.length === 2 && recs14[0].bytes === 3 && recs14[0].isInit === false && recs14[1].isInit === true && recs14[1].bytes === ini14.length,
  "a short tail before an init does not swallow it"
);

// An init+index range (ftyp moov sidx, no moof) is all init.
const sb15 = ms.addSourceBuffer('video/mp4; codecs="avc1.64001f"');
const ini15 = Buffer.concat([init("avc1"), box("sidx", Buffer.alloc(4))]);
sb15.appendBuffer(ab(ini15));
check(last().isInit === true && last().initLen === null, "a trailing sidx without moof stays init");
sb15.appendBuffer(ab(media(0)));
check(last().isInit === false && last().initCont === false, "the init after it was complete");

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
