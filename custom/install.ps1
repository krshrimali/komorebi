# Build the custom komorebi fork and (re)install it for the current user.
# Usage: powershell -ExecutionPolicy Bypass -File custom\install.ps1
$ErrorActionPreference = 'Stop'

$repo = Split-Path -Parent $PSScriptRoot
$dest = Join-Path $env:LOCALAPPDATA 'komorebi-custom'
$env:Path = [Environment]::GetEnvironmentVariable('Path', 'Machine') + ';' + [Environment]::GetEnvironmentVariable('Path', 'User')

Push-Location $repo
try {
    cargo +stable-x86_64-pc-windows-gnu build --release -p komorebi -p komorebi-overview
    if ($LASTEXITCODE -ne 0) { throw "cargo build failed" }
} finally {
    Pop-Location
}

New-Item -ItemType Directory -Force $dest | Out-Null

# Stop the running instance so the binary can be replaced; komorebic is the stock
# CLI and talks to the custom build over the same socket protocol
$wasRunning = [bool](Get-Process komorebi -ErrorAction SilentlyContinue)
if ($wasRunning) {
    komorebic stop --whkd --bar | Out-Null
    Get-Process komorebi -ErrorAction SilentlyContinue | Wait-Process -Timeout 15 -ErrorAction SilentlyContinue
}

Get-Process komorebi-overview -ErrorAction SilentlyContinue | Stop-Process -Force
Copy-Item (Join-Path $repo 'target\release\komorebi.exe') (Join-Path $dest 'komorebi.exe') -Force
Copy-Item (Join-Path $repo 'target\release\komorebi-overview.exe') (Join-Path $dest 'komorebi-overview.exe') -Force
Copy-Item (Join-Path $PSScriptRoot 'start.ps1') (Join-Path $dest 'start.ps1') -Force

# Autostart: replace the stock komorebic shortcut with the custom launcher
$startup = [Environment]::GetFolderPath('Startup')
$shell = New-Object -ComObject WScript.Shell
$lnk = $shell.CreateShortcut((Join-Path $startup 'komorebi.lnk'))
$lnk.TargetPath = 'powershell.exe'
$lnk.Arguments = "-NoProfile -WindowStyle Hidden -ExecutionPolicy Bypass -File `"$dest\start.ps1`""
$lnk.WindowStyle = 7
$lnk.Save()

& (Join-Path $dest 'start.ps1')
Write-Host "custom komorebi installed to $dest and started"
