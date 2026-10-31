<#
.SYNOPSIS
Build streamtap from this checkout and install it for the current user, from the terminal.

.DESCRIPTION
The same entry point the other desktop apps have (scripts/build-install.ps1, scripts/uninstall.ps1),
for a machine where no setup may be started. scripts/build.ps1 builds the three programs (it refuses
while one of them runs from this checkout and names the PIDs); then streamtap-app.exe,
streamtap-sink.exe and streamtap-host.exe are copied to %LOCALAPPDATA%\streamtap (the app finds the
sink and the host next to itself), together with a copy of uninstall.ps1. A Start menu entry
"streamtap" and an entry in Settings > Apps (which runs that uninstall.ps1) are made for the
current user. No administrator rights; a file built on this machine carries no Mark-of-the-Web.
The files are all copied first and then swapped in; a copy or a swap that fails puts the old ones
back, so the install is never a mix of versions.

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

$programs = @('streamtap-app', 'streamtap-sink', 'streamtap-host')
# The Tauri identifier; the host name of the manifest is the same string.
$hostName = 'io.github.wkdsteve.streamtap'
$browsers = [ordered]@{ Chrome = 'Google\Chrome'; Edge = 'Microsoft\Edge'; Brave = 'BraveSoftware\Brave-Browser' }

function Exit-Build([string] $Message) {
    throw "build-install.ps1: $Message"
}

function Get-Setting([string] $Given, [string] $EnvName, [string] $Default) {
    if ($Given) { return $Given }
    $fromEnv = [Environment]::GetEnvironmentVariable($EnvName)
    if ($fromEnv) { return $fromEnv }
    return $Default
}

# Whether $Path lies inside $Dir (Windows paths: letter case does not count).
function Test-Under([string] $Path, [string] $Dir) {
    try {
        $p = [IO.Path]::GetFullPath($Path)
        $d = [IO.Path]::GetFullPath($Dir).TrimEnd('\') + '\'
    }
    catch {
        return $false
    }
    return $p.StartsWith($d, [StringComparison]::OrdinalIgnoreCase)
}

# An absolute folder that is not a drive root, without a trailing backslash (a "C:\x\" would end the
# quoted argument of the Settings > Apps command line with \", which escapes the quote).
function Get-Place([string] $Dir) {
    if (-not $Dir -or $Dir.Contains('"') -or -not [IO.Path]::IsPathRooted($Dir)) { Exit-Build "not an absolute path: '$Dir'" }
    $full = [IO.Path]::GetFullPath($Dir)
    if ($full.TrimEnd('\') -eq ([IO.Path]::GetPathRoot($full)).TrimEnd('\')) { Exit-Build "a drive root is no place for this: '$Dir'" }
    return $full.TrimEnd('\')
}

# The registry root stands for HKCU\Software: a key of the current user, never another hive or a file path.
function Get-RegistryRoot([string] $Key) {
    $Key = $Key.TrimEnd('\')
    if ($Key.Contains('"') -or $Key -notmatch '^(HKCU:|Registry::HKEY_CURRENT_USER)\\.') { Exit-Build "not a key below HKCU: '$Key'" }
    return $Key
}

# The host a native messaging manifest names ('' when it cannot be read). The app writes UTF-8
# without a BOM, which Windows PowerShell 5.1 reads as ANSI unless told.
function Get-ManifestHost([string] $Manifest) {
    try {
        $json = Get-Content -LiteralPath $Manifest -Raw -Encoding UTF8 | ConvertFrom-Json
        if ($json.PSObject.Properties['path']) { return [string]$json.path }
    }
    catch {
        return ''
    }
    return ''
}

# The streamtap processes whose program lies in $Dir.
function Get-RunningFrom([string] $Dir) {
    @(Get-Process -Name $programs -ErrorAction SilentlyContinue |
            Where-Object { $_.Path -and (Test-Under $_.Path $Dir) })
}

$root = Split-Path -Parent $PSScriptRoot

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

$InstallDir = Get-Setting $InstallDir 'STREAMTAP_INSTALL_DIR' (Join-Path $env:LOCALAPPDATA 'streamtap')
$StartMenuDir = Get-Setting $StartMenuDir 'STREAMTAP_START_MENU_DIR' ([Environment]::GetFolderPath('Programs'))
$RegistryRoot = Get-RegistryRoot (Get-Setting $RegistryRoot 'STREAMTAP_REGISTRY_ROOT' 'HKCU:\Software')
$InstallDir = Get-Place $InstallDir
$StartMenuDir = Get-Place $StartMenuDir

foreach ($name in $programs) {
    $source = Join-Path $BuildDir "$name.exe"
    if (-not (Test-Path -LiteralPath $source -PathType Leaf)) { Exit-Build "no $source" }
}
# A running program locks its file, and an app replaced under its sink would be a mix of versions.
$running = Get-RunningFrom $InstallDir
if ($running) {
    $names = ($running | ForEach-Object { "$($_.Name) (PID $($_.Id))" }) -join ', '
    Exit-Build "running from ${InstallDir}: $names; close the app (it stops its sink), then run this again"
}

$version = (Get-Content -LiteralPath (Join-Path $root 'desktop\crates\streamtap-app\tauri.conf.json') -Raw |
        ConvertFrom-Json).version

Write-Output "==> copying the programs to $InstallDir"
New-Item -ItemType Directory -Force -Path $InstallDir | Out-Null
# All files are copied to .new first and renamed only when every copy succeeded: a copy that fails
# leaves the old install whole (no new app beside an old sink), and its .new files are taken away.
$files = @($programs | ForEach-Object { [pscustomobject]@{ From = (Join-Path $BuildDir "$_.exe"); Name = "$_.exe" } })
$files += [pscustomobject]@{ From = (Join-Path $PSScriptRoot 'uninstall.ps1'); Name = 'uninstall.ps1' }
try {
    foreach ($f in $files) {
        Copy-Item -LiteralPath $f.From -Destination (Join-Path $InstallDir "$($f.Name).new") -Force
    }
}
catch {
    foreach ($f in $files) {
        Remove-Item -LiteralPath (Join-Path $InstallDir "$($f.Name).new") -Force -ErrorAction SilentlyContinue
    }
    Exit-Build "copying the programs failed, the installed ones are unchanged: $($_.Exception.Message)"
}
# Then each old file is moved aside to .old and its .new takes its place. A rename that fails (a
# program started meanwhile, a scanner holding a file) puts every file swapped so far back, so the
# install is again all old, never a mix of versions.
$swapped = New-Object System.Collections.Generic.List[object]
try {
    foreach ($f in $files) {
        $dest = Join-Path $InstallDir $f.Name
        $hadOld = Test-Path -LiteralPath $dest
        if ($hadOld) { Move-Item -LiteralPath $dest -Destination "$dest.old" -Force }
        $step = [pscustomobject]@{ Dest = $dest; HadOld = $hadOld; Placed = $false }
        $swapped.Add($step)
        Move-Item -LiteralPath "$dest.new" -Destination $dest -Force
        $step.Placed = $true
    }
}
catch {
    $why = $_.Exception.Message
    for ($i = $swapped.Count - 1; $i -ge 0; $i--) {
        $s = $swapped[$i]
        if ($s.Placed) { Remove-Item -LiteralPath $s.Dest -Force -ErrorAction SilentlyContinue }
        if ($s.HadOld) { Move-Item -LiteralPath "$($s.Dest).old" -Destination $s.Dest -Force -ErrorAction SilentlyContinue }
    }
    foreach ($f in $files) {
        Remove-Item -LiteralPath (Join-Path $InstallDir "$($f.Name).new") -Force -ErrorAction SilentlyContinue
    }
    Exit-Build "replacing the programs failed, the installed ones were put back: $why"
}
# An .old file still held by someone stays; uninstall.ps1 takes it away later.
foreach ($f in $files) {
    Remove-Item -LiteralPath (Join-Path $InstallDir "$($f.Name).old") -Force -ErrorAction SilentlyContinue
}
$app = Join-Path $InstallDir 'streamtap-app.exe'

Write-Output "==> Start menu entry in $StartMenuDir"
New-Item -ItemType Directory -Force -Path $StartMenuDir | Out-Null
$shell = New-Object -ComObject WScript.Shell
$link = $shell.CreateShortcut((Join-Path $StartMenuDir 'streamtap.lnk'))
$link.TargetPath = $app
$link.WorkingDirectory = $InstallDir
$link.IconLocation = "$app,0"
$link.Description = 'streamtap: capture the videos of your browser tabs'
$link.Save()

# The entry of Settings > Apps runs the copy of uninstall.ps1 with the places of this install.
$uninstallKey = "$RegistryRoot\Microsoft\Windows\CurrentVersion\Uninstall\streamtap"
Write-Output "==> Settings > Apps entry ($uninstallKey)"
$ps = Join-Path $env:SystemRoot 'System32\WindowsPowerShell\v1.0\powershell.exe'
# -PauseOnError: Settings opens a console window that would close before an error could be read.
$command = "`"$ps`" -NoProfile -ExecutionPolicy Bypass -File `"$(Join-Path $InstallDir 'uninstall.ps1')`"" +
" -InstallDir `"$InstallDir`" -StartMenuDir `"$StartMenuDir`" -RegistryRoot `"$RegistryRoot`" -PauseOnError"
$size = [int](((Get-ChildItem -LiteralPath $InstallDir -File | Measure-Object -Property Length -Sum).Sum) / 1KB)
# There is one entry: an install in another folder loses it (its files stay there), so say so.
$previous = Get-ItemProperty -LiteralPath $uninstallKey -ErrorAction SilentlyContinue
$previousDir = ''
if ($previous -and $previous.PSObject.Properties['InstallLocation']) { $previousDir = ([string]$previous.InstallLocation).TrimEnd('\') }
if ($previousDir -and $previousDir -ne $InstallDir) {
    Write-Output "the Settings > Apps entry was for the install in $previousDir; it now names this one, that copy stays:"
    Write-Output "  remove it with: & `"$(Join-Path $previousDir 'uninstall.ps1')`" -InstallDir `"$previousDir`""
}
if (Test-Path -LiteralPath $uninstallKey) { Remove-Item -LiteralPath $uninstallKey -Recurse -Force }
New-Item -Path $uninstallKey -Force | Out-Null
$values = [ordered]@{
    DisplayName     = 'streamtap'
    DisplayVersion  = [string]$version
    Publisher       = 'WKDSteve'
    InstallLocation = $InstallDir
    DisplayIcon     = "$app,0"
    UninstallString = $command
}
foreach ($k in $values.Keys) {
    New-ItemProperty -LiteralPath $uninstallKey -Name $k -Value $values[$k] -PropertyType String -Force | Out-Null
}
foreach ($k in 'NoModify', 'NoRepair') {
    New-ItemProperty -LiteralPath $uninstallKey -Name $k -Value 1 -PropertyType DWord -Force | Out-Null
}
New-ItemProperty -LiteralPath $uninstallKey -Name 'EstimatedSize' -Value $size -PropertyType DWord -Force | Out-Null

Write-Output ''
Write-Output "installed streamtap $version to $InstallDir"
Write-Output "your settings, pairings and captures were not touched"

# Read only: the app owns the registration (manifest, key, rollback). A browser that starts another
# copy's host still works while that copy exists, but it is not this install's.
$others = @()
foreach ($browser in $browsers.Keys) {
    $key = "$RegistryRoot\$($browsers[$browser])\NativeMessagingHosts\$hostName"
    $item = Get-Item -LiteralPath $key -ErrorAction SilentlyContinue
    if (-not $item) { continue }
    $manifest = [string]$item.GetValue('')
    $hostExe = Get-ManifestHost $manifest
    if (-not $hostExe) {
        $others += "  ${browser}: its manifest ($manifest) cannot be read"
    }
    elseif (-not (Test-Under $hostExe $InstallDir)) {
        $others += "  ${browser}: starts $hostExe"
    }
}
if ($others) {
    Write-Output ''
    Write-Output 'these browsers still start the host of another copy of streamtap:'
    $others | ForEach-Object { Write-Output $_ }
    Write-Output 'open streamtap from the Start menu, Connect browser, and press "Register again" for each'
}
