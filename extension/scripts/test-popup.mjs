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

const run = async (state, overview = null, sets = [], tailnet = undefined, sent = []) => {
  const ids = {};
  const document = {
    getElementById: (id) => (ids[id] ??= new FakeNode(`#${id}`)),
    createElement: (tag) => new FakeNode(tag),
    createTextNode: (t) => new FakeText(t),
  };
  const chrome = {
    runtime: {
      lastError: undefined,
      sendMessage: (msg, cb) => {
        sent.push(msg);
        cb?.(msg.kind === "vc-get" ? state : msg.kind === "vc-source-of" ? overview : msg.kind === "vc-tailnet-status" ? tailnet : msg.kind === "vc-tailnet-forget" ? true : undefined);
      },
    },
    tabs: { query: async () => [{ id: 1 }] },
    storage: { local: { get: (d, cb) => cb(d), set: (o, cb) => (sets.push(o), cb?.()) } },
  };
  vm.runInNewContext(source, { document, chrome, setTimeout, String, Number, Array, Set });
  await flush();
  run.ids = ids;
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

// The source section: forged overview values come out as text and form values.
{
  const sets = [];
  await run(null, { verdict: { state: "locked", groups: ["obfuscated", EVIL] }, on: true, unlocked: ["social"], own: [EVIL], learning: ["lms.example"] }, sets);
  const ids = run.ids;
  check(ids.source.textContent.includes("obfuscated service") && ids.source.textContent.includes(EVIL) && ids.source.textContent.includes("locked: nothing is copied"), "the source line names the group and the state as text");
  check(ids.source.children.every((c) => c instanceof FakeText), "the source line holds text only");
  check(ids["policy-on"].checked === true && ids["unlock-social"].checked === true && ids["unlock-obfuscated"].checked === false, "the checkboxes show the settings in effect");
  check(ids["own-hosts"].value === EVIL && ids["learning-hosts"].value === "lms.example", "user hosts come back as form values");
  ids["unlock-obfuscated"].checked = true;
  ids["policy-on"].checked = false;
  ids["own-hosts"].value = "tv.home.lan\n  nas.lan , \n";
  ids["policy-save"].onclick();
  const w = sets.at(-1);
  check(
    w && w.sourcePolicy === "off" && JSON.stringify(w.sourceUnlocked) === JSON.stringify(["obfuscated", "social"]) && JSON.stringify(w.sourceOwnHosts) === JSON.stringify(["tv.home.lan", "nas.lan"]),
    "save writes the policy, the unlocked groups and the cleaned host lines"
  );
  await run(null, { verdict: null, on: false, unlocked: [], own: [], learning: [] });
  check(run.ids.source.textContent.includes("no page seen"), "a tab without a page says so");
  await run(null, null);
  check(run.ids.source.textContent === "source: unavailable", "no answer from the service worker says unavailable");
  check(innerHtmlWrites === 0, "the source section writes no innerHTML either");
}

// The tailnet target: the state names the next step, forged values stay text, pairing and forgetting.
{
  const sent = [];
  await run(null, null, [], { available: true, paired: true, host: EVIL, device: EVIL, state: "unreachable", offer: EVIL }, sent);
  const ids = run.ids;
  const line = ids["tailnet-state"];
  check(line.textContent.includes(EVIL) && line.textContent.includes("does not answer, nothing is sent"), "tailnet: a PC that does not answer is said, with the forged name as text");
  check(line.children.every((c) => c instanceof FakeText) && line.className === "locked", "tailnet: the state line holds text only");
  check(ids["tailnet-offer"].hidden === false && ids["tailnet-offer-text"].textContent.includes(EVIL), "tailnet: an opened pairing link is offered by the PC's name");
  check(ids["tailnet-forget"].hidden === false, "tailnet: a paired device can forget the pairing");
  ids["tailnet-offer-pair"].onclick();
  check(sent.some((m) => m.kind === "vc-tailnet-pair" && m.offer === true && !("code" in m)), "tailnet: confirming the offer sends no code from the popup");
  // The fake DOM makes a field when the popup first asks for it.
  for (const id of ["tailnet-code", "tailnet-name"]) ids[id] ??= new FakeNode(`#${id}`);
  ids["tailnet-code"].value = "  https://pc.example.ts.net/#streamtap-pair=1&code=0123456789abcdef0123456789abcdef ";
  ids["tailnet-name"].value = "my phone";
  ids["tailnet-pair"].onclick();
  const typed = sent.findLast((m) => m.kind === "vc-tailnet-pair");
  check(typed.kind === "vc-tailnet-pair" && typed.host === "pc.example.ts.net" && typed.code === "0123456789abcdef0123456789abcdef" && typed.name === "my phone", "tailnet: a pasted link gives the PC and the code");
  check(ids["tailnet-result"].textContent === "pairing failed" && ids["tailnet-result"].className === "locked", "tailnet: no answer to a pairing is a failure, said");
  const before = sent.filter((m) => m.kind === "vc-tailnet-forget").length;
  ids["tailnet-forget"].onclick();
  check(sent.filter((m) => m.kind === "vc-tailnet-forget").length === before && ids["tailnet-forget"].textContent.includes("tap again"), "tailnet: forgetting asks for a second tap");
  ids["tailnet-forget"].onclick();
  check(sent.filter((m) => m.kind === "vc-tailnet-forget").length === before + 1, "tailnet: the second tap forgets");

  for (const [state, words] of [
    ["not-paired", "not paired"],
    ["revoked", "Pair again"],
    ["hub-changed", "another sink answers"],
    ["version", "another version"],
    ["connected", "connected to"],
    ["lost", "ended"],
    ["idle", "nothing sent yet"],
  ]) {
    await run(null, null, [], { available: true, paired: state !== "not-paired", host: "pc.example.ts.net", device: "ab".repeat(16), state, offer: "" });
    check(run.ids["tailnet-state"].textContent.includes(words), `tailnet: the state ${state} has its own words`);
  }
  await run(null, null, [], undefined);
  check(run.ids["tailnet-state"].textContent.includes("not available") && run.ids["target-tailnet"].disabled === true, "tailnet: without the module the target is off and says so");
  const sets = [];
  await run(null, null, sets, { available: true, paired: true, host: "pc.example.ts.net", device: "ab".repeat(16), state: "idle", offer: "" });
  run.ids["target-tailnet"].onchange();
  check(sets.at(-1).sinkTarget === "tailnet", "tailnet: choosing the target saves it");
  run.ids["target-sink"].onchange();
  check(sets.at(-1).sinkTarget === "", "tailnet: back to the local sink");
  check(innerHtmlWrites === 0, "the tailnet section writes no innerHTML either");
}

if (failures.length) {
  console.log(`\n${failures.length} check(s) failed`);
  process.exit(1);
}
console.log("\nall checks passed");
