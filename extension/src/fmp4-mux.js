// The target "a video on this device": the messages a recording holds (file-target.js) made into one playable MP4
// on the phone, without re-encoding. The segments a player appends to MSE are fragmented MP4 already; this joins the
// tracks of every SourceBuffer under one `moov` (track IDs renumbered, one `trex` each) and writes their `moof`+`mdat`
// pairs in decode order, numbered anew. The output is a fragmented MP4 (ftyp, moov, moof+mdat...), which browsers and
// players play as it is.
//
// It takes the common case and says plainly when a capture is not one (MOB-09): WebM, a quality switch (a second,
// different init on a SourceBuffer), sequence mode, a timestampOffset that changes, protection (DRM), a lost init, a
// fragment with absolute data offsets. Then nothing is made here, and the recording goes to the PC instead, where the
// sink's code handles all of it (`streamtap-sink replay`, `mux`).
//
// Within the common case it is honest about what it holds: a fragment appended again after a seek (same track, same
// decode time) is kept once; one that overlaps the one before is dropped; a gap between fragments (more than 50 ms or
// 1 % of the fragment's length) is reported with its length.
"use strict";

const MUX_MAX_TRACKS = 8;
const MUX_MAX_INIT = 1024 * 1024;

const muxType = (u8, off) => String.fromCharCode(u8[off + 4], u8[off + 5], u8[off + 6], u8[off + 7]);

// The boxes in u8[start..end): { type, start, body, end }; `complete` is false when the last one runs past `end`.
const muxBoxes = (u8, start, end) => {
  const dv = new DataView(u8.buffer, u8.byteOffset, u8.byteLength);
  const out = [];
  let off = start;
  while (off + 8 <= end) {
    let size = dv.getUint32(off);
    let head = 8;
    if (size === 1) {
      if (off + 16 > end) return { boxes: out, complete: false, rest: off };
      size = Number(dv.getBigUint64(off + 8));
      head = 16;
    } else if (size === 0) {
      size = end - off;
    }
    if (size < head) return { boxes: out, complete: false, bad: true, rest: off };
    if (off + size > end) return { boxes: out, complete: false, rest: off };
    out.push({ type: muxType(u8, off), start: off, body: off + head, end: off + size });
    off += size;
  }
  return { boxes: out, complete: off === end, rest: off };
};

const muxChild = (u8, box, type) => muxBoxes(u8, box.body, box.end).boxes.find((b) => b.type === type) || null;
const muxChildren = (u8, box, type) => muxBoxes(u8, box.body, box.end).boxes.filter((b) => b.type === type);

const muxConcat = (parts) => {
  const n = parts.reduce((a, p) => a + p.byteLength, 0);
  const out = new Uint8Array(n);
  let off = 0;
  for (const p of parts) {
    out.set(p, off);
    off += p.byteLength;
  }
  return out;
};

const muxBox = (type, parts) => {
  const body = muxConcat(parts);
  const out = new Uint8Array(8 + body.byteLength);
  new DataView(out.buffer).setUint32(0, out.byteLength);
  for (let i = 0; i < 4; i++) out[4 + i] = type.charCodeAt(i);
  out.set(body, 8);
  return out;
};

const muxSame = (a, b) => a.byteLength === b.byteLength && a.every((v, i) => v === b[i]);

// The markers of protection in init bytes: an encrypted sample entry, a track encryption box, a pssh.
const MUX_DRM = ["encv", "enca", "tenc", "pssh", "sinf"];
const muxProtected = (u8) => {
  for (let i = 0; i + 4 <= u8.byteLength; i++) {
    const t = String.fromCharCode(u8[i], u8[i + 1], u8[i + 2], u8[i + 3]);
    if (MUX_DRM.includes(t)) return true;
  }
  return false;
};

// One track of an init: its tkhd and mdhd fields, its trex (from the init's mvex), its kind.
const muxTracksOf = (u8) => {
  const top = muxBoxes(u8, 0, u8.byteLength);
  const ftyp = top.boxes.find((b) => b.type === "ftyp");
  const moov = top.boxes.find((b) => b.type === "moov");
  if (!moov) return null;
  const dv = new DataView(u8.buffer, u8.byteOffset, u8.byteLength);
  const mvhd = muxChild(u8, moov, "mvhd");
  if (!mvhd) return null;
  const mvhdV1 = u8[mvhd.body] === 1;
  const movieScale = dv.getUint32(mvhd.body + (mvhdV1 ? 20 : 12));
  const mvex = muxChild(u8, moov, "mvex");
  const trexes = mvex ? muxChildren(u8, mvex, "trex") : [];
  const tracks = [];
  for (const trak of muxChildren(u8, moov, "trak")) {
    const tkhd = muxChild(u8, trak, "tkhd");
    const mdia = muxChild(u8, trak, "mdia");
    const mdhd = mdia && muxChild(u8, mdia, "mdhd");
    const hdlr = mdia && muxChild(u8, mdia, "hdlr");
    if (!tkhd || !mdhd) return null;
    const idOff = tkhd.body + (u8[tkhd.body] === 1 ? 20 : 12);
    const id = dv.getUint32(idOff);
    const scale = dv.getUint32(mdhd.body + (u8[mdhd.body] === 1 ? 20 : 12));
    const trex = trexes.find((t) => dv.getUint32(t.body + 4) === id);
    if (!trex) return null;
    tracks.push({
      id,
      scale,
      kind: hdlr ? muxType(u8, hdlr.body + 8 - 4) : "",
      trak: u8.subarray(trak.start, trak.end),
      trakIdOff: idOff - trak.start,
      trex: u8.subarray(trex.start, trex.end),
      defaultDur: dv.getUint32(trex.body + 12),
    });
  }
  return { ftyp: ftyp ? u8.subarray(ftyp.start, ftyp.end) : null, mvhd: u8.subarray(mvhd.start, mvhd.end), movieScale, tracks };
};

// The trafs of a moof: track ID, tfdt, duration (from trun, tfhd or trex), and whether its offsets are absolute.
const muxFragment = (u8, moof, defaults) => {
  const dv = new DataView(u8.buffer, u8.byteOffset, u8.byteLength);
  const trafs = [];
  for (const traf of muxChildren(u8, moof, "traf")) {
    const tfhd = muxChild(u8, traf, "tfhd");
    const tfdt = muxChild(u8, traf, "tfdt");
    if (!tfhd || !tfdt) return null;
    const flags = dv.getUint32(tfhd.body) & 0xffffff;
    const id = dv.getUint32(tfhd.body + 4);
    let o = tfhd.body + 8;
    if (flags & 0x1) return { absolute: true };
    if (flags & 0x2) o += 4;
    let defDur = defaults(id);
    if (flags & 0x8) defDur = dv.getUint32(o);
    const dt = u8[tfdt.body] === 1 ? Number(dv.getBigUint64(tfdt.body + 4)) : dv.getUint32(tfdt.body + 4);
    let dur = 0;
    for (const trun of muxChildren(u8, traf, "trun")) {
      const tf = dv.getUint32(trun.body) & 0xffffff;
      const count = dv.getUint32(trun.body + 4);
      let p = trun.body + 8;
      if (tf & 0x1) p += 4;
      if (tf & 0x4) p += 4;
      const per = (tf & 0x100 ? 4 : 0) + (tf & 0x200 ? 4 : 0) + (tf & 0x400 ? 4 : 0) + (tf & 0x800 ? 4 : 0);
      if (p + per * count > trun.end) return null;
      for (let i = 0; i < count; i++) {
        dur += tf & 0x100 ? dv.getUint32(p + i * per) : defDur;
      }
    }
    trafs.push({ id, tfhdIdOff: tfhd.body + 4 - moof.start, tfdt: dt, dur });
  }
  if (trafs.length === 0) return null;
  const mfhd = muxChild(u8, moof, "mfhd");
  if (!mfhd) return null;
  return { trafs, seqOff: mfhd.body + 4 - moof.start };
};

const MUX_REASONS = {
  webm: "the source is WebM",
  switch: "the quality changed during the video (a second init)",
  seqmode: "the player appends in sequence mode",
  tso: "the player moved the timeline (timestampOffset changed)",
  drm: "the stream is protected (DRM)",
  initlost: "an init segment was lost while recording",
  absolute: "a fragment has absolute data offsets",
  tracks: "too many tracks",
  structure: "the bytes are no fragmented MP4 this can read",
  empty: "there is no media",
};

// `messages`: what a recording holds, in order: { header (the JSON header), payload (Uint8Array) }.
// Returns { ok: true, parts (Uint8Array[] of the file), report } or { ok: false, reason, text }.
const fmp4Mux = (messages) => {
  const no = (reason) => ({ ok: false, reason, text: MUX_REASONS[reason] });
  const sbs = new Map(); // sb -> { init: Uint8Array|null, pending: Uint8Array|null, carry, frags: [], tso }
  let tso = null;
  let full = false;
  const sbOf = (n) => {
    if (!sbs.has(n)) sbs.set(n, { init: null, pending: null, carry: new Uint8Array(0), frags: [], cut: 0 });
    return sbs.get(n);
  };
  // Complete moof+mdat pairs out of the carry; what is left waits for the next append.
  const drain = (s) => {
    const r = muxBoxes(s.carry, 0, s.carry.byteLength);
    if (r.bad) return false;
    let used = 0;
    const list = r.boxes;
    for (let i = 0; i < list.length; i++) {
      const b = list[i];
      if (b.type === "moof") {
        const next = list[i + 1];
        if (!next) break;
        if (next.type !== "mdat") return false;
        s.frags.push({ moof: s.carry.subarray(b.start, b.end), mdat: s.carry.subarray(next.start, next.end) });
        used = next.end;
        i++;
      } else if (b.type === "mdat") {
        return false;
      } else {
        // styp, sidx, emsg, prft, free: not part of the file made here.
        used = b.end;
      }
    }
    s.carry = s.carry.slice(used);
    return true;
  };
  for (const { header: h, payload } of messages) {
    if (!h || typeof h !== "object") continue;
    if (h.type === "event") {
      if (h.name === "initlost") return no("initlost");
      if (h.name === "recordfull") full = true;
      continue;
    }
    if (h.type !== "segment" || !Number.isSafeInteger(h.sb)) continue;
    if (h.kind && h.kind !== "mp4") return no("webm");
    if (h.seqMode === true) return no("seqmode");
    const s = sbOf(h.sb);
    if (h.isInit === true) {
      // A new init: what was cut off before it is lost (counted), and the init is assembled anew.
      if (s.carry.byteLength) s.cut++;
      s.carry = new Uint8Array(0);
      s.pending = payload;
    } else if (h.initCont === true && s.pending) {
      s.pending = muxConcat([s.pending, payload]);
    } else {
      if (h.tfdt !== null && h.tfdt !== undefined) {
        if (tso === null) tso = h.tso ?? 0;
        else if ((h.tso ?? 0) !== tso) return no("tso");
        // A segment that starts anew while bytes of a cut one wait: the cut one is lost (an abort after a seek).
        if (s.carry.byteLength && muxType(payload, 0) !== "" && ["moof", "styp", "sidx", "emsg", "prft"].includes(muxType(payload, 0))) {
          s.cut++;
          s.carry = new Uint8Array(0);
        }
      }
      s.carry = muxConcat([s.carry, payload]);
      if (!drain(s)) return no("structure");
      continue;
    }
    if (s.pending.byteLength > MUX_MAX_INIT) return no("structure");
    // The init is complete once its moov is; bytes after it (an init+media append) are media.
    const top = muxBoxes(s.pending, 0, s.pending.byteLength);
    if (top.bad) return no("structure");
    const moov = top.boxes.find((b) => b.type === "moov");
    if (!moov) continue;
    const init = s.pending.subarray(0, moov.end);
    const rest = s.pending.subarray(moov.end);
    s.pending = null;
    if (muxProtected(init)) return no("drm");
    if (s.init && !muxSame(s.init, init)) return no("switch");
    s.init = s.init || init.slice();
    if (rest.byteLength) {
      s.carry = muxConcat([s.carry, rest]);
      if (!drain(s)) return no("structure");
    }
  }

  // The tracks, renumbered 1..n across the SourceBuffers in the order they came.
  const tracks = [];
  let first = null;
  for (const [sb, s] of sbs) {
    if (!s.init) continue;
    const parsed = muxTracksOf(s.init);
    if (!parsed || parsed.tracks.length === 0) return no("structure");
    if (!first) first = parsed;
    for (const t of parsed.tracks) tracks.push({ ...t, sb, movieScale: parsed.movieScale, newId: tracks.length + 1 });
  }
  if (tracks.length === 0) return no("empty");
  if (tracks.length > MUX_MAX_TRACKS) return no("tracks");
  const trackOf = (sb, id) => tracks.find((t) => t.sb === sb && t.id === id);

  // The fragments of every track, read, deduplicated, ordered; gaps and overlaps counted.
  const frags = [];
  let cut = 0;
  for (const [sb, s] of sbs) {
    cut += s.cut + (s.carry.byteLength ? 1 : 0);
    for (const f of s.frags) {
      const moofBox = muxBoxes(f.moof, 0, f.moof.byteLength).boxes[0];
      const info = muxFragment(f.moof, moofBox, (id) => (trackOf(sb, id) || { defaultDur: 0 }).defaultDur);
      if (!info) return no("structure");
      if (info.absolute) return no("absolute");
      const lead = trackOf(sb, info.trafs[0].id);
      if (!lead || info.trafs.some((t) => !trackOf(sb, t.id))) return no("structure");
      frags.push({ sb, f, info, lead, tfdt: info.trafs[0].tfdt, dur: info.trafs[0].dur });
    }
  }
  if (frags.length === 0) return no("empty");
  const kept = [];
  const report = { tracks: [], gaps: [], dupes: 0, overlaps: 0, cut, full };
  for (const t of tracks) {
    const mine = frags.filter((x) => x.lead === t).sort((a, b) => a.tfdt - b.tfdt);
    let end = null;
    let seconds = 0;
    for (const x of mine) {
      if (end !== null && x.tfdt < end) {
        // The same fragment again (a seek back) or one that overlaps: kept once.
        if (kept.length && kept.some((k) => k.lead === t && k.tfdt === x.tfdt)) report.dupes++;
        else report.overlaps++;
        continue;
      }
      if (end !== null && x.tfdt > end) {
        const slack = Math.max(0.05 * t.scale, 0.01 * x.dur);
        if (x.tfdt - end > slack) report.gaps.push({ track: t.newId, at: end / t.scale, seconds: (x.tfdt - end) / t.scale });
      }
      kept.push(x);
      end = x.tfdt + x.dur;
      seconds += x.dur / t.scale;
    }
    report.tracks.push({ id: t.newId, kind: t.kind, fragments: kept.filter((k) => k.lead === t).length, seconds });
  }
  // Decode order across the tracks, by time in seconds.
  kept.sort((a, b) => a.tfdt / a.lead.scale - b.tfdt / b.lead.scale);

  // moov: the first init's mvhd (next track ID set), every trak with its new ID, an mvex with every trex.
  const mvhd = first.mvhd.slice();
  const mdv = new DataView(mvhd.buffer);
  mdv.setUint32(mvhd.byteLength - 4, tracks.length + 1);
  const traks = tracks.map((t) => {
    const trak = t.trak.slice();
    new DataView(trak.buffer).setUint32(t.trakIdOff, t.newId);
    // An edit list counts in the movie's timescale: rescaled when this track's movie had another one.
    if (t.movieScale !== first.movieScale) muxRescaleEdits(trak, first.movieScale / t.movieScale);
    return trak;
  });
  const trexes = tracks.map((t) => {
    const trex = t.trex.slice();
    new DataView(trex.buffer).setUint32(12, t.newId);
    return trex;
  });
  const moov = muxBox("moov", [mvhd, ...traks, muxBox("mvex", trexes)]);
  const ftyp = first.ftyp || muxBox("ftyp", [new TextEncoder().encode("iso6"), new Uint8Array(4), new TextEncoder().encode("iso6mp41")]);
  const parts = [ftyp, moov];
  let seq = 1;
  for (const x of kept) {
    const moof = x.f.moof.slice();
    const dv = new DataView(moof.buffer);
    dv.setUint32(x.info.seqOff, seq++);
    for (const tr of x.info.trafs) dv.setUint32(tr.tfhdIdOff, trackOf(x.sb, tr.id).newId);
    parts.push(moof, x.f.mdat);
  }
  return { ok: true, parts, report };
};

// Multiplies the segment durations of a trak's edit list (edts/elst) by `factor`, in place.
const muxRescaleEdits = (trak, factor) => {
  const dv = new DataView(trak.buffer, trak.byteOffset, trak.byteLength);
  const top = muxBoxes(trak, 0, trak.byteLength).boxes[0];
  const edts = top && muxChild(trak, top, "edts");
  const elst = edts && muxChild(trak, edts, "elst");
  if (!elst) return;
  const v1 = trak[elst.body] === 1;
  const count = dv.getUint32(elst.body + 4);
  let p = elst.body + 8;
  for (let i = 0; i < count && p < elst.end; i++) {
    if (v1) {
      dv.setBigUint64(p, BigInt(Math.round(Number(dv.getBigUint64(p)) * factor)));
      p += 20;
    } else {
      dv.setUint32(p, Math.round(dv.getUint32(p) * factor));
      p += 12;
    }
  }
};

