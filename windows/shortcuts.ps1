$ErrorActionPreference = 'Stop'
$binary = $env:ASP_INSTALL_BINARY
if (-not (Test-Path -LiteralPath $binary -PathType Leaf)) { throw 'asp.exe is missing' }
$shell = New-Object -ComObject WScript.Shell
foreach ($folder in @([Environment]::GetFolderPath('Startup'), [Environment]::GetFolderPath('Programs'))) {
    $shortcut = $shell.CreateShortcut((Join-Path $folder 'Agents Sleep Preventer.lnk'))
    $shortcut.TargetPath = $binary
    $shortcut.Arguments = 'tray'
    $shortcut.WorkingDirectory = Split-Path -Parent $binary
    $shortcut.Description = 'Keep Windows awake while coding agents work'
    $shortcut.WindowStyle = 7
    $shortcut.Save()
}
