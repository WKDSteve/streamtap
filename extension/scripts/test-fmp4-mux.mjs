#!/usr/bin/env node
// The muxer of the target "a video on this device" (extension/src/fmp4-mux.js) on real fragmented MP4s:
// extension/scripts/fixtures/video-frag.mp4 (H.264) and audio-frag.mp4 (AAC), two SourceBuffers as a player appends
// them, and desktop/scripts/fixtures/frag.mp4 (both tracks in one init, one SourceBuffer). Checks the file it makes
// (tracks, fragments, order, renumbered IDs) and, where ffprobe and ffmpeg are installed, that they read and decode it
// without an error (skipped without them). Then the cases it refuses, each with its reason.
// Exit 0 when every check passed.
import { readFileSync, writeFileSync, mkdtempSync, rmSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";
import { tmpdir } from "node:os";
import { spawnSync } from "node:child_process";
import vm from "node:vm";
import { splitFmp4 } from "../../desktop/scripts/fmp4-split.mjs";

const here = dirname(fileURLToPath(import.meta.url));
const ctx = vm.createContext({ DataView, Uint8Array, TextEncoder, BigInt, Number, Math });
vm.runInContext(readFileSync(join(here, "..", "src", "fmp4-mux.js"), "utf8"), ctx, { filename: "fmp4-mux.js" });
const mux = (messages) => vm.runInContext("fmp4Mux", ctx)(messages);

const failures = [];
const check = (ok, what) => {
  console.log(`${ok ? "ok  " : "FAIL"} ${what}`);
  if (!ok) failures.push(what);
};

const video = splitFmp4(join(here, "fixtures", "video-frag.mp4"));
const audio = splitFmp4(join(here, "fixtures", "audio-frag.mp4"));
const both = splitFmp4(join(here, "..", "..", "desktop", "scripts", "fixtures", "frag.mp4"));
const u8 = (b) => new Uint8Array(b.buffer, b.byteOffset, b.byteLength);

let seq = 1;
const init = (sb, bytes, extra = {}) => ({ header: { type: "segment", sb, seq: seq++, isInit: true, kind: "mp4", tso: 0, ...extra }, payload: u8(bytes) });
const cont = (sb, bytes) => ({ header: { type: "segment", sb, seq: seq++, initCont: true, kind: "mp4", tso: 0 }, payload: u8(bytes) });
const media = (sb, split, i, extra = {}) => ({
  header: { type: "segment", sb, seq: seq++, kind: "mp4", tfdt: split.tfdtOf(split.segments[i]), tso: 0, ...extra },
  payload: u8(split.segments[i]),
});
const raw = (sb, bytes, tfdt = null) => ({ header: { type: "segment", sb, seq: seq++, kind: "mp4", tfdt, tso: 0 }, payload: u8(bytes) });

// The boxes of the made file (top level), and the track IDs of its moof/traf/tfhd in order.
const fileOf = (r) => Buffer.concat(r.parts.map((p) => Buffer.from(p.buffer, p.byteOffset, p.byteLength)));
const top = (b) => {
  const out = [];
  for (let off = 0; off + 8 <= b.length; ) {
    const size = b.readUInt32BE(off);
    out.push({ type: b.toString("latin1", off + 4, off + 8), off, size });
    off += size;
  }
  return out;
};
const tfhdIds = (b) => {
  const ids = [];
  for (let i = 0; i + 8 <= b.length; i++) if (b.toString("latin1", i, i + 4) === "tfhd") ids.push(b.readUInt32BE(i + 8));
  return ids;
};
const mfhdSeqs = (b) => {
  const seqs = [];
  for (let i = 0; i + 8 <= b.length; i++) if (b.toString("latin1", i, i + 4) === "mfhd") seqs.push(b.readUInt32BE(i + 8));
  return seqs;
};

const tmp = mkdtempSync(join(tmpdir(), "streamtap-mux-"));
const have = (tool) => spawnSync(tool, ["-version"], { stdio: "ignore" }).status === 0;
const ffprobe = have("ffprobe");
const ffmpeg = have("ffmpeg");
// What ffprobe sees in the file, and whether ffmpeg decodes all of it without an error.
const probe = (b, name) => {
  const f = join(tmp, name);
  writeFileSync(f, b);
  const p = spawnSync("ffprobe", ["-v", "error", "-show_entries", "stream=codec_name", "-of", "csv=p=0", f], { encoding: "utf8" });
  const d = ffmpeg ? spawnSync("ffmpeg", ["-v", "error", "-i", f, "-f", "null", "-"], { encoding: "utf8" }) : { status: 0, stderr: "" };
  return { codecs: p.stdout.trim().split(/\r?\n/).sort().join(","), decodeErrors: d.status === 0 ? d.stderr.trim() : `exit ${d.status}` };
};

// two SourceBuffers, appended as a player does: each init, then media of both in turn
{
  const msgs = [init(1, video.init), init(2, audio.init)];
  for (let i = 0; i < 3; i++) msgs.push(media(1, video, i), media(2, audio, i));
  const r = mux(msgs);
  check(r.ok, `two SourceBuffers: made (${r.ok ? "" : r.text})`);
  const b = fileOf(r);
  const types = top(b).map((x) => x.type);
  check(types[0] === "ftyp" && types[1] === "moov" && types.slice(2).join(" ") === "moof mdat ".repeat(6).trim(), `two SourceBuffers: ftyp moov then 6 moof+mdat (got ${types.join(" ")})`);
  check(r.report.tracks.length === 2 && r.report.tracks.every((t) => t.fragments === 3), "two SourceBuffers: 2 tracks with 3 fragments each");
  const ids = tfhdIds(b.subarray(top(b)[2].off));
  check(new Set(ids).size === 2 && ids.every((i) => i === 1 || i === 2), `two SourceBuffers: tracks renumbered 1 and 2 (got ${ids})`);
  check(mfhdSeqs(b).join() === "1,2,3,4,5,6", `two SourceBuffers: moofs numbered anew (got ${mfhdSeqs(b)})`);
  check(r.report.gaps.length === 0 && r.report.dupes === 0 && r.report.cut === 0, "two SourceBuffers: no gap, no duplicate, nothing cut");
  if (ffprobe) {
    const p = probe(b, "two.mp4");
    check(p.codecs === "aac,h264", `two SourceBuffers: ffprobe sees h264 and aac (got ${p.codecs})`);
    check(p.decodeErrors === "", `two SourceBuffers: ffmpeg decodes it without an error (${p.decodeErrors})`);
  } else {
    console.log("skip ffprobe/ffmpeg not installed: the file is checked by its structure only");
  }
}

// one SourceBuffer with both tracks in its init
{
  const msgs = [init(1, both.init), ...both.segments.map((_, i) => media(1, both, i))];
  const r = mux(msgs);
  check(r.ok && r.report.tracks.length === 2, `one SourceBuffer with two tracks: made with 2 tracks (${r.ok ? "" : r.text})`);
  if (ffprobe && r.ok) {
    const p = probe(fileOf(r), "both.mp4");
    check(p.codecs === "aac,h264" && p.decodeErrors === "", `one SourceBuffer with two tracks: ffprobe and ffmpeg read it (${p.codecs} ${p.decodeErrors})`);
  }
}

// the same fragment appended again after a seek back: kept once, the file is the same as without it
{
  const plain = mux([init(1, video.init), media(1, video, 0), media(1, video, 1), media(1, video, 2)]);
  const again = mux([init(1, video.init), media(1, video, 0), media(1, video, 1), media(1, video, 2), media(1, video, 1)]);
  check(again.ok && again.report.dupes === 1, "seek back: the repeated fragment is counted once as a duplicate");
  check(again.ok && fileOf(again).equals(fileOf(plain)), "seek back: the file is the one without the repeat");
}

// a fragment missing: a gap of its length is reported
{
  const r = mux([init(1, video.init), media(1, video, 0), media(1, video, 2)]);
  const g = r.ok && r.report.gaps[0];
  check(r.ok && r.report.gaps.length === 1 && g.seconds > 0.5 && g.seconds < 1.5, `gap: reported with its length (got ${JSON.stringify(r.report && r.report.gaps)})`);
}

// a media segment cut over two appends, an init cut over two (isInit + initCont), an init+media append
{
  const seg = video.segments[1];
  const half = Math.floor(seg.length / 2);
  const cutMedia = mux([
    init(1, video.init),
    media(1, video, 0),
    raw(1, seg.subarray(0, half), video.tfdtOf(seg)),
    raw(1, seg.subarray(half)),
    media(1, video, 2),
  ]);
  check(cutMedia.ok && cutMedia.report.tracks[0].fragments === 3 && cutMedia.report.cut === 0, "cut media: joined to one fragment");
  const ih = Math.floor(video.init.length / 2);
  const cutInit = mux([init(1, video.init.subarray(0, ih)), cont(1, video.init.subarray(ih)), media(1, video, 0)]);
  check(cutInit.ok && cutInit.report.tracks.length === 1, "cut init: assembled from isInit and initCont");
  const together = mux([init(1, Buffer.concat([video.init, video.segments[0]])), media(1, video, 1)]);
  check(together.ok && together.report.tracks[0].fragments === 2, "init+media append: the media after the moov is kept");
}

// what it refuses, each with its reason, and nothing made
{
  const refused = (what, msgs, reason) => {
    const r = mux(msgs);
    check(!r.ok && r.reason === reason && typeof r.text === "string" && r.text.length > 0, `refused, ${what}: ${reason} (got ${r.ok ? "made" : r.reason})`);
  };
  refused("WebM", [init(1, video.init, { kind: "webm" })], "webm");
  refused("sequence mode", [init(1, video.init), media(1, video, 0, { seqMode: true })], "seqmode");
  refused("timestampOffset moved", [init(1, video.init), media(1, video, 0), media(1, video, 1, { tso: 30 })], "tso");
  refused("quality switch", [init(1, video.init), media(1, video, 0), init(1, audio.init), media(1, audio, 0)], "switch");
  refused("lost init", [init(1, video.init), media(1, video, 0), { header: { type: "event", name: "initlost", sb: 1 }, payload: new Uint8Array(0) }], "initlost");
  const drm = Buffer.from(video.init);
  const at = drm.indexOf("avc1");
  drm.write("encv", at, "latin1");
  refused("protected", [init(1, drm), media(1, video, 0)], "drm");
  refused("no media", [init(1, video.init)], "empty");
  refused("nothing at all", [], "empty");
  // The same init again (a player that re-appends it) is no switch.
  const same = mux([init(1, video.init), media(1, video, 0), init(1, video.init), media(1, video, 1)]);
  check(same.ok && same.report.tracks[0].fragments === 2, "the same init again is no quality switch");
}

rmSync(tmp, { recursive: true, force: true });
if (failures.length) {
  console.error(`FAIL: ${failures.length} check(s) failed`);
  process.exit(1);
}
console.log("ok: all checks passed");
