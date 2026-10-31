// Service worker: per-tab registry of MediaSources / SourceBuffers / appends
// for the popup, plus a badge with the number of SourceBuffers that have
// received media segments. State lives in memory only; it is lost when the
// worker is suspended, which is fine for a spike. The sink link (one WebSocket
// per tapping frame) lives in sink-link.js, the pairing with the desktop app
// (native messaging, tab list, per-tab capture) in native-pairing.js.
"use strict";

importScripts("sink-link.js", "native-pairing.js");

const tabs = new Map(); // tabId -> { url, frames: Set, ms: Map<msId, {...}>, drm: [] }

const tabState = (tabId, url) => {
  let t = tabs.get(tabId);
  if (!t) {
    t = { url, title: "", frames: new Set(), ms: new Map(), drm: [], events: 0 };
    tabs.set(tabId, t);
  }
  return t;
};

const msKey = (frameUrl, msId) => `${frameUrl}#${msId}`;

// Pages can forge the hook's records, so the registry is bounded: beyond the
// limits a record updates a throwaway entry that is not kept.
const MAX_MS_PER_TAB = 64;
const MAX_SB_PER_MS = 32;
const MAX_FRAMES_PER_TAB = 100;
const MAX_MIMES = 20;

const msState = (t, frameUrl, msId) => {
  const key = msKey(frameUrl, msId);
  let m = t.ms.get(key);
  if (!m) {
    m = { msId, frameUrl, blobUrl: null, sb: new Map(), ended: false };
    if (t.ms.size < MAX_MS_PER_TAB) t.ms.set(key, m);
  }
  return m;
};

const sbState = (m, sbId, mime) => {
  let s = m.sb.get(sbId);
  if (!s && m.sb.size >= MAX_SB_PER_MS) {
    return { sbId, mime, appends: 0, bytes: 0, inits: 0, media: 0, firstSec: null, lastSec: null, lastEndSec: null, kind: null, mimes: [] };
  }
  if (!s) {
    s = {
      sbId,
      mime,
      appends: 0,
      bytes: 0,
      inits: 0,
      media: 0,
      firstSec: null,
      lastSec: null,
      lastEndSec: null,
      kind: null,
      mimes: [],
    };
    m.sb.set(sbId, s);
  }
  return s;
};

const updateBadge = (tabId) => {
  const t = tabs.get(tabId);
  let active = 0;
  if (t) for (const m of t.ms.values()) for (const s of m.sb.values()) if (s.media > 0) active++;
  const text = active ? String(active) : "";
  chrome.action.setBadgeText({ tabId, text }).catch(() => {});
  if (t && t.drm.length) {
    chrome.action.setBadgeBackgroundColor({ tabId, color: "#b00020" }).catch(() => {});
  } else {
    chrome.action.setBadgeBackgroundColor({ tabId, color: "#1565c0" }).catch(() => {});
  }
};

chrome.runtime.onMessage.addListener((msg, sender, sendResponse) => {
  if (msg && msg.kind === "vc-get" && typeof msg.tabId === "number") {
    sendResponse(serialize(tabs.get(msg.tabId)));
    return false;
  }
  if (msg && msg.kind === "vc-link-status" && typeof msg.tabId === "number") {
    sendResponse(sinkLinkStatus(msg.tabId));
    return false;
  }
  if (msg && msg.kind === "vc-pair-status") {
    sendResponse(npStatus());
    return false;
  }
  if (msg && msg.kind === "vc-pair-now") {
    npRetryNow();
    sendResponse(true);
    return false;
  }
  // A content script asks whether its tab is tapped (mode "selected").
  if (msg && msg.kind === "vc-tap-state") {
    const id = sender.tab && sender.tab.id;
    npTapState(id).then(sendResponse, () => sendResponse({ on: false }));
    return true;
  }
  if (msg && msg.kind === "vc-clear" && typeof msg.tabId === "number") {
    tabs.delete(msg.tabId);
    updateBadge(msg.tabId);
    sendResponse(true);
    return false;
  }

  const tabId = sender.tab && sender.tab.id;
  if (typeof tabId !== "number") return false;
  const t = tabState(tabId, sender.tab.url);
  if (typeof sender.tab.title === "string") t.title = sender.tab.title.slice(0, 300);

  if (msg.kind !== "vc-event" || !msg.event || typeof msg.event !== "object") return false;

  const e = msg.event;
  // Strings are bounded here as well as in content.js; the frame URL comes
  // from the browser, not from the record.
  const cut = (v, n) => String(v ?? "").slice(0, n);
  const frameUrl = cut(sender.url || e.frameUrl, 2048);
  t.events++;

  switch (e.type) {
    case "frame":
      if (t.frames.size < MAX_FRAMES_PER_TAB) t.frames.add(frameUrl);
      break;
    case "mediasource":
      msState(t, frameUrl, e.msId);
      break;
    case "objecturl":
      msState(t, frameUrl, e.msId).blobUrl = cut(e.url, 512);
      break;
    case "sourcebuffer":
      sbState(msState(t, frameUrl, e.msId), e.sbId, cut(e.mime, 256));
      break;
    case "changetype": {
      // SourceBuffer ids count per frame: only the sending frame's MediaSource
      // (msId null for one created before the hook, as its appends).
      const m = t.ms.get(msKey(frameUrl, e.msId));
      const s = m && m.sb.get(e.sbId);
      if (s) {
        if (s.mimes.length < MAX_MIMES) s.mimes.push(s.mime);
        s.mime = cut(e.mime, 256);
      }
      break;
    }
    case "append": {
      const s = sbState(msState(t, frameUrl, e.msId), e.sbId, cut(e.mime, 256));
      s.appends++;
      s.bytes += Number(e.bytes) || 0;
      s.kind = cut(e.kind, 16);
      // The rest of a split init belongs to that init: no media segment.
      if (e.isInit) s.inits++;
      else if (!e.initCont) s.media++;
      if (Number.isFinite(e.startSec)) {
        if (s.firstSec === null || e.startSec < s.firstSec) s.firstSec = e.startSec;
        if (s.lastSec === null || e.startSec > s.lastSec) {
          s.lastSec = e.startSec;
          s.lastEndSec = Number.isFinite(e.durationSec) ? +(e.startSec + e.durationSec).toFixed(3) : null;
        }
      }
      break;
    }
    case "endofstream":
      for (const m of t.ms.values()) if (m.msId === e.msId && m.frameUrl === frameUrl) m.ended = true;
      break;
    case "drm":
      if (t.drm.length < 50) {
        const robustness = Array.isArray(e.robustness) ? e.robustness.slice(0, 10).map((r) => cut(r, 64)) : [];
        t.drm.push({ keySystem: cut(e.keySystem, 128), robustness, frameUrl });
      }
      break;
    case "encrypted":
      // Encrypted media seen in a SourceBuffer: not tapped (hook.js).
      if (t.drm.length < 50) {
        t.drm.push({ keySystem: `encrypted media in SourceBuffer #${cut(e.sbId, 12)}`, robustness: [], frameUrl });
      }
      break;
    default:
      break;
  }
  updateBadge(tabId);
  npScheduleSend(false);
  return false;
});

const serialize = (t) => {
  if (!t) return null;
  return {
    url: t.url,
    events: t.events,
    frames: [...t.frames],
    drm: t.drm,
    mediaSources: [...t.ms.values()].map((m) => ({
      msId: m.msId,
      frameUrl: m.frameUrl,
      blobUrl: m.blobUrl,
      ended: m.ended,
      sourceBuffers: [...m.sb.values()],
    })),
  };
};

// Forget a tab's state when it navigates to a new document or closes.
// tabs.onUpdated needs no extra permission (unlike webNavigation).
chrome.tabs.onRemoved.addListener((tabId) => {
  tabs.delete(tabId);
  npTabClosed(tabId);
});
chrome.tabs.onUpdated.addListener((tabId, changeInfo) => {
  if (changeInfo.status === "loading") {
    tabs.delete(tabId);
    updateBadge(tabId);
    npScheduleSend(false);
  }
});

// Pairing with the desktop app: the tabs that hold a MediaSource are what the
// app lists. Frame URLs are not reported, only the tab's own URL host.
npInit({
  mediaTabs: () =>
    [...tabs]
      .filter(([, t]) => t.ms.size > 0)
      .map(([id, t]) => ({ id, url: t.url, title: t.title, drm: t.drm.length > 0 })),
});
