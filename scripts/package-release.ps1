<#
.SYNOPSIS
Pack the release zip of streamtap from prebuilt programs: streamtap-<version>-x86_64-pc-windows-msvc.zip.

.DESCRIPTION
Nothing is built here unless -Build asks for it (scripts\build.ps1). The zip holds exactly what
scripts\install.ps1 installs, in the layout it installs it:

  bin\streamtap-app.exe, streamtap-sink.exe, streamtap-host.exe
  extension\   the files Chrome needs (manifest.json, popup\, src\, icons\), no tests, no README, no keys
  install.ps1  installs this zip's content for the current user
  uninstall.ps1, LICENSE, README.md (how to install), VERSION

The zip is reproducible: entries are sorted, use forward slashes and a fixed timestamp, so the
same programs give the same bytes (and the same checksum) when packed by the same PowerShell/.NET
runtime (the compressed bytes of an entry depend on the runtime's deflate, so Windows PowerShell 5.1
and PowerShell 7 give different zips with the same content). SHA256SUMS ("<hash>  <file>", one line
per zip packed into that folder) is written next to it for a local check; the release workflow does
not upload that file, the publish action of appkit's release-kit makes the one of the release.

It packs the programs that are in -BuildDir: it refuses while one of them runs from there and warns
about one older than the last commit, but it builds nothing. Build first (-Build, or
scripts\build.ps1); the release workflow builds from a clean checkout first.

The release is a zip plus scripts, not a setup.exe: an unsigned setup would be stopped by
SmartScreen exactly like the programs, and the scripts already do the per-user install
(Start menu entry, Settings > Apps entry, no administrator rights).

build-install.ps1 uses -StageDir and -NoZip: it stages the same folder from a fresh build and hands
it to install.ps1, so the checkout install and the zip install are one code path.

.PARAMETER BuildDir
The folder with streamtap-app.exe, streamtap-sink.exe and streamtap-host.exe (default
desktop\target\release of this checkout).

.PARAMETER OutDir
Where the zip and SHA256SUMS go (default dist\ in the checkout). Not needed with -NoZip.

.PARAMETER StageDir
Stage the folder here (it must not exist or be empty) and keep it. Without it a temporary folder
is used and removed.

.PARAMETER NoZip
Only stage (-StageDir is then required).

.PARAMETER Version
The version the release says it is (a tag without its v). It must be the version of
desktop\crates\streamtap-app\Cargo.toml, which must be the one of tauri.conf.json.

.PARAMETER Rolling
Pack the rolling build: a fixed file name (streamtap-windows-x86_64.zip) and the version label
<version>+<short commit>; needs -Sha.

.PARAMETER Sha
The commit of the rolling build (7 to 40 hex digits); the first 7 go into the version label.

.PARAMETER ExtensionDir
The extension folder (default extension\ of this checkout). For tests.

.PARAMETER Build
Run scripts\build.ps1 first (release build of the three programs).

.EXAMPLE
.\scripts\package-release.ps1 -OutDir dist
.\scripts\package-release.ps1 -Rolling -Sha 0123456789abcdef0123456789abcdef01234567 -OutDir dist
#>
[CmdletBinding()]
param(
    [string] $BuildDir = '',
    [string] $OutDir = '',
    [string] $StageDir = '',
    [switch] $NoZip,
    [string] $Version = '',
    [switch] $Rolling,
    [string] $Sha = '',
    [string] $ExtensionDir = '',
    [switch] $Build
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$programs = @('streamtap-app', 'streamtap-sink', 'streamtap-host')
# What Chrome needs from the extension folder, by type; anything else in popup\, src\ or icons\ is refused.
$extensionTypes = @('.js', '.json', '.html', '.css', '.png', '.svg', '.ico')
$secretTypes = @('.pem', '.key', '.crx', '.p12', '.pfx')

function Exit-Package([string] $Message) {
    throw "package-release.ps1: $Message"
}

$root = Split-Path -Parent $PSScriptRoot
if (-not $BuildDir) { $BuildDir = Join-Path $root 'desktop\target\release' }
if (-not $ExtensionDir) { $ExtensionDir = Join-Path $root 'extension' }
if ($NoZip -and -not $StageDir) { Exit-Package '-NoZip stages only: give -StageDir' }
if (-not $NoZip -and -not $OutDir) { $OutDir = Join-Path $root 'dist' }

# The version of the app: Cargo.toml and tauri.conf.json must agree, and a release must say the same.
$cargoText = Get-Content -LiteralPath (Join-Path $root 'desktop\crates\streamtap-app\Cargo.toml') -Raw
if ($cargoText -notmatch '(?m)^version\s*=\s*"([^"]+)"') { Exit-Package 'no version in streamtap-app/Cargo.toml' }
$appVersion = $Matches[1]
if ($appVersion -cnotmatch '^[0-9]+\.[0-9]+\.[0-9]+$') { Exit-Package "the version '$appVersion' is not X.Y.Z" }
# The zip also carries the sink and the host: they are the app's version too.
foreach ($member in 'streamtap-sink', 'streamtap-host') {
    $memberText = Get-Content -LiteralPath (Join-Path $root "desktop\crates\$member\Cargo.toml") -Raw
    if ($memberText -notmatch '(?m)^version\s*=\s*"([^"]+)"') { Exit-Package "no version in $member/Cargo.toml" }
    if ($Matches[1] -ne $appVersion) {
        Exit-Package "streamtap-app says $appVersion but $member/Cargo.toml says $($Matches[1]): make them one version"
    }
}
$tauriVersion = [string](Get-Content -LiteralPath (Join-Path $root 'desktop\crates\streamtap-app\tauri.conf.json') -Raw |
        ConvertFrom-Json).version
if ($tauriVersion -ne $appVersion) {
    Exit-Package "Cargo.toml says $appVersion but tauri.conf.json says ${tauriVersion}: make them one version"
}
if ($Version) {
    $asked = $Version -replace '^v', ''
    if ($asked -ne $appVersion) {
        Exit-Package "asked for version $asked but Cargo.toml says $appVersion (bump it in its own commit first)"
    }
}

if ($Rolling) {
    if ($Sha -cnotmatch '^[0-9a-f]{7,40}$') { Exit-Package "-Rolling needs -Sha, a commit id in lower case hex (got '$Sha')" }
    $label = "$appVersion+$($Sha.Substring(0, 7))"
    $zipName = 'streamtap-windows-x86_64.zip'
}
else {
    if ($Sha) { Exit-Package '-Sha belongs to -Rolling' }
    $label = $appVersion
    $zipName = "streamtap-$appVersion-x86_64-pc-windows-msvc.zip"
}

if ($Build) {
    & (Join-Path $PSScriptRoot 'build.ps1')
    if ($LASTEXITCODE -ne 0) { Exit-Package 'the build failed' }
}

foreach ($name in $programs) {
    $exe = Join-Path $BuildDir "$name.exe"
    if (-not (Test-Path -LiteralPath $exe -PathType Leaf)) { Exit-Package "no $exe (build first: scripts\build.ps1)" }
    if ((Get-Item -LiteralPath $exe).Length -eq 0) { Exit-Package "$exe is empty" }
}

# A program running from here may be halfway through replacing itself or hold what is copied; a program
# older than the last commit is a stale build. CI builds fresh, so both are the case of a local run.
$committed = & git -C $root log -1 --format=%ct 2>$null
$commitTime = if ($LASTEXITCODE -eq 0 -and "$committed" -match '^\d+$') {
    [DateTimeOffset]::FromUnixTimeSeconds([long]$committed).UtcDateTime
}
foreach ($name in $programs) {
    $exe = (Resolve-Path -LiteralPath (Join-Path $BuildDir "$name.exe")).ProviderPath
    $running = @(Get-Process -Name $name -ErrorAction SilentlyContinue | Where-Object { $_.Path -and ($_.Path -eq $exe) })
    if ($running.Count) {
        Exit-Package "$name is running from $BuildDir (PID $(($running | ForEach-Object { $_.Id }) -join ', ')): stop it first"
    }
    if ($commitTime -and (Get-Item -LiteralPath $exe).LastWriteTimeUtc -lt $commitTime) {
        Write-Warning "$exe is older than the last commit: is it a stale build? (-Build makes a fresh one)"
    }
}

if ($StageDir) {
    if ((Test-Path -LiteralPath $StageDir) -and (Get-ChildItem -LiteralPath $StageDir -Force)) {
        Exit-Package "$StageDir is not empty"
    }
    $stage = $StageDir
    $temporary = $false
}
else {
    $stage = Join-Path ([IO.Path]::GetTempPath()) ('streamtap-stage-' + [Guid]::NewGuid().ToString('N'))
    $temporary = $true
}

function Copy-Into([string] $From, [string] $Rel) {
    $to = Join-Path $stage $Rel
    New-Item -ItemType Directory -Force -Path (Split-Path -Parent $to) | Out-Null
    Copy-Item -LiteralPath $From -Destination $to -Force
}

try {
    New-Item -ItemType Directory -Force -Path $stage | Out-Null

    foreach ($name in $programs) { Copy-Into (Join-Path $BuildDir "$name.exe") "bin\$name.exe" }

    # The extension: manifest.json, popup\, src\ and icons\ only (the tests in scripts\ and the README stay out).
    if (-not (Test-Path -LiteralPath (Join-Path $ExtensionDir 'manifest.json') -PathType Leaf)) {
        Exit-Package "no manifest.json in $ExtensionDir"
    }
    $extFull = (Resolve-Path -LiteralPath $ExtensionDir).ProviderPath.TrimEnd('\') + '\'
    $extFiles = @(Get-Item -LiteralPath (Join-Path $ExtensionDir 'manifest.json'))
    foreach ($sub in 'popup', 'src', 'icons') {
        $dir = Join-Path $ExtensionDir $sub
        if (Test-Path -LiteralPath $dir -PathType Container) {
            $extFiles += @(Get-ChildItem -LiteralPath $dir -Recurse -File -Force)
        }
    }
    foreach ($f in $extFiles) {
        $type = $f.Extension.ToLowerInvariant()
        if ($secretTypes -contains $type) { Exit-Package "the extension folder holds a key or package file ($($f.FullName)): not shipped" }
        if ($extensionTypes -notcontains $type) { Exit-Package "the extension folder holds a file of an unexpected type ($($f.FullName))" }
        Copy-Into $f.FullName ('extension\' + $f.FullName.Substring($extFull.Length))
    }
    # Everything the manifest and the popup name must be in the folder, or Chrome loads half an extension.
    $manifest = Get-Content -LiteralPath (Join-Path $ExtensionDir 'manifest.json') -Raw -Encoding UTF8 | ConvertFrom-Json
    $refs = @()
    # The value of a manifest property by its path (names separated by '.'), or nothing.
    function Get-ManifestValue($Node, [string[]] $Path) {
        foreach ($part in $Path) {
            if ($null -eq $Node -or -not $Node.PSObject.Properties[$part]) { return @() }
            $Node = $Node.$part
        }
        return @($Node | Where-Object { $null -ne $_ })
    }
    # A path value, or the values of an object (icons: size -> file).
    function Get-ManifestFile($Value) {
        foreach ($v in $Value) {
            if ($v -is [string]) { $v }
            elseif ($null -ne $v) { foreach ($p in $v.PSObject.Properties) { [string]$p.Value } }
        }
    }
    $workerDir = ''
    $worker = @(Get-ManifestValue $manifest @('background', 'service_worker'))
    if ($worker.Count) {
        $refs += [string]$worker[0]
        $workerDir = Split-Path -Parent ([string]$worker[0])
    }
    foreach ($cs in @(Get-ManifestValue $manifest @('content_scripts'))) {
        $refs += @(Get-ManifestValue $cs @('js'))
        $refs += @(Get-ManifestValue $cs @('css'))
    }
    $refs += @(Get-ManifestFile (Get-ManifestValue $manifest @('icons')))
    $refs += @(Get-ManifestFile (Get-ManifestValue $manifest @('action', 'default_icon')))
    $refs += @(Get-ManifestValue $manifest @('options_page'))
    $refs += @(Get-ManifestValue $manifest @('options_ui', 'page'))
    $refs += @(Get-ManifestValue $manifest @('side_panel', 'default_path'))
    foreach ($war in @(Get-ManifestValue $manifest @('web_accessible_resources'))) {
        foreach ($resource in @(Get-ManifestValue $war @('resources'))) {
            if ([string]$resource -match '[*?]') {
                # A pattern cannot be looked up; it must at least point into a folder that is shipped.
                if ([string]$resource -notmatch '^(popup|src)/') { Exit-Package "the extension names the pattern '$resource' but only popup\ and src\ are shipped" }
            }
            else { $refs += [string]$resource }
        }
    }
    if ($manifest.PSObject.Properties['default_locale']) { Exit-Package 'the extension has a default_locale: _locales\ is not shipped' }
    # What the service worker pulls in with importScripts("a.js", ...) (relative to the worker).
    if ($worker.Count -and (Test-Path -LiteralPath (Join-Path $stage ('extension\' + ([string]$worker[0] -replace '/', '\'))) -PathType Leaf)) {
        $workerText = Get-Content -LiteralPath (Join-Path $stage ('extension\' + ([string]$worker[0] -replace '/', '\'))) -Raw -Encoding UTF8
        foreach ($call in [regex]::Matches($workerText, 'importScripts\(([^)]*)\)')) {
            foreach ($m in [regex]::Matches($call.Groups[1].Value, '["'']([^"'']+)["'']')) {
                $refs += $(if ($workerDir) { Join-Path $workerDir $m.Groups[1].Value } else { $m.Groups[1].Value })
            }
        }
    }
    $popup = ''
    $popupRef = @(Get-ManifestValue $manifest @('action', 'default_popup'))
    if ($popupRef.Count) {
        $popup = [string]$popupRef[0]
        $refs += $popup
    }
    if ($popup -and (Test-Path -LiteralPath (Join-Path $stage "extension\$popup"))) {
        $html = Get-Content -LiteralPath (Join-Path $stage "extension\$popup") -Raw
        $base = Split-Path -Parent $popup
        foreach ($m in [regex]::Matches($html, '(?:src|href)="([^"#?:]+)"')) { $refs += $(if ($base) { Join-Path $base $m.Groups[1].Value } else { $m.Groups[1].Value }) }
    }
    foreach ($ref in $refs) {
        if (-not $ref -or -not (Test-Path -LiteralPath (Join-Path $stage ('extension\' + ($ref -replace '/', '\'))) -PathType Leaf)) {
            Exit-Package "the extension names '$ref' but it is not in the folder"
        }
    }

    Copy-Into (Join-Path $PSScriptRoot 'install.ps1') 'install.ps1'
    Copy-Into (Join-Path $PSScriptRoot 'uninstall.ps1') 'uninstall.ps1'
    Copy-Into (Join-Path $root 'LICENSE') 'LICENSE'
    Copy-Into (Join-Path $PSScriptRoot 'release-readme.md') 'README.md'
    # LF and no BOM: it is read by install.ps1 and by people.
    [IO.File]::WriteAllText((Join-Path $stage 'VERSION'), "$label`n", (New-Object Text.UTF8Encoding($false)))

    if ($NoZip) {
        Write-Output "staged $label in $stage"
        return
    }

    # The zip: entries sorted by ordinal comparison, forward slashes, one fixed timestamp.
    New-Item -ItemType Directory -Force -Path $OutDir | Out-Null
    $zipPath = Join-Path (Resolve-Path -LiteralPath $OutDir).ProviderPath $zipName
    # SHA256SUMS keeps one line per zip of this folder. The line of this zip goes first: a zip that
    # fails halfway must not leave the old hash beside it (or beside no zip at all).
    $sumsPath = Join-Path (Split-Path -Parent $zipPath) 'SHA256SUMS'
    $sumLines = @()
    if (Test-Path -LiteralPath $sumsPath) {
        $sumLines = @([IO.File]::ReadAllText($sumsPath) -split "\r?\n" | Where-Object { $_ -and -not $_.EndsWith("  $zipName", [StringComparison]::Ordinal) })
        if ($sumLines.Count) { [IO.File]::WriteAllText($sumsPath, (($sumLines -join "`n") + "`n"), (New-Object Text.UTF8Encoding($false))) }
        else { Remove-Item -LiteralPath $sumsPath -Force }
    }
    if (Test-Path -LiteralPath $zipPath) { Remove-Item -LiteralPath $zipPath -Force }
    $stageFull = (Resolve-Path -LiteralPath $stage).ProviderPath.TrimEnd('\') + '\'
    $names = @(Get-ChildItem -LiteralPath $stage -Recurse -File -Force | ForEach-Object { $_.FullName.Substring($stageFull.Length) })
    [Array]::Sort($names, [StringComparer]::Ordinal)
    Add-Type -AssemblyName System.IO.Compression
    $fixed = New-Object DateTimeOffset(2000, 1, 1, 0, 0, 0, [TimeSpan]::Zero)
    $stream = [IO.File]::Create($zipPath)
    try {
        $archive = New-Object IO.Compression.ZipArchive($stream, [IO.Compression.ZipArchiveMode]::Create)
        try {
            foreach ($rel in $names) {
                $entry = $archive.CreateEntry(($rel -replace '\\', '/'), [IO.Compression.CompressionLevel]::Optimal)
                $entry.LastWriteTime = $fixed
                $target = $entry.Open()
                try {
                    $source = [IO.File]::OpenRead((Join-Path $stage $rel))
                    try { $source.CopyTo($target) } finally { $source.Dispose() }
                }
                finally { $target.Dispose() }
            }
        }
        finally { $archive.Dispose() }
    }
    finally { $stream.Dispose() }

    $hash = (Get-FileHash -LiteralPath $zipPath -Algorithm SHA256).Hash.ToLowerInvariant()
    $allLines = @($sumLines) + @("$hash  $zipName")
    $sortKeys = @($allLines | ForEach-Object { $_.Substring($_.IndexOf('  ') + 2) })
    [Array]::Sort($sortKeys, $allLines, [StringComparer]::Ordinal)
    [IO.File]::WriteAllText($sumsPath, (($allLines -join "`n") + "`n"), (New-Object Text.UTF8Encoding($false)))
    Write-Output "packaged $label"
    Write-Output $zipPath
    Write-Output "$hash  $zipName"
}
finally {
    if ($temporary -and (Test-Path -LiteralPath $stage)) { Remove-Item -LiteralPath $stage -Recurse -Force -ErrorAction SilentlyContinue }
}
