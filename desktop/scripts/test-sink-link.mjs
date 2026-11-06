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
//   unkept      a new port (restarted worker) whose content script could not
//               keep an init: initlost, media dropped up to the next init.
//   kept        (no sockets) a page or an ad frame filling the kept-init
//               budget loses its own inits, not another tab's or frame's.
// Without a sink (always run first, in a vm context of their own with a fake
// WebSocket that records what it is sent):
//   mock-open       init bytes dropped under backpressure on an open socket:
//                   initlost right away, that SourceBuffer's media dropped
//                   until its next init.
//   mock-salvage    the same for an init+media append with a small init
//                   part: that part still goes out, the next media flows.
//   mock-pending    while connecting with the queue full: a small init is
//                   still queued (twice the limits), and of an init+media
//                   append its small init part; the next media flows.
//   mock-sbcap      forged records with ever new sb ids on a stalled socket:
//                   initlost frames for at most 32 sb ids per link.
//   mock-backlog    init bytes dropped because too much waits for decoding:
//                   initlost past the limit on an open socket; while
//                   reconnecting, queued media still goes out under its init.
//   mock-gone       a socket that starts, or still connects, when the port's
//                   cleanup runs replays the kept init first; then nothing is
//                   kept.
//   mock-queue      init bytes dropped while connecting with older items of
//                   the same SourceBuffer queued: those still go out under
//                   their init, then initlost; not replayed in front of them.
//   mock-failed     the same when that socket never opens: the next socket
//                   says initlost and replays no stale init.
//   mock-lost       content.js dropped an init (`lost` with drop) while the
//                   worker keeps the previous one: initlost, media dropped.
//   mock-drain      the port closes while the socket connects and a payload
//                   is still decoding: it is still sent, then the socket
//                   closes.
// Usage:
//   node scripts/test-sink-link.mjs            (only the mock scenarios)
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
const live = !!(file || token || captures);
if (live && (!file || !token || !captures)) {
  console.error("usage: test-sink-link.mjs [<fragmented.mp4> --token <t> --captures <dir> [--port 17650] [--origin <o>]]");
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

// --- mock scenarios: fake WebSocket, no sink ------------------------------------
{
  class FakeSocket {
    constructor(url) {
      this.url = url;
      this.readyState = 0;
      this.bufferedAmount = 0;
      this.sent = [];
      sockets.push(this);
    }
    send(blob) {
      if (this.readyState !== 1) throw new Error("send on a socket that is not open");
      this.sent.push(blob);
    }
    open() {
      this.readyState = 1;
      this.onopen();
    }
    close() {
      if (this.readyState >= 2) return;
      this.readyState = 2;
      setTimeout(() => {
        this.readyState = 3;
        this.onclose();
      }, 0);
    }
  }
  Object.assign(FakeSocket, { CONNECTING: 0, OPEN: 1, CLOSING: 2, CLOSED: 3 });
  const sockets = [];
  const gates = new Map(); // data: URL -> promise its decode waits for
  let connect = null;
  const ctx = vm.createContext({
    console,
    Blob,
    TextEncoder,
    setTimeout,
    WebSocket: FakeSocket,
    fetch: async (u) => {
      if (gates.has(u)) await gates.get(u);
      return fetch(u);
    },
    chrome: {
      runtime: { onConnect: { addListener: (f) => (connect = f) } },
      storage: { local: { get: (d, cb) => setTimeout(() => cb({ ...d, sinkPort: 1, sinkToken: "t" }), 0) } },
    },
  });
  vm.runInContext(linkSrc, ctx, { filename: "sink-link.js" });
  const tick = () => sleep(30);
  const mkPort = (tabId) => {
    const p = {};
    connect({
      name: "vc-sink",
      sender: { tab: { id: tabId }, frameId: 0, url: "https://example.test/watch" },
      onMessage: { addListener: (f) => (p.post = f) },
      onDisconnect: { addListener: (f) => (p.gone = f) },
      disconnect: () => {},
    });
    p.post({ t: "hello", session: `mock-${tabId}`, title: "test-sink-link.mjs" });
    return p;
  };
  // A payload is its label, "|", then padding up to `size` bytes.
  const body = (label, size = 0) => {
    const b = Buffer.alloc(Math.max(size, label.length + 1), 0x20);
    b.write(`${label}|`, 0, "latin1");
    return "data:application/octet-stream;base64," + b.toString("base64");
  };
  let mseq = 1;
  const segMsg = (label, { init = false, initLen = null, size = 0, sb = 1 } = {}) => ({
    t: "segment",
    header: { sb, seq: mseq++, ms: 1, mime: "video/mp4", isInit: init, initLen, kind: "mp4", tso: 0 },
    payload: body(label, size),
  });
  // What a socket was sent: H(ello), event names, segment labels.
  const labels = async (ws) => {
    const out = [];
    for (const blob of ws.sent) {
      const b = Buffer.from(await blob.arrayBuffer());
      const n = b.readUInt32LE(0);
      const h = JSON.parse(b.subarray(4, 4 + n).toString("utf8"));
      if (h.type === "hello") out.push("H");
      else if (h.type === "event") out.push(h.name);
      else out.push(b.subarray(4 + n).toString("latin1").split("|")[0]);
    }
    return out.join(" ");
  };
  const linkOf = (tab) => vm.runInContext(`sinkLinks.has(${tab}) ? [...sinkLinks.get(${tab}).values()][0] : undefined`, ctx);
  const fillPendingTotal = (on) => vm.runInContext(`sinkPendingTotal ${on ? "+" : "-"}= 1024 * 1024 * 1024`, ctx);

  // mock-open: an init+media append over 1 MiB has the plain 64 MiB limit;
  // its init part is too large to go out on its own.
  {
    const p = mkPort(1);
    await tick();
    const ws = sockets.at(-1);
    ws.open();
    p.post(segMsg("A", { init: true }));
    p.post(segMsg("a1"));
    await tick();
    ws.bufferedAmount = 64 * 1024 * 1024 - 1000;
    p.post(segMsg("B", { init: true, initLen: 1536 * 1024, size: 2 * 1024 * 1024 }));
    await tick();
    check(!linkOf(1).inits.has(1), "mock-open: the init before the dropped one is no longer kept");
    ws.bufferedAmount = 0;
    p.post(segMsg("b1"));
    p.post(segMsg("C", { init: true }));
    p.post(segMsg("c1"));
    await tick();
    const got = await labels(ws);
    check(got === "H A a1 initlost C c1", `mock-open: initlost at once, media dropped until the next init (got ${got})`);
    p.gone();
    await tick();
  }

  // mock-queue: the init and media of A are queued, B does not fit.
  {
    const p = mkPort(2);
    await tick();
    const ws = sockets.at(-1);
    p.post(segMsg("A", { init: true }));
    p.post(segMsg("a1"));
    await tick();
    fillPendingTotal(true);
    p.post(segMsg("B", { init: true }));
    p.post(segMsg("b1"));
    await tick();
    fillPendingTotal(false);
    ws.open();
    // Media of the lost init arriving once the socket is open.
    p.post(segMsg("b2"));
    p.post(segMsg("C", { init: true }));
    p.post(segMsg("c1"));
    await tick();
    const got = await labels(ws);
    check(got === "H A a1 initlost C c1", `mock-queue: queued items under their init, then initlost (got ${got})`);
    p.gone();
    await tick();
  }

  // mock-failed: as mock-queue, but that socket never opens.
  {
    const p = mkPort(3);
    await tick();
    const first = sockets.at(-1);
    p.post(segMsg("A", { init: true }));
    p.post(segMsg("a1"));
    await tick();
    fillPendingTotal(true);
    p.post(segMsg("B", { init: true }));
    await tick();
    fillPendingTotal(false);
    first.close();
    await tick();
    linkOf(3).nextRetryAt = 0;
    p.post(segMsg("b1"));
    p.post(segMsg("C", { init: true }));
    p.post(segMsg("c1"));
    await tick();
    const ws = sockets.at(-1);
    ws.open();
    await tick();
    const got = ws === first ? "(no second socket)" : await labels(ws);
    check(got === "H initlost C c1", `mock-failed: the next socket says initlost, no stale init replayed (got ${got})`);
    p.gone();
    await tick();
  }

  // mock-lost: content.js dropped init bytes of a running stream.
  {
    const p = mkPort(4);
    await tick();
    const ws = sockets.at(-1);
    ws.open();
    p.post(segMsg("A", { init: true }));
    p.post(segMsg("a1"));
    p.post({ t: "lost", header: { sb: 1, drop: true } });
    p.post(segMsg("a2"));
    p.post(segMsg("C", { init: true }));
    p.post(segMsg("c1"));
    await tick();
    const got = await labels(ws);
    check(got === "H A a1 initlost C c1", `mock-lost: initlost, media dropped until the next init (got ${got})`);
    p.gone();
    await tick();
  }

  // mock-salvage: the same append with a small init part: that part still
  // goes out, so the rendition's next media flows.
  {
    const p = mkPort(7);
    await tick();
    const ws = sockets.at(-1);
    ws.open();
    p.post(segMsg("A", { init: true }));
    p.post(segMsg("a1"));
    await tick();
    ws.bufferedAmount = 63 * 1024 * 1024;
    p.post(segMsg("B", { init: true, initLen: 100, size: 2 * 1024 * 1024 }));
    await tick();
    const kept = linkOf(7).inits.get(1);
    check(kept && kept.length === 1 && kept[0].size < 1024, "mock-salvage: the init part is kept for a new socket");
    ws.bufferedAmount = 0;
    p.post(segMsg("b2"));
    p.post(segMsg("b3"));
    await tick();
    const got = await labels(ws);
    check(got === "H A a1 B b2 b3", `mock-salvage: init part sent, the next media flows (got ${got})`);
    p.gone();
    await tick();
  }

  // mock-pending: A and a1 are queued on a connecting socket, then the queue
  // of the worker is exactly full when B arrives.
  const fillPendingFull = () => {
    const d = vm.runInContext("SINK_MAX_PENDING_TOTAL - sinkPendingTotal", ctx);
    vm.runInContext(`sinkPendingTotal += ${d}`, ctx);
    return () => vm.runInContext(`sinkPendingTotal -= ${d}`, ctx);
  };
  {
    // A small init goes up to twice the limit.
    const p = mkPort(12);
    await tick();
    const ws = sockets.at(-1);
    p.post(segMsg("A", { init: true }));
    p.post(segMsg("a1"));
    await tick();
    const undo = fillPendingFull();
    p.post(segMsg("B", { init: true }));
    await tick();
    undo();
    ws.open();
    p.post(segMsg("b1"));
    await tick();
    const got = await labels(ws);
    check(got === "H A a1 B b1", `mock-pending: a small init is queued past the limit (got ${got})`);
    p.gone();
    await tick();
  }
  {
    // Of an init+media append over 1 MiB the small init part is queued.
    const p = mkPort(13);
    await tick();
    const ws = sockets.at(-1);
    p.post(segMsg("A", { init: true }));
    p.post(segMsg("a1"));
    await tick();
    const undo = fillPendingFull();
    p.post(segMsg("B", { init: true, initLen: 100, size: 2 * 1024 * 1024 }));
    await tick();
    undo();
    ws.open();
    p.post(segMsg("b2"));
    p.post(segMsg("b3"));
    await tick();
    const got = await labels(ws);
    check(got === "H A a1 B b2 b3", `mock-pending: the init part is queued, the next media flows (got ${got})`);
    const kept = linkOf(13).inits.get(1);
    check(kept && kept.length === 1 && kept[0].size < 1024, "mock-pending: the init part is kept for a new socket");
    p.gone();
    await tick();
  }

  // mock-sbcap: a stalled socket, then inits and `lost` drops with new sb ids.
  {
    const p = mkPort(14);
    await tick();
    const ws = sockets.at(-1);
    ws.open();
    ws.bufferedAmount = 128 * 1024 * 1024;
    for (let i = 0; i < 40; i++) p.post(segMsg("F", { init: true, sb: 100 + i }));
    for (let i = 0; i < 40; i++) p.post({ t: "lost", header: { sb: 200 + i, drop: true } });
    await tick();
    const lost = (await labels(ws)).split(" ").filter((l) => l === "initlost").length;
    const link = linkOf(14);
    check(lost === 32 && link.lostInit.size === 32, `mock-sbcap: initlost for at most 32 sb ids (got ${lost}, ${link.lostInit.size} tracked)`);
    ws.bufferedAmount = 0;
    p.gone();
    await tick();
  }

  // mock-backlog: init bytes dropped because too much waits for decoding.
  const fillBacklog = (on) => vm.runInContext(`sinkBacklog ${on ? "+" : "-"}= 1024 * 1024 * 1024`, ctx);
  {
    // On an open socket at the backpressure limit: initlost still goes out.
    const p = mkPort(8);
    await tick();
    const ws = sockets.at(-1);
    ws.open();
    p.post(segMsg("A", { init: true }));
    p.post(segMsg("a1"));
    await tick();
    ws.bufferedAmount = 64 * 1024 * 1024;
    fillBacklog(true);
    p.post(segMsg("B", { init: true }));
    fillBacklog(false);
    await tick();
    ws.bufferedAmount = 0;
    p.post(segMsg("b1"));
    p.post(segMsg("C", { init: true }));
    p.post(segMsg("c1"));
    await tick();
    const got = await labels(ws);
    check(got === "H A a1 initlost C c1", `mock-backlog: initlost past the limit on an open socket (got ${got})`);
    p.gone();
    await tick();
  }
  {
    // While reconnecting with a2 queued: a2 still goes out under the kept A.
    const p = mkPort(9);
    await tick();
    const first = sockets.at(-1);
    first.open();
    p.post(segMsg("A", { init: true }));
    p.post(segMsg("a1"));
    await tick();
    first.close();
    await tick();
    linkOf(9).nextRetryAt = 0;
    p.post(segMsg("a2"));
    await tick();
    fillBacklog(true);
    p.post(segMsg("B", { init: true }));
    fillBacklog(false);
    p.post(segMsg("b1"));
    p.post(segMsg("C", { init: true }));
    p.post(segMsg("c1"));
    await tick();
    const ws = sockets.at(-1);
    if (ws !== first) ws.open();
    await tick();
    const got = ws === first ? "(no second socket)" : await labels(ws);
    check(got === "H A a2 initlost C c1", `mock-backlog: queued media under its kept init, then initlost (got ${got})`);
    p.gone();
    await tick();
  }

  // mock-gone: a socket that starts or still connects when the port's cleanup
  // runs replays the kept init in front of what it sends.
  {
    // The sink closes the open socket while a2 still decodes after the port
    // closed; a2 then starts a new socket.
    const p = mkPort(10);
    await tick();
    const first = sockets.at(-1);
    first.open();
    p.post(segMsg("A", { init: true }));
    p.post(segMsg("a1"));
    await tick();
    let release;
    const a2 = segMsg("a2");
    gates.set(a2.payload, new Promise((r) => (release = r)));
    p.post(a2);
    p.gone();
    first.close();
    await tick();
    linkOf(10).nextRetryAt = 0;
    release();
    await tick();
    const ws = sockets.at(-1);
    if (ws !== first) ws.open();
    await tick();
    const got = ws === first ? "(no second socket)" : await labels(ws);
    check(got === "H A a2" && ws.readyState === 3, `mock-gone: a socket started after the port closed replays the init (got ${got}, state ${ws.readyState})`);
    check(linkOf(10) === undefined && vm.runInContext("sinkKeptTotal", ctx) === 0, "mock-gone: then nothing is kept for that link");
  }
  {
    // A reconnect still connecting when the port goes.
    const p = mkPort(11);
    await tick();
    const first = sockets.at(-1);
    first.open();
    p.post(segMsg("A", { init: true }));
    p.post(segMsg("a1"));
    await tick();
    first.close();
    await tick();
    linkOf(11).nextRetryAt = 0;
    p.post(segMsg("a2"));
    await tick();
    p.gone();
    await tick();
    const ws = sockets.at(-1);
    if (ws !== first) ws.open();
    await tick();
    const got = ws === first ? "(no second socket)" : await labels(ws);
    check(got === "H A a2" && ws.readyState === 3, `mock-gone: a reconnect still connecting at the cleanup replays the init (got ${got}, state ${ws.readyState})`);
    check(linkOf(11) === undefined && vm.runInContext("sinkKeptTotal", ctx) === 0, "mock-gone: then nothing is kept for that link");
  }

  // mock-drain: the port closes while the socket connects and a1 still decodes.
  {
    const p = mkPort(5);
    await tick();
    const ws = sockets.at(-1);
    p.post(segMsg("A", { init: true }));
    await tick();
    let release;
    const a1 = segMsg("a1");
    gates.set(a1.payload, new Promise((r) => (release = r)));
    p.post(a1);
    p.gone();
    await tick();
    ws.open();
    release();
    await tick();
    const got = await labels(ws);
    check(got === "H A a1", `mock-drain: the decoding payload is still sent (got ${got})`);
    check(linkOf(5) === undefined && ws.readyState >= 2, `mock-drain: then the cleanup closes the socket (state ${ws.readyState})`);
  }
  {
    // A socket still connecting when the port's cleanup ran closes after its queue.
    const p = mkPort(6);
    await tick();
    const ws = sockets.at(-1);
    p.post(segMsg("A", { init: true }));
    p.gone();
    await tick();
    ws.open();
    await tick();
    const got = await labels(ws);
    check(got === "H A" && ws.readyState === 3, `mock-drain: a socket opened after the cleanup sends its queue and closes (got ${got}, state ${ws.readyState})`);
  }

  // mock-backoff: the retry policy on a link's own state (no socket needed). The retry is due after the
  // current delay; the delay then doubles from 1 s up to the 10 s cap; a socket that stayed open (stable)
  // starts over at 1 s.
  {
    const policy = JSON.parse(
      vm.runInContext(
        `(() => {
          const link = { nextRetryAt: 0, retryDelay: 1000 };
          const rows = [];
          for (let i = 0; i < 6; i++) {
            const used = link.retryDelay;
            const before = Date.now();
            scheduleRetry(link, false);
            rows.push({ used, waited: link.nextRetryAt - before, next: link.retryDelay });
          }
          link.retryDelay = 8000;
          scheduleRetry(link, true);
          return JSON.stringify({ rows, stable: { waited: link.nextRetryAt - Date.now(), next: link.retryDelay } });
        })()`,
        ctx,
      ),
    );
    check(
      policy.rows.map((r) => r.next).join(",") === "2000,4000,8000,10000,10000,10000",
      `mock-backoff: the delay doubles up to the 10 s cap (got ${policy.rows.map((r) => r.next).join(",")})`,
    );
    check(
      policy.rows.every((r) => r.waited >= r.used - 5 && r.waited <= r.used + 500),
      "mock-backoff: the retry is due after the delay that was current",
    );
    check(
      policy.stable.next === 2000 && policy.stable.waited <= 1500,
      `mock-backoff: a stable socket restarts at 1 s (waits ${policy.stable.waited} ms, next ${policy.stable.next})`,
    );
  }
}
if (!live) {
  console.log("live scenarios skipped (no sink given)");
  console.log(failures.length ? `FAILED: ${failures.length} check(s)` : "ok: all checks passed");
  process.exit(failures.length ? 1 : 0);
}

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
// How long a reconnect takes is the machine's business: wait for the directory, then for what follows it to land.
const waitForDirs = async (prefix, n) => {
  const deadline = Date.now() + 30_000;
  while (Date.now() < deadline) {
    if (readdirSync(captures).filter((d) => !seen.has(d) && d.includes(prefix)).length >= n) break;
    await sleep(100);
  }
  await sleep(1000);
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
  await waitForDirs("reconnec", 2);
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
  await waitForDirs("switch-a", 2);
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
  await waitForDirs("splitrep", 2);
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

// --- unkept init after a worker restart ---------------------------------------
{
  // content.js could not keep sb1's init (too large) for this new port.
  const p = fakePort(6, "unkept-a");
  p.onMessage({ t: "lost", header: { sb: 1 } });
  seg(p, segments[0], false);
  seg(p, init, true);
  seg(p, segments[1], false);
  await sleep(1000);
  p.onDisconnect();
  await sleep(500);
  const dirs = newDirs("unkept-a");
  check(dirs.length === 1, `unkept: one capture directory (got ${dirs.length})`);
  if (dirs.length === 1) {
    const shape = rows(dirs[0]).map((x) => (x.is_init ? "I" : "M")).join("");
    check(shape === "IM", `unkept: media before the next init dropped (got ${shape})`);
    const events = existsSync(join(captures, dirs[0], "events.jsonl"))
      ? readFileSync(join(captures, dirs[0], "events.jsonl"), "utf8")
      : "";
    check(events.includes('"initlost"'), "unkept: the sink is told (initlost)");
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
