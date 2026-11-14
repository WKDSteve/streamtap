#!/usr/bin/env node
// The extension's tailnet target (extension/src/tailnet-target.js, the file a phone runs) against the real sink.
//
//   node desktop/scripts/test-tailnet-live.mjs [path to streamtap-sink]    (after cargo build; default target/debug)
//
// Starts the built streamtap-sink with `--tailnet`, asks it for a pairing code with `streamtap-sink devices pair`,
// and lets the module pair, log in and send a capture, with Node's own WebSocket and WebCrypto. Between the two a
// small proxy here plays Tailscale Serve: it forwards to the sink's loopback port with the tailnet name as `Host`,
// the add-on's origin and an `X-Forwarded-For` header (TLS is Tailscale's part and is not played). What this shows
// that the two unit suites cannot: the key and the signature WebCrypto makes are the ones the sink (appkit) takes,
// and the frames of the module are the frames the sink reads. Then `devices revoke` ends the open connection and
// the next login says "revoked".
import { spawn, spawnSync } from "node:child_process";
import { randomBytes, webcrypto } from "node:crypto";
import { existsSync, mkdtempSync, readdirSync, readFileSync, rmSync, statSync } from "node:fs";
import net from "node:net";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import vm from "node:vm";

const here = dirname(fileURLToPath(import.meta.url));
const exeName = process.platform === "win32" ? "streamtap-sink.exe" : "streamtap-sink";
const exe = process.argv[2] ?? join(here, "..", "target", "debug", exeName);
if (!existsSync(exe)) {
  console.error(`test-tailnet-live: ${exe} is missing (build first: cargo build -p streamtap-sink)`);
  process.exit(2);
}

const NAME = "pc.example.ts.net";
const ORIGIN = "moz-extension://0f3c1a52-7b1e-4d0c-9a3e-2f6f1f0e9c11";
const failures = [];
const check = (ok, what) => {
  console.log(`${ok ? "ok  " : "FAIL"} ${what}`);
  if (!ok) failures.push(what);
};
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const until = async (cond, ms = 10_000) => {
  for (const end = Date.now() + ms; Date.now() < end && !(await cond()); ) await sleep(50);
  return cond();
};

const out = mkdtempSync(join(tmpdir(), "streamtap-tailnet-live-"));
const token = randomBytes(16).toString("hex");
const sink = spawn(exe, ["serve", "--token-stdin", "--port", "0", "--out", out, "--tailnet", NAME], {
  stdio: ["pipe", "pipe", "inherit"],
});
sink.stdin.end(`${token}\n`);
let proxy = null;
const stop = (code) => {
  sink.kill();
  if (proxy) proxy.close();
  // Windows may still hold the files of the sink that was just ended: tried a few times, and a folder that
  // stays (in the system's temp folder) never makes a run red that passed its checks.
  try {
    rmSync(out, { recursive: true, force: true, maxRetries: 5, retryDelay: 200 });
  } catch (e) {
    console.error(`note: could not remove ${out}: ${e.code || e}`);
  }
  // Not at once: on Windows Node aborts when it exits while a WebSocket of its own is still closing.
  process.exitCode = code;
  setTimeout(() => process.exit(code), 500);
};

const port = await new Promise((resolve, reject) => {
  let seen = "";
  const limit = setTimeout(() => reject(new Error("the sink printed no port within 30 s")), 30_000);
  sink.on("exit", (code) => reject(new Error(`the sink ended early (exit ${code})`)));
  sink.stdout.on("data", (chunk) => {
    seen += chunk;
    const m = seen.match(/ws:\/\/127\.0\.0\.1:(\d+)/);
    if (m) {
      clearTimeout(limit);
      sink.stdout.resume();
      resolve(Number(m[1]));
    }
  });
}).catch((e) => {
  console.error(`test-tailnet-live: ${e.message}`);
  stop(1);
});
sink.removeAllListeners("exit");

// Tailscale Serve, as far as the sink can tell: the request head rewritten, then bytes both ways.
proxy = net.createServer((client) => {
  const upstream = net.connect(port, "127.0.0.1");
  let head = Buffer.alloc(0);
  const onData = (chunk) => {
    head = Buffer.concat([head, chunk]);
    const end = head.indexOf("\r\n\r\n");
    if (end < 0) return;
    client.off("data", onData);
    const lines = head
      .subarray(0, end)
      .toString("latin1")
      .split("\r\n")
      .filter((l) => !/^(host|origin):/i.test(l));
    lines.push(`Host: ${NAME}`, `Origin: ${ORIGIN}`, "X-Forwarded-For: 100.64.0.7", "X-Forwarded-Proto: https");
    upstream.write(lines.join("\r\n") + "\r\n\r\n");
    upstream.write(head.subarray(end + 4));
    client.pipe(upstream);
    upstream.pipe(client);
  };
  client.on("data", onData);
  for (const s of [client, upstream]) s.on("error", () => (client.destroy(), upstream.destroy()));
});
await new Promise((r) => proxy.listen(0, "127.0.0.1", r));
const proxyPort = proxy.address().port;

const devices = (...args) => {
  const r = spawnSync(exe, ["devices", ...args, "--token", token, "--port", String(port)], { encoding: "utf8" });
  return { status: r.status, text: r.stdout };
};

// The module as the phone runs it; only the address of its sockets is bent to the proxy.
const idb = new Map();
const request = (run) => ({ result: run() });
const store = { get: (k) => request(() => idb.get(k)), put: (v, k) => request(() => (idb.set(k, v), k)), delete: (k) => request(() => void idb.delete(k)) };
const indexedDB = {
  open: () => {
    const open = {};
    setTimeout(() => {
      open.result = {
        close: () => {},
        transaction: () => {
          const tx = { objectStore: () => store };
          setTimeout(() => tx.oncomplete && tx.oncomplete(), 0);
          return tx;
        },
      };
      open.onsuccess();
    }, 0);
    return open;
  },
};
const urls = [];
class ProxiedWebSocket extends WebSocket {
  constructor(url) {
    urls.push(url);
    super(`ws://127.0.0.1:${proxyPort}/`);
  }
}
const ctx = vm.createContext({ console, TextEncoder, DataView, Uint8Array, Array, Promise, JSON, Math, Date, URL, setTimeout, clearTimeout, crypto: webcrypto, indexedDB, WebSocket: ProxiedWebSocket });
vm.runInContext(readFileSync(join(here, "..", "..", "extension", "src", "tailnet-target.js"), "utf8"), ctx, { filename: "tailnet-target.js" });
const run = (code) => vm.runInContext(code, ctx);
const frame = (header, payload = new Uint8Array(0)) => {
  const h = Buffer.from(JSON.stringify(header));
  const len = Buffer.alloc(4);
  len.writeUInt32LE(h.length);
  return Buffer.concat([len, h, payload]);
};
// A logged-in socket of the module, or the reason it closed.
const login = () =>
  new Promise((resolve) => {
    const ws = run("new TailnetSocket(tn.pairing)");
    ws.onopen = () => resolve(ws);
    ws.onclose = () => resolve(null);
  });

try {
  const shown = devices("pair");
  const code = /code: ([0-9a-f]{32})/.exec(shown.text)?.[1];
  check(shown.status === 0 && Boolean(code) && shown.text.includes(`https://${NAME}/#streamtap-pair=1&code=${code}`), "devices pair prints the link and the code");

  const paired = await run(`tnPair("${NAME}", "${code}", "live test phone")`);
  check(paired.ok === true, `the module pairs with the real sink (${JSON.stringify(paired)})`);
  check(urls.every((u) => u === `wss://${NAME}/`), "the module speaks wss to the PC's name only");
  const again = await run(`tnPair("${NAME}", "${code}", "second phone")`);
  check(again.ok === false && again.reason === "code", "the code is spent: a second phone is refused");

  const ws = await login();
  check(Boolean(ws), "the sink takes the signature WebCrypto made (login ok)");
  if (ws) {
    const init = readFileSync(join(here, "fixtures", "frag.mp4")).subarray(0, 2048);
    ws.send(frame({ type: "hello", token: "", session: "live", url: "https://example.org/v", title: "tailnet live" }));
    ws.send(frame({ type: "segment", sb: 1, seq: 1, ms: 1, mime: "video/mp4", isInit: true, kind: "mp4" }, init));
    const captured = () => readdirSync(out).some((d) => statSync(join(out, d)).isDirectory() && readdirSync(join(out, d)).length > 0);
    check(await until(captured), "a capture of the device is written without the shared token");

    const list = devices();
    const id = /^\s+([0-9a-f]{32})\s+live test phone/m.exec(list.text)?.[1];
    check(list.status === 0 && Boolean(id) && list.text.includes("open connections 1"), "devices lists the phone with its open connection");
    let closed = false;
    ws.onclose = () => (closed = true);
    check(devices("revoke", id).status === 0, "devices revoke succeeds");
    check(await until(() => closed), "the revoked device's open connection ends");
    check((await login()) === null && (await run("tnStatus()")).state === "revoked", "the next login is refused and the module says revoked");
    const before = urls.length;
    check((await login()) === null && urls.length === before, "a revoked module opens no further socket");
  }
} catch (e) {
  console.error(e);
  failures.push(String(e));
}

if (failures.length) {
  console.error(`FAIL: ${failures.length} check(s) failed`);
  stop(1);
}
console.log("ok: all checks passed");
stop(0);
