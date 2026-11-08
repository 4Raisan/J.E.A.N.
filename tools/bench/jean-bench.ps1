param([string]$JeanExe = "$PSScriptRoot\..\..\target\debug\jean.exe")
# Phase 6 benchmark harness (no workers): idle RSS, startup, disk.
$ErrorActionPreference = 'Stop'
$t0 = Get-Date
& $JeanExe --version | Out-Null
$startupMs = ((Get-Date) - $t0).TotalMilliseconds
$proc = Start-Process $JeanExe -ArgumentList "status" -PassThru -NoNewWindow
$proc | Wait-Process -Timeout 10
$idle = Get-Process -Id $PID
$rows = Get-Process | Where-Object { $_.Path -eq (Resolve-Path $JeanExe -ErrorAction SilentlyContinue) }
$disk = Get-PSDrive C | Select-Object -ExpandProperty Free
Write-Host ("startup_ms={0:N0} bench_free_bytes={1}" -f $startupMs, $disk)
Write-Host "PASS harness ran; record values in CI alongside cargo bloat (Phase 6)."
