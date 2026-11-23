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

// PAGE_CHECK_CHROME: the path of a Chrome or Chromium binary where no installed Chrome is (a container).
const browser = await chromium.launch(process.env.PAGE_CHECK_CHROME ? { executablePath: process.env.PAGE_CHECK_CHROME, headless: true } : { channel: "chrome", headless: true });
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

  // What is typed and not saved survives a redraw, also when the focus is elsewhere (FORM-07).
  await page.fill("#port", "17999");
  await page.fill("#dir", "D:/typed");
  await page.fill("#tailnet", "typed.example.ts.net");
  await page.locator("#settings-toggle").focus();
  await page.evaluate(() => {
    state.signature = "";
    render(state.snap);
  });
  const typed = await page.evaluate(() => [$("port").value, $("dir").value, $("tailnet").value]);
  check("app: typed settings survive a redraw", typed.join() === "17999,D:/typed,typed.example.ts.net", typed.join());
  // A saved field is the stored value again (the values of the stand-in are typed back, so the rest of the check sees them).
  await page.fill("#port", "17650");
  await page.fill("#dir", "C:/captures");
  await page.evaluate(async () => {
    document.getElementById("settings-form").requestSubmit();
    await new Promise((resolve) => requestAnimationFrame(resolve));
  });
  const saved = await page.evaluate(async () => {
    await refresh();
    return [state.dirty.has("port"), state.dirty.has("dir"), state.dirty.has("tailnet")].join();
  });
  check("app: a saved setting is no longer an edit", saved === "false,false,true", saved);

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

  const smallFields = await page.evaluate(() =>
    [...document.querySelectorAll("input:not([type=checkbox]):not([type=radio]), select")]
      .filter((n) => n.getClientRects().length > 0 && parseFloat(getComputedStyle(n).fontSize) < 16)
      .map((n) => `${n.id} ${getComputedStyle(n).fontSize}`)
  );
  const appFigures = await page.evaluate(() => ({
    missing: [".card .meta", ".card table", "#captures-count", "#sink-text"].filter((sel) => [...document.querySelectorAll(sel)].some((n) => n.getClientRects().length > 0 && !getComputedStyle(n).fontVariantNumeric.includes("tabular-nums"))),
    plain: [...document.querySelectorAll("h1, h2, h3")].filter((n) => n.getClientRects().length > 0 && getComputedStyle(n).textWrap !== "balance").map((n) => n.textContent.slice(0, 20)),
  }));
  check("app: figures are tabular and headings balanced", appFigures.missing.length === 0 && appFigures.plain.length === 0, JSON.stringify(appFigures));
  check("app: fields of 16 px", smallFields.length === 0, smallFields.join(", "));
  const tinyTargets = await page.evaluate(() =>
    [...document.querySelectorAll("button, summary, input:not([type=hidden]), select, a[href]")]
      .filter((n) => n.getClientRects().length > 0 && !n.classList.contains("skip"))
      .map((n) => [n, n.getBoundingClientRect()])
      .filter(([, r]) => r.height < 24 || r.width < 24)
      .map(([n, r]) => `${n.id || n.textContent.trim().slice(0, 20)} ${Math.round(r.width)}x${Math.round(r.height)}`)
  );
  check("app: targets of 24 px", tinyTargets.length === 0, tinyTargets.join(", "));
  // A long error text and a long host on a narrow window (CSS-08): the toast and the tab list wrap inside the window.
  await page.setViewportSize({ width: 420, height: 800 });
  await page.evaluate((word) => {
    state.signature = "";
    render(state.snap);
    toast(`Mux: ${word}`);
    for (const n of document.querySelectorAll(".tabrow .tabtext .muted")) n.textContent = word;
  }, "x".repeat(120));
  const wide = await page.evaluate(() => {
    const rect = document.getElementById("toast").getBoundingClientRect();
    const muted = [...document.querySelectorAll(".tabrow .tabtext .muted")];
    return {
      toast: Math.round(Math.max(0, rect.right - innerWidth, -rect.left)),
      toastScroll: document.getElementById("toast").scrollWidth - document.getElementById("toast").clientWidth,
      tabs: muted.length,
      tabOver: muted.map((n) => n.scrollWidth - n.clientWidth).filter((x) => x > 0),
      sideways: document.documentElement.scrollWidth - document.documentElement.clientWidth,
    };
  });
  check("app: a toast with a path of 120 characters stays in the window", wide.toast === 0 && wide.toastScroll <= 0, JSON.stringify(wide));
  check("app: a long host in the tab list wraps", wide.tabs > 0 && wide.tabOver.length === 0 && wide.sideways <= 0, JSON.stringify(wide));
  await page.setViewportSize({ width: 1280, height: 720 });

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
              tiny: [...document.querySelectorAll(selector)]
                .filter(visible)
                .filter((n) => n.getBoundingClientRect().height < 24 || n.getBoundingClientRect().width < 24)
                .map((n) => `${n.id || n.textContent.trim().slice(0, 20)} ${Math.round(n.getBoundingClientRect().width)}x${Math.round(n.getBoundingClientRect().height)}`),
              fields: [...document.querySelectorAll("input:not([type=checkbox]):not([type=radio]), textarea, select")]
                .filter(visible)
                .filter((n) => parseFloat(getComputedStyle(n).fontSize) < 16)
                .map((n) => `${n.id} ${getComputedStyle(n).fontSize}`),
              tokenSpellcheck: document.getElementById("token").getAttribute("spellcheck"),
            };
          }, TARGETS);
          check(`${label}: one page`, seen.pages.join() === `page-${view.name.split("-")[0]}`, seen.pages.join());
          if (view.hash === "#target") check(`${label}: only the block of the target`, seen.blocks.join() === `block-${view.name.split("-")[1]}`, seen.blocks.join());
          // A field is at least 16 px (1rem) on every device: a smaller one is zoomed in by iOS and hard to read (FORM-05).
          check(`${label}: fields of 16 px`, seen.fields.length === 0, seen.fields.join(", "));
          // Figures of a size or count are tabular, headings are balanced where the browser knows it (CSS-09).
          const figures = await p.evaluate(() => {
            const wanted = ["table td", "#tab-line", "#target-line", "#file-state"];
            const missing = wanted.filter((sel) => [...document.querySelectorAll(sel)].some((n) => n.getClientRects().length > 0 && !getComputedStyle(n).fontVariantNumeric.includes("tabular-nums")));
            const balanced = CSS.supports("text-wrap", "balance") ? [...document.querySelectorAll("h1")].filter((n) => n.getClientRects().length > 0 && getComputedStyle(n).textWrap !== "balance") : [];
            return { missing, balanced: balanced.map((n) => n.textContent) };
          });
          check(`${label}: figures are tabular and headings balanced`, figures.missing.length === 0 && figures.balanced.length === 0, JSON.stringify(figures));
          // A token is not a word: the attribute says it, whatever a browser does with a password field by itself.
          check(`${label}: the token field has no spell check`, seen.tokenSpellcheck === "false", String(seen.tokenSpellcheck));
          // Before axe: axe reads the style sheet of a file:// page itself and logs that it may not.
          check(`${label}: no console error`, errors.length === 0, errors.join("; "));
          check(`${label}: the colour scheme is the one asked for`, seen.dark === (scheme === "dark") && (seen.background === "rgb(255, 255, 255)") === (scheme === "light"), JSON.stringify(seen));
          if (device.name === "phone") {
            check(`${label}: the phone's style is in effect`, seen.coarse);
            check(`${label}: no sideways scrolling`, seen.sideways <= 0, `${seen.sideways}px`);
            check(`${label}: touch targets of 48 px`, seen.small.length === 0, seen.small.join(", "));
          } else {
            check(`${label}: the PC's style is in effect`, !seen.coarse);
            // With a mouse a target is at least 24 px high and wide (WCAG 2.5.8, ACC-04); the phone's 48 px is checked above.
            check(`${label}: targets of 24 px`, seen.tiny.length === 0, seen.tiny.join(", "));
          }
          await axe(p, label);
          if (shots) await shoot(p, device, `${view.name}-${device.name}-${scheme}`);
          // Last, it changes the page: a path of 120 characters without a space (a file name, a URL) in every line of state
          // must wrap inside its box, not push the page sideways (CSS-08).
          if (scheme === "light") {
            const wide = await p.evaluate((word) => {
              const lines = [...document.querySelectorAll("main:not([hidden]) :is(div, p, span)[id]")].filter((n) => n.getClientRects().length > 0 && n.children.length === 0);
              for (const n of lines) n.textContent = word;
              return { names: lines.map((n) => n.id), over: lines.filter((n) => n.scrollWidth > n.clientWidth).map((n) => `${n.id} +${n.scrollWidth - n.clientWidth}px`), sideways: document.documentElement.scrollWidth - document.documentElement.clientWidth };
            }, "x".repeat(120));
            check(`${label}: ${wide.names.length} lines of state wrap a path of 120 characters`, wide.names.length > 0 && wide.over.length === 0 && wide.sideways <= 0, `${wide.over.join(", ")}; the page scrolls ${wide.sideways}px`);
          }
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
      // The way there and back in a real browser: a tap on an entry, then the browser's own back. A tap changes
      // the hash; the page follows in the `hashchange` event, after the click has returned: each check waits for
      // the state it names (up to 5 s) instead of reading it at once, which a loaded runner loses.
      const until = (fn) => p.waitForFunction(fn, null, { timeout: 5000 }).then(() => true, () => false);
      await p.goto(popup_url);
      await p.click("#nav-sources");
      check("popup: a tap on an entry opens its page", await until(() => !document.getElementById("page-sources").hidden && document.getElementById("page-home").hidden));
      check("popup: focus is on the way back", await until(() => document.activeElement.id === "back-sources"));
      await p.goBack();
      check("popup: the browser's back key returns to the start page", await until(() => !document.getElementById("page-home").hidden && document.getElementById("page-sources").hidden));
      await p.click("#nav-about");
      await until(() => !document.getElementById("page-about").hidden);
      await p.click("#back-about");
      const home = await until(() => !document.getElementById("page-home").hidden && location.hash === "");
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
