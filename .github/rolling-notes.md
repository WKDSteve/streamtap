The rolling build of the newest green commit of `main`, for looking at the state of the work. Not a release, unsigned, replaced after every green CI run.
For private machines only: not for the Tricentis workstation.

The file is `streamtap-windows-x86_64.zip` (a zip with install scripts, not a setup.exe); check it against `SHA256SUMS`, unpack it and run `install.ps1` from the unpacked folder:
`powershell -NoProfile -ExecutionPolicy Bypass -File .\install.ps1`. The extension is loaded unpacked from `%LOCALAPPDATA%\streamtap\extension`.
Needs **ffmpeg on the PATH** (`scoop install ffmpeg`). The version shown in *Settings > Apps* is `<version>+<short commit>`.
Details: the `README.md` inside the zip.
