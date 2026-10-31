"use strict";

const out = document.getElementById("out");

// Every value shown here comes from the page (window.postMessage can be forged by any page script),
// so each one is escaped, numbers included.
const esc = (s) =>
  String(s).replace(/[&<>"']/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" })[c]);

const fmtBytes = (v) => {
  const n = Number(v) || 0;
  return n >= 1 << 20 ? (n / (1 << 20)).toFixed(1) + " MiB" : n >= 1024 ? (n / 1024).toFixed(0) + " KiB" : n + " B";
};

const render = (state) => {
  if (!state) {
    out.innerHTML = '<span class="muted">No MSE activity seen in this tab yet. Play a video, then reopen.</span>';
    return;
  }
  let html = "";
  if (state.drm.length) {
    html += `<div class="drm">DRM (not captured): ${state.drm.map((d) => esc(d.keySystem) + (d.robustness.length ? " (" + esc(d.robustness.join(", ")) + ")" : "")).join("; ")}</div>`;
  }
  if (!state.mediaSources.length) {
    html += '<div class="muted">No MediaSource created yet.</div>';
  }
  for (const m of state.mediaSources) {
    html += `<div class="ms"><h2>MediaSource #${esc(m.msId)}${m.ended ? " · ended" : ""}</h2>`;
    html += `<div class="muted">${esc(m.frameUrl)}${m.blobUrl ? "<br>" + esc(m.blobUrl) : ""}</div>`;
    if (m.sourceBuffers.length) {
      html += "<table><tr><th>#</th><th>mime</th><th>kind</th><th>init</th><th>media</th><th>bytes</th><th>range</th></tr>";
      for (const s of m.sourceBuffers) {
        const range =
          s.firstSec === null ? "–" : `${esc(s.firstSec)}s – ${esc(s.lastEndSec ?? s.lastSec)}s`;
        html += `<tr><td>${esc(s.sbId)}</td><td class="mime">${esc(s.mime)}${s.mimes.length ? '<br><span class="muted">was: ' + esc(s.mimes.join(" → ")) + "</span>" : ""}</td><td>${esc(s.kind ?? "–")}</td><td>${esc(s.inits)}</td><td>${esc(s.media)}</td><td>${fmtBytes(s.bytes)}</td><td>${range}</td></tr>`;
      }
      html += "</table>";
    }
    html += "</div>";
  }
  html += `<div class="muted">${esc(state.events)} events · ${esc(state.frames.length)} frame(s)</div>`;
  out.innerHTML = html;
};

const fmtLink = (s) =>
  !s
    ? "sink: no segments tapped in this tab yet"
    : `sink: ${s.connected ? "connected" : "not connected"} · ${s.sent} msgs / ${fmtBytes(s.sentBytes)} sent · ${s.dropped} dropped`;

const load = async () => {
  const [tab] = await chrome.tabs.query({ active: true, currentWindow: true });
  if (!tab) return render(null);
  chrome.runtime.sendMessage({ kind: "vc-get", tabId: tab.id }, (state) => render(state));
  document.getElementById("clear").onclick = () =>
    chrome.runtime.sendMessage({ kind: "vc-clear", tabId: tab.id }, () => render(null));

  const port = document.getElementById("port");
  const token = document.getElementById("token");
  chrome.storage.local.get({ sinkPort: 17650, sinkToken: "" }, (v) => {
    port.value = v.sinkPort;
    token.value = v.sinkToken;
  });
  // A manual entry taps every tab (the old behaviour) and is not the app's to take back.
  document.getElementById("save").onclick = () =>
    chrome.storage.local.set(
      { sinkPort: Number(port.value) || 17650, sinkToken: token.value, sinkPaired: false, tapMode: "all" },
      () => {
        document.getElementById("link").textContent = "saved; reload the page to reconnect";
      }
    );

  const pairLine = document.getElementById("pair");
  const showPair = (s) => {
    void chrome.runtime.lastError;
    if (!s) return (pairLine.textContent = "app pairing: unavailable");
    const how = !s.connected
      ? s.error === "host-unavailable"
        ? "the connector is not registered, or the app is not running (Connect browser in the app)"
        : "not connected"
      : s.pairing === "approved"
        ? `paired${s.tapped ? `, ${s.tapped} tab(s) captured` : ""}`
        : s.pairing === "pending"
          ? "waiting for your approval in the app"
          : s.pairing === "denied"
            ? "denied in the app"
            : s.pairing === "revoked"
              ? "revoked in the app"
              : s.error === "app-not-running"
                ? "the app is not running"
                : "connecting";
    pairLine.textContent = `app pairing: ${how}`;
  };
  chrome.runtime.sendMessage({ kind: "vc-pair-status" }, showPair);
  document.getElementById("pair-now").onclick = () =>
    chrome.runtime.sendMessage({ kind: "vc-pair-now" }, () => {
      void chrome.runtime.lastError;
      setTimeout(() => chrome.runtime.sendMessage({ kind: "vc-pair-status" }, showPair), 800);
    });

  // The service worker owns the sink sockets; ask it about this tab.
  chrome.runtime.sendMessage({ kind: "vc-link-status", tabId: tab.id }, (s) => {
    void chrome.runtime.lastError;
    document.getElementById("link").textContent = fmtLink(s);
  });
};

load();
