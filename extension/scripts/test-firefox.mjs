#!/usr/bin/env node
// The Firefox (for Android) build without a browser: builds into a temporary
// folder and checks the derived manifest (event page, add-on id, no Chrome-only
// keys or permissions, flavour in front of content.js, every listed file
// shipped), reads the .xpi back (every entry inflates to the built file with
// its CRC), loads the background scripts in the manifest's order in a context
// without importScripts (an event page, not a service worker) and checks that
// the pairing is not started and the source policy is on by default; and
// that content.js with the flavour asks the policy before it taps.
// Usage: node extension/scripts/test-firefox.mjs   (exit 0 when all checks pass)
import { readFileSync, mkdtempSync, rmSync, existsSync } from "node:fs";
import { tmpdir } from "node:os";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";
import vm from "node:vm";
import zlib from "node:zlib";
import { build, GECKO_ID } from "./build-firefox.mjs";

const here = dirname(fileURLToPath(import.meta.url));
const root = join(here, "..");
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

const out = mkdtempSync(join(tmpdir(), "streamtap-ff-"));
try {
  const chrome = JSON.parse(readFileSync(join(root, "manifest.json"), "utf8"));
  const r = build(out);
  const m = JSON.parse(readFileSync(join(r.dir, "manifest.json"), "utf8"));

  // ---- the manifest -------------------------------------------------------------
  check(m.manifest_version === 3 && m.version === chrome.version && m.name === chrome.name, "same name, version and manifest version as the Chrome build");
  check(!("key" in m) && !("minimum_chrome_version" in m), "no Chrome key or minimum Chrome version");
  check(!m.permissions.includes("nativeMessaging") && !m.permissions.includes("favicon") && m.permissions.includes("storage"), "permissions: storage, without nativeMessaging and favicon");
  check(!m.background.service_worker && Array.isArray(m.background.scripts), "an event page (background.scripts), no service worker");
  check(m.background.scripts[0] === "src/flavor-firefox.js" && m.background.scripts.at(-1) === "src/background.js", "background: the flavour first, background.js last");
  check(["src/sink-link.js", "src/native-pairing.js", "src/source-policy.js"].every((s) => m.background.scripts.includes(s)), "background: every script background.js imports");
  check(m.browser_specific_settings.gecko.id === GECKO_ID && m.browser_specific_settings.gecko_android.strict_min_version === "128.0", "gecko id and Firefox 128 for Android (MAIN world)");
  const [hook, content] = m.content_scripts;
  check(hook.world === "MAIN" && hook.js.length === 1 && hook.js[0] === "src/hook.js", "the hook runs alone in the MAIN world (no flavour there)");
  check(content.js[0] === "src/flavor-firefox.js" && content.js[1] === "src/content.js", "the flavour runs in front of content.js");
  check(content.all_frames === true && content.run_at === "document_start", "content scripts keep all frames and document_start");
  const listed = [...m.background.scripts, ...m.content_scripts.flatMap((c) => c.js), m.action.default_popup, ...Object.values(m.icons)];
  check(listed.every((p) => existsSync(join(r.dir, p))), "every file the manifest names is shipped");
  check(!r.names.some((n) => n.startsWith("scripts/") || n.endsWith(".md")), "no test scripts or READMEs in the add-on");

  // ---- the xpi --------------------------------------------------------------------
  const x = readFileSync(r.xpi);
  const endAt = x.lastIndexOf(Buffer.from([0x50, 0x4b, 0x05, 0x06]));
  const count = x.readUInt16LE(endAt + 10);
  let at = x.readUInt32LE(endAt + 16);
  let intact = count === r.names.length;
  for (let i = 0; i < count; i++) {
    const nameLen = x.readUInt16LE(at + 28);
    const name = x.subarray(at + 46, at + 46 + nameLen).toString("utf8");
    const crc = x.readUInt32LE(at + 16);
    const size = x.readUInt32LE(at + 20);
    const off = x.readUInt32LE(at + 42);
    const localName = x.readUInt16LE(off + 26);
    const data = zlib.inflateRawSync(x.subarray(off + 30 + localName, off + 30 + localName + size));
    const built = readFileSync(join(r.dir, ...name.split("/")));
    if (!data.equals(built) || zlib.crc32(data) !== crc) intact = false;
    at += 46 + nameLen;
  }
  check(intact, `the xpi holds every built file intact (${count} entries)`);
  const again = build(mkdtempSync(join(out, "again-")));
  check(readFileSync(again.xpi).equals(x), "the xpi is reproducible");

  // ---- the background page ----------------------------------------------------------
  const calls = { connectNative: 0 };
  const stored = {};
  const listeners = {};
  const ev = (name) => ({ addListener: (f) => (listeners[name] = f) });
  const ctx = vm.createContext({
    URL,
    console,
    setTimeout,
    clearTimeout,
    crypto: globalThis.crypto,
    btoa,
    TextEncoder,
    chrome: {
      runtime: {
        id: GECKO_ID,
        lastError: null,
        onMessage: ev("message"),
        onConnect: ev("connect"),
        onInstalled: ev("installed"),
        onStartup: ev("startup"),
        getManifest: () => m,
        getURL: (p) => "moz-extension://x" + p,
        // Firefox for Android has no connectNative: calling it would throw.
        get connectNative() {
          calls.connectNative++;
          return undefined;
        },
      },
      action: { setBadgeText: async () => {}, setBadgeBackgroundColor: async () => {} },
      tabs: { onRemoved: ev("removed"), onUpdated: ev("updated"), reload: async () => {} },
      storage: {
        local: { get: (k, cb) => (cb ? cb({ ...stored }) : Promise.resolve({ ...stored })), set: async () => {} },
        session: { get: async (d) => d, set: async () => {} },
        onChanged: ev("changed"),
      },
    },
  });
  let loaded = true;
  for (const s of m.background.scripts) {
    try {
      vm.runInContext(readFileSync(join(r.dir, s), "utf8"), ctx, { filename: s });
    } catch (e) {
      loaded = false;
      console.log(`  ${s}: ${e && e.message}`);
    }
  }
  check(loaded, "the background scripts load in the manifest's order without importScripts");
  check(calls.connectNative === 0, "the pairing with the desktop app is not started");
  const ask = (msg, sender = {}) =>
    new Promise((resolve) => {
      const async = listeners.message(msg, sender, resolve);
      if (!async) setTimeout(() => resolve(undefined), 50);
    });
  const pair = await ask({ kind: "vc-pair-status" });
  check(pair && pair.error === "unsupported" && pair.connected === false, "the popup is told that pairing is unsupported here");
  await ask({ kind: "vc-pair-now" });
  check(calls.connectNative === 0, "pair now does not try native messaging either");
  const yt = await ask({ kind: "vc-source-state" }, { frameId: 0, url: "https://www.youtube.com/watch", tab: { id: 1, url: "https://www.youtube.com/watch" } });
  check(yt && yt.tap === false && yt.state === "locked", "the source policy is on by default: YouTube is locked");
  const ard = await ask({ kind: "vc-source-state" }, { frameId: 0, url: "https://www.ardmediathek.de/", tab: { id: 1, url: "https://www.ardmediathek.de/" } });
  check(ard && ard.tap === true && ard.state === "released", "a released source is tapped");
  const ov = await ask({ kind: "vc-source-of", tabId: 99 });
  check(ov && ov.on === true && ov.verdict === null && ov.lockable.includes("obfuscated") && ov.lockable.includes("social"), "the popup sees the policy on by default and the lockable groups");
  stored.sourcePolicy = "off";
  const off = await ask({ kind: "vc-source-state" }, { frameId: 0, url: "https://www.youtube.com/", tab: { id: 1, url: "https://www.youtube.com/" } });
  check(off && off.state === "off", "a stored \"off\" turns it off");

  // ---- content.js with the flavour ------------------------------------------------
  {
    const asked = [];
    const posts = [];
    globalThis.window = globalThis;
    globalThis.location = { href: "https://www.youtube.com/watch" };
    globalThis.document = { title: "t", addEventListener: () => {} };
    globalThis.addEventListener = () => {};
    globalThis.postMessage = (p) => posts.push(p);
    globalThis.chrome = {
      runtime: {
        id: GECKO_ID,
        lastError: null,
        sendMessage: (msg, cb) => {
          asked.push(msg.kind);
          if (msg.kind === "vc-source-state") setTimeout(() => cb({ tap: false, state: "locked", groups: ["obfuscated"] }), 5);
          else if (cb) cb();
        },
        onMessage: { addListener: () => {} },
        connect: () => ({ postMessage: () => {}, onDisconnect: { addListener: () => {} }, disconnect: () => {} }),
      },
      storage: {
        local: { get: (d, cb) => setTimeout(() => cb({ ...d, sinkToken: "t".repeat(32) }), 5) },
        onChanged: { addListener: () => {} },
      },
    };
    vm.runInThisContext(readFileSync(join(r.dir, "src/flavor-firefox.js"), "utf8"), { filename: "flavor-firefox.js" });
    vm.runInThisContext(readFileSync(join(r.dir, "src/content.js"), "utf8"), { filename: "content.js" });
    const lastTap = () => posts.filter((p) => p.__vcCtl === 1).at(-1)?.tap;
    await until(() => lastTap() !== undefined);
    check(asked.includes("vc-source-state") && lastTap() === false, "content.js: nothing stored, the flavour turns the policy on and a locked site is not tapped");
  }
} finally {
  rmSync(out, { recursive: true, force: true });
}

console.log(failures.length ? `FAILED: ${failures.length} check(s)` : "ok: all checks passed");
process.exit(failures.length ? 1 : 0);
