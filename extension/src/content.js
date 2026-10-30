// Isolated-world content script. Receives the records hook.js posts on the
// window and forwards them to the service worker, which keeps a per-tab
// registry for the popup. Spike 1: metadata only, no media bytes.
(() => {
  "use strict";

  const TAG = "[vc-content]";

  const forward = (msg) => {
    try {
      chrome.runtime.sendMessage(msg, () => {
        // Swallow "receiving end does not exist" while the worker restarts.
        void chrome.runtime.lastError;
      });
    } catch (e) {
      // Extension context invalidated (reload during development): stay quiet.
    }
  };

  window.addEventListener(
    "message",
    (event) => {
      if (event.source !== window) return;
      const data = event.data;
      if (!data || data.__vc !== 1) return;
      forward({ kind: "vc-event", event: data });
    },
    false
  );

  forward({ kind: "vc-frame", frameUrl: location.href, top: window === window.top });
  console.debug(TAG, "ready in", location.href);
})();
