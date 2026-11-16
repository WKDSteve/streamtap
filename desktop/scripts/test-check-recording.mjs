// check-recording.mjs against recordings ffmpeg makes here from lavfi: a good one is green, and every way a recording
// can be wrong is red with its reason (wrong length, no sound, a pause, black throughout, a cut-off file). Without
// ffmpeg and ffprobe it says so and exits 3: a skip, never a green run.
//
// usage: node desktop/scripts/test-check-recording.mjs

import { spawnSync } from "node:child_process";
import { mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const checker = join(here, "check-recording.mjs");

function have(tool) {
  const r = spawnSync(tool, ["-version"], { encoding: "utf8" });
  return !r.error && r.status === 0;
}
if (!have("ffmpeg") || !have("ffprobe")) {
  console.log("skip ffmpeg or ffprobe is not installed: check-recording cannot be tested here");
  process.exit(3);
}

const dir = mkdtempSync(join(tmpdir(), "check-recording-"));
let failed = 0;
function check(ok, what) {
  console.log(`${ok ? "ok  " : "FAIL"} ${what}`);
  if (!ok) failed++;
}

/** A recording from lavfi inputs; `video` and `audio` are lavfi sources, `vf` a filter on the picture. */
function make(name, { video, audio, vf, passthrough }) {
  const file = join(dir, name);
  const args = ["-hide_banner", "-loglevel", "error", "-y", "-f", "lavfi", "-i", video];
  if (audio) args.push("-f", "lavfi", "-i", audio);
  if (vf) args.push("-vf", vf);
  if (passthrough) args.push("-fps_mode", "passthrough");
  args.push("-c:v", "mpeg4", "-q:v", "10");
  if (audio) args.push("-c:a", "aac");
  args.push(file);
  const r = spawnSync("ffmpeg", args, { encoding: "utf8" });
  if (r.status !== 0) throw new Error(`ffmpeg could not make ${name}: ${r.stderr}`);
  return file;
}

function judge(file, ...opts) {
  const r = spawnSync(process.execPath, [checker, file, "--json", ...opts], { encoding: "utf8" });
  let out = null;
  try {
    out = JSON.parse(r.stdout);
  } catch {
    // a usage error writes no JSON
  }
  return { status: r.status, out, text: `${r.stdout}${r.stderr}` };
}

const reasons = (j) => (j.out?.checks ?? []).filter((c) => !c.ok).map((c) => c.what).join("; ");

try {
  const good = make("good.mp4", { video: "testsrc=duration=4:size=160x120:rate=15", audio: "sine=duration=4" });
  let j = judge(good, "--seconds", "4");
  check(j.status === 0 && j.out?.verdict === "green", `a good recording is green (${reasons(j) || "no reason"})`);

  j = judge(good, "--seconds", "12");
  check(j.status === 1 && /12 s were recorded/.test(reasons(j)), `a length off the recording time is red: ${reasons(j)}`);

  const silent = make("silent.mp4", { video: "testsrc=duration=4:size=160x120:rate=15" });
  j = judge(silent);
  check(j.status === 1 && /one audio track \(0\)/.test(reasons(j)), `no audio track is red: ${reasons(j)}`);
  j = judge(silent, "--video-only");
  check(j.status === 0, `... and green when no sound is wanted (${reasons(j) || "no reason"})`);

  const black = make("black.mp4", { video: "color=c=black:duration=4:size=160x120:rate=15", audio: "sine=duration=4" });
  j = judge(black, "--seconds", "4");
  check(j.status === 1 && /black from start to end/.test(reasons(j)), `a black recording is red: ${reasons(j)}`);

  // the picture stops for 1.5 s: frames between 1 s and 2.5 s are dropped, their timestamps kept
  const paused = make("paused.mp4", {
    video: "testsrc=duration=4:size=160x120:rate=15",
    audio: "sine=duration=4",
    vf: "select='not(between(t,1,2.5))'",
    passthrough: true,
  });
  j = judge(paused);
  check(j.status === 1 && /video track pauses/.test(reasons(j)), `a pause in the picture is red: ${reasons(j)}`);
  j = judge(paused, "--max-gap", "2");
  check(j.status === 0, `... and green when that pause is allowed (${reasons(j) || "no reason"})`);

  // cut off: an mp4 that ffmpeg wrote has its index at the end, so the first half is no readable file
  const cut = join(dir, "cut.mp4");
  const bytes = readFileSync(good);
  writeFileSync(cut, bytes.subarray(0, Math.floor(bytes.length / 2)));
  j = judge(cut);
  check(j.status === 1 && /cannot read the container/.test(reasons(j)), `a cut-off file is red: ${reasons(j)}`);

  j = judge(join(dir, "missing.mp4"));
  check(j.status === 1 && /there is no file/.test(reasons(j)), `a missing file is red: ${reasons(j)}`);

  j = judge(good, "--seconds");
  check(j.status === 2 && /takes a number/.test(j.text), "an option without its number is a usage error");
  j = judge(good, "--frobnicate");
  check(j.status === 2 && /unknown option/.test(j.text), "an unknown option is a usage error");
} finally {
  rmSync(dir, { recursive: true, force: true });
}

console.log(failed ? `${failed} check(s) failed` : "all checks passed");
process.exit(failed ? 1 : 0);
