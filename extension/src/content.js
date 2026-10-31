// Isolated-world content script.
//
// Forwards the hook's metadata records to the service worker (badge, popup)
// and hands every segment payload to the service worker over runtime ports
// ("vc-sink"), one port per MediaSource: one MediaSource is one video, and
// each port becomes one capture session on the sink. The service worker owns
// the WebSocket to the local sink (src/sink-link.js), so the page's CSP does
// not apply to it and the sink can check the extension's origin exactly.
//
// Port messages are JSON, so payloads travel as data: URLs (FileReader encodes
// natively). One promise chain per port keeps them in append order.
//
// The hook's records arrive via window.postMessage, which any page script can
// forge: every field that leaves this script is type-checked and bounded.
//
// Injected outside an extension (tests) there is no chrome.*: the script then
// only counts what it would have sent.
(() => {
  "use strict";

  const TAG = "[vc-content]";
  const inExtension = typeof chrome !== "undefined" && !!(chrome.runtime && chrome.runtime.id);

  // ---------------------------------------------------------------------------
  // Validation of the forgeable records
  // ---------------------------------------------------------------------------

  const isId = (v) => Number.isInteger(v) && v > 0 && v <= 0xffffffff;
  const isU64 = (v) => Number.isSafeInteger(v) && v >= 0;
  // tfdt above 2^53 comes as a decimal string (hook.js u64Exact).
  const isTfdt = (v) => isU64(v) || (typeof v === "string" && /^[0-9]{1,20}$/.test(v));
  const isNum = (v) => typeof v === "number" && Number.isFinite(v);
  const str = (v, max) => (typeof v === "string" ? v.slice(0, max) : "");
  const optId = (v) => (isId(v) ? v : null);
  const optNum = (v) => (isNum(v) ? v : null);

  // A bounded copy of a hook record for the service worker's registry. The
  // frame URL is this script's own, not the page's claim.
  const cleanRecord = (d) => {
    const base = { type: str(d.type, 32), frameUrl: location.href.slice(0, 2048) };
    switch (d.type) {
      case "append":
        return {
          ...base,
          sbId: optId(d.sbId),
          msId: optId(d.msId),
          mime: str(d.mime, 256),
          seq: isU64(d.seq) ? d.seq : 0,
          bytes: isU64(d.bytes) ? d.bytes : 0,
          kind: str(d.kind, 16),
          isInit: d.isInit === true,
          boxes: Array.isArray(d.boxes) ? d.boxes.slice(0, 16).map((b) => str(b, 8)) : [],
          startSec: optNum(d.startSec),
          durationSec: optNum(d.durationSec),
          encrypted: d.encrypted === true,
        };
      case "mediasource":
      case "endofstream":
      case "msclose":
        return { ...base, msId: optId(d.msId) };
      case "objecturl":
        return { ...base, msId: optId(d.msId), url: str(d.url, 512) };
      case "sourcebuffer":
      case "changetype":
        return { ...base, sbId: optId(d.sbId), msId: optId(d.msId), mime: str(d.mime, 256) };
      case "encrypted":
        return { ...base, sbId: optId(d.sbId), msId: optId(d.msId) };
      case "drm":
        return {
          ...base,
          keySystem: str(d.keySystem, 128),
          robustness: Array.isArray(d.robustness) ? d.robustness.slice(0, 10).map((r) => str(r, 64)) : [],
        };
      case "remove":
      case "timestampoffset":
        return { ...base, sbId: optId(d.sbId) };
      default:
        return null;
    }
  };

  // The segment header for the sink, or null when the record is malformed.
  const segmentHeader = (m) => {
    if (!isId(m.sbId) || !isU64(m.seq)) return null;
    if (m.msId != null && !isId(m.msId)) return null;
    if (m.tfdt != null && !isTfdt(m.tfdt)) return null;
    if (m.timescale != null && !isId(m.timescale)) return null;
    if (m.dur != null && !isU64(m.dur)) return null;
    if (m.tso != null && !isNum(m.tso)) return null;
    return {
      sb: m.sbId,
      seq: m.seq,
      ms: m.msId ?? null,
      mime: str(m.mime, 256),
      isInit: m.isInit === true,
      kind: str(m.kind, 16),
      tfdt: m.tfdt ?? null,
      timescale: m.timescale ?? null,
      dur: m.dur ?? null,
      tso: m.tso ?? null,
      seqMode: m.seqMode === true,
    };
  };

  // ---------------------------------------------------------------------------
  // Metadata relay
  // ---------------------------------------------------------------------------

  const forward = (record) => {
    if (!inExtension || !record) return;
    try {
      chrome.runtime.sendMessage({ kind: "vc-event", event: record }, () => {
        void chrome.runtime.lastError;
      });
    } catch (e) {
      // Extension context invalidated (reload during development): stay quiet.
    }
  };

  // ---------------------------------------------------------------------------
  // Segment relay: one port (one sink session) per MediaSource
  // ---------------------------------------------------------------------------

  const MAX_QUEUED_BYTES = 64 * 1024 * 1024; // all payloads waiting in this frame
  const MAX_INIT_BYTES = 1024 * 1024; // init segments kept for a new port
  const MAX_KEPT_INITS = 16;
  const MAX_LINKS = 8; // MediaSources tapped at the same time in this frame

  const links = new Map(); // ms key -> link
  const stats = { queuedBytes: 0, posted: 0, dropped: 0, dead: false };

  const toDataUrl = (buffer) =>
    new Promise((resolve, reject) => {
      const r = new FileReader();
      r.onload = () => resolve(r.result);
      r.onerror = () => reject(r.error);
      r.readAsDataURL(new Blob([buffer]));
    });

  const linkFor = (msId) => {
    const key = msId == null ? "pre-hook" : String(msId);
    let link = links.get(key);
    if (!link) {
      if (links.size >= MAX_LINKS) return null;
      link = {
        key,
        session: (crypto.randomUUID && crypto.randomUUID()) || String(Math.random()).slice(2),
        port: null,
        ports: 0,
        inits: new Map(), // sb -> { header, payload }: init in effect at the end of the chain
        chain: Promise.resolve(),
        closed: false,
      };
      links.set(key, link);
    }
    return link;
  };

  // Opens the link's port when there is none. A port after the first one (the
  // service worker restarted) first gets the init segments in effect at this
  // point of the chain: the sink starts a new capture directory, and the
  // player appended its inits only once.
  const ensurePort = async (link) => {
    if (link.port || link.closed || stats.dead || !inExtension) return link.port;
    let port;
    try {
      port = chrome.runtime.connect({ name: "vc-sink" });
    } catch (e) {
      stats.dead = true;
      return null;
    }
    link.port = port;
    link.ports++;
    port.onDisconnect.addListener(() => {
      void chrome.runtime.lastError;
      if (link.port === port) link.port = null;
      // A restarted service worker: reconnect on the next segment. A reloaded
      // extension: give up.
      if (!(chrome.runtime && chrome.runtime.id)) stats.dead = true;
    });
    port.postMessage({ t: "hello", session: link.session, title: document.title.slice(0, 300) });
    if (link.ports > 1) {
      for (const { header, payload } of link.inits.values()) {
        port.postMessage({ t: "segment", header, payload: await toDataUrl(payload) });
      }
    }
    return port;
  };

  const post = async (link, msg) => {
    const port = await ensurePort(link);
    if (!port) {
      stats.dropped++;
      return false;
    }
    try {
      port.postMessage(msg);
      stats.posted++;
      return true;
    } catch (e) {
      stats.dropped++;
      return false;
    }
  };

  // Remembered when it is handed on, in chain order: media still queued
  // behind it belongs to it, media before it to the previous one.
  const keepInit = (link, header, payload) => {
    if (!header.isInit || payload.byteLength > MAX_INIT_BYTES) return;
    link.inits.delete(header.sb);
    link.inits.set(header.sb, { header, payload });
    while (link.inits.size > MAX_KEPT_INITS) link.inits.delete(link.inits.keys().next().value);
  };

  const sendSegment = (header, payload) => {
    const size = payload.byteLength;
    const link = linkFor(header.ms);
    if (!link || link.closed || stats.dead || stats.queuedBytes + size > MAX_QUEUED_BYTES) {
      stats.dropped++;
      return;
    }
    stats.queuedBytes += size;
    link.chain = link.chain
      .then(() => toDataUrl(payload))
      .then((dataUrl) => post(link, { t: "segment", header, payload: dataUrl }))
      .then((sent) => {
        if (sent) keepInit(link, header, payload);
      })
      .catch(() => {
        stats.dropped++;
      })
      .finally(() => {
        stats.queuedBytes -= size;
      });
  };

  const sendEvent = (msId, header) => {
    const link = links.get(msId == null ? "pre-hook" : String(msId));
    if (!link || link.closed || stats.dead) return;
    link.chain = link.chain.then(() => post(link, { t: "event", header })).catch(() => {});
  };

  // The MediaSource closed: drain its queue, then end its sink session.
  const closeLink = (msId) => {
    const key = msId == null ? "pre-hook" : String(msId);
    const link = links.get(key);
    if (!link) return;
    link.closed = true;
    link.chain = link.chain
      .then(() => {
        if (link.port) link.port.disconnect();
        link.port = null;
        links.delete(key);
      })
      .catch(() => {});
  };

  // ---------------------------------------------------------------------------
  // Tapping on/off: only with a configured sink (token set)
  // ---------------------------------------------------------------------------

  // null until chrome.storage answers: appends are held (bounded) meanwhile,
  // so the first init segment is not lost while the setting is read.
  let tapping = inExtension ? null : false;
  let held = [];
  let heldBytes = 0;

  const setTapping = (on) => {
    tapping = on;
    // Tell the hook whether to copy segment bytes at all.
    window.postMessage({ __vcCtl: 1, tap: on }, "*");
    const waiting = held;
    held = [];
    heldBytes = 0;
    if (on) for (const [header, payload] of waiting) sendSegment(header, payload);
  };
  if (inExtension) {
    try {
      chrome.storage.local.get({ sinkToken: "" }, (v) => setTapping(!!v.sinkToken));
      chrome.storage.onChanged.addListener((changes, area) => {
        if (area === "local" && changes.sinkToken) setTapping(!!changes.sinkToken.newValue);
      });
    } catch (e) {
      setTapping(false);
    }
  }

  const tap = (header, payload) => {
    if (tapping === true) {
      sendSegment(header, payload);
    } else if (tapping === null && heldBytes + payload.byteLength <= MAX_QUEUED_BYTES) {
      held.push([header, payload]);
      heldBytes += payload.byteLength;
    } else if (tapping === null) {
      stats.dropped++;
    }
  };

  // ---------------------------------------------------------------------------
  // Events from hook.js
  // ---------------------------------------------------------------------------

  window.addEventListener(
    "message",
    (event) => {
      if (event.source !== window) return;
      const data = event.data;
      if (!data || data.__vc !== 1 || typeof data.type !== "string") return;

      forward(cleanRecord(data));
      if (data.type === "append") {
        const payload = data.payload;
        if (!(payload instanceof ArrayBuffer) || data.encrypted === true) return;
        const header = segmentHeader(data);
        if (!header) {
          stats.dropped++;
          return;
        }
        tap(header, payload);
        return;
      }
      if (tapping !== true) return;
      if (data.type === "endofstream" || data.type === "changetype") {
        const sb = data.type === "changetype" ? optId(data.sbId) : null;
        sendEvent(optId(data.msId), { name: data.type, sb });
      } else if (data.type === "msclose") {
        closeLink(optId(data.msId));
      }
    },
    false
  );

  // Counters for test injection.
  window.__vcLink = { links, stats };

  forward({ type: "frame", frameUrl: location.href.slice(0, 2048) });
  console.debug(TAG, "ready in", location.href, inExtension ? "(extension)" : "(injected)");
})();
