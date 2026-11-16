-- The hand check of what the desktop sheets open-folder, open-result and open-procs cannot see: the console of the
-- browser's native host. That the default program opens out.mp4 is open-result, that the app starts no shell for
-- either button is open-procs. In the installed app on the private machine; app-proc-watch is tasks-vault
-- TOOLS/app-proc-watch.md.
--   testing sheets step open-result-live
return {
  id = "streamtap.open-result-live",
  feature = "streamtap.captures-list",
  title = "streamtap: the native host of the browser starts without a visible console window",
  task = "streamtap/handlauf-open-result-und-open-folder-oeffnen-ueber-die-shell",
  engine = "manual",
  steps = {
    {
      ask = "The extension in a browser with the registered host: app-proc-watch in attach mode on the browser shows"
        .. " cmd.exe /d /c streamtap-host.exe without a visible console window (result noted in the task)?",
    },
  },
}
