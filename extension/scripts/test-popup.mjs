#!/usr/bin/env node
// Drives extension/popup/popup.js (with popup/i18n.js) in a vm context with a fake DOM and a fake chrome. No browser.
//   - every value of a (forged) tab state comes out as text, never as markup; the popup does not use innerHTML;
//   - the pages: the start page first, a settings page by location.hash, the way back, focus follows the page;
//   - of the target page only the block of the chosen target is shown;
//   - both languages: the same keys and placeholders in both tables, every key the popup or the page uses exists,
//     no key is left unused, a value that looks like a placeholder is not filled in again.
// The fake DOM is built from popup.html: an id the script asks for and the page does not have fails the run.
// Usage: node extension/scripts/test-popup.mjs   (exit 0 when all checks pass)
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";
import vm from "node:vm";

const here = dirname(fileURLToPath(import.meta.url));
const popupDir = join(here, "..", "popup");
const source = readFileSync(join(popupDir, "popup.js"), "utf8");
const i18nSource = readFileSync(join(popupDir, "i18n.js"), "utf8");
const html = readFileSync(join(popupDir, "popup.html"), "utf8");
const failures = [];
const check = (ok, what) => {
  console.log(`${ok ? "ok  " : "FAIL"} ${what}`);
  if (!ok) failures.push(what);
};
const flush = () => new Promise((r) => setTimeout(r, 20));

// --- fake DOM ------------------------------------------------------------------
let innerHtmlWrites = 0;
class FakeNode {
  constructor(tag, attrs = {}) {
    this.tag = tag;
    this.className = attrs.class || "";
    this.children = [];
    this.value = "";
    this.attrs = { ...attrs };
    this.hidden = "hidden" in attrs;
    this.focused = 0;
    this.dataset = {};
    for (const [k, v] of Object.entries(attrs)) {
      if (k.startsWith("data-")) this.dataset[k.slice(5).replace(/-(\w)/g, (_m, c) => c.toUpperCase())] = v;
    }
  }
  append(...items) {
    for (const i of items) this.children.push(typeof i === "string" ? new FakeText(i) : i);
  }
  replaceChildren(...items) {
    this.children = [];
    this.append(...items);
  }
  setAttribute(name, value) {
    this.attrs[name] = String(value);
  }
  removeAttribute(name) {
    delete this.attrs[name];
  }
  focus() {
    this.focused++;
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

// Every element of popup.html with its attributes (comments left out); the page has no `>` inside an attribute.
const pageNodes = () =>
  [...html.replace(/<!--[\s\S]*?-->/g, "").matchAll(/<([a-z][a-z0-9]*)((?:\s+[a-z0-9-]+(?:="[^"]*")?)*)\s*\/?>/g)].map(
    ([, tag, rest]) => new FakeNode(tag, Object.fromEntries([...rest.matchAll(/([a-z0-9-]+)(?:="([^"]*)")?/g)].map(([, k, v]) => [k, v ?? ""])))
  );

const PAGES = ["home", "target", "sources", "about"];
const BLOCKS = ["sink", "tailnet", "device"];

// One popup: `o.state` the tab's registry, `o.overview` the sources, `o.tailnet`, `o.link`, `o.pair` what the
// background answers, `o.target` the stored target, `o.lang` the browser's language, `o.hash` where it opens.
const run = async (o = {}) => {
  const nodes = pageNodes();
  const ids = Object.fromEntries(nodes.filter((n) => n.attrs.id).map((n) => [n.attrs.id, n]));
  const missing = [];
  const env = { ids, sent: [], sets: [], drafts: [], during: [], missing, misses: [], timeouts: [], onMessage: [] };
  const listeners = [];
  const trail = [o.hash || ""];
  const location = {
    get hash() {
      return trail.at(-1);
    },
    set hash(v) {
      // A real browser reports an empty fragment as "", whatever was assigned.
      trail.push(v === "#" ? "" : String(v));
      for (const l of listeners) l();
    },
  };
  const history = {
    back: () => {
      if (trail.length > 1) trail.pop();
      for (const l of listeners) l();
    },
  };
  const document = {
    documentElement: { lang: "" },
    getElementById: (id) => {
      if (!ids[id]) missing.push(id);
      return ids[id] ?? null;
    },
    querySelectorAll: (sel) => {
      const attr = /^\[([a-z0-9-]+)\]$/.exec(sel)[1];
      return nodes.filter((n) => attr in n.attrs);
    },
    createElement: (tag) => new FakeNode(tag),
    createTextNode: (t) => new FakeText(t),
  };
  const answers = {
    "vc-get": o.state ?? null,
    "vc-source-of": o.overview ?? null,
    "vc-tailnet-status": o.tailnet,
    "vc-tailnet-forget": true,
    "vc-link-status": o.link,
    "vc-pair-status": o.pair,
    "vc-tailnet-pair": o.pairAnswer,
  };
  const chrome = {
    runtime: {
      lastError: undefined,
      getManifest: () => ({ version: "9.8.7" }),
      sendMessage: (msg, cb) => {
        env.sent.push(msg);
        // What the buttons are while the request is out (the answer comes later in a real browser).
        env.during.push({ kind: msg.kind, pair: ids["tailnet-pair"]?.disabled, offer: ids["tailnet-offer-pair"]?.disabled });
        cb?.(answers[msg.kind]);
      },
      onMessage: { addListener: (f) => env.onMessage.push(f) },
    },
    tabs: { query: async () => [{ id: 1 }] },
    storage: {
      // chrome.storage.session, where the browser has it: the draft of what is typed and not saved.
      ...(o.nosession ? {} : { session: { get: (_d, cb) => cb({ popupDraft: o.draft ?? {} }), set: (s, cb) => (env.drafts.push(structuredClone(s.popupDraft)), cb?.()) } }),
      local: {
        get: (d, cb) => cb("sinkTarget" in d ? { ...d, sinkTarget: env.sets.findLast((s) => "sinkTarget" in s)?.sinkTarget ?? o.target ?? "" } : d),
        set: (s, cb) => (env.sets.push(s), env.during.push({ kind: "set", save: ids.save?.disabled }), cb?.()),
      },
    },
  };
  const ctx = vm.createContext({
    document,
    chrome,
    location,
    history,
    navigator: { language: o.lang || "en-US" },
    // The popup waits for nothing: a delay would be noted here (the test below says there is none).
    setTimeout: (f, ms) => (env.timeouts.push(ms), 0),
    addEventListener: (kind, fn) => kind === "hashchange" && listeners.push(fn),
  });
  vm.runInContext(i18nSource, ctx);
  // Every text the popup asks for goes through here: a key neither table has is noted.
  ctx.__misses = env.misses;
  vm.runInContext("{ const real = I18N.t; I18N.t = (key, params) => { if (!Object.hasOwn(I18N.tables.en, key)) __misses.push(String(key)); return real(key, params); }; }", ctx);
  vm.runInContext(source, ctx);
  await flush();
  env.ctx = ctx;
  env.location = location;
  env.historyLength = () => trail.length;
  env.document = document;
  env.shownPage = () => PAGES.filter((p) => !ids[`page-${p}`].hidden);
  env.shownBlocks = () => BLOCKS.filter((b) => !ids[`block-${b}`].hidden);
  env.eval = (code) => vm.runInContext(code, ctx);
  runs.push(env);
  return env;
};
const runs = [];

const EVIL = `<img src=x onerror=alert(1)> & "q" 'a'`;
const ALLOWED = new Set(["div", "span", "h2", "table", "tr", "th", "td", "br"]);

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

// The details of the tab: forged values are text.
{
  const { ids } = await run({ state: forged });
  const out = ids.out;
  const nodes = [...out.walk()].slice(1);
  check(innerHtmlWrites === 0, "popup never writes innerHTML");
  check(nodes.length > 5 && nodes.every((n) => ALLOWED.has(n.tag)), "only the elements the popup builds exist (no <img> from a value)");
  const all = out.textContent;
  check(all.includes(EVIL), "a value with <, &, \" and ' appears as literal text");
  check(all.split(EVIL).length - 1 >= 10, "every shown field carries its value as text");
  check(all.includes("5.0 MiB"), "bytes are formatted");
  check(ids["tab-line"].textContent === "This tab uses DRM: it is not captured." && ids["tab-line"].className === "locked", "the start page says DRM in one sentence");
}
{
  const { ids } = await run({});
  check(ids.out.textContent.startsWith("No video seen") && ids["tab-line"].textContent.startsWith("No video seen"), "no state shows the hint, on the start page and in the details");
  const none = await run({ state: { drm: [], events: 3, frames: [], mediaSources: [] } });
  check(none.ids.out.textContent.includes("No MediaSource created yet.") && none.ids.out.textContent.includes("3 events"), "an empty registry says so");
  check(none.ids["tab-line"].textContent === "This tab: a player was seen, no video yet", "a tab without a SourceBuffer has its own sentence");
  const two = await run({
    state: {
      drm: [],
      events: 1,
      frames: [1],
      mediaSources: [
        { msId: 1, ended: false, frameUrl: "https://a.example/", blobUrl: "", sourceBuffers: [1, 2].map((sbId) => ({ sbId, mime: "video/mp4", mimes: [], kind: "mp4", inits: 1, media: 2, bytes: 1 << 20, firstSec: 0, lastEndSec: 4, lastSec: 2 })) },
      ],
    },
  });
  check(two.ids["tab-line"].textContent === "This tab: 2 track(s), 2.0 MiB seen", `the start page sums the tab up (got ${two.ids["tab-line"].textContent})`);
  check(innerHtmlWrites === 0, "still no innerHTML after the other states");
}

// The pages: one at a time, by the hash; the way back is the browser's own.
{
  const p = await run({});
  check(p.shownPage().join() === "home", "the popup opens on the start page");
  check(p.ids["about-version"].textContent === "streamtap 9.8.7", "the about page names the version of the manifest");
  for (const name of PAGES.slice(1)) {
    p.ids[`nav-${name}`].onclick();
    p.location.hash = p.ids[`nav-${name}`].attrs.href;
    check(p.shownPage().join() === name, `the entry ${name} of the start page opens that page alone`);
    check(p.ids[`back-${name}`].focused === 1, `${name}: focus goes to the way back`);
    p.ids[`back-${name}`].onclick();
    check(p.shownPage().join() === "home" && p.location.hash === "", `${name}: back returns to the start page`);
    check(p.historyLength() === 1, `${name}: back is the browser's own way back, it leaves no entry behind (got ${p.historyLength()})`);
    check(p.ids[`nav-${name}`].focused === 1, `${name}: back at home the focus is on the entry it was left through`);
  }
  check(PAGES.slice(1).every((name) => p.ids[`nav-${name}`].attrs.href === `#${name}`), "every entry is a link to its page (it works as a link, the script only shows the page)");
  // The browser's back key, not the popup's button.
  p.ids["nav-sources"].onclick();
  p.location.hash = "#sources";
  p.eval("history.back()");
  check(p.shownPage().join() === "home", "the browser's back key leaves a settings page");
  const direct = await run({ hash: "#target" });
  check(direct.shownPage().join() === "target", "a popup opened at #target shows the target page");
  direct.ids["back-target"].onclick();
  check(direct.shownPage().join() === "home" && direct.location.hash === "", "back on a page opened directly goes to the start page");
  const junk = await run({ hash: "#<img>" });
  check(junk.shownPage().join() === "home", "an unknown hash is the start page");
  const home = await run({ hash: "#home" });
  check(home.shownPage().join() === "home", "#home is the start page");
}

// Of the target page only the block of the chosen target is shown.
{
  const file = { available: true, recording: 1, media: 4, bytes: 2048, full: false, saved: [] };
  const link = { links: 1, connected: true, sent: 7, sentBytes: 4096, dropped: 0, file };
  const tailnet = { available: true, paired: true, host: "pc.example.ts.net", device: "ab".repeat(16), state: "connected", offer: "" };
  for (const [target, block, line] of [
    ["", "sink", "Target: the sink on the PC · connected · 7 msgs / 4 KiB sent · 0 dropped"],
    ["tailnet", "tailnet", "Over the tailnet: connected to pc.example.ts.net (device abababab)"],
    ["file", "device", "Target: a file on this device · recording: 4 media segments, 2 KiB"],
    ["video", "device", "Target: a video on this device · recording: 4 media segments, 2 KiB"],
  ]) {
    const p = await run({ target, link, tailnet });
    const name = target || "sink";
    check(p.shownBlocks().join() === block, `target ${name}: only its block is on the target page (got ${p.shownBlocks().join()})`);
    check(p.ids[`target-${name}`].checked === true, `target ${name}: its choice is checked`);
    check(p.ids["target-line"].textContent === line, `target ${name}: the start page says it in one sentence (got ${p.ids["target-line"].textContent})`);
    check(p.ids["file-save"].hidden === (block !== "device"), `target ${name}: "save the recording now" is offered only for a recording on this device`);
    check((p.ids["file-state"].textContent !== "") === (block === "device"), `target ${name}: the recording's state is said only there`);
  }
  const p = await run({ target: "", link, tailnet });
  p.ids["target-tailnet"].onchange();
  check(p.sets.at(-1).sinkTarget === "tailnet" && p.shownBlocks().join() === "tailnet", "choosing the tailnet saves it and swaps the block");
  check(p.ids["target-saved"].textContent === "saved; reload the video page", "a new target says what to do next");
  p.ids["target-file"].onchange();
  check(p.sets.at(-1).sinkTarget === "file" && p.shownBlocks().join() === "device" && p.ids["target-line"].textContent.startsWith("Target: a file on this device"), "choosing the file swaps block and start page line");
  p.ids["target-sink"].onchange();
  check(p.sets.at(-1).sinkTarget === "" && p.shownBlocks().join() === "sink", "back to the local sink");

  // Chrome: no recording on the device, no tailnet module asked yet or missing.
  const chromeLike = await run({ target: "", link: { links: 0, file: { available: false } }, tailnet: undefined });
  check(chromeLike.ids["target-file"].disabled === true && chromeLike.ids["target-video"].disabled === true && chromeLike.ids["target-tailnet"].disabled === true, "targets this browser cannot offer are off");
  check(
    chromeLike.ids["target-note"].textContent === "a file or a video on this device: not available in this browser\nOver the tailnet: not available in this browser",
    "and the line under the choices says which and why"
  );
  check(chromeLike.ids["target-line"].textContent === "Target: the sink on the PC · no segments tapped in this tab yet", "the sink without a link says that nothing was tapped");
  const all = await run({ target: "", link, tailnet });
  check(all.ids["target-note"].textContent === "", "with every target on offer there is no such line");

  // The manual sink fields.
  const s = await run({});
  s.ids.port.value = "17700";
  s.ids.token.value = "typed";
  s.ids.save.onclick();
  const w = s.sets.at(-1);
  check(w.sinkPort === 17700 && w.sinkToken === "typed" && w.sinkPaired === false && w.tapMode === "all", "save writes port and token as a manual entry");
  check(s.ids["sink-saved"].textContent.startsWith("saved; reload"), "and says what to do next");
  check(s.during.findLast((d) => d.kind === "set").save === true && s.ids.save.disabled === false, "save is locked while the browser writes and free after");
  for (const bad of ["0", "70000", "1.5", "abc", ""]) {
    const b = await run({});
    b.ids.port.value = bad;
    b.ids.save.onclick();
    check(!b.sets.some((x) => "sinkPort" in x) && b.ids.port.attrs["aria-invalid"] === "true" && b.ids.port.focused === 1 && b.ids["sink-saved"].textContent.startsWith("port:"), `save: the port "${bad}" is refused at the field`);
  }
  s.ids.port.value = "17701";
  s.ids.save.onclick();
  check(s.ids.port.attrs["aria-invalid"] === "false", "save: a good port clears the mark");
}

// The pairing with the app.
{
  for (const [pair, words] of [
    [undefined, "App pairing: unavailable"],
    [{ error: "unsupported", connected: false }, "App pairing: not available in this browser"],
    [{ connected: false }, "App pairing: not connected"],
    [{ connected: false, error: "host-unavailable", detail: EVIL }, `browser says: ${EVIL}`],
    [{ connected: true, pairing: "approved", tapped: 2 }, "App pairing: paired, 2 tab(s) captured"],
    [{ connected: true, pairing: "approved" }, "App pairing: paired"],
    [{ connected: true, pairing: "pending" }, "waiting for your approval"],
    [{ connected: true, pairing: "denied" }, "denied in the app"],
    [{ connected: true, pairing: "revoked" }, "revoked in the app"],
    [{ connected: true, error: "app-not-running" }, "App pairing: the app is not running"],
    [{ connected: true }, "App pairing: connecting"],
  ]) {
    const p = await run({ pair });
    check(p.ids.pair.textContent.includes(words) && p.ids.pair.children.every((c) => c instanceof FakeText), `pairing: ${words.slice(0, 40)}`);
    check(p.ids["pair-now"].hidden === Boolean(pair && pair.error === "unsupported"), "pairing: pair now is hidden only where pairing does not exist");
  }
  const p = await run({ pair: { connected: false } });
  p.ids["pair-now"].onclick();
  check(p.sent.some((m) => m.kind === "vc-pair-now"), "pair now asks the background");
  const count = (kind) => p.sent.filter((m) => m.kind === kind).length;
  check(count("vc-pair-status") === 2, "pair now asks for the state again at once");
  check(p.timeouts.length === 0, `pairing: the popup waits for no timer (got ${p.timeouts})`);
  // The answer of the host comes later: the background says so, and the popup asks again.
  check(p.onMessage.length === 1, "the popup listens for the background's news");
  p.onMessage[0]({ kind: "vc-changed", topic: "pair" });
  check(count("vc-pair-status") === 3, "news about the pairing: the popup asks for the state");
  p.onMessage[0]({ kind: "vc-changed", topic: "other" });
  p.onMessage[0]({ kind: "something-else", topic: "pair" });
  p.onMessage[0](null);
  check(count("vc-pair-status") === 3, "other news changes nothing");
}

// The source section: forged overview values come out as text and form values.
{
  const p = await run({ overview: { verdict: { state: "locked", groups: ["obfuscated", EVIL] }, on: true, unlocked: ["social"], own: [EVIL], learning: ["lms.example"] } });
  const ids = p.ids;
  check(ids.source.textContent === `Source: obfuscated service, ${EVIL} · locked: nothing is copied (unlock under Sources)` && ids.source.className === "locked", "the source line names the group and the state as text");
  check(ids.source.children.every((c) => c instanceof FakeText), "the source line holds text only");
  check(ids["policy-on"].checked === true && ids["unlock-social"].checked === true && ids["unlock-obfuscated"].checked === false, "the checkboxes show the settings in effect");
  check(ids["own-hosts"].value === EVIL && ids["learning-hosts"].value === "lms.example", "user hosts come back as form values");
  ids["unlock-obfuscated"].checked = true;
  ids["policy-on"].checked = false;
  ids["own-hosts"].value = "tv.home.lan\n  nas.lan , \n";
  ids["policy-save"].onclick();
  const w = p.sets.at(-1);
  check(
    w && w.sourcePolicy === "off" && JSON.stringify(w.sourceUnlocked) === JSON.stringify(["obfuscated", "social"]) && JSON.stringify(w.sourceOwnHosts) === JSON.stringify(["tv.home.lan", "nas.lan"]),
    "save writes the policy, the unlocked groups and the cleaned host lines"
  );
  check(ids["policy-saved"].textContent === "saved; open tabs follow at once", "saving the sources says so");
  const forgedState = await run({ overview: { verdict: { state: "constructor", groups: ["toString"] }, on: false, unlocked: [], own: [], learning: [] } });
  check(forgedState.ids.source.textContent === "Source: toString · constructor", "a state or group that is the name of an Object method stays that word");
  const noPage = await run({ overview: { verdict: null, on: false, unlocked: [], own: [], learning: [] } });
  check(noPage.ids.source.textContent === "Source: no page seen in this tab yet", "a tab without a page says so");
  const gone = await run({ overview: null });
  check(gone.ids.source.textContent === "Source: unavailable", "no answer from the service worker says unavailable");
  check(innerHtmlWrites === 0, "the source section writes no innerHTML either");
}

// The tailnet target: the state names the next step, forged values stay text, pairing and forgetting.
{
  const p = await run({ target: "tailnet", tailnet: { available: true, paired: true, host: EVIL, device: EVIL, state: "unreachable", offer: EVIL } });
  const { ids, sent } = p;
  const line = ids["tailnet-state"];
  check(line.textContent.includes(EVIL) && line.textContent.includes("does not answer, nothing is sent"), "tailnet: a PC that does not answer is said, with the forged name as text");
  check(line.children.every((c) => c instanceof FakeText || (c.tag === "code" && c.attrs.translate === "no" && c.children.every((x) => x instanceof FakeText))) && line.className === "locked", "tailnet: the state line holds text and commands only");
  const commands = line.children.filter((c) => !(c instanceof FakeText)).map((c) => c.textContent);
  check(commands.join("|") === "--tailnet|tailscale serve" && !line.textContent.includes("`"), `tailnet: the commands of the line stand in elements of their own, no mark is left (got ${commands})`);
  check(ids["target-line"].textContent === line.textContent && ids["target-line"].className === "locked", "tailnet: the start page carries the same sentence");
  check(ids["tailnet-offer"].hidden === false && ids["tailnet-offer-text"].textContent.includes(EVIL), "tailnet: an opened pairing link is offered by the PC's name");
  check(ids["tailnet-forget"].hidden === false, "tailnet: a paired device can forget the pairing");
  ids["tailnet-offer-pair"].onclick();
  check(sent.some((m) => m.kind === "vc-tailnet-pair" && m.offer === true && !("code" in m)), "tailnet: confirming the offer sends no code from the popup");
  check(p.shownPage().join() === "target", "tailnet: confirming the offer on the start page opens the target page, where the answer is said");
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
  check(ids["tailnet-forget"].textContent === "forget this pairing" && ids["tailnet-result"].textContent.startsWith("forgotten on this device"), "tailnet: and says where to revoke it too");

  // An offer while another target is chosen: the pairing block appears with the answer.
  const offered = await run({ target: "file", tailnet: { available: true, paired: false, host: "", device: "", state: "not-paired", offer: "pc.example.ts.net" }, pairAnswer: { ok: false, reason: "code" } });
  check(offered.shownBlocks().join() === "device", "an offer alone does not show the tailnet block of another target");
  offered.ids["tailnet-offer-pair"].onclick();
  check(offered.shownBlocks().join() === "tailnet,device" && offered.ids["tailnet-result"].textContent.startsWith("the code is wrong"), "a pairing that was started shows its block and its answer");
  const good = await run({ target: "", tailnet: { available: true, paired: false, host: "", device: "", state: "not-paired", offer: "" }, pairAnswer: { ok: true, host: EVIL } });
  good.ids["tailnet-code"].value = "0123456789ABCDEF0123456789abcdef";
  good.ids["tailnet-host"].value = "pc.example.ts.net";
  good.ids["tailnet-pair"].onclick();
  check(good.sent.findLast((m) => m.kind === "vc-tailnet-pair").code === "0123456789abcdef0123456789abcdef", "tailnet: a typed code is sent in lower case");
  check(good.ids["tailnet-result"].textContent === `paired with ${EVIL}; the target is now the sink over the tailnet (reload the video page)` && good.ids["tailnet-result"].className === "released" && good.ids["tailnet-code"].value === "", "tailnet: a pairing that worked says so and clears the code");
  for (const reason of ["host", "code", "unreachable", "busy", "full", "known", "name", "version", "storage", "unsupported"]) {
    const r = await run({ target: "tailnet", tailnet: { available: true, paired: false, host: "", device: "", state: "not-paired", offer: "" }, pairAnswer: { ok: false, reason } });
    r.ids["tailnet-pair"].onclick();
    const said = r.ids["tailnet-result"].textContent;
    check(said !== "pairing failed" && said.length > 10, `tailnet: the refusal ${reason} has its own words`);
  }
  // FORM-06: no second try while one is out, the failed field is marked and has the focus, the words stand above the button.
  {
    const ok = { available: true, paired: false, host: "", device: "", state: "not-paired", offer: "pc.example.ts.net" };
    const answered = await run({ target: "tailnet", tailnet: ok, pairAnswer: { ok: false, reason: "code" } });
    answered.ids["tailnet-pair"].onclick();
    const out = answered.during.findLast((d) => d.kind === "vc-tailnet-pair");
    check(out.pair === true && out.offer === true, "tailnet: both pairing buttons are locked while the request is out");
    check(answered.ids["tailnet-pair"].disabled === false && answered.ids["tailnet-offer-pair"].disabled === false, "tailnet: and free again with the answer");
    const marks = ["tailnet-host", "tailnet-code", "tailnet-name"].map((id) => answered.ids[id].attrs["aria-invalid"]).join();
    check(marks === "false,true,false" && answered.ids["tailnet-code"].focused === 1, `tailnet: a wrong code marks the code field and takes the focus (got ${marks})`);
    check(answered.ids["tailnet-code"].attrs["aria-describedby"] === "tailnet-result" && !("aria-describedby" in answered.ids["tailnet-host"].attrs), "tailnet: only the failed field is described by the words");
    answered.ids["tailnet-pair"].onclick();
    check(answered.ids["tailnet-host"].focused === 0 && answered.ids["tailnet-code"].attrs["aria-invalid"] === "true", "tailnet: the mark stays while the answer says the same");
    const gone = await run({ target: "tailnet", tailnet: ok, pairAnswer: { ok: true, host: "pc.example.ts.net" } });
    gone.ids["tailnet-code"].setAttribute("aria-invalid", "true");
    gone.ids["tailnet-pair"].onclick();
    check(gone.ids["tailnet-code"].attrs["aria-invalid"] === "false", "tailnet: a pairing that worked clears the mark");
    const whole = await run({ target: "tailnet", tailnet: ok, pairAnswer: { ok: false, reason: "busy" } });
    whole.ids["tailnet-pair"].onclick();
    check(["tailnet-host", "tailnet-code", "tailnet-name"].every((id) => whole.ids[id].attrs["aria-invalid"] === "false"), "tailnet: an answer about no field marks none");
    check(html.indexOf('id="tailnet-result"') < html.indexOf('id="tailnet-pair"') && html.indexOf('id="sink-saved"') < html.indexOf('id="save"'), "the words of a try stand with the fields, above the button");
  }
  const odd = await run({ target: "tailnet", tailnet: { available: true, paired: false, host: "", device: "", state: "not-paired", offer: "" }, pairAnswer: { ok: false, reason: "toString" } });
  odd.ids["tailnet-pair"].onclick();
  check(odd.ids["tailnet-result"].textContent === "pairing failed", "tailnet: an unknown refusal is a plain failure");

  for (const [state, words] of [
    ["not-paired", "not paired"],
    ["revoked", "Pair again"],
    ["hub-changed", "another sink answers"],
    ["version", "another version"],
    ["connected", "connected to"],
    ["connecting", "connecting to"],
    ["refused", "refused the login"],
    ["lost", "ended"],
    ["idle", "nothing sent yet"],
    ["constructor", "nothing sent yet"],
  ]) {
    const r = await run({ target: "tailnet", tailnet: { available: true, paired: state !== "not-paired", host: "pc.example.ts.net", device: "ab".repeat(16), state, offer: "" } });
    check(r.ids["tailnet-state"].textContent.includes(words), `tailnet: the state ${state} has its own words`);
  }
  const off = await run({ target: "tailnet", tailnet: undefined });
  check(off.ids["tailnet-state"].textContent.includes("not available") && off.ids["target-tailnet"].disabled === true && off.ids["tailnet-pair"].disabled === true, "tailnet: without the module the target is off and says so");
  check(innerHtmlWrites === 0, "the tailnet section writes no innerHTML either");
}

// A recording or a video on this device: what was saved, in words.
{
  const saved = [
    { name: EVIL, bytes: 3 << 20, video: true, report: { gaps: 2, gapSeconds: 1.26, dupes: 3, overlaps: 1, cut: 4 }, full: true },
    { name: "b.streamtap", bytes: 10, fallback: "the background's own words", reason: "webm" },
    { name: "c.streamtap", bytes: 10, fallback: EVIL, reason: "something-new" },
    { name: "d.streamtap", bytes: 10 },
    { error: "download", name: EVIL },
    { error: "blob" },
    { error: EVIL },
  ];
  const p = await run({ target: "video", link: { links: 1, file: { available: true, recording: 0, media: 0, bytes: 0, full: false, saved } } });
  const lines = p.ids["file-state"].textContent.split("\n");
  check(lines[0].startsWith("recording: nothing in this tab yet"), "device: no recording yet says how to get one");
  check(lines[1] === `saved video: ${EVIL}, 3.0 MiB (memory was full; 2 gap(s), 1.3 s missing; 3 repeated part(s) kept once; 1 overlapping part(s) dropped; 4 cut part(s) lost)`, `device: a saved video says what it holds (got ${lines[1]})`);
  check(lines[2] === "saved recording: b.streamtap, 10 B - no video: the source is WebM; on the PC: streamtap-sink replay, then mux", "device: a recording in place of a video says why, in the popup's words for the reason");
  check(lines[3].includes(`no video: ${EVIL};`), "device: a reason this popup does not know is said in the background's words, as text");
  check(lines[4] === "saved recording: d.streamtap, 10 B", "device: a plain recording");
  check(lines[5] === `not saved (the download failed): ${EVIL}` && lines[6] === "not saved (the browser could not hold the file)" && lines[7] === `not saved (${EVIL})`, "device: a file that was not saved says why");
  check(p.ids["file-save"].hidden === true, "device: nothing to save, no button");
  const full = await run({ target: "file", link: { links: 1, file: { available: true, recording: 2, media: 9, bytes: 1 << 20, full: true, saved: [] } } });
  check(full.ids["file-state"].textContent.includes("memory full") && full.ids["file-save"].hidden === false, "device: a full memory is said, and the recording can be saved");
  full.ids["file-save"].onclick();
  check(full.sent.some((m) => m.kind === "vc-file-save" && m.tabId === 1), "device: save now asks the background for this tab");
  const links = () => full.sent.filter((m) => m.kind === "vc-link-status").length;
  const before = links();
  check(full.timeouts.length === 0, `device: the popup waits for no timer after save now (got ${full.timeouts})`);
  full.onMessage[0]({ kind: "vc-changed", topic: "file" });
  check(links() === before + 1, "device: news that a file was saved makes the popup ask for the link again");
}

// The draft: what is typed and not saved survives the popup (FORM-07), never a secret.
{
  const d = await run({});
  d.ids.port.value = "1234";
  d.ids.port.oninput();
  d.ids["own-hosts"].value = "tv.example";
  d.ids["own-hosts"].oninput();
  d.ids["policy-on"].checked = true;
  d.ids["policy-on"].oninput();
  d.ids.token.value = "secret";
  check(d.ids.token.oninput === undefined && d.ids["tailnet-code"].oninput === undefined, "draft: the token and the pairing code are not watched");
  const last = d.drafts.at(-1);
  check(last.port === "1234" && last["own-hosts"] === "tv.example" && last["policy-on"] === true && !JSON.stringify(d.drafts).includes("secret"), `draft: typed values are kept, no secret (got ${JSON.stringify(last)})`);
  const back = await run({ draft: { port: "4321", "own-hosts": "x.example", "policy-on": true, "tailnet-name": "tablet", "tailnet-host": "pc.example.ts.net", token: "secret", "unlock-social": "yes", "learning-hosts": 5 }, overview: { verdict: null, on: false, unlocked: [], own: ["stored.example"], learning: ["l.example"] } });
  check(back.ids.port.value === "4321" && back.ids["own-hosts"].value === "x.example" && back.ids["policy-on"].checked === true && back.ids["tailnet-name"].value === "tablet" && back.ids["tailnet-host"].value === "pc.example.ts.net", "draft: a reopened popup shows what was typed, over the stored values");
  check(back.ids.token.value !== "secret" && back.ids["learning-hosts"].value === "l.example" && back.ids["unlock-social"].checked === false, "draft: a token, or a value of the wrong type, is not taken");
  const saved = await run({ draft: { port: "4321", "own-hosts": "x.example" } });
  saved.ids.port.value = "17700";
  saved.ids.save.onclick();
  check(!("port" in saved.drafts.at(-1)) && saved.drafts.at(-1)["own-hosts"] === "x.example", "draft: a saved field leaves the draft, the others stay");
  saved.ids["policy-save"].onclick();
  check(Object.keys(saved.drafts.at(-1)).length === 0, "draft: saving the sources clears theirs");
  const none = await run({ nosession: true });
  none.ids.port.value = "9";
  none.ids.port.oninput();
  check(none.ids.port.value === "9" && none.drafts.length === 0, "draft: without chrome.storage.session the popup works and keeps none");
}

// --- both languages ------------------------------------------------------------
// Numbers follow the browser's language (JS-04): a German browser writes 1,5 and 5,0, an English one 1.5 and 5.0.
{
  const buffer = { sbId: 1, mime: "video/mp4", mimes: [], kind: "mp4", inits: 1, media: 2, bytes: 5 << 20, firstSec: 0, lastEndSec: 1234.5, lastSec: 0 };
  const state = { drm: [], events: 1, frames: [], mediaSources: [{ msId: 1, ended: false, frameUrl: "https://a.example/", blobUrl: "", sourceBuffers: [buffer] }] };
  const saved = [{ name: "a.mp4", bytes: 3 << 20, video: true, report: { gaps: 2, gapSeconds: 1.25 } }];
  const link = { links: 1, file: { available: true, recording: 0, media: 0, bytes: 0, full: false, saved } };
  const de = await run({ lang: "de-DE", state, target: "video", link });
  check(de.ids.out.textContent.includes("5,0 MiB") && de.ids.out.textContent.includes("0s – 1.234,5s"), `German browser: bytes and seconds in German digits (got ${de.ids.out.textContent})`);
  check(de.ids["file-state"].textContent.includes("3,0 MiB") && de.ids["file-state"].textContent.includes("1,3 s"), `German browser: the gap's seconds in German digits (got ${de.ids["file-state"].textContent})`);
  const en = await run({ lang: "en-US", state, target: "video", link });
  check(!/\.toFixed\(/.test(source), "the popup writes no number by toFixed (it knows no language)");
  check(en.ids.out.textContent.includes("5.0 MiB") && en.ids.out.textContent.includes("0s – 1,234.5s"), "English browser: bytes and seconds in English digits");
}
{
  const p = await run({ lang: "de-AT", target: "tailnet", tailnet: { available: true, paired: true, host: "{id}", device: "ab".repeat(16), state: "connected", offer: "" }, overview: { verdict: { state: "locked", groups: ["social"] }, on: true, unlocked: [], own: [], learning: [] } });
  const { en, de } = p.eval("I18N.tables");
  check(p.eval("I18N.lang") === "de" && p.document.documentElement.lang === "de", "a German browser gets German, and the page says its language");
  check(p.ids["nav-target"].attrs.href === "#target" && p.ids["back-target"].textContent === "Zurück", "the static text of the page is German");
  check(p.ids.source.textContent === "Quelle: soziale Medien · gesperrt: nichts wird kopiert (entsperren unter Quellen)", `the source line is German (got ${p.ids.source.textContent})`);
  check(p.ids["tailnet-state"].textContent === "Über das Tailnet: verbunden mit {id} (Gerät abababab)", `a value that looks like a placeholder is not filled in again (got ${p.ids["tailnet-state"].textContent})`);
  check(p.ids["tailnet-name"].attrs.placeholder === "Handy", "a placeholder attribute is translated");
  const german = await run({ lang: "de", target: "video", link: { links: 1, file: { available: true, recording: 0, media: 0, bytes: 0, full: false, saved: [{ name: "b.streamtap", bytes: 10, fallback: "the source is WebM", reason: "webm" }] } } });
  check(german.ids["file-state"].textContent.split("\n")[1] === "Aufnahme gespeichert: b.streamtap, 10 B - kein Video: die Quelle ist WebM; am PC: streamtap-sink replay, dann mux", "the reason a video became a recording is German too");
  const english = await run({ lang: "fr-FR" });
  check(english.eval("I18N.lang") === "en" && english.ids["back-target"].textContent === "Back", "any other language gets English");
  check(english.eval("I18N.resolve('DE')") === "de" && english.eval("I18N.resolve('den')") === "de" && english.eval("I18N.resolve(undefined)") === "en", "the language is read from the start of the browser's tag");
  const asked = english.misses.length;
  check(english.eval("I18N.t('noSuchKey')") === "noSuchKey" && english.eval("I18N.t('constructor')") === "constructor" && english.eval("I18N.t('toString', {})") === "toString", "a key no table has is shown as the key, also the name of an Object method");
  // Those three were asked for on purpose.
  english.misses.splice(asked);
  check(english.eval("I18N.t('tnOffer', { pc: '{pc}{constructor}' })") === "A pairing link of {pc}{constructor} was opened in this browser.", "a value is put in once");
  check(english.eval("I18N.t('{toString} {pc}', { pc: 'x' })") === "{toString} x", "a placeholder is filled from the values given, never from what every object has");
  english.misses.splice(asked);
  check(english.eval("I18N.t('tnConnected', { pc: 'x' })") === "Over the tailnet: connected to x (device {id})", "a placeholder without a value stays visible");

  const keysEn = Object.keys(en);
  const keysDe = Object.keys(de);
  check(keysEn.length > 100 && keysEn.every((k) => k in de) && keysDe.every((k) => k in en), "both tables have the same keys");
  const holes = (s) => [...s.matchAll(/\{(\w+)\}/g)].map((m) => m[1]).sort().join();
  const uneven = keysEn.filter((k) => k in de && holes(en[k]) !== holes(de[k]));
  check(uneven.length === 0, `every text has the same placeholders in both languages${uneven.length ? ` (not: ${uneven.join(", ")})` : ""}`);
  const empty = [...keysEn.filter((k) => !en[k].trim()), ...keysDe.filter((k) => !de[k].trim())];
  check(empty.length === 0, `no text is empty${empty.length ? ` (${empty.join(", ")})` : ""}`);
  const same = keysEn.filter((k) => en[k] === de[k]);
  const SAME_IN_BOTH = ["groupArd", "groupZdf", "msTitle", "aboutVersion"];
  check(same.sort().join() === SAME_IN_BOTH.sort().join(), `only names are the same in both languages (the same: ${same.join(", ")})`);

  // Every key the page and the script use exists; no key is left over.
  const inPage = [...html.matchAll(/data-i18n(?:-placeholder)?="([^"]*)"/g)].map((m) => m[1]);
  const maps = ["GROUP_KEYS", "PAIR_ERROR_KEYS", "MUX_KEYS", "SAVE_ERROR_KEYS"].flatMap((name) => Object.values(p.eval(name)));
  const pairs = ["STATE_KEYS", "TAILNET_STATES"].flatMap((name) => Object.values(p.eval(name)).map((v) => v[1]));
  const literal = [...source.matchAll(/\bt\(\s*"(\w+)"/g)].map((m) => m[1]);
  const used = [...inPage, ...maps, ...pairs, ...literal];
  const unknown = [...new Set(used.filter((k) => !Object.hasOwn(en, k)))];
  check(inPage.length > 30 && maps.length > 20 && literal.length > 30, "the page and the script name their texts by key");
  check(unknown.length === 0, `every key the page and the script use exists${unknown.length ? ` (missing: ${unknown.join(", ")})` : ""}`);
  // A key chosen by a condition stands in the script as a string of its own; two are put together from a stem.
  const mentioned = new Set([...used, ...[...source.matchAll(/"(\w+)"/g)].map((m) => m[1]), "savedVideoNotes", "savedRecordingNotes"]);
  const unused = keysEn.filter((k) => !mentioned.has(k));
  check(unused.length === 0, `no text is left over${unused.length ? ` (unused: ${unused.join(", ")})` : ""}`);
  check(en.savedVideoNotes && en.savedRecordingNotes && source.includes('(s.video ? "savedVideo" : "savedRecording") + (notes.length ? "Notes" : "")'), "the two keys built from a stem exist");
  // A command is marked with backticks (`cmd`, I18N.fill makes <code translate="no"> of it): the marks come in pairs,
  // in both languages alike, and never in a text the page sets as a plain attribute or static text.
  const marks = (s) => (s.match(/`/g) || []).length;
  const odd = keysEn.filter((k) => marks(en[k]) % 2 || marks(de[k]) % 2 || marks(en[k]) !== marks(de[k]));
  check(odd.length === 0, `the backticks of a text come in pairs, alike in both languages${odd.length ? ` (${odd.join(", ")})` : ""}`);
  check(keysEn.filter((k) => marks(en[k])).length >= 3 && !inPage.some((k) => marks(en[k] ?? "")), "texts with a command carry marks, the static texts of the page carry none");
  // Text size is in rem, so the browser's font size preference reaches it (typography guide); and a field that holds an
  // identifier, not prose, has no autocorrect.
  const css = readFileSync(join(popupDir, "popup.css"), "utf8");
  check(!/font(?:-size)?:\s*\d+(?:\.\d+)?px/.test(css), "popup.css sets no font size in px");
  const ids = ["token", "tailnet-host", "tailnet-code", "own-hosts", "learning-hosts"];
  const prose = ids.filter((id) => !new RegExp(`id="${id}"[^>]*autocorrect="off"`).test(html));
  check(prose.length === 0, `fields with an identifier have autocorrect off${prose.length ? ` (not: ${prose.join(", ")})` : ""}`);
  // Static text: what stands in popup.html without scripts is the English text of its key.
  const stale = [...html.matchAll(/data-i18n="(\w+)"[^>]*>([^<]*)</g)].filter(([, key, textInPage]) => en[key] !== textInPage.replace(/&amp;/g, "&")).map((m) => m[1]);
  check(stale.length === 0, `the text in popup.html is the English text of its key${stale.length ? ` (not: ${stale.join(", ")})` : ""}`);
  // Names and commands are not translated by the browser (HTML-05): the brand and every command carry translate="no", and
  // no static text holds a command (it stands in an element of its own).
  const COMMAND = /streamtap-sink|tailscale (?:serve|funnel)|chrome:\/\/|ffmpeg|--tailnet/;
  const codes = [...html.matchAll(/<code\b[^>]*>/g)];
  const inCommand = [...new Set(inPage)].filter((k) => COMMAND.test(en[k]) || COMMAND.test(de[k]));
  check(/<h1 translate="no">streamtap<\/h1>/.test(html), 'the name streamtap in popup.html is translate="no"');
  check(codes.length > 0 && codes.every((m) => /\btranslate="no"/.test(m[0])), 'every command in popup.html is translate="no"');
  check(inCommand.length === 0, `no static text holds a command${inCommand.length ? ` (${inCommand.join(", ")})` : ""}`);
}

// Across every popup of this run.
{
  const askedMissing = [...new Set(runs.flatMap((r) => r.misses))];
  check(askedMissing.length === 0, `no text was asked for that the tables lack${askedMissing.length ? ` (${askedMissing.join(", ")})` : ""}`);
  const noId = [...new Set(runs.flatMap((r) => r.missing))];
  check(noId.length === 0, `every element the script asks for is in popup.html${noId.length ? ` (missing: ${noId.join(", ")})` : ""}`);
  const first = runs[0];
  const idsInPage = Object.keys(first.ids);
  const unusedIds = idsInPage.filter((id) => !source.includes(`"${id}"`) && !/^(page|nav|back|target|block)-/.test(id));
  check(unusedIds.length === 0, `no id of popup.html is left over${unusedIds.length ? ` (${unusedIds.join(", ")})` : ""}`);
  check(runs.length > 40 && runs.every((r) => r.shownPage().length === 1), "exactly one page is shown, in every popup of this run");
  check(/<script src="i18n\.js"><\/script>\s*<script src="popup\.js"><\/script>/.test(html) && !/<script(?![^>]*\bsrc=)/.test(html) && !/\son[a-z]+=/.test(html) && !/<style|style="/.test(html), "popup.html loads i18n.js before popup.js and carries no inline script or style");
  check(innerHtmlWrites === 0 && !/innerHTML|insertAdjacentHTML|outerHTML|document\.write/.test(source + i18nSource), "nothing in the popup writes markup");
}

if (failures.length) {
  console.log(`\n${failures.length} check(s) failed`);
  process.exit(1);
}
console.log("\nall checks passed");
