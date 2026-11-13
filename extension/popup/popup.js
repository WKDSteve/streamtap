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

const GROUP_LABELS = {
  own: "own server",
  free: "free licence",
  ard: "ARD",
  zdf: "ZDF / 3sat / KiKA",
  learning: "learning platform",
  obfuscated: "obfuscated service",
  social: "social media",
};
const STATE_TEXT = {
  off: ["", "source policy off: every site without DRM is captured"],
  released: ["released", "released: captured"],
  unlocked: ["", "unlocked by you: captured"],
  locked: ["locked", "locked: nothing is copied (unlock below)"],
  unknown: ["", "not in the list: captured, check the site's terms"],
};

// The source line and the settings form; every value is set as text or as a
// form value, never parsed as markup.
const showSources = (o) => {
  void chrome.runtime.lastError;
  const line = document.getElementById("source");
  if (!o) {
    line.textContent = "source: unavailable";
    return;
  }
  const v = o.verdict;
  if (!v) {
    line.textContent = "source: no page seen in this tab yet";
  } else {
    const [cls, what] = STATE_TEXT[v.state] || ["", String(v.state)];
    const groups = (Array.isArray(v.groups) ? v.groups : []).map((g) => GROUP_LABELS[g] || String(g));
    line.className = cls || "muted";
    line.textContent = `source: ${groups.length ? groups.join(", ") + " · " : ""}${what}`;
  }
  document.getElementById("policy-on").checked = o.on === true;
  const unlocked = new Set(Array.isArray(o.unlocked) ? o.unlocked : []);
  document.getElementById("unlock-obfuscated").checked = unlocked.has("obfuscated");
  document.getElementById("unlock-social").checked = unlocked.has("social");
  document.getElementById("own-hosts").value = (Array.isArray(o.own) ? o.own : []).join("\n");
  document.getElementById("learning-hosts").value = (Array.isArray(o.learning) ? o.learning : []).join("\n");
};

// One host per line; the service worker cleans and bounds them again.
const hostLines = (v) =>
  String(v)
    .split(/[\s,]+/)
    .map((h) => h.trim())
    .filter(Boolean)
    .slice(0, 200);

const fmtLink = (s) =>
  !s || !s.links
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
    if (s.error === "unsupported") {
      // Firefox (for Android): no native messaging, no desktop app to pair with.
      document.getElementById("pair-now").hidden = true;
      return (pairLine.textContent = "app pairing: not available in this browser");
    }
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

  const askSources = () => chrome.runtime.sendMessage({ kind: "vc-source-of", tabId: tab.id }, showSources);
  askSources();
  document.getElementById("policy-save").onclick = () => {
    const unlocked = [];
    if (document.getElementById("unlock-obfuscated").checked) unlocked.push("obfuscated");
    if (document.getElementById("unlock-social").checked) unlocked.push("social");
    chrome.storage.local.set(
      {
        sourcePolicy: document.getElementById("policy-on").checked ? "on" : "off",
        sourceUnlocked: unlocked,
        sourceOwnHosts: hostLines(document.getElementById("own-hosts").value),
        sourceLearningHosts: hostLines(document.getElementById("learning-hosts").value),
      },
      () => {
        document.getElementById("policy-saved").textContent = "saved; open tabs follow at once";
        askSources();
      }
    );
  };

  // The target: the sink on the PC, or a recording saved on this device.
  const fileState = document.getElementById("file-state");
  const fileSave = document.getElementById("file-save");
  const showFile = (f, target) => {
    const lines = [];
    if (!f || !f.available) {
      document.getElementById("target-file").disabled = true;
      lines.push("a file on this device: not available in this browser");
    } else if (target === "file") {
      lines.push(
        f.recording
          ? `recording: ${f.media} media segments, ${fmtBytes(f.bytes)}${f.full ? " · memory full: newer media is not kept (said in the file)" : ""}`
          : "recording: nothing in this tab yet (play the video; reload it after switching the target)"
      );
      for (const s of f.saved || []) {
        lines.push(s.error ? `not saved (${s.error})${s.name ? `: ${s.name}` : ""}` : `saved: ${s.name}, ${fmtBytes(s.bytes)}${s.full ? " (memory was full)" : ""}`);
      }
    }
    fileState.textContent = lines.join("\n");
    fileSave.hidden = !(f && f.available && target === "file" && f.recording > 0);
  };
  const askLink = (target) =>
    // The background owns the sink sockets and the recordings; ask it about this tab.
    chrome.runtime.sendMessage({ kind: "vc-link-status", tabId: tab.id }, (s) => {
      void chrome.runtime.lastError;
      document.getElementById("link").textContent = fmtLink(s);
      showFile(s && s.file, target);
    });
  chrome.storage.local.get({ sinkTarget: "" }, (v) => {
    const target = v.sinkTarget === "file" ? "file" : "sink";
    document.getElementById(`target-${target}`).checked = true;
    askLink(target);
  });
  for (const value of ["sink", "file"]) {
    document.getElementById(`target-${value}`).onchange = () =>
      chrome.storage.local.set({ sinkTarget: value === "file" ? "file" : "" }, () => {
        document.getElementById("target-saved").textContent = "saved; reload the video page";
        askLink(value);
      });
  }
  fileSave.onclick = () =>
    chrome.runtime.sendMessage({ kind: "vc-file-save", tabId: tab.id }, () => {
      void chrome.runtime.lastError;
      setTimeout(() => askLink("file"), 500);
    });
};

load();
