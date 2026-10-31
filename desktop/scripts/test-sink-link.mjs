#!/usr/bin/env node
// Drives the extension's service-worker sink link (extension/src/sink-link.js)
// in Node against a running sink: mocks chrome.runtime/chrome.storage, opens a
// fake "vc-sink" port and posts hello + segments exactly as content.js does
// (payloads as data: URLs).
//
// Phase 1 posts the init segment and the first half of the media segments
// right after hello (they must be queued while the settings are read, not
// dropped). Then the socket is closed as if the sink had restarted; phase 2
// posts the rest. The sink starts a new capture directory for the second
// socket, which must begin with the replayed init segment. Usage:
//
//   node scripts/test-sink-link.mjs <fragmented.mp4> --token <token> --captures <sink --out dir> [--port 17650]
//
// Exit 0 when nothing was dropped and both capture directories hold what was sent.
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
if (!file || !token || !captures) {
  console.error("usage: test-sink-link.mjs <fragmented.mp4> --token <token> --captures <dir> [--port 17650]");
  process.exit(2);
}

const here = dirname(fileURLToPath(import.meta.url));
const linkSrc = readFileSync(join(here, "..", "..", "extension", "src", "sink-link.js"), "utf8");
const ORIGIN = "chrome-extension://oggpddakdbffbjhkjmcakidgmhpgakia";
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const failures = [];
const check = (ok, what) => {
  console.log(`${ok ? "ok  " : "FAIL"} ${what}`);
  if (!ok) failures.push(what);
};

// --- chrome mock ---------------------------------------------------------
let onConnect = null;
globalThis.chrome = {
  runtime: { onConnect: { addListener: (f) => (onConnect = f) } },
  storage: {
    local: {
      // async like the real one, so the hello/segment race is exercised
      get: (defaults, cb) => setTimeout(() => cb({ ...defaults, sinkPort: port, sinkToken: token }), 50),
    },
  },
};
// A browser sends the extension origin by itself; Node's WebSocket needs it spelled out.
const NodeWebSocket = globalThis.WebSocket;
globalThis.WebSocket = class extends NodeWebSocket {
  constructor(url) {
    super(url, { headers: { Origin: ORIGIN } });
  }
};

vm.runInThisContext(linkSrc, { filename: "sink-link.js" });

// --- fake port -------------------------------------------------------------
let onMessage = null;
let onDisconnect = null;
onConnect({
  name: "vc-sink",
  sender: { tab: { id: 7 }, frameId: 0, url: "https://example.test/watch" },
  onMessage: { addListener: (f) => (onMessage = f) },
  onDisconnect: { addListener: (f) => (onDisconnect = f) },
  disconnect: () => {},
});

const { init, segments, tfdtOf, timescale } = splitFmp4(file);
const dataUrl = (u8) => "data:application/octet-stream;base64," + Buffer.from(u8).toString("base64");
const mime = 'video/mp4; codecs="avc1.64001e, mp4a.40.2"';
let seq = 1;
const seg = (u8, isInit) =>
  onMessage({
    t: "segment",
    header: { sb: 1, seq: seq++, ms: 1, mime, isInit, kind: "mp4", tfdt: isInit ? null : tfdtOf(u8), timescale, tso: 0 },
    payload: dataUrl(u8),
  });
const status = () => vm.runInThisContext("sinkLinkStatus(7)");
const before = new Set(existsSync(captures) ? readdirSync(captures) : []);

const half = Math.ceil(segments.length / 2);
onMessage({ t: "hello", session: "sink-link-test", title: "test-sink-link.mjs" });
seg(init, true);
for (const s of segments.slice(0, half)) seg(s, false);
await sleep(800);
check(status().connected && status().dropped === 0, `phase 1 sent, nothing dropped (${JSON.stringify(status())})`);

// Simulate a sink restart: the socket goes away under the link.
vm.runInThisContext("for (const m of sinkLinks.values()) for (const l of m.values()) if (l.ws) l.ws.close();");
await sleep(1300); // past the first retry delay
for (const s of segments.slice(half)) seg(s, false);
onMessage({ t: "event", header: { name: "endofstream", sb: null } });
await sleep(1000);
const st = status();
check(st.connected && st.dropped === 0, `phase 2 sent after reconnect, nothing dropped (${JSON.stringify(st)})`);
onDisconnect();
await sleep(500);

// What the sink wrote: two new directories of this session.
const dirs = readdirSync(captures)
  .filter((d) => !before.has(d) && d.includes("sink-lin"))
  .sort();
check(dirs.length === 2, `two capture directories (got ${dirs.length}: ${dirs.join(", ")})`);
const rows = (d) =>
  readFileSync(join(captures, d, "index.jsonl"), "utf8")
    .split("\n")
    .filter(Boolean)
    .map((l) => JSON.parse(l));
if (dirs.length === 2) {
  const [a, b] = dirs.map(rows);
  check(a.length === 1 + half && a[0].is_init, `first directory: init + ${half} media (got ${a.length})`);
  const rest = segments.length - half;
  check(b.length === 1 + rest && b[0].is_init, `second directory starts with the replayed init + ${rest} media (got ${b.length})`);
}
console.log(failures.length ? `FAILED: ${failures.length} check(s)` : "ok: all checks passed");
process.exit(failures.length ? 1 : 0);
