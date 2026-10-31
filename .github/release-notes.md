streamtap {tag} for Windows. Unsigned, for private machines only: not for the Tricentis workstation.

**This is a zip with install scripts, not a setup.exe.**

1. Download `streamtap-<version>-x86_64-pc-windows-msvc.zip` and check it against `SHA256SUMS`
   (`(Get-FileHash <zip>).Hash` must be the hash in that file; the repository is private, so use `gh release download {tag}` or the page while signed in).
2. Unpack it, open a normal (not elevated) PowerShell in the unpacked folder and run
   `powershell -NoProfile -ExecutionPolicy Bypass -File .\install.ps1`.
3. Load the extension: `chrome://extensions`, Developer mode, Load unpacked, `%LOCALAPPDATA%\streamtap\extension`.
4. Open *streamtap* from the Start menu and use *Connect browser*.

Needs **ffmpeg on the PATH** (not in the zip): `scoop install ffmpeg`.

SmartScreen may stop the first start of an unsigned program ("More info", then "Run anyway").
Uninstall: *Settings > Apps > streamtap*. Details: the `README.md` inside the zip.
