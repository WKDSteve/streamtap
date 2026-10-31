#!/usr/bin/env node
// Drives extension/src/native-pairing.js (the pairing with the desktop app) in a
// vm context with a fake native port, fake storage and fake timers, and the
// per-tab tapping gate of extension/src/content.js. No browser, no app.
// Usage: node extension/scripts/test-pairing.mjs   (exit 0 when all checks pass)
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
const flush = () => sleep(20);

const SECRET = "0123456789abcdef0123456789abcdef";

// --- fakes ---------------------------------------------------------------------
const makeStorage = (initial = {}) => {
  const data = { ...initial };
  return {
    data,
    get: async (d) => {
      const out = { ...d };
      for (const k of Object.keys(d)) if (k in data) out[k] = data[k];
      return out;
    },
    set: async (o) => void Object.assign(data, o),
    remove: async (keys) => void [].concat(keys).forEach((k) => delete data[k]),
  };
};

const makeWorld = (opts = {}) => {
  const local = makeStorage(opts.local);
  const session = makeStorage(opts.session);
  const ports = [];
  const sentToTabs = [];
  const reloads = [];
  const timers = [];
  const logs = [];
  let nextConnectThrows = false;
  const png = Uint8Array.from([0x89, 0x50, 0x4e, 0x47, 1, 2, 3, 4, 5, 6, 7, 8]);
  const world = {
    local,
    session,
    ports,
    sentToTabs,
    reloads,
    timers,
    logs,
    media: opts.media ?? [],
    active: opts.active ?? [],
    favicon: "favicon" in opts ? opts.favicon : png,
    faviconCalls: 0,
    failConnect: () => (nextConnectThrows = true),
  };
  const chrome = {
    runtime: {
      lastError: null,
      getURL: (p) => "chrome-extension://oggpddakdbffbjhkjmcakidgmhpgakia" + p,
      getManifest: () => ({ version: "0.3.0" }),
      connectNative: (name) => {
        if (nextConnectThrows) {
          nextConnectThrows = false;
          throw new Error("no host");
        }
        const handlers = { msg: [], disc: [] };
        const p = {
          name,
          sent: [],
          onMessage: { addListener: (f) => handlers.msg.push(f) },
          onDisconnect: { addListener: (f) => handlers.disc.push(f) },
          postMessage: (m) => p.sent.push(m),
          emit: (m) => handlers.msg.forEach((f) => f(m)),
          drop: () => handlers.disc.forEach((f) => f()),
        };
        ports.push(p);
        return p;
      },
    },
    storage: { local, session },
    tabs: {
      sendMessage: async (id, m) => void sentToTabs.push([id, m]),
      reload: async (id) => void reloads.push(id),
      query: async () => world.active.map((id) => ({ id })),
    },
  };
  const ctx = vm.createContext({
    console: { log: (...a) => logs.push(a), debug: (...a) => logs.push(a), warn: (...a) => logs.push(a), error: (...a) => logs.push(a) },
    chrome,
    navigator: { userAgentData: { brands: opts.brands ?? [{ brand: "Chromium" }, { brand: "Google Chrome" }] } },
    crypto: { randomUUID: () => "11111111-2222-3333-4444-555555555555" },
    btoa: (s) => Buffer.from(s, "binary").toString("base64"),
    URL,
    Date,
    setTimeout: (f, ms) => {
      timers.push({ f, ms, live: true });
      return timers.length;
    },
    clearTimeout: (h) => {
      if (timers[h - 1]) timers[h - 1].live = false;
    },
    fetch: async () => {
      world.faviconCalls++;
      return { ok: world.favicon !== null, arrayBuffer: async () => world.favicon.buffer.slice(world.favicon.byteOffset, world.favicon.byteOffset + world.favicon.byteLength) };
    },
  });
  vm.runInContext(src("native-pairing.js"), ctx, { filename: "native-pairing.js" });
  world.run = (code) => vm.runInContext(code, ctx);
  world.start = () => {
    ctx.__deps = { mediaTabs: () => world.media };
    world.run("npInit(__deps)");
  };
  world.port = () => ports.at(-1);
  world.liveTimers = () => timers.filter((t) => t.live);
  // Runs the pending timers (once) whatever their delay.
  world.fire = async () => {
    for (const t of [...timers]) {
      if (t.live) {
        t.live = false;
        t.f();
      }
    }
    await flush();
  };
  return world;
};

const approve = async (w) => {
  w.port().emit({ v: 1, t: "state", pairing: "approved" });
  await flush();
};
const tabsSent = (w) => w.port().sent.filter((m) => m.t === "tabs");

// --- connection and hello ------------------------------------------------------
{
  const w = makeWorld({ brands: [{ brand: "Microsoft Edge" }] });
  w.start();
  await flush();
  const hello = w.port().sent[0];
  check(w.port().name === "io.github.wkdsteve.streamtap", "connects to the host by its registered name");
  check(
    hello && hello.t === "hello" && hello.v === 1 && hello.browser === "edge" && hello.version === "0.3.0" && /^[A-Za-z0-9-]{8,64}$/.test(hello.instance),
    `hello names the browser, version and a profile instance (got ${JSON.stringify(hello)})`
  );
  check(w.local.data.pairInstance === hello.instance, "the instance id is stored, so the same profile is the same instance");
  check(Object.keys(hello).sort().join() === "browser,instance,t,v,version", "hello carries nothing but the schema's fields");
}

// --- credentials only after approval ---------------------------------------------
{
  const w = makeWorld();
  w.start();
  await flush();
  const good = { v: 1, t: "credentials", port: 17650, token: SECRET };
  w.port().emit(good);
  await flush();
  check(!("sinkToken" in w.local.data), "credentials before any approval are ignored");
  w.port().emit({ v: 1, t: "state", pairing: "pending" });
  w.port().emit(good);
  await flush();
  check(!("sinkToken" in w.local.data), "credentials while the pairing is pending are ignored");
  await approve(w);
  for (const bad of [
    { ...good, port: 80 },
    { ...good, port: "17650" },
    { ...good, token: "short" },
    { ...good, token: "--looks-like-a-flag-0123456789" },
    { ...good, token: "has space in it 0123456789" },
    { ...good, v: 2 },
  ]) {
    w.port().emit(bad);
    await flush();
  }
  check(!("sinkToken" in w.local.data), "credentials with a bad port, token or version are not stored");
  w.port().emit(good);
  await flush();
  check(
    w.local.data.sinkPort === 17650 && w.local.data.sinkToken === SECRET && w.local.data.sinkPaired === true && w.local.data.tapMode === "selected",
    "approved credentials are stored like the popup stores them, in mode selected"
  );
  check(!JSON.stringify(w.run("npStatus()")).includes(SECRET), "the status the popup reads never contains the token");
  check(!w.logs.some((l) => JSON.stringify(l).includes(SECRET)), "the token is never logged");
}

// --- the tab list -----------------------------------------------------------------
{
  const w = makeWorld({
    active: [5],
    media: [
      { id: 5, url: "https://video.example/watch?v=secret#frag", title: "Ti\u0007tle\u202e with\n\tcontrols  " + "x".repeat(500), drm: false },
      { id: 6, url: "chrome://extensions/", title: "internal", drm: true },
      { id: 7, url: "https://other.example:8443/p", title: "", drm: true },
    ],
  });
  w.start();
  await flush();
  await approve(w);
  await w.fire();
  const tabs = tabsSent(w).at(-1)?.tabs ?? [];
  check(tabs.length === 3, `the tabs that hold a MediaSource are reported (got ${tabs.length})`);
  const a = tabs.find((t) => t.id === 5);
  check(a && a.host === "video.example", "only the host is reported, not path, query or fragment");
  check(a && !/[\u0000-\u001f\u202e]/.test(a.title) && a.title.length <= 200 && a.title.startsWith("Ti tle with controls"), `titles are cleaned and bounded (got ${JSON.stringify(a && a.title.slice(0, 40))})`);
  check(a && a.active === true && tabs.find((t) => t.id === 7).active === false, "the visible tab of the window is marked");
  check(a && /^data:image\/png;base64,/.test(a.favicon), "the favicon comes as a small PNG data URL");
  check(tabs.find((t) => t.id === 6).host === "" && tabs.find((t) => t.id === 7).host === "other.example", "non-web URLs have no host; the port is not part of it");
  check(tabs.find((t) => t.id === 7).drm === true, "the DRM flag is reported");
  const before = tabsSent(w).length;
  w.run("npScheduleSend(false)");
  await w.fire();
  check(tabsSent(w).length === before, "an unchanged list is not sent again");
  w.media.push({ id: 9, url: "https://new.example/", title: "new", drm: false });
  w.run("npScheduleSend(false)");
  await w.fire();
  check(tabsSent(w).length === before + 1, "a changed list is sent");

  // A favicon that is not a small PNG is dropped.
  const w2 = makeWorld({ media: [{ id: 1, url: "https://a.example/", title: "a", drm: false }], favicon: new Uint8Array(20000).fill(0x41) });
  w2.start();
  await flush();
  await approve(w2);
  await w2.fire();
  check(tabsSent(w2).at(-1).tabs[0].favicon === null, "a favicon over the size limit or without a PNG header is not sent");
  const w3 = makeWorld({ media: [{ id: 1, url: "https://a.example/", title: "a", drm: false }], favicon: null });
  w3.start();
  await flush();
  await approve(w3);
  await w3.fire();
  check(tabsSent(w3).at(-1).tabs[0].favicon === null, "a failing favicon request leaves the tab in the list without an icon");
  check(tabsSent(w3).at(-1).tabs.length === 1, "the tab list does not depend on the favicon");
}

// --- tap on / off -------------------------------------------------------------------
{
  const w = makeWorld({ media: [{ id: 5, url: "https://video.example/", title: "v", drm: false }] });
  w.start();
  await flush();
  w.port().emit({ v: 1, t: "tap", tab: 5, on: true });
  await flush();
  check(w.reloads.length === 0 && w.sentToTabs.length === 0, "a tap request before the approval is ignored");
  await approve(w);
  w.port().emit({ v: 1, t: "tap", tab: 99, on: true });
  await flush();
  check(w.reloads.length === 0 && w.sentToTabs.length === 0, "a tab that holds no MediaSource cannot be switched on");
  for (const bad of [{ tab: "5", on: true }, { tab: 5, on: "yes" }, { tab: -1, on: true }, { tab: 5.5, on: true }]) {
    w.port().emit({ v: 1, t: "tap", ...bad });
  }
  await flush();
  check(w.reloads.length === 0, "malformed tap requests are ignored");

  w.port().emit({ v: 1, t: "tap", tab: 5, on: true });
  await flush();
  check(JSON.stringify(w.session.data.tapTabs) === "[5]", "the selection survives a service worker restart (session storage)");
  check(w.reloads.join() === "5", "switching on reloads the tab once so the capture sees the start of the stream");
  check(w.sentToTabs.some(([id, m]) => id === 5 && m.kind === "vc-tap" && m.on === true), "the content scripts of the tab are told");
  check((await w.run("npTapState(5)")).on === true && (await w.run("npTapState(6)")).on === false, "only the chosen tab answers on");
  await w.fire();
  check(tabsSent(w).at(-1).tabs[0].tapping === true, "the list reports the tab as tapping");

  w.port().emit({ v: 1, t: "tap", tab: 5, on: true });
  await flush();
  check(w.reloads.length === 1, "switching on twice reloads once");
  w.port().emit({ v: 1, t: "tap", tab: 5, on: false });
  await flush();
  check(w.reloads.length === 1 && w.sentToTabs.at(-1)[1].on === false && (await w.run("npTapState(5)")).on === false, "switching off tells the tab and does not reload it");

  // A restart: the selection comes back from the session storage.
  const w2 = makeWorld({ session: { tapTabs: [5, "x", -2, 7.5, 8], tapOrigins: { 5: "https://video.example", 8: "javascript:alert(1)" } } });
  w2.start();
  check((await w2.run("npTapState(5)")).on === true && (await w2.run("npTapState(7)")).on === false, "after a restart only valid stored tab ids are tapped");
  check((await w2.run("npTapState(8)")).on === false, "a stored tab whose origin is not a web origin (or is missing) is not tapped");

  w.run("npTabClosed(5)");
  w.port().emit({ v: 1, t: "tap", tab: 5, on: true });
  await flush();
  w.run("npTabClosed(5)");
  await flush();
  check(JSON.stringify(w.session.data.tapTabs) === "[]", "a closed tab is no longer tapped");
}

// --- a choice belongs to the site it was made on ---------------------------------------------
{
  const w = makeWorld({
    media: [
      { id: 5, url: "https://video.example/watch?v=1", title: "v", drm: false },
      { id: 6, url: "chrome://extensions/", title: "internal", drm: false },
    ],
  });
  w.start();
  await flush();
  await approve(w);
  w.port().emit({ v: 1, t: "tap", tab: 6, on: true });
  await flush();
  check(w.reloads.length === 0 && !w.session.data.tapTabs, "a tab that is not a web page cannot be switched on");
  w.port().emit({ v: 1, t: "tap", tab: 5, on: true });
  await flush();
  const ask = (frame) => w.run(`npTapState(5, ${JSON.stringify(frame)})`);
  check((await ask({ frameId: 0, url: "https://video.example/another/page?x=1" })).on === true, "the same origin keeps the choice (another page of the site)");
  check((await ask({ frameId: 3, url: "https://ads.example/frame" })).on === true, "a frame below the top is not what the choice is judged by");
  check((await ask({ frameId: 0, url: "https://video.example/reloaded" })).on === true && w.session.data.tapTabs.join() === "5", "a reload of the chosen page keeps the choice");
  w.sentToTabs.length = 0;
  check((await ask({ frameId: 0, url: "https://bank.example/login" })).on === false, "a top frame on another origin is not tapped");
  check(
    (await w.run("npTapState(5)")).on === false && w.session.data.tapTabs.length === 0 && Object.keys(w.session.data.tapOrigins).length === 0,
    "and the choice is over, also in the stored selection"
  );
  check(w.sentToTabs.some(([id, m]) => id === 5 && m.kind === "vc-tap" && m.on === false), "the tab's content scripts are told");
  await w.fire();
  check(tabsSent(w).at(-1).tabs[0].tapping === false, "the app's list shows the tab as no longer captured");
  // The scheme and the port are part of the origin.
  for (const other of ["http://video.example/", "https://video.example:8443/"]) {
    const x = makeWorld({ media: [{ id: 5, url: "https://video.example/", title: "v", drm: false }] });
    x.start();
    await flush();
    await approve(x);
    x.port().emit({ v: 1, t: "tap", tab: 5, on: true });
    await flush();
    check((await x.run(`npTapState(5, ${JSON.stringify({ frameId: 0, url: other })})`)).on === false, `${other} is another origin`);
  }
}

// --- nothing handed out stays when the approval is gone ----------------------------------------
{
  for (const pairing of ["pending", "denied", "revoked"]) {
    const w = makeWorld({
      local: { sinkToken: SECRET, sinkPaired: true, tapMode: "selected", sinkPort: 17650 },
      session: { tapTabs: [3], tapOrigins: { 3: "https://video.example" } },
    });
    w.start();
    await flush();
    w.port().emit({ v: 1, t: "state", pairing });
    await flush();
    check(!("sinkToken" in w.local.data) && !("tapMode" in w.local.data) && w.session.data.tapTabs.length === 0, `a paired token and selection do not outlive the approval (${pairing}, also when it ended while the browser was away)`);
  }
  const manual = makeWorld({ local: { sinkToken: SECRET, sinkPaired: false, sinkPort: 17650 } });
  manual.start();
  await flush();
  manual.port().emit({ v: 1, t: "state", pairing: "pending" });
  await flush();
  check(manual.local.data.sinkToken === SECRET, "a token entered by hand is not touched by a pending state");
}

// --- denial, revocation, retries ---------------------------------------------------------
{
  const w = makeWorld();
  w.start();
  await flush();
  w.port().emit({ v: 1, t: "state", pairing: "denied" });
  await flush();
  w.port().drop();
  const retry = w.liveTimers().at(-1);
  check(retry && retry.ms >= 9 * 60 * 1000, `after a denial nobody asks again for minutes (delay ${retry && retry.ms} ms)`);
  w.run("npRetryNow()");
  await flush();
  check(w.ports.length === 2, "the popup's pair-now asks again at once");

  // Revocation takes paired credentials back, a manual setup stays.
  const paired = makeWorld({ local: { sinkToken: SECRET, sinkPaired: true, tapMode: "selected", sinkPort: 17650 }, session: { tapTabs: [3], tapOrigins: { 3: "https://video.example" } } });
  paired.start();
  await flush();
  await approve(paired);
  paired.port().emit({ v: 1, t: "state", pairing: "revoked" });
  await flush();
  check(!("sinkToken" in paired.local.data) && !("tapMode" in paired.local.data) && paired.session.data.tapTabs.length === 0, "a revoked pairing removes the token and the selection");
  const manual = makeWorld({ local: { sinkToken: SECRET, sinkPaired: false, sinkPort: 17650 } });
  manual.start();
  await flush();
  await approve(manual);
  manual.port().emit({ v: 1, t: "state", pairing: "revoked" });
  await flush();
  check(manual.local.data.sinkToken === SECRET, "a token entered by hand in the popup is not touched by a revocation");

  // A host that is not there: backoff, no tight loop.
  const gone = makeWorld();
  gone.start();
  await flush();
  const delays = [];
  for (let i = 0; i < 4; i++) {
    gone.port().drop();
    delays.push(gone.liveTimers().at(-1).ms);
    await gone.fire();
  }
  check(delays[0] >= 5000 && delays[1] > delays[0] && delays.every((d) => d <= 5 * 60 * 1000), `reconnects back off (${delays.join(", ")} ms)`);
  const throwing = makeWorld();
  throwing.failConnect();
  throwing.start();
  await flush();
  check(throwing.ports.length === 0 && throwing.liveTimers().length === 1, "a connectNative that throws is retried later, not lost");
}

// --- junk from the host -----------------------------------------------------------------
{
  const w = makeWorld({ media: [{ id: 5, url: "https://video.example/", title: "v", drm: false }] });
  w.start();
  await flush();
  for (const junk of [null, 5, "state", [], { t: "state", pairing: "approved" }, { v: 2, t: "state", pairing: "approved" }, { v: 1, t: "state", pairing: "root" }, { v: 1, t: "shell" }]) {
    w.port().emit(junk);
  }
  await flush();
  check(w.run("npStatus()").pairing === null, "messages outside the schema change nothing");
  w.port().emit({ v: 1, t: "error", code: "app-not-running".repeat(10) });
  await flush();
  check(w.run("npStatus()").error.length <= 32, "an error code is bounded");
}

// --- content.js: the tapping gate ----------------------------------------------------------
{
  const store = { sinkToken: SECRET };
  const posts = [];
  let listener = null;
  let runtimeListener = null;
  let storageListener = null;
  const ports = [];
  const tapState = { on: false, asked: 0 };
  globalThis.window = globalThis;
  globalThis.location = { href: "https://video.example/watch" };
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
        if (m.kind === "vc-tap-state") {
          tapState.asked++;
          setTimeout(() => cb({ on: tapState.on }), 5);
        } else if (cb) cb();
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
  await sleep(40);
  const lastTap = () => posts.filter((p) => p.__vcCtl === 1).at(-1)?.tap;
  const append = (isInit) =>
    listener({ source: window, data: { __vc: 1, type: "append", sbId: 1, msId: 1, seq: 1, mime: "video/mp4", kind: "mp4", isInit, payload: new ArrayBuffer(50) } });
  const segmentsOnPorts = () => ports.flatMap((p) => p.sent).filter((m) => m.t === "segment").length;

  check(lastTap() === true && tapState.asked === 0, "mode all (popup setup): every tab is tapped, the service worker is not asked");
  append(true);
  await sleep(60);
  check(segmentsOnPorts() === 1, "mode all: a segment is sent");

  // Mode selected, tab not chosen.
  store.tapMode = "selected";
  storageListener({ tapMode: { newValue: "selected" } }, "local");
  await sleep(40);
  check(tapState.asked === 1 && lastTap() === false, "mode selected: the service worker is asked and the tab is not tapped");
  const before = segmentsOnPorts();
  append(false);
  await sleep(60);
  check(segmentsOnPorts() === before, "mode selected, tab not chosen: nothing is sent");

  // The app switches the tab on, then off.
  runtimeListener({ kind: "vc-tap", on: true }, { id: "oggpddakdbffbjhkjmcakidgmhpgakia" });
  check(lastTap() === true, "the app's tap-on message switches the tab on");
  append(true);
  await sleep(60);
  check(segmentsOnPorts() === before + 1, "a chosen tab sends its segments");
  runtimeListener({ kind: "vc-tap", on: false }, { id: "oggpddakdbffbjhkjmcakidgmhpgakia" });
  check(lastTap() === false, "tap-off stops the copying");
  runtimeListener({ kind: "vc-tap", on: true }, { id: "some-other-extension" });
  check(lastTap() === false, "a tap message from another sender is ignored");
  runtimeListener({ kind: "vc-tap", on: "yes" }, { id: "oggpddakdbffbjhkjmcakidgmhpgakia" });
  check(lastTap() === false, "a tap message without a boolean is ignored");

  // After a reload the service worker says on.
  tapState.on = true;
  storageListener({ tapMode: { newValue: "selected" } }, "local");
  await sleep(40);
  check(lastTap() === true, "a fresh document of a chosen tab is tapped from its first segment");

  // A late answer does not undo a newer message.
  tapState.on = false;
  storageListener({ tapMode: { newValue: "selected" } }, "local");
  runtimeListener({ kind: "vc-tap", on: true }, { id: "oggpddakdbffbjhkjmcakidgmhpgakia" });
  await sleep(40);
  check(lastTap() === true, "an older question's answer does not overwrite a newer tap message");

  // Token removed (revocation): no tapping.
  delete store.sinkToken;
  storageListener({ sinkToken: { newValue: undefined } }, "local");
  await sleep(40);
  check(lastTap() === false, "without a token nothing is tapped, whatever the mode");
}

console.log(failures.length ? `FAILED: ${failures.length} check(s)` : "ok: all checks passed");
process.exit(failures.length ? 1 : 0);
