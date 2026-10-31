# streamtap: install from this zip

This is the Windows release of streamtap. It is a zip with install scripts, not a setup.exe. It is
not signed (see "SmartScreen" below), and it is meant for private machines, not for a managed
workstation.

## What you need

- Windows 10 or 11 (64 bit) with the WebView2 runtime (part of Windows 11 and of current Windows 10).
- **ffmpeg on the PATH**, for muxing the captures. It is not in this zip. Install it with
  `scoop install ffmpeg`, or put `ffmpeg.exe` (and `ffprobe.exe`) in a folder on your PATH. The app
  shows "ffmpeg not found" until it can start it.
- Chrome, Edge or Brave for the extension.

## Install

In a normal (not elevated) PowerShell, from the folder you unpacked this zip into:

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -File .\install.ps1
```

It installs for the current user, without administrator rights:

| What | Where |
|---|---|
| `streamtap-app.exe`, `streamtap-sink.exe`, `streamtap-host.exe` | `%LOCALAPPDATA%\streamtap\bin` |
| the browser extension | `%LOCALAPPDATA%\streamtap\extension` |
| `uninstall.ps1`, `LICENSE`, `README.md`, `VERSION` | `%LOCALAPPDATA%\streamtap` |
| Start menu entry *streamtap*, entry in *Settings > Apps* | your Start menu, your registry |

Your settings, token, pairings and captures (`%APPDATA%\io.github.wkdsteve.streamtap`) are never
touched. Installing again replaces the files (close the app first; it refuses while a program runs).

## SmartScreen

Nothing here is code-signed. A zip you downloaded in a browser carries a Mark-of-the-Web, and
Windows may ask before it runs the scripts or the first start of `streamtap-app.exe` ("Windows
protected your PC": *More info*, then *Run anyway*). Check the download against `SHA256SUMS` of the
release first:

```powershell
(Get-FileHash .\streamtap-*.zip -Algorithm SHA256).Hash.ToLower()   # compare with the line in SHA256SUMS
```

To unblock the unpacked scripts: `Get-ChildItem -Recurse | Unblock-File` in the unpacked folder.

## Load the extension (once per browser)

Chrome does not install extensions from outside its store, so you load it unpacked:

1. Open `chrome://extensions` (Edge: `edge://extensions`).
2. Turn on *Developer mode*.
3. *Load unpacked*, and pick `%LOCALAPPDATA%\streamtap\extension`.
4. Open *streamtap* from the Start menu, *Connect browser*, and approve the extension there.

After an update, press *Reload* on the extension's card at `chrome://extensions`.

## Uninstall

*Settings > Apps > streamtap*, or:

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -File "$env:LOCALAPPDATA\streamtap\uninstall.ps1"
powershell -NoProfile -ExecutionPolicy Bypass -File "$env:LOCALAPPDATA\streamtap\uninstall.ps1" -RemoveData   # also settings, token, pairings
```

Captures are never removed.
