// The stand-in for the Tauri bridge of the app's page, for the checks that open the page without the app: what the Rust
// side answers, with something in every part of the page. tools/page-check.mjs puts it in as an init script,
// tools/sheet-page.mjs into a copy of the page for the test sheets. It runs in the page, so it uses nothing from outside.

/** Sets `window.__TAURI__` before the page's scripts run. */
export function bridge() {
  const settings = { port: 17650, token: "a-token", captures_dir: "C:/captures", tailnet: "pc.example.ts.net", language: "en", theme: "system" };
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
  // The phones of the sink (the panel asks only while the sink runs): one paired phone, and a pairing code whose QR
  // code is a pattern, not a real code.
  const phones = [{ id: "0123456789abcdef0123456789abcdef", name: "A phone", paired: 1700000000, last_used: 1700000100, open: 1 }];
  const code = "00112233445566778899aabbccddeeff";
  const pairing = {
    tailnet: settings.tailnet,
    link: `https://${settings.tailnet}/#streamtap-pair=1&code=${code}`,
    code,
    ttl: 60,
    qr: Array.from({ length: 25 }, (_, y) => Array.from({ length: 25 }, (_, x) => ((x * 7 + y * 3) % 5 < 2 ? "1" : "0")).join("")),
  };
  const answers = { snapshot, get_settings: settings, phone_devices: phones, phone_pair: pairing };
  window.__TAURI__ = {
    core: {
      invoke: async (cmd) => answers[cmd] ?? null,
    },
  };
}
