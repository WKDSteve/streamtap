// Isolated-world content script.
//
// Forwards the hook's metadata records to the service worker (badge, popup)
// and hands every segment payload to the service worker over runtime ports
// ("vc-sink"), one port per MediaSource: one MediaSource is one video, and
// each port becomes one capture session on the sink. The service worker owns
// the WebSocket to the local sink (src/sink-link.js), so the page's CSP does
// not apply to it and the sink can check the extension's origin exactly.
//
// Port messages are JSON, so payloads travel as data: URLs (FileReader encodes
// natively). One promise chain per port keeps them in append order.
//
// The hook's records arrive via window.postMessage, which any page script can
// forge: every field that leaves this script is type-checked and bounded.
//
// Injected outside an extension (tests) there is no chrome.*: the script then
// only counts what it would have sent.
(() => {
  "use strict";

  const TAG = "[vc-content]";
  const inExtension = typeof chrome !== "undefined" && !!(chrome.runtime && chrome.runtime.id);

  // ---------------------------------------------------------------------------
  // Validation of the forgeable records
  // ---------------------------------------------------------------------------

  const isId = (v) => Number.isInteger(v) && v > 0 && v <= 0xffffffff;
  const isU64 = (v) => Number.isSafeInteger(v) && v >= 0;
  // tfdt above 2^53 comes as a decimal string (hook.js u64Exact).
  const isTfdt = (v) => isU64(v) || (typeof v === "string" && /^[0-9]{1,20}$/.test(v));
  const isNum = (v) => typeof v === "number" && Number.isFinite(v);
  const str = (v, max) => (typeof v === "string" ? v.slice(0, max) : "");
  const optId = (v) => (isId(v) ? v : null);
  const optNum = (v) => (isNum(v) ? v : null);

  // A bounded copy of a hook record for the service worker's registry. The
  // frame URL is this script's own, not the page's claim.
  const cleanRecord = (d) => {
    const base = { type: str(d.type, 32), frameUrl: location.href.slice(0, 2048) };
    switch (d.type) {
      case "append":
        return {
          ...base,
          sbId: optId(d.sbId),
          msId: optId(d.msId),
          mime: str(d.mime, 256),
          seq: isU64(d.seq) ? d.seq : 0,
          bytes: isU64(d.bytes) ? d.bytes : 0,
          kind: str(d.kind, 16),
          isInit: d.isInit === true,
          // The rest of a split init: neither an init nor media in the counts.
          initCont: d.initCont === true && d.isInit !== true,
          boxes: Array.isArray(d.boxes) ? d.boxes.slice(0, 16).map((b) => str(b, 8)) : [],
          startSec: optNum(d.startSec),
          durationSec: optNum(d.durationSec),
          encrypted: d.encrypted === true,
        };
      case "mediasource":
      case "endofstream":
      case "msclose":
        return { ...base, msId: optId(d.msId) };
      case "objecturl":
        return { ...base, msId: optId(d.msId), url: str(d.url, 512) };
      case "sourcebuffer":
      case "changetype":
        return { ...base, sbId: optId(d.sbId), msId: optId(d.msId), mime: str(d.mime, 256) };
      case "encrypted":
        return { ...base, sbId: optId(d.sbId), msId: optId(d.msId) };
      case "drm":
        return {
          ...base,
          keySystem: str(d.keySystem, 128),
          robustness: Array.isArray(d.robustness) ? d.robustness.slice(0, 10).map((r) => str(r, 64)) : [],
        };
      case "remove":
      case "timestampoffset":
        return { ...base, sbId: optId(d.sbId) };
      default:
        return null;
    }
  };

  // The segment header for the sink, or null when the record is malformed.
  const segmentHeader = (m) => {
    if (!isId(m.sbId) || !isU64(m.seq)) return null;
    if (m.msId != null && !isId(m.msId)) return null;
    if (m.tfdt != null && !isTfdt(m.tfdt)) return null;
    if (m.timescale != null && !isId(m.timescale)) return null;
    if (m.dur != null && !isU64(m.dur)) return null;
    if (m.track != null && !isId(m.track)) return null;
    if (m.tso != null && !isNum(m.tso)) return null;
    if (m.initLen != null && !(isU64(m.initLen) && m.initLen > 0)) return null;
    return {
      sb: m.sbId,
      seq: m.seq,
      ms: m.msId ?? null,
      mime: str(m.mime, 256),
      isInit: m.isInit === true,
      initCont: m.initCont === true && m.isInit !== true,
      // The init part of an init+media append (for keeping only the init).
      initLen: m.isInit === true && m.initLen != null ? m.initLen : null,
      kind: str(m.kind, 16),
      tfdt: m.tfdt ?? null,
      // Track ID of the first traf: the track tfdt and dur belong to.
      track: m.track ?? null,
      timescale: m.timescale ?? null,
      dur: m.dur ?? null,
      tso: m.tso ?? null,
      seqMode: m.seqMode === true,
      tsoSet: m.tsoSet === true,
    };
  };

  // ---------------------------------------------------------------------------
  // Metadata relay
  // ---------------------------------------------------------------------------

  const forward = (record) => {
    if (!inExtension || !record) return;
    try {
      chrome.runtime.sendMessage({ kind: "vc-event", event: record }, () => {
        void chrome.runtime.lastError;
      });
    } catch (e) {
      // Extension context invalidated (reload during development): stay quiet.
    }
  };

  // ---------------------------------------------------------------------------
  // Segment relay: one port (one sink session) per MediaSource
  // ---------------------------------------------------------------------------

  const MAX_QUEUED_BYTES = 64 * 1024 * 1024; // all payloads waiting in this frame
  const MAX_INIT_BYTES = 1024 * 1024; // init segments kept for a new port
  const MAX_KEPT_INITS = 16;
  // sb noted in `unkept` per link. A page can forge init records with ever new sb ids (each pushes the oldest kept
  // init in); unbounded, every new port would get one `lost` per entry and the sink one `initlost` frame each.
  const MAX_UNKEPT = 64;
  const MAX_LINKS = 8; // MediaSources tapped at the same time in this frame

  const links = new Map(); // ms key -> link
  const stats = { queuedBytes: 0, posted: 0, dropped: 0, dead: false };

  const toDataUrl = (buffer) =>
    new Promise((resolve, reject) => {
      const r = new FileReader();
      r.onload = () => resolve(r.result);
      r.onerror = () => reject(r.error);
      r.readAsDataURL(new Blob([buffer]));
    });

  const linkFor = (msId) => {
    const key = msId == null ? "pre-hook" : String(msId);
    let link = links.get(key);
    if (!link) {
      if (links.size >= MAX_LINKS) return null;
      link = {
        key,
        session: (crypto.randomUUID && crypto.randomUUID()) || String(Math.random()).slice(2),
        port: null,
        ports: 0,
        // sb -> [{ header, payload }, ...]: the init in effect at the end of
        // the chain, with its continuation chunks (an init split across appends)
        inits: new Map(),
        // sb whose init in effect could not be kept (too large, or more
        // than MAX_KEPT_INITS): a new port is told, so it waits for the next
        unkept: new Set(),
        // sb whose init was dropped at this point of the chain: its media is
        // not handed on until its next init
        lost: new Set(),
        chain: Promise.resolve(),
        closed: false,
      };
      links.set(key, link);
    }
    return link;
  };

  // Opens the link's port when there is none. A port after the first one (the
  // service worker restarted) first gets the init segments in effect at this
  // point of the chain: the sink starts a new capture directory, and the
  // player appended its inits only once.
  const ensurePort = async (link) => {
    if (link.port || link.closed || stats.dead || !inExtension) return link.port;
    let port;
    try {
      port = chrome.runtime.connect({ name: "vc-sink" });
    } catch (e) {
      stats.dead = true;
      return null;
    }
    link.port = port;
    link.ports++;
    port.onDisconnect.addListener(() => {
      void chrome.runtime.lastError;
      if (link.port === port) link.port = null;
      // A restarted service worker: reconnect on the next segment. A reloaded
      // extension: give up.
      if (!(chrome.runtime && chrome.runtime.id)) stats.dead = true;
    });
    port.postMessage({ t: "hello", session: link.session, title: document.title.slice(0, 300) });
    if (link.ports > 1) {
      // An init that cannot be replayed: its media is useless on the new
      // port until the next init (the service worker says initlost).
      for (const sb of link.unkept) port.postMessage({ t: "lost", header: { sb } });
      for (const chunks of link.inits.values()) {
        for (const { header, payload } of chunks) {
          port.postMessage({ t: "segment", header, payload: await toDataUrl(payload) });
        }
      }
    }
    return port;
  };

  const post = async (link, msg) => {
    const port = await ensurePort(link);
    if (!port) {
      stats.dropped++;
      return false;
    }
    try {
      port.postMessage(msg);
      stats.posted++;
      return true;
    } catch (e) {
      stats.dropped++;
      return false;
    }
  };

  // Remembered when it is handed on, in chain order: media still queued
  // behind it belongs to it, media before it to the previous one.
  // A continuation chunk joins the kept init; a new init replaces it. An init
  // too large to keep also drops the old one, so the old quality's init is
  // never replayed in front of the new quality's media; it is noted in
  // `unkept`.
  const noteUnkept = (link, sb) => {
    link.unkept.delete(sb);
    link.unkept.add(sb);
    while (link.unkept.size > MAX_UNKEPT) link.unkept.delete(link.unkept.values().next().value);
  };

  const keepInit = (link, header, payload) => {
    if (header.initCont) {
      const chunks = link.inits.get(header.sb);
      if (!chunks) return;
      const kept = chunks.reduce((n, c) => n + c.payload.byteLength, 0);
      if (kept + payload.byteLength > MAX_INIT_BYTES) {
        link.inits.delete(header.sb);
        noteUnkept(link, header.sb);
      } else chunks.push({ header, payload });
      return;
    }
    if (!header.isInit) return;
    link.inits.delete(header.sb);
    noteUnkept(link, header.sb);
    // An init+media append (video.js VHS) keeps only its init part.
    let init = { header, payload };
    if (header.initLen && header.initLen < payload.byteLength) {
      init = {
        header: { ...header, tfdt: null, track: null, dur: null, initLen: null },
        payload: payload.slice(0, header.initLen),
      };
    }
    if (init.payload.byteLength > MAX_INIT_BYTES) return;
    link.inits.set(header.sb, [init]);
    link.unkept.delete(header.sb);
    while (link.inits.size > MAX_KEPT_INITS) {
      const oldest = link.inits.keys().next().value;
      link.inits.delete(oldest);
      noteUnkept(link, oldest);
    }
  };

  // Init bytes of sb did not reach the port (in chain order): media after
  // them would be filed under the previous init. The kept init is stale, a
  // new port is told (`unkept`), the service worker is told now (`lost` with
  // drop: it says initlost and drops its kept init), and this SourceBuffer's
  // media waits for its next init.
  const initLost = (link, sb) => {
    link.inits.delete(sb);
    noteUnkept(link, sb);
    link.lost.add(sb);
    if (!link.port) return;
    try {
      link.port.postMessage({ t: "lost", header: { sb, drop: true } });
    } catch (e) {
      // The port is going: the next one is told (`unkept`).
    }
  };

  // `payload` null: an init held before tapping started that did not fit
  // (tap), noted at its place in the stream.
  const sendSegment = (header, payload) => {
    const link = linkFor(header.ms);
    if (!link || link.closed || stats.dead) {
      if (payload) stats.dropped++;
      return;
    }
    const initBytes = header.isInit || header.initCont;
    if (!payload) {
      link.chain = link.chain.then(() => initLost(link, header.sb)).catch(() => {});
      return;
    }
    let size = payload.byteLength;
    // A full queue drops media, not small init bytes: media after a lost init
    // would be filed under the previous one.
    const room = (n, init) => stats.queuedBytes + n <= (init && n <= MAX_INIT_BYTES ? 2 * MAX_QUEUED_BYTES : MAX_QUEUED_BYTES);
    if (!room(size, initBytes) && header.isInit && header.initLen && header.initLen < size && room(header.initLen, true)) {
      // An init+media append: its init part alone still goes, its media part
      // is dropped like any media.
      stats.dropped++;
      payload = payload.slice(0, header.initLen);
      header = { ...header, tfdt: null, track: null, dur: null, initLen: null };
      size = payload.byteLength;
    } else if (!room(size, initBytes)) {
      stats.dropped++;
      if (initBytes) link.chain = link.chain.then(() => initLost(link, header.sb)).catch(() => {});
      return;
    }
    stats.queuedBytes += size;
    link.chain = link.chain
      .then(async () => {
        if (header.isInit) link.lost.delete(header.sb);
        else if (link.lost.has(header.sb)) {
          // Media (or the rest) of a lost init.
          stats.dropped++;
          return;
        }
        const sent = await post(link, { t: "segment", header, payload: await toDataUrl(payload) });
        if (sent) keepInit(link, header, payload);
        else if (initBytes) initLost(link, header.sb);
      })
      .catch(() => {
        stats.dropped++;
        if (initBytes) initLost(link, header.sb);
      })
      .finally(() => {
        stats.queuedBytes -= size;
      });
  };

  const sendEvent = (msId, header) => {
    const link = links.get(msId == null ? "pre-hook" : String(msId));
    if (!link || link.closed || stats.dead) return;
    link.chain = link.chain.then(() => post(link, { t: "event", header })).catch(() => {});
  };

  // The MediaSource closed: drain its queue, then end its sink session.
  const closeLink = (msId) => {
    const key = msId == null ? "pre-hook" : String(msId);
    const link = links.get(key);
    if (!link) return;
    link.closed = true;
    link.chain = link.chain
      .then(() => {
        if (link.port) link.port.disconnect();
        link.port = null;
        links.delete(key);
      })
      .catch(() => {});
  };

  // ---------------------------------------------------------------------------
  // Tapping on/off: only with a configured sink (token set). A browser paired
  // with the desktop app is in mode "selected": it taps only the tabs the app
  // switched on, which the service worker answers (vc-tap-state) and announces
  // (vc-tap). A manual setup in the popup taps every tab ("all").
  // ---------------------------------------------------------------------------

  // null until chrome.storage answers: appends are held (bounded) meanwhile,
  // so the first init segment is not lost while the setting is read.
  let tapping = inExtension ? null : false;
  let held = [];
  let heldBytes = 0;
  const HELD_MARK_BYTES = 64; // what a held lost-init mark counts

  const setTapping = (on) => {
    tapping = on;
    // Tell the hook whether to copy segment bytes at all.
    window.postMessage({ __vcCtl: 1, tap: on }, "*");
    const waiting = held;
    held = [];
    heldBytes = 0;
    if (on) for (const [header, payload] of waiting) sendSegment(header, payload);
  };

  // Reads the setting and, in mode "selected", asks the service worker; a
  // later answer replaces an earlier one only if it is the latest question.
  // With the source policy on (source-policy.js), the service worker also
  // judges the site: a locked group is not tapped, and no answer means no tap.
  let question = 0;
  // Whether the policy is on, from the latest read; null until the first one
  // answered (then the service worker is asked, which knows).
  let sourcePolicyOn = null;
  const withPolicy = (on, answer) => {
    if (!on || sourcePolicyOn === false) return answer(on);
    chrome.runtime.sendMessage({ kind: "vc-source-state" }, (r) => {
      void chrome.runtime.lastError;
      answer(!!r && r.tap === true);
    });
  };
  const readTapping = () => {
    const mine = ++question;
    const answer = (on) => {
      if (mine === question) setTapping(on);
    };
    chrome.storage.local.get({ sinkToken: "", tapMode: "all", sourcePolicy: "" }, (v) => {
      sourcePolicyOn = v.sourcePolicy === "on";
      if (!v.sinkToken) return answer(false);
      if (v.tapMode !== "selected") return withPolicy(true, answer);
      chrome.runtime.sendMessage({ kind: "vc-tap-state" }, (r) => {
        void chrome.runtime.lastError;
        withPolicy(!!r && r.on === true, answer);
      });
    });
  };
  const SOURCE_KEYS = ["sourcePolicy", "sourceUnlocked", "sourceOwnHosts", "sourceLearningHosts"];
  if (inExtension) {
    try {
      readTapping();
      chrome.storage.onChanged.addListener((changes, area) => {
        if (area === "local" && (changes.sinkToken || changes.tapMode || SOURCE_KEYS.some((k) => changes[k]))) readTapping();
      });
      // The app switched this tab on or off.
      chrome.runtime.onMessage.addListener((msg, sender) => {
        // Switching on goes through the source policy like any other start.
        if (sender.id === chrome.runtime.id && msg && msg.kind === "vc-tap" && typeof msg.on === "boolean") {
          const mine = ++question;
          withPolicy(msg.on, (on) => {
            if (mine === question) setTapping(on);
          });
        }
        return false;
      });
    } catch (e) {
      setTapping(false);
    }
  }

  const tap = (header, payload) => {
    if (tapping === true) {
      sendSegment(header, payload);
    } else if (tapping === null && heldBytes + payload.byteLength <= MAX_QUEUED_BYTES) {
      held.push([header, payload]);
      heldBytes += payload.byteLength;
    } else if (tapping === null) {
      stats.dropped++;
      // A dropped init is noted at its place once tapping starts; the marks
      // have a small budget of their own.
      if ((header.isInit || header.initCont) && heldBytes + HELD_MARK_BYTES <= MAX_QUEUED_BYTES + MAX_INIT_BYTES) {
        held.push([header, null]);
        heldBytes += HELD_MARK_BYTES;
      }
    }
  };

  // ---------------------------------------------------------------------------
  // Events from hook.js
  // ---------------------------------------------------------------------------

  window.addEventListener(
    "message",
    (event) => {
      if (event.source !== window) return;
      const data = event.data;
      if (!data || data.__vc !== 1 || typeof data.type !== "string") return;

      forward(cleanRecord(data));
      if (data.type === "append") {
        const payload = data.payload;
        if (!(payload instanceof ArrayBuffer) || data.encrypted === true) return;
        const header = segmentHeader(data);
        if (!header) {
          stats.dropped++;
          return;
        }
        tap(header, payload);
        return;
      }
      if (tapping !== true) return;
      if (data.type === "endofstream" || data.type === "changetype") {
        const sb = data.type === "changetype" ? optId(data.sbId) : null;
        sendEvent(optId(data.msId), { name: data.type, sb });
      } else if (data.type === "msclose") {
        closeLink(optId(data.msId));
      }
    },
    false
  );

  // Counters for test injection.
  window.__vcLink = { links, stats };

  forward({ type: "frame", frameUrl: location.href.slice(0, 2048) });
  console.debug(TAG, "ready in", location.href, inExtension ? "(extension)" : "(injected)");
})();
