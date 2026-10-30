// Service worker: per-tab registry of MediaSources / SourceBuffers / appends
// for the popup, plus a badge with the number of SourceBuffers that have
// received media segments. State lives in memory only; it is lost when the
// worker is suspended, which is fine for a spike.
"use strict";

const tabs = new Map(); // tabId -> { url, frames: Set, ms: Map<msId, {...}>, drm: [] }

const tabState = (tabId, url) => {
  let t = tabs.get(tabId);
  if (!t) {
    t = { url, frames: new Set(), ms: new Map(), drm: [], events: 0 };
    tabs.set(tabId, t);
  }
  return t;
};

const msKey = (frameUrl, msId) => `${frameUrl}#${msId}`;

const msState = (t, frameUrl, msId) => {
  const key = msKey(frameUrl, msId);
  let m = t.ms.get(key);
  if (!m) {
    m = { msId, frameUrl, blobUrl: null, sb: new Map(), ended: false };
    t.ms.set(key, m);
  }
  return m;
};

const sbState = (m, sbId, mime) => {
  let s = m.sb.get(sbId);
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
  if (msg && msg.kind === "vc-clear" && typeof msg.tabId === "number") {
    tabs.delete(msg.tabId);
    updateBadge(msg.tabId);
    sendResponse(true);
    return false;
  }

  const tabId = sender.tab && sender.tab.id;
  if (typeof tabId !== "number") return false;
  const t = tabState(tabId, sender.tab.url);

  if (msg.kind === "vc-frame") {
    t.frames.add(msg.frameUrl);
    return false;
  }
  if (msg.kind !== "vc-event") return false;

  const e = msg.event;
  t.events++;

  switch (e.type) {
    case "mediasource":
      msState(t, e.frameUrl, e.msId);
      break;
    case "objecturl":
      msState(t, e.frameUrl, e.msId).blobUrl = e.url;
      break;
    case "sourcebuffer":
      sbState(msState(t, e.frameUrl, e.msId), e.sbId, e.mime);
      break;
    case "changetype": {
      for (const m of t.ms.values()) {
        const s = m.sb.get(e.sbId);
        if (s) {
          s.mimes.push(s.mime);
          s.mime = e.mime;
        }
      }
      break;
    }
    case "append": {
      const s = sbState(msState(t, e.frameUrl, e.msId), e.sbId, e.mime);
      s.appends++;
      s.bytes += e.bytes;
      s.kind = e.kind;
      if (e.isInit) s.inits++;
      else s.media++;
      if (e.startSec !== null) {
        if (s.firstSec === null || e.startSec < s.firstSec) s.firstSec = e.startSec;
        if (s.lastSec === null || e.startSec > s.lastSec) {
          s.lastSec = e.startSec;
          s.lastEndSec = e.durationSec ? +(e.startSec + e.durationSec).toFixed(3) : null;
        }
      }
      break;
    }
    case "endofstream":
      for (const m of t.ms.values()) if (m.msId === e.msId && m.frameUrl === e.frameUrl) m.ended = true;
      break;
    case "drm":
      t.drm.push({ keySystem: e.keySystem, robustness: e.robustness, frameUrl: e.frameUrl });
      break;
    default:
      break;
  }
  updateBadge(tabId);
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
chrome.tabs.onRemoved.addListener((tabId) => tabs.delete(tabId));
chrome.tabs.onUpdated.addListener((tabId, changeInfo) => {
  if (changeInfo.status === "loading") {
    tabs.delete(tabId);
    updateBadge(tabId);
  }
});
