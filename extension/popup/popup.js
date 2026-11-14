"use strict";

// The popup: the start page says what is (this tab, its source, the target) and holds the one action that matters
// while capturing; the settings live on their own pages (popup.html). Every text comes from i18n.js.

const t = I18N.t;
const $ = (id) => document.getElementById(id);
const out = $("out");

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
// One line of state: its words and how it reads ("" is the quiet grey).
const say = (node, cls, words) => {
  node.className = cls || "muted";
  node.textContent = words;
};

const fmtBytes = (v) => {
  const n = Number(v) || 0;
  return n >= 1 << 20 ? (n / (1 << 20)).toFixed(1) + " MiB" : n >= 1024 ? (n / 1024).toFixed(0) + " KiB" : n + " B";
};

// --- pages -------------------------------------------------------------------------------------------------------
// location.hash names the page, so the browser's own back key (the only one on a phone) leaves a settings page.
const PAGES = ["home", "target", "sources", "about"];
let page = "home";
// How many pages this popup went forward: "back" then is the browser's back; a page opened directly goes home.
let deep = 0;
const pageOf = (hash) => {
  const want = String(hash || "").replace(/^#/, "");
  return PAGES.includes(want) && want !== "home" ? want : "home";
};
const showPage = (moveFocus) => {
  const from = page;
  page = pageOf(location.hash);
  for (const p of PAGES) $(`page-${p}`).hidden = p !== page;
  if (!moveFocus || from === page) return;
  // Focus follows the page: on a settings page its way back, at home the entry it was left through.
  const target = page === "home" ? $(`nav-${from}`) : $(`back-${page}`);
  if (target && typeof target.focus === "function") target.focus();
};
const go = (name) => {
  deep++;
  location.hash = `#${name}`;
};
const back = () => {
  if (deep > 0) {
    deep--;
    history.back();
  } else {
    location.hash = "";
  }
};

// --- this tab ----------------------------------------------------------------------------------------------------
const bufferRow = (s) => {
  const range = s.firstSec === null ? "–" : `${s.firstSec}s – ${s.lastEndSec ?? s.lastSec}s`;
  const mime = text("td", "mime", s.mime);
  if (s.mimes.length) mime.append(el("br"), text("span", "muted", t("wasMime", { mimes: s.mimes.join(" → ") })));
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
  const title = t(m.ended ? "msEnded" : "msTitle", { id: m.msId });
  const box = el("div", "ms", text("h2", "", title));
  // The table scrolls sideways inside the box on a narrow page: the keyboard reaches it too.
  box.tabIndex = 0;
  box.setAttribute("role", "group");
  box.setAttribute("aria-label", title);
  const where = text("div", "muted", m.frameUrl);
  if (m.blobUrl) where.append(el("br"), document.createTextNode(String(m.blobUrl)));
  box.append(where);
  if (m.sourceBuffers.length) {
    const head = el("tr", "");
    for (const h of ["#", t("thMime"), t("thKind"), t("thInit"), t("thMedia"), t("thBytes"), t("thRange")]) head.append(text("th", "", h));
    box.append(el("table", "", head, ...m.sourceBuffers.map(bufferRow)));
  }
  return box;
};

// The start page gets one sentence about the tab, the details below it get everything.
const render = (state) => {
  const line = $("tab-line");
  if (!state) {
    say(line, "", t("tabNone"));
    out.replaceChildren(text("span", "muted", t("tabNone")));
    return;
  }
  const buffers = state.mediaSources.flatMap((m) => m.sourceBuffers);
  if (state.drm.length) say(line, "locked", t("tabDrm"));
  else if (!buffers.length) say(line, "", t("tabNoSource"));
  else say(line, "", t("tabSummary", { buffers: buffers.length, bytes: fmtBytes(buffers.reduce((sum, s) => sum + (Number(s.bytes) || 0), 0)) }));

  const parts = [];
  if (state.drm.length) {
    const keys = state.drm.map((d) => d.keySystem + (d.robustness.length ? " (" + d.robustness.join(", ") + ")" : ""));
    parts.push(text("div", "drm", t("drmLine", { keys: keys.join("; ") })));
  }
  if (!state.mediaSources.length) {
    parts.push(text("div", "muted", t("noMediaSource")));
  }
  for (const m of state.mediaSources) parts.push(mediaSourceBox(m));
  parts.push(text("div", "muted", t("eventsLine", { events: state.events, frames: state.frames.length })));
  out.replaceChildren(...parts);
};

// --- sources -----------------------------------------------------------------------------------------------------
const GROUP_KEYS = {
  own: "groupOwn",
  free: "groupFree",
  ard: "groupArd",
  zdf: "groupZdf",
  learning: "groupLearning",
  obfuscated: "groupObfuscated",
  social: "groupSocial",
};
// state -> [class, key]
const STATE_KEYS = {
  off: ["", "stateOff"],
  released: ["released", "stateReleased"],
  unlocked: ["", "stateUnlocked"],
  locked: ["locked", "stateLocked"],
  unknown: ["", "stateUnknown"],
};

// The source line and the settings form; every value is set as text or as a
// form value, never parsed as markup.
const showSources = (o) => {
  void chrome.runtime.lastError;
  const line = $("source");
  if (!o) {
    say(line, "", t("sourceUnavailable"));
    return;
  }
  const v = o.verdict;
  if (!v) {
    say(line, "", t("sourceNoPage"));
  } else {
    const known = Object.hasOwn(STATE_KEYS, v.state) ? STATE_KEYS[v.state] : null;
    const what = known ? t(known[1]) : String(v.state);
    const groups = (Array.isArray(v.groups) ? v.groups : []).map((g) => (Object.hasOwn(GROUP_KEYS, g) ? t(GROUP_KEYS[g]) : String(g)));
    say(line, known ? known[0] : "", groups.length ? t("sourceLineGroups", { groups: groups.join(", "), what }) : t("sourceLine", { what }));
  }
  $("policy-on").checked = o.on === true;
  const unlocked = new Set(Array.isArray(o.unlocked) ? o.unlocked : []);
  $("unlock-obfuscated").checked = unlocked.has("obfuscated");
  $("unlock-social").checked = unlocked.has("social");
  $("own-hosts").value = (Array.isArray(o.own) ? o.own : []).join("\n");
  $("learning-hosts").value = (Array.isArray(o.learning) ? o.learning : []).join("\n");
};

// One host per line; the service worker cleans and bounds them again.
const hostLines = (v) =>
  String(v)
    .split(/[\s,]+/)
    .map((h) => h.trim())
    .filter(Boolean)
    .slice(0, 200);

// --- the targets -------------------------------------------------------------------------------------------------
// The state of the tailnet target as [class, text]: what is, and the next step (never a silent failure).
const TAILNET_STATES = {
  "not-paired": ["", "tnNotPaired"],
  connected: ["released", "tnConnected"],
  connecting: ["", "tnConnecting"],
  unreachable: ["locked", "tnUnreachable"],
  lost: ["locked", "tnLost"],
  revoked: ["locked", "tnRevoked"],
  "hub-changed": ["locked", "tnHubChanged"],
  version: ["locked", "tnVersion"],
  refused: ["locked", "tnRefused"],
};
const tailnetText = (s) => {
  const [cls, key] = Object.hasOwn(TAILNET_STATES, s.state) ? TAILNET_STATES[s.state] : ["", "tnIdle"];
  return [cls, t(key, { pc: String(s.host), id: String(s.device).slice(0, 8) })];
};

const PAIR_ERROR_KEYS = {
  host: "pairErrHost",
  code: "pairErrCode",
  unreachable: "pairErrUnreachable",
  busy: "pairErrBusy",
  full: "pairErrFull",
  known: "pairErrKnown",
  name: "pairErrName",
  version: "pairErrVersion",
  storage: "pairErrStorage",
  unsupported: "pairErrUnsupported",
};

// Why a wanted video became a recording: the muxer's reason in this language, or its own words for one not known here.
const MUX_KEYS = {
  webm: "muxWebm",
  switch: "muxSwitch",
  seqmode: "muxSeqmode",
  tso: "muxTso",
  drm: "muxDrm",
  initlost: "muxInitlost",
  absolute: "muxAbsolute",
  tracks: "muxTracks",
  structure: "muxStructure",
  empty: "muxEmpty",
  failed: "muxFailed",
};
const SAVE_ERROR_KEYS = { blob: "errorBlob", download: "errorDownload" };

// One saved file in words: a video with what it holds (gaps, repeats dropped), or a recording, also why a wanted
// video became one. Every value comes from the background and is put in as text.
const savedLine = (s) => {
  const r = s.report || {};
  const notes = [];
  if (s.full) notes.push(t("noteFull"));
  if (r.gaps) notes.push(t("noteGaps", { gaps: r.gaps, seconds: Number(r.gapSeconds || 0).toFixed(1) }));
  if (r.dupes) notes.push(t("noteDupes", { n: r.dupes }));
  if (r.overlaps) notes.push(t("noteOverlaps", { n: r.overlaps }));
  if (r.cut) notes.push(t("noteCut", { n: r.cut }));
  const key = (s.video ? "savedVideo" : "savedRecording") + (notes.length ? "Notes" : "");
  const line = t(key, { name: s.name, bytes: fmtBytes(s.bytes), notes: notes.join("; ") });
  if (!s.fallback && !s.reason) return line;
  const why = Object.hasOwn(MUX_KEYS, s.reason) ? t(MUX_KEYS[s.reason]) : String(s.fallback || s.reason);
  return t("savedFallback", { line, why });
};
const notSavedLine = (s) => {
  const error = Object.hasOwn(SAVE_ERROR_KEYS, s.error) ? t(SAVE_ERROR_KEYS[s.error]) : String(s.error);
  return s.name ? t("notSavedName", { error, name: s.name }) : t("notSaved", { error });
};

const linkState = (s) =>
  !s || !s.links
    ? t("linkNone")
    : t("linkState", { connected: t(s.connected ? "linkConnected" : "linkNotConnected"), sent: s.sent, bytes: fmtBytes(s.sentBytes), dropped: s.dropped });

// What a recording on this device is doing, first line first.
const fileLines = (f) => {
  if (!f || !f.available) return [t("fileUnavailable")];
  const lines = [
    f.recording ? t(f.full ? "fileRecordingFull" : "fileRecording", { media: f.media, bytes: fmtBytes(f.bytes) }) : t("fileNothing"),
  ];
  for (const s of f.saved || []) lines.push(s.error ? notSavedLine(s) : savedLine(s));
  return lines;
};

const pairHow = (s) => {
  if (!s.connected) {
    if (s.error !== "host-unavailable") return t("pairNotConnected");
    return s.detail ? t("pairHostUnavailableDetail", { detail: s.detail }) : t("pairHostUnavailable");
  }
  if (s.pairing === "approved") return s.tapped ? t("pairApprovedTabs", { tabs: s.tapped }) : t("pairApproved");
  if (s.pairing === "pending") return t("pairPending");
  if (s.pairing === "denied") return t("pairDenied");
  if (s.pairing === "revoked") return t("pairRevoked");
  return t(s.error === "app-not-running" ? "pairAppNotRunning" : "pairConnecting");
};

// What the background last said; the start page's target line and the blocks of the target page are drawn from it.
const view = { target: "sink", link: null, file: null, tailnet: null, pairingOpen: false, asked: new Set() };
const onDevice = (target) => target === "file" || target === "video";

const showTargetLine = () => {
  const line = $("target-line");
  if (view.target === "tailnet") {
    const s = view.tailnet;
    const [cls, words] = s && s.available ? tailnetText(s) : ["", t("tnUnavailable")];
    return say(line, cls, words);
  }
  if (view.target === "sink") return say(line, "", t("homeSink", { state: linkState(view.link) }));
  const f = view.file;
  if (!f || !f.available) return say(line, "", t("fileUnavailable"));
  say(line, "", t(view.target === "video" ? "homeVideo" : "homeFile", { state: fileLines(f)[0] }));
};

// Only what belongs to the chosen target is on the page; a pairing that was started stays visible until it is done.
const showBlocks = () => {
  $("block-sink").hidden = view.target !== "sink";
  $("block-tailnet").hidden = !(view.target === "tailnet" || view.pairingOpen);
  $("block-device").hidden = !onDevice(view.target);
};

// A target this browser cannot offer is greyed out; the line under the choices says which and that it is the browser.
const showTargetNote = () => {
  const notes = [];
  if (view.asked.has("file") && !(view.file && view.file.available)) notes.push(t("fileUnavailable"));
  if (view.asked.has("tailnet") && !(view.tailnet && view.tailnet.available)) notes.push(t("tnUnavailable"));
  $("target-note").textContent = notes.join("\n");
};

const load = async () => {
  I18N.applyStatic(document);
  document.documentElement.lang = I18N.lang;
  showPage(false);
  addEventListener("hashchange", () => showPage(true));
  for (const name of PAGES.slice(1)) {
    $(`nav-${name}`).onclick = () => {
      deep++;
    };
    $(`back-${name}`).onclick = back;
  }
  const manifest = typeof chrome.runtime.getManifest === "function" ? chrome.runtime.getManifest() : null;
  $("about-version").textContent = t("aboutVersion", { version: manifest ? manifest.version : "" });

  const [tab] = await chrome.tabs.query({ active: true, currentWindow: true });
  if (!tab) return render(null);
  chrome.runtime.sendMessage({ kind: "vc-get", tabId: tab.id }, (state) => render(state));
  $("clear").onclick = () => chrome.runtime.sendMessage({ kind: "vc-clear", tabId: tab.id }, () => render(null));

  const port = $("port");
  const token = $("token");
  chrome.storage.local.get({ sinkPort: 17650, sinkToken: "" }, (v) => {
    port.value = v.sinkPort;
    token.value = v.sinkToken;
  });
  // A manual entry taps every tab (the old behaviour) and is not the app's to take back.
  $("save").onclick = () =>
    chrome.storage.local.set({ sinkPort: Number(port.value) || 17650, sinkToken: token.value, sinkPaired: false, tapMode: "all" }, () => {
      $("sink-saved").textContent = t("sinkSaved");
    });

  const pairLine = $("pair");
  const showPair = (s) => {
    void chrome.runtime.lastError;
    if (!s) return (pairLine.textContent = t("pairLine", { how: t("pairUnavailable") }));
    if (s.error === "unsupported") {
      // Firefox (for Android): no native messaging, no desktop app to pair with.
      $("pair-now").hidden = true;
      return (pairLine.textContent = t("pairLine", { how: t("pairUnsupported") }));
    }
    pairLine.textContent = t("pairLine", { how: pairHow(s) });
  };
  chrome.runtime.sendMessage({ kind: "vc-pair-status" }, showPair);
  $("pair-now").onclick = () =>
    chrome.runtime.sendMessage({ kind: "vc-pair-now" }, () => {
      void chrome.runtime.lastError;
      setTimeout(() => chrome.runtime.sendMessage({ kind: "vc-pair-status" }, showPair), 800);
    });

  const askSources = () => chrome.runtime.sendMessage({ kind: "vc-source-of", tabId: tab.id }, showSources);
  askSources();
  $("policy-save").onclick = () => {
    const unlocked = [];
    if ($("unlock-obfuscated").checked) unlocked.push("obfuscated");
    if ($("unlock-social").checked) unlocked.push("social");
    chrome.storage.local.set(
      {
        sourcePolicy: $("policy-on").checked ? "on" : "off",
        sourceUnlocked: unlocked,
        sourceOwnHosts: hostLines($("own-hosts").value),
        sourceLearningHosts: hostLines($("learning-hosts").value),
      },
      () => {
        $("policy-saved").textContent = t("policySaved");
        askSources();
      }
    );
  };

  // The target: the sink on the PC, or a recording or a video saved on this device.
  const fileSave = $("file-save");
  const showFile = () => {
    const f = view.file;
    const usable = Boolean(f && f.available);
    $("target-file").disabled = !usable;
    $("target-video").disabled = !usable;
    $("file-state").textContent = usable && onDevice(view.target) ? fileLines(f).join("\n") : "";
    showTargetNote();
    fileSave.hidden = !(usable && onDevice(view.target) && f.recording > 0);
  };
  const askLink = () =>
    // The background owns the sink sockets and the recordings; ask it about this tab.
    chrome.runtime.sendMessage({ kind: "vc-link-status", tabId: tab.id }, (s) => {
      void chrome.runtime.lastError;
      view.link = s || null;
      view.file = (s && s.file) || null;
      view.asked.add("file");
      $("link").textContent = t("linkLine", { state: linkState(s) });
      showFile();
      showTargetLine();
    });
  const showTarget = () =>
    chrome.storage.local.get({ sinkTarget: "" }, (v) => {
      view.target = ["file", "video", "tailnet"].includes(v.sinkTarget) ? v.sinkTarget : "sink";
      $(`target-${view.target}`).checked = true;
      showBlocks();
      showTargetLine();
      askLink();
    });
  showTarget();
  for (const value of ["sink", "file", "video", "tailnet"]) {
    $(`target-${value}`).onchange = () =>
      chrome.storage.local.set({ sinkTarget: value === "sink" ? "" : value }, () => {
        view.target = value;
        $("target-saved").textContent = t("targetSaved");
        showBlocks();
        showTargetLine();
        askLink();
      });
  }

  // The target "the sink on the PC over the tailnet": its state with the next step, and the pairing.
  const tailnetState = $("tailnet-state");
  const tailnetResult = $("tailnet-result");
  const tailnetForget = $("tailnet-forget");
  const showTailnet = (s) => {
    void chrome.runtime.lastError;
    const usable = Boolean(s && s.available);
    view.tailnet = s || null;
    view.asked.add("tailnet");
    showTargetNote();
    $("target-tailnet").disabled = !usable;
    $("tailnet-pair").disabled = !usable;
    $("tailnet-offer").hidden = !(usable && s.offer);
    tailnetForget.hidden = !(usable && s.paired);
    showTargetLine();
    if (!usable) return say(tailnetState, "", t("tnUnavailable"));
    if (s.offer) $("tailnet-offer-text").textContent = t("tnOffer", { pc: s.offer });
    if (s.paired && !$("tailnet-host").value) $("tailnet-host").value = s.host;
    say(tailnetState, ...tailnetText(s));
  };
  const askTailnet = () => chrome.runtime.sendMessage({ kind: "vc-tailnet-status" }, showTailnet);
  askTailnet();
  const paired = (r) => {
    void chrome.runtime.lastError;
    const ok = Boolean(r && r.ok);
    const reason = r && r.reason;
    say(tailnetResult, ok ? "released" : "locked", ok ? t("tnPaired", { pc: r.host }) : t(Object.hasOwn(PAIR_ERROR_KEYS, reason) ? PAIR_ERROR_KEYS[reason] : "tnPairFailed"));
    if (ok) $("tailnet-code").value = "";
    askTailnet();
    showTarget();
  };
  const pairWith = (msg) => {
    // The answer is said in the tailnet block of the target page: it is shown from here on, whatever the target is.
    view.pairingOpen = true;
    showBlocks();
    say(tailnetResult, "", t("tnPairing"));
    chrome.runtime.sendMessage({ kind: "vc-tailnet-pair", name: $("tailnet-name").value, ...msg }, paired);
  };
  $("tailnet-offer-pair").onclick = () => {
    if (page !== "target") go("target");
    pairWith({ offer: true });
  };
  $("tailnet-pair").onclick = () => {
    // The code alone, or the whole pairing link (it names the PC too).
    const typed = String($("tailnet-code").value).trim();
    const link = /^https:\/\/([^/#?]+)\/?#streamtap-pair=1&code=([0-9a-f]{32})$/.exec(typed);
    if (link) $("tailnet-host").value = link[1];
    pairWith({ host: $("tailnet-host").value, code: link ? link[2] : typed.toLowerCase() });
  };
  // Forgetting needs a second tap: captures to the PC stop with it.
  let forgetArmed = false;
  tailnetForget.onclick = () => {
    if (!forgetArmed) {
      forgetArmed = true;
      tailnetForget.textContent = t("tnForgetArmed");
      return;
    }
    forgetArmed = false;
    tailnetForget.textContent = t("tnForget");
    chrome.runtime.sendMessage({ kind: "vc-tailnet-forget" }, (ok) => {
      void chrome.runtime.lastError;
      say(tailnetResult, ok ? "" : "locked", t(ok ? "tnForgotten" : "tnForgetFailed"));
      askTailnet();
    });
  };
  fileSave.onclick = () =>
    chrome.runtime.sendMessage({ kind: "vc-file-save", tabId: tab.id }, () => {
      void chrome.runtime.lastError;
      setTimeout(askLink, 500);
    });
};

load();
