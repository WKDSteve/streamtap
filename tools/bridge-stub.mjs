// The stand-in for the Tauri bridge of the app's page, for the checks that open the page without the app: what the Rust
// side answers, with something in every part of the page. tools/page-check.mjs puts it in as an init script,
// tools/sheet-page.mjs into a copy of the page for the test sheets. It runs in the page, so it uses nothing from outside.

/** Sets `window.__TAURI__` before the page's scripts run. */
export function bridge() {
  const settings = { port: 17650, token: "a-token", captures_dir: "C:/captures", language: "en", theme: "system" };
  const snapshot = {
    sink: { state: "stopped" },
    sink_path: "C:/sink.exe",
    ffmpeg: "C:/ffmpeg.exe",
    config_error: "config.json could not be read",
    captures: [
      {
        id: "c1",
        title: "A capture",
        url: "https://example.invalid/",
        host: "example.invalid",
        started: 1700000000,
        live: false,
        muxed: false,
        drm: false,
        bytes: 1024,
        segments: 3,
        duration_ms: 4000,
        source_buffers: [],
      },
    ],
    captures_error: null,
    mux: {},
    log: ["a line of the log"],
    browser: {
      host_path: "C:/host.exe",
      bridge_error: null,
      registrations: [{ browser: "chrome", registered: true, path: "C:/manifest.json" }],
      link: { connections: [], approvals: [{ instance: "p-1", browser: "chrome", profile: "Default", connected: true }], store_error: null },
    },
  };
  window.__TAURI__ = {
    core: {
      invoke: async (cmd) => (cmd === "snapshot" ? snapshot : cmd === "get_settings" ? settings : null),
    },
  };
}
