"use strict";

// The UI only talks to the app's own Tauri commands (see capabilities/main.json).
// Every string from disk or from the page (titles, URLs, MIME types) is set with
// textContent, never as HTML.

const invoke = (cmd, args) => window.__TAURI__.core.invoke(cmd, args);
const $ = (id) => document.getElementById(id);

const POLL_MS = 2000;

const state = {
  snap: null,
  settings: null,
  tokenShown: false,
  settingsOpen: false,
  browserOpen: false,
  signature: "",
  busy: false,
};

// ---- helpers --------------------------------------------------------------

const el = (tag, props = {}, ...children) => {
  const node = document.createElement(tag);
  for (const [k, v] of Object.entries(props)) {
    if (k === "class") node.className = v;
    else if (k === "text") node.textContent = v;
    else if (k.startsWith("on")) node.addEventListener(k.slice(2), v);
    else if (v === true) node.setAttribute(k, "");
    else if (v !== false && v != null) node.setAttribute(k, v);
  }
  for (const c of children) if (c != null) node.append(c);
  return node;
};

const fmtBytes = (n) => {
  if (!n) return "0 B";
  const units = ["B", "KiB", "MiB", "GiB", "TiB"];
  let i = 0;
  let v = n;
  while (v >= 1024 && i < units.length - 1) {
    v /= 1024;
    i++;
  }
  return `${i === 0 ? v : v.toFixed(v < 10 ? 2 : 1)} ${units[i]}`;
};

const fmtDuration = (ms) => {
  const s = Math.floor(ms / 1000);
  const h = Math.floor(s / 3600);
  const m = Math.floor((s % 3600) / 60);
  const p = (x) => String(x).padStart(2, "0");
  return h ? `${h}:${p(m)}:${p(s % 60)}` : `${m}:${p(s % 60)}`;
};

const fmtDate = (secs) => (secs ? new Date(secs * 1000).toLocaleString() : "unknown time");

let toastTimer = 0;
const toast = (msg) => {
  const t = $("toast");
  t.textContent = msg;
  t.hidden = false;
  clearTimeout(toastTimer);
  toastTimer = setTimeout(() => (t.hidden = true), 5000);
};

const run = async (label, fn) => {
  try {
    await fn();
  } catch (e) {
    toast(`${label}: ${e}`);
  }
  await refresh();
};

// ---- rendering ------------------------------------------------------------

const renderHeader = (snap) => {
  const s = snap.sink;
  const dot = $("sink-dot");
  dot.className = `dot dot-${s.state}`;
  let text = "Sink: stopped";
  if (s.state === "running") text = `Sink: running on 127.0.0.1:${state.settings?.port ?? ""}`;
  if (s.state === "crashed") text = `Sink: crashed${s.exit_code != null ? ` (exit ${s.exit_code})` : ""}`;
  $("sink-text").textContent = text;
  const btn = $("sink-toggle");
  btn.textContent = s.state === "running" ? "Stop" : "Start";
  btn.disabled = state.busy || (s.state !== "running" && !snap.sink_path);
  btn.classList.toggle("danger", s.state === "running");
  btn.classList.toggle("primary", s.state !== "running");
};

const notice = (kind, title, body, pre) =>
  el("div", { class: `notice ${kind}` }, el("strong", { text: title }), body ? el("span", { text: body }) : null, pre ? el("pre", { text: pre }) : null);

const renderNotices = (snap) => {
  const box = $("notices");
  box.replaceChildren();
  const s = snap.sink;
  if (snap.config_error) {
    box.append(notice("error", "The saved settings could not be used.", `${snap.config_error} Defaults are active; saving settings replaces the file.`));
  }
  if (!snap.sink_path) {
    box.append(notice("error", "streamtap-sink was not found.", "It must sit next to this app (or in desktop/target/release while developing). Build it with scripts/build.ps1."));
  }
  if (s.state === "crashed") {
    box.append(notice("error", "The sink stopped by itself.", "Often the port is already in use: change it in Settings, or close the other program. Last output:", snap.log.slice(-8).join("\n")));
  }
  if (!snap.ffmpeg) {
    box.append(notice("warn", "ffmpeg was not found on PATH.", "Capturing works, but muxing a capture into an MP4 needs ffmpeg. Install it and restart this app."));
  }
  if (snap.captures_error) box.append(notice("error", "Cannot list captures.", snap.captures_error));
};

const renderEmpty = (snap) => {
  const box = $("empty");
  box.replaceChildren();
  if (snap.captures.length) {
    box.hidden = true;
    return;
  }
  box.hidden = false;
  const port = state.settings?.port ?? 17650;
  if (snap.sink.state !== "running") {
    box.append(
      el("strong", { text: "The sink is not running." }),
      el("span", { text: "The extension cannot deliver anything while it is stopped. Press Start in the header." }),
    );
    return;
  }
  box.append(
    el("strong", { text: "No captures yet. The sink is waiting for the extension." }),
    el("ol", {},
      el("li", { text: `Open Settings, copy the token, and paste it with port ${port} into the streamtap extension popup (Save).` }),
      el("li", { text: "Reload the page with the video and start playing it." }),
      el("li", { text: "The capture appears here within a few seconds. DRM-protected streams are refused on purpose." }),
    ),
  );
};

const badge = (text, cls) => el("span", { class: `badge ${cls}`, text });

const muxBox = (job) => {
  if (!job) return null;
  if (job.state === "running") return el("div", { class: "mux", role: "status", text: "Muxing with ffmpeg, no re-encoding. This can take a while for long videos." });
  if (job.state === "ok") return el("div", { class: "mux", role: "status", text: "Mux finished. The result is out.mp4 in the capture folder." });
  const code = job.code == null ? "" : ` (exit ${job.code})`;
  return el("div", { class: "mux failed", role: "alert" }, el("strong", { text: `Mux failed${code}: ${job.meaning}` }), job.stderr_tail ? el("pre", { text: job.stderr_tail }) : null);
};

const card = (c, job, detailsOpen) => {
  const title = c.title || "(untitled page)";
  const badges = el("div", { class: "badges" });
  badges.append(badge(c.live ? "live" : "done", c.live ? "live" : "done"));
  if (c.drm) badges.append(badge("DRM refused", "drm"));
  if (c.muxed) badges.append(badge("muxed", "muxed"));
  const muxRunning = job?.state === "running";

  const sbTable = el("table", {},
    el("thead", {}, el("tr", {}, ...["#", "kind", "type", "segments", "size"].map((h) => el("th", { text: h })))),
    el("tbody", {}, ...c.source_buffers.map((s) => el("tr", {},
      el("td", { text: String(s.sb) }), el("td", { text: s.kind || "-" }), el("td", { text: s.mime || "-" }),
      el("td", { text: String(s.segments) }), el("td", { text: fmtBytes(s.bytes) })))),
  );

  const btn = (label, fid, onclick, extra = {}) => el("button", { type: "button", "data-fid": `${c.id}:${fid}`, onclick, ...extra, text: label });
  const actions = el("div", { class: "actions" },
    btn(muxRunning ? "Muxing..." : c.muxed ? "Mux again" : "Mux", "mux", () => run("Mux", () => invoke("mux_capture", { id: c.id })),
      { disabled: muxRunning || c.live || !c.segments, title: c.live ? "Wait until the capture is done" : !c.segments ? "Nothing to mux yet" : "" }),
    btn("Open result", "open", () => run("Open result", () => invoke("open_result", { id: c.id })), { disabled: !c.muxed }),
    btn("Open folder", "folder", () => run("Open folder", () => invoke("open_folder", { id: c.id }))),
    btn("Delete", "delete", () => askDelete(c), { class: "danger", disabled: muxRunning || c.live }),
  );

  return el("li", { class: "card", "data-id": c.id },
    el("h3", { text: title }),
    el("div", { class: "sub", text: `${c.host || "unknown host"} - ${fmtDate(c.started)}`, title: c.url }),
    badges,
    el("div", { class: "meta" },
      el("span", { text: `Duration ${c.duration_ms ? fmtDuration(c.duration_ms) : "-"}` }),
      el("span", { text: fmtBytes(c.bytes) }),
      el("span", { text: `${c.segments} segments` }),
      el("span", { text: `${c.source_buffers.length} SourceBuffer${c.source_buffers.length === 1 ? "" : "s"}` }),
      c.muxed ? el("span", { text: `Result ${fmtBytes(c.result_bytes)}` }) : null,
    ),
    c.source_buffers.length ? el("details", { open: detailsOpen }, el("summary", { text: "SourceBuffers" }), sbTable) : null,
    muxBox(job),
    actions,
  );
};

const renderList = (snap) => {
  const list = $("list");
  // The list is rebuilt on every change (a live capture changes every poll): keep
  // the SourceBuffer tables the user opened.
  const open = new Set([...list.querySelectorAll("li.card details[open]")].map((d) => d.closest("li").dataset.id));
  list.replaceChildren(...snap.captures.map((c) => card(c, snap.mux[c.id], open.has(c.id))));
  $("captures-count").textContent = snap.captures.length ? `(${snap.captures.length})` : "";
};

const renderSettings = (snap) => {
  const running = snap.sink.state === "running";
  const s = state.settings;
  $("settings-lock").hidden = !running;
  for (const id of ["port", "dir", "token-new", "settings-save"]) $(id).disabled = running;
  if (s && document.activeElement !== $("port") && document.activeElement !== $("dir")) {
    $("port").value = s.port;
    $("dir").value = s.captures_dir;
  }
  if (s) $("token").value = s.token;
  $("token").type = state.tokenShown ? "text" : "password";
  $("token-show").textContent = state.tokenShown ? "Hide" : "Show";
  $("token-show").setAttribute("aria-pressed", String(state.tokenShown));
  $("fact-ffmpeg").textContent = snap.ffmpeg || "not found on PATH";
  $("fact-sink").textContent = snap.sink_path || "not found";
};

const renderLog = (snap) => {
  const pre = $("log");
  const atEnd = pre.scrollTop + pre.clientHeight >= pre.scrollHeight - 8;
  pre.textContent = snap.log.length ? snap.log.join("\n") : "(no output yet)";
  if (atEnd) pre.scrollTop = pre.scrollHeight;
};

// ---- browser link ---------------------------------------------------------

const BROWSER_NAMES = { chrome: "Chrome", edge: "Edge", brave: "Brave", other: "Browser" };
const browserName = (id) => BROWSER_NAMES[id] ?? "Browser";
const REG_TEXT = { registered: "registered", "not-registered": "not registered", stale: "registered for another location" };

// Page-controlled strings (titles, hosts) arrive cleaned and bounded from the app and
// are set with textContent only. A favicon is shown only when it is an image data URL.
const faviconImg = (url) =>
  typeof url === "string" && /^data:image\/(png|jpeg|gif|webp|x-icon);base64,/.test(url)
    ? el("img", { class: "fav", src: url, alt: "", width: "16", height: "16" })
    : el("span", { class: "fav", "aria-hidden": "true" });

const renderPairing = (snap) => {
  const box = $("pairing");
  box.replaceChildren();
  for (const c of snap.browser.link.connections.filter((x) => !x.approved)) {
    box.append(
      el("div", { class: "notice warn", role: "alert" },
        el("strong", { text: `${browserName(c.browser)} (profile ${c.profile}) wants to pair with streamtap.` }),
        el("span", { text: "If you started this just now, approve it: the extension then receives the sink's port and token and can list your video tabs. Otherwise deny." }),
        el("div", { class: "row" },
          el("button", { type: "button", class: "primary", "data-fid": `pair:${c.id}:yes`, text: "Approve", onclick: () => run("Approve", () => invoke("browser_decide", { conn: c.id, approve: true })) }),
          el("button", { type: "button", "data-fid": `pair:${c.id}:no`, text: "Deny", onclick: () => run("Deny", () => invoke("browser_decide", { conn: c.id, approve: false })) }),
        )),
    );
  }
};

const renderBrowser = (snap) => {
  const b = snap.browser;
  const reg = $("browser-reg");
  reg.replaceChildren(
    ...b.registrations.map((r) => {
      const registered = r.state === "registered";
      const label = r.error ? `error: ${r.error}` : REG_TEXT[r.state] ?? "unknown";
      return el("li", { class: "regrow" },
        el("strong", { text: browserName(r.browser) }),
        el("span", { class: registered ? "ok" : "muted", text: label }),
        el("span", { class: "grow" }),
        registered
          ? el("button", { type: "button", "data-fid": `reg:${r.browser}:rm`, text: "Remove", onclick: () => run("Remove", () => invoke("browser_unregister", { browser: r.browser })) })
          : el("button", { type: "button", class: "primary", "data-fid": `reg:${r.browser}:add`, text: r.state === "stale" ? "Register again" : "Register", disabled: !b.host_path, title: b.host_path ? "" : "streamtap-host was not found next to the app", onclick: () => run("Register", () => invoke("browser_register", { browser: r.browser })) }),
      );
    }),
  );
  const note = $("browser-note");
  const notes = [];
  if (!b.host_path) notes.push("streamtap-host was not found next to the app (build it with scripts/build.ps1).");
  if (b.bridge_error) notes.push(b.bridge_error);
  if (b.link.store_error) notes.push(b.link.store_error);
  note.hidden = !notes.length;
  note.textContent = notes.join(" ");

  const list = $("approvals");
  list.replaceChildren(
    ...(b.link.approvals.length
      ? b.link.approvals.map((a) => el("li", { class: "regrow" },
          el("strong", { text: `${browserName(a.browser)} (profile ${a.profile})` }),
          el("span", { class: a.connected ? "ok" : "muted", text: a.connected ? "connected" : "not connected" }),
          el("span", { class: "grow" }),
          el("button", { type: "button", class: "danger", "data-fid": `rev:${a.instance}`, text: "Revoke", onclick: () => run("Revoke", () => invoke("browser_revoke", { instance: a.instance })) })))
      : [el("li", { class: "muted", text: "No browser is paired yet." })]),
  );

  const running = snap.sink.state === "running";
  const approved = b.link.connections.filter((c) => c.approved);
  $("tabs-hint").textContent = approved.length
    ? running
      ? "Capture reloads the tab once so that it sees the start of the stream. No previews of the page are taken (that would need access to all sites); icon and title come from the tab."
      : "Start the sink (header) before you switch a tab to capture."
    : "A paired and connected browser lists the tabs that hold a video here.";
  const box = $("tabs");
  box.replaceChildren(
    ...approved.map((c) =>
      el("div", { class: "browser-tabs" },
        el("h4", { text: `${browserName(c.browser)} (profile ${c.profile})` }),
        c.tabs.length
          ? el("ul", { class: "plain" }, ...c.tabs.map((t) =>
              el("li", { class: "tabrow" },
                faviconImg(t.favicon),
                el("span", { class: "tabtext" },
                  el("span", { class: "tabtitle", text: t.title || t.host || "(untitled)" }),
                  el("span", { class: "muted", text: `${t.host}${t.active ? " - visible tab" : ""}` })),
                t.drm ? badge("DRM", "drm") : null,
                t.tapping ? badge("capturing", "live") : null,
                el("button", {
                  type: "button",
                  class: t.tapping ? "danger" : "primary",
                  "data-fid": `tab:${c.id}:${t.id}`,
                  text: t.tapping ? "Stop" : "Capture",
                  disabled: (!t.tapping && !running) || (t.drm && !t.tapping),
                  title: t.drm ? "DRM-protected: streamtap leaves it alone" : "",
                  onclick: () => run("Capture", () => invoke("browser_tap", { conn: c.id, tab: t.id, on: !t.tapping })),
                }))))
          : el("p", { class: "muted", text: "No tab with a video yet. Open a page and start playing." })),
    ),
  );
};

const render = (snap) => {
  const sig = JSON.stringify([snap, state.settings, state.tokenShown, state.busy]);
  if (sig === state.signature) return;
  state.signature = sig;
  const focusId = document.activeElement?.dataset?.fid;
  renderHeader(snap);
  renderNotices(snap);
  renderEmpty(snap);
  renderList(snap);
  renderSettings(snap);
  renderLog(snap);
  renderPairing(snap);
  renderBrowser(snap);
  if (focusId) document.querySelector(`[data-fid="${CSS.escape(focusId)}"]`)?.focus();
};

// ---- actions --------------------------------------------------------------

const refresh = async () => {
  try {
    if (!state.settings) state.settings = await invoke("get_settings");
    state.snap = await invoke("snapshot");
    render(state.snap);
  } catch (e) {
    $("sink-text").textContent = `App error: ${e}`;
  }
};

let pending = null;
const askDelete = (c) => {
  pending = c.id;
  $("confirm-text").textContent = `"${c.title || c.id}" (${fmtBytes(c.bytes)}) and its folder ${c.id} will be removed from disk. This cannot be undone.`;
  $("confirm").showModal();
  $("confirm-cancel").focus();
};

$("confirm").addEventListener("close", () => {
  const id = pending;
  pending = null;
  if ($("confirm").returnValue === "ok" && id) run("Delete", () => invoke("delete_capture", { id }));
});

$("sink-toggle").addEventListener("click", async () => {
  state.busy = true;
  const running = state.snap?.sink.state === "running";
  await run(running ? "Stop" : "Start", () => invoke(running ? "stop_sink" : "start_sink"));
  state.busy = false;
  await refresh();
});

$("settings-toggle").addEventListener("click", () => {
  state.settingsOpen = !state.settingsOpen;
  $("settings").hidden = !state.settingsOpen;
  $("settings-toggle").setAttribute("aria-expanded", String(state.settingsOpen));
  if (state.settingsOpen) $("port").focus();
});

$("browser-toggle").addEventListener("click", () => {
  state.browserOpen = !state.browserOpen;
  $("browser").hidden = !state.browserOpen;
  $("browser-toggle").setAttribute("aria-expanded", String(state.browserOpen));
});

$("settings-form").addEventListener("submit", (ev) => {
  ev.preventDefault();
  run("Save settings", async () => {
    state.settings = await invoke("save_settings", { port: Number($("port").value), capturesDir: $("dir").value });
    toast("Settings saved");
  });
});

$("token-show").addEventListener("click", () => {
  state.tokenShown = !state.tokenShown;
  if (state.snap) render(state.snap);
});

const copyText = async (text) => {
  try {
    await navigator.clipboard.writeText(text);
    return true;
  } catch {
    // Fall back to selecting the (readonly) field for a manual Ctrl+C.
    const input = $("token");
    const prev = input.type;
    input.type = "text";
    input.select();
    const ok = document.execCommand?.("copy") ?? false;
    input.type = prev;
    return ok;
  }
};

$("token-copy").addEventListener("click", async () => {
  if (!state.settings) return;
  toast((await copyText(state.settings.token)) ? "Token copied" : "Copy failed: press Show, then Ctrl+C");
});

$("token-new").addEventListener("click", () =>
  run("New token", async () => {
    state.settings = await invoke("regenerate_token");
    toast("New token created. Update it in the extension popup too.");
  }),
);

// ---- start ----------------------------------------------------------------

let polling = false;
const tick = async () => {
  if (polling) return;
  polling = true;
  await refresh();
  polling = false;
};

tick();
setInterval(tick, POLL_MS);
