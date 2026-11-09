// The app's page for the test sheets (TESTS/sheets, .testing.lua): a copy of desktop/crates/streamtap-app/ui/index.html
// in <dir> whose scripts find the stand-in for the Tauri bridge (tools/bridge-stub.mjs) first, and whose other files
// come from the real folder (<base>). Prints the copy's file: link and stays until its stdin closes, as an app of a
// sheet does; the sheet's run removes <dir>. No dependency beyond node.
//
// usage: node tools/sheet-page.mjs <dir>

import { readFileSync, writeFileSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";
import { bridge } from "./bridge-stub.mjs";

const [dir] = process.argv.slice(2);
if (!dir) {
  console.error("usage: node tools/sheet-page.mjs <dir>");
  process.exit(2);
}
const here = dirname(fileURLToPath(import.meta.url));
const ui = join(here, "..", "desktop", "crates", "streamtap-app", "ui");
const page = readFileSync(join(ui, "index.html"), "utf8");
const first = page.indexOf("<script");
const head = page.indexOf("<head>");
if (first < 0 || head < 0) {
  console.error("sheet-page: index.html has no <head> or no <script>");
  process.exit(1);
}
const stub = join(resolve(dir), "bridge-stub.js");
writeFileSync(stub, `(${bridge.toString()})();\n`);
const base = `<base href="${pathToFileURL(ui).href}/">`;
const copy =
  page.slice(0, head + "<head>".length) +
  base +
  page.slice(head + "<head>".length, first) +
  `<script src="${pathToFileURL(stub).href}"></script>\n` +
  page.slice(first);
const out = join(resolve(dir), "index.html");
writeFileSync(out, copy);
console.log(pathToFileURL(out).href);
process.stdin.resume();
