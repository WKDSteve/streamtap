# Closes the Explorer windows that show a folder below -Under: the `cleanup` step of the app `captures` in .testing.lua
# (testing.nvim docs/SHEETS.md). "Open folder" asks the shell to show a capture of the run, and Explorer keeps that
# window open after the run removes the folder. Only windows whose folder lies below -Under (the folder of the run)
# are closed, through the shell's own list of its windows; every other Explorer window stays.
#
# usage: powershell -NoProfile -NonInteractive -File tools/sheet-close-folders.ps1 -Under <dir>
param([Parameter(Mandatory = $true)][string]$Under)

$ErrorActionPreference = 'Stop'
$root = [System.IO.Path]::GetFullPath($Under).TrimEnd('\') + '\'
$shell = New-Object -ComObject Shell.Application
foreach ($window in @($shell.Windows())) {
  $url = $window.LocationURL
  if (-not $url -or -not $url.StartsWith('file:///')) { continue }
  try {
    $path = [System.IO.Path]::GetFullPath(([System.Uri]$url).LocalPath).TrimEnd('\') + '\'
  } catch {
    continue
  }
  if ($path.StartsWith($root, [System.StringComparison]::OrdinalIgnoreCase)) {
    $window.Quit()
  }
}
