// Isolated-world content script.
//
// Forwards the hook's metadata records to the service worker (badge, popup)
// and hands every segment payload to the service worker over a runtime port
// ("vc-sink"). The service worker owns the WebSocket to the local sink
// (src/sink-link.js), so the page's CSP does not apply to it and the sink can
// check the extension's origin exactly.
//
// Port messages are JSON, so payloads travel as data: URLs (FileReader encodes
// natively). A promise chain keeps them in append order.
//
// The hook's records arrive via window.postMessage, which any page script can
// forge: every field that reaches the sink is type-checked here.
//
// Injected outside an extension (tests) there is no chrome.*: the script then
// only counts what it would have sent.
(() => {
  "use strict";

  const TAG = "[vc-content]";
  const inExtension = typeof chrome !== "undefined" && !!(chrome.runtime && chrome.runtime.id);

  // ---------------------------------------------------------------------------
  // Metadata relay
  // ---------------------------------------------------------------------------

  const forward = (msg) => {
    if (!inExtension) return;
    try {
      chrome.runtime.sendMessage(msg, () => {
        void chrome.runtime.lastError;
      });
    } catch (e) {
      // Extension context invalidated (reload during development): stay quiet.
    }
  };

  // ---------------------------------------------------------------------------
  // Tapping on/off: only with a configured sink (token set)
  // ---------------------------------------------------------------------------

  let tapping = false;
  const setTapping = (on) => {
    tapping = on;
    // Tell the hook whether to copy segment bytes at all.
    window.postMessage({ __vcCtl: 1, tap: on }, "*");
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
  } else {
    setTapping(false);
  }

  // ---------------------------------------------------------------------------
  // Validation of the forgeable records
  // ---------------------------------------------------------------------------

  const isId = (v) => Number.isInteger(v) && v > 0 && v <= 0xffffffff;
  const isU64 = (v) => Number.isSafeInteger(v) && v >= 0;
  const str = (v, max) => (typeof v === "string" ? v.slice(0, max) : "");

  // The segment header for the sink, or null when the record is malformed.
  const segmentHeader = (m) => {
    if (!isId(m.sbId) || !isU64(m.seq)) return null;
    if (m.msId !== null && m.msId !== undefined && !isId(m.msId)) return null;
    if (m.tfdt !== null && m.tfdt !== undefined && !isU64(m.tfdt)) return null;
    if (m.timescale !== null && m.timescale !== undefined && !isId(m.timescale)) return null;
    if (m.tso !== null && m.tso !== undefined && !Number.isFinite(m.tso)) return null;
    return {
      sb: m.sbId,
      seq: m.seq,
      ms: m.msId ?? null,
      mime: str(m.mime, 256),
      isInit: m.isInit === true,
      kind: str(m.kind, 16),
      tfdt: m.tfdt ?? null,
      timescale: m.timescale ?? null,
      tso: m.tso ?? null,
    };
  };

  // ---------------------------------------------------------------------------
  // Segment relay to the service worker
  // ---------------------------------------------------------------------------

  const MAX_QUEUED_BYTES = 64 * 1024 * 1024; // payloads waiting to be encoded
  const MAX_INIT_BYTES = 2 * 1024 * 1024; // init segments kept for a new port
  const MAX_KEPT_INITS = 32;
  const session = (crypto.randomUUID && crypto.randomUUID()) || String(Math.random()).slice(2);

  const link = {
    port: null,
    dead: false, // the extension was reloaded: stop trying
    ports: 0, // ports opened so far
    inits: new Map(), // sb -> { header, payload }: replayed on a new port
    queuedBytes: 0,
    chain: Promise.resolve(),
    posted: 0,
    dropped: 0,
  };

  const toDataUrl = (buffer) =>
    new Promise((resolve, reject) => {
      const r = new FileReader();
      r.onload = () => resolve(r.result);
      r.onerror = () => reject(r.error);
      r.readAsDataURL(new Blob([buffer]));
    });

  // Opens the port when there is none. A port after the first one (the
  // service worker restarted) gets the kept init segments again, because the
  // sink starts a new capture directory and the player appends inits once.
  const ensurePort = async () => {
    if (link.port || link.dead || !inExtension) return link.port;
    let port;
    try {
      port = chrome.runtime.connect({ name: "vc-sink" });
    } catch (e) {
      link.dead = true;
      return null;
    }
    link.port = port;
    link.ports++;
    port.onDisconnect.addListener(() => {
      void chrome.runtime.lastError;
      if (link.port === port) link.port = null;
      // A restarted service worker: reconnect on the next segment. A reloaded
      // extension: give up.
      link.dead = !(chrome.runtime && chrome.runtime.id);
    });
    port.postMessage({ t: "hello", session, title: document.title });
    if (link.ports > 1) {
      for (const { header, payload } of link.inits.values()) {
        port.postMessage({ t: "segment", header, payload: await toDataUrl(payload) });
      }
    }
    return port;
  };

  const post = async (msg) => {
    const port = await ensurePort();
    if (!port) {
      link.dropped++;
      return;
    }
    try {
      port.postMessage(msg);
      link.posted++;
    } catch (e) {
      link.dropped++;
    }
  };

  const keepInit = (header, payload) => {
    if (!header.isInit || payload.byteLength > MAX_INIT_BYTES) return;
    link.inits.delete(header.sb);
    link.inits.set(header.sb, { header, payload });
    while (link.inits.size > MAX_KEPT_INITS) link.inits.delete(link.inits.keys().next().value);
  };

  const sendSegment = (header, payload) => {
    const size = payload.byteLength;
    if (!inExtension || link.dead || link.queuedBytes + size > MAX_QUEUED_BYTES) {
      link.dropped++;
      return;
    }
    keepInit(header, payload);
    link.queuedBytes += size;
    link.chain = link.chain
      .then(() => toDataUrl(payload))
      .then((dataUrl) => post({ t: "segment", header, payload: dataUrl }))
      .catch(() => {
        link.dropped++;
      })
      .finally(() => {
        link.queuedBytes -= size;
      });
  };

  const sendEvent = (header) => {
    if (!inExtension || link.dead || !tapping) return;
    link.chain = link.chain.then(() => post({ t: "event", header })).catch(() => {});
  };

  // ---------------------------------------------------------------------------
  // Events from hook.js
  // ---------------------------------------------------------------------------

  window.addEventListener(
    "message",
    (event) => {
      if (event.source !== window) return;
      const data = event.data;
      if (!data || data.__vc !== 1) return;

      if (data.type === "append") {
        const { payload, ...meta } = data;
        forward({ kind: "vc-event", event: meta });
        if (!tapping || !(payload instanceof ArrayBuffer) || meta.encrypted === true) return;
        const header = segmentHeader(meta);
        if (!header) {
          link.dropped++;
          return;
        }
        sendSegment(header, payload);
        return;
      }

      forward({ kind: "vc-event", event: data });
      if ((data.type === "endofstream" || data.type === "changetype") && (data.sbId == null || isId(data.sbId))) {
        sendEvent({ name: data.type, sb: data.sbId ?? null });
      }
    },
    false
  );

  // Counters for test injection.
  window.__vcLink = link;

  forward({ kind: "vc-frame", frameUrl: location.href, top: window === window.top });
  console.debug(TAG, "ready in", location.href, inExtension ? "(extension)" : "(injected)");
})();
