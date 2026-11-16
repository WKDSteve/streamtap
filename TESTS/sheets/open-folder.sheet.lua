-- "Open folder" of a finished capture, in the real app window (by the names a screen reader hears): the app asks the
-- shell to show the capture's folder, and Explorer opens that folder itself (its window is titled by the folder and
-- lists what is in it), not only its parent with the folder selected. The app `captures` of .testing.lua runs against
-- a folder of the run with one capture (tools/sheet-captures.mjs); the Explorer window goes away when the run removes
-- that folder. What a person still looks at: that no explorer.exe, cmd or powershell is a child of the app
-- (app-proc-watch), and "Open result" with a real recording (manual sheet open-result-live).
-- It needs sheet-driver of appkit ($TESTING_DESKTOP_DRIVER), else it is a skip; not part of the CI (no desktop there).
--   testing sheets run --sheet open-folder
return {
  id = "streamtap.open-folder",
  feature = "streamtap.captures-list",
  title = "streamtap: Open folder shows the capture's own folder in Explorer",
  task = "streamtap/handlauf-open-result-und-open-folder-oeffnen-ueber-die-shell",
  engine = "desktop",
  app = "captures",
  sizes = { { cols = 1000, rows = 700 } },
  steps = {
    { wait = "Sheet capture", timeout = 20000, note = "the capture of the run is listed" },
    { click = "Open folder", note = "open its folder" },
    {
      wait = { name = "segments", window = "1730000000_sheet" },
      timeout = 20000,
      note = "an Explorer window shows the capture's folder itself: titled by it, with its contents",
    },
  },
}
