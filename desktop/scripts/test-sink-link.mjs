#!/usr/bin/env node
// Drives the extension's service-worker sink link (extension/src/sink-link.js)
// in Node against a running sink: mocks chrome.runtime/chrome.storage, opens a
// fake "vc-sink" port and posts hello + segments exactly as content.js does
// (payloads as data: URLs), including segments posted right after hello
// (they must be queued while the settings are read, not dropped). Usage:
//
//   node scripts/test-sink-link.mjs <fragmented.mp4> --token <token> [--port 17650]
//
// Exit 0 when every segment was sent and none was dropped.
import { readFileSync } from "node:fs";
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
if (!file || !token) {
  console.error("usage: test-sink-link.mjs <fragmented.mp4> --token <token> [--port 17650]");
  process.exit(2);
}

const here = dirname(fileURLToPath(import.meta.url));
const linkSrc = readFileSync(join(here, "..", "..", "extension", "src", "sink-link.js"), "utf8");
const ORIGIN = "chrome-extension://oggpddakdbffbjhkjmcakidgmhpgakia";

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
const port_ = {
  name: "vc-sink",
  sender: { tab: { id: 7 }, frameId: 0, url: "https://example.test/watch" },
  onMessage: { addListener: (f) => (onMessage = f) },
  onDisconnect: { addListener: (f) => (onDisconnect = f) },
  disconnect: () => {},
};
onConnect(port_);

const { init, segments, tfdtOf, timescale } = splitFmp4(file);
const dataUrl = (u8) => "data:application/octet-stream;base64," + Buffer.from(u8).toString("base64");
const mime = 'video/mp4; codecs="avc1.64001e, mp4a.40.2"';
let seq = 1;
const seg = (u8, isInit) =>
  onMessage({
    t: "segment",
    header: { sb: 1, seq: seq++, mime, isInit, kind: "mp4", tfdt: isInit ? null : tfdtOf(u8), timescale },
    payload: dataUrl(u8),
  });

onMessage({ t: "hello", session: "sink-link-test", title: "test-sink-link.mjs" });
seg(init, true);
for (const s of segments) seg(s, false);
onMessage({ t: "event", header: { name: "endofstream", sb: null } });

setTimeout(() => {
  const status = vm.runInThisContext("sinkLinkStatus(7)");
  console.log("status", JSON.stringify(status));
  onDisconnect();
  const want = segments.length + 2; // init + media + event
  const ok = status && status.connected && status.sent === want && status.dropped === 0;
  console.log(ok ? `ok: ${want} messages sent, 0 dropped` : `FAILED: expected ${want} sent and 0 dropped`);
  setTimeout(() => process.exit(ok ? 0 : 1), 300);
}, 1500);
