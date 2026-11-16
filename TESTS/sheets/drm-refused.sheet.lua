-- The extension in Chromium (testing.nvim loads extension/ unpacked) against a real sink, with a page that sets
-- MediaKeys before it appends (app "mse", tools/sheet-mse.mjs, page /drm): the hook must stop copying that frame, the
-- service worker marks the tab as DRM (red badge, the popup says it is not captured), and the sink gets nothing of it.
-- The key system is ClearKey, the one every browser has built in, and the fixture is not encrypted: what the extension
-- refuses is the use of EME, never a real licence or CDM. The control at the end, the plain fixture in another tab,
-- shows that the sink was reachable all along. Like mse-capture it needs Playwright's full Chromium: CI runs it in the
-- job `extension-sheet` on a hosted Linux VM.
--   testing sheets run --sheet drm-refused
-- The sheet's own page is the extension's popup, opened as a tab (an extension page can open tabs and ask the service
-- worker); the fixture's address rides along in its query, because only an `open` step fills in ${link}.
local POPUP = "chrome-extension://oggpddakdbffbjhkjmcakidgmhpgakia/popup/popup.html?fixture=${link}"
local FIXTURE = "new URLSearchParams(location.search).get('fixture')"
local DRM_TAB = "Number(sessionStorage.getItem('drmTab'))"
local INDEX = "fetch("
  .. FIXTURE
  .. " + 'index').then((r) => r.text()).then((t) => t.split(String.fromCharCode(10))"
  .. ".filter(Boolean).map((l) => JSON.parse(l)))"

return {
  id = "streamtap.drm-refused",
  feature = "streamtap.drm",
  title = "streamtap: a page with EME is refused: red badge, DRM in the popup, nothing of it reaches the sink",
  task = "streamtap/drm-badge-mit-echter-drm-quelle-im-browser-pruefen",
  engine = "browser",
  app = "mse",
  extension = "extension",
  sizes = { { cols = 800, rows = 600 } },
  steps = {
    { open = POPUP, note = "the popup as a page: the extension is loaded" },
    {
      expect = "chrome.storage.local.set({ sinkPort: 17651, sinkToken: 'sheet-mse-capture-token', sinkPaired: false,"
        .. " tapMode: 'all' }).then(() => true)",
      note = "port and token of the run's sink, as the popup's save stores them",
    },
    {
      expect = "chrome.tabs.create({ url: "
        .. FIXTURE
        .. " + 'drm' })"
        .. ".then((t) => { sessionStorage.setItem('drmTab', String(t.id)); return true; })",
      note = "the page with EME in a tab of its own, the active one",
    },
    {
      wait = "fetch(" .. FIXTURE .. " + 'drm-state').then((r) => r.text()).then((s) => s === 'appended')",
      timeout = 15000,
      note = "it set MediaKeys of ClearKey, then appended the fixture",
    },
    {
      wait = "chrome.runtime.sendMessage({ kind: 'vc-get', tabId: "
        .. DRM_TAB
        .. " })"
        .. ".then((s) => !!s && s.drm.some((d) => d.keySystem === 'setMediaKeys'))",
      timeout = 10000,
      note = "the service worker knows that the tab set MediaKeys",
    },
    {
      expect = "chrome.action.getBadgeBackgroundColor({ tabId: "
        .. DRM_TAB
        .. " })"
        .. ".then((c) => c[0] === 176 && c[1] === 0 && c[2] === 32)",
      note = "its badge is red",
    },
    { open = POPUP, note = "the popup again: it shows the active tab, the one with EME" },
    {
      wait = "document.getElementById('tab-line').textContent.includes('DRM')"
        .. " && [...document.querySelectorAll('.drm')].some((e) => e.textContent.startsWith('DRM (not captured)'))",
      timeout = 5000,
      note = "it says the tab uses DRM and is not captured",
    },
    {
      expect = "chrome.tabs.create({ url: " .. FIXTURE .. " }).then(() => true)",
      note = "the control: the same fixture without EME in another tab",
    },
    {
      wait = INDEX .. ".then((rows) => rows.some((row) => !row.is_init))",
      timeout = 30000,
      note = "the sink wrote the control's media: it was reachable",
    },
    {
      expect = INDEX .. ".then((rows) => rows.filter((row) => row.is_init).length === 1)",
      note = "and only the control: one init, nothing of the tab with EME",
    },
  },
}
