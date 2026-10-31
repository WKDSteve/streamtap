#!/usr/bin/env node
// Drives extension/src/content.js in Node with a mocked chrome.runtime: hook
// records go in as window messages, port messages are recorded. Checks what
// a port after the first one (the service worker restarted) is sent before
// the media: the kept inits, and a "lost" note for an init that could not be
// kept (too large), so the worker says initlost instead of filing media under
// no init.
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
globalThis.chrome = {
  runtime: {
    id: "oggpddakdbffbjhkjmcakidgmhpgakia",
    lastError: null,
    sendMessage: (m, cb) => cb && cb(),
    connect: () => {
      const p = { sent: [], gone: [] };
      p.postMessage = (m) => p.sent.push(m);
      p.onDisconnect = { addListener: (f) => p.gone.push(f) };
      p.disconnect = () => {};
      ports.push(p);
      return p;
    },
  },
  storage: {
    local: { get: (d, cb) => setTimeout(() => cb({ ...d, sinkToken: "t" }), 5) },
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
const shape = (p) =>
  p.sent.map((m) => (m.t === "segment" ? `${m.header.isInit ? "I" : "M"}${m.header.sb}` : m.t === "lost" ? `L${m.header.sb}` : m.t[0].toUpperCase())).join(" ");

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

console.log(failures.length ? `FAILED: ${failures.length} check(s)` : "ok: all checks passed");
process.exit(failures.length ? 1 : 0);
