-- The extension itself in Chromium (testing.nvim loads extension/ unpacked) against a real sink and the repository's
-- own MSE fixture (app "mse", tools/sheet-mse.mjs): the hook taps the append, the service worker sends it, the sink
-- writes a capture whose media rows carry the track id the hook read from `tfhd`. Never a page of somebody else, never
-- DRM. It needs Playwright's full Chromium (branded Chrome refuses an unpacked extension): CI runs it in the job
-- `extension-sheet` on a hosted Linux VM; the self-hosted job leaves it out.
--   testing sheets run --sheet mse-capture
return {
  id = "streamtap.mse-capture",
  title = "streamtap: the extension taps an MSE append and the sink writes it with its track",
  task = "streamtap/track-id-live-gegen-echten-browser-mitschnitt-pruefen",
  engine = "browser",
  app = "mse",
  extension = "extension",
  sizes = { { cols = 800, rows = 600 } },
  steps = {
    {
      open = "chrome-extension://oggpddakdbffbjhkjmcakidgmhpgakia/popup/popup.html",
      note = "the popup: the extension is loaded",
    },
    {
      expect = "chrome.storage.local.set({ sinkPort: 17651, sinkToken: 'sheet-mse-capture-token', sinkPaired: false, tapMode: 'all' }).then(() => true)",
      note = "port and token of the run's sink, as the popup's save stores them",
    },
    { open = "${link}", note = "the fixture page with the MSE player" },
    {
      wait = "document.getElementById('state').textContent === 'appended'",
      timeout = 15000,
      note = "the player appended the fixture",
    },
    {
      wait = "fetch('/index').then((r) => r.text()).then((t) => t.split(String.fromCharCode(10)).filter(Boolean).map((l) => JSON.parse(l)).some((row) => !row.is_init && Number.isInteger(row.track)))",
      timeout = 30000,
      note = "the sink wrote a media row with the track id",
    },
    {
      expect = "fetch('/index').then((r) => r.text()).then((t) => t.split(String.fromCharCode(10)).filter(Boolean).map((l) => JSON.parse(l)).some((row) => row.is_init && row.kind === 'mp4'))",
      note = "and the init segment before it",
    },
  },
}
