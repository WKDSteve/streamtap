// Service-worker side of the sink link (loaded by background.js via importScripts).
//
// Every content script that taps segments opens a runtime port named
// "vc-sink". The service worker owns one WebSocket to the local sink per port,
// so the sink sees the extension's own origin (chrome-extension://<id>) and
// can compare it exactly; the page's CSP never applies to this socket.
//
// Port messages are JSON (Chrome extension messaging cannot carry an
// ArrayBuffer), so payloads arrive as data: URLs and are decoded with fetch().
//
// Wire format to the sink per binary message: u32 LE header length | JSON
// header | payload. The first message is {"type":"hello","token":...}.
"use strict";

const SINK_MAX_PENDING_BYTES = 64 * 1024 * 1024; // per port, while CONNECTING only
const SINK_MAX_BUFFERED_BYTES = 64 * 1024 * 1024; // per socket, unsent while OPEN
const SINK_MAX_INIT_BYTES = 2 * 1024 * 1024; // init segments kept for a reconnect
const SINK_MAX_KEPT_INITS = 32;
const sinkEnc = new TextEncoder();

// The header fields the sink understands; everything else is dropped.
const sinkSegmentHeader = (h) => ({
  type: "segment",
  sb: h.sb,
  seq: h.seq,
  ms: h.ms ?? null,
  mime: typeof h.mime === "string" ? h.mime.slice(0, 256) : "",
  isInit: h.isInit === true,
  kind: typeof h.kind === "string" ? h.kind.slice(0, 16) : "",
  tfdt: h.tfdt ?? null,
  timescale: h.timescale ?? null,
  tso: h.tso ?? null,
});

// tabId -> Map<portId, link>; read by the popup through "vc-link-status".
const sinkLinks = new Map();
let nextPortId = 1;

const sinkFrame = (header, payload) => {
  const h = sinkEnc.encode(JSON.stringify(header));
  const len = new Uint8Array(4);
  new DataView(len.buffer).setUint32(0, h.byteLength, true);
  return payload ? new Blob([len, h, payload]) : new Blob([len, h]);
};

const sinkSettings = () =>
  new Promise((resolve) => {
    chrome.storage.local.get({ sinkPort: 17650, sinkToken: "" }, (v) =>
      resolve(v.sinkToken ? { port: Number(v.sinkPort) || 17650, token: v.sinkToken } : null)
    );
  });

const linkConnect = async (link) => {
  if (link.ws || link.connecting || link.closed || Date.now() < link.nextRetryAt || !link.hello) return;
  // Segments arriving while the settings are read are queued, not dropped.
  link.connecting = true;
  const settings = await sinkSettings();
  let ws = null;
  if (settings && !link.closed) {
    try {
      ws = new WebSocket(`ws://127.0.0.1:${settings.port}/`);
    } catch (e) {
      ws = null;
    }
  }
  link.connecting = false;
  if (!ws) {
    // Not configured (metadata-only mode) or refused: drop what was queued.
    link.dropped += link.pending.length;
    link.pending = [];
    link.pendingBytes = 0;
    link.nextRetryAt = Date.now() + link.retryDelay;
    return;
  }
  ws.binaryType = "arraybuffer";
  link.ws = ws;
  ws.onopen = () => {
    link.retryDelay = 1000;
    ws.send(sinkFrame({ type: "hello", token: settings.token, ...link.hello }));
    // A reconnect starts a new capture directory on the sink; the player
    // appended its init segments only once, so send the kept ones first
    // (an identical init in the pending queue is deduped by the sink).
    if (link.connections > 0) {
      for (const b of link.inits.values()) {
        ws.send(b);
        link.sent++;
        link.sentBytes += b.size;
      }
    }
    link.connections++;
    for (const b of link.pending) {
      ws.send(b);
      link.sent++;
      link.sentBytes += b.size;
    }
    link.pending = [];
    link.pendingBytes = 0;
  };
  ws.onclose = () => {
    link.ws = null;
    link.dropped += link.pending.length;
    link.pending = [];
    link.pendingBytes = 0;
    link.nextRetryAt = Date.now() + link.retryDelay;
    link.retryDelay = Math.min(link.retryDelay * 2, 10000);
  };
  ws.onerror = () => {
    // onclose follows.
  };
};

const linkSend = (link, blob) => {
  const ws = link.ws;
  if (ws && ws.readyState === WebSocket.OPEN) {
    // Backpressure: a stalled sink must not make the worker buffer without
    // bound.
    if (ws.bufferedAmount + blob.size > SINK_MAX_BUFFERED_BYTES) {
      link.dropped++;
      return;
    }
    ws.send(blob);
    link.sent++;
    link.sentBytes += blob.size;
    return;
  }
  // No socket and the retry window has passed: start connecting now and
  // queue this blob instead of dropping it (linkConnect sets `connecting`
  // synchronously).
  if (!ws) linkConnect(link);
  const connecting = (link.ws && link.ws.readyState === WebSocket.CONNECTING) || (!link.ws && link.connecting);
  if (connecting && link.pendingBytes + blob.size <= SINK_MAX_PENDING_BYTES) {
    link.pending.push(blob);
    link.pendingBytes += blob.size;
    return;
  }
  // Not connected: never buffer unboundedly. Count and drop.
  link.dropped++;
};

const linkKeepInit = (link, sb, blob) => {
  if (blob.size > SINK_MAX_INIT_BYTES) return;
  link.inits.delete(sb);
  link.inits.set(sb, blob);
  while (link.inits.size > SINK_MAX_KEPT_INITS) link.inits.delete(link.inits.keys().next().value);
};

const decodePayload = async (dataUrl) => {
  if (typeof dataUrl !== "string" || !dataUrl.startsWith("data:")) return null;
  return (await fetch(dataUrl)).arrayBuffer();
};

chrome.runtime.onConnect.addListener((port) => {
  if (port.name !== "vc-sink") return;
  const tabId = port.sender && port.sender.tab && port.sender.tab.id;
  if (typeof tabId !== "number") {
    port.disconnect();
    return;
  }
  const portId = nextPortId++;
  const link = {
    tabId,
    top: port.sender.frameId === 0,
    hello: null,
    ws: null,
    connecting: false,
    closed: false,
    connections: 0, // sockets opened so far
    inits: new Map(), // sb -> framed init segment, replayed after a reconnect
    pending: [],
    pendingBytes: 0,
    sent: 0,
    sentBytes: 0,
    dropped: 0,
    nextRetryAt: 0,
    retryDelay: 1000,
  };
  if (!sinkLinks.has(tabId)) sinkLinks.set(tabId, new Map());
  sinkLinks.get(tabId).set(portId, link);

  // Messages are decoded one after the other so the sink receives them in
  // append order even though decoding is asynchronous.
  let chain = Promise.resolve();
  port.onMessage.addListener((msg) => {
    chain = chain.then(async () => {
      if (!msg || typeof msg !== "object") return;
      if (msg.t === "hello") {
        // The frame URL comes from the browser, not from the page.
        link.hello = {
          session: String(msg.session || ""),
          url: port.sender.url || "",
          title: String(msg.title || "").slice(0, 300),
        };
        linkConnect(link);
        return;
      }
      if (!link.hello) return;
      if (msg.t === "segment" && msg.header && typeof msg.header === "object") {
        let payload = null;
        try {
          payload = await decodePayload(msg.payload);
        } catch (e) {
          payload = null;
        }
        if (!payload) {
          link.dropped++;
          return;
        }
        const header = sinkSegmentHeader(msg.header);
        const blob = sinkFrame(header, payload);
        if (header.isInit) linkKeepInit(link, header.sb, blob);
        linkSend(link, blob);
        return;
      }
      if (msg.t === "event" && msg.header && typeof msg.header === "object") {
        const name = msg.header.name === "endofstream" || msg.header.name === "changetype" ? msg.header.name : null;
        if (name) linkSend(link, sinkFrame({ type: "event", name, sb: msg.header.sb ?? null }));
      }
    }).catch(() => {
      // One failed message must not stop the ones behind it.
      link.dropped++;
    });
  });

  port.onDisconnect.addListener(() => {
    link.closed = true;
    // Let the queued messages drain before the socket closes.
    chain.then(() => {
      if (link.ws) link.ws.close();
      const m = sinkLinks.get(tabId);
      if (m) {
        m.delete(portId);
        if (!m.size) sinkLinks.delete(tabId);
      }
    });
  });
});

// Status of a tab's links for the popup: summed over all frames of the tab.
const sinkLinkStatus = (tabId) => {
  const m = sinkLinks.get(tabId);
  if (!m) return null;
  const s = { connected: false, links: 0, sent: 0, sentBytes: 0, dropped: 0 };
  for (const l of m.values()) {
    s.links++;
    s.connected = s.connected || !!(l.ws && l.ws.readyState === WebSocket.OPEN);
    s.sent += l.sent;
    s.sentBytes += l.sentBytes;
    s.dropped += l.dropped;
  }
  return s;
};
