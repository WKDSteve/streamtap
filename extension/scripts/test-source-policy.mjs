#!/usr/bin/env node
// The source policy (extension/src/source-policy.js) in a vm context, and the
// content script's tapping gate with the policy on, both without a browser.
// Checks the grouping (subdomains, http(s) only, user hosts, locked groups
// first), that a broken setting never unlocks a group, that one locked URL
// (a locked player embedded in another site) locks the frame, and that the
// content script taps a locked site neither on its own nor when the desktop
// app switches the tab on, and fails closed without an answer.
// Usage: node extension/scripts/test-source-policy.mjs   (exit 0 when all checks pass)
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";
import vm from "node:vm";

const here = dirname(fileURLToPath(import.meta.url));
const src = (name) => readFileSync(join(here, "..", "src", name), "utf8");
const failures = [];
const check = (ok, what) => {
  console.log(`${ok ? "ok  " : "FAIL"} ${what}`);
  if (!ok) failures.push(what);
};
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const until = async (f, ms = 1000) => {
  const end = Date.now() + ms;
  while (!f() && Date.now() < end) await sleep(5);
};

// ---- the module ---------------------------------------------------------------
{
  const stored = {};
  const ctx = vm.createContext({
    URL,
    console,
    chrome: {
      runtime: { lastError: null },
      storage: { local: { get: (keys, cb) => cb(Object.fromEntries(keys.filter((k) => k in stored).map((k) => [k, stored[k]]))) } },
    },
  });
  vm.runInContext(src("source-policy.js") + "\n;globalThis.__sp = { spSettings, spClassify, spJudge, spCleanHost, spState };", ctx, { filename: "source-policy.js" });
  const { spSettings, spClassify, spJudge, spCleanHost, spState } = ctx.__sp;
  const on = (extra = {}) => spSettings({ sourcePolicy: "on", ...extra });
  const cls = (url, s = on()) => spClassify(url, s);

  check(cls("https://www.ardmediathek.de/video/x").group === "ard" && cls("https://www.ardmediathek.de/video/x").state === "released", "a subdomain of a released host is released (ARD)");
  check(cls("https://www.zdf.de/").group === "zdf" && cls("https://www.3sat.de/").group === "zdf", "ZDF and 3sat are the zdf group");
  check(cls("https://media.ccc.de/v/talk").group === "free", "media.ccc.de is a free licence");
  check(cls("https://www.youtube.com/watch?v=1").state === "locked" && cls("https://m.youtube.com/").group === "obfuscated", "YouTube (also m.) is locked");
  check(cls("https://www.instagram.com/reel/1").state === "locked" && cls("https://www.instagram.com/").group === "social", "Instagram is social, locked");
  check(cls("https://notyoutube.com/").state === "unknown", "a host that only ends in the same letters is not covered");
  check(cls("https://youtube.com.evil.example/").state === "unknown", "a listed name inside another host is not covered");
  check(cls("https://www.youtube.com./x").state === "locked", "a trailing dot does not escape the list");
  check(cls("file:///C:/video.mp4").state === "unknown" && cls("file:///C:/video.mp4").group === null, "a non-web URL has no group");
  check(cls("not a url").state === "unknown", "garbage is unknown");
  check(cls("https://video.example/").state === "unknown", "an unlisted site is unknown");

  check(cls("https://tv.home.lan/", on({ sourceOwnHosts: ["home.lan"] })).group === "own", "an own host the user entered is released");
  check(cls("https://kurs.uni.example/", on({ sourceLearningHosts: ["https://uni.example/path"] })).group === "learning", "a learning platform entered with scheme and path is cleaned and released");
  check(cls("https://www.youtube.com/", on({ sourceOwnHosts: ["youtube.com"] })).state === "locked", "listing a locked host as own does not release it");
  check(cls("https://www.youtube.com/", on({ sourceUnlocked: ["obfuscated"] })).state === "unlocked", "an unlocked group is unlocked");
  check(cls("https://www.instagram.com/", on({ sourceUnlocked: ["obfuscated"] })).state === "locked", "unlocking one group leaves the others locked");

  for (const bad of ["obfuscated", { 0: "obfuscated" }, ["OBFUSCATED"], ["ard"], ["constructor"], ["__proto__"], [1], null]) {
    const s = on({ sourceUnlocked: bad });
    check(s.unlocked.size === 0, `a broken unlock setting unlocks nothing (${JSON.stringify(bad)})`);
  }
  check(spSettings({ sourcePolicy: true }).on === true && spSettings({ sourcePolicy: "OFF" }).on === true && spSettings(null).on === true, "a broken or missing value keeps the policy on");
  check(spSettings({ sourcePolicy: "off" }).on === false, "only the exact value \"off\" turns the policy off");
  check(spCleanHost("bad host") === "" && spCleanHost("a..b") === "" && spCleanHost("x".repeat(300)) === "" && spCleanHost(5) === "", "malformed hosts are dropped");
  check(spCleanHost(" HTTPS://Tv.Home.Lan:8096/web ") === "tv.home.lan", "a host is cleaned of scheme, port, path and case");
  check(spSettings({ sourceOwnHosts: Array.from({ length: 500 }, (_, i) => `h${i}.example`) }).own.length === 200, "user host lists are bounded");

  // The verdict for a frame.
  const offS = spSettings({ sourcePolicy: "off" });
  check(spJudge(["https://www.youtube.com/"], offS).tap === true && spJudge([], offS).state === "off", "policy off: everything is tapped as before");
  check(spJudge(["https://www.youtube.com/"], spSettings({})).tap === false, "nothing stored: on by default, YouTube is locked");
  check(spJudge(["https://www.ardmediathek.de/", "https://www.ardmediathek.de/player"], on()).state === "released", "all released: released, tapped");
  const embed = spJudge(["https://blog.example/post", "https://www.youtube-nocookie.com/embed/1"], on());
  check(embed.tap === false && embed.state === "locked", "a locked player embedded in an unlisted site is locked");
  const onLocked = spJudge(["https://www.youtube.com/watch", "https://ads.example/frame"], on());
  check(onLocked.tap === false, "any frame of a locked page is locked");
  check(spJudge(["https://www.ardmediathek.de/", "https://cdn.example/"], on()).state === "unknown", "a released page with an unlisted frame is unknown (tapped, hint)");
  check(spJudge(["https://www.youtube.com/"], on({ sourceUnlocked: ["obfuscated"] })).tap === true, "after unlocking, the group is tapped");
  check(spJudge(["https://www.ardmediathek.de/", "https://www.youtube.com/"], on({ sourceUnlocked: ["obfuscated"] })).state === "unlocked", "released plus unlocked says unlocked");

  // spState reads the stored settings and judges the URLs the browser gave.
  Object.assign(stored, { sourcePolicy: "on" });
  const st = await spState({ url: "https://www.youtube.com/embed/1", tabUrl: "https://www.ardmediathek.de/" });
  check(st.tap === false && st.groups.includes("obfuscated"), "spState: frame and top document are both judged");
  stored.sourceUnlocked = ["obfuscated"];
  check((await spState({ frameId: 0, url: "https://www.youtube.com/" })).tap === true, "spState: the stored unlock is read");
  const noTop = await spState({ frameId: 3, url: "https://www.ardmediathek.de/player" });
  check(noTop.tap === false && noTop.state === "no-top", "spState: a sub-frame without the tab's URL is not tapped (Firefox without host access)");
  check((await spState({ frameId: 0, url: "https://www.ardmediathek.de/" })).tap === true, "spState: a top frame is judged by its own URL");
  stored.sourcePolicy = "off";
  check((await spState({ frameId: 3, url: "https://x.example/" })).state === "off", "spState: policy off, no sub-frame rule");
  stored.sourcePolicy = "on";
  check(spSettings({ sourcePolicy: "off" }).on === false && spSettings({}).on === true, "Chrome build: on by default, off only when stored off");
}

// ---- the content script's gate --------------------------------------------------
{
  const SECRET = "t".repeat(32);
  const store = { sinkToken: SECRET, sourcePolicy: "on" };
  const posts = [];
  let listener = null;
  let runtimeListener = null;
  let storageListener = null;
  const ports = [];
  const answers = { tapState: { on: true }, source: { tap: false, state: "locked", groups: ["obfuscated"] }, asked: 0, silent: false };
  globalThis.window = globalThis;
  globalThis.location = { href: "https://www.youtube.com/watch" };
  globalThis.document = { title: "t", addEventListener: () => {} };
  globalThis.addEventListener = (type, f) => {
    if (type === "message") listener = f;
  };
  globalThis.postMessage = (m) => posts.push(m);
  globalThis.FileReader = class {
    readAsDataURL(blob) {
      blob.arrayBuffer().then((b) => {
        this.result = "data:application/octet-stream;base64," + Buffer.from(b).toString("base64");
        this.onload();
      });
    }
  };
  globalThis.chrome = {
    runtime: {
      id: "oggpddakdbffbjhkjmcakidgmhpgakia",
      lastError: null,
      sendMessage: (m, cb) => {
        if (m.kind === "vc-source-state") {
          answers.asked++;
          // silent: the service worker is gone, the callback gets nothing.
          setTimeout(() => cb(answers.silent ? undefined : answers.source), 5);
        } else if (m.kind === "vc-tap-state") setTimeout(() => cb(answers.tapState), 5);
        else if (cb) cb();
      },
      onMessage: { addListener: (f) => (runtimeListener = f) },
      connect: () => {
        const p = { sent: [], postMessage: (m) => p.sent.push(m), onDisconnect: { addListener: () => {} }, disconnect: () => {} };
        ports.push(p);
        return p;
      },
    },
    storage: {
      local: { get: (d, cb) => setTimeout(() => cb({ ...d, ...store }), 5) },
      onChanged: { addListener: (f) => (storageListener = f) },
    },
  };
  vm.runInThisContext(src("content.js"), { filename: "content.js" });
  const lastTap = () => posts.filter((p) => p.__vcCtl === 1).at(-1)?.tap;
  const append = (isInit) =>
    listener({ source: window, data: { __vc: 1, type: "append", sbId: 1, msId: 1, seq: 1, mime: "video/mp4", kind: "mp4", isInit, payload: new ArrayBuffer(50) } });
  const segmentsOnPorts = () => ports.flatMap((p) => p.sent).filter((m) => m.t === "segment").length;

  await until(() => lastTap() !== undefined);
  check(answers.asked === 1 && lastTap() === false, "policy on, locked site: the service worker is asked and nothing is tapped");
  append(true);
  await sleep(40);
  check(segmentsOnPorts() === 0, "a locked site sends nothing, not even the init");

  // The desktop app switches the tab on: still locked.
  runtimeListener({ kind: "vc-tap", on: true }, { id: "oggpddakdbffbjhkjmcakidgmhpgakia" });
  await until(() => answers.asked === 2);
  await sleep(20);
  check(answers.asked === 2 && lastTap() === false, "the app's tap-on goes through the policy and does not tap a locked site");

  // The user unlocks the group.
  answers.source = { tap: true, state: "unlocked", groups: ["obfuscated"] };
  store.sourceUnlocked = ["obfuscated"];
  storageListener({ sourceUnlocked: { newValue: ["obfuscated"] } }, "local");
  await until(() => lastTap() === true);
  check(lastTap() === true, "unlocking the group (a storage change) taps the site");
  append(true);
  await until(() => segmentsOnPorts() === 1);
  check(segmentsOnPorts() === 1, "an unlocked site sends its segments");

  // The service worker does not answer: fail closed.
  answers.silent = true;
  storageListener({ sourcePolicy: { newValue: "on" } }, "local");
  await until(() => lastTap() === false);
  check(lastTap() === false, "no answer from the service worker means no tap");
  answers.silent = false;

  // Mode selected: the tab must be chosen and the site allowed.
  store.tapMode = "selected";
  answers.tapState = { on: false };
  const askedBefore = answers.asked;
  storageListener({ tapMode: { newValue: "selected" } }, "local");
  await sleep(40);
  check(lastTap() === false && answers.asked === askedBefore, "mode selected, tab not chosen: not tapped, the policy is not even asked");

  // Policy off: the old behaviour, the policy is never asked.
  store.sourcePolicy = "off";
  store.tapMode = "all";
  storageListener({ sourcePolicy: { newValue: "off" } }, "local");
  await until(() => lastTap() === true);
  const askedOff = answers.asked;
  runtimeListener({ kind: "vc-tap", on: true }, { id: "oggpddakdbffbjhkjmcakidgmhpgakia" });
  check(lastTap() === true && answers.asked === askedOff, "policy off: tapped as before, the tap-on message acts at once");
}

console.log(failures.length ? `FAILED: ${failures.length} check(s)` : "ok: all checks passed");
process.exit(failures.length ? 1 : 0);
