<#
.SYNOPSIS
Test scripts/package-release.ps1 and the release zip it makes, without a build and without a real install.

.DESCRIPTION
Packs the release zip from programs that already exist (stand-ins that are copies of cmd.exe, or a
build you name with -BuildDir: nothing is built) and checks:

- the zip holds exactly the programs in bin/, the files Chrome needs of the extension (no tests, no
  README, no keys), install.ps1, uninstall.ps1, LICENSE, README.md and VERSION, with forward slashes
  and the bytes of the sources;
- SHA256SUMS names the zip with its real SHA-256, and packing twice gives the same bytes;
- the file names and version labels of a release and of the rolling build, and what is refused
  (a version that is not the one of Cargo.toml or tauri.conf.json, a rolling build without a commit,
  a missing or empty program, a key file or a file of an unexpected type in the extension, a manifest
  that names a file that is not there);
- the zip unpacked into a folder, and its install.ps1 run into temp places (a throw-away registry
  root, a temp Start menu and install folder; the account's own are never touched), then the
  installed uninstall.ps1 run.

With -Zip the same checks on the zip itself run too, against the programs of -BuildDir (the release
workflow checks the zip it is about to publish this way).

Exit 0 when all checks pass.

.PARAMETER BuildDir
A folder with streamtap-app.exe, streamtap-sink.exe and streamtap-host.exe (default: stand-ins).

.PARAMETER Zip
A release zip made from -BuildDir, checked as well.

.PARAMETER Label
The version label the -Zip must say (X.Y.Z, or X.Y.Z+<short commit> for the rolling build). Without
it the zip's own VERSION is believed, which checks less.
#>
[CmdletBinding()]
param(
    [string] $BuildDir = '',
    [string] $Zip = '',
    [string] $Label = ''
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$here = $PSScriptRoot
$root = Split-Path -Parent $here
$id = [Guid]::NewGuid().ToString('N')
# A space and a non-ASCII letter in every place, as test-install.ps1 does.
$tmp = Join-Path ([IO.Path]::GetTempPath()) ("streamtap test-release $([char]0xFC)-$id")
$regRoot = "HKCU:\Software\streamtap-test-release-$id"
New-Item -ItemType Directory -Path $tmp | Out-Null

$script:failures = 0
function Write-Check([bool] $Passed, [string] $Name) {
    if ($Passed) { Write-Output "ok   $Name" }
    else { Write-Output "FAIL $Name"; $script:failures++ }
}

$packager = Join-Path $here 'package-release.ps1'
$identity = [Security.Principal.WindowsIdentity]::GetCurrent()
$elevated = ([Security.Principal.WindowsPrincipal] $identity).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
$windowsPowerShell = Join-Path $env:SystemRoot 'System32\WindowsPowerShell\v1.0\powershell.exe'

# The packager in this process: whether it ended, and what it said.
function Invoke-Package([hashtable] $Arguments, [string] $Script = $packager) {
    try {
        $out = @(& $Script @Arguments)
        return [pscustomobject]@{ Ok = $true; Text = ($out -join "`n") }
    }
    catch {
        return [pscustomobject]@{ Ok = $false; Text = $_.Exception.Message }
    }
}

# A script in a child PowerShell with the places of the test (the child runs without CLAUDECODE).
function Invoke-Child([string] $Script, [string[]] $Arguments, [string] $Shell = '') {
    if (-not $Shell) { $Shell = (Get-Process -Id $PID).Path }
    $extra = @()
    if ($elevated) { $extra = @('-AllowElevated') }
    $saved = $env:CLAUDECODE
    $ErrorActionPreference = 'Continue'
    try {
        $env:CLAUDECODE = $null
        $output = & $Shell -NoProfile -NonInteractive -ExecutionPolicy Bypass -File $Script @Arguments @extra 2>&1 | Out-String
        $code = $LASTEXITCODE
    }
    finally {
        $env:CLAUDECODE = $saved
    }
    return [pscustomobject]@{ Code = $code; Text = $output }
}

function Get-Sha([string] $Path) { (Get-FileHash -LiteralPath $Path -Algorithm SHA256).Hash.ToLowerInvariant() }

# The entries of a zip, as the names inside it.
function Get-ZipEntryName([string] $Path) {
    Add-Type -AssemblyName System.IO.Compression.FileSystem
    $archive = [IO.Compression.ZipFile]::OpenRead($Path)
    try { return @($archive.Entries | ForEach-Object { $_.FullName }) }
    finally { $archive.Dispose() }
}

# The bytes of one entry as text.
function Read-ZipEntry([string] $Path, [string] $Name) {
    Add-Type -AssemblyName System.IO.Compression.FileSystem
    $archive = [IO.Compression.ZipFile]::OpenRead($Path)
    try {
        $entry = $archive.GetEntry($Name)
        $reader = New-Object IO.StreamReader($entry.Open(), (New-Object Text.UTF8Encoding($false)))
        try { return $reader.ReadToEnd() } finally { $reader.Dispose() }
    }
    finally { $archive.Dispose() }
}

# What the extension folder ships: manifest.json, popup\ and src\ (never scripts\ or the README).
function Get-ExtensionShipped([string] $Dir) {
    $prefix = (Resolve-Path -LiteralPath $Dir).ProviderPath.TrimEnd('\') + '\'
    $files = @(Get-Item -LiteralPath (Join-Path $Dir 'manifest.json'))
    foreach ($sub in 'popup', 'src') { $files += @(Get-ChildItem -LiteralPath (Join-Path $Dir $sub) -Recurse -File) }
    @($files | ForEach-Object { 'extension/' + $_.FullName.Substring($prefix.Length).Replace('\', '/') })
}

# The checks every release zip must pass, against the programs it was made from.
function Test-Archive([string] $Path, [string] $Programs, [string] $Label, [string] $Title) {
    $names = Get-ZipEntryName $Path
    $expected = @('bin/streamtap-app.exe', 'bin/streamtap-sink.exe', 'bin/streamtap-host.exe',
        'install.ps1', 'uninstall.ps1', 'LICENSE', 'README.md', 'VERSION') + (Get-ExtensionShipped (Join-Path $root 'extension'))
    $missing = @($expected | Where-Object { $names -cnotcontains $_ })
    $unexpected = @($names | Where-Object { $expected -cnotcontains $_ })
    Write-Check ($missing.Count -eq 0 -and $unexpected.Count -eq 0) "$Title holds exactly the programs, the extension files, the scripts, LICENSE, README.md and VERSION"
    if ($missing.Count -or $unexpected.Count) { Write-Output "  missing: $($missing -join ', ')  unexpected: $($unexpected -join ', ')" }
    Write-Check (@($names | Where-Object { $_ -match '\\' -or $_ -match '^/' -or $_ -match '\.\.' }).Count -eq 0) "$Title uses forward slashes and no path that leaves its folder"
    Write-Check (@($names | Where-Object { $_ -match '^extension/(scripts|README)' -or $_ -match '\.(pem|key|crx|map)$|test' }).Count -eq 0) "$Title ships no test, no README and no key of the extension"
    Write-Check ($names -cnotcontains 'SHA256SUMS') "$Title does not hold the checksums of itself"
    $ordered = @($names)
    [Array]::Sort($ordered, [StringComparer]::Ordinal)
    Write-Check (($names -join '|') -ceq ($ordered -join '|')) "$Title lists its entries in ordinal order"
    Add-Type -AssemblyName System.IO.Compression.FileSystem
    $dir = Join-Path $tmp ('unpacked-' + [Guid]::NewGuid().ToString('N'))
    [IO.Compression.ZipFile]::ExtractToDirectory($Path, $dir)
    $same = $true
    foreach ($name in 'streamtap-app', 'streamtap-sink', 'streamtap-host') {
        if ((Get-Sha (Join-Path $dir "bin\$name.exe")) -ne (Get-Sha (Join-Path $Programs "$name.exe"))) { $same = $false }
    }
    Write-Check $same "$Title carries the bytes of the programs"
    $sources = @{ 'install.ps1' = (Join-Path $here 'install.ps1'); 'uninstall.ps1' = (Join-Path $here 'uninstall.ps1'); 'LICENSE' = (Join-Path $root 'LICENSE')
        'README.md' = (Join-Path $here 'release-readme.md') }
    # Every file of the extension, not just two of them.
    foreach ($rel in Get-ExtensionShipped (Join-Path $root 'extension')) { $sources[$rel.Replace('/', '\')] = Join-Path $root $rel.Replace('/', '\') }
    $sameSources = $true
    foreach ($key in $sources.Keys) {
        if ((Get-Sha (Join-Path $dir $key)) -ne (Get-Sha $sources[$key])) { $sameSources = $false }
    }
    Write-Check $sameSources "$Title carries the scripts, LICENSE, README.md and every extension file of the checkout unchanged"
    Write-Check ((Get-Content -LiteralPath (Join-Path $dir 'VERSION') -Raw) -ceq "$Label`n") "$Title says its version is $Label"
    $script:archiveDir = $dir
}

# Whether the manifest's files are all in the unpacked extension folder (Chrome would refuse half an extension).
function Test-ExtensionComplete([string] $ExtensionDir) {
    $manifest = Get-Content -LiteralPath (Join-Path $ExtensionDir 'manifest.json') -Raw | ConvertFrom-Json
    $refs = @($manifest.background.service_worker, $manifest.action.default_popup)
    foreach ($cs in $manifest.content_scripts) { $refs += @($cs.js) }
    foreach ($ref in $refs) {
        if (-not (Test-Path -LiteralPath (Join-Path $ExtensionDir ($ref -replace '/', '\')) -PathType Leaf)) { return $false }
    }
    return $true
}

# A copy of what package-release.ps1 reads, so a test can break one of its inputs.
function Copy-FakeRoot([string] $Name) {
    $fake = Join-Path $tmp $Name
    New-Item -ItemType Directory -Path (Join-Path $fake 'scripts'), (Join-Path $fake 'desktop\crates\streamtap-app') | Out-Null
    foreach ($file in 'package-release.ps1', 'install.ps1', 'uninstall.ps1', 'release-readme.md') {
        Copy-Item -LiteralPath (Join-Path $here $file) -Destination (Join-Path $fake "scripts\$file")
    }
    foreach ($file in 'Cargo.toml', 'tauri.conf.json') {
        Copy-Item -LiteralPath (Join-Path $root "desktop\crates\streamtap-app\$file") -Destination (Join-Path $fake "desktop\crates\streamtap-app\$file")
    }
    foreach ($member in 'streamtap-sink', 'streamtap-host') {
        New-Item -ItemType Directory -Path (Join-Path $fake "desktop\crates\$member") | Out-Null
        Copy-Item -LiteralPath (Join-Path $root "desktop\crates\$member\Cargo.toml") -Destination (Join-Path $fake "desktop\crates\$member\Cargo.toml")
    }
    Copy-Item -LiteralPath (Join-Path $root 'LICENSE') -Destination (Join-Path $fake 'LICENSE')
    Copy-Item -LiteralPath (Join-Path $root 'extension') -Destination (Join-Path $fake 'extension') -Recurse
    return $fake
}

$envNames = 'STREAMTAP_INSTALL_DIR', 'STREAMTAP_START_MENU_DIR', 'STREAMTAP_REGISTRY_ROOT', 'STREAMTAP_DATA_DIR', 'STREAMTAP_LOCAL_DATA_DIR'
$savedEnv = @{}
foreach ($name in $envNames) { $savedEnv[$name] = [Environment]::GetEnvironmentVariable($name) }
try {
    # Whatever a call forgets to pass, it must not reach the real folders or keys of this account.
    $install = Join-Path $tmp 'install'
    $startMenu = Join-Path $tmp 'start-menu'
    [Environment]::SetEnvironmentVariable('STREAMTAP_INSTALL_DIR', $install)
    [Environment]::SetEnvironmentVariable('STREAMTAP_START_MENU_DIR', $startMenu)
    [Environment]::SetEnvironmentVariable('STREAMTAP_REGISTRY_ROOT', $regRoot)
    [Environment]::SetEnvironmentVariable('STREAMTAP_DATA_DIR', (Join-Path $tmp 'roaming'))
    [Environment]::SetEnvironmentVariable('STREAMTAP_LOCAL_DATA_DIR', (Join-Path $tmp 'local'))

    if ($BuildDir -eq '') {
        $BuildDir = Join-Path $tmp 'build'
        New-Item -ItemType Directory -Path $BuildDir | Out-Null
        foreach ($name in 'streamtap-app', 'streamtap-sink', 'streamtap-host') {
            Copy-Item -LiteralPath (Join-Path $env:SystemRoot 'System32\cmd.exe') -Destination (Join-Path $BuildDir "$name.exe")
            # Three different programs, so a zip that packs one for another is seen.
            $extra = [Text.Encoding]::ASCII.GetBytes("stand-in $name")
            $exeStream = [IO.File]::Open((Join-Path $BuildDir "$name.exe"), [IO.FileMode]::Append)
            try { $exeStream.Write($extra, 0, $extra.Length) } finally { $exeStream.Dispose() }
        }
    }
    $cargo = Get-Content -LiteralPath (Join-Path $root 'desktop\crates\streamtap-app\Cargo.toml') -Raw
    $null = $cargo -match '(?m)^version\s*=\s*"([^"]+)"'
    $version = $Matches[1]

    # 1. A release zip.
    $out1 = Join-Path $tmp 'dist one'
    $r = Invoke-Package @{ BuildDir = $BuildDir; OutDir = $out1 }
    Write-Check $r.Ok 'package a release'
    if (-not $r.Ok) { Write-Output $r.Text }
    $zipName = "streamtap-$version-x86_64-pc-windows-msvc.zip"
    $zip1 = Join-Path $out1 $zipName
    Write-Check (Test-Path -LiteralPath $zip1) "the file is called $zipName"
    Write-Check (@(Get-ChildItem -LiteralPath $out1 -File | ForEach-Object { $_.Name } | Sort-Object).Count -eq 2) 'beside it only SHA256SUMS'
    Test-Archive $zip1 $BuildDir $version 'the release zip'
    $dir1 = $script:archiveDir
    $sums = (Get-Content -LiteralPath (Join-Path $out1 'SHA256SUMS') -Raw)
    Write-Check ($sums -ceq ("$(Get-Sha $zip1)  $zipName`n")) 'SHA256SUMS names the zip with its real SHA-256 ("<hash>  <file>")'
    Write-Check (Test-ExtensionComplete (Join-Path $dir1 'extension')) 'every file the manifest names is in the unpacked extension'

    # 2. The same programs give the same bytes.
    $out2 = Join-Path $tmp 'dist two'
    # Zip times have a resolution of two seconds: wait longer, so a zip that took the clock would differ.
    Start-Sleep -Seconds 3
    $null = Invoke-Package @{ BuildDir = $BuildDir; OutDir = $out2 }
    Write-Check ((Get-Sha $zip1) -eq (Get-Sha (Join-Path $out2 $zipName))) 'packing twice gives the same zip, byte for byte'

    # 3. The rolling build: a fixed name, the short commit in the version label.
    $sha = '0123456789abcdef0123456789abcdef01234567'
    $out3 = Join-Path $tmp 'dist rolling'
    $r = Invoke-Package @{ BuildDir = $BuildDir; OutDir = $out3; Rolling = $true; Sha = $sha }
    $zip3 = Join-Path $out3 'streamtap-windows-x86_64.zip'
    Write-Check ($r.Ok -and (Test-Path -LiteralPath $zip3)) 'package the rolling build under its fixed name'
    if ($r.Ok) { Test-Archive $zip3 $BuildDir "$version+0123456" 'the rolling zip' }
    $r = Invoke-Package @{ BuildDir = $BuildDir; OutDir = $out3; Rolling = $true }
    Write-Check (-not $r.Ok -and $r.Text -match 'needs -Sha') 'a rolling build without a commit is refused'
    $r = Invoke-Package @{ BuildDir = $BuildDir; OutDir = $out3; Rolling = $true; Sha = 'ABCDEF1' }
    Write-Check (-not $r.Ok -and $r.Text -match 'lower case hex') 'and so is a commit that is not lower case hex'
    $r = Invoke-Package @{ BuildDir = $BuildDir; OutDir = $out3; Sha = $sha }
    Write-Check (-not $r.Ok -and $r.Text -match 'belongs to -Rolling') 'a commit on a release is refused'

    # 4. The version a release says it is.
    $r = Invoke-Package @{ BuildDir = $BuildDir; OutDir = (Join-Path $tmp 'dist v'); Version = "v$version" }
    Write-Check $r.Ok 'the tag form vX.Y.Z of the current version is accepted'
    $r = Invoke-Package @{ BuildDir = $BuildDir; OutDir = (Join-Path $tmp 'dist v2'); Version = '9.9.9' }
    Write-Check (-not $r.Ok -and $r.Text -match 'Cargo.toml says') 'a version that is not the one of Cargo.toml is refused'
    Write-Check (-not (Test-Path -LiteralPath (Join-Path $tmp 'dist v2'))) 'and nothing is written for it'
    $fake = Copy-FakeRoot 'fake tauri'
    $conf = Join-Path $fake 'desktop\crates\streamtap-app\tauri.conf.json'
    [IO.File]::WriteAllText($conf, ([IO.File]::ReadAllText($conf) -replace '"version":\s*"[^"]+"', '"version": "9.9.9"'), (New-Object Text.UTF8Encoding($false)))
    $r = Invoke-Package @{ BuildDir = $BuildDir; OutDir = (Join-Path $tmp 'dist t') } (Join-Path $fake 'scripts\package-release.ps1')
    Write-Check (-not $r.Ok -and $r.Text -match 'tauri.conf.json says') 'Cargo.toml and tauri.conf.json that disagree are refused'

    # 5. Programs that are not there.
    $partial = Join-Path $tmp 'partial'
    New-Item -ItemType Directory -Path $partial | Out-Null
    Copy-Item -Path (Join-Path $BuildDir '*.exe') -Destination $partial
    Remove-Item -LiteralPath (Join-Path $partial 'streamtap-host.exe')
    $r = Invoke-Package @{ BuildDir = $partial; OutDir = (Join-Path $tmp 'dist p') }
    Write-Check (-not $r.Ok -and $r.Text -match 'streamtap-host.exe') 'a missing program is refused, and named'
    Set-Content -LiteralPath (Join-Path $partial 'streamtap-host.exe') -Value '' -NoNewline
    $r = Invoke-Package @{ BuildDir = $partial; OutDir = (Join-Path $tmp 'dist p') }
    Write-Check (-not $r.Ok -and $r.Text -match 'is empty') 'an empty program is refused'

    # 6. What the extension may hold.
    $fake = Copy-FakeRoot 'fake extension'
    $ext = Join-Path $fake 'extension'
    $pkg = Join-Path $fake 'scripts\package-release.ps1'
    Set-Content -LiteralPath (Join-Path $ext 'src\private.pem') -Value 'key'
    $r = Invoke-Package @{ BuildDir = $BuildDir; OutDir = (Join-Path $tmp 'dist e1') } $pkg
    Write-Check (-not $r.Ok -and $r.Text -match 'key or package file') 'a key file in the extension is not shipped'
    Remove-Item -LiteralPath (Join-Path $ext 'src\private.pem')
    Set-Content -LiteralPath (Join-Path $ext 'src\tool.exe') -Value 'x'
    $r = Invoke-Package @{ BuildDir = $BuildDir; OutDir = (Join-Path $tmp 'dist e2') } $pkg
    Write-Check (-not $r.Ok -and $r.Text -match 'unexpected type') 'a file of an unexpected type in the extension is not shipped'
    Remove-Item -LiteralPath (Join-Path $ext 'src\tool.exe')
    Remove-Item -LiteralPath (Join-Path $ext 'src\background.js')
    $r = Invoke-Package @{ BuildDir = $BuildDir; OutDir = (Join-Path $tmp 'dist e3') } $pkg
    Write-Check (-not $r.Ok -and $r.Text -match "names 'src/background.js'") 'a manifest that names a file that is not there is refused'
    Copy-Item -LiteralPath (Join-Path $root 'extension\src\background.js') -Destination (Join-Path $ext 'src\background.js')
    Remove-Item -LiteralPath (Join-Path $ext 'popup\popup.js')
    $r = Invoke-Package @{ BuildDir = $BuildDir; OutDir = (Join-Path $tmp 'dist e4') } $pkg
    Write-Check (-not $r.Ok -and $r.Text -match 'popup.js') 'and so is a popup whose script is not there'
    Copy-Item -LiteralPath (Join-Path $root 'extension\popup\popup.js') -Destination (Join-Path $ext 'popup\popup.js')
    $r = Invoke-Package @{ BuildDir = $BuildDir; OutDir = (Join-Path $tmp 'dist e5') } $pkg
    Write-Check $r.Ok 'the fake root packs when nothing is broken (the refusals above are the breaks)'

    # 6b. Everything the manifest names, not just the scripts and the popup, must be in the folder.
    function Test-ExtensionVariant([string] $Name, [scriptblock] $Edit, [string] $Pattern, [string] $Title) {
        $fake = Copy-FakeRoot $Name
        & $Edit (Join-Path $fake 'extension')
        $r = Invoke-Package @{ BuildDir = $BuildDir; OutDir = (Join-Path $tmp "dist $Name") } (Join-Path $fake 'scripts\package-release.ps1')
        if ($Pattern) { Write-Check (-not $r.Ok -and $r.Text -match $Pattern) $Title }
        else { Write-Check $r.Ok $Title; if (-not $r.Ok) { Write-Output $r.Text } }
    }
    function Edit-Text([string] $Path, [string] $Old, [string] $New) {
        $text = [IO.File]::ReadAllText($Path)
        if (-not $text.Contains($Old)) { throw "no '$Old' in $Path" }
        [IO.File]::WriteAllText($Path, $text.Replace($Old, $New), (New-Object Text.UTF8Encoding($false)))
    }
    Test-ExtensionVariant 'ext icons' { param($e) Edit-Text (Join-Path $e 'manifest.json') '"manifest_version": 3,' '"manifest_version": 3, "icons": {"16": "icons/16.png"},' } "names 'icons/16.png'" 'an icon the manifest names but the folder lacks is refused'
    Test-ExtensionVariant 'ext default icon' { param($e) Edit-Text (Join-Path $e 'manifest.json') '"default_title": "streamtap",' '"default_title": "streamtap", "default_icon": "icons/a.png",' } "names 'icons/a.png'" 'so is the action icon'
    Test-ExtensionVariant 'ext css' { param($e) Edit-Text (Join-Path $e 'manifest.json') '"src/hook.js"' '"src/hook.js"], "css": ["styles/missing.css"' } "names 'styles/missing.css'" 'so is a content script style'
    Test-ExtensionVariant 'ext war' { param($e) Edit-Text (Join-Path $e 'manifest.json') '"manifest_version": 3,' '"manifest_version": 3, "web_accessible_resources": [{"resources": ["assets/x.js"], "matches": ["<all_urls>"]}],' } "names 'assets/x.js'" 'so is a web accessible resource'
    Test-ExtensionVariant 'ext war glob' { param($e) Edit-Text (Join-Path $e 'manifest.json') '"manifest_version": 3,' '"manifest_version": 3, "web_accessible_resources": [{"resources": ["assets/*"], "matches": ["<all_urls>"]}],' } 'pattern .assets/\*.' 'and a pattern that points outside popup and src'
    Test-ExtensionVariant 'ext locale' { param($e) Edit-Text (Join-Path $e 'manifest.json') '"manifest_version": 3,' '"manifest_version": 3, "default_locale": "en",' } 'default_locale' 'a default locale (its _locales is not shipped) is refused'
    Test-ExtensionVariant 'ext import' { param($e) Edit-Text (Join-Path $e 'src\background.js') '"native-pairing.js"' '"missing-pairing.js"' } 'missing-pairing\.js' 'a script the service worker imports but the folder lacks is refused'
    Test-ExtensionVariant 'ext icon ok' {
        param($e)
        Set-Content -LiteralPath (Join-Path $e 'src\icon.png') -Value 'png'
        Edit-Text (Join-Path $e 'manifest.json') '"manifest_version": 3,' '"manifest_version": 3, "icons": {"16": "src/icon.png"},'
    } '' 'an icon inside src packs'

    # 6c. The sink and the host are the app's version too.
    foreach ($member in 'streamtap-sink', 'streamtap-host') {
        $fake = Copy-FakeRoot "fake $member"
        $toml = Join-Path $fake "desktop\crates\$member\Cargo.toml"
        [IO.File]::WriteAllText($toml, ([IO.File]::ReadAllText($toml) -replace '(?m)^version\s*=\s*"[^"]+"', 'version = "9.9.9"'), (New-Object Text.UTF8Encoding($false)))
        $r = Invoke-Package @{ BuildDir = $BuildDir; OutDir = (Join-Path $tmp "dist $member") } (Join-Path $fake 'scripts\package-release.ps1')
        Write-Check (-not $r.Ok -and $r.Text -match "$member/Cargo.toml says 9.9.9") "a $member with another version than the app is refused"
    }

    # 6d. SHA256SUMS keeps one line per zip of the folder, and never the hash of a zip that was not made.
    $both = Join-Path $tmp 'dist both'
    $null = Invoke-Package @{ BuildDir = $BuildDir; OutDir = $both }
    $null = Invoke-Package @{ BuildDir = $BuildDir; OutDir = $both; Rolling = $true; Sha = $sha }
    $null = Invoke-Package @{ BuildDir = $BuildDir; OutDir = $both; Rolling = $true; Sha = $sha }
    $lines = @((Get-Content -LiteralPath (Join-Path $both 'SHA256SUMS')) | Where-Object { $_ })
    $want = @("$(Get-Sha (Join-Path $both 'streamtap-windows-x86_64.zip'))  streamtap-windows-x86_64.zip", "$(Get-Sha (Join-Path $both $zipName))  $zipName") | Sort-Object { $_.Substring(66) }
    Write-Check (($lines -join '|') -ceq ($want -join '|')) 'a release and a rolling zip in one folder give two lines in SHA256SUMS (packing again replaces a line)'
    $locked = [IO.File]::Open((Join-Path $both $zipName), [IO.FileMode]::Open, [IO.FileAccess]::Read, [IO.FileShare]::None)
    try { $r = Invoke-Package @{ BuildDir = $BuildDir; OutDir = $both } }
    finally { $locked.Dispose() }
    $left = (Get-Content -LiteralPath (Join-Path $both 'SHA256SUMS') -Raw)
    Write-Check (-not $r.Ok -and $left -notmatch [regex]::Escape($zipName) -and $left -match 'streamtap-windows-x86_64.zip') 'a zip that cannot be written leaves no hash of its old self in SHA256SUMS'

    # 7. -StageDir / -NoZip, as build-install.ps1 uses them.
    $stage = Join-Path $tmp 'stage'
    $r = Invoke-Package @{ BuildDir = $BuildDir; StageDir = $stage; NoZip = $true }
    Write-Check ($r.Ok -and (Test-Path -LiteralPath (Join-Path $stage 'bin\streamtap-app.exe')) -and -not (Test-Path -LiteralPath (Join-Path $stage 'SHA256SUMS'))) '-NoZip stages the folder and makes no zip'
    $r = Invoke-Package @{ BuildDir = $BuildDir; StageDir = $stage; NoZip = $true }
    Write-Check (-not $r.Ok -and $r.Text -match 'not empty') 'a stage folder that holds something is refused'
    $r = Invoke-Package @{ BuildDir = $BuildDir; NoZip = $true }
    Write-Check (-not $r.Ok -and $r.Text -match 'give -StageDir') '-NoZip without a stage folder is refused'

    # 8. The unpacked zip installs and uninstalls (Windows PowerShell 5.1 runs the install: the one Windows has).
    $places = @('-InstallDir', $install, '-StartMenuDir', $startMenu, '-RegistryRoot', $regRoot)
    $r = Invoke-Child (Join-Path $dir1 'install.ps1') $places -Shell $windowsPowerShell
    Write-Check ($r.Code -eq 0) 'install.ps1 of the unpacked zip installs'
    if ($r.Code -ne 0) { Write-Output $r.Text }
    $installedTree = @(Get-ChildItem -LiteralPath $install -Recurse -File | ForEach-Object { $_.FullName.Substring($install.Length + 1).Replace('\', '/') } | Sort-Object)
    $zipTree = @(Get-ZipEntryName $zip1 | Where-Object { $_ -cne 'install.ps1' } | Sort-Object)
    Write-Check (($installedTree -join '|') -ceq ($zipTree -join '|')) 'the install holds exactly the zip but install.ps1'
    $entry = Get-ItemProperty -LiteralPath "$regRoot\Microsoft\Windows\CurrentVersion\Uninstall\streamtap" -ErrorAction SilentlyContinue
    Write-Check ($null -ne $entry -and $entry.DisplayVersion -eq $version -and $entry.InstallLocation -eq $install) 'the Settings > Apps entry says the version of the zip'
    Write-Check (Test-ExtensionComplete (Join-Path $install 'extension')) 'the installed extension is complete: Load unpacked can take it'
    $r = Invoke-Child (Join-Path $install 'uninstall.ps1') $places
    Write-Check ($r.Code -eq 0 -and -not (Test-Path -LiteralPath $install) -and -not (Test-Path -LiteralPath (Join-Path $startMenu 'streamtap.lnk'))) 'the installed uninstall.ps1 removes it all'
    Write-Check (-not (Test-Path -LiteralPath "$regRoot\Microsoft\Windows\CurrentVersion\Uninstall\streamtap")) 'and the Settings > Apps entry'

    # 9. The zip the workflow is about to publish.
    if ($Zip) {
        if (-not (Test-Path -LiteralPath $Zip -PathType Leaf)) { throw "no such zip: $Zip" }
        if (-not $Label) { $Label = (Read-ZipEntry $Zip 'VERSION').Trim() }
        Test-Archive $Zip $BuildDir $Label "the zip $(Split-Path -Leaf $Zip)"
    }
}
finally {
    foreach ($name in $savedEnv.Keys) { [Environment]::SetEnvironmentVariable($name, $savedEnv[$name]) }
    if (Test-Path -LiteralPath $regRoot) { Remove-Item -LiteralPath $regRoot -Recurse -Force }
    for ($attempt = 1; Test-Path -LiteralPath $tmp; $attempt++) {
        try {
            Remove-Item -LiteralPath $tmp -Recurse -Force -ErrorAction Stop
        }
        catch {
            if ($attempt -ge 10) {
                Write-Output "FAIL could not remove $tmp : $($_.Exception.Message)"
                $script:failures++
                break
            }
            Start-Sleep -Milliseconds 200
        }
    }
}

if ($script:failures -ne 0) {
    Write-Output ''
    Write-Output "$($script:failures) check(s) failed"
    exit 1
}
Write-Output ''
Write-Output 'all checks passed'
