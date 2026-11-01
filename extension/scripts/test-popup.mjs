#!/usr/bin/env node
// Drives extension/popup/popup.js in a vm context with a fake DOM and a fake chrome:
// every value of a (forged) tab state must come out as text, never as markup, and
// the popup must not use innerHTML at all. No browser.
// Usage: node extension/scripts/test-popup.mjs   (exit 0 when all checks pass)
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";
import vm from "node:vm";

const here = dirname(fileURLToPath(import.meta.url));
const source = readFileSync(join(here, "..", "popup", "popup.js"), "utf8");
const failures = [];
const check = (ok, what) => {
  console.log(`${ok ? "ok  " : "FAIL"} ${what}`);
  if (!ok) failures.push(what);
};
const flush = () => new Promise((r) => setTimeout(r, 20));

// --- fake DOM ------------------------------------------------------------------
let innerHtmlWrites = 0;
class FakeNode {
  constructor(tag) {
    this.tag = tag;
    this.className = "";
    this.children = [];
    this.value = "";
  }
  append(...items) {
    for (const i of items) this.children.push(typeof i === "string" ? new FakeText(i) : i);
  }
  replaceChildren(...items) {
    this.children = [];
    this.append(...items);
  }
  set textContent(v) {
    this.children = [new FakeText(String(v))];
  }
  get textContent() {
    return this.children.map((c) => c.textContent).join("");
  }
  set innerHTML(_v) {
    innerHtmlWrites++;
  }
  *walk() {
    yield this;
    for (const c of this.children) if (c instanceof FakeNode) yield* c.walk();
  }
}
class FakeText {
  constructor(t) {
    this.textContent = t;
  }
}

const run = async (state) => {
  const ids = {};
  const document = {
    getElementById: (id) => (ids[id] ??= new FakeNode(`#${id}`)),
    createElement: (tag) => new FakeNode(tag),
    createTextNode: (t) => new FakeText(t),
  };
  const chrome = {
    runtime: {
      lastError: undefined,
      sendMessage: (msg, cb) => cb?.(msg.kind === "vc-get" ? state : undefined),
    },
    tabs: { query: async () => [{ id: 1 }] },
    storage: { local: { get: (d, cb) => cb(d), set: (_o, cb) => cb?.() } },
  };
  vm.runInNewContext(source, { document, chrome, setTimeout, String, Number });
  await flush();
  return ids.out;
};

const EVIL = `<img src=x onerror=alert(1)> & "q" 'a'`;
const ALLOWED = new Set(["#out", "div", "span", "h2", "table", "tr", "th", "td", "br"]);

const forged = {
  drm: [{ keySystem: EVIL, robustness: [EVIL] }],
  events: EVIL,
  frames: [1],
  mediaSources: [
    {
      msId: EVIL,
      ended: true,
      frameUrl: EVIL,
      blobUrl: EVIL,
      sourceBuffers: [
        { sbId: EVIL, mime: EVIL, mimes: [EVIL, EVIL], kind: EVIL, inits: EVIL, media: EVIL, bytes: 5 << 20, firstSec: EVIL, lastEndSec: EVIL, lastSec: 0 },
      ],
    },
  ],
};

const out = await run(forged);
const nodes = [...out.walk()];
check(innerHtmlWrites === 0, "popup never writes innerHTML");
check(nodes.every((n) => ALLOWED.has(n.tag)), "only the elements the popup builds exist (no <img> from a value)");
const all = out.textContent;
check(all.includes(EVIL), "a value with <, &, \" and ' appears as literal text");
check(all.split(EVIL).length - 1 >= 10, "every shown field carries its value as text");
check(all.includes("5.0 MiB"), "bytes are formatted");

const empty = await run(null);
check(empty.textContent.startsWith("No MSE activity"), "no state shows the hint");
const none = await run({ drm: [], events: 3, frames: [], mediaSources: [] });
check(none.textContent.includes("No MediaSource created yet.") && none.textContent.includes("3 events"), "an empty registry says so");
check(innerHtmlWrites === 0, "still no innerHTML after the other states");

if (failures.length) {
  console.log(`\n${failures.length} check(s) failed`);
  process.exit(1);
}
console.log("\nall checks passed");
