// The folder of the app for the sheet that opens a capture's folder (TESTS/sheets/open-folder.sheet.lua): the `prepare`
// step of the app `captures` in .testing.lua. Writes into <dir> (STREAMTAP_APP_DIR of the app) a config.json whose
// captures folder is <dir>/captures, in English, and one finished capture there, `1730000000_sheet`, with its
// session.json, a small out.mp4 and a folder `segments`: a name Explorer shows as it is (it hides known extensions of
// files, never anything of a folder's name). No dependency beyond node.
//
// usage: node tools/sheet-captures.mjs <dir>

import { mkdirSync, writeFileSync } from "node:fs";
import { join, resolve } from "node:path";

const [dir] = process.argv.slice(2);
if (!dir) {
  console.error("usage: node tools/sheet-captures.mjs <dir>");
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
writeFileSync(join(capture, "out.mp4"), "not a real video");
