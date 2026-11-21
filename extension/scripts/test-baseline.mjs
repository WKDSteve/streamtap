#!/usr/bin/env node
// The Baseline guard of the extension. Its target is Baseline 2023 (extension/.mwgrc) with the manifest minimums
// Chrome 116 and Firefox 128 (decided 2026-11-21, CLAUDE.md "Web platform"). Baseline 2023 holds features that
// reached Chrome only in 117-120: a Chrome 116 parses or runs them wrong (a nested CSS rule is dropped, URL.canParse
// is undefined). They may be used only behind feature detection with a fallback, or with a polyfill shipped in the
// extension (rules-vault BASE-02; the extension loads no remote code).
//
// This test reads every script, style sheet and page the extension ships (src/, popup/) and fails on any use of
// such a feature that carries no decision. A use that is guarded (feature detection plus fallback, or a polyfill
// that runs only after the check) is marked on its line or the line above with
//     baseline-guard: <feature id> <why it is safe>
// and its fallback gets a test of its own that is red without it. The test also checks its own detectors against a
// sample with and a sample without each feature, so a broken pattern cannot pass silently.
//
// The list: web-features 3.47.0 (npm), every feature of kind "feature" with baseline_low_date <= 2023-12-31 and
// support.chrome > 116. That gave exactly these twelve; Firefox 128 has all of them (they are Baseline 2023, so
// Firefox had them by the end of 2023). Features newer than Baseline 2023 are outside the target altogether.
// Usage: node extension/scripts/test-baseline.mjs   (exit 0 when all checks pass)
import { readFileSync, readdirSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, join, relative } from "node:path";

const here = dirname(fileURLToPath(import.meta.url));
const root = join(here, "..");
const failures = [];
const check = (ok, what) => {
  console.log(`${ok ? "ok  " : "FAIL"} ${what}`);
  if (!ok) failures.push(what);
};

// ---- text without comments and strings ---------------------------------------------------------------------------
// Comments become spaces (newlines kept, so line numbers stay). In scripts the contents of strings stay: a selector
// or a media query in a string is code (querySelector(":user-valid"), matchMedia("(scripting: none)")).

/** @param {string} s */
const blank = (s) => s.replace(/[^\n]/g, " ");

/** Strips the comments of a script; a regex literal is told from a division by the token in front of it. */
function stripJs(src) {
  let out = "";
  let i = 0;
  let prev = ""; // the last significant character outside comments
  while (i < src.length) {
    const c = src[i];
    const n = src[i + 1];
    if (c === "/" && n === "/") {
      const end = src.indexOf("\n", i);
      const stop = end < 0 ? src.length : end;
      out += blank(src.slice(i, stop));
      i = stop;
    } else if (c === "/" && n === "*") {
      const end = src.indexOf("*/", i + 2);
      const stop = end < 0 ? src.length : end + 2;
      out += blank(src.slice(i, stop));
      i = stop;
    } else if (c === '"' || c === "'" || c === "`") {
      let j = i + 1;
      while (j < src.length && src[j] !== c) j += src[j] === "\\" ? 2 : 1;
      out += src.slice(i, j + 1);
      i = j + 1;
      prev = c;
    } else if (c === "/" && (prev === "" || "(,=:[!&|?{};+-*%<>~^".includes(prev) || /\b(return|typeof|case|in|of)$/.test(out.trimEnd()))) {
      // a regex literal: copied as it is, so a "//" or "/*" inside it is not taken for a comment
      let j = i + 1;
      let inClass = false;
      while (j < src.length && src[j] !== "\n") {
        if (src[j] === "\\") j += 2;
        else {
          if (src[j] === "[") inClass = true;
          else if (src[j] === "]") inClass = false;
          else if (src[j] === "/" && !inClass) break;
          j += 1;
        }
      }
      out += src.slice(i, j + 1);
      i = j + 1;
      prev = "/";
    } else {
      out += c;
      if (!/\s/.test(c)) prev = c;
      i += 1;
    }
  }
  return out;
}

/** Strips the comments of a style sheet. */
const stripCss = (src) => src.replace(/\/\*[\s\S]*?\*\//g, blank);

/** Strips the comments of a page and blanks what is not markup or CSS: the scripts. */
const stripHtml = (src) => src.replace(/<!--[\s\S]*?-->/g, blank).replace(/(<script\b[^>]*>)([\s\S]*?)(<\/script>)/gi, (m, a, b, c) => a + blank(b) + c);

// ---- CSS nesting -----------------------------------------------------------------------------------------------------
// A style rule inside a style rule, or "&". At-rules that hold rules (@media, @supports, @layer, @container, ...) are
// groups; @font-face, @page and the like hold declarations and count as style rules.

const GROUP_AT_RULES = /^@(media|supports|layer|container|scope|document|-moz-document|keyframes|-webkit-keyframes|starting-style)\b/i;

/** @returns {number[]} offsets in `css` (comments already stripped) where a nested rule starts */
function nestingAt(css) {
  const hits = [];
  const stack = []; // "group" | "style"
  let start = 0; // where the current prelude starts
  for (let i = 0; i < css.length; i += 1) {
    const c = css[i];
    if (c === '"' || c === "'") {
      let j = i + 1;
      while (j < css.length && css[j] !== c) j += css[j] === "\\" ? 2 : 1;
      i = j;
    } else if (c === "&") {
      hits.push(i);
    } else if (c === "{") {
      const prelude = css.slice(start, i).trim();
      if (stack[stack.length - 1] === "style") hits.push(start + css.slice(start, i).search(/\S/));
      stack.push(GROUP_AT_RULES.test(prelude) ? "group" : "style");
      start = i + 1;
    } else if (c === "}") {
      stack.pop();
      start = i + 1;
    } else if (c === ";") {
      start = i + 1;
    }
  }
  return hits;
}

// ---- the features ----------------------------------------------------------------------------------------------------
// kinds: which texts a pattern is matched against. "css" covers style sheets and the <style> of pages, "html" the
// markup, "js" scripts (with their strings: selectors and media queries passed to the DOM).

const FEATURES = [
  {
    id: "subgrid",
    chrome: 117,
    kinds: ["css", "js"],
    re: /\bsubgrid\b/g,
    yes: ".a { grid-template-columns: subgrid; }",
    no: ".a { grid-template-columns: 1fr 1fr; }",
  },
  {
    id: "cap",
    chrome: 118,
    kinds: ["css", "js"],
    re: /(?<![\w-])\d*\.?\d+cap\b/g,
    yes: ".a { height: 1.2cap; }",
    no: ".a { height: 12px; text-transform: capitalize; }",
  },
  {
    id: "search",
    chrome: 118,
    kinds: ["html", "js"],
    re: /<search\b|createElement\(\s*["'`]search["'`]/gi,
    yes: '<search><input type="search"></search>',
    no: '<form role="search"><input type="search"></form>',
  },
  {
    id: "clip-path-boxes",
    chrome: 119,
    kinds: ["css", "js"],
    re: /clip-?[pP]ath\b[^;{}]*?\b(margin|border|padding|content|fill|stroke|view)-box\b/g,
    yes: ".a { clip-path: border-box; }",
    no: ".a { clip-path: inset(0 round 4px); box-sizing: border-box; }",
  },
  {
    id: "storage-access",
    chrome: 119,
    kinds: ["js"],
    re: /\b(requestStorageAccess|hasStorageAccess)\b/g,
    yes: "await document.requestStorageAccess();",
    no: "await chrome.storage.local.get({});",
  },
  {
    id: "user-pseudos",
    chrome: 119,
    kinds: ["css", "js"],
    re: /:user-(valid|invalid)\b/g,
    yes: "input:user-invalid { border-color: red; }",
    no: "input:invalid { border-color: red; }",
  },
  {
    id: "dir-pseudo",
    chrome: 120,
    kinds: ["css", "js"],
    re: /:dir\(/g,
    yes: "p:dir(rtl) { text-align: right; }",
    no: 'p[dir="rtl"] { text-align: right; }',
  },
  {
    id: "exp-functions",
    chrome: 120,
    kinds: ["css"],
    re: /(?<![\w-])(pow|sqrt|hypot|log|exp)\(/g,
    yes: ".a { width: calc(1px * pow(2, 3)); }",
    no: ".a { width: calc(1px * 8); }",
  },
  {
    id: "masks",
    chrome: 120,
    kinds: ["css", "js"],
    re: /(?<![\w-])mask(-(image|mode|repeat|position|clip|origin|size|composite|type))?\s*:|\.style\.mask/g,
    yes: ".a { mask-image: linear-gradient(black, transparent); }",
    no: ".a { -webkit-mask-image: none; background-image: none; }",
  },
  {
    id: "nesting",
    chrome: 120,
    kinds: ["css"],
    find: nestingAt,
    yes: ".a { color: red; .b { color: blue; } }",
    no: "@media (pointer: coarse) { .a { color: red; } } @supports (display: grid) { .b { display: grid; } }",
  },
  {
    id: "scripting",
    chrome: 120,
    kinds: ["css", "js"],
    re: /\(\s*scripting\s*(:|\))/g,
    yes: "@media (scripting: none) { .js-only { display: none; } }",
    no: "@media (prefers-reduced-motion: reduce) { * { animation: none; } }",
  },
  {
    id: "url-canparse",
    chrome: 120,
    kinds: ["js"],
    re: /\bURL\s*\.\s*canParse\b/g,
    yes: "if (URL.canParse(text)) go(text);",
    no: "try { go(new URL(text)); } catch { /* not a URL */ }",
  },
];
const IDS = new Set(FEATURES.map((f) => f.id));

/** @returns {number[]} offsets of every use of `f` in `text` */
const findAll = (f, text) => (f.find ? f.find(text) : [...text.matchAll(f.re)].map((m) => m.index));

// ---- the detectors themselves -----------------------------------------------------------------------------------------
{
  check(FEATURES.length === 12, "twelve features of Baseline 2023 that Chrome got only in 117-120");
  check(FEATURES.every((f) => f.chrome >= 117 && f.chrome <= 120), "every one of them needs Chrome 117-120");
  for (const f of FEATURES) {
    const strip = f.kinds.includes("css") ? stripCss : f.kinds.includes("html") ? stripHtml : stripJs;
    check(findAll(f, strip(f.yes)).length > 0, `${f.id}: found in "${f.yes}"`);
    check(findAll(f, strip(f.no)).length === 0, `${f.id}: not found in "${f.no}"`);
  }
  // a use only mentioned in a comment is not a use; in a string handed to the DOM it is
  check(findAll(FEATURES.find((f) => f.id === "url-canparse"), stripJs("// URL.canParse would be nicer\nnew URL(x);")).length === 0, "a comment is not a use");
  check(findAll(FEATURES.find((f) => f.id === "user-pseudos"), stripJs('el.matches(":user-invalid")')).length === 1, "a selector in a string is a use");
  check(stripJs('const u = "ws://127.0.0.1/"; x = 1;') === 'const u = "ws://127.0.0.1/"; x = 1;', "a URL in a string is not a comment");
  check(stripJs("const re = /a\\/\\/b/g; y = 2;") === "const re = /a\\/\\/b/g; y = 2;", "slashes in a regex literal are not a comment");
  check(nestingAt(stripCss("/* .a { .b {} } */ .c { color: red; }")).length === 0, "nesting in a comment is not nesting");
  check(nestingAt("@font-face { font-family: x; src: url(x.woff2); } .a { color: red; }").length === 0, "@font-face is no nesting");
}

// ---- what the extension ships ------------------------------------------------------------------------------------------

/** @returns {string[]} the files under `dir` with one of `exts` */
const filesIn = (dir, exts) =>
  readdirSync(join(root, dir), { withFileTypes: true })
    .filter((e) => e.isFile() && exts.some((x) => e.name.endsWith(x)))
    .map((e) => join(root, dir, e.name));

const shipped = [...filesIn("src", [".js"]), ...filesIn("popup", [".js", ".css", ".html"])];
check(shipped.length >= 13, `the guard reads what the extension ships (${shipped.length} files in src/ and popup/)`);

/** The texts of one file per kind, comments stripped, offsets kept. */
function textsOf(path, raw) {
  if (path.endsWith(".js")) return { js: stripJs(raw) };
  if (path.endsWith(".css")) return { css: stripCss(raw) };
  const html = stripHtml(raw);
  // the <style> of a page is CSS; the rest is markup
  const css = blank(html).split("");
  for (const m of html.matchAll(/(<style\b[^>]*>)([\s\S]*?)<\/style>/gi)) {
    const at = m.index + m[1].length;
    for (let k = 0; k < m[2].length; k += 1) css[at + k] = m[2][k];
  }
  return { html, css: css.join("") };
}

const MARK = /baseline-guard:\s*([\w-]+)/g;
const unmarked = [];
let marked = 0;
for (const path of shipped) {
  const raw = readFileSync(path, "utf8");
  const lines = raw.split("\n");
  const lineOf = (offset) => raw.slice(0, offset).split("\n").length;
  const name = relative(root, path);
  for (const m of raw.matchAll(MARK)) check(IDS.has(m[1]), `${name}:${lineOf(m.index)}: baseline-guard names a known feature (${m[1]})`);
  const texts = textsOf(path, raw);
  for (const f of FEATURES) {
    for (const kind of f.kinds) {
      if (!texts[kind]) continue;
      for (const at of findAll(f, texts[kind])) {
        const ln = lineOf(at);
        const near = `${lines[ln - 2] || ""}\n${lines[ln - 1]}`;
        if ([...near.matchAll(MARK)].some((m) => m[1] === f.id)) marked += 1;
        else unmarked.push(`${name}:${ln}: ${f.id} (Chrome ${f.chrome}) without feature detection, fallback or polyfill`);
      }
    }
  }
}
for (const u of unmarked) check(false, u);
check(unmarked.length === 0, `no feature of Chrome 117-120 unguarded in the extension (${marked} guarded use${marked === 1 ? "" : "s"})`);

if (failures.length) {
  console.log(`\n${failures.length} check(s) failed`);
  process.exit(1);
}
console.log("\nall checks passed");
