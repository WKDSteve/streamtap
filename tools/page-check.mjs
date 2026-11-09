// The app's page in a real Chrome with a stand-in for the Tauri bridge: axe in light and dark, also with a button hovered
// and focused, and with a hint and a capture on the page. streamtap itself has no Node dependency: Playwright and axe come
// at fixed versions from the folder tools/page-check, which nothing else reads.
//
//   pnpm --dir tools/page-check install --frozen-lockfile
//   node tools/page-check.mjs tools/page-check/node_modules
//
// usage: node tools/page-check.mjs <node_modules with @playwright/test and @axe-core/playwright>

import { createRequire } from "node:module";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";
import { bridge } from "./bridge-stub.mjs";

const [modules] = process.argv.slice(2);
if (!modules) {
  console.error("usage: node tools/page-check.mjs <node_modules>");
  process.exit(2);
}
const require = createRequire(join(resolve(modules), "x.js"));
const { chromium } = require("@playwright/test");
const { AxeBuilder } = require("@axe-core/playwright");

const here = dirname(fileURLToPath(import.meta.url));
const page_url = pathToFileURL(join(here, "..", "desktop", "crates", "streamtap-app", "ui", "index.html")).href;

const failed = [];
function check(what, ok, detail = "") {
  console.log(`${ok ? "ok    " : "FAILED"} ${what}${ok ? "" : ` ${detail}`}`);
  if (!ok) failed.push(what);
}

async function axe(page, label) {
  const result = await new AxeBuilder({ page }).analyze();
  const lines = result.violations.map((v) => `${v.id}: ${v.nodes.map((n) => n.target.join(" ")).join(", ")}`);
  check(`axe ${label}`, lines.length === 0, lines.join("; "));
}

const browser = await chromium.launch({ channel: "chrome", headless: true });
try {
  const context = await browser.newContext();
  const page = await context.newPage();
  const problems = [];
  page.on("pageerror", (error) => problems.push(String(error)));
  page.on("console", (message) => {
    if (message.type() === "error") problems.push(message.text());
  });
  await page.addInitScript(bridge);
  await page.goto(page_url);
  // The page draws what the snapshot says; the first draw is done when the capture is on it. Without it axe would check
  // an empty page.
  const drawn = await page
    .waitForFunction(() => document.body.innerText.includes("A capture"), null, { timeout: 30_000 })
    .then(() => true, () => false);
  check("the stand-in's data is on the page", drawn);
  check("the page draws without a console error", problems.length === 0, problems.join("; "));

  // The panels a click opens are part of the page.
  await page.evaluate(() => {
    for (const id of ["browser", "settings"]) document.getElementById(id).hidden = false;
  });

  for (const theme of ["light", "dark"]) {
    await page.evaluate((name) => {
      document.documentElement.dataset.theme = name;
    }, theme);
    await axe(page, theme);
    const button = page.locator("button:visible").first();
    await button.hover();
    await axe(page, `${theme}, a button hovered`);
    await button.focus();
    await axe(page, `${theme}, a button focused`);
    await page.mouse.move(0, 0);
    // Axe is content with a button that does not react, so the colours are compared here.
    const primary = page.locator("button.primary:visible:not(:disabled)").first();
    const fill = () => primary.evaluate((node) => getComputedStyle(node).backgroundColor);
    const atRest = await fill();
    await primary.hover();
    check(`${theme}: a filled button shows the pointer`, (await fill()) !== atRest);
    await axe(page, `${theme}, a filled button hovered`);
    await primary.focus();
    await axe(page, `${theme}, a filled button hovered and focused`);
    await page.mouse.move(0, 0);
  }
} finally {
  await browser.close();
}

if (failed.length) {
  console.error(`page-check: ${failed.length} failed`);
  process.exit(1);
}
console.log("page-check: all green");
