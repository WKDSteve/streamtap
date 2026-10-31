<#
.SYNOPSIS
Test scripts/install.ps1, scripts/build-install.ps1 and scripts/uninstall.ps1 in a temporary folder, without a build.

.DESCRIPTION
install.ps1 is the one installer (the release zip's and, through a staged build, build-install.ps1's);
most checks run it on a payload staged by package-release.ps1 -NoZip, one install runs the whole
build-install.ps1 -BuildDir path.

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
# A space and a non-ASCII letter in every place: the quoting of the Settings > Apps command line and
# the reading of the app's UTF-8 manifests in Windows PowerShell 5.1 are checked with them.
$tmp = Join-Path ([IO.Path]::GetTempPath()) ("streamtap test-install $([char]0xFC)-$id")
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
# loses a parameter) still never reaches the real folders or keys of this account. The first three
# end in a backslash, as a user may type them: the command line of the Settings > Apps entry must
# still hold (they come through the environment because Windows PowerShell 5.1 would itself break a
# quoted argument that ends in a backslash when it starts the child).
$envPlaces = [ordered]@{
    STREAMTAP_INSTALL_DIR    = "$install\"
    STREAMTAP_START_MENU_DIR = "$startMenu\"
    STREAMTAP_REGISTRY_ROOT  = "$regRoot\"
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
$windowsPowerShell = Join-Path $env:SystemRoot 'System32\WindowsPowerShell\v1.0\powershell.exe'
function Invoke-Script([string] $Path, [string[]] $Arguments, [switch] $Claude, [string] $Shell = '', [string] $PathEnv = '') {
    if (-not $Shell) { $Shell = (Get-Process -Id $PID).Path }
    $extra = @()
    if ($elevated) { $extra = @('-AllowElevated') }
    $saved = $env:CLAUDECODE
    $savedPath = $env:PATH
    # Windows PowerShell 5.1 turns the child's stderr into error records: text to read here, not errors.
    $ErrorActionPreference = 'Continue'
    try {
        if ($Claude) { $env:CLAUDECODE = '1' } else { $env:CLAUDECODE = $null }
        if ($PathEnv) { $env:PATH = $PathEnv }
        $output = & $Shell -NoProfile -NonInteractive -ExecutionPolicy Bypass -File $Path @Arguments @extra 2>&1 | Out-String
        $code = $LASTEXITCODE
    }
    finally {
        $env:CLAUDECODE = $saved
        $env:PATH = $savedPath
    }
    return [pscustomobject]@{ Code = $code; Text = $output }
}

# Every file below $Dir, relative, sorted: the shape of an install or a payload.
function Get-Tree([string] $Dir) {
    $prefix = (Resolve-Path -LiteralPath $Dir).ProviderPath.TrimEnd('\') + '\'
    @(Get-ChildItem -LiteralPath $Dir -Recurse -File -Force | ForEach-Object { $_.FullName.Substring($prefix.Length) } | Sort-Object)
}

# Whether the programs in $Dir are the ones of $From (by hash).
function Test-SameProgram([string] $Dir, [string] $From) {
    foreach ($name in 'streamtap-app.exe', 'streamtap-sink.exe', 'streamtap-host.exe') {
        $a = Join-Path $Dir $name
        if (-not (Test-Path -LiteralPath $a)) { return $false }
        if ((Get-FileHash -LiteralPath $a).Hash -ne (Get-FileHash -LiteralPath (Join-Path $From $name)).Hash) { return $false }
    }
    return $true
}

# Run the UninstallString of the Settings > Apps entry the way Windows does: the command line as it
# stands, not split into arguments here. Enter is pressed after $EnterAfterMs (a pause on an error
# waits for it); Waited says whether the process was still there then.
function Invoke-CommandLine([string] $CommandLine, [int] $EnterAfterMs = 0, [string] $WorkDir = '') {
    if ($CommandLine -notmatch '^"([^"]+)"\s+(.*)$') { throw "not a command line with a quoted program: $CommandLine" }
    $psi = New-Object Diagnostics.ProcessStartInfo
    $psi.FileName = $Matches[1]
    $psi.Arguments = $Matches[2]
    if ($WorkDir) { $psi.WorkingDirectory = $WorkDir }
    if ($elevated) { $psi.Arguments += ' -AllowElevated' }
    $psi.UseShellExecute = $false
    $psi.RedirectStandardInput = $true
    $psi.RedirectStandardOutput = $true
    $psi.RedirectStandardError = $true
    $saved = $env:CLAUDECODE
    try {
        $env:CLAUDECODE = $null
        $p = [Diagnostics.Process]::Start($psi)
    }
    finally {
        $env:CLAUDECODE = $saved
    }
    $out = $p.StandardOutput.ReadToEndAsync()
    $err = $p.StandardError.ReadToEndAsync()
    $waited = $false
    if ($EnterAfterMs -gt 0) { $waited = -not $p.WaitForExit($EnterAfterMs) }
    # A process that has ended already closed its end of the pipe: nothing to press then.
    try {
        $p.StandardInput.WriteLine('')
        $p.StandardInput.Close()
    }
    catch [IO.IOException], [Management.Automation.MethodInvocationException] {
        Write-Verbose "the command line had ended before Enter: $($_.Exception.Message)"
    }
    if (-not $p.WaitForExit(60000)) { $p.Kill(); throw "the command line did not end: $CommandLine" }
    return [pscustomobject]@{ Code = $p.ExitCode; Text = $out.Result + $err.Result; Waited = $waited }
}

function Get-KeyPath([string] $Vendor) {
    "$regRoot\$Vendor\NativeMessagingHosts\$hostName"
}

# A browser registration the way the app writes it: a manifest in the data folder, a key that names it.
function Add-Registration([string] $Browser, [string] $Vendor, [string] $HostExe) {
    $folder = Join-Path $dataDir 'native-messaging'
    New-Item -ItemType Directory -Force -Path $folder | Out-Null
    $manifest = Join-Path $folder "$Browser.json"
    # As serde_json writes it: UTF-8 without a BOM, a non-ASCII letter as itself (not as the
    # escape backslash-u-00fc, which ConvertTo-Json may write), only the backslashes escaped.
    $escaped = $HostExe.Replace('\', '\\')
    $json = "{`n  `"name`": `"$hostName`",`n  `"path`": `"$escaped`",`n  `"type`": `"stdio`",`n" +
    "  `"allowed_origins`": [`"chrome-extension://oggpddakdbffbjhkjmcakidgmhpgakia/`"]`n}"
    [IO.File]::WriteAllText($manifest, $json, (New-Object Text.UTF8Encoding($false)))
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
    $installer = Join-Path $here 'install.ps1'
    $packager = Join-Path $here 'package-release.ps1'
    $uninstallSrc = Join-Path $here 'uninstall.ps1'
    # The payload the installer takes: the shape of the release zip, staged from the stand-ins.
    $payload = Join-Path $tmp 'payload'
    & $packager -BuildDir $BuildDir -StageDir $payload -NoZip | Out-Null
    # The places from the environment, which has them with trailing backslashes (see above).
    $installArgs = @('-PayloadDir', $payload)
    $buildInstallArgs = @('-BuildDir', $BuildDir)
    $bin = Join-Path $install 'bin'
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
    $r = Invoke-Script $buildInstall $buildInstallArgs -Claude
    Write-Check ($r.Code -ne 0 -and $r.Text -match 'Claude session') 'build-install in a Claude session is refused, and says why'
    $r = Invoke-Script $installer $installArgs -Claude
    Write-Check ($r.Code -ne 0 -and $r.Text -match 'Claude session') 'install.ps1 in a Claude session is refused, and says why'
    $r = Invoke-Script $uninstallSrc $places -Claude
    Write-Check ($r.Code -ne 0 -and $r.Text -match 'Claude session') 'uninstall in a Claude session is refused'
    Write-Check (-not (Test-Path -LiteralPath $install)) 'nothing was installed by them'

    # 1b. Places that are no places: a drive root (a letter no drive has, so a script that took it
    # would find nothing there) and a registry root outside HKCU. Refused, and said why.
    $used = @([IO.DriveInfo]::GetDrives() | ForEach-Object { $_.Name.Substring(0, 1).ToUpperInvariant() })
    $free = @([char[]]'QRSTUVWXYZ' | Where-Object { $used -notcontains [string]$_ })[0]
    $driveRoot = "${free}:\"
    $r = Invoke-Script $installer @('-PayloadDir', $payload, '-InstallDir', $driveRoot, '-StartMenuDir', $startMenu, '-RegistryRoot', $regRoot)
    Write-Check ($r.Code -ne 0 -and $r.Text -match 'drive root') 'install refuses a drive root as the install folder'
    $r = Invoke-Script $uninstallSrc @('-InstallDir', $driveRoot, '-StartMenuDir', $startMenu, '-RegistryRoot', $regRoot)
    Write-Check ($r.Code -ne 0 -and $r.Text -match 'drive root') 'uninstall refuses a drive root as the install folder'
    $r = Invoke-Script $uninstallSrc @('-InstallDir', $install, '-StartMenuDir', $startMenu, '-RegistryRoot', "HKLM:\Software\streamtap-test-install-$id")
    Write-Check ($r.Code -ne 0 -and $r.Text -match 'below HKCU') 'uninstall refuses a registry root outside HKCU'

    # 1c. A folder that is not the shape of a release is refused before anything is written.
    function Copy-Payload([string] $Name) {
        $copy = Join-Path $tmp $Name
        Copy-Item -LiteralPath $payload -Destination $copy -Recurse
        return $copy
    }
    $bad = Copy-Payload 'bad-foreign'
    Set-Content -LiteralPath (Join-Path $bad 'mine.txt') -Value 'x'
    $r = Invoke-Script $installer (@('-PayloadDir', $bad) + $places)
    Write-Check ($r.Code -ne 0 -and $r.Text -match "holds 'mine.txt'") 'a payload with a foreign file at its top is refused'
    $bad = Copy-Payload 'bad-missing'
    Remove-Item -LiteralPath (Join-Path $bad 'bin\streamtap-host.exe')
    $r = Invoke-Script $installer (@('-PayloadDir', $bad) + $places)
    Write-Check ($r.Code -ne 0 -and $r.Text -match 'no bin\\streamtap-host.exe') 'a payload without one of the programs is refused'
    $bad = Copy-Payload 'bad-bin'
    Set-Content -LiteralPath (Join-Path $bad 'bin\other.exe') -Value 'x'
    $r = Invoke-Script $installer (@('-PayloadDir', $bad) + $places)
    Write-Check ($r.Code -ne 0 -and $r.Text -match "bin\\ holds 'other.exe'") 'a payload with a foreign program in bin is refused'
    $bad = Copy-Payload 'bad-version'
    Set-Content -LiteralPath (Join-Path $bad 'VERSION') -Value 'latest'
    $r = Invoke-Script $installer (@('-PayloadDir', $bad) + $places)
    Write-Check ($r.Code -ne 0 -and $r.Text -match 'not a version') 'a payload whose VERSION is no version is refused'
    $bad = Copy-Payload 'bad-extension'
    Remove-Item -LiteralPath (Join-Path $bad 'extension\manifest.json')
    $r = Invoke-Script $installer (@('-PayloadDir', $bad) + $places)
    Write-Check ($r.Code -ne 0 -and $r.Text -match 'no extension\\manifest.json') 'a payload without the extension is refused'
    $r = Invoke-Script $installer (@('-PayloadDir', $here) + $places)
    Write-Check ($r.Code -ne 0 -and $r.Text -match 'does not') 'the scripts folder of a checkout is no payload'
    Write-Check (-not (Test-Path -LiteralPath $install)) 'and none of them wrote anything'

    # 2. A browser registered from a checkout before the install.
    $checkoutHost = Join-Path $tmp 'checkout\streamtap-host.exe'
    $chromeManifest = Add-Registration 'chrome' 'Google\Chrome' $checkoutHost

    # 3. Install.
    # Through build-install.ps1 -BuildDir: the whole path (stage the build, install the stage).
    $r = Invoke-Script $buildInstall $buildInstallArgs
    Write-Check ($r.Code -eq 0) 'install (build-install.ps1 -BuildDir: stage, then install.ps1)'
    if ($r.Code -ne 0) { Write-Output $r.Text }
    Write-Check (Test-SameProgram $bin $BuildDir) 'the three programs are installed in bin\'
    Write-Check (-not (Get-ChildItem -LiteralPath $install -Filter '*.exe')) 'and none directly in the install folder'
    Write-Check ((Get-FileHash -LiteralPath (Join-Path $install 'uninstall.ps1')).Hash -eq (Get-FileHash -LiteralPath $uninstallSrc).Hash) 'uninstall.ps1 is installed beside bin'
    $extSrc = Join-Path (Split-Path -Parent $here) 'extension'
    Write-Check ((Get-FileHash -LiteralPath (Join-Path $install 'extension\manifest.json')).Hash -eq (Get-FileHash -LiteralPath (Join-Path $extSrc 'manifest.json')).Hash -and
        (Test-Path -LiteralPath (Join-Path $install 'extension\src\background.js')) -and (Test-Path -LiteralPath (Join-Path $install 'extension\popup\popup.html'))) 'the extension is installed unpacked, ready for Load unpacked'
    Write-Check (-not (Test-Path -LiteralPath (Join-Path $install 'extension\scripts'))) 'without its tests'
    Write-Check (@(Get-Tree $install | Where-Object { $_ -match '\.(new|old)$' }).Count -eq 0) 'no .new or .old file is left'
    $shell = New-Object -ComObject WScript.Shell
    Write-Check ((Test-Path -LiteralPath $link) -and $shell.CreateShortcut($link).TargetPath -eq (Join-Path $bin 'streamtap-app.exe')) 'the Start menu entry starts the installed app'
    $entry = Get-ItemProperty -LiteralPath $uninstallKey -ErrorAction SilentlyContinue
    $version = (Get-Content -LiteralPath (Join-Path (Split-Path -Parent $here) 'desktop\crates\streamtap-app\tauri.conf.json') -Raw | ConvertFrom-Json).version
    Write-Check ($null -ne $entry -and $entry.DisplayName -eq 'streamtap' -and $entry.DisplayVersion -eq $version -and $entry.InstallLocation -eq $install) 'the Settings > Apps entry names streamtap, its version and the folder'
    Write-Check ($null -ne $entry -and $entry.DisplayIcon -eq "$(Join-Path $bin 'streamtap-app.exe'),0") 'and the icon of the installed app'
    Write-Check ($null -ne $entry -and $entry.UninstallString -like "*$(Join-Path $install 'uninstall.ps1')*" -and $entry.UninstallString -like "*-RegistryRoot `"$regRoot`"*") 'the entry runs the installed uninstall.ps1 with the places of this install'
    Write-Check ((Get-Content -LiteralPath $config -Raw) -match 'keep-me') 'the settings are untouched'
    Write-Check ($r.Text -match 'Chrome: starts .*checkout' -and $r.Text -match 'Register again') 'a browser that starts the checkout''s host is named, with what to do'
    Write-Check ($r.Text -match 'Load unpacked' -and $r.Text.Contains((Join-Path $install 'extension'))) 'the output names the extension folder to load unpacked'

    # 3b. Whether ffmpeg is found is said; the PATH of the child decides (pwsh's own folder: no ffmpeg).
    $pwshDir = Split-Path -Parent (Get-Process -Id $PID).Path
    $r = Invoke-Script $installer $installArgs -PathEnv $pwshDir
    Write-Check ($r.Code -eq 0 -and $r.Text -match 'ffmpeg was not found on the PATH' -and $r.Text -match 'scoop install ffmpeg') 'a missing ffmpeg is named, with how to get it'
    $fakeFfmpeg = Join-Path $tmp 'fake ffmpeg'
    New-Item -ItemType Directory -Path $fakeFfmpeg | Out-Null
    Copy-Item -LiteralPath (Join-Path $env:SystemRoot 'System32\cmd.exe') -Destination (Join-Path $fakeFfmpeg 'ffmpeg.exe')
    $r = Invoke-Script $installer $installArgs -PathEnv "$pwshDir;$fakeFfmpeg"
    Write-Check ($r.Code -eq 0 -and $r.Text -notmatch 'ffmpeg was not found') 'and nothing is said when it is there'

    # 4. A browser registered for the installed app is not named; installing again replaces the files.
    # In Windows PowerShell 5.1, whose Get-Content reads a file without a BOM as ANSI unless told.
    $edgeManifest = Add-Registration 'edge' 'Microsoft\Edge' (Join-Path $bin 'streamtap-host.exe')
    $r = Invoke-Script $buildInstall $buildInstallArgs -Shell $windowsPowerShell
    Write-Check ($r.Code -eq 0) 'install again (build-install.ps1 in Windows PowerShell 5.1)'
    if ($r.Code -ne 0) { Write-Output $r.Text }
    Write-Check ($r.Text -notmatch 'Edge:' -and $r.Text -match 'Chrome:') 'only the browser of the other copy is named (a non-ASCII path in a UTF-8 manifest)'
    $expectedTree = Get-Tree $payload | Where-Object { $_ -ne 'install.ps1' }
    Write-Check ((@(Get-Tree $install) -join '|') -ceq (@($expectedTree) -join '|')) 'the install holds exactly the payload but install.ps1'

    # 4a. A newer release drops a file of the extension: the installed copy of it goes, the rest stays.
    $stale = Join-Path $install 'extension\src\gone-in-this-release.js'
    Set-Content -LiteralPath $stale -Value 'old'
    $r = Invoke-Script $installer $installArgs
    Write-Check ($r.Code -eq 0 -and -not (Test-Path -LiteralPath $stale) -and (Test-Path -LiteralPath (Join-Path $install 'extension\src\background.js'))) 'an extension file the release no longer has is removed on install'

    # 4b. A new release whose last program cannot be read: the copy fails, and the installed programs
    # are all still the old ones (no new app beside an old sink), with no .new file left.
    $payload2 = Join-Path $tmp 'payload two'
    Copy-Item -LiteralPath $payload -Destination $payload2 -Recurse
    foreach ($name in 'streamtap-app', 'streamtap-sink', 'streamtap-host') {
        [IO.File]::AppendAllText((Join-Path $payload2 "bin\$name.exe"), 'v2')
    }
    $args2 = @('-PayloadDir', $payload2) + $places
    $held = [IO.File]::Open((Join-Path $payload2 'bin\streamtap-host.exe'), [IO.FileMode]::Open, [IO.FileAccess]::Read, [IO.FileShare]::None)
    try {
        $r = Invoke-Script $installer $args2
    }
    finally {
        $held.Dispose()
    }
    Write-Check ($r.Code -ne 0 -and $r.Text -match 'unchanged') 'a copy that fails is an error'
    Write-Check ((Test-SameProgram $bin $BuildDir) -and -not (Get-Tree $install | Where-Object { $_ -match '\.(new|old)$' })) 'and the installed programs are all the old ones, no .new file left'
    $r = Invoke-Script $installer $args2
    Write-Check ($r.Code -eq 0 -and (Test-SameProgram $bin (Join-Path $payload2 'bin'))) 'the next install replaces all three'
    $r = Invoke-Script $installer $installArgs
    Write-Check ($r.Code -eq 0 -and (Test-SameProgram $bin $BuildDir)) 'and the first build again'

    # 4c. Every copy succeeds but a rename fails (an installed file held without delete sharing, as a
    # program started meanwhile would): the files swapped so far are put back, all old again.
    $held = [IO.File]::Open((Join-Path $bin 'streamtap-host.exe'), [IO.FileMode]::Open, [IO.FileAccess]::Read, [IO.FileShare]::Read)
    try {
        $r = Invoke-Script $installer $args2
    }
    finally {
        $held.Dispose()
    }
    Write-Check ($r.Code -ne 0 -and $r.Text -match 'put back') 'a rename that fails is an error'
    if ($r.Code -eq 0 -or $r.Text -notmatch 'put back') { Write-Output $r.Text }
    Write-Check (Test-SameProgram $bin $BuildDir) 'and the installed programs are all the old ones again'
    Write-Check ((@(Get-Tree $install) -join '|') -ceq (@($expectedTree) -join '|')) 'with no .new or .old file left'

    # 4d. An install into another folder takes the one Settings > Apps entry over and says which install
    # lost it; installing here again takes it back.
    $install2 = Join-Path $tmp 'install two'
    $r = Invoke-Script $installer @('-PayloadDir', $payload, '-InstallDir', $install2)
    Write-Check ($r.Code -eq 0 -and $r.Text -match ('was for the install in ' + [regex]::Escape($install) + ';')) 'an install elsewhere names the install whose entry it takes'
    $r = Invoke-Script $installer $installArgs
    Write-Check ($r.Code -eq 0 -and $r.Text -match ('was for the install in ' + [regex]::Escape($install2) + ';') -and
        ([string](Get-ItemProperty -LiteralPath $uninstallKey).InstallLocation) -eq $install) 'and installing here again takes it back'

    # 5. A program running from the install folder: both scripts refuse, name it, change nothing.
    $runner = Start-Process -FilePath (Join-Path $bin 'streamtap-sink.exe') -ArgumentList '/d', '/k' -WindowStyle Hidden -PassThru
    Start-Sleep -Milliseconds 300
    $r = Invoke-Script $installer $installArgs
    Write-Check ($r.Code -ne 0 -and $r.Text -match "streamtap-sink \(PID $($runner.Id)\)") 'install refuses while the sink runs from there, and names the PID'
    $r = Invoke-Script $uninstallSrc $places
    Write-Check ($r.Code -ne 0 -and $r.Text -match "PID $($runner.Id)") 'uninstall refuses while the sink runs from there'
    # What Settings > Apps runs: the error is shown and the window waits for Enter.
    $uninstallString = [string](Get-ItemProperty -LiteralPath $uninstallKey).UninstallString
    # Enter comes after 8 s: an uninstall that does not wait has long ended by then.
    $r = Invoke-CommandLine $uninstallString -EnterAfterMs 8000
    Write-Check ($r.Code -ne 0 -and $r.Waited -and $r.Text -match 'uninstall failed: .*PID') 'from Settings > Apps the refusal is shown and waits for Enter'
    Write-Check ((Test-Path -LiteralPath (Join-Path $bin 'streamtap-app.exe')) -and (Test-Path -LiteralPath $link) -and (Test-Path -LiteralPath (Get-KeyPath 'Microsoft\Edge'))) 'and nothing was removed'
    Stop-Process -Id $runner.Id -Force
    $runner.WaitForExit()
    $runner = $null

    # 6. The entry's command line as Windows runs it (Windows PowerShell 5.1, the installed copy of
    # uninstall.ps1, the places with spaces) removes the install and the script itself, and what an
    # install that was cut short leaves (.new and .old files).
    foreach ($name in 'bin\streamtap-app.exe.old', 'uninstall.ps1.new', 'extension\src\background.js.new') { Set-Content -LiteralPath (Join-Path $install $name) -Value 'left' }
    $r = Invoke-CommandLine $uninstallString
    Write-Check ($r.Code -eq 0) 'uninstall with the command line of the Settings > Apps entry'
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
    $r = Invoke-Script $installer $installArgs
    Write-Check ($r.Code -eq 0) 'install before the foreign file'
    Set-Content -LiteralPath (Join-Path $install 'mine.txt') -Value 'mine'
    $r = Invoke-Script $uninstallSrc $places
    Write-Check ($r.Code -eq 0 -and (Test-Path -LiteralPath (Join-Path $install 'mine.txt')) -and -not (Test-Path -LiteralPath (Join-Path $bin 'streamtap-app.exe'))) 'the foreign file and so the folder stay, the programs go'
    Remove-Item -LiteralPath (Join-Path $install 'mine.txt')

    # 8b. Run from a terminal whose current folder is the install folder: the folder cannot go, but
    # everything in it and the Settings > Apps entry do (an entry left would run a script that is gone).
    $r = Invoke-Script $installer $installArgs
    Write-Check ($r.Code -eq 0) 'install before the run from inside the folder'
    $uninstallString = [string](Get-ItemProperty -LiteralPath $uninstallKey).UninstallString
    $r = Invoke-CommandLine $uninstallString -WorkDir $install
    Write-Check ($r.Code -eq 0 -and $r.Text -match 'kept the empty folder') 'uninstall from inside the install folder keeps only the empty folder'
    if ($r.Code -ne 0) { Write-Output $r.Text }
    Write-Check (-not (Test-Path -LiteralPath $uninstallKey) -and -not (Get-ChildItem -LiteralPath $install -Force)) 'and the files and the Settings > Apps entry are gone'
    Remove-Item -LiteralPath $install

    # 8c. A folder that is not an install of this program (a checkout, a work folder) is neither
    # installed into nor emptied: a wrong -InstallDir must not cost it its extension\, README.md or LICENSE.
    $foreign = Join-Path $tmp 'somebody elses'
    New-Item -ItemType Directory -Force -Path (Join-Path $foreign 'extension\src') | Out-Null
    $foreignFiles = 'extension\src\work.js', 'README.md', 'LICENSE', 'uninstall.ps1'
    foreach ($rel in $foreignFiles) { Set-Content -LiteralPath (Join-Path $foreign $rel) -Value 'mine' }
    $foreignPlaces = @('-InstallDir', $foreign, '-StartMenuDir', $startMenu, '-RegistryRoot', $regRoot)
    $before = (@(Get-Tree $foreign) -join '|')
    $r = Invoke-Script $uninstallSrc $foreignPlaces
    Write-Check ($r.Code -eq 0 -and $r.Text -match 'nothing to do' -and $r.Text -match 'not an install of this program') 'uninstall of a folder that is not an install says so and removes nothing'
    Write-Check ((@(Get-Tree $foreign) -join '|') -ceq $before) 'and its extension folder, README.md, LICENSE and uninstall.ps1 are all still there'
    $r = Invoke-Script $installer ($installArgs + $foreignPlaces)
    Write-Check ($r.Code -ne 0 -and $r.Text -match 'holds files but no streamtap install') 'install into a folder with somebody else''s files is refused'
    Write-Check ((@(Get-Tree $foreign) -join '|') -ceq $before) 'and it is unchanged'
    Remove-Item -LiteralPath $foreign -Recurse -Force
    # An empty folder, and one with only the leftovers of an install that was cut short, are fine.
    New-Item -ItemType Directory -Force -Path (Join-Path $foreign 'bin') | Out-Null
    Set-Content -LiteralPath (Join-Path $foreign 'bin\streamtap-app.exe.new') -Value 'half'
    $r = Invoke-Script $installer ($installArgs + $foreignPlaces)
    Write-Check ($r.Code -eq 0 -and (Test-Path -LiteralPath (Join-Path $foreign 'bin\streamtap-app.exe'))) 'a folder with the leftovers of a cut short install is installed into'
    $r = Invoke-Script $uninstallSrc $foreignPlaces
    Write-Check ($r.Code -eq 0 -and -not (Test-Path -LiteralPath (Join-Path $foreign 'bin'))) 'and an install is still removed'
    Remove-Item -LiteralPath $foreign -Recurse -Force -ErrorAction SilentlyContinue

    # 9. A program file that cannot be removed is an error, not a "removed", and the entry stays.
    $r = Invoke-Script $installer $installArgs
    $lock = [IO.File]::Open((Join-Path $bin 'streamtap-host.exe'), [IO.FileMode]::Open, [IO.FileAccess]::Read, [IO.FileShare]::Read)
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

    # 11. An install of the layout before bin\ (programs directly in the install folder, as the earlier
    # build-install.ps1 made it): installing moves it to bin\, names a browser that still starts the old
    # host, and leaves nothing behind in the root.
    New-Item -ItemType Directory -Path $install | Out-Null
    foreach ($name in 'streamtap-app.exe', 'streamtap-sink.exe', 'streamtap-host.exe') {
        Copy-Item -LiteralPath (Join-Path $BuildDir $name) -Destination (Join-Path $install $name)
    }
    Copy-Item -LiteralPath $uninstallSrc -Destination (Join-Path $install 'uninstall.ps1')
    $oldManifest = Add-Registration 'chrome' 'Google\Chrome' (Join-Path $install 'streamtap-host.exe')
    $r = Invoke-Script $installer $installArgs
    Write-Check ($r.Code -eq 0) 'install over the layout before bin\'
    Write-Check (-not (Get-ChildItem -LiteralPath $install -Filter '*.exe') -and (Test-SameProgram $bin $BuildDir)) 'the programs moved to bin\'
    Write-Check ($r.Text -match 'live in bin' -and $r.Text -match 'Chrome: starts .*streamtap-host.exe' -and $r.Text -match 'Register again') 'and a browser that still starts the old host is named'
    Write-Check ((@(Get-Tree $install) -join '|') -ceq (@($expectedTree) -join '|')) 'nothing else is left in the install folder'
    # The uninstaller also takes the old layout: programs in the root, no bin\.
    foreach ($name in 'streamtap-app.exe', 'streamtap-sink.exe', 'streamtap-host.exe') {
        Copy-Item -LiteralPath (Join-Path $BuildDir $name) -Destination (Join-Path $install $name)
    }
    Remove-Item -LiteralPath $bin, (Join-Path $install 'extension') -Recurse -Force
    $r = Invoke-Script $uninstallSrc $places
    Write-Check ($r.Code -eq 0 -and -not (Test-Path -LiteralPath $install)) 'uninstall removes the programs of the old layout too'
    Write-Check (-not (Test-Path -LiteralPath (Get-KeyPath 'Google\Chrome')) -and -not (Test-Path -LiteralPath $oldManifest)) 'and the registration of the old host'
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
