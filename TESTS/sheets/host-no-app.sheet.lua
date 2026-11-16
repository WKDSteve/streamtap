-- The extension in Chromium with the native messaging host of this build, registered for this run only (testing.nvim's
-- `native_host`: HKCU\Software\Chromium on Windows, never the key of the installed app): "pair now" in the popup makes
-- Chromium start streamtap-host.exe, which finds no app (STREAMTAP_APP_DIR is an empty folder of the run, so it never
-- reaches an app the person runs) and the popup says so. No counter-check without the host: Chromium would then start
-- the installed app's host (Software\Google\Chrome), outside any folder of the run.
-- Windows only so far (the host's path); Playwright's full Chromium; not part of the CI.
--   testing sheets run --sheet host-no-app
return {
  id = "streamtap.host-no-app",
  title = "streamtap: the host of this build, started by Chromium, says that the app is not running",
  task = "streamtap/live-check-browser-koppeln-nach-dem-bridge-handshake",
  engine = "browser",
  app = "mse-host",
  extension = "extension",
  native_host = {
    name = "io.github.wkdsteve.streamtap",
    path = "desktop/target/debug/streamtap-host.exe",
    env = { STREAMTAP_APP_DIR = "${temp}/no-app" },
  },
  sizes = { { cols = 800, rows = 600 } },
  steps = {
    {
      open = "chrome-extension://oggpddakdbffbjhkjmcakidgmhpgakia/popup/popup.html",
      note = "the popup",
    },
    {
      expect = "new Promise((r) => chrome.runtime.sendMessage({ kind: 'vc-pair-now' }, () => r(true)))",
      note = "pair now, as the popup's button does: Chromium starts the host",
    },
    {
      wait = "new Promise((r) => chrome.runtime.sendMessage({ kind: 'vc-pair-status' },"
        .. " (s) => r(!!s && s.error === 'app-not-running')))",
      timeout = 15000,
      note = "the host ran and found no app (without a host it would be host-unavailable)",
    },
  },
}
