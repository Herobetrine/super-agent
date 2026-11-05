param(
    # Defaults to "Super Agent.lnk" on the current user's desktop.
    [string]$ShortcutPath
)
$ErrorActionPreference = 'Stop'
$target = Join-Path $PSScriptRoot 'target\release\super-agent.exe'
if (-not (Test-Path -LiteralPath $target -PathType Leaf)) {
    throw 'Build the desktop first: scripts\cargo.cmd build --release -p sa-desktop'
}
if (-not $ShortcutPath) {
    $ShortcutPath = Join-Path ([Environment]::GetFolderPath('Desktop')) 'Super Agent.lnk'
}
$shell = New-Object -ComObject WScript.Shell
$link = $shell.CreateShortcut($ShortcutPath)
$link.TargetPath = $target
$link.Arguments = ''
$link.WorkingDirectory = $PSScriptRoot
$link.IconLocation = (Join-Path $PSScriptRoot 'assets\super-agent.ico') + ',0'
$link.Description = 'Super Agent local conversation workspace'
$link.Save()
$verified = $shell.CreateShortcut($ShortcutPath)
if ($verified.TargetPath -ne $target -or $verified.WorkingDirectory -ne $PSScriptRoot) { throw 'Shortcut verification failed' }
Write-Output $ShortcutPath
