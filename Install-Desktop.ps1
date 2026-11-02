$ErrorActionPreference = 'Stop'
$entry = Join-Path $PSScriptRoot 'desktop.pyw'
if (-not (Test-Path -LiteralPath $entry -PathType Leaf)) { throw 'Missing desktop.pyw' }
$pythonw = (Get-Command pythonw.exe -ErrorAction Stop).Source
$desktopDir = [Environment]::GetFolderPath('Desktop')
$linkPath = Join-Path $desktopDir 'Super Agent.lnk'
$shell = New-Object -ComObject WScript.Shell
$link = $shell.CreateShortcut($linkPath)
$link.TargetPath = $pythonw
$link.Arguments = '"' + $entry + '"'
$link.WorkingDirectory = $PSScriptRoot
$link.IconLocation = (Join-Path $PSScriptRoot 'assets\super-agent.ico') + ',0'
$link.Description = 'Super Agent local conversation workspace'
$link.Save()
$verified = $shell.CreateShortcut($linkPath)
if ($verified.TargetPath -ne $pythonw -or $verified.Arguments -ne $link.Arguments) { throw 'Shortcut verification failed' }
Write-Output $linkPath
