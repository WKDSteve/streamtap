-- "Open result" of a finished capture, in the real app window (by the names a screen reader hears): the app asks the
-- shell to open out.mp4, the default program of the machine opens it, and the sheet closes that program's window again
-- (sheet-driver's `close`: UI Automation, no kill). The app `captures-video` of .testing.lua runs against a folder of
-- the run with one capture whose result is a real video of a minute (tools/sheet-captures.mjs --video, ffmpeg): a
-- player names the file in its title only while it plays. Which program opens it is the machine's choice; the sheet
-- sees that a window with "out.mp4" in its title comes. Whether the app starts explorer, cmd or powershell for it, and
-- a real recording, stay in the manual sheet open-result-live.
-- It needs sheet-driver of appkit ($TESTING_DESKTOP_DRIVER), else it is a skip; not part of the CI (no desktop there).
--   testing sheets run --sheet open-result
return {
  id = "streamtap.open-result",
  feature = "streamtap.captures-list",
  title = "streamtap: Open result opens out.mp4 in the default program, and the sheet closes it again",
  task = "streamtap/handlauf-open-result-und-open-folder-oeffnen-ueber-die-shell",
  engine = "desktop",
  app = "captures-video",
  sizes = { { cols = 1000, rows = 700 } },
  steps = {
    { wait = "Sheet capture", timeout = 20000, note = "the capture of the run is listed" },
    { click = "Open result", note = "open its result" },
    {
      close = "out.mp4",
      timeout = 20000,
      note = "a window of the default program shows out.mp4 in its title; closed as its close button does",
    },
  },
}
