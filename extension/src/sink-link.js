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
const SINK_MAX_KEPT_TOTAL = 64 * 1024 * 1024; // kept init bytes of all links (parked ones too)
const SINK_MAX_LINKS = 16; // tapping ports of the whole worker
const SINK_MAX_BACKLOG = 128 * 1024 * 1024; // data: URL chars received, not yet decoded
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
  initCont: h.initCont === true && h.isInit !== true,
  kind: typeof h.kind === "string" ? h.kind.slice(0, 16) : "",
  tfdt: typeof h.tfdt === "number" || typeof h.tfdt === "string" ? h.tfdt : null,
  timescale: h.timescale ?? null,
  dur: h.dur ?? null,
  tso: h.tso ?? null,
  seqMode: h.seqMode === true,
  tsoSet: h.tsoSet === true,
});

// The item for one segment; an init+media append also carries a framed copy
// of its init part alone, which is what a new socket replays.
const sinkItem = (header, payload, initLen) => {
  const item = {
    blob: sinkFrame(header, payload),
    sb: header.sb,
    isInit: header.isInit,
    initCont: header.initCont,
    keep: null,
  };
  if (header.isInit && Number.isSafeInteger(initLen) && initLen > 0 && initLen < payload.byteLength) {
    item.keep = sinkFrame({ ...header, tfdt: null, dur: null }, payload.slice(0, initLen));
  }
  return item;
};

// tabId -> Map<portId, link>; read by the popup through "vc-link-status".
const sinkLinks = new Map();
let nextPortId = 1;
let sinkLinkCount = 0; // links that may connect (not overflow)
let sinkPendingTotal = 0;
let sinkBacklog = 0;
let sinkKeptTotal = 0;

const blobsSize = (chunks) => (chunks ? chunks.reduce((n, b) => n + b.size, 0) : 0);

// The kept init with the most bytes on any link but `except`, as [link, sb].
const largestKept = (except) => {
  let best = null;
  let bestSize = 0;
  for (const m of sinkLinks.values()) {
    for (const l of m.values()) {
      if (l === except) continue;
      for (const [sb, chunks] of l.inits) {
        const n = blobsSize(chunks);
        if (n > bestSize) {
          best = [l, sb];
          bestSize = n;
        }
      }
    }
  }
  return best;
};

// Sets (or with null drops) the init bytes a link keeps for sb, within the
// worker-wide budget. A link's current init is never refused for another
// link's: the largest one kept elsewhere goes first (a page that forges big
// inits loses its own). What still does not fit is not kept at all, so a
// stale init is never replayed in its place. A link whose port is gone
// keeps nothing.
const linkKeep = (link, sb, chunks) => {
  const old = blobsSize(link.inits.get(sb));
  let size = blobsSize(chunks);
  if (link.gone || size > SINK_MAX_INIT_BYTES) {
    chunks = null;
    size = 0;
  }
  while (chunks && sinkKeptTotal - old + size > SINK_MAX_KEPT_TOTAL) {
    const victim = largestKept(link);
    if (!victim) {
      chunks = null;
      size = 0;
      break;
    }
    linkKeep(victim[0], victim[1], null);
  }
  link.inits.delete(sb);
  if (chunks) link.inits.set(sb, chunks);
  sinkKeptTotal += size - old;
};

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
// Continuation chunks of a split init join it. An init too large to keep
// also drops the old one, so the old quality's init is never replayed in
// front of the new quality's media.
// An init+media append keeps only its init part (item.keep).
const linkConsumed = (link, item) => {
  const keep = item.keep || item.blob;
  if (item.initCont) {
    const chunks = link.inits.get(item.sb);
    if (chunks) linkKeep(link, item.sb, [...chunks, keep]);
    return;
  }
  if (!item.isInit) return;
  linkKeep(link, item.sb, [keep]);
  while (link.inits.size > SINK_MAX_KEPT_INITS) linkKeep(link, link.inits.keys().next().value, null);
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
  if (link.overflow || link.ws || link.connecting || Date.now() < link.nextRetryAt || !link.hello) return;
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
    for (const chunks of link.inits.values()) {
      for (const blob of chunks) {
        ws.send(blob);
        link.sent++;
        link.sentBytes += blob.size;
      }
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
    // bound. Small init bytes go up to twice the limit (the media after them
    // is useless without them); everything else stops at the limit.
    const initBytes = (item.isInit || item.initCont) && item.blob.size <= SINK_MAX_INIT_BYTES;
    const limit = initBytes ? 2 * SINK_MAX_BUFFERED_BYTES : SINK_MAX_BUFFERED_BYTES;
    if (ws.bufferedAmount + item.blob.size > limit) {
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

// A link beyond the worker-wide limit is accepted but parked: it opens no
// socket and drops its segments until a slot frees up (checked again on each
// segment), so the content script does not have to reconnect.
const linkAdmit = (link) => {
  if (!link.overflow) return true;
  if (sinkLinkCount >= SINK_MAX_LINKS) return false;
  link.overflow = false;
  sinkLinkCount++;
  linkConnect(link);
  return true;
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
    overflow: true, // until linkAdmit gives it a slot
    inits: new Map(), // sb -> [framed init, continuation...] in effect, replayed on every new socket
    lostInit: new Set(), // sb whose init was dropped: nothing until the next init
    gone: false, // the port's cleanup ran: nothing is kept any more
    pending: [], // { blob, sb, isInit, initCont } while connecting
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
  // append order even though decoding is asynchronous. What waits for
  // decoding is bounded worker-wide (content.js counts a payload as handed
  // on as soon as postMessage returns).
  let chain = Promise.resolve();
  port.onMessage.addListener((msg) => {
    if (!msg || typeof msg !== "object") return;
    let held = 0;
    if (msg.t === "segment") {
      held = typeof msg.payload === "string" ? msg.payload.length : 0;
      // Small init bytes may go up to twice the cap: media after a lost init
      // would be filed under the previous one.
      const h = msg.header || {};
      const initBytes = h.isInit === true || h.initCont === true;
      const smallInit = initBytes && held <= 2 * SINK_MAX_INIT_BYTES;
      if (sinkBacklog + held > (smallInit ? 2 * SINK_MAX_BACKLOG : SINK_MAX_BACKLOG)) {
        link.dropped++;
        // Media after a lost init would be filed under the previous one:
        // in stream order, that SourceBuffer takes nothing until its next
        // init, and its kept init is not replayed.
        if (initBytes) {
          const sb = h.sb;
          chain = chain.then(() => {
            if (!link.lostInit.has(sb) && Number.isSafeInteger(sb) && sb >= 0 && linkAdmit(link)) {
              // The sink notes it, and mux says the capture of sb ends here.
              linkSend(link, { blob: sinkFrame({ type: "event", name: "initlost", sb }), sb: null, isInit: false, initCont: false });
            }
            link.lostInit.add(sb);
            linkKeep(link, sb, null);
          });
        }
        return;
      }
      sinkBacklog += held;
    }
    chain = chain
      .then(async () => {
        if (msg.t === "hello") {
          // The frame URL comes from the browser, not from the page.
          link.hello = {
            session: String(msg.session || "").slice(0, 64),
            url: String(port.sender.url || "").slice(0, 2048),
            title: String(msg.title || "").slice(0, 300),
          };
          if (linkAdmit(link)) linkConnect(link);
          return;
        }
        if (!link.hello) return;
        if (msg.t === "segment" && msg.header && typeof msg.header === "object") {
          const admitted = linkAdmit(link);
          const initBytes = msg.header.isInit === true || msg.header.initCont === true;
          if (msg.header.isInit === true) link.lostInit.delete(msg.header.sb);
          else if (link.lostInit.has(msg.header.sb)) {
            link.dropped++;
            return;
          }
          // A parked link still keeps its init bytes, so it can replay them
          // once it is admitted; its media is dropped.
          if (!admitted && !initBytes) {
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
          const item = sinkItem(header, payload, msg.header.initLen);
          if (!admitted) {
            linkConsumed(link, item);
            link.dropped++;
            return;
          }
          linkSend(link, item);
          return;
        }
        if (msg.t === "event" && msg.header && typeof msg.header === "object") {
          const name = msg.header.name === "endofstream" || msg.header.name === "changetype" ? msg.header.name : null;
          if (name && linkAdmit(link)) {
            const blob = sinkFrame({ type: "event", name, sb: msg.header.sb ?? null });
            linkSend(link, { blob, sb: null, isInit: false, initCont: false });
          }
        }
      })
      .catch(() => {
        // One failed message must not stop the ones behind it.
        link.dropped++;
      })
      .finally(() => {
        sinkBacklog -= held;
      });
  });

  port.onDisconnect.addListener(() => {
    link.closed = true;
    // Let the queued messages drain before the socket closes. A socket still
    // connecting closes itself after flushing its queue (onopen).
    chain.then(() => {
      if (link.ws && link.ws.readyState === WebSocket.OPEN) link.ws.close();
      if (!link.overflow) sinkLinkCount--;
      link.overflow = true; // never admitted again
      for (const sb of [...link.inits.keys()]) linkKeep(link, sb, null);
      // Items still queued on a connecting socket are consumed later; they
      // must not be kept for a link that is gone.
      link.gone = true;
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
