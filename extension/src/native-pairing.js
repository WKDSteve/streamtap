// Service-worker side of the pairing with the streamtap app (loaded by
// background.js via importScripts).
//
// The extension connects to the app's native messaging host
// ("io.github.wkdsteve.streamtap", registered from the app's "Connect browser"
// panel). The app asks the user to approve this browser profile; only then it
// sends the sink's port and token, which are stored exactly like the popup
// stores them. The popup input stays as a manual fallback.
//
// While paired, the extension reports the tabs that hold a MediaSource (title,
// host, favicon, DRM flag) and the app switches capture on or off per tab: the
// default for a paired browser is "selected" mode, where only the tabs the user
// chose in the app are tapped (storage key tapMode).
//
// Everything that arrives from the host is checked field by field; everything
// that leaves is built here (page-controlled strings are cleaned and bounded).
// The token is never logged.
"use strict";

const NP_HOST = "io.github.wkdsteve.streamtap";
const NP_PROTOCOL = 1;
const NP_MAX_TABS = 32;
const NP_MAX_TITLE = 200;
const NP_MAX_HOST = 253;
const NP_FAVICON_MAX_BYTES = 6000; // as base64 plus prefix this stays under the host's 8 KiB limit
const NP_FAVICON_TTL_MS = 5 * 60 * 1000;
const NP_FAVICON_CACHE = 64;
const NP_BACKOFF_MIN_MS = 5000;
const NP_BACKOFF_MAX_MS = 5 * 60 * 1000;
const NP_PAUSE_MS = 10 * 60 * 1000; // after a denial or a revocation nobody asks again for this long
const NP_SEND_GAP_MS = 1000; // at most one tab list per second
const NP_PAIRINGS = ["pending", "approved", "denied", "revoked"];

const np = {
  deps: null, // { mediaTabs(): [{ id, url, title, drm }] }
  port: null,
  status: { connected: false, pairing: null, error: null },
  selected: new Set(), // tab ids that are tapped (mode "selected")
  origins: new Map(), // tab id -> the origin the tab was chosen on; a tab that leaves it is no longer chosen
  ready: Promise.resolve(),
  backoff: NP_BACKOFF_MIN_MS,
  retryTimer: null,
  pausedUntil: 0,
  sendTimer: null,
  lastSentAt: 0,
  lastSig: "",
  forceNext: false,
  favicons: new Map(), // host -> { url, at }
};

// A string the page influences, as one line of plain text of bounded length.
const npClean = (v, max) =>
  String(v ?? "")
    .replace(/[\u0000-\u001f\u007f-\u009f\u200b-\u200f\u202a-\u202e\u2060-\u2064\u2066-\u2069\ufeff]+/g, " ")
    .replace(/\s+/g, " ")
    .trim()
    .slice(0, max);

// The origin (scheme, host, port) of a web page, or "" for anything else.
const npOriginOf = (url) => {
  try {
    const u = new URL(url);
    return u.protocol === "http:" || u.protocol === "https:" ? u.origin : "";
  } catch (e) {
    return "";
  }
};

const npHostOf = (url) => {
  try {
    const u = new URL(url);
    if (u.protocol !== "http:" && u.protocol !== "https:") return "";
    const host = u.hostname;
    return /^[A-Za-z0-9._:\[\]-]+$/.test(host) && host.length <= NP_MAX_HOST ? host : "";
  } catch (e) {
    return "";
  }
};

const npBrowser = () => {
  try {
    const brands = (navigator.userAgentData && navigator.userAgentData.brands) || [];
    const names = brands.map((b) => String(b.brand));
    if (names.some((n) => /edge/i.test(n))) return "edge";
    if (names.some((n) => /brave/i.test(n))) return "brave";
    if (names.some((n) => /chrom/i.test(n))) return "chrome";
  } catch (e) {
    // fall through
  }
  return "other";
};

const npInstanceId = async () => {
  const v = await chrome.storage.local.get({ pairInstance: "" });
  if (typeof v.pairInstance === "string" && /^[A-Za-z0-9-]{8,64}$/.test(v.pairInstance)) return v.pairInstance;
  const id = crypto.randomUUID();
  await chrome.storage.local.set({ pairInstance: id });
  return id;
};

// ---- credentials and tapping ------------------------------------------------

const npValidCredentials = (m) =>
  Number.isInteger(m.port) &&
  m.port >= 1024 &&
  m.port <= 65535 &&
  typeof m.token === "string" &&
  m.token.length >= 16 &&
  m.token.length <= 256 &&
  /^[\x21-\x7e]+$/.test(m.token) &&
  !m.token.startsWith("-");

const npStoreCredentials = (m) =>
  chrome.storage.local.set({ sinkPort: m.port, sinkToken: m.token, sinkPaired: true, tapMode: "selected" });

// A revoked pairing takes back what it gave. A manual entry in the popup
// (sinkPaired false) is left alone.
const npClearPaired = async () => {
  const v = await chrome.storage.local.get({ sinkPaired: false });
  if (v.sinkPaired === true) await chrome.storage.local.remove(["sinkToken", "sinkPaired", "tapMode"]);
  await npSetSelected(new Set(), []);
};

const npPersistSelected = () => {
  try {
    return chrome.storage.session.set({ tapTabs: [...np.selected], tapOrigins: Object.fromEntries(np.origins) });
  } catch (e) {
    return Promise.resolve();
  }
};

// Replaces the selection; `notify` lists the tab ids whose content scripts are told.
const npSetSelected = async (next, notify) => {
  const before = np.selected;
  np.selected = next;
  for (const id of [...np.origins.keys()]) if (!next.has(id)) np.origins.delete(id);
  await npPersistSelected();
  for (const id of new Set([...before, ...next, ...notify])) {
    npNotifyTab(id, next.has(id));
  }
};

const npNotifyTab = (tabId, on) => {
  try {
    chrome.tabs.sendMessage(tabId, { kind: "vc-tap", on }).catch(() => {});
  } catch (e) {
    // no content script there
  }
};

// The app asked to switch capture of one tab. Only a web tab that holds a
// MediaSource is accepted, and the choice is bound to that tab's origin (see
// npTapState). Switching on reloads the tab once: a stream that is already
// playing has appended its init segment, and a capture without it is useless.
const npSetTap = async (tabId, on) => {
  if (!Number.isInteger(tabId) || tabId <= 0 || typeof on !== "boolean") return false;
  const media = on ? np.deps.mediaTabs().find((t) => t.id === tabId) : null;
  const origin = media ? npOriginOf(media.url) : "";
  if (on && !origin) return false;
  if (on === np.selected.has(tabId)) return true;
  const next = new Set(np.selected);
  if (on) {
    if (next.size >= NP_MAX_TABS) return false;
    next.add(tabId);
    np.origins.set(tabId, origin);
  } else {
    next.delete(tabId);
  }
  await npSetSelected(next, []);
  if (on) {
    try {
      await chrome.tabs.reload(tabId);
    } catch (e) {
      // the tab is gone
    }
  }
  npScheduleSend(true);
  return true;
};

// What a content script asks when it starts: is this tab tapped? The user
// chose a tab showing one site: when the top frame of that tab is on another
// origin now (the user or a redirect navigated away), the choice ends here,
// instead of silently copying whatever the next site plays. `frame` is the
// asking frame (frameId, url) as the browser reports it.
const npTapState = async (tabId, frame) => {
  await np.ready;
  if (!Number.isInteger(tabId) || !np.selected.has(tabId)) return { on: false };
  if (frame && frame.frameId === 0 && npOriginOf(frame.url) !== np.origins.get(tabId)) {
    const next = new Set(np.selected);
    next.delete(tabId);
    await npSetSelected(next, []);
    npScheduleSend(true);
    return { on: false };
  }
  return { on: true };
};

// ---- the tab list -------------------------------------------------------------

const npToDataUrl = (bytes) => {
  let s = "";
  for (let i = 0; i < bytes.length; i++) s += String.fromCharCode(bytes[i]);
  return "data:image/png;base64," + btoa(s);
};

const npFavicon = async (pageUrl, host) => {
  if (!host) return null;
  const hit = np.favicons.get(host);
  if (hit && Date.now() - hit.at < NP_FAVICON_TTL_MS) return hit.url;
  let url = null;
  try {
    const u = new URL(chrome.runtime.getURL("/_favicon/"));
    u.searchParams.set("pageUrl", pageUrl);
    u.searchParams.set("size", "32");
    const r = await fetch(u.toString());
    if (r.ok) {
      const bytes = new Uint8Array(await r.arrayBuffer());
      const png = bytes.length > 8 && bytes[0] === 0x89 && bytes[1] === 0x50 && bytes[2] === 0x4e && bytes[3] === 0x47;
      if (png && bytes.length <= NP_FAVICON_MAX_BYTES) url = npToDataUrl(bytes);
    }
  } catch (e) {
    url = null;
  }
  if (np.favicons.size >= NP_FAVICON_CACHE) np.favicons.delete(np.favicons.keys().next().value);
  np.favicons.set(host, { url, at: Date.now() });
  return url;
};

const npActiveIds = async () => {
  try {
    const active = await chrome.tabs.query({ active: true });
    return new Set(active.map((t) => t.id));
  } catch (e) {
    return new Set();
  }
};

const npTabList = async () => {
  const active = await npActiveIds();
  const media = np.deps.mediaTabs().slice(0, NP_MAX_TABS);
  const out = [];
  for (const t of media) {
    const host = npHostOf(t.url);
    out.push({
      id: t.id,
      title: npClean(t.title, NP_MAX_TITLE),
      host,
      favicon: await npFavicon(t.url, host),
      drm: t.drm === true,
      tapping: np.selected.has(t.id),
      active: active.has(t.id),
    });
  }
  return out;
};

const npSend = (msg) => {
  if (!np.port) return false;
  try {
    np.port.postMessage(msg);
    return true;
  } catch (e) {
    return false;
  }
};

const npSendTabs = async (force) => {
  if (!np.port || np.status.pairing !== "approved") return;
  const tabsList = await npTabList();
  const sig = JSON.stringify(tabsList.map((t) => [t.id, t.title, t.host, t.drm, t.tapping, t.active, !!t.favicon]));
  if (!force && sig === np.lastSig) return;
  np.lastSig = sig;
  np.lastSentAt = Date.now();
  // The favicon is left out when absent (the host's schema allows a missing one).
  npSend({ v: NP_PROTOCOL, t: "tabs", tabs: tabsList.map((t) => (t.favicon ? t : { ...t, favicon: null })) });
};

// Called when the registry changed; coalesced to one list per second.
const npScheduleSend = (force) => {
  if (!np.port || np.status.pairing !== "approved") return;
  if (np.sendTimer) {
    if (force) np.forceNext = true;
    return;
  }
  if (force) np.forceNext = true;
  const wait = Math.max(0, np.lastSentAt + NP_SEND_GAP_MS - Date.now());
  np.sendTimer = setTimeout(() => {
    np.sendTimer = null;
    const f = np.forceNext === true;
    np.forceNext = false;
    npSendTabs(f).catch(() => {});
  }, wait);
};

// ---- the connection -----------------------------------------------------------

const npPause = () => {
  np.pausedUntil = Date.now() + NP_PAUSE_MS;
};

const npOnMessage = async (m) => {
  if (!m || typeof m !== "object" || m.v !== NP_PROTOCOL || typeof m.t !== "string") return;
  switch (m.t) {
    case "state": {
      if (!NP_PAIRINGS.includes(m.pairing)) return;
      np.status.pairing = m.pairing;
      np.status.error = null;
      np.backoff = NP_BACKOFF_MIN_MS;
      if (m.pairing === "approved") {
        np.lastSig = "";
        npScheduleSend(true);
      } else {
        if (m.pairing === "denied" || m.pairing === "revoked") npPause();
        // Whatever the pairing handed out is gone with the approval, also when
        // the revocation happened while this browser was not connected.
        await npClearPaired();
      }
      break;
    }
    case "credentials":
      // Only an approved pairing carries credentials; anything else is ignored.
      if (np.status.pairing === "approved" && npValidCredentials(m)) await npStoreCredentials(m);
      break;
    case "tap":
      if (np.status.pairing === "approved") await npSetTap(m.tab, m.on);
      break;
    case "error":
      np.status.error = typeof m.code === "string" ? m.code.slice(0, 32) : "error";
      break;
    default:
      break;
  }
};

const npScheduleReconnect = () => {
  if (np.retryTimer) return;
  const wait = Math.max(np.backoff, np.pausedUntil - Date.now());
  np.backoff = Math.min(np.backoff * 2, NP_BACKOFF_MAX_MS);
  np.retryTimer = setTimeout(() => {
    np.retryTimer = null;
    npConnect();
  }, wait);
};

const npConnect = () => {
  if (np.port) return;
  if (Date.now() < np.pausedUntil) {
    npScheduleReconnect();
    return;
  }
  let port;
  try {
    port = chrome.runtime.connectNative(NP_HOST);
  } catch (e) {
    np.status = { connected: false, pairing: null, error: "connect-failed" };
    npScheduleReconnect();
    return;
  }
  np.port = port;
  np.status = { connected: true, pairing: null, error: null };
  port.onMessage.addListener((m) => {
    npOnMessage(m).catch(() => {});
  });
  port.onDisconnect.addListener(() => {
    const reason = chrome.runtime.lastError && chrome.runtime.lastError.message;
    if (np.port === port) np.port = null;
    np.status.connected = false;
    np.status.pairing = null;
    if (!np.status.error && reason) np.status.error = "host-unavailable";
    if (np.sendTimer) {
      clearTimeout(np.sendTimer);
      np.sendTimer = null;
    }
    npScheduleReconnect();
  });
  npInstanceId()
    .then((instance) =>
      npSend({ v: NP_PROTOCOL, t: "hello", instance, browser: npBrowser(), version: chrome.runtime.getManifest().version })
    )
    .catch(() => {});
};

// Status for the popup. Never contains the token.
const npStatus = () => ({
  connected: np.status.connected,
  pairing: np.status.pairing,
  error: np.status.error,
  paused: Date.now() < np.pausedUntil,
  tapped: np.selected.size,
});

// Asks for a new try at once (the popup's "pair now"), also after a denial.
const npRetryNow = () => {
  np.pausedUntil = 0;
  np.backoff = NP_BACKOFF_MIN_MS;
  if (np.retryTimer) {
    clearTimeout(np.retryTimer);
    np.retryTimer = null;
  }
  npConnect();
};

// A closed tab is no longer tapped.
const npTabClosed = (tabId) => {
  if (np.selected.has(tabId)) {
    const next = new Set(np.selected);
    next.delete(tabId);
    np.selected = next;
    np.origins.delete(tabId);
    npPersistSelected();
  }
  npScheduleSend(false);
};

const npInit = (deps) => {
  np.deps = deps;
  np.ready = (async () => {
    try {
      const v = await chrome.storage.session.get({ tapTabs: [], tapOrigins: {} });
      const origins = v.tapOrigins && typeof v.tapOrigins === "object" ? v.tapOrigins : {};
      if (Array.isArray(v.tapTabs)) {
        // A tab without a stored web origin cannot be checked later: it is not chosen.
        for (const n of v.tapTabs) {
          const o = origins[n];
          if (np.selected.size >= NP_MAX_TABS) break;
          if (Number.isInteger(n) && n > 0 && typeof o === "string" && o.length <= 300 && npOriginOf(o) === o) {
            np.selected.add(n);
            np.origins.set(n, o);
          }
        }
      }
    } catch (e) {
      np.selected = new Set();
      np.origins = new Map();
    }
  })();
  npConnect();
};
