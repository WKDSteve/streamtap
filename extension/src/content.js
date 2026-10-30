// Isolated-world content script.
//
// Spike 1: forwards the hook's metadata records to the service worker (badge,
// popup). Spike 2: additionally streams every segment payload as a binary
// WebSocket message to the local sink (streamtap-sink serve).
//
// Wire format per message: u32 LE header length | JSON header | payload.
// The first message is {"type":"hello","token":...}; wrong token = the sink
// closes the socket.
//
// The script also runs outside an extension (injected for tests): then there
// is no chrome.* and the sink settings come from window.__vcSinkConfig.
(() => {
  "use strict";

  const TAG = "[vc-content]";
  const inExtension = typeof chrome !== "undefined" && !!(chrome.runtime && chrome.runtime.id);

  // ---------------------------------------------------------------------------
  // Service-worker relay (metadata only)
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
  // Sink link (binary)
  // ---------------------------------------------------------------------------

  const MAX_PENDING_BYTES = 64 * 1024 * 1024; // while CONNECTING only
  const session = (crypto.randomUUID && crypto.randomUUID()) || String(Math.random()).slice(2);
  const enc = new TextEncoder();

  const link = {
    ws: null,
    settings: null, // { port, token }
    pending: [], // Blobs queued while the socket is connecting
    pendingBytes: 0,
    dropped: 0,
    sent: 0,
    sentBytes: 0,
    nextRetryAt: 0,
    retryDelay: 1000,
  };

  const loadSettings = () =>
    new Promise((resolve) => {
      if (!inExtension) return resolve(window.__vcSinkConfig || null);
      try {
        chrome.storage.local.get({ sinkPort: 17650, sinkToken: "" }, (v) =>
          resolve(v.sinkToken ? { port: v.sinkPort, token: v.sinkToken } : null)
        );
      } catch (e) {
        resolve(null);
      }
    });

  const frame = (header, payload) => {
    const h = enc.encode(JSON.stringify(header));
    const len = new Uint8Array(4);
    new DataView(len.buffer).setUint32(0, h.byteLength, true);
    return payload ? new Blob([len, h, payload]) : new Blob([len, h]);
  };

  const flushPending = () => {
    for (const b of link.pending) link.ws.send(b);
    link.sent += link.pending.length;
    link.pending = [];
    link.pendingBytes = 0;
  };

  const connect = async () => {
    if (link.ws || Date.now() < link.nextRetryAt) return;
    link.settings = link.settings || (await loadSettings());
    if (!link.settings) return; // not configured: metadata-only mode
    const { port, token } = link.settings;
    let ws;
    try {
      ws = new WebSocket(`ws://127.0.0.1:${port}/`);
    } catch (e) {
      console.warn(TAG, "sink connect refused by browser:", e && e.message);
      link.nextRetryAt = Date.now() + link.retryDelay;
      return;
    }
    ws.binaryType = "arraybuffer";
    link.ws = ws;
    ws.onopen = () => {
      link.retryDelay = 1000;
      ws.send(frame({ type: "hello", token, session, url: location.href, title: document.title }));
      flushPending();
      console.debug(TAG, "sink connected, session", session);
    };
    ws.onclose = () => {
      link.ws = null;
      link.pending = [];
      link.pendingBytes = 0;
      link.nextRetryAt = Date.now() + link.retryDelay;
      link.retryDelay = Math.min(link.retryDelay * 2, 10000);
      console.debug(TAG, "sink closed");
    };
    ws.onerror = () => {
      // onclose follows; nothing to do here.
    };
  };

  const send = (header, payload) => {
    const b = frame(header, payload);
    if (link.ws && link.ws.readyState === WebSocket.OPEN) {
      link.ws.send(b);
      link.sent++;
      link.sentBytes += b.size;
      return;
    }
    if (link.ws && link.ws.readyState === WebSocket.CONNECTING && link.pendingBytes + b.size <= MAX_PENDING_BYTES) {
      link.pending.push(b);
      link.pendingBytes += b.size;
      return;
    }
    // Not connected: never buffer unboundedly in the page. Count and drop.
    link.dropped++;
    connect();
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
        if (payload) {
          send(
            {
              type: "segment",
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
        send({ type: "event", name: data.type, sb: data.sbId ?? null });
      }
    },
    false
  );

  // Expose counters for the popup / for test injection.
  window.__vcLink = link;
  if (inExtension) {
    chrome.runtime.onMessage.addListener((msg, _sender, sendResponse) => {
      if (msg && msg.kind === "vc-link-status") {
        sendResponse({ connected: !!(link.ws && link.ws.readyState === WebSocket.OPEN), sent: link.sent, sentBytes: link.sentBytes, dropped: link.dropped, session });
      }
      return false;
    });
  }

  forward({ kind: "vc-frame", frameUrl: location.href, top: window === window.top });
  connect();
  console.debug(TAG, "ready in", location.href, inExtension ? "(extension)" : "(injected)");
})();
