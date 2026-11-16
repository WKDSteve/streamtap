# Programs streamtap starts

Every program streamtap starts, with why, how long it lives and how it is started. **A process
that is not in this list is a bug.** The list is taken from the code (`sink::command` and
`jobs::KillOnClose` of `streamtap-app`, `proc.rs` of `streamtap-sink`; `tools/process-starts.allow` names
these files, and `tools/check-process-starts.py` in the CI refuses a start by hand anywhere else) and checked against a
measurement with `app-proc-watch`.

streamtap is not built on appkit, so it does not use `appkit_core::child`; its two helpers do the
same for its children (the rule `RS-08` of the native ruleset names the sink, `ffmpeg`/`ffprobe`
and the native messaging host as exceptions of the plan "no visible shell and background
processes"):

- **child of the window**: `sink::command` starts with `CREATE_NO_WINDOW` and pipes, and
  `jobs::KillOnClose` puts the child into a job object with `KILL_ON_JOB_CLOSE` that only the
  window holds: the child ends with the window, also when the window crashes or is killed,
  together with everything it started. If the job cannot be made, the log of the window says
  so ("the sink is not tied to the app").
- **child of the sink**: `proc.rs` of `streamtap-sink` starts `ffmpeg` and `ffprobe` with
  `CREATE_NO_WINDOW`, an argv (no shell) and a time limit. Under the window they are in the
  window's job as well.

| Program | Started by | Why | How long | Way |
| --- | --- | --- | --- | --- |
| `streamtap-sink serve --port <n> --root <captures> …` | the window | **Start** of the sink: it takes the captures of the browser extension; the token goes over stdin, never on the command line | until **Stop**, or the window ends | child of the window |
| `streamtap-sink mux <capture>` | the window | **Mux** of a capture into one `.mp4` | until it is done, cancelled, or the window ends | child of the window |
| `ffmpeg -hide_banner -version` | the sink | which `ffmpeg` is there | the call, with a time limit | child of the sink |
| `ffprobe …` | the sink | the streams and start time of a captured file | the call, with a time limit | child of the sink |
| `ffmpeg …` (mux) | the sink | joins the captured tracks | the call, with the time limit of the mux | child of the sink |
| the program of a file, the file manager | the window | **Open** a capture, **Show in folder** | yours | the system: Windows asks the shell (`ShellExecuteExW`, `SHOpenFolderAndSelectItems`), no `explorer.exe`; `open`/`xdg-open` elsewhere |
| `streamtap-host` | **the browser**, not streamtap | the native messaging host of the extension | as long as the browser keeps the port open | started by the browser from its registration; it starts nothing |
| `msedgewebview2.exe` (several) | the WebView2 runtime | draws the window | as long as the window (another window of streamtap may share them) | not started by the app's code |
| `conhost.exe` | Windows | every console program (`streamtap-sink`, `ffmpeg`, `ffprobe`) gets a console host, also without a window | as long as its console program | comes with each child; no window |

The window starts no shell, no `cmd.exe`, no `icacls.exe` and no `taskkill.exe`; **Stop** ends
the sink itself (`Child::kill`, no program), and its job ends what the sink started.
