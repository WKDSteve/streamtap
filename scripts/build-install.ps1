<#
.SYNOPSIS
Build streamtap from this checkout and install it for the current user, from the terminal.

.DESCRIPTION
The same entry point the other desktop apps have (scripts/build-install.ps1, scripts/uninstall.ps1),
for a machine where no setup may be started. scripts/build.ps1 builds the three programs (it refuses
while one of them runs from this checkout and names the PIDs); then the build is staged in the shape
of the release zip (scripts/package-release.ps1 -NoZip: bin\*.exe, the extension, uninstall.ps1, ...)
and installed by scripts/install.ps1, the one installer the zip uses as well: programs in
%LOCALAPPDATA%\streamtap\bin, the extension in %LOCALAPPDATA%\streamtap\extension, a Start menu
entry "streamtap" and an entry in Settings > Apps (which runs the installed uninstall.ps1). No
administrator rights; a file built on this machine carries no Mark-of-the-Web. The files are all
copied first and then swapped in; a copy or a swap that fails puts the old ones back, so the
install is never a mix of versions.

Your data is not touched: the settings, the token and the pairings in
%APPDATA%\io.github.wkdsteve.streamtap and the captures folder stay as they are.

The browsers: the app registers its native messaging host itself (Connect browser). A browser you
registered from the checkout keeps starting the checkout's streamtap-host.exe after the install;
the script names such browsers. Open the installed app from the Start menu and press "Register
again" for each (the app shows them as "registered for another location").

Run it in a normal terminal: a Claude session (the Claude app redirects AppData and HKCU) may
build (-BuildOnly), installing there is refused.

.PARAMETER BuildOnly
Build desktop\target\release\*.exe (scripts\build.ps1) and stop (no install).

.PARAMETER AllowElevated
Install although the session is elevated (for a build machine).

.PARAMETER BuildDir
Skip the build and install the three programs from this folder (a build of your own; the tests'
stand-ins).

.PARAMETER InstallDir
Where the programs go (default $env:STREAMTAP_INSTALL_DIR, else %LOCALAPPDATA%\streamtap).

.PARAMETER StartMenuDir
Where the Start menu entry goes (default $env:STREAMTAP_START_MENU_DIR, else the current user's
Start menu programs folder).

.PARAMETER RegistryRoot
The registry key that stands for HKCU\Software: the Settings > Apps entry is written below it, and
the browsers' NativeMessagingHosts keys are read below it (default $env:STREAMTAP_REGISTRY_ROOT,
else HKCU:\Software). For tests.

.EXAMPLE
.\scripts\build-install.ps1
.\scripts\build-install.ps1 -BuildOnly
# Windows PowerShell 5.1 under its default execution policy (Restricted), from the repo folder:
powershell -NoProfile -ExecutionPolicy Bypass -File scripts\build-install.ps1
#>
[CmdletBinding()]
param(
    [switch] $BuildOnly,
    [switch] $AllowElevated,
    [string] $BuildDir = '',
    [string] $InstallDir = '',
    [string] $StartMenuDir = '',
    [string] $RegistryRoot = ''
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$root = Split-Path -Parent $PSScriptRoot

function Exit-Build([string] $Message) {
    throw "build-install.ps1: $Message"
}

if (-not $BuildOnly) {
    if ($env:CLAUDECODE) {
        Exit-Build 'install in a normal terminal, not in a Claude session (the app redirects AppData and the registry); -BuildOnly builds'
    }
    # Everything goes to the account that runs this: elevated, that may be the administrator's.
    $principal = New-Object Security.Principal.WindowsPrincipal([Security.Principal.WindowsIdentity]::GetCurrent())
    if (-not $AllowElevated -and $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
        Exit-Build 'do not run this elevated: it installs for one user (-AllowElevated for a build machine)'
    }
}

if ($BuildDir) {
    if ($BuildOnly) { Exit-Build '-BuildDir skips the build, -BuildOnly only builds: give one of them' }
    Write-Output "==> using the programs in $BuildDir (no build)"
}
else {
    Write-Output '==> scripts\build.ps1'
    try {
        & (Join-Path $PSScriptRoot 'build.ps1')
    }
    catch {
        Exit-Build "the build failed: $($_.Exception.Message)"
    }
    if ($LASTEXITCODE -ne 0) { Exit-Build "the build failed (exit code $LASTEXITCODE)" }
    $BuildDir = Join-Path $root 'desktop\target\release'
}
if ($BuildOnly) { return }

# The build in the shape of the release zip, then the same installer the zip uses.
$stage = Join-Path ([IO.Path]::GetTempPath()) ('streamtap-stage-' + [Guid]::NewGuid().ToString('N'))
try {
    Write-Output '==> staging the release layout'
    & (Join-Path $PSScriptRoot 'package-release.ps1') -BuildDir $BuildDir -StageDir $stage -NoZip | Out-Null
    $forward = @{ PayloadDir = $stage }
    if ($InstallDir) { $forward['InstallDir'] = $InstallDir }
    if ($StartMenuDir) { $forward['StartMenuDir'] = $StartMenuDir }
    if ($RegistryRoot) { $forward['RegistryRoot'] = $RegistryRoot }
    if ($AllowElevated) { $forward['AllowElevated'] = $true }
    & (Join-Path $PSScriptRoot 'install.ps1') @forward
}
finally {
    if (Test-Path -LiteralPath $stage) { Remove-Item -LiteralPath $stage -Recurse -Force -ErrorAction SilentlyContinue }
}
