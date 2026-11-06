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

// What the Rust side answers, with something in every part of the page.
const bridge = () => {
  const settings = { port: 17650, token: "a-token", captures_dir: "C:/captures", language: "en", theme: "system" };
  const snapshot = {
    sink: { state: "stopped" },
    sink_path: "C:/sink.exe",
    ffmpeg: "C:/ffmpeg.exe",
    config_error: "config.json could not be read",
    captures: [
      { id: "c1", title: "A capture", url: "https://example.invalid/", state: "ended", muxed: false, bytes: 1024, drm: false },
    ],
    captures_error: null,
    mux: {},
    log: ["a line of the log"],
    browser: {
      host_path: "C:/host.exe",
      bridge_error: null,
      registrations: [{ browser: "chrome", registered: true, path: "C:/manifest.json" }],
      link: { connections: [], approvals: [{ instance: "p-1", browser: "chrome", profile: "Default", connected: true }], store_error: null },
    },
  };
  window.__TAURI__ = {
    core: {
      invoke: async (cmd) => (cmd === "snapshot" ? snapshot : cmd === "get_settings" ? settings : null),
    },
  };
};

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
  await page.waitForFunction(() => document.querySelectorAll("button").length > 0);
  // The page draws what the snapshot says; the first draw is done when the capture is on it.
  await page.waitForTimeout(300);
  check("the page draws without a console error", problems.length === 0, problems.join("; "));
  const text = await page.locator("body").innerText();
  check("the stand-in's data is on the page", text.includes("A capture"), "no capture drawn: axe would check an empty page");

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
  }
} finally {
  await browser.close();
}

if (failed.length) {
  console.error(`page-check: ${failed.length} failed`);
  process.exit(1);
}
console.log("page-check: all green");
