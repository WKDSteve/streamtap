// Main-world hook. Runs in the page's own JS context before any player code
// (run_at: document_start, world: MAIN), patches the MSE prototypes and reports
// every append as a small metadata record. Spike 1 never copies or stores the
// media bytes; it only parses enough of each segment to say what it was.
//
// Reporting channel: window.postMessage({ __vc: 1, ... }) -> content.js.
// Console channel: console.debug("[vc-hook] ...") in the page console.
(() => {
  "use strict";

  if (window.__vcHookInstalled) return;
  Object.defineProperty(window, "__vcHookInstalled", { value: true });

  const TAG = "[vc-hook]";
  const log = (...args) => console.debug(TAG, ...args);

  // ---------------------------------------------------------------------------
  // Identity bookkeeping
  // ---------------------------------------------------------------------------

  let nextMsId = 1;
  let nextSbId = 1;

  // Whether segment bytes are copied at all. content.js turns this off when
  // no sink is configured (metadata-only mode). On by default so the first
  // init segment is not lost while the setting is still being read. A page
  // can forge this message too; it can only switch off its own capture or
  // cost itself a copy.
  let tapping = true;
  window.addEventListener("message", (event) => {
    if (event.source !== window) return;
    const d = event.data;
    if (d && d.__vcCtl === 1 && typeof d.tap === "boolean") tapping = d.tap;
  });
  const msInfo = new WeakMap(); // MediaSource -> { id, blobUrl }
  const sbInfo = new WeakMap(); // SourceBuffer -> { id, msId, mime, timescale, seq }

  // `transfer` moves ArrayBuffers to the receiver instead of cloning them.
  const post = (type, payload, transfer) => {
    try {
      window.postMessage({ __vc: 1, type, frameUrl: location.href, ...payload }, "*", transfer || []);
    } catch (e) {
      // A detached frame or a serialization problem must never break the player.
    }
  };

  // ---------------------------------------------------------------------------
  // ISO BMFF (fMP4) parsing: just enough for box types, tfdt and mdhd timescale
  // ---------------------------------------------------------------------------

  const ascii = (u8, off) =>
    String.fromCharCode(u8[off], u8[off + 1], u8[off + 2], u8[off + 3]);

  const u32 = (dv, off) => dv.getUint32(off);
  const u64 = (dv, off) => Number(dv.getBigUint64(off));
  // A 64-bit decode time: a Number while exact, a decimal string above 2^53.
  const u64Exact = (dv, off) => {
    const v = dv.getBigUint64(off);
    return v <= BigInt(Number.MAX_SAFE_INTEGER) ? Number(v) : v.toString();
  };

  // Iterate boxes in [start, end), calling fn(type, headerSize, bodyStart, bodyEnd).
  // A box that runs past `end` stops the walk; onOpen(type, missingBytes) is
  // told about it (an init segment split across appends).
  const eachBox = (u8, dv, start, end, fn, onOpen) => {
    let off = start;
    while (off + 8 <= end) {
      let size = u32(dv, off);
      const type = ascii(u8, off + 4);
      let header = 8;
      if (size === 1) {
        if (off + 16 > end) break;
        size = u64(dv, off + 8);
        header = 16;
      } else if (size === 0) {
        size = end - off;
      }
      if (size >= header && off + size > end && onOpen) onOpen(type, off + size - end);
      if (size < header || off + size > end) break;
      fn(type, header, off + header, off + size);
      off += size;
    }
  };

  const CONTAINERS = new Set(["moov", "trak", "mdia", "moof", "traf", "mvex"]);

  const ENCRYPTION_FOURCCS = ["encv", "enca", "tenc", "pssh"].map((s) => [...s].map((c) => c.charCodeAt(0)));

  // True when one of the byte patterns occurs in u8[start, end).
  const hasAnyFourcc = (u8, start, end, patterns) => {
    for (let i = start; i + 4 <= end; i++) {
      for (const p of patterns) {
        if (u8[i] === p[0] && u8[i + 1] === p[1] && u8[i + 2] === p[2] && u8[i + 3] === p[3]) return true;
      }
    }
    return false;
  };

  // Returns { boxes: [...top-level types], isInit, tfdt, timescale, samples, duration }
  const parseIsoBmff = (u8) => {
    const dv = new DataView(u8.buffer, u8.byteOffset, u8.byteLength);
    const out = { boxes: [], isInit: false, tfdt: null, timescale: null, samples: 0, duration: 0, encrypted: false };

    const walk = (start, end, depth) => {
      eachBox(u8, dv, start, end, (type, header, bodyStart, bodyEnd) => {
        if (depth === 0) out.boxes.push(type);
        if (type === "moov" || type === "ftyp") out.isInit = true;
        if (type === "moov") out.moov = [bodyStart, bodyEnd];
        if (depth === 0 && (type === "moof" || type === "mdat")) out.media = true;
        // Protection markers: a pssh box anywhere, sample encryption in traf.
        if (type === "pssh" || type === "senc") out.encrypted = true;

        if (type === "mdhd" && out.timescale === null) {
          const version = u8[bodyStart];
          // fullbox(4) + creation + modification (4+4 or 8+8) -> timescale
          out.timescale = u32(dv, bodyStart + (version === 1 ? 20 : 12));
        } else if (type === "tfdt") {
          // The first fragment's decode time is the append's start; later
          // fragments (several moofs, or a second track) must not replace it.
          if (out.tfdt === null) {
            const version = u8[bodyStart];
            out.tfdt = version === 1 ? u64Exact(dv, bodyStart + 4) : u32(dv, bodyStart + 4);
          }
        } else if (type === "tfhd") {
          const flags = u32(dv, bodyStart) & 0xffffff;
          out._track = u32(dv, bodyStart + 4);
          if (out._firstTrack === undefined) out._firstTrack = out._track;
          let p = bodyStart + 8; // fullbox + track_ID
          if (flags & 0x000001) p += 8; // base_data_offset
          if (flags & 0x000002) p += 4; // sample_description_index
          out._defaultDuration = flags & 0x000008 ? u32(dv, p) : null;
        } else if (type === "trun" && out._track !== out._firstTrack) {
          // Duration counts the first track only (one timescale).
        } else if (type === "trun") {
          const flags = u32(dv, bodyStart) & 0xffffff;
          const count = u32(dv, bodyStart + 4);
          let p = bodyStart + 8;
          if (flags & 0x000001) p += 4; // data_offset
          if (flags & 0x000004) p += 4; // first_sample_flags
          const perSample =
            (flags & 0x000100 ? 4 : 0) +
            (flags & 0x000200 ? 4 : 0) +
            (flags & 0x000400 ? 4 : 0) +
            (flags & 0x000800 ? 4 : 0);
          out.samples += count;
          if (flags & 0x000100) {
            for (let i = 0; i < count && p + perSample <= bodyEnd; i++, p += perSample) {
              out.duration += u32(dv, p);
            }
          } else if (out._defaultDuration) {
            out.duration += count * out._defaultDuration;
          }
        }

        if (CONTAINERS.has(type)) walk(bodyStart, bodyEnd, depth + 1);
      });
    };

    try {
      walk(0, u8.byteLength, 0);
    } catch (e) {
      out.error = String(e);
    }
    // Encrypted sample entries (encv/enca) and their tenc live deep inside
    // moov/trak/mdia/minf/stbl/stsd; the init segment is small, so scan it.
    if (out.moov && !out.encrypted) {
      out.encrypted = hasAnyFourcc(u8, out.moov[0], out.moov[1], ENCRYPTION_FOURCCS);
    }
    out.moovComplete = !!out.moov;
    delete out._defaultDuration;
    delete out._track;
    delete out._firstTrack;
    delete out.moov;
    return out;
  };

  // ---------------------------------------------------------------------------
  // WebM / Matroska parsing: EBML header vs Cluster, cluster timecode
  // ---------------------------------------------------------------------------

  const readVint = (u8, off, keepMarker) => {
    const first = u8[off];
    if (first === undefined) return null;
    let len = 1;
    let mask = 0x80;
    while (len <= 8 && !(first & mask)) {
      mask >>= 1;
      len++;
    }
    if (len > 8) return null;
    let value = keepMarker ? first : first & (mask - 1);
    for (let i = 1; i < len; i++) value = value * 256 + u8[off + i];
    return { value, len };
  };

  // Calls fn(id, bodyStart, bodyEnd) for each EBML child in [start, end).
  const ebmlChildren = (u8, start, end, fn) => {
    for (let p = start; p < end; ) {
      const id = readVint(u8, p, true);
      const size = id && readVint(u8, p + id.len, false);
      if (!id || !size) return;
      const s = p + id.len + size.len;
      const unknown = size.value === Math.pow(2, 7 * size.len) - 1;
      const e = unknown ? end : Math.min(end, s + size.value);
      fn(id.value, s, e);
      if (unknown || s + size.value <= p) return;
      p = s + size.value;
    }
  };

  // True when the two-byte sequence (a, b) occurs in u8[start, end).
  const hasPair = (u8, start, end, a, b) => {
    const stop = Math.min(end, u8.byteLength) - 1;
    for (let i = start; i < stop; i++) if (u8[i] === a && u8[i + 1] === b) return true;
    return false;
  };

  const parseWebm = (u8) => {
    const out = { boxes: [], isInit: false, tfdt: null, timescale: null, samples: 0, duration: 0, encrypted: false };
    let off = 0;
    while (off < u8.byteLength) {
      const id = readVint(u8, off, true);
      if (!id) break;
      const size = readVint(u8, off + id.len, false);
      if (!size) break;
      const bodyStart = off + id.len + size.len;
      const unknownSize = size.value === Math.pow(2, 7 * size.len) - 1;
      const bodyEnd = unknownSize ? u8.byteLength : Math.min(u8.byteLength, bodyStart + size.value);

      if (id.value === 0x1a45dfa3) {
        out.boxes.push("EBML");
        out.isInit = true;
      } else if (id.value === 0x18538067) {
        out.boxes.push("Segment");
        out.isInit = true;
        // Segment is the container for Info/Tracks (init) and Clusters (media).
        // With unknown size we must descend instead of skipping to the end.
        off = bodyStart;
        continue;
      } else if (id.value === 0x1f43b675) {
        out.boxes.push("Cluster");
        out.media = true;
        // First child is normally Timecode (0xE7).
        const tc = readVint(u8, bodyStart, true);
        if (tc && tc.value === 0xe7) {
          const tcSize = readVint(u8, bodyStart + tc.len, false);
          const valueStart = tcSize ? bodyStart + tc.len + tcSize.len : 0;
          // An unsigned integer element has at most 8 bytes; anything else is
          // corrupt (and a loop over a huge size would freeze the page).
          if (tcSize && tcSize.value >= 1 && tcSize.value <= 8 && valueStart + tcSize.value <= u8.byteLength) {
            let v = 0;
            for (let i = 0; i < tcSize.value; i++) v = v * 256 + u8[valueStart + i];
            if (out.tfdt === null) out.tfdt = v;
          }
        }
      } else if (id.value === 0x1654ae6b) {
        out.boxes.push("Tracks");
        out.isInit = true;
        // TrackEntry > ContentEncodings > ContentEncoding > ContentEncryption:
        // WebM encryption (Widevine/ClearKey on WebM), read as structure, not
        // guessed from byte pairs.
        ebmlChildren(u8, bodyStart, bodyEnd, (tid, ts0, te0) => {
          if (tid !== 0xae) return;
          ebmlChildren(u8, ts0, te0, (cid, cs0, ce0) => {
            if (cid !== 0x6d80) return;
            ebmlChildren(u8, cs0, ce0, (eid, es0, ee0) => {
              if (eid !== 0x6240) return;
              ebmlChildren(u8, es0, ee0, (xid) => {
                if (xid === 0x5035) out.encrypted = true;
              });
            });
          });
        });
      } else if (id.value === 0x1549a966) {
        out.boxes.push("Info");
        out.isInit = true;
        // TimecodeScale (0x2AD7B1, nanoseconds per tick, default 1 ms):
        // Cluster timecodes count in it.
        for (let p = bodyStart; p < bodyEnd; ) {
          const cid = readVint(u8, p, true);
          const csize = cid && readVint(u8, p + cid.len, false);
          if (!cid || !csize) break;
          const cstart = p + cid.len + csize.len;
          if (cid.value === 0x2ad7b1 && csize.value >= 1 && csize.value <= 8 && cstart + csize.value <= u8.byteLength) {
            let ns = 0;
            for (let i = 0; i < csize.value; i++) ns = ns * 256 + u8[cstart + i];
            if (ns > 0) out.timescale = Math.max(1, Math.round(1e9 / ns));
          }
          p = cstart + csize.value;
        }
      } else if (id.value === 0x1c53bb6b) {
        out.boxes.push("Cues");
      }
      if (unknownSize) break;
      off = bodyEnd;
    }
    return out;
  };

  const sniff = (u8) => {
    if (u8.byteLength >= 8) {
      const t = ascii(u8, 4);
      if (/^[A-Za-z0-9 ]{4}$/.test(t)) return "mp4";
    }
    if (u8.byteLength >= 4) {
      const id = ((u8[0] << 24) | (u8[1] << 16) | (u8[2] << 8) | u8[3]) >>> 0;
      // EBML, Cluster, Segment, Tracks, Info (an init split across appends
      // can start with Tracks or Info).
      if ([0x1a45dfa3, 0x1f43b675, 0x18538067, 0x1654ae6b, 0x1549a966].includes(id)) return "webm";
    }
    if (u8[0] === 0x47 && u8[188] === 0x47) return "ts";
    return "unknown";
  };

  // ---------------------------------------------------------------------------
  // Init assembly: an init segment may arrive in several appends
  // ---------------------------------------------------------------------------

  const INIT_MAX = 4 * 1024 * 1024;
  const EBML_ID = 0x1a45dfa3;
  const SEGMENT_ID = 0x18538067;
  const CLUSTER_ID = 0x1f43b675;

  // Does this append start an init segment? MP4: a first box ftyp or moov
  // (also when the box itself is cut off); WebM: an EBML header or Segment.
  const startsInit = (u8, kind) => {
    if (kind === "mp4") return u8.byteLength >= 8 && ["ftyp", "moov"].includes(ascii(u8, 4));
    if (kind === "webm") {
      const id = ((u8[0] << 24) | (u8[1] << 16) | (u8[2] << 8) | u8[3]) >>> 0;
      return id === EBML_ID || id === SEGMENT_ID;
    }
    return false;
  };

  // Where the init part of these bytes ends and whether it is still open:
  // { end, open }. end is the offset of the first media (moof/mdat, Cluster),
  // or the length; open means the last box/element runs past the bytes.
  // An init is complete only with its moov (MP4) or Tracks (WebM): bytes that
  // end right after ftyp or Info are still an open init.
  const mp4InitEnd = (u8) => {
    const dv = new DataView(u8.buffer, u8.byteOffset, u8.byteLength);
    let off = 0;
    let sawMoov = false;
    while (off + 8 <= u8.byteLength) {
      let size = dv.getUint32(off);
      const type = ascii(u8, off + 4);
      let header = 8;
      if (size === 1) {
        if (off + 16 > u8.byteLength) return { end: u8.byteLength, open: true };
        size = Number(dv.getBigUint64(off + 8));
        header = 16;
      } else if (size === 0) {
        size = u8.byteLength - off;
      }
      if (type === "moof" || type === "mdat") return { end: off, open: false };
      if (size < header) return { end: off, open: false };
      if (off + size > u8.byteLength) return { end: u8.byteLength, open: true };
      if (type === "moov") sawMoov = true;
      off += size;
    }
    return { end: u8.byteLength, open: off < u8.byteLength || !sawMoov };
  };

  const webmInitEnd = (u8) => {
    let off = 0;
    let sawTracks = false;
    while (off < u8.byteLength) {
      const id = readVint(u8, off, true);
      if (!id || off + id.len > u8.byteLength) return { end: u8.byteLength, open: true };
      const size = readVint(u8, off + id.len, false);
      if (!size || off + id.len + size.len > u8.byteLength) return { end: u8.byteLength, open: true };
      if (id.value === CLUSTER_ID) return { end: off, open: false };
      const bodyStart = off + id.len + size.len;
      const unknown = size.value === Math.pow(2, 7 * size.len) - 1;
      if (id.value === SEGMENT_ID || unknown) {
        off = bodyStart; // descend: Info, Tracks ... then the first Cluster
        continue;
      }
      if (bodyStart + size.value > u8.byteLength) return { end: u8.byteLength, open: true };
      if (id.value === 0x1654ae6b) sawTracks = true;
      off = bodyStart + size.value;
    }
    return { end: u8.byteLength, open: !sawTracks };
  };

  const initEnd = (u8, kind) => (kind === "webm" ? webmInitEnd(u8) : mp4InitEnd(u8));

  // Protection markers in init bytes that cannot be parsed yet (still open).
  // MP4: the ISO fourccs. WebM: all three element IDs (ContentEncodings
  // 0x6D80, ContentEncryption 0x5035, ContentEncKeyID 0x47E2), which clear
  // init bytes practically never hold together. Only init bytes are scanned,
  // never media.
  const openInitEncrypted = (u8, kind) =>
    kind === "webm"
      ? hasPair(u8, 0, u8.byteLength, 0x6d, 0x80) &&
        hasPair(u8, 0, u8.byteLength, 0x50, 0x35) &&
        hasPair(u8, 0, u8.byteLength, 0x47, 0xe2)
      : hasAnyFourcc(u8, 0, u8.byteLength, ENCRYPTION_FOURCCS);

  // A complete init: protection and timescale from its structure.
  const finishInit = (info, initBytes, kind) => {
    const p = kind === "webm" ? parseWebm(initBytes) : parseIsoBmff(initBytes);
    if (p.encrypted) info.encrypted = true;
    if (p.timescale) info.timescale = p.timescale;
  };

  const concat = (a, b) => {
    const out = new Uint8Array(a.byteLength + b.byteLength);
    out.set(a, 0);
    out.set(b, a.byteLength);
    return out;
  };

  const toU8 = (data) => {
    if (data instanceof ArrayBuffer) return new Uint8Array(data);
    if (ArrayBuffer.isView(data)) return new Uint8Array(data.buffer, data.byteOffset, data.byteLength);
    return null;
  };

  const hex = (u8, n) =>
    Array.from(u8.subarray(0, n), (b) => b.toString(16).padStart(2, "0")).join(" ");

  // ---------------------------------------------------------------------------
  // Patches
  // ---------------------------------------------------------------------------

  const patch = (proto, name, make) => {
    const desc = Object.getOwnPropertyDescriptor(proto, name);
    if (!desc) return false;
    const replacement = make(desc);
    Object.defineProperty(proto, name, { ...desc, ...replacement });
    return true;
  };

  const ensureMs = (ms) => {
    let info = msInfo.get(ms);
    if (!info) {
      info = { id: nextMsId++, blobUrl: null };
      msInfo.set(ms, info);
      post("mediasource", { msId: info.id });
      log(`MediaSource #${info.id} created`);
      // A closed MediaSource is done: content.js ends its sink session.
      try {
        ms.addEventListener("sourceclose", () => post("msclose", { msId: info.id }));
      } catch (e) {
        // not an EventTarget in some test stand-ins
      }
    }
    return info;
  };

  // ---------------------------------------------------------------------------
  // EME in this frame: stop tapping altogether
  // ---------------------------------------------------------------------------

  // Real DRM playback needs MediaKeys on the media element, and the browser
  // fires "encrypted" when it meets protection data in appended media. Either
  // one switches this frame to metadata only for good, whatever the per-
  // segment markers say (an init split across appends, clear lead, cbcs).
  let emeActive = false;
  const markEme = (why) => {
    if (emeActive) return;
    emeActive = true;
    post("drm", { keySystem: why, robustness: [] });
    log(`EME active (${why}); not tapping this frame any more`);
  };
  if (typeof HTMLMediaElement !== "undefined" && HTMLMediaElement.prototype.setMediaKeys) {
    patch(HTMLMediaElement.prototype, "setMediaKeys", (d) => ({
      value: function (mediaKeys) {
        if (mediaKeys) markEme("setMediaKeys");
        return d.value.call(this, mediaKeys);
      },
    }));
  }
  if (typeof document !== "undefined" && document.addEventListener) {
    document.addEventListener("encrypted", () => markEme("encrypted event"), true);
  }

  if (typeof MediaSource !== "undefined") {
    patch(MediaSource.prototype, "addSourceBuffer", (d) => ({
      value: function (mime) {
        const sb = d.value.call(this, mime);
        const ms = ensureMs(this);
        const info = { id: nextSbId++, msId: ms.id, mime: String(mime), timescale: null, seq: 0 };
        sbInfo.set(sb, info);
        post("sourcebuffer", { sbId: info.id, msId: ms.id, mime: info.mime });
        log(`SourceBuffer #${info.id} on MediaSource #${ms.id}: ${info.mime}`);
        return sb;
      },
    }));

    patch(MediaSource.prototype, "endOfStream", (d) => ({
      value: function (...args) {
        const ms = msInfo.get(this);
        post("endofstream", { msId: ms ? ms.id : null, error: args[0] || null });
        log(`MediaSource #${ms ? ms.id : "?"} endOfStream(${args[0] || ""})`);
        return d.value.apply(this, args);
      },
    }));
  }

  const onAppend = (sb, data, api) => {
    const u8 = toU8(data);
    if (!u8) return;
    let info = sbInfo.get(sb);
    if (!info) {
      // Created before the hook was installed (late injection, or a player that
      // cached addSourceBuffer): register it now with what we can still read.
      info = { id: nextSbId++, msId: null, mime: "(pre-hook)", timescale: null, seq: 0 };
      sbInfo.set(sb, info);
      post("sourcebuffer", { sbId: info.id, msId: null, mime: info.mime });
      log(`SourceBuffer #${info.id} seen first in append (created before hook)`);
    }
    info.seq++;

    const kind = sniff(u8);
    const parsed = kind === "mp4" ? parseIsoBmff(u8) : kind === "webm" ? parseWebm(u8) : null;
    if (parsed && parsed.timescale) info.timescale = parsed.timescale;

    // DRM: protection marks the SourceBuffer as protected for good; its bytes
    // are never copied (streamtap detects DRM and leaves it alone).
    // - media: pssh / senc in a moof (parsed);
    // - init: read from its structure once complete (encv/enca/tenc/pssh in
    //   the moov, ContentEncryption in WebM Tracks). An init split across
    //   appends, anywhere (even inside a box header), is assembled: from an
    //   append that starts an init until the init's end, appends that do not
    //   start a new init are its continuation (initCont). While it is still
    //   open, its bytes (init bytes only) are scanned for the markers.
    let encryptedHere = !!(parsed && parsed.encrypted);
    let initCont = false;
    let isInit = false;
    let initLen = null; // set when init and media share this append
    if (startsInit(u8, kind)) {
      isInit = true;
      const r = initEnd(u8, kind);
      if (r.open && r.end === u8.byteLength) {
        info.initBuf = u8.byteLength <= INIT_MAX ? { kind, bytes: u8.slice() } : null;
        if (openInitEncrypted(u8, kind)) encryptedHere = true;
      } else {
        info.initBuf = null;
        finishInit(info, u8.subarray(0, r.end), kind);
        if (r.end < u8.byteLength) initLen = r.end;
      }
    } else if (info.initBuf && info.initBuf.kind === null) {
      // Fewer than 8 bytes came first: only now can they be told apart.
      const joined = concat(info.initBuf.bytes, u8);
      const k = sniff(joined);
      info.initBuf = null;
      if (startsInit(joined, k)) {
        initCont = true;
        const r = initEnd(joined, k);
        if (r.open && r.end === joined.byteLength && joined.byteLength <= INIT_MAX) {
          info.initBuf = { kind: k, bytes: joined };
          if (openInitEncrypted(joined, k)) encryptedHere = true;
        } else {
          finishInit(info, joined.subarray(0, r.end), k);
        }
      }
    } else if (!info.initBuf && kind === "unknown" && u8.byteLength < 8) {
      info.initBuf = { kind: null, bytes: u8.slice() };
    } else if (info.initBuf) {
      initCont = true;
      const joined = concat(info.initBuf.bytes, u8);
      const r = initEnd(joined, info.initBuf.kind);
      if (r.open && r.end === joined.byteLength && joined.byteLength <= INIT_MAX) {
        info.initBuf.bytes = joined;
        if (openInitEncrypted(joined, info.initBuf.kind)) encryptedHere = true;
      } else {
        finishInit(info, joined.subarray(0, r.end), info.initBuf.kind);
        info.initBuf = null;
      }
    }
    if (encryptedHere) info.encrypted = true;
    if (info.encrypted && !info.encryptedReported) {
      info.encryptedReported = true;
      post("encrypted", { sbId: info.id, msId: info.msId, mime: info.mime });
      log(`SourceBuffer #${info.id} carries encrypted media (DRM); not tapping it`);
    }

    let tso = null;
    let seqMode = false;
    try {
      tso = Number(sb.timestampOffset);
      if (!Number.isFinite(tso)) tso = null;
      seqMode = sb.mode === "sequence";
    } catch (e) {
      tso = null;
    }

    // WebM Cluster timecodes count in the init's TimecodeScale (1 ms when the
    // init did not say otherwise).
    const ts = info.timescale ?? (kind === "webm" ? 1000 : null);
    const tfdtNum = parsed && parsed.tfdt !== null ? Number(parsed.tfdt) : null;
    const rec = {
      sbId: info.id,
      msId: info.msId,
      mime: info.mime,
      seq: info.seq,
      api,
      bytes: u8.byteLength,
      kind,
      isInit,
      initCont,
      initLen,
      boxes: parsed ? parsed.boxes : [],
      tfdt: parsed ? parsed.tfdt : null,
      timescale: ts,
      dur: parsed && parsed.duration ? parsed.duration : null,
      tso,
      seqMode,
      encrypted: !!info.encrypted || emeActive,
      startSec: tfdtNum !== null && ts ? +(tfdtNum / ts).toFixed(3) : null,
      durationSec: parsed && parsed.duration && ts ? +(parsed.duration / ts).toFixed(3) : null,
      samples: parsed ? parsed.samples : 0,
      head: hex(u8, 12),
    };
    // One copy (the player keeps using the original), then hand the copy over
    // by transfer so no second copy is made on the way to the content script.
    // Protected media and frames whose content script does not tap get
    // metadata only.
    if (info.encrypted || emeActive || !tapping) {
      post("append", rec);
    } else {
      const copy = u8.slice().buffer;
      post("append", { ...rec, payload: copy }, [copy]);
    }

    const where =
      rec.isInit ? "INIT" : rec.startSec !== null ? `t=${rec.startSec}s` : "t=?";
    const dur = rec.durationSec ? ` +${rec.durationSec}s` : "";
    log(
      `#${rec.sbId} seq ${rec.seq} ${where}${dur} ${rec.bytes} B ${rec.kind} [${rec.boxes.join(" ")}] ${rec.mime}`
    );
  };

  if (typeof SourceBuffer !== "undefined") {
    // The native append runs first: one the browser rejects synchronously
    // (QuotaExceededError, InvalidStateError) throws before anything is
    // recorded, so a player's retry is not stored twice. appendBuffer copies
    // the bytes synchronously and segment parsing starts asynchronously, so
    // the data and timestampOffset read right after the call are still the
    // ones of this append.
    patch(SourceBuffer.prototype, "appendBuffer", (d) => ({
      value: function (data) {
        const result = d.value.call(this, data);
        try {
          onAppend(this, data, "appendBuffer");
        } catch (e) {
          log("hook error", e);
        }
        return result;
      },
    }));

    // Firefox-only async variant.
    patch(SourceBuffer.prototype, "appendBufferAsync", (d) => ({
      value: function (data) {
        const result = d.value.call(this, data);
        try {
          onAppend(this, data, "appendBufferAsync");
        } catch (e) {
          log("hook error", e);
        }
        return result;
      },
    }));

    patch(SourceBuffer.prototype, "changeType", (d) => ({
      value: function (mime) {
        const info = sbInfo.get(this);
        if (info) {
          log(`SourceBuffer #${info.id} changeType ${info.mime} -> ${mime}`);
          info.mime = String(mime);
          info.timescale = null;
          post("changetype", { sbId: info.id, msId: info.msId, mime: info.mime });
        }
        return d.value.call(this, mime);
      },
    }));

    patch(SourceBuffer.prototype, "remove", (d) => ({
      value: function (start, end) {
        const info = sbInfo.get(this);
        if (info) post("remove", { sbId: info.id, start, end });
        return d.value.call(this, start, end);
      },
    }));

    patch(SourceBuffer.prototype, "timestampOffset", (d) => ({
      set: function (v) {
        const info = sbInfo.get(this);
        if (info) {
          log(`SourceBuffer #${info.id} timestampOffset = ${v}`);
          post("timestampoffset", { sbId: info.id, value: v });
        }
        return d.set.call(this, v);
      },
    }));
  }

  // blob: URL -> MediaSource, so a <video src="blob:..."> can be attributed later.
  patch(URL, "createObjectURL", (d) => ({
    value: function (obj) {
      const url = d.value.call(this, obj);
      if (typeof MediaSource !== "undefined" && obj instanceof MediaSource) {
        const ms = ensureMs(obj);
        ms.blobUrl = url;
        post("objecturl", { msId: ms.id, url });
        log(`MediaSource #${ms.id} -> ${url}`);
      }
      return url;
    },
  }));

  // DRM detector. Observed only, never altered.
  if (navigator.requestMediaKeySystemAccess) {
    patch(Navigator.prototype, "requestMediaKeySystemAccess", (d) => ({
      value: function (keySystem, configs) {
        let robustness = [];
        try {
          robustness = (configs || []).flatMap((c) =>
            [...(c.videoCapabilities || []), ...(c.audioCapabilities || [])]
              .map((cap) => cap.robustness)
              .filter(Boolean)
          );
        } catch (e) {
          // ignore malformed configs, the browser will reject them anyway
        }
        post("drm", { keySystem: String(keySystem), robustness: [...new Set(robustness)] });
        log(`DRM requested: ${keySystem}`, robustness);
        return d.value.call(this, keySystem, configs);
      },
    }));
  }

  log("installed in", location.href);
})();
