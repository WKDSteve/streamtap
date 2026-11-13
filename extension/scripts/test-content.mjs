#!/usr/bin/env node
// Drives extension/src/content.js in Node with a mocked chrome.runtime: hook
// records go in as window messages, port messages are recorded. Checks what
// a port after the first one (the service worker restarted) is sent before
// the media: the kept inits, and a "lost" note for an init that could not be
// kept (too large), so the worker says initlost instead of filing media under
// no init. An init dropped on the way (full queue, a failing post, held
// before tapping started) is not replaced by the stale kept one: the port
// gets a "lost" note with drop at its place and none of that SourceBuffer's
// media until its next init; an init+media append still sends its init part.
// Also runs the service worker's registry (background.js) on the records the
// script forwards: the rest of a split init is no media segment, and a
// changetype only touches the SourceBuffer of the frame it came from.
// Usage: node extension/scripts/test-content.mjs   (exit 0 when all checks pass)
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";
import vm from "node:vm";

const here = dirname(fileURLToPath(import.meta.url));
const failures = [];
const check = (ok, what) => {
  console.log(`${ok ? "ok  " : "FAIL"} ${what}`);
  if (!ok) failures.push(what);
};
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

// --- page and extension mocks ------------------------------------------------
let listener = null;
globalThis.window = globalThis;
globalThis.location = { href: "https://example.test/watch" };
globalThis.document = { title: "test-content.mjs", addEventListener: () => {} };
globalThis.addEventListener = (type, f) => {
  if (type === "message") listener = f;
};
globalThis.postMessage = () => {};
globalThis.FileReader = class {
  readAsDataURL(blob) {
    blob.arrayBuffer().then((b) => {
      this.result = "data:application/octet-stream;base64," + Buffer.from(b).toString("base64");
      this.onload();
    });
  }
};
const ports = [];
const forwarded = []; // records sent to the service worker
globalThis.chrome = {
  runtime: {
    id: "oggpddakdbffbjhkjmcakidgmhpgakia",
    lastError: null,
    onMessage: { addListener: () => {} },
    sendMessage: (m, cb) => {
      forwarded.push(m);
      if (cb) cb();
    },
    connect: () => {
      const p = { sent: [], gone: [], failNext: false };
      p.postMessage = (m) => {
        // A segment post that throws once (a port going away).
        if (p.failNext && m.t === "segment") {
          p.failNext = false;
          throw new Error("Attempting to use a disconnected port object");
        }
        p.sent.push(m);
      };
      p.onDisconnect = { addListener: (f) => p.gone.push(f) };
      p.disconnect = () => {};
      ports.push(p);
      return p;
    },
  },
  storage: {
    // The source policy (on by default) is tested in test-source-policy.mjs.
    local: { get: (d, cb) => setTimeout(() => cb({ ...d, sinkToken: "t", sourcePolicy: "off" }), 5) },
    onChanged: { addListener: () => {} },
  },
};

vm.runInThisContext(readFileSync(join(here, "..", "src", "content.js"), "utf8"), { filename: "content.js" });
await sleep(30); // the token is read: tapping

let seq = 1;
const append = (sbId, isInit, bytes) =>
  listener({
    source: window,
    data: { __vc: 1, type: "append", sbId, msId: 1, seq: seq++, mime: "video/mp4", kind: "mp4", isInit, payload: new ArrayBuffer(bytes) },
  });
// The worker restarts: the port goes, the next segment opens a new one.
const restart = () => {
  for (const f of ports[ports.length - 1].gone) f();
};
// X: a "lost" note with drop (an init dropped in a running stream).
const shape = (p) =>
  p.sent
    .map((m) =>
      m.t === "segment"
        ? `${m.header.isInit ? "I" : "M"}${m.header.sb}`
        : m.t === "lost"
          ? `${m.header.drop ? "X" : "L"}${m.header.sb}`
          : m.t[0].toUpperCase()
    )
    .join(" ");

append(1, true, 1100 * 1024); // over 1 MiB: not kept for a new port
append(2, true, 500); // kept
append(1, false, 100);
await sleep(100);
check(ports.length === 1, `one port so far (got ${ports.length})`);

restart();
append(1, false, 100);
await sleep(100);
check(ports.length === 2, `a new port after the restart (got ${ports.length})`);
check(shape(ports[1]) === "H L1 I2 M1", `new port: hello, sb1 lost, sb2's init replayed, then media (got ${shape(ports[1])})`);

// A later init of sb1 that fits is kept: the next port replays it, no note.
append(1, true, 600);
await sleep(100);
restart();
append(1, false, 100);
await sleep(100);
check(shape(ports[2]) === "H I2 I1 M1", `third port: both inits replayed, nothing lost (got ${shape(ports[2])})`);

// --- inits dropped on the way ----------------------------------------------------
const MiB = 1024 * 1024;
const contentSrc = readFileSync(join(here, "..", "src", "content.js"), "utf8");
const rec = (msId, sbId, fields, bytes) =>
  listener({
    source: window,
    data: { __vc: 1, type: "append", sbId, msId, seq: seq++, mime: "video/mp4", kind: "mp4", isInit: false, ...fields, payload: new ArrayBuffer(bytes) },
  });
const payloadLen = (m) => Buffer.from(m.payload.split(",")[1], "base64").length;
{
  // The queue is full when an init+media append (VHS quality switch) comes.
  const { stats, links } = window.__vcLink;
  const first = ports.length;
  rec(2, 1, { isInit: true }, 500);
  rec(2, 1, {}, 100);
  await sleep(100);
  const p = ports[first];
  const pad = 64 * MiB - stats.queuedBytes;
  stats.queuedBytes += pad;
  rec(2, 1, { isInit: true, initLen: 700, tfdt: 9000, dur: 10 }, 2 * MiB);
  stats.queuedBytes -= pad;
  rec(2, 1, {}, 100);
  await sleep(100);
  check(shape(p) === "H I1 M1 I1 M1", `full queue, init+media: its init part still goes (got ${shape(p)})`);
  const part = p.sent.find((m, i) => i > 1 && m.t === "segment" && m.header.isInit);
  check(
    part && payloadLen(part) === 700 && part.header.initLen === null && part.header.tfdt === null,
    `full queue, init+media: only the init part, no media fields (got ${part && payloadLen(part)} bytes)`
  );
  {
    rec(2, 1, { tfdt: 5, dur: 1, track: 2 }, 100);
    rec(2, 1, { tfdt: 6, dur: 1, track: 0 }, 100);
    rec(2, 1, { tfdt: 7, dur: 1, track: "2" }, 100);
    await sleep(100);
    const timed = p.sent.filter((m) => m.t === "segment" && [5, 6, 7].includes(m.header.tfdt));
    check(
      timed.length === 1 && timed[0].header.tfdt === 5 && timed[0].header.track === 2,
      `the track ID of the first traf is passed on, a forged one drops the record (got ${timed.map((m) => m.header.tfdt)})`
    );
  }
  const kept = links.get("2").inits.get(1);
  check(kept && kept[0].payload.byteLength === 700, "full queue, init+media: that init part is the one kept");
}
{
  // A large init that does not fit: noted, its media held back, not replayed stale.
  const { stats, links } = window.__vcLink;
  const first = ports.length;
  rec(3, 1, { isInit: true }, 500);
  rec(3, 1, {}, 100);
  await sleep(100);
  const p = ports[first];
  const pad = 64 * MiB - stats.queuedBytes;
  stats.queuedBytes += pad;
  rec(3, 1, { isInit: true }, 2 * MiB);
  stats.queuedBytes -= pad;
  rec(3, 1, {}, 100);
  await sleep(100);
  check(shape(p) === "H I1 M1 X1", `full queue, large init: lost noted, its media not sent (got ${shape(p)})`);
  const link = links.get("3");
  check(!link.inits.has(1) && link.unkept.has(1), "full queue, large init: the previous init is no longer kept");
  for (const f of p.gone) f();
  rec(3, 1, { isInit: true }, 600);
  rec(3, 1, {}, 100);
  await sleep(100);
  const q = ports[ports.length - 1];
  check(q !== p && shape(q) === "H L1 I1 M1", `full queue, large init: the next port gets no stale init (got ${shape(q)})`);
}
{
  // The post of an init throws (the port is going away).
  const first = ports.length;
  rec(4, 1, { isInit: true }, 500);
  rec(4, 1, {}, 100);
  await sleep(100);
  const p = ports[first];
  p.failNext = true;
  rec(4, 1, { isInit: true }, 600);
  rec(4, 1, {}, 100);
  rec(4, 1, { isInit: true }, 700);
  rec(4, 1, {}, 100);
  await sleep(100);
  check(shape(p) === "H I1 M1 X1 I1 M1", `failed post of an init: lost noted, media up to the next init held back (got ${shape(p)})`);
}

// --- an init held before tapping started that did not fit -------------------------
let release = null;
chrome.storage.local.get = (d, cb) => {
  release = () => cb({ ...d, sinkToken: "t", sourcePolicy: "off" });
};
vm.runInThisContext(contentSrc, { filename: "content.js" }); // a second frame, still reading the token
{
  const { stats } = window.__vcLink;
  const first = ports.length;
  rec(5, 1, { isInit: true }, 500);
  rec(5, 1, {}, 64 * MiB - 500); // the held budget is full now
  rec(5, 1, { isInit: true }, 600); // dropped
  rec(5, 1, {}, 100);
  release();
  for (let i = 0; i < 100 && stats.queuedBytes > 0; i++) await sleep(20);
  rec(5, 1, {}, 100);
  rec(5, 1, { isInit: true }, 700);
  rec(5, 1, {}, 100);
  await sleep(100);
  const p = ports[first];
  check(p && shape(p) === "H I1 M1 X1 I1 M1", `held init dropped: noted at its place once tapping starts (got ${p && shape(p)})`);
}

// --- the service worker's registry (background.js) -------------------------------
const bg = { listener: null, badges: [] };
const bgCtx = vm.createContext({
  console,
  importScripts: () => {},
  // native-pairing.js is not loaded here (test-pairing.mjs covers it): inert stand-ins.
  npInit: () => {},
  npScheduleSend: () => {},
  npTabClosed: () => {},
  npStatus: () => ({}),
  npRetryNow: () => {},
  npTapState: async () => ({ on: false }),
  chrome: {
    runtime: { onMessage: { addListener: (f) => (bg.listener = f) } },
    action: {
      setBadgeText: (o) => {
        bg.badges.push(o.text);
        return Promise.resolve();
      },
      setBadgeBackgroundColor: () => Promise.resolve(),
    },
    tabs: { onRemoved: { addListener: () => {} }, onUpdated: { addListener: () => {} } },
  },
});
vm.runInContext(readFileSync(join(here, "..", "src", "background.js"), "utf8"), bgCtx, { filename: "background.js" });
const toBg = (event, url, tabId) => bg.listener({ kind: "vc-event", event }, { tab: { id: tabId, url }, url }, () => {});
const sbOf = (tabId, frameUrl, msId, sbId) =>
  vm.runInContext(`tabs.get(${tabId})`, bgCtx).ms.get(`${frameUrl}#${msId}`).sb.get(sbId);
{
  // An init split across two appends (ftyp, then moov), forwarded by content.js.
  const from = forwarded.length;
  rec(6, 1, { isInit: true, boxes: ["ftyp"] }, 32);
  rec(6, 1, { initCont: true, boxes: ["moov"] }, 64);
  const records = forwarded.slice(from).map((m) => m.event).filter((e) => e.type === "append");
  for (const e of records) toBg(e, location.href, 7);
  const s = sbOf(7, location.href, 6, 1);
  check(records.length === 2 && records[1].initCont === true, "registry: content.js forwards initCont");
  check(s.inits === 1 && s.media === 0 && s.appends === 2, `registry: the rest of a split init is no media segment (init ${s.inits}, media ${s.media})`);
  check(bg.badges.at(-1) === "", `registry: no badge before any media (got "${bg.badges.at(-1)}")`);
  const at = forwarded.length;
  rec(6, 1, { boxes: ["moof", "mdat"] }, 100);
  for (const m of forwarded.slice(at)) if (m.event.type === "append") toBg(m.event, location.href, 7);
  check(s.media === 1 && bg.badges.at(-1) === "1", `registry: the first media segment counts (media ${s.media}, badge "${bg.badges.at(-1)}")`);
}
{
  // Two frames of one tab, each with its SourceBuffer #1; the embedded one changes type.
  const top = "https://top.test/watch";
  const embed = "https://player.test/embed";
  for (const [url, msId] of [[top, 1], [embed, 1], [top, null], [embed, null]]) {
    toBg({ type: "sourcebuffer", msId, sbId: 1, mime: 'video/mp4; codecs="avc1"' }, url, 8);
  }
  toBg({ type: "changetype", msId: 1, sbId: 1, mime: 'video/mp4; codecs="hvc1"' }, embed, 8);
  toBg({ type: "changetype", msId: null, sbId: 1, mime: 'video/mp4; codecs="vp09"' }, embed, 8);
  const a = sbOf(8, top, 1, 1);
  const b = sbOf(8, embed, 1, 1);
  check(a.mime.includes("avc1") && a.mimes.length === 0, `registry: changetype leaves another frame's SourceBuffer #1 alone (got ${a.mime})`);
  check(b.mime.includes("hvc1") && b.mimes.length === 1, `registry: changetype renames the sending frame's one (got ${b.mime})`);
  const c = sbOf(8, top, null, 1);
  const d = sbOf(8, embed, null, 1);
  check(c.mime.includes("avc1") && d.mime.includes("vp09"), "registry: changetype of a pre-hook SourceBuffer (msId null) as well");
}

{
  // Forged init records with ever new sb ids: every init beyond the 16 kept pushes the oldest into `unkept`, which
  // used to grow without a bound (each new port then got one `lost` per entry).
  const { links } = window.__vcLink;
  for (let sb = 1; sb <= 300; sb++) rec(30, sb, { isInit: true }, 50);
  await sleep(150);
  const link = links.get("30");
  check(link && link.unkept.size <= 64, `forged inits: unkept stays bounded (got ${link && link.unkept.size})`);
  check(link.inits.size <= 16, `forged inits: the kept inits stay bounded (got ${link.inits.size})`);
}

console.log(failures.length ? `FAILED: ${failures.length} check(s)` : "ok: all checks passed");
process.exit(failures.length ? 1 : 0);
