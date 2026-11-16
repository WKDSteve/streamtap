-- "Open result" and "Open folder" start nothing of the app's own: the shell opens the player and Explorer, never a
-- cmd, powershell or explorer.exe as a child of the app, and no console window appears. app-proc-watch watches the
-- app from its start (testing.nvim's `procs` step, attach mode); the player and the Explorer window are no
-- descendants of the app and do not count. The app `captures-video` of .testing.lua, as in open-result.
-- It needs sheet-driver of appkit and app-proc-watch ($TESTING_PROC_WATCH, the path of app-proc-watch.ps1 in
-- tasks-vault TOOLS/scripts/app-proc-watch), else it is a skip; never on the workstation, not part of the CI.
--   testing sheets run --sheet open-procs
return {
  id = "streamtap.open-procs",
  feature = "streamtap.captures-list",
  title = "streamtap: opening a result or a folder starts no shell and no console window of the app",
  task = "streamtap/handlauf-open-result-und-open-folder-oeffnen-ueber-die-shell",
  engine = "desktop",
  app = "captures-video",
  sizes = { { cols = 1000, rows = 700 } },
  steps = {
    { wait = "Sheet capture", timeout = 20000, note = "the capture of the run is listed" },
    { procs = "clean", note = "starting and listing: no shell, no console window" },
    { click = "Open result", note = "open its result" },
    { close = "out.mp4", timeout = 20000, note = "the default program showed it; closed again" },
    { click = "Open folder", note = "open its folder" },
    {
      wait = { name = "segments", window = "1730000000_sheet" },
      timeout = 20000,
      note = "Explorer shows the capture's folder",
    },
    { procs = "clean", note = "neither opening started a shell or a console window of the app" },
  },
}
