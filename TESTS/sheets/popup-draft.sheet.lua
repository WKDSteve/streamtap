-- What is typed into the popup's settings and not saved survives closing the popup, in the real browser: the draft sits
-- in chrome.storage.session (the fake of extension/scripts/test-popup.mjs cannot say that the real area does what the
-- popup expects). The extension runs in Playwright's full Chromium (testing.nvim loads extension/ unpacked), the popup
-- is opened as a page, typed into, opened again: the port and the hosts stand there again, the token and the pairing
-- code never do (they are not even in the area), and a save takes the port out of the draft, not what is unsaved.
-- Like mse-capture it needs Playwright's full Chromium: CI runs it in the job `extension-sheet` on a hosted Linux VM.
--   testing sheets run --sheet popup-draft
local POPUP = "chrome-extension://oggpddakdbffbjhkjmcakidgmhpgakia/popup/popup.html"
local DRAFT = "chrome.storage.session.get({ popupDraft: {} }).then((v) => v.popupDraft)"
local FIELD = "document.getElementById"

-- A person typing: the value, then the event the popup listens to.
local function type_into(id, value)
  return FIELD
    .. "('"
    .. id
    .. "').value = '"
    .. value
    .. "'; "
    .. FIELD
    .. "('"
    .. id
    .. "').dispatchEvent(new Event('input', { bubbles: true }));"
end

-- (a sheet loads with an empty environment: no table.concat)
local TYPED = type_into("port", "18123")
TYPED = TYPED .. type_into("own-hosts", "media.example.org")
TYPED = TYPED .. type_into("token", "secret-token-of-the-sheet")
TYPED = TYPED .. type_into("tailnet-code", "code-of-the-sheet")

return {
  id = "streamtap.popup-draft",
  feature = "streamtap.popup-draft",
  title = "streamtap: a popup's typed settings survive closing it, a secret never, a save ends the draft",
  task = "streamtap/sheet-popup-entwurf-nach-dem-schliessen-in-chromium",
  engine = "browser",
  app = "mse",
  extension = "extension",
  sizes = { { cols = 800, rows = 600 } },
  steps = {
    { open = POPUP, note = "the popup as a page: the extension is loaded" },
    {
      wait = FIELD .. "('port').value === '17650'",
      timeout = 5000,
      note = "the popup read the stored port (the default) and drew its settings",
    },
    {
      expect = "(() => { " .. TYPED .. " return true; })()",
      note = "typed: port, own servers, and the two secrets (token, pairing code)",
    },
    {
      wait = DRAFT .. ".then((d) => d.port === '18123' && d['own-hosts'] === 'media.example.org')",
      timeout = 5000,
      note = "the draft holds the port and the hosts",
    },
    {
      expect = "chrome.storage.session.get(null).then((all) => { const s = JSON.stringify(all);"
        .. " return !s.includes('secret-token-of-the-sheet') && !s.includes('code-of-the-sheet'); })",
      note = "and nothing of the token or the pairing code, in no key of the area",
    },
    { open = POPUP, note = "the popup closed and opened again" },
    {
      wait = FIELD .. "('port').value === '18123' && " .. FIELD .. "('own-hosts').value === 'media.example.org'",
      timeout = 5000,
      note = "the port and the hosts are back in their fields, the stored port (17650) did not win",
    },
    {
      expect = FIELD .. "('token').value === '' && " .. FIELD .. "('tailnet-code').value === ''",
      note = "the secrets are not",
    },
    {
      expect = "(() => { " .. FIELD .. "('save').click(); return true; })()",
      note = "save the port",
    },
    {
      wait = DRAFT
        .. ".then((d) => !Object.hasOwn(d, 'port') && d['own-hosts'] === 'media.example.org')"
        .. ".then((ok) => ok && chrome.storage.local.get({ sinkPort: 0 }).then((v) => v.sinkPort === 18123))",
      timeout = 5000,
      note = "the port is stored and out of the draft; the hosts, not saved, are still in it",
    },
  },
}
