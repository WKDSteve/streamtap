streamtap {tag} for Windows. Unsigned, for private machines only: not for the Tricentis workstation.

**This is a zip with install scripts, not a setup.exe.**

1. Download `streamtap-<version>-x86_64-pc-windows-msvc.zip` (the repository is private, so use `gh release download {tag}` or the page while signed in).
   Before you unpack it, check it against `SHA256SUMS` (`(Get-FileHash <zip> -Algorithm SHA256).Hash.ToLower()` must be the hash in that file)
   and run `Unblock-File <zip>`, so the installed programs carry no Mark-of-the-Web.
2. Unpack it into a new, empty folder, open a normal (not elevated) PowerShell in it and run
   `powershell -NoProfile -ExecutionPolicy Bypass -File .\install.ps1`.
3. Load the extension: `chrome://extensions`, Developer mode, Load unpacked, `%LOCALAPPDATA%\streamtap\extension`.
4. Open *streamtap* from the Start menu, *Connect browser*, **Register** your browser, **Start** the sink and **Approve** the pairing
   (no prompt after a few seconds: *Reload* the extension's card or press *pair now* in its popup). Then **Capture** a tab, **Mux**, open the result.

Needs **ffmpeg on the PATH** (not in the zip): `scoop install ffmpeg`, before you start the app (restart the app if you install it later).

SmartScreen may stop the first start of an unsigned program ("More info", then "Run anyway").
Update: close the app, run the new zip's `install.ps1`, *Reload* the extension. Uninstall: *Settings > Apps > streamtap*, then remove the extension's card in the browser.
Details: the `README.md` inside the zip.
