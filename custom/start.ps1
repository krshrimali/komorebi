# Start the custom komorebi build plus the stock whkd and komorebi-bar.
$exe = Join-Path $PSScriptRoot 'komorebi.exe'
$env:Path = [Environment]::GetEnvironmentVariable('Path', 'Machine') + ';' + [Environment]::GetEnvironmentVariable('Path', 'User')

if (-not (Get-Process komorebi -ErrorAction SilentlyContinue)) {
    Start-Process $exe -WindowStyle Hidden
    for ($i = 0; $i -lt 30 -and -not (Get-Process komorebi -ErrorAction SilentlyContinue); $i++) {
        Start-Sleep -Milliseconds 200
    }
    # give komorebi time to create its socket before the bar connects
    Start-Sleep -Seconds 2
}

if (-not (Get-Process whkd -ErrorAction SilentlyContinue)) {
    Start-Process whkd -WindowStyle Hidden
}

if (-not (Get-Process komorebi-bar -ErrorAction SilentlyContinue)) {
    Start-Process komorebi-bar -WindowStyle Hidden
}

# Workspace overview (Alt+Q): stays resident so it opens instantly
$overview = Join-Path $PSScriptRoot 'komorebi-overview.exe'
if ((Test-Path $overview) -and -not (Get-Process komorebi-overview -ErrorAction SilentlyContinue)) {
    Start-Process $overview -ArgumentList '--daemon'
}
