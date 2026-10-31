"use strict";

// The UI only talks to the app's own Tauri commands (see capabilities/main.json).
// Every string from disk or from the page (titles, URLs, MIME types) is set with
// textContent, never as HTML. UI texts come from I18N (i18n.js); error texts of the
// commands stay as the app sends them (English).

const invoke = (cmd, args) => window.__TAURI__.core.invoke(cmd, args);
const $ = (id) => document.getElementById(id);
const t = I18N.t;

const POLL_MS = 2000;
const LANGUAGES = ["system", "de", "en"];
const THEMES = ["system", "light", "dark"];

const state = {
  snap: null,
  settings: null,
  tokenShown: false,
  settingsOpen: false,
  browserOpen: false,
  signature: "",
  busy: false,
  // Language and theme as stored ("system" follows the webview / the OS).
  prefs: { language: "system", theme: "system" },
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

const fmtNumber = (v, digits) => v.toLocaleString(I18N.lang, { minimumFractionDigits: digits, maximumFractionDigits: digits });

const fmtBytes = (n) => {
  if (!n) return "0 B";
  const units = ["B", "KiB", "MiB", "GiB", "TiB"];
  let i = 0;
  let v = n;
  while (v >= 1024 && i < units.length - 1) {
    v /= 1024;
    i++;
  }
  return `${i === 0 ? v : fmtNumber(v, v < 10 ? 2 : 1)} ${units[i]}`;
};

const fmtDuration = (ms) => {
  const s = Math.floor(ms / 1000);
  const h = Math.floor(s / 3600);
  const m = Math.floor((s % 3600) / 60);
  const p = (x) => String(x).padStart(2, "0");
  return h ? `${h}:${p(m)}:${p(s % 60)}` : `${m}:${p(s % 60)}`;
};

const fmtDate = (secs) => (secs ? new Date(secs * 1000).toLocaleString(I18N.lang) : t("unknownTime"));

let toastTimer = 0;
const toast = (msg) => {
  const box = $("toast");
  box.textContent = msg;
  box.hidden = false;
  clearTimeout(toastTimer);
  toastTimer = setTimeout(() => (box.hidden = true), 5000);
};

const run = async (label, fn) => {
  try {
    await fn();
  } catch (e) {
    toast(`${label}: ${e}`);
  }
  await refresh();
};

// ---- language and theme ---------------------------------------------------

const pick = (value, allowed) => (allowed.includes(value) ? value : "system");

// Applies the stored preferences at once: language of every text, <html lang>,
// data-theme (absent = follow the OS). The DOM is updated in place, so open panels,
// open SourceBuffer tables, the pairing prompt and the tab list stay as they are.
const applyPrefs = () => {
  const root = document.documentElement;
  I18N.setLanguage(I18N.resolve(state.prefs.language, navigator.language));
  root.lang = I18N.lang;
  if (state.prefs.theme === "system") delete root.dataset.theme;
  else root.dataset.theme = state.prefs.theme;
  I18N.applyStatic(document);
  $("pref-language").value = state.prefs.language;
  $("pref-theme").value = state.prefs.theme;
  fillConfirm();
  state.signature = "";
  if (state.snap) render(state.snap);
};

const takePrefs = (settings) => {
  state.prefs = { language: pick(settings?.language, LANGUAGES), theme: pick(settings?.theme, THEMES) };
};

const onPrefChange = () => {
  state.prefs = { language: pick($("pref-language").value, LANGUAGES), theme: pick($("pref-theme").value, THEMES) };
  applyPrefs();
  const { language, theme } = state.prefs;
  invoke("save_ui_prefs", { language, theme })
    .then((s) => {
      if (state.settings) state.settings = { ...state.settings, language: s.language, theme: s.theme };
    })
    .catch((e) => toast(`${t("saveAppearance")}: ${e}`));
};

$("pref-language").addEventListener("change", onPrefChange);
$("pref-theme").addEventListener("change", onPrefChange);
// "System" follows a change of the webview's language without a restart.
window.addEventListener("languagechange", () => {
  if (state.prefs.language === "system") applyPrefs();
});

// ---- rendering ------------------------------------------------------------

const renderHeader = (snap) => {
  const s = snap.sink;
  const dot = $("sink-dot");
  dot.className = `dot dot-${s.state}`;
  let text = t("sinkStopped");
  if (s.state === "running") text = t("sinkRunning", { port: state.settings?.port ?? "" });
  if (s.state === "crashed") text = s.exit_code != null ? t("sinkCrashedCode", { code: s.exit_code }) : t("sinkCrashed");
  $("sink-text").textContent = text;
  const btn = $("sink-toggle");
  btn.textContent = s.state === "running" ? t("stop") : t("start");
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
    box.append(notice("error", t("noticeConfigTitle"), t("noticeConfigBody", { error: snap.config_error })));
  }
  if (!snap.sink_path) {
    box.append(notice("error", t("noSinkTitle"), t("noSinkBody")));
  }
  if (s.state === "crashed") {
    box.append(notice("error", t("crashedTitle"), t("crashedBody"), snap.log.slice(-8).join("\n")));
  }
  if (!snap.ffmpeg) {
    box.append(notice("warn", t("noFfmpegTitle"), t("noFfmpegBody")));
  }
  if (snap.captures_error) box.append(notice("error", t("capturesErrorTitle"), snap.captures_error));
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
      el("strong", { text: t("emptyStoppedTitle") }),
      el("span", { text: t("emptyStoppedBody", { start: t("start") }) }),
    );
    return;
  }
  box.append(
    el("strong", { text: t("emptyWaitingTitle") }),
    el("ol", {},
      el("li", { text: t("emptyStep1", { port }) }),
      el("li", { text: t("emptyStep2") }),
      el("li", { text: t("emptyStep3") }),
    ),
  );
};

const badge = (text, cls) => el("span", { class: `badge ${cls}`, text });

// The app sends a stable reason key with every failed mux; its English meaning is
// the fallback for a key this UI does not know.
const muxMeaning = (job) => {
  const key = `reason.${job.reason}`;
  return job.reason && key in I18N.tables.en ? t(key) : job.meaning;
};

const muxBox = (job) => {
  if (!job) return null;
  if (job.state === "running") return el("div", { class: "mux", role: "status", text: t("muxRunningText") });
  if (job.state === "ok") return el("div", { class: "mux", role: "status", text: t("muxOkText") });
  const meaning = muxMeaning(job);
  const text = job.code == null ? t("muxFailed", { meaning }) : t("muxFailedCode", { code: job.code, meaning });
  return el("div", { class: "mux failed", role: "alert" }, el("strong", { text }), job.stderr_tail ? el("pre", { text: job.stderr_tail }) : null);
};

const card = (c, job, detailsOpen) => {
  const title = c.title || t("untitledPage");
  const badges = el("div", { class: "badges" });
  badges.append(badge(c.live ? t("badgeLive") : t("badgeDone"), c.live ? "live" : "done"));
  if (c.drm) badges.append(badge(t("badgeDrmRefused"), "drm"));
  if (c.muxed) badges.append(badge(t("badgeMuxed"), "muxed"));
  const muxRunning = job?.state === "running";

  const sbTable = el("table", {},
    el("thead", {}, el("tr", {}, ...["#", t("colKind"), t("colType"), t("colSegments"), t("colSize")].map((h) => el("th", { text: h })))),
    el("tbody", {}, ...c.source_buffers.map((s) => el("tr", {},
      el("td", { text: String(s.sb) }), el("td", { text: s.kind || "-" }), el("td", { text: s.mime || "-" }),
      el("td", { text: String(s.segments) }), el("td", { text: fmtBytes(s.bytes) })))),
  );

  const btn = (label, fid, onclick, extra = {}) => el("button", { type: "button", "data-fid": `${c.id}:${fid}`, onclick, ...extra, text: label });
  const actions = el("div", { class: "actions" },
    btn(muxRunning ? t("muxing") : c.muxed ? t("muxAgain") : t("mux"), "mux", () => run(t("mux"), () => invoke("mux_capture", { id: c.id })),
      { disabled: muxRunning || c.live || !c.segments, title: c.live ? t("waitUntilDone") : !c.segments ? t("nothingToMux") : "" }),
    btn(t("openResult"), "open", () => run(t("openResult"), () => invoke("open_result", { id: c.id })), { disabled: !c.muxed }),
    btn(t("openFolder"), "folder", () => run(t("openFolder"), () => invoke("open_folder", { id: c.id }))),
    btn(t("delete"), "delete", () => askDelete(c), { class: "danger", disabled: muxRunning || c.live }),
  );

  const sbCount = c.source_buffers.length;
  return el("li", { class: "card", "data-id": c.id },
    el("h3", { text: title }),
    el("div", { class: "sub", text: `${c.host || t("unknownHost")} - ${fmtDate(c.started)}`, title: c.url }),
    badges,
    el("div", { class: "meta" },
      el("span", { text: t("duration", { d: c.duration_ms ? fmtDuration(c.duration_ms) : "-" }) }),
      el("span", { text: fmtBytes(c.bytes) }),
      el("span", { text: t(c.segments === 1 ? "segmentsOne" : "segmentsMany", { n: c.segments }) }),
      el("span", { text: t(sbCount === 1 ? "sourceBuffersOne" : "sourceBuffersMany", { n: sbCount }) }),
      c.muxed ? el("span", { text: t("result", { size: fmtBytes(c.result_bytes) }) }) : null,
    ),
    sbCount ? el("details", { open: detailsOpen }, el("summary", { text: t("sourceBuffers") }), sbTable) : null,
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

// Broad groups that may read config.json, by the stable id the app sends (SRV-14).
const GROUP_KEYS = {
  everyone: "groupEveryone",
  users: "groupUsers",
  "authenticated-users": "groupAuthenticatedUsers",
  interactive: "groupInteractive",
  local: "groupLocal",
  guests: "groupGuests",
  "domain-users": "groupDomainUsers",
  "all-app-packages": "groupAllAppPackages",
};

const renderConfigAccess = (snap) => {
  const box = $("config-access");
  const access = snap.config_access;
  if (!access) {
    box.replaceChildren();
    return;
  }
  if (access.state === "exposed") {
    const groups = (access.groups ?? []).map((g) => (GROUP_KEYS[g] ? t(GROUP_KEYS[g]) : String(g))).join(", ");
    box.replaceChildren(notice("warn", t("configAccessTitle"), t("configAccessBody", { groups })));
  } else {
    box.replaceChildren(notice("warn", t("configAccessUnknownTitle"), t("configAccessUnknownBody", { error: access.error ?? "" })));
  }
};

const renderSettings = (snap) => {
  renderConfigAccess(snap);
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
  $("token-show").textContent = state.tokenShown ? t("hide") : t("show");
  $("token-show").setAttribute("aria-pressed", String(state.tokenShown));
  $("fact-ffmpeg").textContent = snap.ffmpeg || t("notFoundOnPath");
  $("fact-sink").textContent = snap.sink_path || t("notFound");
};

const renderLog = (snap) => {
  const pre = $("log");
  const atEnd = pre.scrollTop + pre.clientHeight >= pre.scrollHeight - 8;
  pre.textContent = snap.log.length ? snap.log.join("\n") : t("noOutput");
  if (atEnd) pre.scrollTop = pre.scrollHeight;
};

// ---- browser link ---------------------------------------------------------

const BROWSER_NAMES = { chrome: "Chrome", edge: "Edge", brave: "Brave" };
const browserName = (id) => BROWSER_NAMES[id] ?? t("browserFallback");
const REG_KEYS = { registered: "regRegistered", "not-registered": "regNotRegistered", stale: "regStale" };
const profileLabel = (c) => t("browserProfile", { browser: browserName(c.browser), profile: c.profile });

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
        el("strong", { text: t("pairPrompt", { browser: browserName(c.browser), profile: c.profile }) }),
        el("span", { text: t("pairBody") }),
        el("div", { class: "row" },
          el("button", { type: "button", class: "primary", "data-fid": `pair:${c.id}:yes`, text: t("approve"), onclick: () => run(t("approve"), () => invoke("browser_decide", { conn: c.id, approve: true })) }),
          el("button", { type: "button", "data-fid": `pair:${c.id}:no`, text: t("deny"), onclick: () => run(t("deny"), () => invoke("browser_decide", { conn: c.id, approve: false })) }),
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
      const label = r.error ? t("regError", { error: r.error }) : t(REG_KEYS[r.state] ?? "regUnknown");
      return el("li", { class: "regrow" },
        el("strong", { text: browserName(r.browser) }),
        el("span", { class: registered ? "ok" : "muted", text: label }),
        el("span", { class: "grow" }),
        registered
          ? el("button", { type: "button", "data-fid": `reg:${r.browser}:rm`, text: t("remove"), onclick: () => run(t("remove"), () => invoke("browser_unregister", { browser: r.browser })) })
          : el("button", { type: "button", class: "primary", "data-fid": `reg:${r.browser}:add`, text: r.state === "stale" ? t("registerAgain") : t("register"), disabled: !b.host_path, title: b.host_path ? "" : t("hostMissing"), onclick: () => run(t("register"), () => invoke("browser_register", { browser: r.browser })) }),
      );
    }),
  );
  const note = $("browser-note");
  const notes = [];
  if (!b.host_path) notes.push(t("hostMissingNote"));
  if (b.bridge_error) notes.push(b.bridge_error);
  if (b.link.store_error) notes.push(b.link.store_error);
  note.hidden = !notes.length;
  note.textContent = notes.join(" ");

  const list = $("approvals");
  list.replaceChildren(
    ...(b.link.approvals.length
      ? b.link.approvals.map((a) => el("li", { class: "regrow" },
          el("strong", { text: profileLabel(a) }),
          el("span", { class: a.connected ? "ok" : "muted", text: a.connected ? t("connected") : t("notConnected") }),
          el("span", { class: "grow" }),
          el("button", { type: "button", class: "danger", "data-fid": `rev:${a.instance}`, text: t("revoke"), onclick: () => askRevoke(a) })))
      : [el("li", { class: "muted", text: t("noPaired") })]),
  );

  const running = snap.sink.state === "running";
  const approved = b.link.connections.filter((c) => c.approved);
  $("tabs-hint").textContent = approved.length
    ? running
      ? t("tabsHintCapture")
      : t("tabsHintStart")
    : t("tabsHintNone");
  const box = $("tabs");
  box.replaceChildren(
    ...approved.map((c) =>
      el("div", { class: "browser-tabs" },
        el("h4", { text: profileLabel(c) }),
        c.tabs.length
          ? el("ul", { class: "plain" }, ...c.tabs.map((tab) =>
              el("li", { class: "tabrow" },
                faviconImg(tab.favicon),
                el("span", { class: "tabtext" },
                  el("span", { class: "tabtitle", text: tab.title || tab.host || t("untitled") }),
                  el("span", { class: "muted", text: tab.active ? t("visibleTab", { host: tab.host }) : tab.host })),
                tab.drm ? badge(t("badgeDrm"), "drm") : null,
                tab.tapping ? badge(t("badgeCapturing"), "live") : null,
                el("button", {
                  type: "button",
                  class: tab.tapping ? "danger" : "primary",
                  "data-fid": `tab:${c.id}:${tab.id}`,
                  text: tab.tapping ? t("stop") : t("capture"),
                  disabled: (!tab.tapping && !running) || (tab.drm && !tab.tapping),
                  title: tab.drm ? t("drmTabTitle") : "",
                  onclick: () => run(t("capture"), () => invoke("browser_tap", { conn: c.id, tab: tab.id, on: !tab.tapping })),
                }))))
          : el("p", { class: "muted", text: t("noTabs") })),
    ),
  );
};

const render = (snap) => {
  const sig = JSON.stringify([snap, state.settings, state.tokenShown, state.busy, I18N.lang]);
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
    if (!state.settings) {
      state.settings = await invoke("get_settings");
      takePrefs(state.settings);
      applyPrefs();
    }
    state.snap = await invoke("snapshot");
    render(state.snap);
  } catch (e) {
    $("sink-text").textContent = t("appError", { error: e });
  }
};

// What the confirm dialog asks about: { heading, ok, text(), action() } (its texts
// are redone on a language change).
let pendingConfirm = null;

const fillConfirm = () => {
  if (!pendingConfirm) return;
  $("confirm-h").textContent = t(pendingConfirm.heading);
  $("confirm-ok").textContent = t(pendingConfirm.ok);
  $("confirm-text").textContent = pendingConfirm.text();
};

const ask = (question) => {
  pendingConfirm = question;
  fillConfirm();
  $("confirm").showModal();
  $("confirm-cancel").focus();
};

const askDelete = (c) =>
  ask({
    heading: "deleteCaptureQ",
    ok: "delete",
    text: () => t("confirmText", { title: c.title || c.id, size: fmtBytes(c.bytes), id: c.id }),
    action: () => run(t("delete"), () => invoke("delete_capture", { id: c.id })),
  });

// Revoking also gives the sink a new token (pushed to the other paired browsers)
// and restarts a running sink, which ends its captures in progress. The settings
// are read again afterwards, also when the revoke reports an error (the token may
// have changed before the sink failed to start again): Show and Copy must give the
// new token, which a browser set up by hand needs.
const askRevoke = (a) =>
  ask({
    heading: "revokeQ",
    ok: "revoke",
    text: () =>
      `${t("revokeText", { name: profileLabel(a) })}${state.snap?.sink.state === "running" ? ` ${t("revokeRunning")}` : ""}`,
    action: () =>
      run(t("revoke"), async () => {
        try {
          await invoke("browser_revoke", { instance: a.instance });
        } finally {
          state.settings = await invoke("get_settings");
        }
      }),
  });

$("confirm").addEventListener("close", () => {
  const question = pendingConfirm;
  pendingConfirm = null;
  if ($("confirm").returnValue === "ok" && question) question.action();
});

$("sink-toggle").addEventListener("click", async () => {
  state.busy = true;
  const running = state.snap?.sink.state === "running";
  await run(running ? t("stop") : t("start"), () => invoke(running ? "stop_sink" : "start_sink"));
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
  run(t("saveSettings"), async () => {
    state.settings = await invoke("save_settings", { port: Number($("port").value), capturesDir: $("dir").value });
    toast(t("settingsSaved"));
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
  toast((await copyText(state.settings.token)) ? t("tokenCopied") : t("copyFailed"));
});

$("token-new").addEventListener("click", () =>
  run(t("newToken"), async () => {
    state.settings = await invoke("regenerate_token");
    toast(t("newTokenCreated"));
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

// Until the stored preferences arrive, the page follows the webview's language.
applyPrefs();
tick();
setInterval(tick, POLL_MS);
