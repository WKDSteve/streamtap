# streamtap: install from this zip

This is the Windows release of streamtap. It is a zip with install scripts, not a setup.exe. It is
not signed (see "SmartScreen" below), and it is meant for private machines, not for a managed
workstation.

## What you need

- Windows 10 or 11 (64 bit) with the WebView2 runtime (part of Windows 11 and of current Windows 10).
- **ffmpeg on the PATH**, for muxing the captures. It is not in this zip. Install it with
  `scoop install ffmpeg`, or put `ffmpeg.exe` (and `ffprobe.exe`) in a folder on your PATH, and do it
  before you start the app: the app looks on the PATH it was started with, so if you install ffmpeg
  later, close the app and start it again. Until then it shows "ffmpeg not found".
- Chrome, Edge or Brave for the extension.

## Check and unblock the download

Do this in the folder you downloaded the zip to, **before you unpack it**. Compare the hash with the
line for this zip in `SHA256SUMS` of the same release:

```powershell
(Get-FileHash .\streamtap-*.zip -Algorithm SHA256).Hash.ToLower()
Unblock-File .\streamtap-*.zip      # removes the Mark-of-the-Web; what you unpack from it carries none
```

## Install

Unpack the zip into a **new, empty folder** (the installer stops when it finds files in it that a
release does not have). If you unpacked it before unblocking it, run `Get-ChildItem -Recurse |
Unblock-File` in the unpacked folder now: the installer copies the marks along to the installed
programs, and Windows would stop their first start (see "SmartScreen"). Then, in a normal (not
elevated) PowerShell in that folder:

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
It also refuses a folder (`-InstallDir`) that holds files of somebody else's and no streamtap install.

## SmartScreen

Nothing here is code-signed. A zip you downloaded in a browser carries a Mark-of-the-Web, and
so does everything you unpack from it with Explorer. `-ExecutionPolicy Bypass` runs the scripts
regardless, but the first start of `streamtap-app.exe` can be stopped ("Windows protected your PC":
*More info*, then *Run anyway*) if the mark is still on the file. That is why the zip is unblocked
before it is unpacked, above. The checksum tells you the zip is the release's; it is no signature.

## Load the extension and capture (once per browser)

Chrome does not install extensions from outside its store, so you load it unpacked:

1. Open `chrome://extensions` (Edge: `edge://extensions`).
2. Turn on *Developer mode*.
3. *Load unpacked*, and pick `%LOCALAPPDATA%\streamtap\extension`.
4. Open *streamtap* from the Start menu, press *Connect browser*, then **Register** for your
   browser (Chrome, Edge or Brave; once per browser), and press **Start** in the header to start the
   sink. A capture needs a running sink.
5. The app shows "<browser> wants to pair": **Approve** it. If nothing is asked within a few seconds
   (the extension tried before you registered and waits a while before it tries again), press
   *Reload* on the extension's card at `chrome://extensions`, or *pair now* in the extension's popup.
6. Open a page with a video. The app lists the tab under *Tabs with video*: press **Capture** (the tab is
   reloaded once), then play the video. The capture appears in the list; press **Mux** when it is done, then open the
   result.

After an update, press *Reload* on the extension's card at `chrome://extensions`. Installing a new
version does not remove the extension from your browser.

## Uninstall

*Settings > Apps > streamtap*, or:

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -File "$env:LOCALAPPDATA\streamtap\uninstall.ps1"
powershell -NoProfile -ExecutionPolicy Bypass -File "$env:LOCALAPPDATA\streamtap\uninstall.ps1" -RemoveData   # also settings, token, pairings
```

Captures are never removed. The extension stays in your browser as a card that points at a folder
that is gone: remove it at `chrome://extensions` (*Remove*).
