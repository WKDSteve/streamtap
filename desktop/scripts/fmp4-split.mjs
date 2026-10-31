// Splits a fragmented MP4 into its init segment (everything up to moov) and
// its media segments (moof+mdat pairs), with the tfdt of each media segment
// and the first mdhd timescale. Shared by the test scripts.
import { readFileSync } from "node:fs";

export const splitFmp4 = (file) => {
  const buf = readFileSync(file);
  const u8 = new Uint8Array(buf.buffer, buf.byteOffset, buf.byteLength);
  const dv = new DataView(u8.buffer, u8.byteOffset, u8.byteLength);
  const type = (off) => String.fromCharCode(u8[off + 4], u8[off + 5], u8[off + 6], u8[off + 7]);

  const boxes = [];
  for (let off = 0; off + 8 <= u8.byteLength; ) {
    let size = dv.getUint32(off);
    if (size === 1) size = Number(dv.getBigUint64(off + 8));
    else if (size === 0) size = u8.byteLength - off;
    if (size < 8) break;
    boxes.push({ type: type(off), start: off, end: off + size });
    off += size;
  }

  const moovIdx = boxes.findIndex((b) => b.type === "moov");
  if (moovIdx < 0) throw new Error(`${file}: no moov box (not a fragmented MP4?)`);
  const init = u8.subarray(0, boxes[moovIdx].end);
  const segments = [];
  for (let i = moovIdx + 1; i < boxes.length; i++) {
    if (boxes[i].type === "moof" && boxes[i + 1] && boxes[i + 1].type === "mdat") {
      segments.push(u8.subarray(boxes[i].start, boxes[i + 1].end));
      i++;
    }
  }

  const findBox = (start, end, path) => {
    const [want, ...rest] = path;
    for (let off = start; off + 8 <= end; ) {
      const size = dv.getUint32(off);
      if (size < 8) return null;
      if (type(off) === want) return rest.length ? findBox(off + 8, off + size, rest) : off;
      off += size;
    }
    return null;
  };
  const tfdtOf = (seg) => {
    const base = seg.byteOffset - u8.byteOffset;
    const off = findBox(base, base + seg.byteLength, ["moof", "traf", "tfdt"]);
    if (off === null) return null;
    return u8[off + 8] === 1 ? Number(dv.getBigUint64(off + 12)) : dv.getUint32(off + 12);
  };
  const mdhdOff = findBox(0, init.byteLength, ["moov", "trak", "mdia", "mdhd"]);
  const timescale = mdhdOff === null ? null : dv.getUint32(mdhdOff + 8 + (u8[mdhdOff + 8] === 1 ? 20 : 12));

  // Duration of media segment i in timescale ticks: the step to the next
  // tfdt (the last segment repeats the step before it), null when unknown.
  const durOf = (i) => {
    const t = (j) => (segments[j] ? tfdtOf(segments[j]) : null);
    const step = (j) => (t(j) !== null && t(j + 1) !== null ? t(j + 1) - t(j) : null);
    const d = step(i) ?? (i > 0 ? step(i - 1) : null);
    return d !== null && d > 0 ? d : null;
  };

  return { init, segments, tfdtOf, durOf, timescale };
};
