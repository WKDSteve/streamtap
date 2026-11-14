#!/usr/bin/env node
// The target "the sink on the PC over the tailnet" (extension/src/tailnet-target.js) with the real sink link
// (extension/src/sink-link.js) in a vm context. The WebSocket is a fake whose other end plays the sink's door
// (pair, login, challenge, ok or a refusal); WebCrypto is Node's real one, so the signature the module makes is
// checked against the public key it sent at pairing, over exactly the message the sink checks. IndexedDB is a small
// in-memory stand-in that keeps the key object as it is. Checks:
//   pair        a typed address is cut to the host, the public key goes out, the private key is kept non-extractable
//   pair-errors a bad host or code opens no socket; a refusal and a silent PC come back as their reason
//   login       nothing but `login` leaves before the sink's ok; the signature verifies; then hello (no token), media
//   hub         a sink with another id gets no signature, and no second attempt
//   revoked     a refused device stops trying until it is paired again; then its waiting link tries at once
//   unreachable a PC that does not answer drops the segments, counted, and says so
//   not-paired  the target without a pairing opens no socket at all, never the local sink in its place
//   offer       a pairing link counts only from a top-level https page of a plain host; the status never holds the code
//   forget      the record is gone
//   stored      a record of another form reads as "not paired"
// Exit 0 when every check passed.
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";
import { webcrypto } from "node:crypto";
import vm from "node:vm";

const here = dirname(fileURLToPath(import.meta.url));
const src = (n) => readFileSync(join(here, "..", "src", n), "utf8");
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const failures = [];
const check = (ok, what) => {
  console.log(`${ok ? "ok  " : "FAIL"} ${what}`);
  if (!ok) failures.push(what);
};
const until = async (cond) => {
  for (let i = 0; i < 150 && !cond(); i++) await sleep(20);
  return cond();
};

const HOST = "pc.example.ts.net";
const CODE = "0123456789abcdef0123456789abcdef";
const HUB = "aa".repeat(16);
const DEVICE = "bb".repeat(16);
const hex = (buf) => Buffer.from(buf).toString("hex");

// The sink's door behind the fake WebSocket. `mode` says how it behaves.
const makeSink = () => {
  const sink = {
    mode: "ok", // ok | silent | close | junk | refuse:<reason> | hub
    hub: HUB,
    keys: new Map(), // device -> public key (hex)
    sockets: [],
    heard: [], // every header that arrived, in order, with the socket's number
    signatureOk: null,
    async receive(ws, header, payload) {
      sink.heard.push({ n: ws.n, ...header, payload: payload.toString("latin1").split("|")[0] });
      if (sink.mode === "silent") return;
      if (sink.mode === "junk") return ws.say("x".repeat(5000));
      if (header.type === "pair") {
        if (sink.mode.startsWith("refuse:")) return ws.say(JSON.stringify({ type: "refused", v: 1, reason: sink.mode.slice(7) }));
        sink.keys.set(DEVICE, header.key);
        return ws.say(JSON.stringify({ type: "paired", v: 1, device: DEVICE, hub: sink.hub }));
      }
      if (header.type === "login") {
        if (sink.mode.startsWith("refuse:")) return ws.say(JSON.stringify({ type: "refused", v: 1, reason: sink.mode.slice(7) }));
        ws.challenge = hex(webcrypto.getRandomValues(new Uint8Array(32)));
        ws.device = header.device;
        return ws.say(JSON.stringify({ type: "challenge", v: 1, hub: sink.mode === "hub" ? "cc".repeat(16) : sink.hub, challenge: ws.challenge }));
      }
      if (header.type === "auth") {
        const raw = Buffer.from(sink.keys.get(ws.device) || "", "hex");
        const key = await webcrypto.subtle.importKey("raw", raw, { name: "ECDSA", namedCurve: "P-256" }, false, ["verify"]);
        const message = `appkit-device-login/1\nhttps://${HOST}\n${ws.device}\n${ws.challenge}`;
        sink.signatureOk = await webcrypto.subtle.verify({ name: "ECDSA", hash: "SHA-256" }, key, Buffer.from(header.signature, "hex"), Buffer.from(message));
        return ws.say(JSON.stringify(sink.signatureOk ? { type: "ok", v: 1 } : { type: "refused", v: 1, reason: "device" }));
      }
    },
  };
  return sink;
};

const world = ({ target = "tailnet", stored = undefined } = {}) => {
  const sink = makeSink();
  const idb = new Map();
  if (stored !== undefined) idb.set("pc", stored);
  const request = (run) => {
    const r = {};
    r.result = run();
    return r;
  };
  const indexedDB = {
    open: () => {
      const open = {};
      setTimeout(() => {
        const store = {
          get: (k) => request(() => idb.get(k)),
          put: (v, k) => request(() => (idb.set(k, v), k)),
          delete: (k) => request(() => void idb.delete(k)),
        };
        open.result = {
          createObjectStore: () => store,
          close: () => {},
          transaction: () => {
            const tx = { objectStore: () => store };
            setTimeout(() => tx.oncomplete && tx.oncomplete(), 0);
            return tx;
          },
        };
        if (open.onsuccess) open.onsuccess();
      }, 0);
      return open;
    },
  };
  class FakeWebSocket {
    static CONNECTING = 0;
    static OPEN = 1;
    static CLOSING = 2;
    static CLOSED = 3;
    constructor(url) {
      this.url = url;
      this.n = sink.sockets.length;
      this.readyState = 0;
      this.bufferedAmount = 0;
      this.chain = Promise.resolve();
      sink.sockets.push(this);
      setTimeout(() => {
        if (this.readyState !== 0) return;
        if (sink.mode === "close") return this.close();
        this.readyState = 1;
        if (this.onopen) this.onopen();
      }, 0);
    }
    send(data) {
      // A frame is bytes (tailnet-target.js) or a Blob (sink-link.js); the order is kept.
      this.chain = this.chain.then(async () => {
        const b = Buffer.from(data instanceof Uint8Array ? data : await data.arrayBuffer());
        const hl = b.readUInt32LE(0);
        await sink.receive(this, JSON.parse(b.subarray(4, 4 + hl).toString("utf8")), b.subarray(4 + hl));
      });
    }
    say(text) {
      if (this.readyState === 1 && this.onmessage) this.onmessage({ data: text });
    }
    close() {
      if (this.readyState === 3) return;
      this.readyState = 3;
      setTimeout(() => this.onclose && this.onclose(), 0);
    }
  }
  let connect = null;
  const chrome = {
    runtime: { id: "addon-id", getURL: (p) => `moz-extension://0f3c1a52/${p}`, onConnect: { addListener: (f) => (connect = f) }, lastError: undefined },
    // A token of the local sink is set too: the tailnet target must never fall back to it.
    storage: { local: { get: (d, cb) => setTimeout(() => cb({ ...d, sinkTarget: target, sinkToken: "local-token" }), 0) } },
  };
  // The module's waits (15 s) run in 60 ms here.
  const quick = (f, ms) => setTimeout(f, ms > 1000 ? 60 : ms);
  const ctx = vm.createContext({ console, Blob, TextEncoder, DataView, Uint8Array, Array, Promise, JSON, Math, Date, URL, URLSearchParams, setTimeout: quick, clearTimeout, fetch, crypto: webcrypto, indexedDB, chrome, WebSocket: FakeWebSocket });
  vm.runInContext(src("tailnet-target.js"), ctx, { filename: "tailnet-target.js" });
  vm.runInContext(src("sink-link.js"), ctx, { filename: "sink-link.js" });
  const port = (tabId) => {
    const p = {};
    connect({
      name: "vc-sink",
      sender: { tab: { id: tabId }, frameId: 0, url: "https://www.ardmediathek.de/video/x" },
      onMessage: { addListener: (f) => (p.post = f) },
      onDisconnect: { addListener: (f) => (p.gone = f) },
      disconnect: () => {},
    });
    p.post({ t: "hello", session: `tailnet-${tabId}`, title: "test-tailnet.mjs" });
    return p;
  };
  return { ctx, sink, idb, port, run: (code) => vm.runInContext(code, ctx) };
};

let seq = 1;
const seg = (label, init = false) => ({
  t: "segment",
  header: { sb: 1, seq: seq++, ms: 1, mime: "video/mp4", isInit: init, kind: "mp4", tso: 0 },
  payload: "data:application/octet-stream;base64," + Buffer.from(`${label}|`, "latin1").toString("base64"),
});
const state = async (w) => (await w.run("tnStatus()")).state;

// pair
{
  const w = world();
  const r = await w.run(`tnPair("https://PC.Example.ts.net/", "${CODE}", "  my\\u0007 phone  ")`);
  check(r.ok === true && r.host === HOST, "pair: a pasted address is cut down to the host");
  check(w.sink.sockets.length === 1 && w.sink.sockets[0].url === `wss://${HOST}/`, "pair: one socket, wss to the PC's name");
  const said = w.sink.heard[0];
  check(said.type === "pair" && said.v === 1 && said.code === CODE && said.name === "my phone", "pair: the code and a cleaned name go out");
  check(/^04[0-9a-f]{128}$/.test(said.key), "pair: the public key is an uncompressed P-256 point");
  const kept = w.idb.get("pc");
  check(kept && kept.schema === 1 && kept.host === HOST && kept.device === DEVICE && kept.hub === HUB, "pair: the PC, the device id and the sink's id are kept, with a schema number");
  check(kept.key.type === "private" && kept.key.extractable === false, "pair: the private key is not extractable");
  let exported = true;
  await webcrypto.subtle.exportKey("pkcs8", kept.key).catch(() => (exported = false));
  check(!exported, "pair: exporting the private key fails");
  const status = await w.run("tnStatus()");
  check(status.paired && status.host === HOST && JSON.stringify(status).indexOf(CODE) < 0, "pair: the status names the PC, never the code");
}

// pair-errors
{
  const w = world();
  check((await w.run(`tnPair("pc", "${CODE}", "p")`)).reason === "host", "pair-errors: a name without a dot is no host");
  check((await w.run(`tnPair("evil.example/x#y", "${CODE}", "p")`)).reason === "host", "pair-errors: a path is no host");
  check((await w.run(`tnPair("pc.example.ts.net:8443", "${CODE}", "p")`)).reason === "host", "pair-errors: a port is refused");
  check((await w.run(`tnPair("${HOST}", "nope", "p")`)).reason === "code", "pair-errors: a code of another form");
  check(w.sink.sockets.length === 0, "pair-errors: none of them opened a socket");
  w.sink.mode = "refuse:code";
  check((await w.run(`tnPair("${HOST}", "${CODE}", "p")`)).reason === "code", "pair-errors: the sink's refusal is passed on");
  w.sink.mode = "silent";
  check((await w.run(`tnPair("${HOST}", "${CODE}", "p")`)).reason === "unreachable", "pair-errors: a silent PC ends at the time limit");
  w.sink.mode = "close";
  check((await w.run(`tnPair("${HOST}", "${CODE}", "p")`)).reason === "unreachable", "pair-errors: a PC that is not there");
  w.sink.mode = "junk";
  check((await w.run(`tnPair("${HOST}", "${CODE}", "p")`)).reason === "unreachable", "pair-errors: an oversized answer is not parsed");
  check(!w.idb.has("pc") && (await state(w)) === "not-paired", "pair-errors: nothing was kept");
}

// login
{
  const w = world();
  await w.run(`tnPair("${HOST}", "${CODE}", "p")`);
  w.sink.heard.length = 0;
  const p = w.port(1);
  p.post(seg("I", true));
  p.post(seg("m1"));
  await until(() => w.sink.heard.some((h) => h.payload === "m1"));
  const types = w.sink.heard.map((h) => (h.type === "segment" ? h.payload : h.type)).join(" ");
  check(types === "login auth hello I m1", `login: login, auth, then hello and the media in order (${types})`);
  check(w.sink.signatureOk === true, "login: the signature verifies with the paired public key");
  const hello = w.sink.heard.find((h) => h.type === "hello");
  check(hello.token === "" && hello.url.startsWith("https://www.ardmediathek.de/"), "login: the hello carries no token");
  check(w.sink.sockets.at(-1).url === `wss://${HOST}/`, "login: wss to the paired PC only");
  check((await state(w)) === "connected", "login: the status says connected");
  p.gone();
  await sleep(60);
  check((await state(w)) === "idle", "login: after the video the status is back to paired");
}

// hub
{
  const w = world();
  await w.run(`tnPair("${HOST}", "${CODE}", "p")`);
  w.sink.heard.length = 0;
  w.sink.mode = "hub";
  const p = w.port(2);
  p.post(seg("I", true));
  await until(() => w.sink.heard.length > 0);
  await sleep(100);
  check(w.sink.heard.map((h) => h.type).join(" ") === "login", "hub: another sink gets the login and no signature, no hello, no media");
  check((await state(w)) === "hub-changed", "hub: the status says another sink answers");
  const before = w.sink.sockets.length;
  await sleep(1300);
  p.post(seg("m"));
  await sleep(150);
  check(w.sink.sockets.length === before, "hub: no further attempt until a new pairing");
  check(w.run("sinkLinkStatus(2)").dropped > 0, "hub: the segments are counted as dropped");
}

// revoked
{
  const w = world();
  await w.run(`tnPair("${HOST}", "${CODE}", "p")`);
  w.sink.mode = "refuse:device";
  const p = w.port(3);
  p.post(seg("I", true));
  await until(() => w.sink.heard.some((h) => h.type === "login"));
  await sleep(100);
  check((await state(w)) === "revoked", "revoked: the status says so");
  const gone = await w.run("tnStatus()");
  check(!w.idb.has("pc") && gone.paired === false && gone.host === HOST, "revoked: the key is deleted here too, the status still names the PC");
  const before = w.sink.sockets.length;
  await sleep(1300);
  p.post(seg("m"));
  await sleep(150);
  check(w.sink.sockets.length === before, "revoked: no further attempt");
  w.sink.mode = "ok";
  // The link's own backoff doubled while it was refused (seconds by now): the new pairing ends the wait, the
  // running link logs in with the new key at once, before any new segment.
  const waiting = w.run("[...sinkLinks.get(3).values()].every((l) => !l.ws && l.nextRetryAt > Date.now() + 1000)");
  w.sink.heard.length = 0;
  const soon = Date.now();
  check((await w.run(`tnPair("${HOST}", "${CODE}", "p")`)).ok === true && (await state(w)) !== "revoked", "revoked: a new pairing clears it");
  const loggedIn = await until(() => w.sink.heard.some((h) => h.type === "hello"));
  check(waiting, "revoked: the link was waiting for its next attempt, more than a second away");
  check(loggedIn && Date.now() - soon < 900, `revoked: the waiting link is in again at once after the new pairing (${Date.now() - soon} ms)`);
  p.post(seg("I2", true));
  p.post(seg("m9"));
  await until(() => w.sink.heard.some((h) => h.payload === "m9"));
  check(w.sink.heard.some((h) => h.type === "hello"), "revoked: after the new pairing the link is in again");
}

// unreachable
{
  const w = world();
  await w.run(`tnPair("${HOST}", "${CODE}", "p")`);
  w.sink.mode = "close";
  const p = w.port(4);
  p.post(seg("I", true));
  p.post(seg("m1"));
  await sleep(250);
  check((await state(w)) === "unreachable", "unreachable: the status says the PC does not answer");
  check(w.run("sinkLinkStatus(4)").connected === false, "unreachable: the link is not connected");
  await sleep(1300);
  p.post(seg("m2"));
  await sleep(200);
  check(w.run("sinkLinkStatus(4)").dropped > 0, "unreachable: the segments are counted as dropped, not kept without bound");
  w.sink.mode = "silent";
  await sleep(2200);
  p.post(seg("m3"));
  await sleep(300);
  check((await state(w)) === "unreachable", "unreachable: a PC that opens and then says nothing ends at the time limit");
}

// not-paired
{
  const w = world();
  const p = w.port(5);
  p.post(seg("I", true));
  p.post(seg("m1"));
  await sleep(200);
  check(w.sink.sockets.length === 0, "not-paired: no socket at all, the local sink's token is not used in its place");
  check((await state(w)) === "not-paired", "not-paired: the status says so");
}

// offer
{
  const w = world();
  const offer = (sender, code = CODE) => w.run(`tnOffer(${JSON.stringify(sender)}, ${JSON.stringify(code)})`);
  check(offer({ frameId: 1, url: `https://${HOST}/` }) === false, "offer: not from a frame inside a page");
  check(offer({ frameId: 0, url: `http://${HOST}/` }) === false, "offer: not from http");
  check(offer({ frameId: 0, url: `https://${HOST}:8443/` }) === false, "offer: not with a port");
  check(offer({ frameId: 0, url: "https://localhost/" }) === false, "offer: not a name without a dot");
  check(offer({ frameId: 0, url: `https://${HOST}/` }, "<script>") === false, "offer: not with a code of another form");
  check((await w.run("tnStatus()")).offer === "", "offer: none of them left an offer");
  check(offer({ frameId: 0, url: `https://${HOST}/` }) === true, "offer: a top-level https page of a plain host");
  const status = await w.run("tnStatus()");
  check(status.offer === HOST && JSON.stringify(status).indexOf(CODE) < 0, "offer: the status names the PC, never the code");
  w.run("tn.offer.until = Date.now() - 1");
  check((await w.run("tnStatus()")).offer === "", "offer: gone after a minute");
}

// own-page: only the add-on's own pages may pair or forget; the popup is a tab on Firefox for Android.
{
  const w = world();
  const own = (sender) => w.run(`tnOwnPage(${JSON.stringify(sender)})`);
  check(own({ id: "addon-id", url: "moz-extension://0f3c1a52/popup/popup.html", tab: { id: 3 } }) === true, "own-page: the popup, also when it is shown as a tab");
  check(own({ id: "addon-id", url: "https://evil.example/", tab: { id: 3 }, frameId: 0 }) === false, "own-page: not a content script beside a web page");
  check(own({ id: "other-addon", url: "moz-extension://0f3c1a52/popup/popup.html" }) === false, "own-page: not another add-on");
  check(own({ id: "addon-id", url: "moz-extension://0f3c1a52.evil.example/" }) === false, "own-page: not a look-alike address");
  check(own({ id: "addon-id" }) === false && own(null) === false, "own-page: not a sender without an address");
}

// forget
{
  const w = world();
  await w.run(`tnPair("${HOST}", "${CODE}", "p")`);
  check((await w.run("tnForget()")) === true && !w.idb.has("pc"), "forget: the record with the key is deleted");
  check((await state(w)) === "not-paired", "forget: the status says not paired");
}

// stored
{
  for (const [what, stored] of [
    ["a host with a port", { schema: 1, key: {}, host: "pc.example.ts.net:1", device: DEVICE, hub: HUB }],
    ["a device id of another form", { schema: 1, key: {}, host: HOST, device: "x", hub: HUB }],
    ["no key", { schema: 1, host: HOST, device: DEVICE, hub: HUB }],
    ["a string", "pc.example.ts.net"],
    ["a record of another schema", { schema: 2, key: {}, host: HOST, device: DEVICE, hub: HUB }],
  ]) {
    const w = world({ stored });
    const p = w.port(6);
    p.post(seg("I", true));
    await sleep(120);
    check((await state(w)) === "not-paired" && w.sink.sockets.length === 0, `stored: ${what} reads as not paired`);
  }
}

// link: content.js hands the code of an opened pairing link to the service worker and takes it out of the address.
{
  const page = ({ href, top = true }) => {
    const u = new URL(href);
    const sent = [];
    const replaced = [];
    const win = {};
    const ctx = vm.createContext({
      window: win,
      location: { href, hash: u.hash, protocol: u.protocol, pathname: u.pathname, search: u.search },
      history: { replaceState: (_s, _t, to) => replaced.push(to) },
      URLSearchParams,
      console: { debug: () => {}, warn: () => {}, log: () => {} },
      chrome: { runtime: { id: "x", lastError: undefined, sendMessage: (m, cb) => (sent.push(m), cb && cb()), onMessage: { addListener: () => {} } } },
    });
    win.top = top ? win : {};
    try {
      vm.runInContext(src("content.js"), ctx, { filename: "content.js" });
    } catch (e) {
      // The rest of the script needs a real page; the pairing link is handled first.
    }
    return { offers: sent.filter((m) => m.kind === "vc-tailnet-offer"), replaced };
  };
  const link = `https://${HOST}/?x=1#streamtap-pair=1&code=${CODE}`;
  const a = page({ href: link });
  check(a.offers.length === 1 && a.offers[0].code === CODE && Object.keys(a.offers[0]).length === 2, "link: the code goes to the service worker, and nothing else (the host is the browser's to say)");
  check(a.replaced.length === 1 && a.replaced[0] === "/?x=1", "link: the fragment leaves the address at once");
  check(page({ href: link, top: false }).offers.length === 0, "link: not from a frame inside a page");
  check(page({ href: link.replace("https:", "http:") }).offers.length === 0, "link: not from http");
  const b = page({ href: `https://${HOST}/#streamtap-pair=1&code=<script>` });
  check(b.offers.length === 0 && b.replaced.length === 1, "link: a code of another form is dropped, the fragment still leaves");
  check(page({ href: `https://${HOST}/#other` }).replaced.length === 0, "link: another fragment is left alone");
}

if (failures.length) {
  console.error(`FAIL: ${failures.length} check(s) failed`);
  process.exit(1);
}
console.log("ok: all checks passed");
process.exit(0);
