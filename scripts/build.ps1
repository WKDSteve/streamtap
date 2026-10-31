<#
.SYNOPSIS
Builds desktop\target\release\streamtap-sink.exe, streamtap-host.exe and streamtap-app.exe (release, locked).

.DESCRIPTION
Used by the nvim-config's :MyNative build and :MyPlugins machine. Two things a
bare `cargo build` gets wrong for them:
- A running sink (`streamtap-sink serve`, also the one the app started), a running
  host (the browser starts it) or a running app locks its exe; cargo then fails with "Access is denied (os error 5)". The
  script names the process instead.
- A no-op build leaves the exes' timestamps alone, so a build that is current
  still looks older than the newest commit and stays "stale". After a
  successful build both exes get the current time.
Nothing is installed here (scripts\build-install.ps1 builds through this script and
installs); the extension is loaded unpacked.

.EXAMPLE
.\scripts\build.ps1
#>
[CmdletBinding()]
param()

$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
$manifest = Join-Path $root 'desktop\Cargo.toml'
$exes = @('streamtap-sink', 'streamtap-host', 'streamtap-app') | ForEach-Object {
    [pscustomobject]@{ Name = $_; Path = (Join-Path $root "desktop\target\release\$_.exe") }
}

foreach ($e in $exes) {
    $running = Get-Process -Name $e.Name -ErrorAction SilentlyContinue |
        Where-Object { $_.Path -and ($_.Path -eq $e.Path) }
    if ($running) {
        $ids = ($running | ForEach-Object { $_.Id }) -join ', '
        Write-Error "$($e.Name) is running from this checkout (PID $ids); stop it before building"
        exit 1
    }
}

# The target dir is named explicitly: a CARGO_TARGET_DIR or a build.target-dir
# setting would otherwise send the artifact elsewhere, and the old exe below
# would be touched and reported as current.
$targetDir = Join-Path $root 'desktop\target'
& cargo build --release --locked --manifest-path $manifest --target-dir $targetDir
if ($LASTEXITCODE -ne 0) {
    Write-Error "cargo build failed (exit code $LASTEXITCODE)"
    exit 1
}
foreach ($e in $exes) {
    if (-not (Test-Path -LiteralPath $e.Path)) {
        Write-Error "build finished but $($e.Path) is missing"
        exit 1
    }
    (Get-Item -LiteralPath $e.Path).LastWriteTime = Get-Date
    Write-Output "built $($e.Path)"
}