param(
    # Defaults to "Super Agent.lnk" on the current user's desktop.
    [string]$ShortcutPath,
    # Point the shortcut at the legacy Electron workbench instead of the Rust one.
    [switch]$Legacy
)
$ErrorActionPreference = 'Stop'
if ($Legacy) {
    $target = Join-Path $PSScriptRoot 'node_modules\electron\dist\electron.exe'
    $native = Join-Path $PSScriptRoot 'native\target\release\super-agent-core.exe'
    foreach ($binary in @($target, $native)) {
        if (-not (Test-Path -LiteralPath $binary -PathType Leaf)) {
            throw 'Build the legacy runtime first: npm ci, node node_modules/electron/install.js, npm run build:native'
        }
    }
    $arguments = '"' + $PSScriptRoot + '"'
} else {
    $target = Join-Path $PSScriptRoot 'target\release\super-agent.exe'
    if (-not (Test-Path -LiteralPath $target -PathType Leaf)) {
        throw 'Build the desktop first: node scripts/cargo.cjs build --release -p sa-desktop'
    }
    $arguments = ''
}
if (-not $ShortcutPath) {
    $ShortcutPath = Join-Path ([Environment]::GetFolderPath('Desktop')) 'Super Agent.lnk'
}
$shell = New-Object -ComObject WScript.Shell
$link = $shell.CreateShortcut($ShortcutPath)
$link.TargetPath = $target
$link.Arguments = $arguments
$link.WorkingDirectory = $PSScriptRoot
$link.IconLocation = (Join-Path $PSScriptRoot 'assets\super-agent.ico') + ',0'
$link.Description = 'Super Agent local conversation workspace'
$link.Save()
$verified = $shell.CreateShortcut($ShortcutPath)
if ($verified.TargetPath -ne $target -or $verified.Arguments -ne $arguments -or $verified.WorkingDirectory -ne $PSScriptRoot) { throw 'Shortcut verification failed' }
Write-Output $ShortcutPath
