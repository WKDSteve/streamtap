// The app's page in a real Chrome with a stand-in for the Tauri bridge: axe in light and dark, also with a button hovered
// and focused, and with a hint and a capture on the page; then the phones panel with a pairing code and a paired phone. streamtap itself has no Node dependency: Playwright and axe come
// at fixed versions from the folder tools/page-check, which nothing else reads.
//
//   pnpm --dir tools/page-check install --frozen-lockfile
//   node tools/page-check.mjs tools/page-check/node_modules
//
// Then the extension's popup (extension/popup/popup.html) with a stand-in for `chrome.*`: every page of it and the block of
// every target, on a PC and on a phone (narrow, touch), light and dark, in English and once in German: axe, no console
// error, on the phone no sideways scrolling and touch targets of 48 px.
//
// usage: node tools/page-check.mjs <node_modules with @playwright/test and @axe-core/playwright> [--shots <folder>]
//        (--shots: a picture of every popup page is written there, for the eye)

import { createRequire } from "node:module";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";
import { mkdirSync } from "node:fs";
import { bridge } from "./bridge-stub.mjs";
import { chromeStub } from "./chrome-stub.mjs";

const [modules, flag, shots] = process.argv.slice(2);
if (!modules || (flag !== undefined && (flag !== "--shots" || !shots))) {
  console.error("usage: node tools/page-check.mjs <node_modules> [--shots <folder>]");
  process.exit(2);
}
if (shots) mkdirSync(shots, { recursive: true });
const require = createRequire(join(resolve(modules), "x.js"));
const { chromium } = require("@playwright/test");
const { AxeBuilder } = require("@axe-core/playwright");

const here = dirname(fileURLToPath(import.meta.url));
const popup_url = pathToFileURL(join(here, "..", "extension", "popup", "popup.html")).href;
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
    for (const id of ["browser", "settings", "phones"]) document.getElementById(id).hidden = false;
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

  // The phones panel while the sink runs: the paired phone and a pairing code with its QR code. The stand-in's
  // snapshot is the object the page holds, so the sink is "started" by changing it.
  // Axe reads the style sheets of a file:// page itself and logs that it may not; only what the panel adds counts.
  const before = problems.length;
  await page.evaluate(async () => {
    state.snap.sink.state = "running";
    state.phonesOpen = true;
    await pairPhone();
  });
  const shown = await page.evaluate(() => ({
    qr: document.querySelectorAll("#pairing-qr svg.qr path").length,
    box: !document.getElementById("pairing-box").hidden,
    phone: document.getElementById("phones-list").innerText,
    command: document.getElementById("serve-cmd").innerText,
  }));
  check("the phones panel draws without a console error", problems.length === before, problems.slice(before).join("; "));
  check("the phones panel shows the pairing code as a QR code", shown.box && shown.qr === 1);
  check("the phones panel lists the paired phone", shown.phone.includes("A phone"), shown.phone);
  check("the phones panel shows the command for Tailscale Serve", shown.command === "tailscale serve --bg 17650", shown.command);
  for (const theme of ["light", "dark"]) {
    await page.evaluate((name) => {
      document.documentElement.dataset.theme = name;
    }, theme);
    await axe(page, `${theme}, phones with a pairing code`);
    // A QR code is dark on light in both themes (a scanner may not read an inverted one).
    const lightness = await page.evaluate(() => {
      const of = (selector) => {
        const [r, g, b] = getComputedStyle(document.querySelector(selector)).fill.match(/[\d.]+/g).map(Number);
        return r + g + b;
      };
      return { dark: of("#pairing-qr .qr-dark"), light: of("#pairing-qr .qr-light") };
    });
    check(`${theme}: the QR code is dark on light`, lightness.dark < 200 && lightness.light > 650, JSON.stringify(lightness));
  }

  // ---- the extension's popup ----------------------------------------------------------------------------------
  // What a touch target is on the phone: a button, an entry of the start page, a text field, a choice with its label,
  // the head of a fold.
  const TARGETS = "button, nav a, input[type=text], input[type=number], input[type=password], label.choice, summary";
  const devices = [
    { name: "pc", options: { viewport: { width: 420, height: 640 } } },
    { name: "phone", options: { viewport: { width: 360, height: 740 }, hasTouch: true, isMobile: true } },
  ];
  // Every page, and the target page once per block. The stored target decides the block.
  const views = [
    { name: "home", hash: "", target: "file" },
    { name: "target-sink", hash: "#target", target: "" },
    { name: "target-tailnet", hash: "#target", target: "tailnet" },
    { name: "target-device", hash: "#target", target: "video" },
    { name: "sources", hash: "#sources", target: "" },
    { name: "about", hash: "#about", target: "" },
  ];
  // A picture of the whole page. Not Playwright's fullPage: on the phone that drew the page without the touch
  // emulation, in the PC's style. The window is made as tall as the page instead, and the style is asked again.
  const shoot = async (p, device, name) => {
    const height = await p.evaluate(() => document.documentElement.scrollHeight);
    await p.setViewportSize({ width: device.options.viewport.width, height: Math.max(height, 200) });
    await p.screenshot({ path: join(shots, `${name}.png`) });
    check(`${name}: the picture shows the style that was checked`, (await p.evaluate(() => matchMedia("(pointer: coarse)").matches)) === (device.name === "phone"));
  };
  const popup = async (device, scheme, view, locale) => {
    const ctx = await browser.newContext({ ...device.options, colorScheme: scheme, locale });
    const p = await ctx.newPage();
    const errors = [];
    p.on("pageerror", (error) => errors.push(String(error)));
    p.on("console", (message) => {
      if (message.type() === "error") errors.push(message.text());
    });
    await p.addInitScript(chromeStub, { target: view.target });
    await p.goto(popup_url + view.hash);
    await p.waitForFunction(() => document.getElementById("target-line").textContent !== "", null, { timeout: 30_000 });
    // The folds are part of the page.
    await p.evaluate(() => {
      for (const d of document.querySelectorAll("details")) d.open = true;
    });
    return { ctx, p, errors };
  };
  for (const device of devices) {
    for (const scheme of ["light", "dark"]) {
      for (const view of views) {
        const label = `popup ${view.name}, ${device.name}, ${scheme}`;
        const { ctx, p, errors } = await popup(device, scheme, view, "en-US");
        try {
          const seen = await p.evaluate((selector) => {
            const visible = (node) => node.getClientRects().length > 0;
            return {
              pages: [...document.querySelectorAll("main")].filter(visible).map((m) => m.id),
              blocks: [...document.querySelectorAll("section.block")].filter(visible).map((m) => m.id),
              coarse: matchMedia("(pointer: coarse)").matches,
              dark: matchMedia("(prefers-color-scheme: dark)").matches,
              sideways: document.documentElement.scrollWidth - document.documentElement.clientWidth,
              small: [...document.querySelectorAll(selector)]
                .filter(visible)
                .filter((n) => n.getBoundingClientRect().height < 48)
                .map((n) => n.id || n.textContent.trim().slice(0, 20)),
              background: getComputedStyle(document.body).backgroundColor,
            };
          }, TARGETS);
          check(`${label}: one page`, seen.pages.join() === `page-${view.name.split("-")[0]}`, seen.pages.join());
          if (view.hash === "#target") check(`${label}: only the block of the target`, seen.blocks.join() === `block-${view.name.split("-")[1]}`, seen.blocks.join());
          // Before axe: axe reads the style sheet of a file:// page itself and logs that it may not.
          check(`${label}: no console error`, errors.length === 0, errors.join("; "));
          check(`${label}: the colour scheme is the one asked for`, seen.dark === (scheme === "dark") && (seen.background === "rgb(255, 255, 255)") === (scheme === "light"), JSON.stringify(seen));
          if (device.name === "phone") {
            check(`${label}: the phone's style is in effect`, seen.coarse);
            check(`${label}: no sideways scrolling`, seen.sideways <= 0, `${seen.sideways}px`);
            check(`${label}: touch targets of 48 px`, seen.small.length === 0, seen.small.join(", "));
          } else {
            check(`${label}: the PC's style is in effect`, !seen.coarse);
          }
          await axe(p, label);
          if (shots) await shoot(p, device, `${view.name}-${device.name}-${scheme}`);
        } finally {
          await ctx.close();
        }
      }
    }
  }
  // German: the browser's language decides, and the page says which it speaks.
  {
    const { ctx, p } = await popup(devices[1], "light", views[2], "de-DE");
    try {
      const said = await p.evaluate(() => ({
        lang: document.documentElement.lang,
        title: document.querySelector("#page-target h1").textContent,
        back: document.getElementById("back-target").textContent,
      }));
      check("popup in German: language, title and the way back", said.lang === "de" && said.title === "Ziel" && said.back === "Zurück", JSON.stringify(said));
      const sideways = await p.evaluate(() => document.documentElement.scrollWidth - document.documentElement.clientWidth);
      check("popup in German: no sideways scrolling on the phone", sideways <= 0, `${sideways}px`);
      await axe(p, "popup target-tailnet, phone, light, German");
      if (shots) await shoot(p, devices[1], "target-tailnet-phone-light-de");
      // The way there and back in a real browser: a tap on an entry, then the browser's own back.
      await p.goto(popup_url);
      await p.click("#nav-sources");
      check("popup: a tap on an entry opens its page", await p.evaluate(() => !document.getElementById("page-sources").hidden && document.getElementById("page-home").hidden));
      check("popup: focus is on the way back", await p.evaluate(() => document.activeElement.id === "back-sources"));
      await p.goBack();
      check("popup: the browser's back key returns to the start page", await p.evaluate(() => !document.getElementById("page-home").hidden && document.getElementById("page-sources").hidden));
      await p.click("#nav-about");
      await p.click("#back-about");
      const home = await p
        .waitForFunction(() => !document.getElementById("page-home").hidden && location.hash === "", null, { timeout: 5000 })
        .then(() => true, () => false);
      check("popup: the page's own back returns to the start page", home);
    } finally {
      await ctx.close();
    }
  }
} finally {
  await browser.close();
}

if (failed.length) {
  console.error(`page-check: ${failed.length} failed`);
  process.exit(1);
}
console.log("page-check: all green");
