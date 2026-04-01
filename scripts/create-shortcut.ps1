# Creates a Windows startup shortcut for rsclaw-tray
# Run once after installation

$scriptPath = Join-Path $PSScriptRoot "rsclaw-tray.ps1"
$startupDir = [System.IO.Path]::Combine(
    [Environment]::GetFolderPath("Startup")
)
$shortcutPath = Join-Path $startupDir "RsClaw Tray.lnk"

$shell = New-Object -ComObject WScript.Shell
$shortcut = $shell.CreateShortcut($shortcutPath)
$shortcut.TargetPath = "powershell.exe"
$shortcut.Arguments = "-WindowStyle Hidden -ExecutionPolicy Bypass -File `"$scriptPath`""
$shortcut.WorkingDirectory = $PSScriptRoot
$shortcut.Description = "RsClaw Gateway Tray Controller"
$shortcut.Save()

Write-Host "Startup shortcut created: $shortcutPath"
Write-Host "RsClaw tray will auto-start on login."
