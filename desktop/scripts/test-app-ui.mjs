// Checks the desktop app's UI without a webview: the i18n tables (same keys and
// placeholders in English and German, t() and the language choice), that every key
// index.html and app.js use exists, and that both themes in style.css keep readable
// contrast (WCAG ratios) for every text and control colour pair the UI uses. Then
// app.js runs against a fake DOM and fake commands (the revoke flow).
//
//   node desktop/scripts/test-app-ui.mjs

import { readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import vm from "node:vm";

const ui = join(dirname(fileURLToPath(import.meta.url)), "..", "crates", "streamtap-app", "ui");
const read = (name) => readFileSync(join(ui, name), "utf8");

let failures = 0;
const check = (ok, what) => {
  if (!ok) {
    failures++;
    console.error(`FAIL ${what}`);
  }
};

// ---- i18n ---------------------------------------------------------------

const I18N = vm.runInNewContext(`${read("i18n.js")}\nI18N;`, {});
const { en, de } = I18N.tables;
const placeholders = (s) => [...s.matchAll(/\{(\w+)\}/g)].map((m) => m[1]).sort().join(",");

for (const key of Object.keys(en)) {
  check(key in de, `German table lacks ${key}`);
  check(typeof en[key] === "string" && en[key].length > 0, `English ${key} is empty`);
  if (key in de) {
    check(typeof de[key] === "string" && de[key].length > 0, `German ${key} is empty`);
    check(placeholders(en[key]) === placeholders(de[key]), `placeholders of ${key} differ: ${placeholders(en[key])} / ${placeholders(de[key])}`);
  }
}
for (const key of Object.keys(de)) check(key in en, `English table lacks ${key}`);
check(Object.values(de).join("").includes("ü") && Object.values(de).join("").includes("ß"), "German texts carry their umlauts");

check(I18N.resolve("system", "de-AT") === "de", "system + de-AT is German");
check(I18N.resolve("system", "DE") === "de", "system + DE is German");
check(I18N.resolve("system", "en-US") === "en", "system + en-US is English");
check(I18N.resolve("system", "fr-FR") === "en", "system + another language is English");
check(I18N.resolve("system", undefined) === "en", "system without a language is English");
check(I18N.resolve("de", "en-US") === "de", "an explicit choice wins");
check(I18N.resolve("en", "de-DE") === "en", "an explicit choice wins");
check(I18N.resolve("xx", "de-DE") === "de", "an unknown choice behaves like system");

I18N.setLanguage("de");
check(I18N.lang === "de", "setLanguage(de)");
check(I18N.t("sinkRunning", { port: 17650 }) === "Sink: läuft auf 127.0.0.1:17650", "t() fills a placeholder");
check(I18N.t("noSuchKey") === "noSuchKey", "an unknown key shows itself");
check(I18N.t("muxFailed", {}) === "Muxen fehlgeschlagen: {meaning}", "a missing parameter stays visible");
check(I18N.t("muxFailed", { meaning: "$& {x}" }) === "Muxen fehlgeschlagen: $& {x}", "parameters are inserted literally");
I18N.setLanguage("fr");
check(I18N.lang === "en", "an unknown language falls back to English");

const html = read("index.html");
for (const m of html.matchAll(/data-i18n(?:-aria-label|-title)?="([^"]+)"/g)) check(m[1] in en, `index.html uses unknown key ${m[1]}`);
const app = read("app.js");
for (const m of app.matchAll(/\bt\(\s*"([^"]+)"/g)) check(m[1] in en, `app.js uses unknown key ${m[1]}`);
// The confirm dialog's heading and button keys (ask({ heading: "...", ok: "..." })).
const askKeys = [...app.matchAll(/\b(?:heading|ok):\s*"([^"]+)"/g)];
check(askKeys.length >= 4, "confirm dialog keys found");
for (const m of askKeys) check(m[1] in en, `app.js uses unknown key ${m[1]}`);
for (const m of app.matchAll(/\?\s*"(\w+)"\s*:\s*"(\w+)"\s*,\s*\{/g)) {
  check(m[1] in en && m[2] in en, `app.js uses unknown key ${m[1]} or ${m[2]}`);
}
const regKeys = app.match(/const REG_KEYS = \{([^}]*)\}/);
check(regKeys, "REG_KEYS found");
if (regKeys) for (const m of regKeys[1].matchAll(/:\s*"(\w+)"/g)) check(m[1] in en, `REG_KEYS uses unknown key ${m[1]}`);
const groupKeys = app.match(/const GROUP_KEYS = \{([^}]*)\}/);
check(groupKeys, "GROUP_KEYS found");
if (groupKeys) {
  const ids = [...groupKeys[1].matchAll(/"?([\w-]+)"?:\s*"(\w+)"/g)];
  check(
    ids.map((m) => m[1]).sort().join(",") === "all-app-packages,authenticated-users,domain-users,everyone,guests,interactive,local,users",
    "GROUP_KEYS covers the eight broad groups the app reports"
  );
  for (const m of ids) check(m[2] in en, `GROUP_KEYS uses unknown key ${m[2]}`);
}
for (const reason of ["exit-1", "exit-2", "exit-3", "exit-4", "exit-5", "exit-6", "exit-unexpected", "signal", "spawn", "aborted", "wait"]) {
  check(`reason.${reason}` in en, `no text for the mux reason ${reason}`);
}

// ---- themes -------------------------------------------------------------

// The colours come from palette.css (generated from the icon by the app icons tool): every token is
// light-dark(<light>, <dark>). style.css writes no colour of its own; it only derives the tinted notice
// surfaces with color-mix() in srgb, which the test computes the same way.
const palette = read("palette.css");
const css = read("style.css");
const light = {};
const dark = {};
for (const m of palette.matchAll(/--([\w-]+):\s*light-dark\(\s*(#[0-9a-fA-F]{6})\s*,\s*(#[0-9a-fA-F]{6})\s*\)/g)) {
  light[m[1]] = m[2].toLowerCase();
  dark[m[1]] = m[3].toLowerCase();
}
for (const name of ["bg", "surface", "text", "muted", "line", "accent", "accent-text", "tertiary", "focus", "ok", "warn", "bad"]) {
  check(name in light, `palette.css has --${name}`);
}
check(/GENERATED/.test(palette), "palette.css is the generated file");
check(
  html.indexOf('href="palette.css"') >= 0 && html.indexOf('href="palette.css"') < html.indexOf('href="style.css"'),
  "index.html loads palette.css before style.css"
);
check(!/#[0-9a-fA-F]{3,8}\b|\b(?:rgba?|hsla?|oklch|oklab|lab|lch|hwb|color)\(/i.test(css.replace(/\/\*[\s\S]*?\*\//g, "")), "style.css writes no colour literal");
const ownTokens = [...css.matchAll(/^\s*--([\w-]+):/gm)].map((m) => m[1]).sort().join(",");
check(ownTokens === "err-bg,info-bg,warn-bg", `style.css defines only the notice surfaces (${ownTokens})`);
for (const gone of ["--border", "--danger"]) check(!css.includes(`var(${gone})`), `style.css no longer uses ${gone}`);

const mixes = [...css.matchAll(/--([\w-]+):\s*color-mix\(in srgb, var\(--([\w-]+)\) (\d+)%, var\(--([\w-]+)\)\)/g)];
check(mixes.length === 3, "three notice surfaces are mixed");
const mix = (a, b, p) => {
  const ch = (h, i) => parseInt(h.slice(i, i + 2), 16);
  return "#" + [1, 3, 5].map((i) => Math.round(ch(a, i) * p + ch(b, i) * (1 - p)).toString(16).padStart(2, "0")).join("");
};
for (const theme of [light, dark]) {
  for (const [, name, a, pct, b] of mixes) theme[name] = mix(theme[a], theme[b], Number(pct) / 100);
}
const darkMedia = dark;
// streamtap's accent is a red near --bad: an info notice and the muxed badge take the tertiary colour, so that
// nothing but an error looks like one.
check(/--info-bg: color-mix\(in srgb, var\(--tertiary\)/.test(css), "the info notice is not mixed from the red accent");
check(/\.badge\.muxed \{ color: var\(--tertiary\)/.test(css), "the muxed badge is not red");

const lum = (hex) => {
  const c = [1, 3, 5].map((i) => parseInt(hex.slice(i, i + 2), 16) / 255).map((v) => (v <= 0.03928 ? v / 12.92 : ((v + 0.055) / 1.055) ** 2.4));
  return 0.2126 * c[0] + 0.7152 * c[1] + 0.0722 * c[2];
};
const ratio = (a, b) => {
  const [hi, lo] = [lum(a), lum(b)].sort((x, y) => y - x);
  return (hi + 0.05) / (lo + 0.05);
};

// [foreground, background, minimum, where]. 4.5 for text, 3 for large text,
// borders of controls and the status dot.
const pairs = [
  ["text", "bg", 4.5, "body text"],
  ["text", "surface", 4.5, "panels, cards, buttons, dialog"],
  ["muted", "surface", 4.5, "hints, meta, facts"],
  ["muted", "bg", 4.5, "empty state"],
  ["text", "info-bg", 4.5, "info notice, mux running"],
  ["text", "warn-bg", 4.5, "warning notice, pairing prompt"],
  ["text", "err-bg", 4.5, "error notice, mux failed"],
  ["accent-text", "accent", 4.5, "primary buttons"],
  ["bad", "surface", 4.5, "danger buttons, DRM badges"],
  ["ok", "surface", 4.5, "live badge, connected"],
  ["tertiary", "surface", 4.5, "muxed badge"],
  ["bg", "text", 4.5, "toast"],
  ["text", "bg", 4.5, "inputs, selects, sink log"],
  ["focus", "surface", 3, "focus ring"],
  ["accent", "surface", 3, "hovered button border (buttons sit on the bar, panels and cards)"],
  ["focus", "bg", 3, "focus ring"],
  ["muted", "surface", 3, "stopped dot"],
];
for (const [name, theme] of [["light", light], ["dark", darkMedia]]) {
  for (const [fg, bg, min, where] of pairs) {
    const r = ratio(theme[fg] ?? "#000000", theme[bg] ?? "#000000");
    check(r >= min, `${name}: ${fg} on ${bg} (${where}) has ${r.toFixed(2)}, needs ${min}`);
  }
}

// ---- app.js against a fake DOM and fake commands --------------------------

// Just enough DOM for app.js: elements by id (made on first use), created elements,
// listeners that the test fires by hand. Nothing is laid out or rendered.
class FakeEl {
  constructor(id = "") {
    this.id = id;
    this.textContent = "";
    this.value = "";
    this.hidden = false;
    this.disabled = false;
    this.type = "";
    this.className = "";
    this.returnValue = "";
    this.scrollTop = 0;
    this.clientHeight = 0;
    this.scrollHeight = 0;
    this.dataset = {};
    this.children = [];
    this.attrs = {};
    this.listeners = {};
    this.classList = { toggle() {}, add() {}, remove() {} };
  }
  addEventListener(type, fn) {
    (this.listeners[type] ??= []).push(fn);
  }
  fire(type) {
    for (const fn of this.listeners[type] ?? []) fn({ preventDefault() {} });
  }
  setAttribute(k, v) {
    this.attrs[k] = String(v);
  }
  append(...c) {
    this.children.push(...c);
  }
  replaceChildren(...c) {
    this.children = c;
  }
  querySelectorAll() {
    return [];
  }
  showModal() {
    this.open = true;
  }
  focus() {}
  select() {}
}

const SETTINGS = { port: 17650, token: "old-token", captures_dir: "C:/c", language: "en", theme: "system" };
const SNAPSHOT = {
  sink: { state: "stopped" },
  sink_path: "C:/sink.exe",
  ffmpeg: "C:/ffmpeg.exe",
  config_error: null,
  captures: [],
  captures_error: null,
  mux: {},
  log: [],
  browser: {
    host_path: "C:/host.exe",
    bridge_error: null,
    registrations: [],
    link: { connections: [], approvals: [{ instance: "p-1", browser: "chrome", profile: "Default", connected: true }], store_error: null },
  },
};

// Loads app.js with `commands` as the app's Tauri commands; returns the element
// lookup, what was copied to the clipboard and a way to run code inside the page.
const loadApp = (commands) => {
  const els = new Map();
  const byId = (id) => {
    if (!els.has(id)) els.set(id, new FakeEl(id));
    return els.get(id);
  };
  const copied = [];
  const context = vm.createContext({
    console,
    window: {
      addEventListener() {},
      __TAURI__: {
        core: {
          invoke: async (cmd, args) => {
            if (!(cmd in commands)) throw new Error(`unexpected command ${cmd}`);
            return structuredClone(await commands[cmd](args));
          },
        },
      },
    },
    document: {
      documentElement: new FakeEl("html"),
      activeElement: null,
      getElementById: byId,
      createElement: () => new FakeEl(),
      querySelectorAll: () => [],
      querySelector: () => null,
    },
    navigator: { language: "en-US", clipboard: { writeText: async (text) => void copied.push(text) } },
    CSS: { escape: (s) => s },
    setInterval: () => 0,
    setTimeout: () => 0,
    clearTimeout: () => {},
  });
  vm.runInContext(read("i18n.js"), context);
  vm.runInContext(read("app.js"), context);
  return { byId, copied, page: (code) => vm.runInContext(code, context) };
};

const settle = async () => {
  for (let i = 0; i < 20; i++) await new Promise((r) => setImmediate(r));
};

// After a revoke the token field and Copy give the new token, also when the revoke
// reports an error after the token changed (the sink did not start again).
for (const failAfterRotation of [false, true]) {
  const backend = { ...SETTINGS };
  const app = loadApp({
    get_settings: () => backend,
    snapshot: () => SNAPSHOT,
    browser_revoke: ({ instance }) => {
      check(instance === "p-1", "browser_revoke gets the instance");
      backend.token = "new-token";
      if (failAfterRotation) throw "revoked and the token changed, but the sink did not start again";
      return null;
    },
  });
  await settle();
  const what = failAfterRotation ? "revoke that fails after the rotation" : "revoke";
  check(app.byId("token").value === "old-token", `${what}: the token field shows the token first`);
  app.page(`askRevoke(${JSON.stringify(SNAPSHOT.browser.link.approvals[0])})`);
  const dialog = app.byId("confirm");
  check(dialog.open === true, `${what}: the revoke asks first`);
  dialog.returnValue = "ok";
  dialog.fire("close");
  await settle();
  check(app.byId("token").value === "new-token", `${what}: the token field shows the new token, not ${app.byId("token").value}`);
  app.byId("token-copy").fire("click");
  await settle();
  check(app.copied.at(-1) === "new-token", `${what}: Copy gives the new token, not ${app.copied.at(-1)}`);
}

// The settings panel warns when broad groups may read config.json (SRV-14), with the
// group names translated, and says when the check itself failed. No warning otherwise.
const texts = (node) => (node?.children ?? []).map((c) => c.textContent);
for (const [language, access, want] of [
  ["en", { state: "exposed", groups: ["everyone", "authenticated-users"] }, ["Other accounts can read config.json.", "Everyone, Authenticated Users may read"]],
  ["de", { state: "exposed", groups: ["users"] }, ["Andere Konten können config.json lesen.", "Benutzer dürfen"]],
  ["en", { state: "unknown", error: "access denied" }, ["Access to config.json was not checked.", "access denied"]],
  ["en", null, null],
]) {
  const app = loadApp({ get_settings: () => ({ ...SETTINGS, language }), snapshot: () => ({ ...SNAPSHOT, config_access: access }) });
  await settle();
  const box = app.byId("config-access");
  const what = `config access ${JSON.stringify(access)} (${language})`;
  if (!want) {
    check(box.children.length === 0, `${what}: no warning`);
    continue;
  }
  check(box.children.length === 1, `${what}: one warning, not ${box.children.length}`);
  const [title, body] = texts(box.children[0]);
  check(title === want[0], `${what}: title ${title}`);
  check(typeof body === "string" && body.includes(want[1]), `${what}: body ${body}`);
}

if (failures) {
  console.error(`${failures} check(s) failed`);
  process.exit(1);
}
console.log("app UI: i18n tables, keys, theme contrast, the revoke flow and the config access warning ok");
