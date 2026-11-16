// The part of judging a recording a machine can do: ffprobe reads the file (container), its length fits the time it
// was recorded, it has one video and one audio track, no track pauses longer than a threshold, and ffmpeg's
// blackdetect does not find it black from start to end (a protected source records black). What stays with a person is
// whether it shows the right thing. No dependency beyond node, ffprobe and ffmpeg.
//
// usage: node desktop/scripts/check-recording.mjs <file> [--seconds <n>] [--tolerance <s>] [--max-gap <s>]
//                                                [--video-only] [--json]
//   --seconds     how long the recording ran; without it the length is not checked
//   --tolerance   how far the length may be off (default the larger of 1 s and 5 % of --seconds)
//   --max-gap     the longest pause between two packets of a track, and before the first (default 1 s)
//   --video-only  a recording without sound (an MSE fixture with one SourceBuffer): no audio track wanted
//   --json        one JSON object on stdout instead of lines
// exit: 0 green, 1 red, 2 usage, 3 ffprobe or ffmpeg missing (a skip, never green)

import { spawnSync } from "node:child_process";
import { existsSync } from "node:fs";
import { pathToFileURL } from "node:url";

const USAGE =
  "usage: node desktop/scripts/check-recording.mjs <file> [--seconds <n>] [--tolerance <s>] [--max-gap <s>] [--video-only] [--json]";

/** @returns {{ file: string, seconds?: number, tolerance?: number, maxGap: number, videoOnly: boolean, json: boolean }} */
function parse(argv) {
  const out = { maxGap: 1, videoOnly: false, json: false };
  const number = (name, v) => {
    const n = Number(v);
    if (v === undefined || !Number.isFinite(n) || n < 0) {
      throw new Error(`${name} takes a number of seconds, not ${v === undefined ? "nothing" : JSON.stringify(v)}`);
    }
    return n;
  };
  for (let i = 0; i < argv.length; i++) {
    const a = argv[i];
    if (a === "--seconds") out.seconds = number(a, argv[++i]);
    else if (a === "--tolerance") out.tolerance = number(a, argv[++i]);
    else if (a === "--max-gap") out.maxGap = number(a, argv[++i]);
    else if (a === "--video-only") out.videoOnly = true;
    else if (a === "--json") out.json = true;
    else if (a.startsWith("--")) throw new Error(`unknown option ${a}`);
    else if (out.file === undefined) out.file = a;
    else throw new Error(`one file, not also ${a}`);
  }
  if (out.file === undefined) throw new Error("no file");
  return out;
}

function have(tool) {
  const r = spawnSync(tool, ["-version"], { encoding: "utf8" });
  return !r.error && r.status === 0;
}

function run(tool, args) {
  // no shell: the file name goes to the tool as one argument, whatever is in it
  return spawnSync(tool, args, { encoding: "utf8", maxBuffer: 64 * 1024 * 1024 });
}

/** The timestamps of one track's packets, in seconds. */
function packetTimes(file, select) {
  const r = run("ffprobe", [
    "-v",
    "error",
    "-select_streams",
    select,
    "-show_entries",
    "packet=pts_time",
    "-of",
    "csv=p=0",
    file,
  ]);
  if (r.status !== 0) return null;
  return r.stdout
    .split(/\r?\n/)
    .map((l) => Number.parseFloat(l))
    .filter((n) => Number.isFinite(n))
    .sort((a, b) => a - b);
}

/** The longest pause of a track: before its first packet (from `start`) or between two of them. */
function longestGap(times, start) {
  let gap = { length: times.length ? times[0] - start : Number.POSITIVE_INFINITY, at: start };
  for (let i = 1; i < times.length; i++) {
    const d = times[i] - times[i - 1];
    if (d > gap.length) gap = { length: d, at: times[i - 1] };
  }
  return gap;
}

/** The black parts ffmpeg's blackdetect finds, as [start, end] in seconds. */
function blackParts(file) {
  const r = run("ffmpeg", [
    "-hide_banner",
    "-nostats",
    "-i",
    file,
    "-an",
    "-vf",
    "blackdetect=d=0.5:pix_th=0.10",
    "-f",
    "null",
    "-",
  ]);
  if (r.status !== 0) return null;
  const parts = [];
  for (const m of r.stderr.matchAll(/black_start:\s*([\d.]+)\s+black_end:\s*([\d.]+)/g)) {
    parts.push([Number.parseFloat(m[1]), Number.parseFloat(m[2])]);
  }
  return parts;
}

/**
 * Judges a recording.
 * @returns {{ verdict: "green" | "red" | "skip", checks: { ok: boolean, what: string }[] }}
 */
export function judge(opts) {
  const checks = [];
  const check = (ok, what) => checks.push({ ok, what });
  if (!have("ffprobe") || !have("ffmpeg")) {
    return { verdict: "skip", checks: [{ ok: false, what: "ffprobe or ffmpeg is not on the PATH" }] };
  }
  if (!existsSync(opts.file)) {
    check(false, `there is no file ${opts.file}`);
    return { verdict: "red", checks };
  }
  const probe = run("ffprobe", ["-v", "error", "-print_format", "json", "-show_format", "-show_streams", opts.file]);
  let info;
  try {
    info = probe.status === 0 ? JSON.parse(probe.stdout) : null;
  } catch {
    info = null;
  }
  if (!info?.format) {
    check(false, `ffprobe cannot read the container: ${probe.stderr.trim().split(/\r?\n/)[0] || `exit ${probe.status}`}`);
    return { verdict: "red", checks };
  }
  check(true, `ffprobe reads it as ${info.format.format_name}`);

  const duration = Number.parseFloat(info.format.duration);
  if (!Number.isFinite(duration) || duration <= 0) {
    check(false, "it has no length");
  } else if (opts.seconds !== undefined) {
    const tolerance = opts.tolerance ?? Math.max(1, opts.seconds * 0.05);
    const off = Math.abs(duration - opts.seconds);
    check(
      off <= tolerance,
      `it is ${duration.toFixed(2)} s long, ${opts.seconds} s were recorded (off by ${off.toFixed(2)} s, at most ${tolerance} s)`,
    );
  } else {
    check(true, `it is ${duration.toFixed(2)} s long (no recording time given)`);
  }

  const streams = info.streams ?? [];
  const videos = streams.filter((s) => s.codec_type === "video").length;
  const audios = streams.filter((s) => s.codec_type === "audio").length;
  const wantAudio = opts.videoOnly ? 0 : 1;
  check(videos === 1, `one video track (${videos})`);
  check(audios === wantAudio, opts.videoOnly ? `no audio track (${audios})` : `one audio track (${audios})`);

  const start = Number.parseFloat(info.format.start_time) || 0;
  for (const [select, name, present] of [
    ["v:0", "video", videos > 0],
    ["a:0", "audio", audios > 0],
  ]) {
    if (!present) continue;
    const times = packetTimes(opts.file, select);
    if (!times) {
      check(false, `ffprobe cannot list the packets of the ${name} track`);
      continue;
    }
    const gap = longestGap(times, start);
    check(
      gap.length <= opts.maxGap,
      `the ${name} track pauses at most ${gap.length.toFixed(2)} s (at ${gap.at.toFixed(2)} s; at most ${opts.maxGap} s)`,
    );
  }

  if (videos > 0 && Number.isFinite(duration) && duration > 0) {
    const parts = blackParts(opts.file);
    if (!parts) {
      check(false, "ffmpeg cannot decode the video to look for black");
    } else {
      const black = parts.reduce((sum, [a, b]) => sum + (b - a), 0);
      // a frame or two of picture at either end does not make a black recording a picture
      const throughout = black >= duration - Math.max(0.5, duration * 0.02);
      check(
        !throughout,
        throughout
          ? `the picture is black from start to end (${black.toFixed(2)} of ${duration.toFixed(2)} s): a protected source?`
          : `the picture is not black throughout (${black.toFixed(2)} s black of ${duration.toFixed(2)} s)`,
      );
    }
  }
  return { verdict: checks.every((c) => c.ok) ? "green" : "red", checks };
}

function main() {
  let opts;
  try {
    opts = parse(process.argv.slice(2));
  } catch (e) {
    console.error(`check-recording: ${e.message}\n${USAGE}`);
    process.exit(2);
  }
  const result = judge(opts);
  if (opts.json) {
    console.log(JSON.stringify({ file: opts.file, ...result }));
  } else {
    for (const c of result.checks) console.log(`${c.ok ? "ok  " : "RED "} ${c.what}`);
    console.log(`verdict: ${result.verdict}`);
  }
  process.exit({ green: 0, red: 1, skip: 3 }[result.verdict]);
}

if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) {
  main();
}
