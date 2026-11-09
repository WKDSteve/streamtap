-- .testing.lua -- what the test sheets of this repository run against (testing.nvim, docs/SHEETS.md there). The sheets
-- in TESTS/sheets only name an app; how it starts is here, under testing.nvim's trust gate. streamtap has no Lua spec.

return {
  sheets = {
    -- the Chrome of the machine, like tools/page-check.mjs; playwright-core comes at a fixed version from that folder
    -- (pnpm --dir tools/page-check install --frozen-lockfile)
    browser = { playwright_dir = "tools/page-check", channel = "chrome" },
    apps = {
      -- The app's page with the stand-in for the Tauri bridge (tools/bridge-stub.mjs), as tools/page-check.mjs sees it:
      -- a copy of index.html in a folder of the run, no app, no sink, nothing of the person's settings.
      ["page"] = {
        run = { "node", "tools/sheet-page.mjs", "${temp}" },
        ready = { stdout = "^file:", as = "link" },
      },
    },
  },
}
