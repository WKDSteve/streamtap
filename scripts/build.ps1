<#
.SYNOPSIS
Builds desktop\target\release\streamtap-sink.exe (release, locked).

.DESCRIPTION
Used by the nvim-config's :MyNative build and :MyPlugins machine. Two things a
bare `cargo build` gets wrong for them:
- A running sink (`streamtap-sink serve`) locks the exe; cargo then fails with
  "Access is denied (os error 5)". The script names the process instead.
- A no-op build leaves the exe's timestamp alone, so a build that is current
  still looks older than the newest commit and stays "stale". After a
  successful build the exe gets the current time.
Nothing is installed (spike stage); the extension is loaded unpacked.

.EXAMPLE
.\scripts\build.ps1
#>
[CmdletBinding()]
param()

$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
$manifest = Join-Path $root 'desktop\Cargo.toml'
$exe = Join-Path $root 'desktop\target\release\streamtap-sink.exe'

$running = Get-Process -Name 'streamtap-sink' -ErrorAction SilentlyContinue |
    Where-Object { $_.Path -and ($_.Path -eq $exe) }
if ($running) {
    $ids = ($running | ForEach-Object { $_.Id }) -join ', '
    Write-Error "streamtap-sink is running from this checkout (PID $ids); stop it before building"
    exit 1
}

& cargo build --release --locked --manifest-path $manifest
if ($LASTEXITCODE -ne 0) {
    Write-Error "cargo build failed (exit code $LASTEXITCODE)"
    exit 1
}
if (-not (Test-Path -LiteralPath $exe)) {
    Write-Error "build finished but $exe is missing"
    exit 1
}
(Get-Item -LiteralPath $exe).LastWriteTime = Get-Date
Write-Output "built $exe"
