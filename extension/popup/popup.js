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

// The state of the tailnet target as [class, text]: what is, and the next step (never a silent failure).
const tailnetText = (s) => {
  const pc = String(s.host);
  const id = String(s.device).slice(0, 8);
  switch (s.state) {
    case "not-paired":
      return ["", "over the tailnet: not paired. On the PC: streamtap-sink devices pair, then scan its QR code with this phone or type the code below."];
    case "connected":
      return ["released", `over the tailnet: connected to ${pc} (device ${id})`];
    case "connecting":
      return ["", `over the tailnet: connecting to ${pc}…`];
    case "unreachable":
      return ["locked", `over the tailnet: ${pc} does not answer, nothing is sent. Is the PC on, the sink running with --tailnet, tailscale serve on, and this phone in the tailnet?`];
    case "lost":
      return ["locked", `over the tailnet: the connection to ${pc} ended; the next segment tries again`];
    case "revoked":
      return ["locked", `over the tailnet: ${pc} no longer knows this device (revoked there): nothing is sent and its key was deleted here. Pair again.`];
    case "hub-changed":
      return ["locked", `over the tailnet: another sink answers as ${pc} than the one this device was paired with; nothing was sent to it. Pair again if that is your PC.`];
    case "version":
      return ["locked", `over the tailnet: the sink on ${pc} speaks another version. Update the add-on or the sink.`];
    case "refused":
      return ["locked", `over the tailnet: ${pc} refused the login; the next segment tries again`];
    default:
      return ["", `over the tailnet: paired with ${pc} (device ${id}); nothing sent yet`];
  }
};

const PAIR_ERRORS = {
  host: "not a name in the tailnet (like pc.example.ts.net, no port)",
  code: "the code is wrong, was used, or is older than a minute: make a new one on the PC",
  unreachable: "the PC does not answer: is the sink running with --tailnet, tailscale serve on, and this phone in the tailnet?",
  busy: "too many pairing attempts: wait a minute",
  full: "the PC has its 16 devices: revoke one there first",
  known: "this key is already paired there",
  name: "the PC did not take this device name",
  version: "the sink on the PC speaks another version: update the add-on or the sink",
  storage: "this browser could not keep the key (private window?)",
  unsupported: "pairing is not available in this browser",
};

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
  const showTarget = () =>
    chrome.storage.local.get({ sinkTarget: "" }, (v) => {
      const target = v.sinkTarget === "file" || v.sinkTarget === "tailnet" ? v.sinkTarget : "sink";
      document.getElementById(`target-${target}`).checked = true;
      askLink(target);
    });
  showTarget();
  for (const value of ["sink", "file", "tailnet"]) {
    document.getElementById(`target-${value}`).onchange = () =>
      chrome.storage.local.set({ sinkTarget: value === "sink" ? "" : value }, () => {
        document.getElementById("target-saved").textContent = "saved; reload the video page";
        askLink(value);
      });
  }

  // The target "the sink on the PC over the tailnet": its state with the next step, and the pairing.
  const tailnetState = document.getElementById("tailnet-state");
  const tailnetResult = document.getElementById("tailnet-result");
  const tailnetForget = document.getElementById("tailnet-forget");
  const showTailnet = (s) => {
    void chrome.runtime.lastError;
    const usable = Boolean(s && s.available);
    document.getElementById("target-tailnet").disabled = !usable;
    document.getElementById("tailnet-pair").disabled = !usable;
    document.getElementById("tailnet-offer").hidden = !(usable && s.offer);
    tailnetForget.hidden = !(usable && s.paired);
    if (!usable) {
      tailnetState.className = "muted";
      tailnetState.textContent = "over the tailnet: not available in this browser";
      return;
    }
    if (s.offer) document.getElementById("tailnet-offer-text").textContent = `A pairing link of ${s.offer} was opened in this browser.`;
    if (s.paired && !document.getElementById("tailnet-host").value) document.getElementById("tailnet-host").value = s.host;
    const [cls, what] = tailnetText(s);
    tailnetState.className = cls || "muted";
    tailnetState.textContent = what;
  };
  const askTailnet = () => chrome.runtime.sendMessage({ kind: "vc-tailnet-status" }, showTailnet);
  askTailnet();
  const paired = (r) => {
    void chrome.runtime.lastError;
    const ok = Boolean(r && r.ok);
    tailnetResult.className = ok ? "released" : "locked";
    tailnetResult.textContent = ok
      ? `paired with ${r.host}; the target is now the sink over the tailnet (reload the video page)`
      : PAIR_ERRORS[r && r.reason] || "pairing failed";
    if (ok) document.getElementById("tailnet-code").value = "";
    askTailnet();
    showTarget();
  };
  const pairWith = (msg) => {
    tailnetResult.className = "muted";
    tailnetResult.textContent = "pairing…";
    chrome.runtime.sendMessage({ kind: "vc-tailnet-pair", name: document.getElementById("tailnet-name").value, ...msg }, paired);
  };
  document.getElementById("tailnet-offer-pair").onclick = () => pairWith({ offer: true });
  document.getElementById("tailnet-pair").onclick = () => {
    // The code alone, or the whole pairing link (it names the PC too).
    const typed = String(document.getElementById("tailnet-code").value).trim();
    const link = /^https:\/\/([^/#?]+)\/?#streamtap-pair=1&code=([0-9a-f]{32})$/.exec(typed);
    if (link) document.getElementById("tailnet-host").value = link[1];
    pairWith({ host: document.getElementById("tailnet-host").value, code: link ? link[2] : typed.toLowerCase() });
  };
  // Forgetting needs a second tap: captures to the PC stop with it.
  let forgetArmed = false;
  tailnetForget.onclick = () => {
    if (!forgetArmed) {
      forgetArmed = true;
      tailnetForget.textContent = "tap again to forget (then revoke it on the PC too)";
      return;
    }
    forgetArmed = false;
    tailnetForget.textContent = "forget this pairing";
    chrome.runtime.sendMessage({ kind: "vc-tailnet-forget" }, (ok) => {
      void chrome.runtime.lastError;
      tailnetResult.className = ok ? "muted" : "locked";
      tailnetResult.textContent = ok ? "forgotten on this device; on the PC: streamtap-sink devices revoke <id>" : "could not forget the pairing";
      askTailnet();
    });
  };
  fileSave.onclick = () =>
    chrome.runtime.sendMessage({ kind: "vc-file-save", tabId: tab.id }, () => {
      void chrome.runtime.lastError;
      setTimeout(() => askLink("file"), 500);
    });
};

load();
