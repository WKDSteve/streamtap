// The target "the sink on the PC over the tailnet" (loaded by background.js in front of sink-link.js).
//
// The sink listens on the PC's loopback only; `tailscale serve` on the PC gives it a name and a certificate inside
// the tailnet (Tailscale, or a Headscale that issues certificates), so this browser reaches it as wss://<pc>/ and
// nowhere else. There is no shared token on this way. The phone is paired once: the PC shows a one-time code
// (`streamtap-sink devices pair`: a link as a QR code, good for a minute), this module makes an ECDSA P-256 key pair
// whose private half WebCrypto keeps non-extractable (it can sign, nobody can read it out, not even this add-on) and
// sends the public half with the code. From then on every connection logs in: the sink sends a challenge with its
// own id, the id is compared with the one of the pairing (another sink on the same name gets no signature), the key
// signs the challenge together with the origin it speaks to, and only after the sink's `ok` the link of sink-link.js
// sees an open socket and sends its hello and the segments.
//
// The pairing (key, PC, device id, the sink's id) lives in IndexedDB, in one record with a schema number; "forget"
// deletes it and reads back that it is gone. On the PC `streamtap-sink devices revoke <id>` ends it from the other
// side: the module then deletes the key here too (MSEC-12), and the popup says so and asks for a new pairing
// instead of trying again.
//
// What the sink says is JSON in text messages, bounded here before it is parsed. Every wait has a time limit, so a
// tunnel that hangs never leaves a link waiting, and each state (not paired, PC not reachable, revoked, another
// sink, update needed) is kept for the popup (MOB-09).
"use strict";

const TN_PROTOCOL = 1;
const TN_ANSWER_MS = 15000; // pairing, and login up to the sink's ok
const TN_OFFER_MS = 60000; // as long as the code of a pairing link lives
const TN_MAX_ANSWER = 1024; // characters of one message of the sink
const TN_DB = "streamtap-tailnet";
const TN_STORE = "pairing";
const TN_KEY = "pc";
const TN_SCHEMA = 1; // of the stored record
// WebSocket.readyState values, which sink-link.js compares with.
const TN_CONNECTING = 0;
const TN_OPEN = 1;
const TN_CLOSED = 3;
const TN_HEX32 = /^[0-9a-f]{32}$/;

const tn = {
  pairing: undefined, // undefined: not read yet; null: not paired; else { key, host, device, hub }
  state: "idle", // the last thing that happened, for the popup
  open: 0, // sockets logged in right now
  blocked: "", // "revoked" | "hub-changed" | "version": no further attempt until a new pairing
  offer: null, // { host, code, until } from a pairing link opened in this browser
  lastHost: "", // the PC of a pairing that was revoked, for the popup's words
};

const tnAvailable = () =>
  typeof indexedDB !== "undefined" && typeof crypto !== "undefined" && Boolean(crypto.subtle) && typeof WebSocket === "function";

// The name of the PC in the tailnet: a plain host name in lower case with a dot, as the sink's --tailnet takes it.
// A pasted address (https://pc.example.ts.net/) is cut down to it; anything else is "".
const tnHost = (value) => {
  const host = String(value ?? "")
    .trim()
    .toLowerCase()
    .replace(/^https:\/\//, "")
    .replace(/\/$/, "");
  if (host.length > 253 || !host.includes(".")) return "";
  const label = /^[a-z0-9]([a-z0-9-]{0,61}[a-z0-9])?$/;
  return host.split(".").every((l) => label.test(l)) ? host : "";
};

// A device name as one line of plain text.
const tnName = (value) =>
  String(value ?? "")
    .replace(/[\u0000-\u001f\u007f-\u009f\s]+/g, " ")
    .trim()
    .slice(0, 40) || "phone";

const tnHex = (buffer) => Array.from(new Uint8Array(buffer), (b) => b.toString(16).padStart(2, "0")).join("");

// One message in the sink's framing: 32-bit little-endian header length, JSON header.
const tnFrame = (header) => {
  const h = new TextEncoder().encode(JSON.stringify(header));
  const out = new Uint8Array(4 + h.byteLength);
  new DataView(out.buffer).setUint32(0, h.byteLength, true);
  out.set(h, 4);
  return out;
};

// What the sink said, when it is a small JSON object with a type; else null.
const tnParse = (data) => {
  if (typeof data !== "string" || data.length > TN_MAX_ANSWER) return null;
  try {
    const v = JSON.parse(data);
    return v && typeof v === "object" && typeof v.type === "string" ? v : null;
  } catch (e) {
    return null;
  }
};

const tnStore = (mode, run) =>
  new Promise((resolve, reject) => {
    const open = indexedDB.open(TN_DB, 1);
    open.onupgradeneeded = () => open.result.createObjectStore(TN_STORE);
    open.onerror = () => reject(open.error);
    open.onsuccess = () => {
      const db = open.result;
      const tx = db.transaction(TN_STORE, mode);
      const request = run(tx.objectStore(TN_STORE));
      tx.oncomplete = () => {
        db.close();
        resolve(request.result);
      };
      tx.onerror = tx.onabort = () => {
        db.close();
        reject(tx.error);
      };
    };
  });

// What is stored is not trusted when read back (MSEC-13): a record of another form counts as "not paired".
const tnValid = (p) =>
  Boolean(p) && typeof p === "object" && p.schema === TN_SCHEMA && tnHost(p.host) === p.host && TN_HEX32.test(p.device) && TN_HEX32.test(p.hub) && Boolean(p.key) && typeof p.key === "object";

const tnLoad = async () => {
  if (tn.pairing !== undefined) return tn.pairing;
  if (!tnAvailable()) return (tn.pairing = null);
  try {
    const p = await tnStore("readonly", (s) => s.get(TN_KEY));
    tn.pairing = tnValid(p) ? p : null;
  } catch (e) {
    tn.pairing = null;
  }
  return tn.pairing;
};

// The pairing for sink-link.js; null (and the state says why) when this browser is not paired.
const tnSettings = async () => {
  const p = await tnLoad();
  if (!p && !tn.blocked) tn.state = "not-paired";
  return p;
};

// One question to the sink: opens a socket, sends `header`, resolves with the first answer; null when the PC does
// not answer in time.
const tnAsk = (host, header) =>
  new Promise((resolve) => {
    let ws = null;
    let done = false;
    const finish = (answer) => {
      if (done) return;
      done = true;
      clearTimeout(timer);
      try {
        if (ws) ws.close();
      } catch (e) {
        // already closed
      }
      resolve(answer);
    };
    const timer = setTimeout(() => finish(null), TN_ANSWER_MS);
    try {
      ws = new WebSocket(`wss://${host}/`);
    } catch (e) {
      finish(null);
      return;
    }
    ws.onopen = () => ws.send(tnFrame(header));
    ws.onmessage = (ev) => finish(tnParse(ev.data));
    ws.onclose = () => finish(null);
    ws.onerror = () => {};
  });

// Pairs this browser with the sink on `host` using the one-time `code`. Resolves { ok, host } or { ok: false, reason }.
// The key pair is made here and its private half is never exportable; a pairing that fails keeps the old one.
const tnPair = async (hostValue, code, nameValue) => {
  if (!tnAvailable()) return { ok: false, reason: "unsupported" };
  const host = tnHost(hostValue);
  if (!host) return { ok: false, reason: "host" };
  if (!TN_HEX32.test(String(code))) return { ok: false, reason: "code" };
  let keys = null;
  let publicKey = "";
  try {
    keys = await crypto.subtle.generateKey({ name: "ECDSA", namedCurve: "P-256" }, false, ["sign"]);
    publicKey = tnHex(await crypto.subtle.exportKey("raw", keys.publicKey));
  } catch (e) {
    return { ok: false, reason: "unsupported" };
  }
  const said = await tnAsk(host, { type: "pair", v: TN_PROTOCOL, code, name: tnName(nameValue), key: publicKey });
  if (!said) return { ok: false, reason: "unreachable" };
  if (said.type !== "paired") return { ok: false, reason: typeof said.reason === "string" ? said.reason.slice(0, 32) : "refused" };
  if (!TN_HEX32.test(said.device) || !TN_HEX32.test(said.hub)) return { ok: false, reason: "refused" };
  const pairing = { schema: TN_SCHEMA, key: keys.privateKey, host, device: said.device, hub: said.hub };
  try {
    await tnStore("readwrite", (s) => s.put(pairing, TN_KEY));
  } catch (e) {
    return { ok: false, reason: "storage" };
  }
  tn.pairing = pairing;
  tn.blocked = "";
  tn.state = "idle";
  tn.offer = null;
  // A capture that is running waits for its next attempt (the sink link's backoff, up to 10 s): it logs in with
  // the new key now.
  if (typeof sinkRetryNow === "function") sinkRetryNow();
  return { ok: true, host };
};

// Forgets the pairing on this device: the key is gone, nothing is sent to the PC any more. (Revoke it on the PC
// too: `streamtap-sink devices revoke <id>`.)
const tnForget = async () => {
  if (!(await tnDelete())) return false;
  tn.blocked = "";
  tn.state = "not-paired";
  tn.lastHost = "";
  return true;
};

// Deletes the stored pairing and reads back that it is gone (MSEC-12). False: it is still there.
const tnDelete = async () => {
  try {
    if (tnAvailable()) {
      await tnStore("readwrite", (s) => s.delete(TN_KEY));
      if ((await tnStore("readonly", (s) => s.get(TN_KEY))) !== undefined) return false;
    }
  } catch (e) {
    return false;
  }
  tn.pairing = null;
  return true;
};

// A pairing link opened in a tab (content.js saw `#streamtap-pair=1&code=...` on a top-level https page). The host
// is the browser's (`sender.url`), never the page's word; the popup names it and the person confirms there.
const tnOffer = (sender, code) => {
  if (!sender || sender.frameId !== 0 || !TN_HEX32.test(String(code))) return false;
  let url = null;
  try {
    url = new URL(sender.url);
  } catch (e) {
    return false;
  }
  const host = url.protocol === "https:" && !url.port ? tnHost(url.hostname) : "";
  if (!host) return false;
  tn.offer = { host, code, until: Date.now() + TN_OFFER_MS };
  return true;
};

// Whether a message comes from a page of the add-on itself (the popup; on Firefox for Android it opens as a tab,
// so `sender.tab` says nothing). A content script's sender carries the URL of the web page it runs beside.
const tnOwnPage = (sender) =>
  Boolean(sender) && sender.id === chrome.runtime.id && typeof sender.url === "string" && sender.url.startsWith(chrome.runtime.getURL(""));

const tnOpenOffer = () => {
  if (tn.offer && Date.now() >= tn.offer.until) tn.offer = null;
  return tn.offer;
};

// For the popup: never the code, never the key.
const tnStatus = async () => {
  const p = await tnLoad();
  const offer = tnOpenOffer();
  return {
    available: tnAvailable(),
    paired: Boolean(p),
    host: p ? p.host : tn.lastHost,
    device: p ? p.device : "",
    state: tn.blocked || (tn.open > 0 ? "connected" : !p ? "not-paired" : tn.state),
    offer: offer ? offer.host : "",
  };
};

// A socket to the sink that counts as open only once the device is logged in. sink-link.js uses it in place of a
// WebSocket: readyState, bufferedAmount, send, close, onopen, onclose.
class TailnetSocket {
  constructor(pairing) {
    this.readyState = TN_CONNECTING;
    this.binaryType = "arraybuffer";
    this.onopen = null;
    this.onclose = null;
    this.onerror = null;
    this._ws = null;
    this._done = false;
    this._asked = false;
    this._timer = null;
    // Revoked, another sink, or too old: no network until the person pairs again (or updates).
    if (!pairing || tn.blocked) {
      setTimeout(() => this._end(tn.blocked || "not-paired"), 0);
      return;
    }
    tn.state = "connecting";
    try {
      this._ws = new WebSocket(`wss://${pairing.host}/`);
    } catch (e) {
      setTimeout(() => this._end("unreachable"), 0);
      return;
    }
    const ws = this._ws;
    ws.binaryType = "arraybuffer";
    this._timer = setTimeout(() => this._end("unreachable"), TN_ANSWER_MS);
    ws.onopen = () => ws.send(tnFrame({ type: "login", v: TN_PROTOCOL, device: pairing.device }));
    ws.onmessage = (ev) => {
      this._heard(pairing, tnParse(ev.data)).catch(() => this._end("unreachable"));
    };
    // After the login the sink ended it (stopped, its time limit, a revocation): the link tries again and learns which.
    ws.onclose = () => this._end(this.readyState === TN_OPEN ? "lost" : "unreachable");
    ws.onerror = () => {};
  }

  get bufferedAmount() {
    return this._ws ? this._ws.bufferedAmount : 0;
  }

  async _heard(pairing, said) {
    if (this._done || this.readyState !== TN_CONNECTING || !said) return;
    if (said.type === "challenge" && !this._asked) {
      this._asked = true;
      // Sign only for the sink of the pairing (MSEC-04): another one on this name gets nothing.
      if (said.hub !== pairing.hub) {
        tn.blocked = "hub-changed";
        this._end("hub-changed");
        return;
      }
      if (typeof said.challenge !== "string" || !/^[0-9a-f]{64}$/.test(said.challenge)) {
        this._end("unreachable");
        return;
      }
      const message = `appkit-device-login/1\nhttps://${pairing.host}\n${pairing.device}\n${said.challenge}`;
      const signature = await crypto.subtle.sign({ name: "ECDSA", hash: "SHA-256" }, pairing.key, new TextEncoder().encode(message));
      if (!this._done) this._ws.send(tnFrame({ type: "auth", signature: tnHex(signature) }));
      return;
    }
    if (said.type === "ok" && this._asked) {
      clearTimeout(this._timer);
      this.readyState = TN_OPEN;
      tn.open++;
      tn.state = "connected";
      if (this.onopen) this.onopen();
      return;
    }
    if (said.type === "refused") {
      // The PC does not know this device (any more), or speaks another version: trying again changes nothing.
      const why = said.reason === "device" ? "revoked" : said.reason === "version" ? "version" : "";
      if (why) tn.blocked = why;
      // Revoked on the PC: the key is of no use any more and leaves this device too. Should the deletion fail, the
      // module still sends nothing (blocked); "forget" in the popup tries again.
      if (why === "revoked") {
        tn.lastHost = pairing.host;
        await tnDelete();
      }
      this._end(why || "refused");
    }
  }

  send(data) {
    if (this.readyState === TN_OPEN) this._ws.send(data);
  }

  // The link closes it (the video ended, the tab is gone).
  close() {
    this._end("");
  }

  _end(why) {
    if (this._done) return;
    this._done = true;
    clearTimeout(this._timer);
    if (this.readyState === TN_OPEN) tn.open = Math.max(0, tn.open - 1);
    this.readyState = TN_CLOSED;
    if (why) tn.state = why;
    else if (tn.open === 0 && tn.state === "connected") tn.state = "idle";
    try {
      if (this._ws) this._ws.close();
    } catch (e) {
      // already closed
    }
    if (this.onclose) this.onclose();
  }
}
