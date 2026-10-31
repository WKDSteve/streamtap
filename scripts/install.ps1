<#
.SYNOPSIS
Install streamtap for the current user from an unpacked release zip (or a staged folder).

.DESCRIPTION
Run it from the folder you unpacked streamtap-<version>-x86_64-pc-windows-msvc.zip into. It installs
exactly what that folder holds:

  %LOCALAPPDATA%\streamtap\bin\       streamtap-app.exe, streamtap-sink.exe, streamtap-host.exe
  %LOCALAPPDATA%\streamtap\extension\ the browser extension, to be loaded unpacked in Chrome
  %LOCALAPPDATA%\streamtap\           uninstall.ps1, LICENSE, README.md, VERSION

plus a Start menu entry "streamtap" and an entry in Settings > Apps (which runs the installed
uninstall.ps1). No administrator rights. The files are all copied first and then swapped in; a copy
or a swap that fails puts the old ones back, so the install is never a mix of versions. An install
from an earlier build-install.ps1 (programs directly in the install folder) is moved to bin\.

Your data is not touched: the settings, the token and the pairings in
%APPDATA%\io.github.wkdsteve.streamtap and the captures folder stay as they are.

The browsers: the app registers its native messaging host itself (Connect browser). A browser that
still starts a host from another place (a checkout, the layout before bin\) is named; open the
installed app from the Start menu and press "Register again" for each.

Prerequisite, not installed here: ffmpeg on the PATH (scoop install ffmpeg). The script says when it
finds none. The extension is loaded by hand: chrome://extensions, Developer mode, Load unpacked,
the extension folder named at the end.

Nothing is signed: Windows (SmartScreen) may ask before the first start of the app. Run it in a
normal terminal: a Claude session (the Claude app redirects AppData and HKCU) is refused.

This is the one installer: scripts\build-install.ps1 stages a fresh build in the same shape and
calls this script.

.PARAMETER PayloadDir
The folder to install from (default: the folder of this script, i.e. the unpacked zip).

.PARAMETER AllowElevated
Install although the session is elevated (for a build machine).

.PARAMETER InstallDir
Where it goes (default $env:STREAMTAP_INSTALL_DIR, else %LOCALAPPDATA%\streamtap).

.PARAMETER StartMenuDir
Where the Start menu entry goes (default $env:STREAMTAP_START_MENU_DIR, else the current user's
Start menu programs folder).

.PARAMETER RegistryRoot
The registry key that stands for HKCU\Software: the Settings > Apps entry is written below it, and
the browsers' NativeMessagingHosts keys are read below it (default $env:STREAMTAP_REGISTRY_ROOT,
else HKCU:\Software). For tests.

.EXAMPLE
.\install.ps1
# Windows PowerShell 5.1 under its default execution policy (Restricted), from the unpacked folder:
powershell -NoProfile -ExecutionPolicy Bypass -File .\install.ps1
#>
[CmdletBinding()]
param(
    [switch] $AllowElevated,
    [string] $PayloadDir = '',
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
# What a payload may hold at its top: anything else is refused (a folder that is not a release).
$topLevel = @('bin', 'extension', 'install.ps1', 'uninstall.ps1', 'LICENSE', 'README.md', 'VERSION')

function Exit-Install([string] $Message) {
    throw "install.ps1: $Message"
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
    if (-not $Dir -or $Dir.Contains('"') -or -not [IO.Path]::IsPathRooted($Dir)) { Exit-Install "not an absolute path: '$Dir'" }
    $full = [IO.Path]::GetFullPath($Dir)
    if ($full.TrimEnd('\') -eq ([IO.Path]::GetPathRoot($full)).TrimEnd('\')) { Exit-Install "a drive root is no place for this: '$Dir'" }
    return $full.TrimEnd('\')
}

# The registry root stands for HKCU\Software: a key of the current user, never another hive or a file path.
function Get-RegistryRoot([string] $Key) {
    $Key = $Key.TrimEnd('\')
    if ($Key.Contains('"') -or $Key -notmatch '^(HKCU:|Registry::HKEY_CURRENT_USER)\\.') { Exit-Install "not a key below HKCU: '$Key'" }
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

# The streamtap processes whose program lies in $Dir (bin\ or, in the older layout, the folder itself).
function Get-RunningFrom([string] $Dir) {
    @(Get-Process -Name $programs -ErrorAction SilentlyContinue |
            Where-Object { $_.Path -and (Test-Under $_.Path $Dir) })
}

if ($env:CLAUDECODE) {
    Exit-Install 'install in a normal terminal, not in a Claude session (the app redirects AppData and the registry)'
}
# Everything goes to the account that runs this: elevated, that may be the administrator's.
$principal = New-Object Security.Principal.WindowsPrincipal([Security.Principal.WindowsIdentity]::GetCurrent())
if (-not $AllowElevated -and $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
    Exit-Install 'do not run this elevated: it installs for one user (-AllowElevated for a build machine)'
}

$InstallDir = Get-Setting $InstallDir 'STREAMTAP_INSTALL_DIR' (Join-Path $env:LOCALAPPDATA 'streamtap')
$StartMenuDir = Get-Setting $StartMenuDir 'STREAMTAP_START_MENU_DIR' ([Environment]::GetFolderPath('Programs'))
$RegistryRoot = Get-RegistryRoot (Get-Setting $RegistryRoot 'STREAMTAP_REGISTRY_ROOT' 'HKCU:\Software')
$InstallDir = Get-Place $InstallDir
$StartMenuDir = Get-Place $StartMenuDir
$binDir = Join-Path $InstallDir 'bin'
$extensionDir = Join-Path $InstallDir 'extension'

# The payload: the shape of the release zip, checked before anything is written.
if (-not $PayloadDir) { $PayloadDir = $PSScriptRoot }
if (-not (Test-Path -LiteralPath $PayloadDir -PathType Container)) { Exit-Install "no such folder: $PayloadDir" }
$PayloadDir = (Resolve-Path -LiteralPath $PayloadDir).ProviderPath.TrimEnd('\')
foreach ($item in Get-ChildItem -LiteralPath $PayloadDir -Force) {
    if ($topLevel -cnotcontains $item.Name) { Exit-Install "$PayloadDir holds '$($item.Name)', which a release does not: nothing was installed" }
}
foreach ($name in $programs) {
    if (-not (Test-Path -LiteralPath (Join-Path $PayloadDir "bin\$name.exe") -PathType Leaf)) {
        Exit-Install "no bin\$name.exe in $PayloadDir (run this from the unpacked release zip)"
    }
}
$extraInBin = @(Get-ChildItem -LiteralPath (Join-Path $PayloadDir 'bin') -Force | Where-Object { $programs -notcontains $_.BaseName -or $_.Extension -ne '.exe' })
if ($extraInBin) { Exit-Install "bin\ holds '$($extraInBin[0].Name)', which a release does not: nothing was installed" }
foreach ($need in 'uninstall.ps1', 'VERSION', 'extension\manifest.json') {
    if (-not (Test-Path -LiteralPath (Join-Path $PayloadDir $need) -PathType Leaf)) { Exit-Install "no $need in $PayloadDir" }
}
$version = (Get-Content -LiteralPath (Join-Path $PayloadDir 'VERSION') -TotalCount 1).Trim()
if ($version -cnotmatch '^[0-9]+\.[0-9]+\.[0-9]+(\+[0-9a-f]{7})?$') { Exit-Install "VERSION says '$version', which is not a version of a release" }

# A running program locks its file, and an app replaced under its sink would be a mix of versions.
$running = @(Get-RunningFrom $InstallDir)
if ($running) {
    $names = ($running | ForEach-Object { "$($_.Name) (PID $($_.Id))" }) -join ', '
    Exit-Install "running from ${InstallDir}: $names; close the app (it stops its sink), then run this again"
}

# Everything in the payload but this script goes, with its place relative to the install folder.
$payloadPrefix = $PayloadDir + '\'
$files = @(Get-ChildItem -LiteralPath $PayloadDir -Recurse -File -Force |
        ForEach-Object { [pscustomobject]@{ From = $_.FullName; Name = $_.FullName.Substring($payloadPrefix.Length) } } |
        Where-Object { $_.Name -cne 'install.ps1' })

Write-Output "==> copying streamtap $version to $InstallDir"
New-Item -ItemType Directory -Force -Path $InstallDir | Out-Null
# All files are copied to .new first and renamed only when every copy succeeded: a copy that fails
# leaves the old install whole (no new app beside an old sink), and its .new files are taken away.
try {
    foreach ($f in $files) {
        $dest = Join-Path $InstallDir "$($f.Name).new"
        New-Item -ItemType Directory -Force -Path (Split-Path -Parent $dest) | Out-Null
        Copy-Item -LiteralPath $f.From -Destination $dest -Force
    }
}
catch {
    foreach ($f in $files) {
        Remove-Item -LiteralPath (Join-Path $InstallDir "$($f.Name).new") -Force -ErrorAction SilentlyContinue
    }
    Exit-Install "copying the files failed, the installed ones are unchanged: $($_.Exception.Message)"
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
    Exit-Install "replacing the files failed, the installed ones were put back: $why"
}
# An .old file still held by someone stays; uninstall.ps1 takes it away later.
foreach ($f in $files) {
    Remove-Item -LiteralPath (Join-Path $InstallDir "$($f.Name).old") -Force -ErrorAction SilentlyContinue
}

# What the earlier version had and this one has not: a file of the extension that went away, and the
# programs of the layout before bin\ (they are not running: the check above covers the folder).
$kept = @($files | ForEach-Object { Join-Path $InstallDir $_.Name })
if (Test-Path -LiteralPath $extensionDir) {
    foreach ($old in Get-ChildItem -LiteralPath $extensionDir -Recurse -File -Force) {
        if ($kept -notcontains $old.FullName) { Remove-Item -LiteralPath $old.FullName -Force -ErrorAction SilentlyContinue }
    }
    foreach ($dir in Get-ChildItem -LiteralPath $extensionDir -Recurse -Directory -Force | Sort-Object { $_.FullName.Length } -Descending) {
        if (-not (Get-ChildItem -LiteralPath $dir.FullName -Force)) { Remove-Item -LiteralPath $dir.FullName -Force -ErrorAction SilentlyContinue }
    }
}
foreach ($name in $programs) {
    $legacy = Join-Path $InstallDir "$name.exe"
    if (Test-Path -LiteralPath $legacy) {
        try {
            Remove-Item -LiteralPath $legacy -Force
            Write-Output "removed $legacy (the programs live in bin\ now)"
        }
        catch {
            Write-Output "could not remove $legacy : $($_.Exception.Message)"
        }
    }
}
$app = Join-Path $binDir 'streamtap-app.exe'

Write-Output "==> Start menu entry in $StartMenuDir"
New-Item -ItemType Directory -Force -Path $StartMenuDir | Out-Null
$shell = New-Object -ComObject WScript.Shell
$link = $shell.CreateShortcut((Join-Path $StartMenuDir 'streamtap.lnk'))
$link.TargetPath = $app
$link.WorkingDirectory = $binDir
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
$size = [int](((Get-ChildItem -LiteralPath $InstallDir -Recurse -File | Measure-Object -Property Length -Sum).Sum) / 1KB)
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
Write-Output 'your settings, pairings and captures were not touched'
Write-Output ''
Write-Output 'next: load the extension in Chrome (chrome://extensions, Developer mode, Load unpacked) from'
Write-Output "  $extensionDir"
Write-Output 'then open streamtap from the Start menu and use Connect browser'
if (-not (Get-Command -Name 'ffmpeg' -CommandType Application -ErrorAction SilentlyContinue)) {
    Write-Output ''
    Write-Output 'ffmpeg was not found on the PATH: muxing needs it (scoop install ffmpeg, or put ffmpeg.exe on the PATH)'
}

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
    elseif (-not (Test-Under $hostExe $binDir)) {
        $others += "  ${browser}: starts $hostExe"
    }
}
if ($others) {
    Write-Output ''
    Write-Output 'these browsers still start the host of another copy of streamtap:'
    $others | ForEach-Object { Write-Output $_ }
    Write-Output 'open streamtap from the Start menu, Connect browser, and press "Register again" for each'
}
