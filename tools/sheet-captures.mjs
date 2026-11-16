// The folder of the app for the sheets that open a capture (TESTS/sheets/open-folder.sheet.lua, open-result.sheet.lua):
// the `prepare` step of the apps `captures` and `captures-video` in .testing.lua. Writes into <dir> (STREAMTAP_APP_DIR
// of the app) a config.json whose captures folder is <dir>/captures, in English, and one finished capture there,
// `1730000000_sheet`, with its session.json, an out.mp4 and a folder `segments`: a name Explorer shows as it is (it
// hides known extensions of files, never anything of a folder's name). No dependency beyond node.
//
// With --video the out.mp4 is a real video of a minute (ffmpeg on the PATH, a test picture and a tone of lavfi), so the
// default program that "Open result" starts plays it and keeps its name in the window title while the sheet looks: a
// player shows the file's name only while it plays (VLC, measured 2026-11-16). Without it, a few bytes stand in.
//
// usage: node tools/sheet-captures.mjs <dir> [--video]

import { spawnSync } from "node:child_process";
import { mkdirSync, writeFileSync } from "node:fs";
import { join, resolve } from "node:path";

const args = process.argv.slice(2);
const video = args.includes("--video");
const [dir] = args.filter((a) => a !== "--video");
if (!dir) {
  console.error("usage: node tools/sheet-captures.mjs <dir> [--video]");
  process.exit(2);
}
const root = resolve(dir);
const captures = join(root, "captures");
const capture = join(captures, "1730000000_sheet");
mkdirSync(join(capture, "segments"), { recursive: true });
writeFileSync(
  join(root, "config.json"),
  JSON.stringify(
    {
      port: 17651,
      token: "sheet-token-0123456789",
      captures_dir: captures,
      language: "en",
      theme: "light",
    },
    null,
    2,
  ),
);
writeFileSync(
  join(capture, "session.json"),
  JSON.stringify({
    url: "https://example.test/video",
    title: "Sheet capture",
    session: "sheet",
    started: 1730000000,
  }),
);
const out = join(capture, "out.mp4");
if (!video) {
  writeFileSync(out, "not a real video");
} else {
  // encoders every ffmpeg build has (mpeg4, aac): no libx264 needed; a small picture keeps the file at a few hundred KB
  const made = spawnSync(
    "ffmpeg",
    [
      "-hide_banner",
      "-loglevel",
      "error",
      "-y",
      "-f",
      "lavfi",
      "-i",
      "testsrc=duration=60:size=320x240:rate=15",
      "-f",
      "lavfi",
      "-i",
      "sine=frequency=440:duration=60",
      "-c:v",
      "mpeg4",
      "-q:v",
      "10",
      "-c:a",
      "aac",
      "-shortest",
      out,
    ],
    { stdio: ["ignore", "inherit", "inherit"] },
  );
  if (made.error || made.status !== 0) {
    console.error(
      `sheet-captures: ffmpeg could not write ${out}: ${made.error ? made.error.message : `exit ${made.status}`}`,
    );
    process.exit(1);
  }
}
