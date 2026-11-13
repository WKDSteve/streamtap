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
const SINK_RETRY_MIN_MS = 1000; // first reconnect delay, and the one a stable socket restarts at
const SINK_RETRY_MAX_MS = 10000; // the backoff doubles up to this
const SINK_MAX_SB = 32; // distinct SourceBuffer ids per link (background.js MAX_SB_PER_MS)
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
  track: h.track ?? null,
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
    item.keep = sinkFrame({ ...header, tfdt: null, track: null, dur: null }, payload.slice(0, initLen));
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

// Groups of `links` by `keyOf`, the one keeping the most init bytes first;
// the group of `keeperKey` counts `adding` bytes more (the init about to be
// kept).
const keptShares = (links, keyOf, keeperKey, adding) => {
  const shares = new Map();
  for (const l of links) {
    const k = keyOf(l);
    let n = shares.has(k) ? shares.get(k) : k === keeperKey ? adding : 0;
    for (const chunks of l.inits.values()) n += blobsSize(chunks);
    shares.set(k, n);
  }
  return [...shares].sort((a, b) => b[1] - a[1]).map(([k]) => k);
};

// What to drop when the kept-init budget is full, as [link, sb]: the largest
// init of the frame that keeps the most bytes within the tab that keeps the
// most, the keeper's own tab and frame counted with the init it is about to
// keep. A page that forges inits fills its own tab's share and so loses its
// own; many small forged ones do not push another tab's init out, nor do
// an ad frame's another frame's of the same tab. The init being kept
// (`keeper`, `sb`) is never the one dropped.
const largestKept = (keeper, sb, adding) => {
  const all = [];
  for (const m of sinkLinks.values()) all.push(...m.values());
  for (const tab of keptShares(all, (l) => l.tabId, keeper.tabId, adding)) {
    const inTab = all.filter((l) => l.tabId === tab);
    const keeperFrame = tab === keeper.tabId ? keeper.frameId : undefined;
    for (const frame of keptShares(inTab, (l) => l.frameId, keeperFrame, adding)) {
      let best = null;
      let bestSize = 0;
      for (const l of inTab) {
        if (l.frameId !== frame) continue;
        for (const [s, chunks] of l.inits) {
          if (l === keeper && s === sb) continue;
          const n = blobsSize(chunks);
          if (n > bestSize) {
            best = [l, s];
            bestSize = n;
          }
        }
      }
      if (best) return best;
    }
  }
  return null;
};

// Sets (or with null drops) the init bytes a link keeps for sb, within the
// worker-wide budget: what does not fit makes room by `largestKept`. What
// still does not fit is not kept at all, so a stale init is never replayed
// in its place, and the link's next socket says so (initlost). A link whose
// port is gone keeps nothing.
const linkKeep = (link, sb, chunks) => {
  const old = blobsSize(link.inits.get(sb));
  let size = blobsSize(chunks);
  if (link.gone || size > SINK_MAX_INIT_BYTES) {
    // Too large to replay: the next socket says so, like for an evicted one.
    if (!link.gone && chunks) link.evicted.add(sb);
    chunks = null;
    size = 0;
  }
  while (chunks && sinkKeptTotal - old + size > SINK_MAX_KEPT_TOTAL) {
    const victim = largestKept(link, sb, size - old);
    if (!victim) {
      link.evicted.add(sb);
      chunks = null;
      size = 0;
      break;
    }
    linkKeep(victim[0], victim[1], null);
    // A new socket of that link cannot replay it: said then (initlost).
    victim[0].evicted.add(victim[1]);
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

// The target: the sink over a WebSocket (a port and a token), or a file on the device (file-target.js, where this
// browser can save one). Without either the extension stays in metadata-only mode.
const sinkSettings = () =>
  new Promise((resolve) => {
    chrome.storage.local.get({ sinkPort: 17650, sinkToken: "", sinkTarget: "" }, (v) => {
      if (v.sinkTarget === "file" && typeof fileTargetAvailable === "function" && fileTargetAvailable()) {
        resolve({ file: true, token: "" });
        return;
      }
      resolve(v.sinkToken ? { port: Number(v.sinkPort) || 17650, token: v.sinkToken } : null);
    });
  });

// An item leaves the stream (sent or dropped). Init segments are remembered
// at this point, in stream order: everything still pending after it belongs
// to it, so a new socket replays exactly the inits in effect.
// Continuation chunks of a split init join it. An init too large to keep
// also drops the old one, so the old quality's init is never replayed in
// front of the new quality's media.
// An init+media append keeps only its init part (item.keep).
// A lost-init marker (linkInitLost) drops the kept init at its place in the
// stream; the link's next socket says initlost.
const linkConsumed = (link, item) => {
  if (item.lostMark) {
    linkKeep(link, item.sb, null);
    link.evicted.add(item.sb);
    return;
  }
  const keep = item.keep || item.blob;
  if (item.initCont) {
    const chunks = link.inits.get(item.sb);
    if (chunks) linkKeep(link, item.sb, [...chunks, keep]);
    return;
  }
  if (!item.isInit) return;
  link.evicted.delete(item.sb);
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
  // A recorder (file-target.js) counts media to know whether a recording is worth saving; a WebSocket ignores it.
  ws.send(item.blob, !item.isInit && !item.initCont && Number.isSafeInteger(item.sb));
  link.sent++;
  link.sentBytes += item.blob.size;
  linkConsumed(link, item);
};

const initLostFrame = (sb) =>
  Number.isSafeInteger(sb) && sb >= 0 ? sinkFrame({ type: "event", name: "initlost", sb }) : null;

// Init bytes of sb were dropped (here or in content.js) while the stream
// goes on: media after them would be filed under the previous init. In
// stream order, that SourceBuffer takes nothing until its next init, its
// kept init is not replayed, and the sink is told (initlost; mux names the
// SourceBuffer). Returns false when nothing of sb is in flight, so the
// caller decides what is in effect for the next socket.
const linkInitLost = (link, sb) => {
  const ws = link.ws;
  if (ws && ws.readyState === WebSocket.OPEN) {
    // A few bytes: past the backpressure limit, so the sink always learns it.
    const frame = link.lostInit.has(sb) ? null : initLostFrame(sb);
    if (frame) linkWrite(link, ws, { blob: frame, sb: null, isInit: false, initCont: false, keep: null });
    link.lostInit.add(sb);
    linkKeep(link, sb, null);
    return true;
  }
  // Items of sb queued on a connecting socket still belong to the previous
  // init (kept or queued): a marker behind them drops it once they are out.
  let last = null;
  for (let i = link.pending.length - 1; i >= 0 && !last; i--) if (link.pending[i].sb === sb) last = link.pending[i];
  if (!last) return false;
  link.lostInit.add(sb);
  // Outside the byte limits; one per run of sb's items, so it stays bounded.
  if (!last.lostMark) link.pending.push({ lostMark: true, blob: initLostFrame(sb), sb, isInit: false, initCont: false, keep: null });
  return true;
};

// A socket of the link is being set up: its onopen replays the kept inits
// in front of what is queued.
const linkInFlight = (link) =>
  link.connecting || link.pending.length > 0 || !!(link.ws && link.ws.readyState === WebSocket.CONNECTING);

// The port is gone and its cleanup ran (`gone`): once no socket is being set
// up any more, it keeps nothing and leaves the registry. Until then its kept
// inits stay, so a socket that opens after the cleanup still sends media
// under its init.
const linkRelease = (link) => {
  if (!link.gone || linkInFlight(link)) return;
  for (const sb of [...link.inits.keys()]) linkKeep(link, sb, null);
  const m = sinkLinks.get(link.tabId);
  if (m) {
    m.delete(link.portId);
    if (!m.size) sinkLinks.delete(link.tabId);
  }
};

// The reconnect policy in one place: the next attempt is due after the current delay, which then doubles up to
// the cap. A socket that stayed open (`stable`) starts the backoff over first.
const scheduleRetry = (link, stable) => {
  if (stable) link.retryDelay = SINK_RETRY_MIN_MS;
  link.nextRetryAt = Date.now() + link.retryDelay;
  link.retryDelay = Math.min(link.retryDelay * 2, SINK_RETRY_MAX_MS);
};

const linkConnect = async (link) => {
  // A gone link is never admitted again (overflow), so it opens no new
  // socket; its late items reach linkSend before its cleanup runs.
  if (link.overflow || link.ws || link.connecting || Date.now() < link.nextRetryAt || !link.hello) return;
  // Segments arriving while the settings are read are queued, not dropped.
  link.connecting = true;
  const settings = await sinkSettings();
  let ws = null;
  if (settings && settings.file) {
    ws = new FileRecorder(link.tabId, link.hello.url);
  } else if (settings) {
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
    scheduleRetry(link, false);
    linkRelease(link);
    return;
  }
  ws.binaryType = "arraybuffer";
  link.ws = ws;
  let openedAt = 0;
  ws.onopen = () => {
    openedAt = Date.now();
    ws.send(sinkFrame({ type: "hello", token: settings.token, ...link.hello }));
    // An init that had to make room cannot be replayed: that SourceBuffer
    // resumes at its next init (in the queue or later), and the sink is told.
    const resume = new Set();
    for (const sb of link.evicted) {
      if (link.inits.has(sb)) continue;
      resume.add(sb);
      link.lostInit.add(sb);
      if (Number.isSafeInteger(sb) && sb >= 0) ws.send(sinkFrame({ type: "event", name: "initlost", sb }));
    }
    link.evicted.clear();
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
    for (const item of queued) {
      if (item.lostMark) {
        // Init bytes dropped while this was queued: what came before belongs
        // to the previous init, what follows waits for the next one.
        if (item.blob && !resume.has(item.sb)) {
          ws.send(item.blob);
          link.sent++;
          link.sentBytes += item.blob.size;
        }
        linkKeep(link, item.sb, null);
        resume.add(item.sb);
        link.lostInit.add(item.sb);
        continue;
      }
      // Queued media of such a SourceBuffer has no init on this socket.
      if (resume.has(item.sb)) {
        if (!item.isInit) {
          linkConsumed(link, item);
          link.dropped++;
          continue;
        }
        resume.delete(item.sb);
        link.lostInit.delete(item.sb);
      }
      linkWrite(link, ws, item);
    }
    // WebSocket.close() still sends what is buffered. A closed port whose
    // messages are still being decoded keeps the socket until they are out
    // (its cleanup closes it).
    if (link.gone) {
      ws.close();
      linkRelease(link);
    }
  };
  ws.onclose = () => {
    if (link.ws === ws) link.ws = null;
    linkDropPending(link);
    linkRelease(link);
    // A recording closed to be saved (file-target.js) refuses nothing: the next segment starts a new one at once.
    if (ws.restartAtOnce === true) {
      link.retryDelay = SINK_RETRY_MIN_MS;
      link.nextRetryAt = 0;
      return;
    }
    // A sink that closes right after the hello (wrong token, refused) must
    // not be hammered: only a socket that stayed open resets the backoff.
    scheduleRetry(link, Boolean(openedAt) && Date.now() - openedAt >= SINK_STABLE_MS);
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
      link.dropped++;
      // Of an init+media append the small init part still goes out (as
      // content.js does): the rendition's next media is not lost with it.
      const part = item.keep;
      if (part && part.size <= SINK_MAX_INIT_BYTES && ws.bufferedAmount + part.size <= 2 * SINK_MAX_BUFFERED_BYTES) {
        linkWrite(link, ws, { blob: part, sb: item.sb, isInit: true, initCont: false, keep: null });
        return;
      }
      // Dropped init bytes are not the init in effect: initlost now.
      if (item.isInit || item.initCont) linkInitLost(link, item.sb);
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
  // Small init bytes may be queued up to twice the limits, as above.
  const queue = (q) => {
    const size = q.blob.size;
    const n = (q.isInit || q.initCont) && size <= SINK_MAX_INIT_BYTES ? 2 : 1;
    if (link.pendingBytes + size > n * SINK_MAX_PENDING_BYTES || sinkPendingTotal + size > n * SINK_MAX_PENDING_TOTAL) return false;
    link.pending.push(q);
    link.pendingBytes += size;
    sinkPendingTotal += size;
    return true;
  };
  if (connecting && queue(item)) return;
  // Not connected: never buffer unboundedly. Count and drop.
  link.dropped++;
  // Of an init+media append the small init part is still queued (as on an
  // open socket): the rendition's next media is not lost with it.
  const part = item.keep;
  if (connecting && part && part.size <= SINK_MAX_INIT_BYTES) {
    if (queue({ blob: part, sb: item.sb, isInit: true, initCont: false, keep: null })) return;
  }
  // Dropped init bytes behind queued items of the same SourceBuffer must not
  // be replayed in front of them (linkInitLost). With nothing of it queued
  // (no socket, retry window) the dropped init is the one in effect for the
  // next socket and is kept.
  if ((item.isInit || item.initCont) && linkInitLost(link, item.sb)) return;
  linkConsumed(link, item);
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
    portId,
    frameId: port.sender.frameId,
    top: port.sender.frameId === 0,
    hello: null,
    ws: null,
    connecting: false,
    overflow: true, // until linkAdmit gives it a slot
    inits: new Map(), // sb -> [framed init, continuation...] in effect, replayed on every new socket
    lostInit: new Set(), // sb whose init was dropped: nothing until the next init
    gone: false, // the port's cleanup ran: nothing new is kept (linkRelease)
    evicted: new Set(), // sb whose kept init made room for another tab's
    sbs: new Set(), // sb ids seen, at most SINK_MAX_SB
    pending: [], // { blob, sb, isInit, initCont, keep, lostMark? } while connecting
    pendingBytes: 0,
    sent: 0,
    sentBytes: 0,
    dropped: 0,
    nextRetryAt: 0,
    retryDelay: SINK_RETRY_MIN_MS,
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
    // Each sb has its own kept init and initlost frames: a page forging
    // records with new ids gets no more than SINK_MAX_SB of them per link.
    if ((msg.t === "segment" || msg.t === "lost") && msg.header && typeof msg.header === "object") {
      const sb = msg.header.sb;
      if (!link.sbs.has(sb)) {
        if (link.sbs.size >= SINK_MAX_SB) {
          link.dropped++;
          return;
        }
        link.sbs.add(sb);
      }
    }
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
            // As for any other dropped init bytes (linkInitLost): initlost at
            // once on an open socket, behind what is queued of sb while
            // connecting, else on the next socket.
            if (Number.isSafeInteger(sb) && sb >= 0 && linkAdmit(link) && linkInitLost(link, sb)) return;
            linkKeep(link, sb, null);
            if (Number.isSafeInteger(sb) && sb >= 0) link.evicted.add(sb);
            link.lostInit.add(sb);
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
        if (msg.t === "lost" && msg.header && typeof msg.header === "object") {
          const sb = msg.header.sb;
          if (!Number.isSafeInteger(sb) || sb < 0) return;
          if (msg.header.drop === true) {
            // content.js dropped init bytes of the running stream: the init
            // kept here is stale from this point on.
            if (!linkAdmit(link) || !linkInitLost(link, sb)) {
              linkKeep(link, sb, null);
              link.evicted.add(sb);
            }
            return;
          }
          // content.js could not keep this SourceBuffer's init for a new
          // port (a restarted worker): handled like an evicted one.
          if (link.inits.has(sb)) return;
          if (link.ws && link.ws.readyState === WebSocket.OPEN) {
            if (!link.lostInit.has(sb) && linkAdmit(link)) {
              linkSend(link, { blob: sinkFrame({ type: "event", name: "initlost", sb }), sb: null, isInit: false, initCont: false });
            }
            link.lostInit.add(sb);
          } else {
            link.evicted.add(sb);
          }
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
    // Let the queued messages drain before the socket closes: this closes an
    // open one; a socket still connecting then closes itself after flushing
    // its queue (onopen, once `gone`).
    chain.then(() => {
      if (link.ws && link.ws.readyState === WebSocket.OPEN) link.ws.close();
      if (!link.overflow) sinkLinkCount--;
      link.overflow = true; // never admitted again
      // Items still queued on a connecting socket are consumed later; they
      // must not be kept for a link that is gone. The inits kept so far are
      // dropped once no socket is being set up any more (that socket replays
      // them first), else now.
      link.gone = true;
      linkRelease(link);
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
