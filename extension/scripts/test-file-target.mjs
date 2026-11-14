#!/usr/bin/env node
// The target "file on the device" (extension/src/file-target.js) with the real sink link (extension/src/sink-link.js)
// in a vm context: chrome.storage says `sinkTarget: "file"`, the downloads API and blob URLs are fakes that keep what
// they were given. Fake "vc-sink" ports post hello and segments as content.js does. Checks:
//   saved      a recording holds the magic line, then hello, the init and the media in order; saved once the port
//              closes, under streamtap-<time>-<host>.streamtap, with the bytes the status said
//   no-media   a port that never appends media saves nothing
//   save-now   the popup's "save now" closes the recording; the next segment starts a new one that replays the init
//              in effect first, so each file stands on its own
//   full       beyond the bound media is dropped, `recordfull` is noted once, init bytes are still kept
//   no-api     without the downloads API (a Chrome service worker) nothing is recorded: metadata only
//   video      target "video": real fMP4 segments are saved as one .mp4 (ftyp, moov, moof+mdat)
//   fallback   target "video" with a capture the muxer refuses (WebM): the recording is saved, with the reason
// With `--write <file>` the recording of "saved" is also written there, for `streamtap-sink replay` (the CI checks
// that the sink reads what the extension saves).
// Exit 0 when every check passed.
import { readFileSync, writeFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";
import vm from "node:vm";

const here = dirname(fileURLToPath(import.meta.url));
const src = (n) => readFileSync(join(here, "..", "src", n), "utf8");
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const failures = [];
const check = (ok, what) => {
  console.log(`${ok ? "ok  " : "FAIL"} ${what}`);
  if (!ok) failures.push(what);
};

const MAGIC = "STREAMTAP-RECORDING/1\n";

// A vm context with both scripts; `downloads: false` leaves the API out.
const world = ({ downloads = true, target = "file" } = {}) => {
  const blobs = new Map();
  const saved = [];
  let connect = null;
  let n = 0;
  const chrome = {
    runtime: { onConnect: { addListener: (f) => (connect = f) }, lastError: undefined },
    storage: { local: { get: (d, cb) => setTimeout(() => cb({ ...d, sinkTarget: target }), 0) } },
  };
  if (downloads) {
    chrome.downloads = {
      download: (opts, cb) => {
        saved.push({ ...opts, blob: blobs.get(opts.url) });
        setTimeout(() => cb(saved.length), 0);
      },
    };
  }
  const url = {
    createObjectURL: (b) => {
      const u = `blob:test/${++n}`;
      blobs.set(u, b);
      return u;
    },
    revokeObjectURL: (u) => blobs.delete(u),
  };
  // `new URL(...)` (the host of the file name) and the blob URL functions of a background page.
  const URLx = function (...a) {
    return new URL(...a);
  };
  Object.assign(URLx, url);
  const ctx = vm.createContext({ console, Blob, TextEncoder, TextDecoder, BigInt, Math, DataView, Uint8Array, setTimeout, fetch, URL: URLx, chrome, WebSocket: { CONNECTING: 0, OPEN: 1, CLOSING: 2, CLOSED: 3 } });
  vm.runInContext(src("fmp4-mux.js"), ctx, { filename: "fmp4-mux.js" });
  vm.runInContext(src("file-target.js"), ctx, { filename: "file-target.js" });
  vm.runInContext(src("sink-link.js"), ctx, { filename: "sink-link.js" });
  const port = (tabId, url = "https://www.ardmediathek.de/video/x") => {
    const p = {};
    connect({
      name: "vc-sink",
      sender: { tab: { id: tabId }, frameId: 0, url },
      onMessage: { addListener: (f) => (p.post = f) },
      onDisconnect: { addListener: (f) => (p.gone = f) },
      disconnect: () => {},
    });
    p.post({ t: "hello", session: `file-${tabId}`, title: "test-file-target.mjs" });
    return p;
  };
  return { ctx, saved, port, run: (code) => vm.runInContext(code, ctx) };
};

// A payload is its label, "|", then padding up to `size` bytes.
const body = (label, size = 0) => {
  const b = Buffer.alloc(Math.max(size, label.length + 1), 0x20);
  b.write(`${label}|`, 0, "latin1");
  return "data:application/octet-stream;base64," + b.toString("base64");
};
let seq = 1;
const seg = (label, { init = false, size = 0, sb = 1 } = {}) => ({
  t: "segment",
  header: { sb, seq: seq++, ms: 1, mime: "video/mp4", isInit: init, kind: "mp4", tso: 0 },
  payload: body(label, size),
});

// The messages of a recording: H(ello), event names, segment labels; null when the layout is wrong.
const read = async (blob) => {
  const b = Buffer.from(await blob.arrayBuffer());
  if (b.subarray(0, MAGIC.length).toString("latin1") !== MAGIC) return null;
  const out = [];
  let off = MAGIC.length;
  while (off < b.length) {
    if (off + 4 > b.length) return null;
    const len = b.readUInt32LE(off);
    const msg = b.subarray(off + 4, off + 4 + len);
    if (msg.length !== len) return null;
    const hl = msg.readUInt32LE(0);
    const h = JSON.parse(msg.subarray(4, 4 + hl).toString("utf8"));
    if (h.type === "hello") out.push(h.token === "" ? "H" : "H?");
    else if (h.type === "event") out.push(h.name);
    else out.push(msg.subarray(4 + hl).toString("latin1").split("|")[0]);
    off += 4 + len;
  }
  return out.join(" ");
};
const tick = () => sleep(40);
// Waits until `cond()` holds (decoding a data: URL takes a while), at most 2 s.
const until = async (cond) => {
  for (let i = 0; i < 100 && !cond(); i++) await sleep(20);
};

// saved
{
  const w = world();
  const p = w.port(1);
  p.post(seg("A", { init: true }));
  p.post(seg("a1"));
  p.post(seg("a2"));
  await until(() => w.run("fileStatus(1)").media === 2);
  const st = w.run("fileStatus(1)");
  check(st.recording === 1 && st.media === 2 && st.bytes > 0, `saved: the status counts one recording with 2 media segments (got ${JSON.stringify({ r: st.recording, m: st.media })})`);
  p.gone();
  await tick();
  check(w.saved.length === 1, "saved: one file once the port closed");
  const f = w.saved[0];
  check(/^streamtap-\d{8}-\d{6}-www\.ardmediathek\.de\.streamtap$/.test(f.filename), `saved: the file name (got ${f.filename})`);
  check(f.saveAs === false && f.conflictAction === "uniquify", "saved: no dialog, an existing name is not overwritten");
  check((await read(f.blob)) === "H A a1 a2", `saved: magic, hello, init, media in order (got ${await read(f.blob)})`);
  check(w.run("fileRecordedTotal") === 0, "saved: the bound is given back");
  check(w.run("fileStatus(1)").saved[0].name === f.filename, "saved: the popup is told the name");
  const out = process.argv.indexOf("--write");
  if (out >= 0 && process.argv[out + 1]) writeFileSync(process.argv[out + 1], Buffer.from(await f.blob.arrayBuffer()));
}

// no-media
{
  const w = world();
  const p = w.port(2);
  p.post(seg("A", { init: true }));
  await tick();
  p.gone();
  await tick();
  check(w.saved.length === 0, "no-media: an init without media saves nothing");
}

// save-now
{
  const w = world();
  const p = w.port(3);
  p.post(seg("A", { init: true }));
  p.post(seg("a1"));
  await tick();
  check(w.run("fileSaveTab(3)") === 1, "save-now: one recording closed");
  await tick();
  p.post(seg("a2"));
  await tick();
  p.gone();
  await tick();
  check(w.saved.length === 2, `save-now: two files (got ${w.saved.length})`);
  check((await read(w.saved[0].blob)) === "H A a1", `save-now: the first holds what came before (got ${await read(w.saved[0].blob)})`);
  check((await read(w.saved[1].blob)) === "H A a2", `save-now: the second starts with the init in effect (got ${await read(w.saved[1].blob)})`);
}

// full
{
  const w = world();
  const p = w.port(4);
  p.post(seg("A", { init: true }));
  p.post(seg("a1"));
  await tick();
  w.run("fileRecordedTotal += FILE_MAX_TOTAL");
  p.post(seg("a2"));
  p.post(seg("a3"));
  p.post(seg("B", { init: true }));
  await tick();
  check(w.run("fileStatus(4)").full === true, "full: the popup is told");
  w.run("fileRecordedTotal -= FILE_MAX_TOTAL");
  p.gone();
  await tick();
  check((await read(w.saved[0].blob)) === "H A a1 recordfull B", `full: media dropped, noted once, init kept (got ${await read(w.saved[0].blob)})`);
}

// no-api
{
  const w = world({ downloads: false });
  const p = w.port(5);
  p.post(seg("A", { init: true }));
  p.post(seg("a1"));
  await tick();
  check(w.run("fileStatus(5)").available === false && w.run("fileStatus(5)").recording === 0, "no-api: nothing is recorded");
  p.gone();
  await tick();
  check(w.saved.length === 0, "no-api: nothing saved");
}

// video and fallback: real segments of extension/scripts/fixtures/video-frag.mp4
{
  const { splitFmp4 } = await import("../../desktop/scripts/fmp4-split.mjs");
  const v = splitFmp4(join(here, "fixtures", "video-frag.mp4"));
  const url = (b) => "data:application/octet-stream;base64," + Buffer.from(b).toString("base64");
  const real = (kind) => [
    { t: "segment", header: { sb: 1, seq: seq++, ms: 1, mime: "video/mp4", isInit: true, kind, tso: 0 }, payload: url(v.init) },
    ...v.segments.map((s) => ({ t: "segment", header: { sb: 1, seq: seq++, ms: 1, mime: "video/mp4", isInit: false, kind, tfdt: v.tfdtOf(s), tso: 0 }, payload: url(s) })),
  ];
  const w = world({ target: "video" });
  const p = w.port(6);
  for (const m of real("mp4")) p.post(m);
  await until(() => w.run("fileStatus(6)").media === 3);
  p.gone();
  await until(() => w.saved.length === 1);
  const f = w.saved[0];
  check(f && /\.mp4$/.test(f.filename), `video: saved as .mp4 (got ${f && f.filename})`);
  const b = f ? Buffer.from(await f.blob.arrayBuffer()) : Buffer.alloc(0);
  check(b.toString("latin1", 4, 8) === "ftyp" && b.includes("moov") && b.includes("moof"), "video: an MP4 with ftyp, moov and fragments");
  await until(() => w.run("fileStatus(6)").saved.length === 1);
  const note = w.run("fileStatus(6)").saved[0];
  check(note.video === true && note.report && note.report.gaps === 0, `video: the popup is told it is a video without gaps (got ${JSON.stringify(note)})`);

  const w2 = world({ target: "video" });
  const p2 = w2.port(7);
  for (const m of real("webm")) p2.post(m);
  await until(() => w2.run("fileStatus(7)").media === 3);
  p2.gone();
  await until(() => w2.saved.length === 1);
  const f2 = w2.saved[0];
  check(f2 && /\.streamtap$/.test(f2.filename), `fallback: saved as the recording (got ${f2 && f2.filename})`);
  await until(() => w2.run("fileStatus(7)").saved.length === 1);
  const n2 = w2.run("fileStatus(7)").saved[0];
  check(typeof n2.fallback === "string" && n2.fallback.includes("WebM"), `fallback: the reason is said (got ${n2.fallback})`);
  check(n2.reason === "webm", `fallback: and named by its code, for the popup's own language (got ${n2.reason})`);
}

if (failures.length) {
  console.error(`FAIL: ${failures.length} check(s) failed`);
  process.exit(1);
}
console.log("ok: all checks passed");
// The blob URLs are let go on timers of two minutes: not waited for.
process.exit(0);
