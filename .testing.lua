-- .testing.lua -- what the test sheets of this repository run against (testing.nvim, docs/SHEETS.md there). The sheets
-- in TESTS/sheets only name an app; how it starts is here, under testing.nvim's trust gate. streamtap has no Lua spec.

---The real app (its window, by the names a screen reader hears), against a folder of the run: STREAMTAP_APP_DIR holds
---its settings and the one capture `prepare` (tools/sheet-captures.mjs) writes there, WEBVIEW2_USER_DATA_FOLDER the
---page's storage. Nothing of the person's settings, captures or browser registration is read or written.
---@param prepare string[]
---@return table
local function captures(prepare)
  return {
    build = { "cargo", "build", "--manifest-path", "desktop/Cargo.toml", "-p", "streamtap-app", "--locked" },
    prepare = prepare,
    -- Explorer keeps the window of "Open folder" after the run removes the folder: closed here, and only that one
    cleanup = {
      "powershell",
      "-NoProfile",
      "-NonInteractive",
      "-File",
      "tools/sheet-close-folders.ps1",
      "-Under",
      "${temp}",
    },
    run = { "desktop/target/debug/streamtap-app.exe" },
    env = {
      STREAMTAP_APP_DIR = "${temp}/app",
      WEBVIEW2_USER_DATA_FOLDER = "${temp}/webview",
    },
    window = "streamtap",
  }
end

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
      -- A real streamtap-sink and a page with an MSE player of the repository's own fixture (tools/sheet-mse.mjs), for
      -- the sheets that load the extension (mse-capture, drm-refused). The sink listens on 17651 with the fixed token
      -- of the sheets; its captures go to the folder of the run.
      ["mse"] = {
        build = { "cargo", "build", "--manifest-path", "desktop/Cargo.toml", "-p", "streamtap-sink", "--locked" },
        run = { "node", "tools/sheet-mse.mjs", "${temp}" },
        ready = { stdout = "^http://127%.0%.0%.1:%d+/$", as = "link" },
      },
      -- The same sink and page, and the native messaging host of this build for the sheets that register it for one run
      -- (testing.nvim's native_host): host-no-app.
      ["mse-host"] = {
        build = {
          "cargo",
          "build",
          "--manifest-path",
          "desktop/Cargo.toml",
          "-p",
          "streamtap-sink",
          "-p",
          "streamtap-host",
          "--locked",
        },
        run = { "node", "tools/sheet-mse.mjs", "${temp}" },
        ready = { stdout = "^http://127%.0%.0%.1:%d+/$", as = "link" },
      },
      -- The app with a capture whose result is a few bytes (open-folder). It needs a sheet-driver of appkit from
      -- 2026-11-12 or later (a look into the window of another program).
      ["captures"] = captures({ "node", "tools/sheet-captures.mjs", "${temp}/app" }),
      -- The same with a real video of a minute as the result (ffmpeg on the PATH), for "Open result" (open-result): its
      -- player is closed by the sheet's `close` step, a sheet-driver of appkit from 2026-11-16 or later.
      ["captures-video"] = captures({ "node", "tools/sheet-captures.mjs", "${temp}/app", "--video" }),
    },
  },
}
