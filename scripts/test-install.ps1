<#
.SYNOPSIS
Test scripts/build-install.ps1 and scripts/uninstall.ps1 in a temporary folder, without a build.

.DESCRIPTION
Everything happens in places of the test's own: the install folder, the Start menu folder and the
data folders lie in a temporary folder, and the registry root (for the Settings > Apps entry and
the browsers' native messaging keys) is a throw-away key HKCU:\Software\streamtap-test-install-<id>
that the test removes. The real Start menu, the real uninstall entries and the browsers' keys are
never touched; nothing is built (-BuildDir). The stand-ins for the three programs are copies of
cmd.exe, so one of them can run from the install folder for the "refuses while it runs" checks.
Exit 0 when all checks pass.

.PARAMETER BuildDir
A folder with streamtap-app.exe, streamtap-sink.exe and streamtap-host.exe to install (default:
the stand-ins).
#>
[CmdletBinding()]
param(
    [string] $BuildDir = ''
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$here = $PSScriptRoot
$id = [Guid]::NewGuid().ToString('N')
$tmp = Join-Path ([IO.Path]::GetTempPath()) ("streamtap-test-install-$id")
$regRoot = "HKCU:\Software\streamtap-test-install-$id"
New-Item -ItemType Directory -Path $tmp | Out-Null

$install = Join-Path $tmp 'install'
$startMenu = Join-Path $tmp 'start-menu'
$dataDir = Join-Path $tmp 'roaming\io.github.wkdsteve.streamtap'
$localDataDir = Join-Path $tmp 'local\io.github.wkdsteve.streamtap'
$hostName = 'io.github.wkdsteve.streamtap'
$places = @('-InstallDir', $install, '-StartMenuDir', $startMenu, '-RegistryRoot', $regRoot)
$dataPlaces = @('-DataDir', $dataDir, '-LocalDataDir', $localDataDir)

# The defaults of the scripts point at the same places, so that a call without them (or a script that
# loses a parameter) still never reaches the real folders or keys of this account.
$envPlaces = [ordered]@{
    STREAMTAP_INSTALL_DIR    = $install
    STREAMTAP_START_MENU_DIR = $startMenu
    STREAMTAP_REGISTRY_ROOT  = $regRoot
    STREAMTAP_DATA_DIR       = $dataDir
    STREAMTAP_LOCAL_DATA_DIR = $localDataDir
}
$savedEnv = @{}
foreach ($name in $envPlaces.Keys) {
    $savedEnv[$name] = [Environment]::GetEnvironmentVariable($name)
    [Environment]::SetEnvironmentVariable($name, $envPlaces[$name])
}

$script:failures = 0
function Write-Check([bool] $Passed, [string] $Name) {
    if ($Passed) { Write-Output "ok   $Name" }
    else { Write-Output "FAIL $Name"; $script:failures++ }
}

$identity = [Security.Principal.WindowsIdentity]::GetCurrent()
$elevated = ([Security.Principal.WindowsPrincipal] $identity).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)

# Run a script in a child PowerShell (its exit and its output are the answer). The children run
# without CLAUDECODE (the places are all the test's own), unless -Claude asks for it.
function Invoke-Script([string] $Path, [string[]] $Arguments, [switch] $Claude) {
    $shell = (Get-Process -Id $PID).Path
    $extra = @()
    if ($elevated) { $extra = @('-AllowElevated') }
    $saved = $env:CLAUDECODE
    # Windows PowerShell 5.1 turns the child's stderr into error records: text to read here, not errors.
    $ErrorActionPreference = 'Continue'
    try {
        if ($Claude) { $env:CLAUDECODE = '1' } else { $env:CLAUDECODE = $null }
        $output = & $shell -NoProfile -NonInteractive -ExecutionPolicy Bypass -File $Path @Arguments @extra 2>&1 | Out-String
        $code = $LASTEXITCODE
    }
    finally {
        $env:CLAUDECODE = $saved
    }
    return [pscustomobject]@{ Code = $code; Text = $output }
}

function Get-KeyPath([string] $Vendor) {
    "$regRoot\$Vendor\NativeMessagingHosts\$hostName"
}

# A browser registration the way the app writes it: a manifest in the data folder, a key that names it.
function Add-Registration([string] $Browser, [string] $Vendor, [string] $HostExe) {
    $folder = Join-Path $dataDir 'native-messaging'
    New-Item -ItemType Directory -Force -Path $folder | Out-Null
    $manifest = Join-Path $folder "$Browser.json"
    $json = [ordered]@{ name = $hostName; path = $HostExe; type = 'stdio'; allowed_origins = @('chrome-extension://oggpddakdbffbjhkjmcakidgmhpgakia/') }
    Set-Content -LiteralPath $manifest -Value ($json | ConvertTo-Json) -Encoding UTF8
    $key = Get-KeyPath $Vendor
    New-Item -Path $key -Force | Out-Null
    Set-Item -LiteralPath $key -Value $manifest
    return $manifest
}

$runner = $null
try {
    if ($BuildDir -eq '') {
        $BuildDir = Join-Path $tmp 'build'
        New-Item -ItemType Directory -Path $BuildDir | Out-Null
        foreach ($name in 'streamtap-app', 'streamtap-sink', 'streamtap-host') {
            Copy-Item -LiteralPath (Join-Path $env:SystemRoot 'System32\cmd.exe') -Destination (Join-Path $BuildDir "$name.exe")
        }
    }
    $buildInstall = Join-Path $here 'build-install.ps1'
    $uninstallSrc = Join-Path $here 'uninstall.ps1'
    $installArgs = @('-BuildDir', $BuildDir) + $places
    $uninstallKey = "$regRoot\Microsoft\Windows\CurrentVersion\Uninstall\streamtap"
    $link = Join-Path $startMenu 'streamtap.lnk'
    $config = Join-Path $dataDir 'config.json'
    # The data as the app leaves it: its files, a capture in the default captures folder, a file of yours.
    $capture = Join-Path $dataDir 'captures\1700000000_abc'
    $webView = Join-Path $localDataDir 'EBWebView'
    New-Item -ItemType Directory -Force -Path $capture, $webView | Out-Null
    Set-Content -LiteralPath $config -Value '{"token":"keep-me"}'
    foreach ($name in 'pairings.json', 'bridge.json') { Set-Content -LiteralPath (Join-Path $dataDir $name) -Value '{}' }
    Set-Content -LiteralPath (Join-Path $capture 'out.mp4') -Value 'video'
    Set-Content -LiteralPath (Join-Path $dataDir 'notes.txt') -Value 'mine'
    Set-Content -LiteralPath (Join-Path $webView 'cache.dat') -Value 'x'

    # 1. A Claude session is refused, before anything is written.
    $r = Invoke-Script $buildInstall $installArgs -Claude
    Write-Check ($r.Code -ne 0 -and $r.Text -match 'Claude session') 'install in a Claude session is refused, and says why'
    $r = Invoke-Script $uninstallSrc $places -Claude
    Write-Check ($r.Code -ne 0 -and $r.Text -match 'Claude session') 'uninstall in a Claude session is refused'
    Write-Check (-not (Test-Path -LiteralPath $install)) 'nothing was installed by them'

    # 2. A browser registered from a checkout before the install.
    $checkoutHost = Join-Path $tmp 'checkout\streamtap-host.exe'
    $chromeManifest = Add-Registration 'chrome' 'Google\Chrome' $checkoutHost

    # 3. Install.
    $r = Invoke-Script $buildInstall $installArgs
    Write-Check ($r.Code -eq 0) 'install'
    if ($r.Code -ne 0) { Write-Output $r.Text }
    foreach ($name in 'streamtap-app.exe', 'streamtap-sink.exe', 'streamtap-host.exe') {
        $dest = Join-Path $install $name
        $same = (Test-Path -LiteralPath $dest) -and
            ((Get-FileHash -LiteralPath $dest).Hash -eq (Get-FileHash -LiteralPath (Join-Path $BuildDir $name)).Hash)
        Write-Check $same "$name is installed"
    }
    Write-Check ((Get-FileHash -LiteralPath (Join-Path $install 'uninstall.ps1')).Hash -eq (Get-FileHash -LiteralPath $uninstallSrc).Hash) 'uninstall.ps1 is installed beside them'
    Write-Check (-not (Get-ChildItem -LiteralPath $install -Filter '*.new')) 'no .new file is left'
    $shell = New-Object -ComObject WScript.Shell
    Write-Check ((Test-Path -LiteralPath $link) -and $shell.CreateShortcut($link).TargetPath -eq (Join-Path $install 'streamtap-app.exe')) 'the Start menu entry starts the installed app'
    $entry = Get-ItemProperty -LiteralPath $uninstallKey -ErrorAction SilentlyContinue
    $version = (Get-Content -LiteralPath (Join-Path (Split-Path -Parent $here) 'desktop\crates\streamtap-app\tauri.conf.json') -Raw | ConvertFrom-Json).version
    Write-Check ($null -ne $entry -and $entry.DisplayName -eq 'streamtap' -and $entry.DisplayVersion -eq $version -and $entry.InstallLocation -eq $install) 'the Settings > Apps entry names streamtap, its version and the folder'
    Write-Check ($null -ne $entry -and $entry.UninstallString -like "*$(Join-Path $install 'uninstall.ps1')*" -and $entry.UninstallString -like "*-RegistryRoot `"$regRoot`"*") 'the entry runs the installed uninstall.ps1 with the places of this install'
    Write-Check ((Get-Content -LiteralPath $config -Raw) -match 'keep-me') 'the settings are untouched'
    Write-Check ($r.Text -match 'Chrome: starts .*checkout' -and $r.Text -match 'Register again') 'a browser that starts the checkout''s host is named, with what to do'

    # 4. A browser registered for the installed app is not named; installing again replaces the files.
    $edgeManifest = Add-Registration 'edge' 'Microsoft\Edge' (Join-Path $install 'streamtap-host.exe')
    $r = Invoke-Script $buildInstall $installArgs
    Write-Check ($r.Code -eq 0) 'install again'
    Write-Check ($r.Text -notmatch 'Edge:' -and $r.Text -match 'Chrome:') 'only the browser of the other copy is named'
    Write-Check (@(Get-ChildItem -LiteralPath $install).Count -eq 4) 'still the three programs and uninstall.ps1'

    # 5. A program running from the install folder: both scripts refuse, name it, change nothing.
    $runner = Start-Process -FilePath (Join-Path $install 'streamtap-sink.exe') -ArgumentList '/d', '/k' -WindowStyle Hidden -PassThru
    Start-Sleep -Milliseconds 300
    $r = Invoke-Script $buildInstall $installArgs
    Write-Check ($r.Code -ne 0 -and $r.Text -match "streamtap-sink \(PID $($runner.Id)\)") 'install refuses while the sink runs from there, and names the PID'
    $r = Invoke-Script $uninstallSrc $places
    Write-Check ($r.Code -ne 0 -and $r.Text -match "PID $($runner.Id)") 'uninstall refuses while the sink runs from there'
    Write-Check ((Test-Path -LiteralPath (Join-Path $install 'streamtap-app.exe')) -and (Test-Path -LiteralPath $link) -and (Test-Path -LiteralPath (Get-KeyPath 'Microsoft\Edge'))) 'and nothing was removed'
    Stop-Process -Id $runner.Id -Force
    $runner.WaitForExit()
    $runner = $null

    # 6. The installed copy of uninstall.ps1 (what Settings > Apps runs) removes the install and itself.
    $r = Invoke-Script (Join-Path $install 'uninstall.ps1') $places
    Write-Check ($r.Code -eq 0) 'uninstall with the installed copy'
    if ($r.Code -ne 0) { Write-Output $r.Text }
    Write-Check (-not (Test-Path -LiteralPath $install)) 'the install folder is gone, the script with it'
    Write-Check (-not (Test-Path -LiteralPath $link)) 'the Start menu entry is gone'
    Write-Check (-not (Test-Path -LiteralPath $uninstallKey)) 'the Settings > Apps entry is gone'
    Write-Check (-not (Test-Path -LiteralPath (Get-KeyPath 'Microsoft\Edge')) -and -not (Test-Path -LiteralPath $edgeManifest)) 'the registration of the installed host is gone, its manifest too'
    Write-Check ((Test-Path -LiteralPath (Get-KeyPath 'Google\Chrome')) -and (Test-Path -LiteralPath $chromeManifest)) 'the registration of the checkout stays'
    Write-Check ((Get-Content -LiteralPath $config -Raw) -match 'keep-me' -and (Test-Path -LiteralPath (Join-Path $webView 'cache.dat'))) 'the settings and the WebView data stay'

    # 7. Again, with no parameters at all (the places come from the environment): nothing to do, not an error.
    $r = Invoke-Script $uninstallSrc @()
    Write-Check ($r.Code -eq 0 -and $r.Text -match 'nothing to do' -and $r.Text -match [regex]::Escape($install)) 'uninstall again says there is nothing to do (places from the environment)'

    # 8. A file that is not the installer's keeps the folder.
    $r = Invoke-Script $buildInstall $installArgs
    Write-Check ($r.Code -eq 0) 'install before the foreign file'
    Set-Content -LiteralPath (Join-Path $install 'mine.txt') -Value 'mine'
    $r = Invoke-Script $uninstallSrc $places
    Write-Check ($r.Code -eq 0 -and (Test-Path -LiteralPath (Join-Path $install 'mine.txt')) -and -not (Test-Path -LiteralPath (Join-Path $install 'streamtap-app.exe'))) 'the foreign file and so the folder stay, the programs go'
    Remove-Item -LiteralPath (Join-Path $install 'mine.txt')

    # 9. A program file that cannot be removed is an error, not a "removed", and the entry stays.
    $r = Invoke-Script $buildInstall $installArgs
    $lock = [IO.File]::Open((Join-Path $install 'streamtap-host.exe'), [IO.FileMode]::Open, [IO.FileAccess]::Read, [IO.FileShare]::Read)
    try {
        $r = Invoke-Script $uninstallSrc $places
        Write-Check ($r.Code -ne 0 -and $r.Text -notmatch '(?m)^removed ') 'a file that cannot be removed is an error'
        Write-Check (Test-Path -LiteralPath $uninstallKey) 'and the Settings > Apps entry stays for the next run'
    }
    finally {
        $lock.Dispose()
    }

    # 10. -RemoveData takes the app's own files and the keys that lead to them, never a capture or a file of yours.
    $r = Invoke-Script $uninstallSrc ($places + $dataPlaces + @('-RemoveData'))
    Write-Check ($r.Code -eq 0) 'uninstall -RemoveData'
    if ($r.Code -ne 0) { Write-Output $r.Text }
    $appFiles = @('config.json', 'pairings.json', 'bridge.json', 'native-messaging') | Where-Object { Test-Path -LiteralPath (Join-Path $dataDir $_) }
    Write-Check (-not $appFiles) 'the settings, pairings, bridge file and host manifests are gone'
    Write-Check (-not (Test-Path -LiteralPath $localDataDir)) 'the WebView data is gone'
    $video = Join-Path $capture 'out.mp4'
    Write-Check ((Test-Path -LiteralPath $video) -and (Get-Content -LiteralPath $video -Raw) -match 'video') 'the capture in the default captures folder stays'
    Write-Check (Test-Path -LiteralPath (Join-Path $dataDir 'notes.txt')) 'a file that is not the app''s stays'
    Write-Check (-not (Test-Path -LiteralPath (Get-KeyPath 'Google\Chrome'))) 'the key that led to a removed manifest is gone'
    Write-Check (-not (Test-Path -LiteralPath $install) -and -not (Test-Path -LiteralPath $uninstallKey)) 'the install is gone'
}
finally {
    foreach ($name in $savedEnv.Keys) {
        [Environment]::SetEnvironmentVariable($name, $savedEnv[$name])
    }
    if ($runner) { Stop-Process -Id $runner.Id -Force -ErrorAction SilentlyContinue; $runner.WaitForExit() }
    if (Test-Path -LiteralPath $regRoot) { Remove-Item -LiteralPath $regRoot -Recurse -Force }
    # A folder that stays is a failure of the run, not a silent leftover (a file held for a moment is tried again).
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
