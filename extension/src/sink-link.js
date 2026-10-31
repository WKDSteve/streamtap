// Service-worker side of the sink link (loaded by background.js via importScripts).
//
// Every content script that taps segments opens one runtime port named
// "vc-sink" per MediaSource. The service worker owns one WebSocket to the
// local sink per port, so the sink sees the extension's own origin
// (chrome-extension://<id>) and can compare it exactly; the page's CSP never
// applies to this socket.
//
// Port messages are JSON (Chrome extension messaging cannot carry an
// ArrayBuffer), so payloads arrive as data: URLs and are decoded with fetch().
//
// Wire format to the sink per binary message: u32 LE header length | JSON
// header | payload. The first message is {"type":"hello","token":...}.
"use strict";

const SINK_MAX_PENDING_BYTES = 64 * 1024 * 1024; // per link, while connecting only
const SINK_MAX_PENDING_TOTAL = 128 * 1024 * 1024; // all links of the worker
const SINK_MAX_BUFFERED_BYTES = 64 * 1024 * 1024; // per socket, unsent while open
const SINK_MAX_INIT_BYTES = 1024 * 1024; // init segments kept for a new socket
const SINK_MAX_KEPT_INITS = 16;
const SINK_MAX_LINKS = 16; // tapping ports of the whole worker
const SINK_STABLE_MS = 5000; // a socket open this long resets the backoff
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
  tfdt: typeof h.tfdt === "number" || typeof h.tfdt === "string" ? h.tfdt : null,
  timescale: h.timescale ?? null,
  dur: h.dur ?? null,
  tso: h.tso ?? null,
  seqMode: h.seqMode === true,
});

// tabId -> Map<portId, link>; read by the popup through "vc-link-status".
const sinkLinks = new Map();
let nextPortId = 1;
let sinkLinkCount = 0;
let sinkPendingTotal = 0;

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

// An item leaves the stream (sent or dropped). Init segments are remembered
// at this point, in stream order: everything still pending after it belongs
// to it, so a new socket replays exactly the inits in effect.
const linkConsumed = (link, item) => {
  if (!item.isInit || item.blob.size > SINK_MAX_INIT_BYTES) return;
  link.inits.delete(item.sb);
  link.inits.set(item.sb, item.blob);
  while (link.inits.size > SINK_MAX_KEPT_INITS) link.inits.delete(link.inits.keys().next().value);
};

const linkDropPending = (link) => {
  for (const item of link.pending) {
    linkConsumed(link, item);
    link.dropped++;
  }
  sinkPendingTotal -= link.pendingBytes;
  link.pending = [];
  link.pendingBytes = 0;
};

const linkWrite = (link, ws, item) => {
  ws.send(item.blob);
  link.sent++;
  link.sentBytes += item.blob.size;
  linkConsumed(link, item);
};

const linkConnect = async (link) => {
  // A closed port still gets its queue delivered; it just opens no new socket
  // once that is done.
  if (link.ws || link.connecting || Date.now() < link.nextRetryAt || !link.hello) return;
  if (link.closed && !link.pending.length) return;
  // Segments arriving while the settings are read are queued, not dropped.
  link.connecting = true;
  const settings = await sinkSettings();
  let ws = null;
  if (settings) {
    try {
      ws = new WebSocket(`ws://127.0.0.1:${settings.port}/`);
    } catch (e) {
      ws = null;
    }
  }
  link.connecting = false;
  if (!ws) {
    // Not configured (metadata-only mode) or refused.
    linkDropPending(link);
    link.nextRetryAt = Date.now() + link.retryDelay;
    link.retryDelay = Math.min(link.retryDelay * 2, 10000);
    return;
  }
  ws.binaryType = "arraybuffer";
  link.ws = ws;
  let openedAt = 0;
  ws.onopen = () => {
    openedAt = Date.now();
    ws.send(sinkFrame({ type: "hello", token: settings.token, ...link.hello }));
    // Every socket is a new capture directory on the sink, and the player
    // appended its init segments only once: send the ones in effect first
    // (an identical init in the queue is deduped by the sink).
    for (const blob of link.inits.values()) {
      ws.send(blob);
      link.sent++;
      link.sentBytes += blob.size;
    }
    const queued = link.pending;
    sinkPendingTotal -= link.pendingBytes;
    link.pending = [];
    link.pendingBytes = 0;
    for (const item of queued) linkWrite(link, ws, item);
    // WebSocket.close() still sends what is buffered.
    if (link.closed) ws.close();
  };
  ws.onclose = () => {
    if (link.ws === ws) link.ws = null;
    linkDropPending(link);
    // A sink that closes right after the hello (wrong token, refused) must
    // not be hammered: only a socket that stayed open resets the backoff.
    if (openedAt && Date.now() - openedAt >= SINK_STABLE_MS) link.retryDelay = 1000;
    link.nextRetryAt = Date.now() + link.retryDelay;
    link.retryDelay = Math.min(link.retryDelay * 2, 10000);
  };
  ws.onerror = () => {
    // onclose follows.
  };
};

const linkSend = (link, item) => {
  if (link.overflow) {
    link.dropped++;
    return;
  }
  const ws = link.ws;
  if (ws && ws.readyState === WebSocket.OPEN) {
    // Backpressure: a stalled sink must not make the worker buffer without
    // bound. Init segments always go (small, and the media after them is
    // useless without them).
    if (!item.isInit && ws.bufferedAmount + item.blob.size > SINK_MAX_BUFFERED_BYTES) {
      linkConsumed(link, item);
      link.dropped++;
      return;
    }
    linkWrite(link, ws, item);
    return;
  }
  // No socket and the retry window has passed: start connecting now and
  // queue this item instead of dropping it (linkConnect sets `connecting`
  // synchronously).
  if (!ws) linkConnect(link);
  const connecting = (link.ws && link.ws.readyState === WebSocket.CONNECTING) || (!link.ws && link.connecting);
  const size = item.blob.size;
  if (
    connecting &&
    link.pendingBytes + size <= SINK_MAX_PENDING_BYTES &&
    sinkPendingTotal + size <= SINK_MAX_PENDING_TOTAL
  ) {
    link.pending.push(item);
    link.pendingBytes += size;
    sinkPendingTotal += size;
    return;
  }
  // Not connected: never buffer unboundedly. Count and drop.
  linkConsumed(link, item);
  link.dropped++;
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
    // Beyond the worker-wide limit a port is accepted but drops everything
    // (disconnecting would only make the content script reconnect).
    overflow: sinkLinkCount >= SINK_MAX_LINKS,
    inits: new Map(), // sb -> framed init in effect, replayed on every new socket
    pending: [], // { blob, sb, isInit } while connecting
    pendingBytes: 0,
    sent: 0,
    sentBytes: 0,
    dropped: 0,
    nextRetryAt: 0,
    retryDelay: 1000,
  };
  sinkLinkCount++;
  if (!sinkLinks.has(tabId)) sinkLinks.set(tabId, new Map());
  sinkLinks.get(tabId).set(portId, link);

  // Messages are decoded one after the other so the sink receives them in
  // append order even though decoding is asynchronous.
  let chain = Promise.resolve();
  port.onMessage.addListener((msg) => {
    chain = chain
      .then(async () => {
        if (!msg || typeof msg !== "object") return;
        if (msg.t === "hello") {
          // The frame URL comes from the browser, not from the page.
          link.hello = {
            session: String(msg.session || "").slice(0, 64),
            url: String(port.sender.url || "").slice(0, 2048),
            title: String(msg.title || "").slice(0, 300),
          };
          linkConnect(link);
          return;
        }
        if (!link.hello) return;
        if (msg.t === "segment" && msg.header && typeof msg.header === "object") {
          if (link.overflow) {
            link.dropped++;
            return;
          }
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
          linkSend(link, { blob: sinkFrame(header, payload), sb: header.sb, isInit: header.isInit });
          return;
        }
        if (msg.t === "event" && msg.header && typeof msg.header === "object") {
          const name = msg.header.name === "endofstream" || msg.header.name === "changetype" ? msg.header.name : null;
          if (name) {
            const blob = sinkFrame({ type: "event", name, sb: msg.header.sb ?? null });
            linkSend(link, { blob, sb: null, isInit: false });
          }
        }
      })
      .catch(() => {
        // One failed message must not stop the ones behind it.
        link.dropped++;
      });
  });

  port.onDisconnect.addListener(() => {
    link.closed = true;
    // Let the queued messages drain before the socket closes. A socket still
    // connecting closes itself after flushing its queue (onopen).
    chain.then(() => {
      if (link.ws && link.ws.readyState === WebSocket.OPEN) link.ws.close();
      sinkLinkCount--;
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
