// Checks the desktop app's UI without a webview: the i18n tables (same keys and
// placeholders in English and German, t() and the language choice), that every key
// index.html and app.js use exists, and that both themes in style.css keep readable
// contrast (WCAG ratios) for every text and control colour pair the UI uses.
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
for (const m of app.matchAll(/\?\s*"(\w+)"\s*:\s*"(\w+)"\s*,\s*\{/g)) {
  check(m[1] in en && m[2] in en, `app.js uses unknown key ${m[1]} or ${m[2]}`);
}
const regKeys = app.match(/const REG_KEYS = \{([^}]*)\}/);
check(regKeys, "REG_KEYS found");
if (regKeys) for (const m of regKeys[1].matchAll(/:\s*"(\w+)"/g)) check(m[1] in en, `REG_KEYS uses unknown key ${m[1]}`);
for (const reason of ["exit-1", "exit-2", "exit-3", "exit-4", "exit-5", "exit-6", "exit-unexpected", "signal", "spawn", "aborted", "wait"]) {
  check(`reason.${reason}` in en, `no text for the mux reason ${reason}`);
}

// ---- themes -------------------------------------------------------------

const css = read("style.css");
const tokens = (block) => Object.fromEntries([...block.matchAll(/--([\w-]+):\s*(#[0-9a-fA-F]{6})/g)].map((m) => [m[1], m[2].toLowerCase()]));
const light = tokens(css.match(/^:root \{([^}]*)\}/m)?.[1] ?? "");
const darkMedia = tokens(css.match(/:root:not\(\[data-theme="light"\]\) \{([^}]*)\}/)?.[1] ?? "");
const darkForced = tokens(css.match(/:root\[data-theme="dark"\] \{([^}]*)\}/)?.[1] ?? "");
check(Object.keys(light).length >= 14, "light tokens found");
check(Object.keys(darkMedia).length >= 14, "dark tokens (system) found");
check(JSON.stringify(darkMedia) === JSON.stringify(darkForced), "the system dark and the forced dark tokens are identical");
check(JSON.stringify(Object.keys(light).sort()) === JSON.stringify(Object.keys(darkMedia).sort()), "both themes set the same tokens");

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
  ["danger", "surface", 4.5, "danger buttons, DRM badges"],
  ["ok", "surface", 4.5, "live badge, connected"],
  ["accent", "surface", 4.5, "muxed badge"],
  ["bg", "text", 4.5, "toast"],
  ["text", "bg", 4.5, "inputs, selects, sink log"],
  ["focus", "surface", 3, "focus ring"],
  ["focus", "bg", 3, "focus ring"],
  ["muted", "surface", 3, "stopped dot"],
];
for (const [name, theme] of [["light", light], ["dark", darkMedia]]) {
  for (const [fg, bg, min, where] of pairs) {
    const r = ratio(theme[fg] ?? "#000000", theme[bg] ?? "#000000");
    check(r >= min, `${name}: ${fg} on ${bg} (${where}) has ${r.toFixed(2)}, needs ${min}`);
  }
}

if (failures) {
  console.error(`${failures} check(s) failed`);
  process.exit(1);
}
console.log("app UI: i18n tables, keys and theme contrast ok");
