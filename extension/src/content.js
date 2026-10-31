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
  // Segment relay to the service worker
  // ---------------------------------------------------------------------------

  const MAX_QUEUED_BYTES = 64 * 1024 * 1024; // payloads waiting to be encoded
  const session = (crypto.randomUUID && crypto.randomUUID()) || String(Math.random()).slice(2);

  const link = {
    port: null,
    dead: false, // the extension was reloaded: stop trying
    queuedBytes: 0,
    chain: Promise.resolve(),
    posted: 0,
    dropped: 0,
  };

  const openPort = () => {
    if (link.port || link.dead || !inExtension) return link.port;
    try {
      link.port = chrome.runtime.connect({ name: "vc-sink" });
    } catch (e) {
      link.dead = true;
      return null;
    }
    link.port.onDisconnect.addListener(() => {
      void chrome.runtime.lastError;
      link.port = null;
      // A restarted service worker: reconnect on the next segment (the sink
      // starts a new capture directory). A reloaded extension: give up.
      link.dead = !(chrome.runtime && chrome.runtime.id);
    });
    link.port.postMessage({ t: "hello", session, title: document.title });
    return link.port;
  };

  const toDataUrl = (buffer) =>
    new Promise((resolve, reject) => {
      const r = new FileReader();
      r.onload = () => resolve(r.result);
      r.onerror = () => reject(r.error);
      r.readAsDataURL(new Blob([buffer]));
    });

  const post = (msg) => {
    const port = openPort();
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

  const sendSegment = (header, payload) => {
    const size = payload.byteLength;
    if (!inExtension || link.dead || link.queuedBytes + size > MAX_QUEUED_BYTES) {
      link.dropped++;
      return;
    }
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
    if (!inExtension || link.dead) return;
    link.chain = link.chain.then(() => post({ t: "event", header }));
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
        if (payload instanceof ArrayBuffer) {
          sendSegment(
            {
              sb: meta.sbId,
              seq: meta.seq,
              mime: meta.mime,
              isInit: meta.isInit,
              kind: meta.kind,
              tfdt: meta.tfdt,
              timescale: meta.timescale,
            },
            payload
          );
        }
        return;
      }

      forward({ kind: "vc-event", event: data });
      if (data.type === "endofstream" || data.type === "changetype") {
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
