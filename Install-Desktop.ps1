$ErrorActionPreference = 'Stop'
$electron = Join-Path $PSScriptRoot 'node_modules\electron\dist\electron.exe'
$native = Join-Path $PSScriptRoot 'native\target\release\super-agent-core.exe'
foreach ($binary in @($electron, $native)) {
    if (-not (Test-Path -LiteralPath $binary -PathType Leaf)) {
        throw 'Build the desktop runtime first: npm ci, node node_modules/electron/install.js, npm run build:native'
    }
}
$desktopDir = [Environment]::GetFolderPath('Desktop')
$linkPath = Join-Path $desktopDir 'Super Agent.lnk'
$shell = New-Object -ComObject WScript.Shell
$link = $shell.CreateShortcut($linkPath)
$link.TargetPath = $electron
$link.Arguments = '"' + $PSScriptRoot + '"'
$link.WorkingDirectory = $PSScriptRoot
$link.IconLocation = (Join-Path $PSScriptRoot 'assets\super-agent.ico') + ',0'
$link.Description = 'Super Agent local conversation workspace'
$link.Save()
$verified = $shell.CreateShortcut($linkPath)
if ($verified.TargetPath -ne $electron -or $verified.Arguments -ne $link.Arguments -or $verified.WorkingDirectory -ne $PSScriptRoot) { throw 'Shortcut verification failed' }
Write-Output $linkPath
