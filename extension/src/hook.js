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
  const msInfo = new WeakMap(); // MediaSource -> { id, blobUrl }
  const sbInfo = new WeakMap(); // SourceBuffer -> { id, msId, mime, timescale, seq }

  const post = (type, payload) => {
    try {
      window.postMessage({ __vc: 1, type, frameUrl: location.href, ...payload }, "*");
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

  // Iterate boxes in [start, end), calling fn(type, headerSize, bodyStart, bodyEnd).
  const eachBox = (u8, dv, start, end, fn) => {
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
      if (size < header || off + size > end) break;
      fn(type, header, off + header, off + size);
      off += size;
    }
  };

  const CONTAINERS = new Set(["moov", "trak", "mdia", "moof", "traf", "mvex"]);

  // Returns { boxes: [...top-level types], isInit, tfdt, timescale, samples, duration }
  const parseIsoBmff = (u8) => {
    const dv = new DataView(u8.buffer, u8.byteOffset, u8.byteLength);
    const out = { boxes: [], isInit: false, tfdt: null, timescale: null, samples: 0, duration: 0 };

    const walk = (start, end, depth) => {
      eachBox(u8, dv, start, end, (type, header, bodyStart, bodyEnd) => {
        if (depth === 0) out.boxes.push(type);
        if (type === "moov" || type === "ftyp") out.isInit = true;

        if (type === "mdhd" && out.timescale === null) {
          const version = u8[bodyStart];
          // fullbox(4) + creation + modification (4+4 or 8+8) -> timescale
          out.timescale = u32(dv, bodyStart + (version === 1 ? 20 : 12));
        } else if (type === "tfdt") {
          const version = u8[bodyStart];
          out.tfdt = version === 1 ? u64(dv, bodyStart + 4) : u32(dv, bodyStart + 4);
        } else if (type === "tfhd") {
          const flags = u32(dv, bodyStart) & 0xffffff;
          let p = bodyStart + 8; // fullbox + track_ID
          if (flags & 0x000001) p += 8; // base_data_offset
          if (flags & 0x000002) p += 4; // sample_description_index
          out._defaultDuration = flags & 0x000008 ? u32(dv, p) : null;
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
    delete out._defaultDuration;
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

  const parseWebm = (u8) => {
    const out = { boxes: [], isInit: false, tfdt: null, timescale: 1000, samples: 0, duration: 0 };
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
        // First child is normally Timecode (0xE7).
        const tc = readVint(u8, bodyStart, true);
        if (tc && tc.value === 0xe7) {
          const tcSize = readVint(u8, bodyStart + tc.len, false);
          if (tcSize) {
            let v = 0;
            for (let i = 0; i < tcSize.value; i++) v = v * 256 + u8[bodyStart + tc.len + tcSize.len + i];
            if (out.tfdt === null) out.tfdt = v;
          }
        }
      } else if (id.value === 0x1654ae6b) {
        out.boxes.push("Tracks");
        out.isInit = true;
      } else if (id.value === 0x1549a966) {
        out.boxes.push("Info");
        out.isInit = true;
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
      if (id === 0x1a45dfa3 || id === 0x1f43b675 || id === 0x18538067) return "webm";
    }
    if (u8[0] === 0x47 && u8[188] === 0x47) return "ts";
    return "unknown";
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
    }
    return info;
  };

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

    const ts = info.timescale;
    const rec = {
      sbId: info.id,
      msId: info.msId,
      mime: info.mime,
      seq: info.seq,
      api,
      bytes: u8.byteLength,
      kind,
      isInit: parsed ? parsed.isInit : false,
      boxes: parsed ? parsed.boxes : [],
      tfdt: parsed ? parsed.tfdt : null,
      timescale: ts,
      startSec: parsed && parsed.tfdt !== null && ts ? +(parsed.tfdt / ts).toFixed(3) : null,
      durationSec: parsed && parsed.duration && ts ? +(parsed.duration / ts).toFixed(3) : null,
      samples: parsed ? parsed.samples : 0,
      head: hex(u8, 12),
    };
    post("append", rec);

    const where =
      rec.isInit ? "INIT" : rec.startSec !== null ? `t=${rec.startSec}s` : "t=?";
    const dur = rec.durationSec ? ` +${rec.durationSec}s` : "";
    log(
      `#${rec.sbId} seq ${rec.seq} ${where}${dur} ${rec.bytes} B ${rec.kind} [${rec.boxes.join(" ")}] ${rec.mime}`
    );
  };

  if (typeof SourceBuffer !== "undefined") {
    patch(SourceBuffer.prototype, "appendBuffer", (d) => ({
      value: function (data) {
        try {
          onAppend(this, data, "appendBuffer");
        } catch (e) {
          log("hook error", e);
        }
        return d.value.call(this, data);
      },
    }));

    // Firefox-only async variant.
    patch(SourceBuffer.prototype, "appendBufferAsync", (d) => ({
      value: function (data) {
        try {
          onAppend(this, data, "appendBufferAsync");
        } catch (e) {
          log("hook error", e);
        }
        return d.value.call(this, data);
      },
    }));

    patch(SourceBuffer.prototype, "changeType", (d) => ({
      value: function (mime) {
        const info = sbInfo.get(this);
        if (info) {
          log(`SourceBuffer #${info.id} changeType ${info.mime} -> ${mime}`);
          info.mime = String(mime);
          info.timescale = null;
          post("changetype", { sbId: info.id, mime: info.mime });
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
