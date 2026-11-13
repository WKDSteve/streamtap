// Source policy (loaded by background.js via importScripts): which sites may be
// tapped. Off unless the setting `sourcePolicy` is "on" (the desktop default
// stays as before; the mobile build turns it on). When on, the host of the
// tab's top document and of the asking frame are sorted into groups:
//
// - released: tapped as before (own content, free licences, public
//   broadcasters, learning platforms the user entered);
// - locked: built and working, but nothing is copied until the user unlocks
//   that group (`sourceUnlocked`), e.g. while travelling in a country where
//   it is allowed or after a change in the law;
// - unknown: not in any list, tapped with a hint to check the site's terms.
//
// DRM is not a group: the hook refuses EME in any case, there is no switch.
// The list and its reasons: tasks-vault, streamtap/ROADMAP/reports/
// 2026-11-13_mobil-zulaessige-quellen.md. This is a judgement, not legal advice.
"use strict";

// Group id -> state by default and the hosts it covers (a host also covers its
// subdomains). `own` and `learning` start empty: the user enters the hosts.
const SP_GROUPS = Object.freeze({
  own: { state: "released", hosts: [] },
  free: { state: "released", hosts: ["media.ccc.de", "archive.org", "commons.wikimedia.org", "upload.wikimedia.org"] },
  ard: {
    state: "released",
    hosts: ["ardmediathek.de", "ard.de", "daserste.de", "tagesschau.de", "sportschau.de", "br.de", "hr.de", "mdr.de", "ndr.de", "rbb-online.de", "radiobremen.de", "sr.de", "swr.de", "wdr.de", "one.de", "funk.net", "kinderkanal.de"],
  },
  zdf: { state: "released", hosts: ["zdf.de", "3sat.de", "kika.de", "zdfheute.de"] },
  learning: { state: "released", hosts: [] },
  obfuscated: { state: "locked", hosts: ["youtube.com", "youtube-nocookie.com", "youtu.be", "googlevideo.com"] },
  social: {
    state: "locked",
    hosts: ["instagram.com", "cdninstagram.com", "facebook.com", "fbcdn.net", "tiktok.com", "tiktokcdn.com", "x.com", "twitter.com", "twimg.com", "twitch.tv", "ttvnw.net"],
  },
});

// The default when nothing is stored: off in the Chrome build, on in the
// Firefox build (flavor-firefox.js). A stored "on" or "off" wins.
const SP_DEFAULT = typeof STREAMTAP_FLAVOR === "object" && STREAMTAP_FLAVOR && STREAMTAP_FLAVOR.sourcePolicyDefault === "on" ? "on" : "off";
const spPolicyOn = (stored) => (stored === "on" || stored === "off" ? stored : SP_DEFAULT) === "on";

const SP_MAX_USER_HOSTS = 200;
const SP_MAX_HOST_LEN = 253;

// A host as typed by the user: lower case, no scheme, path, port or leading
// dot; "" when it is not a plain DNS name (an IP address is allowed for own
// servers).
const spCleanHost = (v) => {
  if (typeof v !== "string") return "";
  let h = v.trim().toLowerCase();
  h = h.replace(/^[a-z][a-z0-9+.-]*:\/\//, "").replace(/[/?#].*$/, "").replace(/:\d+$/, "").replace(/^\.+/, "").replace(/\.+$/, "");
  if (!h || h.length > SP_MAX_HOST_LEN || !/^[a-z0-9.-]+$/.test(h) || h.includes("..")) return "";
  return h;
};

// The settings as stored, bounded and type-checked: anything else counts as
// absent, so a broken value never unlocks a group.
const spSettings = (v) => {
  const o = v && typeof v === "object" ? v : {};
  const hosts = (a) => (Array.isArray(a) ? a.slice(0, SP_MAX_USER_HOSTS).map(spCleanHost).filter(Boolean) : []);
  const unlocked = Array.isArray(o.sourceUnlocked)
    ? o.sourceUnlocked.filter((g) => typeof g === "string" && Object.hasOwn(SP_GROUPS, g) && SP_GROUPS[g].state === "locked")
    : [];
  return {
    on: spPolicyOn(o.sourcePolicy),
    unlocked: new Set(unlocked),
    own: hosts(o.sourceOwnHosts),
    learning: hosts(o.sourceLearningHosts),
  };
};

const spHostCovers = (listed, host) => host === listed || host.endsWith("." + listed);

// The group of one URL: { group, state } with state released, locked,
// unlocked or unknown. Anything that is not http(s) is unknown with group null.
// Locked groups are checked first, so a user entry cannot release a locked
// host by listing it as own; unlocking it is the way.
const spClassify = (url, settings) => {
  let host = "";
  try {
    const u = new URL(url);
    if (u.protocol === "http:" || u.protocol === "https:") host = u.hostname.toLowerCase().replace(/\.$/, "");
  } catch (e) {
    host = "";
  }
  if (!host) return { group: null, state: "unknown", host: "" };
  const s = settings || spSettings({});
  const lists = [];
  for (const [id, g] of Object.entries(SP_GROUPS)) if (g.state === "locked") lists.push([id, g.state, g.hosts]);
  lists.push(["own", "released", s.own], ["learning", "released", s.learning]);
  for (const [id, g] of Object.entries(SP_GROUPS)) if (g.state === "released" && g.hosts.length) lists.push([id, g.state, g.hosts]);
  for (const [id, state, hosts] of lists) {
    if (hosts.some((h) => spHostCovers(h, host))) {
      return { group: id, state: state === "locked" && s.unlocked.has(id) ? "unlocked" : state, host };
    }
  }
  return { group: null, state: "unknown", host };
};

// The verdict for a frame: every URL given (the tab's top document and the
// frame itself) is classified; one locked URL locks the frame (a locked
// player embedded in another site, or any frame on a locked page), all
// released make it released, anything else is unknown (tapped, with a hint).
// Policy off: always tap, state "off".
const spJudge = (urls, settings) => {
  const s = settings || spSettings({});
  if (!s.on) return { tap: true, state: "off", groups: [] };
  const seen = (Array.isArray(urls) ? urls : []).filter((u) => typeof u === "string" && u).map((u) => spClassify(u, s));
  const groups = [...new Set(seen.map((c) => c.group).filter(Boolean))];
  if (!seen.length) return { tap: true, state: "unknown", groups };
  if (seen.some((c) => c.state === "locked")) return { tap: false, state: "locked", groups };
  if (seen.every((c) => c.state === "released")) return { tap: true, state: "released", groups };
  if (seen.every((c) => c.state === "released" || c.state === "unlocked")) return { tap: true, state: "unlocked", groups };
  return { tap: true, state: "unknown", groups };
};

const SP_STORAGE_KEYS = ["sourcePolicy", "sourceUnlocked", "sourceOwnHosts", "sourceLearningHosts"];

// For the popup (vc-source-of): the verdict on a tab's top document (null
// when no page was seen) and the settings in effect, the default included.
const spOverview = (tabUrl) =>
  new Promise((resolve) => {
    chrome.storage.local.get(SP_STORAGE_KEYS, (v) => {
      void chrome.runtime.lastError;
      const s = spSettings(v);
      resolve({
        verdict: typeof tabUrl === "string" ? spJudge([tabUrl], s) : null,
        on: s.on,
        unlocked: [...s.unlocked],
        own: s.own,
        learning: s.learning,
        lockable: Object.keys(SP_GROUPS).filter((g) => SP_GROUPS[g].state === "locked"),
      });
    });
  });

// What background.js answers a content script (vc-source-state): reads the
// settings and judges the frame by the URLs the browser reports. A sub-frame
// whose tab URL the browser does not report (Firefox without host access to
// the top document) cannot be judged by its page and is not tapped.
const spState = (frame) =>
  new Promise((resolve) => {
    chrome.storage.local.get(SP_STORAGE_KEYS, (v) => {
      void chrome.runtime.lastError;
      const settings = spSettings(v);
      if (settings.on && frame && frame.frameId !== 0 && typeof frame.tabUrl !== "string") {
        return resolve({ tap: false, state: "no-top", groups: [] });
      }
      const urls = [];
      if (frame && typeof frame.tabUrl === "string") urls.push(frame.tabUrl);
      if (frame && typeof frame.url === "string") urls.push(frame.url);
      resolve(spJudge(urls, settings));
    });
  });
