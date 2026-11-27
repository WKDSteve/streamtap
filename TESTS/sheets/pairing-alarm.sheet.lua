-- The extension in Chromium with no native messaging host registered: every try to reach the desktop app fails at once
-- ("host not found"), the reconnect backs off 5, 10, 20 s, and the next wait (40 s) is longer than a service worker
-- stays alive without an event, so it must be a chrome.alarms alarm and no setTimeout (task
-- streamtap/settimeout-stellen-pruefen). The sheet waits for that alarm in the extension's own page; about 40 s.
-- Then the backoff must be in session storage, where a service worker started later reads it (task
-- streamtap/pairing-pause-ueberlebt-worker-ende).
-- Playwright's full Chromium, like mse-capture (CI job `extension-sheet`).
--   testing sheets run --sheet pairing-alarm
return {
  id = "streamtap.pairing-alarm",
  title = "streamtap: a reconnect wait of 30 s or more to the desktop app is an alarm, not a service worker timer",
  task = "streamtap/settimeout-stellen-pruefen",
  engine = "browser",
  app = "mse",
  extension = "extension",
  sizes = { { cols = 800, rows = 600 } },
  steps = {
    {
      open = "chrome-extension://oggpddakdbffbjhkjmcakidgmhpgakia/popup/popup.html",
      note = "the popup as a page: the extension is loaded, its service worker tries the host",
    },
    {
      wait = "chrome.alarms.getAll().then((a) => a.some((x) => x.name === 'streamtap-np-retry'))",
      timeout = 90000,
      note = "the backoff reached 40 s: the reconnect waits as an alarm",
    },
    {
      wait = "chrome.storage.session.get('npBackoff').then((v) => v.npBackoff >= 40000)",
      timeout = 10000,
      note = "the backoff is in session storage, so a service worker started after this one keeps it",
    },
  },
}
