# The entry of an app in Settings > Apps (the Uninstall key below HKCU): written by the install.ps1 of
# the app, checked and run by its test-release.ps1. One text for every app that installs with a
# script of its own (pult, takt, streamtap): each keeps a copy of this file in its scripts\ folder and
# ships it beside install.ps1, and its tools/check-release-kit-copy.py holds the copy to this file at
# the commit of appkit its workflows pin.
#
#     . (Join-Path $PSScriptRoot 'uninstall-entry.ps1')
#
# The entry runs the installed program itself with --uninstall (appkit-uninstall): no PowerShell and
# no console window. The other places of the install are named only when they are not the user's own
# (a test, a folder of the user's choice). Windows PowerShell 5.1 and PowerShell 7.

Set-StrictMode -Version Latest

# The key of the entry of $Name below $RegistryRoot (the key that stands for HKCU\Software).
function Get-UninstallEntryKey([string] $RegistryRoot, [string] $Name) {
    return "$RegistryRoot\Microsoft\Windows\CurrentVersion\Uninstall\$Name"
}

# The command line of the entry: the installed program with --uninstall, and the places of this install
# that are not the user's own. The places hold no quote and no trailing backslash (the install checked
# them): a "C:\x\" would end the quoted argument with \", which escapes the quote.
function Get-UninstallEntryCommand {
    param(
        [Parameter(Mandatory)] [string] $App,
        [Parameter(Mandatory)] [string] $StartMenuDir,
        [Parameter(Mandatory)] [string] $DesktopDir,
        [Parameter(Mandatory)] [string] $RegistryRoot
    )
    foreach ($place in $App, $StartMenuDir, $DesktopDir, $RegistryRoot) {
        if ($place.Contains('"') -or $place.EndsWith('\')) {
            throw "uninstall-entry: a place with a quote or a trailing backslash: $place"
        }
    }
    $command = "`"$App`" --uninstall"
    # An account without such a folder (a service account) has no place of its own to compare with.
    $ownStartMenu = [Environment]::GetFolderPath('Programs')
    $ownDesktop = [Environment]::GetFolderPath('Desktop')
    if (-not $ownStartMenu -or $StartMenuDir -ne $ownStartMenu.TrimEnd('\')) { $command += " --start-menu-dir `"$StartMenuDir`"" }
    if (-not $ownDesktop -or $DesktopDir -ne $ownDesktop.TrimEnd('\')) { $command += " --desktop-dir `"$DesktopDir`"" }
    if ($RegistryRoot -ne 'HKCU:\Software') { $command += " --registry-root `"$RegistryRoot`"" }
    return $command
}

# Write the entry of an install, in place of the one there was. There is one entry per app: an install in
# another folder loses it (its files stay there), and that is said. Writes what it does to the output.
function Set-UninstallEntry {
    [CmdletBinding(SupportsShouldProcess)]
    param(
        [Parameter(Mandatory)] [string] $RegistryRoot,
        [Parameter(Mandatory)] [string] $Name,
        [Parameter(Mandatory)] [string] $DisplayName,
        [Parameter(Mandatory)] [string] $Version,
        [Parameter(Mandatory)] [string] $InstallDir,
        [Parameter(Mandatory)] [string] $App,
        [Parameter(Mandatory)] [string] $StartMenuDir,
        [Parameter(Mandatory)] [string] $DesktopDir,
        [string] $Publisher = 'WKDSteve'
    )
    if (-not $RegistryRoot.StartsWith('HKCU:\', [StringComparison]::OrdinalIgnoreCase)) {
        throw "uninstall-entry: the registry root must be below HKCU: $RegistryRoot"
    }
    $key = Get-UninstallEntryKey $RegistryRoot $Name
    Write-Output "==> Settings > Apps entry ($key)"
    $command = Get-UninstallEntryCommand -App $App -StartMenuDir $StartMenuDir -DesktopDir $DesktopDir -RegistryRoot $RegistryRoot
    # A folder without files measures nothing: Measure-Object then has no Sum at all.
    [long] $bytes = 0
    foreach ($file in @(Get-ChildItem -LiteralPath $InstallDir -Recurse -File)) { $bytes += $file.Length }
    $size = [int]($bytes / 1KB)
    $previous = Get-ItemProperty -LiteralPath $key -ErrorAction SilentlyContinue
    $previousDir = ''
    if ($previous -and $previous.PSObject.Properties['InstallLocation']) { $previousDir = ([string]$previous.InstallLocation).TrimEnd('\') }
    if ($previousDir -and $previousDir -ne $InstallDir) {
        Write-Output "the Settings > Apps entry was for the install in $previousDir; it now names this one, that copy stays:"
        Write-Output "  remove it with: & `"$(Join-Path $previousDir 'uninstall.ps1')`" -InstallDir `"$previousDir`""
    }
    if (-not $PSCmdlet.ShouldProcess($key, 'write the Settings > Apps entry')) { return }
    if (Test-Path -LiteralPath $key) { Remove-Item -LiteralPath $key -Recurse -Force }
    New-Item -Path $key -Force | Out-Null
    $values = [ordered]@{
        DisplayName     = $DisplayName
        DisplayVersion  = $Version
        Publisher       = $Publisher
        InstallLocation = $InstallDir
        DisplayIcon     = "$App,0"
        UninstallString = $command
    }
    foreach ($k in $values.Keys) {
        New-ItemProperty -LiteralPath $key -Name $k -Value $values[$k] -PropertyType String -Force | Out-Null
    }
    foreach ($k in 'NoModify', 'NoRepair') {
        New-ItemProperty -LiteralPath $key -Name $k -Value 1 -PropertyType DWord -Force | Out-Null
    }
    New-ItemProperty -LiteralPath $key -Name 'EstimatedSize' -Value $size -PropertyType DWord -Force | Out-Null
}

# What is wrong with the entry of an install, as sentences; none when it is as Set-UninstallEntry writes it.
function Test-UninstallEntry {
    param(
        [Parameter(Mandatory)] [string] $RegistryRoot,
        [Parameter(Mandatory)] [string] $Name,
        [Parameter(Mandatory)] [string] $DisplayName,
        [Parameter(Mandatory)] [string] $Version,
        [Parameter(Mandatory)] [string] $InstallDir,
        [Parameter(Mandatory)] [string] $App,
        [Parameter(Mandatory)] [string] $StartMenuDir,
        [Parameter(Mandatory)] [string] $DesktopDir
    )
    $key = Get-UninstallEntryKey $RegistryRoot $Name
    $entry = Get-ItemProperty -LiteralPath $key -ErrorAction SilentlyContinue
    if (-not $entry) { return @("there is no entry $key") }
    $problems = @()
    $wanted = [ordered]@{
        DisplayName     = $DisplayName
        DisplayVersion  = $Version
        InstallLocation = $InstallDir
        DisplayIcon     = "$App,0"
        UninstallString = Get-UninstallEntryCommand -App $App -StartMenuDir $StartMenuDir -DesktopDir $DesktopDir -RegistryRoot $RegistryRoot
    }
    foreach ($k in $wanted.Keys) {
        $has = if ($entry.PSObject.Properties[$k]) { [string]$entry.$k } else { $null }
        if ($has -cne $wanted[$k]) { $problems += "$k is '$has', not '$($wanted[$k])'" }
    }
    $command = if ($entry.PSObject.Properties['UninstallString']) { [string]$entry.UninstallString } else { '' }
    if ($command -match '(?i)(powershell|pwsh|cmd)\.exe|\.ps1|-File') { $problems += "the command names PowerShell, a shell or a script: $command" }
    foreach ($k in 'NoModify', 'NoRepair') {
        if (-not $entry.PSObject.Properties[$k] -or $entry.$k -ne 1) { $problems += "$k is not 1" }
    }
    if (-not $entry.PSObject.Properties['EstimatedSize'] -or $entry.EstimatedSize -lt 0) { $problems += 'EstimatedSize is missing' }
    return $problems
}

# Run the command of an entry as Settings > Apps would, plus $Extra (--yes: no window asks): the program
# it names, with what follows as it stands, from the folder the program lies in, with $Temp as its temp
# folder and without CLAUDECODE. A program that does not end within $Seconds is ended: a window that
# asks would wait for ever. Returns the exit code and what it wrote.
function Invoke-UninstallEntry {
    param(
        [Parameter(Mandatory)] [string] $Command,
        [Parameter(Mandatory)] [string] $Temp,
        [string[]] $Extra = @('--yes'),
        [int] $Seconds = 60
    )
    if ($Command -notmatch '^"([^"]+)"(.*)$') { return [pscustomobject]@{ Code = -1; Text = "not a command: $Command" } }
    $exe = $Matches[1]
    $arguments = @(@($Matches[2].Trim()) + $Extra | Where-Object { $_ })
    $info = New-Object Diagnostics.ProcessStartInfo
    $info.FileName = $exe
    $info.Arguments = $arguments -join ' '
    $info.UseShellExecute = $false
    $info.CreateNoWindow = $true
    $info.RedirectStandardOutput = $true
    $info.RedirectStandardError = $true
    $info.StandardOutputEncoding = [Text.Encoding]::UTF8
    $info.StandardErrorEncoding = [Text.Encoding]::UTF8
    # From inside the folder that is to go, as a terminal standing in it would.
    $info.WorkingDirectory = Split-Path -Parent $exe
    $info.EnvironmentVariables.Remove('CLAUDECODE')
    $info.EnvironmentVariables['TEMP'] = $Temp
    $info.EnvironmentVariables['TMP'] = $Temp
    $process = [Diagnostics.Process]::Start($info)
    try {
        $out = $process.StandardOutput.ReadToEndAsync()
        $err = $process.StandardError.ReadToEndAsync()
        if (-not $process.WaitForExit($Seconds * 1000)) {
            $process.Kill()
            return [pscustomobject]@{ Code = -1; Text = "did not end within $Seconds seconds (does a window ask?)" }
        }
        return [pscustomobject]@{ Code = $process.ExitCode; Text = ($out.Result + $err.Result) }
    }
    finally {
        $process.Dispose()
    }
}
