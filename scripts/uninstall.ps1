<#
.SYNOPSIS
Remove streamtap for the current user on Windows, without administrator rights.

.DESCRIPTION
Takes away what scripts/build-install.ps1 made: the programs and the copy of this script in
%LOCALAPPDATA%\streamtap, the Start menu entry and the entry in Settings > Apps (which runs this
script from there). A browser's native messaging key is removed only when its manifest names a
host inside the install folder; a registration of another copy (a checkout) stays.

Your data stays unless you ask: -RemoveData also removes the files the app makes in its settings
folder %APPDATA%\io.github.wkdsteve.streamtap (config.json with the token, pairings.json,
bridge.json, the host manifests in native-messaging\) and its WebView cache
(%LOCALAPPDATA%\io.github.wkdsteve.streamtap\EBWebView), and then also the browser keys that lead to
a manifest in there. Nothing else is removed: the captures (by default the folder captures\ inside
the settings folder) stay, and so does a folder that still holds anything. A second run finds
nothing to do and says so; it is not an error.

It refuses while streamtap-app, -sink or -host runs from the install folder (and names the PIDs):
close the app first, it stops its sink.

.PARAMETER RemoveData
Also remove the app's own files: settings, token, pairings, host manifests, WebView cache (never the
captures, never a file the app did not make).

.PARAMETER AllowElevated
Remove although the session is elevated (for a build machine).

.PARAMETER InstallDir
The install folder (default $env:STREAMTAP_INSTALL_DIR, else %LOCALAPPDATA%\streamtap).

.PARAMETER StartMenuDir
The folder of the Start menu entry (default $env:STREAMTAP_START_MENU_DIR, else the current user's
Start menu programs folder).

.PARAMETER RegistryRoot
The registry key that stands for HKCU\Software (default $env:STREAMTAP_REGISTRY_ROOT, else
HKCU:\Software). For tests.

.PARAMETER DataDir
The app's settings folder (default $env:STREAMTAP_DATA_DIR, else
%APPDATA%\io.github.wkdsteve.streamtap). Only read with -RemoveData.

.PARAMETER LocalDataDir
The app's WebView data (default $env:STREAMTAP_LOCAL_DATA_DIR, else
%LOCALAPPDATA%\io.github.wkdsteve.streamtap). Only read with -RemoveData.

.EXAMPLE
.\scripts\uninstall.ps1
.\scripts\uninstall.ps1 -RemoveData
#>
[CmdletBinding()]
param(
    [switch] $RemoveData,
    [switch] $AllowElevated,
    [string] $InstallDir = '',
    [string] $StartMenuDir = '',
    [string] $RegistryRoot = '',
    [string] $DataDir = '',
    [string] $LocalDataDir = ''
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$programs = @('streamtap-app', 'streamtap-sink', 'streamtap-host')
$hostName = 'io.github.wkdsteve.streamtap'
$browsers = [ordered]@{ Chrome = 'Google\Chrome'; Edge = 'Microsoft\Edge'; Brave = 'BraveSoftware\Brave-Browser' }

function Exit-Uninstall([string] $Message) {
    throw "uninstall.ps1: $Message"
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

# A file that cannot be removed is an error, never a "removed".
function Invoke-Removal([string] $Path, [switch] $Recurse) {
    try { Remove-Item -LiteralPath $Path -Force -Recurse:$Recurse }
    catch { Exit-Uninstall "cannot remove ${Path}: $($_.Exception.Message)" }
    if (Test-Path -LiteralPath $Path) { Exit-Uninstall "$Path is still there" }
}

if ($env:CLAUDECODE) {
    Exit-Uninstall 'run this in a normal terminal, not in a Claude session (the app redirects AppData and the registry)'
}
$principal = New-Object Security.Principal.WindowsPrincipal([Security.Principal.WindowsIdentity]::GetCurrent())
if (-not $AllowElevated -and $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
    Exit-Uninstall 'do not run this elevated: it removes the installation of one user (-AllowElevated for a build machine)'
}

$InstallDir = Get-Setting $InstallDir 'STREAMTAP_INSTALL_DIR' (Join-Path $env:LOCALAPPDATA 'streamtap')
$StartMenuDir = Get-Setting $StartMenuDir 'STREAMTAP_START_MENU_DIR' ([Environment]::GetFolderPath('Programs'))
$RegistryRoot = Get-Setting $RegistryRoot 'STREAMTAP_REGISTRY_ROOT' 'HKCU:\Software'
$DataDir = Get-Setting $DataDir 'STREAMTAP_DATA_DIR' (Join-Path $env:APPDATA $hostName)
$LocalDataDir = Get-Setting $LocalDataDir 'STREAMTAP_LOCAL_DATA_DIR' (Join-Path $env:LOCALAPPDATA $hostName)
foreach ($dir in $InstallDir, $StartMenuDir, $DataDir, $LocalDataDir) {
    if (-not [IO.Path]::IsPathRooted($dir)) { Exit-Uninstall "not an absolute path: $dir" }
}
$InstallDir = [IO.Path]::GetFullPath($InstallDir).TrimEnd('\')

$running = @(Get-Process -Name $programs -ErrorAction SilentlyContinue |
        Where-Object { $_.Path -and (Test-Under $_.Path $InstallDir) })
if ($running) {
    $names = ($running | ForEach-Object { "$($_.Name) (PID $($_.Id))" }) -join ', '
    Exit-Uninstall "running from ${InstallDir}: $names; close the app (it stops its sink), then run this again"
}

$removed = @()

# 1. The browsers' keys whose manifest names a host in the install folder (or, with -RemoveData, whose
# manifest goes with the data). Another copy's registration stays.
foreach ($browser in $browsers.Keys) {
    $key = "$RegistryRoot\$($browsers[$browser])\NativeMessagingHosts\$hostName"
    $item = Get-Item -LiteralPath $key -ErrorAction SilentlyContinue
    if (-not $item) { continue }
    $manifest = [string]$item.GetValue('')
    $hostExe = ''
    try {
        $json = Get-Content -LiteralPath $manifest -Raw | ConvertFrom-Json
        if ($json.PSObject.Properties['path']) { $hostExe = [string]$json.path }
    }
    catch {
        $hostExe = ''
    }
    $ours = $hostExe -and (Test-Under $hostExe $InstallDir)
    $withData = $RemoveData -and $manifest -and (Test-Under $manifest $DataDir)
    if (-not ($ours -or $withData)) {
        Write-Output "kept the $browser registration (it starts $hostExe, not this install)"
        continue
    }
    Invoke-Removal $key -Recurse
    if ($ours -and (Test-Path -LiteralPath $manifest -PathType Leaf)) { Invoke-Removal $manifest }
    $removed += "the $browser registration"
}

# 2. The Start menu entry, when it leads into the install folder.
$link = Join-Path $StartMenuDir 'streamtap.lnk'
if (Test-Path -LiteralPath $link -PathType Leaf) {
    $target = (New-Object -ComObject WScript.Shell).CreateShortcut($link).TargetPath
    if ($target -and (Test-Under $target $InstallDir)) {
        Invoke-Removal $link
        $removed += "the Start menu entry"
    }
    else {
        Write-Output "kept $link (it starts $target, not this install)"
    }
}

# 3. The files build-install.ps1 puts there; a folder that holds anything else stays.
foreach ($name in @($programs | ForEach-Object { "$_.exe"; "$_.exe.new" }) + @('uninstall.ps1', 'uninstall.ps1.new')) {
    $file = Join-Path $InstallDir $name
    if (Test-Path -LiteralPath $file) {
        Invoke-Removal $file
        if ($name -notlike '*.new') { $removed += $file }
    }
}
if ((Test-Path -LiteralPath $InstallDir) -and -not (Get-ChildItem -LiteralPath $InstallDir -Force)) {
    Invoke-Removal $InstallDir
}

# 4. The Settings > Apps entry last: while anything above fails, it stays to run again.
$uninstallKey = "$RegistryRoot\Microsoft\Windows\CurrentVersion\Uninstall\streamtap"
$entry = Get-ItemProperty -LiteralPath $uninstallKey -ErrorAction SilentlyContinue
if ($entry) {
    $location = ''
    if ($entry.PSObject.Properties['InstallLocation']) { $location = [string]$entry.InstallLocation }
    if ($location -and ([IO.Path]::GetFullPath($location).TrimEnd('\') -eq $InstallDir)) {
        Invoke-Removal $uninstallKey -Recurse
        $removed += 'the Settings > Apps entry'
    }
    else {
        Write-Output "kept the Settings > Apps entry (it is for $location, not this install)"
    }
}

# 5. The data, only when asked, and only the files the app makes. The default captures folder is
# DataDir\captures: it and anything else in there stay, and so does the folder that holds them.
if ($RemoveData) {
    $appFiles = @('config.json', 'pairings.json', 'bridge.json') | ForEach-Object { $_; ($_ -replace '\.json$', '.json.tmp') }
    foreach ($name in $appFiles) {
        $file = Join-Path $DataDir $name
        if (Test-Path -LiteralPath $file -PathType Leaf) {
            Invoke-Removal $file
            $removed += $file
        }
    }
    $manifests = Join-Path $DataDir 'native-messaging'
    foreach ($browser in $browsers.Keys) {
        $id = $browser.ToLowerInvariant()
        foreach ($name in "$id.json", "$id.json.tmp") {
            $file = Join-Path $manifests $name
            if (Test-Path -LiteralPath $file -PathType Leaf) {
                Invoke-Removal $file
                $removed += $file
            }
        }
    }
    # The WebView's own cache (Tauri keeps it in EBWebView).
    $webView = Join-Path $LocalDataDir 'EBWebView'
    if (Test-Path -LiteralPath $webView) {
        Invoke-Removal $webView -Recurse
        $removed += $webView
    }
    foreach ($dir in $manifests, $DataDir, $LocalDataDir) {
        if ((Test-Path -LiteralPath $dir) -and -not (Get-ChildItem -LiteralPath $dir -Force)) { Invoke-Removal $dir }
    }
    if (Test-Path -LiteralPath $DataDir) {
        Write-Output "kept $DataDir (it holds files that are not the app's settings, such as the captures folder)"
    }
}

if (-not $removed) {
    Write-Output "streamtap is not installed for this user ($InstallDir), nothing to do"
    return
}
Write-Output ''
$removed | ForEach-Object { Write-Output "removed $_" }
if (-not $RemoveData) {
    Write-Output 'your settings and pairings were kept (-RemoveData removes them); your captures were not touched'
}
else {
    Write-Output 'your captures were not touched'
}
