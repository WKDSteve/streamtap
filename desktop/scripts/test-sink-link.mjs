#!/usr/bin/env node
// Drives the extension's service-worker sink link (extension/src/sink-link.js)
// in Node against a running sink: mocks chrome.runtime/chrome.storage, opens
// fake "vc-sink" ports and posts hello + segments exactly as content.js does
// (payloads as data: URLs), then checks what the sink wrote to disk.
//
// Scenarios (one port and one tab each):
//   reconnect   init + half the media right after hello (queued while the
//               settings are read), the socket closes as if the sink
//               restarted, the rest follows: the second capture directory
//               must start with the replayed init.
//   first-fail  the first socket cannot connect (wrong port): the init dropped
//               then must still reach the sink once a socket opens.
//   switch      a quality switch arrives while reconnecting: the old init must
//               precede the old media, the new init the new media.
//   evicted     the kept init was dropped for the budget: the new socket says
//               initlost, drops queued media up to the next queued init and
//               takes everything after it.
//   kept        (no sockets) a page or an ad frame filling the kept-init
//               budget loses its own inits, not another tab's or frame's.
// Usage:
//   node scripts/test-sink-link.mjs <fragmented.mp4> --token <t> --captures <sink --out dir>
//        [--port 17650] [--origin chrome-extension://oggpddakdbffbjhkjmcakidgmhpgakia]
// Exit 0 when every check passed.
import { readFileSync, readdirSync, existsSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";
import vm from "node:vm";
import { splitFmp4 } from "./fmp4-split.mjs";

const args = process.argv.slice(2);
const file = args.find((a) => !a.startsWith("--"));
const flag = (n, d) => {
  const i = args.indexOf(n);
  return i >= 0 ? args[i + 1] : d;
};
const token = flag("--token");
const port = Number(flag("--port", "17650"));
const captures = flag("--captures");
const origin = flag("--origin", "chrome-extension://oggpddakdbffbjhkjmcakidgmhpgakia");
if (!file || !token || !captures) {
  console.error("usage: test-sink-link.mjs <fragmented.mp4> --token <t> --captures <dir> [--port 17650] [--origin <o>]");
  process.exit(2);
}

const here = dirname(fileURLToPath(import.meta.url));
const linkSrc = readFileSync(join(here, "..", "..", "extension", "src", "sink-link.js"), "utf8");
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const failures = [];
const check = (ok, what) => {
  console.log(`${ok ? "ok  " : "FAIL"} ${what}`);
  if (!ok) failures.push(what);
};

// --- chrome mock ---------------------------------------------------------
let onConnect = null;
let portOverride = null; // the next storage read answers with this port once
globalThis.chrome = {
  runtime: { onConnect: { addListener: (f) => (onConnect = f) } },
  storage: {
    local: {
      // async like the real one, so the hello/segment race is exercised
      get: (defaults, cb) =>
        setTimeout(() => {
          const p = portOverride ?? port;
          portOverride = null;
          cb({ ...defaults, sinkPort: p, sinkToken: token });
        }, 50),
    },
  },
};
// A browser sends the extension origin by itself; Node's WebSocket needs it spelled out.
const NodeWebSocket = globalThis.WebSocket;
globalThis.WebSocket = class extends NodeWebSocket {
  constructor(url) {
    super(url, { headers: { Origin: origin } });
  }
};

vm.runInThisContext(linkSrc, { filename: "sink-link.js" });

const fakePort = (tabId, session) => {
  const p = { onMessage: null, onDisconnect: null };
  onConnect({
    name: "vc-sink",
    sender: { tab: { id: tabId }, frameId: 0, url: "https://example.test/watch" },
    onMessage: { addListener: (f) => (p.onMessage = f) },
    onDisconnect: { addListener: (f) => (p.onDisconnect = f) },
    disconnect: () => {},
  });
  p.onMessage({ t: "hello", session, title: "test-sink-link.mjs" });
  return p;
};

const { init, segments, tfdtOf, timescale } = splitFmp4(file);
const dataUrl = (u8) => "data:application/octet-stream;base64," + Buffer.from(u8).toString("base64");
const mime = 'video/mp4; codecs="avc1.64001e, mp4a.40.2"';
let seq = 1;
const seg = (p, u8, isInit, marker) => {
  // `marker` changes the init bytes, so the sink sees a different init (a quality switch).
  const bytes = marker ? Buffer.concat([Buffer.from(u8), Buffer.from(mp4Free(marker))]) : u8;
  p.onMessage({
    t: "segment",
    header: { sb: 1, seq: seq++, ms: 1, mime, isInit, kind: "mp4", tfdt: isInit ? null : tfdtOf(u8), timescale, tso: 0 },
    payload: dataUrl(bytes),
  });
};
const mp4Free = (text) => {
  const body = Buffer.from(text);
  const head = Buffer.alloc(8);
  head.writeUInt32BE(8 + body.length, 0);
  head.write("free", 4, "ascii");
  return Buffer.concat([head, body]);
};
const status = (tab) => vm.runInThisContext(`sinkLinkStatus(${tab})`);
const closeSockets = (tab) =>
  vm.runInThisContext(`for (const l of (sinkLinks.get(${tab}) || new Map()).values()) if (l.ws) l.ws.close();`);
const rows = (d) =>
  readFileSync(join(captures, d, "index.jsonl"), "utf8")
    .split("\n")
    .filter(Boolean)
    .map((l) => JSON.parse(l));
let seen = new Set(existsSync(captures) ? readdirSync(captures) : []);
const newDirs = (prefix) => {
  const all = readdirSync(captures);
  const fresh = all.filter((d) => !seen.has(d) && d.includes(prefix)).sort();
  seen = new Set(all);
  return fresh;
};
const half = Math.ceil(segments.length / 2);

// --- reconnect --------------------------------------------------------------
{
  const p = fakePort(1, "reconnect-a");
  seg(p, init, true);
  for (const s of segments.slice(0, half)) seg(p, s, false);
  await sleep(800);
  check(status(1).connected && status(1).dropped === 0, `reconnect: phase 1 sent, nothing dropped (${JSON.stringify(status(1))})`);
  closeSockets(1);
  await sleep(1300); // past the first retry delay
  for (const s of segments.slice(half)) seg(p, s, false);
  p.onMessage({ t: "event", header: { name: "endofstream", sb: null } });
  await sleep(1000);
  check(status(1).dropped === 0, `reconnect: phase 2 sent, nothing dropped (${JSON.stringify(status(1))})`);
  p.onDisconnect();
  await sleep(500);
  const dirs = newDirs("reconnec");
  check(dirs.length === 2, `reconnect: two capture directories (got ${dirs.length})`);
  if (dirs.length === 2) {
    const [a, b] = dirs.map(rows);
    check(a.length === 1 + half && a[0].is_init, `reconnect: first directory init + ${half} media (got ${a.length})`);
    const rest = segments.length - half;
    check(b.length === 1 + rest && b[0].is_init, `reconnect: second directory starts with the replayed init + ${rest} media (got ${b.length})`);
  }
}

// --- first-fail -------------------------------------------------------------
{
  portOverride = port + 1; // nothing listens there: the first socket fails
  const p = fakePort(2, "firstfail-a");
  seg(p, init, true);
  await sleep(600);
  check(!status(2).connected && status(2).dropped >= 1, `first-fail: first socket failed, init dropped (${JSON.stringify(status(2))})`);
  await sleep(1300);
  for (const s of segments.slice(0, 2)) seg(p, s, false);
  await sleep(800);
  p.onDisconnect();
  await sleep(500);
  const dirs = newDirs("firstfai");
  check(dirs.length === 1, `first-fail: one capture directory (got ${dirs.length})`);
  if (dirs.length === 1) {
    const r = rows(dirs[0]);
    check(r.length === 3 && r[0].is_init, `first-fail: the dropped init was replayed before the media (got ${r.length} rows)`);
  }
}

// --- switch -----------------------------------------------------------------
{
  const p = fakePort(3, "switch-a");
  seg(p, init, true, "A");
  seg(p, segments[0], false);
  await sleep(800);
  closeSockets(3);
  await sleep(1300);
  // Queued while the new socket connects: old media, then the new init and new media.
  seg(p, segments[1], false);
  seg(p, init, true, "B");
  seg(p, segments[2], false);
  await sleep(1000);
  p.onDisconnect();
  await sleep(500);
  const dirs = newDirs("switch-a");
  check(dirs.length === 2, `switch: two capture directories (got ${dirs.length})`);
  if (dirs.length === 2) {
    const r = rows(dirs[1]);
    const shape = r.map((x) => `${x.is_init ? "I" : "M"}${x.part}`).join(" ");
    check(shape === "I0 M0 I1 M1", `switch: old init, old media, new init, new media (got ${shape})`);
  }
}

// --- split-init replay -------------------------------------------------------
{
  const p = fakePort(4, "splitrep-a");
  const cut = 200;
  const header = (isInit, initCont, u8) => ({
    sb: 1, seq: seq++, ms: 1, mime, isInit, initCont, kind: isInit ? "mp4" : "unknown",
    tfdt: isInit || initCont ? null : tfdtOf(u8), timescale, tso: 0,
  });
  p.onMessage({ t: "segment", header: header(true, false, init), payload: dataUrl(init.subarray(0, cut)) });
  p.onMessage({ t: "segment", header: header(false, true, init), payload: dataUrl(init.subarray(cut)) });
  p.onMessage({ t: "segment", header: header(false, false, segments[0]), payload: dataUrl(segments[0]) });
  await sleep(800);
  closeSockets(4);
  await sleep(1300);
  p.onMessage({ t: "segment", header: header(false, false, segments[1]), payload: dataUrl(segments[1]) });
  await sleep(800);
  p.onDisconnect();
  await sleep(500);
  const dirs = newDirs("splitrep");
  check(dirs.length === 2, `split-init: two capture directories (got ${dirs.length})`);
  if (dirs.length === 2) {
    const r = rows(dirs[1]);
    const shape = r.map((x) => (x.is_init ? "I" : "M")).join("");
    check(shape === "IIM", `split-init: both init chunks replayed before the media (got ${shape})`);
  }
}

// --- evicted init: resume at the next init ------------------------------------
{
  const p = fakePort(5, "evicted-a");
  seg(p, init, true);
  seg(p, segments[0], false);
  await sleep(800);
  // What a full kept-init budget leaves behind: no kept copy, the link marked.
  vm.runInThisContext(`{ const l = [...sinkLinks.get(5).values()][0]; linkKeep(l, 1, null); l.evicted.add(1); }`);
  closeSockets(5);
  // onclose comes after the close handshake: wait until the retry is due.
  const due = () => vm.runInThisContext(`[...sinkLinks.get(5).values()].every((l) => !l.ws && Date.now() >= l.nextRetryAt)`);
  await sleep(300);
  for (let i = 0; i < 50 && !due(); i++) await sleep(100);
  // Queued while the new socket connects: media without its init, then a
  // seek (init + media); one more segment once the socket is open.
  seg(p, segments[1], false);
  seg(p, init, true);
  seg(p, segments[2], false);
  await sleep(1000);
  seg(p, segments[3], false);
  await sleep(800);
  p.onDisconnect();
  await sleep(500);
  const dirs = newDirs("evicted-");
  check(dirs.length === 2, `evicted: two capture directories (got ${dirs.length})`);
  if (dirs.length === 2) {
    const shape = rows(dirs[1]).map((x) => (x.is_init ? "I" : "M")).join("");
    check(shape === "IMM", `evicted: resumes at the queued init, nothing after it lost (got ${shape})`);
    const events = existsSync(join(captures, dirs[1], "events.jsonl"))
      ? readFileSync(join(captures, dirs[1], "events.jsonl"), "utf8")
      : "";
    check(events.includes('"initlost"'), "evicted: the sink is told (initlost)");
  }
}

// --- link limit ---------------------------------------------------------------
{
  const ports = [];
  for (let i = 0; i < 16; i++) ports.push(fakePort(100 + i, `limit-${i}`));
  const extra = fakePort(200, "limit-extra");
  await sleep(800);
  check(!status(200).connected, "limit: the 17th port is parked, no socket");
  ports[0].onDisconnect();
  await sleep(300);
  seg(extra, init, true);
  await sleep(800);
  check(status(200).connected, "limit: the parked port connects once a slot is free");
  for (const p of ports.slice(1)) p.onDisconnect();
  extra.onDisconnect();
  await sleep(500);
  newDirs("limit-");
}

// --- kept-init budget (no sockets) ---------------------------------------------
{
  const r = vm.runInThisContext(`(() => {
    const mk = (tabId, frameId) => {
      const l = { tabId, frameId, inits: new Map(), evicted: new Set(), gone: false };
      if (!sinkLinks.has(tabId)) sinkLinks.set(tabId, new Map());
      sinkLinks.get(tabId).set("kept-" + frameId, l);
      return l;
    };
    const bytes = (n) => [new Blob([new Uint8Array(n)])];
    const victim = mk(300, 0);
    linkKeep(victim, 1, bytes(778));
    // One page fills the budget and keeps going.
    const page = mk(301, 0);
    for (let i = 0; i < 80; i++) linkKeep(page, i, bytes(1024 * 1024));
    const crossTab = victim.inits.has(1) && victim.evicted.size === 0;
    // An ad frame in the player's tab does the same.
    const player = mk(302, 0);
    linkKeep(player, 1, bytes(1024 * 1024)); // as large as any forged one
    const ad = mk(302, 9);
    for (let i = 0; i < 80; i++) linkKeep(ad, i, bytes(1024 * 1024));
    const sameTab = player.inits.has(1) && player.evicted.size === 0;
    // An init too large to keep cannot be replayed either: marked.
    linkKeep(player, 2, bytes(1024 * 1024 + 1));
    const tooLarge = !player.inits.has(2) && player.evicted.has(2);
    const total = sinkKeptTotal;
    for (const l of [victim, page, player, ad]) for (const sb of [...l.inits.keys()]) linkKeep(l, sb, null);
    for (const t of [300, 301, 302]) sinkLinks.delete(t);
    return { crossTab, sameTab, tooLarge, total, after: sinkKeptTotal };
  })()`);
  check(r.tooLarge, "kept inits: an init too large to keep is marked for initlost");
  check(r.crossTab, "kept inits: a page filling the budget loses its own inits, not another tab's");
  check(r.sameTab, "kept inits: an ad frame filling the budget loses its own, not the player frame's");
  check(r.total <= 64 * 1024 * 1024 && r.after === 0, `kept inits: budget held and released (${r.total}, ${r.after})`);
}

console.log(failures.length ? `FAILED: ${failures.length} check(s)` : "ok: all checks passed");
process.exit(failures.length ? 1 : 0);
