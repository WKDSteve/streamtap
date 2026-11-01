"use strict";

const out = document.getElementById("out");

// Every value shown here comes from the page (window.postMessage can be forged by any page script),
// so the view is built from elements and textContent; no value is ever parsed as markup.
const el = (tag, className, ...children) => {
  const node = document.createElement(tag);
  if (className) node.className = className;
  for (const c of children) node.append(c);
  return node;
};
const text = (tag, className, value) => {
  const node = el(tag, className);
  node.textContent = String(value);
  return node;
};

const fmtBytes = (v) => {
  const n = Number(v) || 0;
  return n >= 1 << 20 ? (n / (1 << 20)).toFixed(1) + " MiB" : n >= 1024 ? (n / 1024).toFixed(0) + " KiB" : n + " B";
};

const bufferRow = (s) => {
  const range = s.firstSec === null ? "–" : `${s.firstSec}s – ${s.lastEndSec ?? s.lastSec}s`;
  const mime = text("td", "mime", s.mime);
  if (s.mimes.length) mime.append(el("br"), text("span", "muted", "was: " + s.mimes.join(" → ")));
  return el(
    "tr",
    "",
    text("td", "", s.sbId),
    mime,
    text("td", "", s.kind ?? "–"),
    text("td", "", s.inits),
    text("td", "", s.media),
    text("td", "", fmtBytes(s.bytes)),
    text("td", "", range)
  );
};

const mediaSourceBox = (m) => {
  const box = el("div", "ms", text("h2", "", `MediaSource #${m.msId}${m.ended ? " · ended" : ""}`));
  const where = text("div", "muted", m.frameUrl);
  if (m.blobUrl) where.append(el("br"), document.createTextNode(String(m.blobUrl)));
  box.append(where);
  if (m.sourceBuffers.length) {
    const head = el("tr", "");
    for (const h of ["#", "mime", "kind", "init", "media", "bytes", "range"]) head.append(text("th", "", h));
    box.append(el("table", "", head, ...m.sourceBuffers.map(bufferRow)));
  }
  return box;
};

const render = (state) => {
  if (!state) {
    out.replaceChildren(text("span", "muted", "No MSE activity seen in this tab yet. Play a video, then reopen."));
    return;
  }
  const parts = [];
  if (state.drm.length) {
    const keys = state.drm.map((d) => d.keySystem + (d.robustness.length ? " (" + d.robustness.join(", ") + ")" : ""));
    parts.push(text("div", "drm", `DRM (not captured): ${keys.join("; ")}`));
  }
  if (!state.mediaSources.length) {
    parts.push(text("div", "muted", "No MediaSource created yet."));
  }
  for (const m of state.mediaSources) parts.push(mediaSourceBox(m));
  parts.push(text("div", "muted", `${state.events} events · ${state.frames.length} frame(s)`));
  out.replaceChildren(...parts);
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
        ? `the connector is not registered, or the app is not running (Connect browser in the app)${s.detail ? ` - browser says: ${s.detail}` : ""}`
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
