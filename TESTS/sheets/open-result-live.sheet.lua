-- The hand check of what the sheet open-folder cannot see: a real recording opened in its program, the processes the
-- app starts for it (none: the shell does it), and the console of the browser's native host. In the installed app on
-- the private machine; app-proc-watch is tasks-vault TOOLS/app-proc-watch.md.
--   testing sheets step open-result-live
return {
  id = "streamtap.open-result-live",
  feature = "streamtap.captures-list",
  title = "streamtap: Open result plays a real recording, and opening starts no explorer, cmd or powershell of the app",
  task = "streamtap/handlauf-open-result-und-open-folder-oeffnen-ueber-die-shell",
  engine = "manual",
  steps = {
    { ask = "In the installed app, a recording with a result: Open result opens out.mp4 in its default program?" },
    {
      ask = "app-proc-watch in attach mode on the app while Open result and Open folder are clicked: no explorer.exe,"
        .. " cmd or powershell is a child of the app?",
    },
    {
      ask = "The extension in a browser with the registered host: app-proc-watch in attach mode on the browser shows"
        .. " cmd.exe /d /c streamtap-host.exe without a visible console window (result noted in the task)?",
    },
  },
}
