# Cache watcher for the engine smokes (2026-10-24) and the Memory cache smoke (2026-11-27).
#
#   powershell -ExecutionPolicy Bypass -File scripts/smoke/watch-cache.ps1 -Mode new   -Seconds 90
#   powershell -ExecutionPolicy Bypass -File scripts/smoke/watch-cache.ps1 -Mode spill -Seconds 90
#   powershell -ExecutionPolicy Bypass -File scripts/smoke/watch-cache.ps1 -Mode spill -Seconds 90 `
#     -CacheDir "$env:APPDATA\OxiTide\config\cache","$env:TEMP"
#
# new   - prints each .fmp4 that appears after the baseline (download-ahead row)
# spill - prints each spill-*.part or other .part file that appears and disappears (seek row)
#
# -CacheDir takes one folder or several; every mode watches all of them. Memory mode has no
# cache folder, so its only possible leak is %TEMP%: watch both to prove nothing is written.
param(
  [ValidateSet("new", "spill")] [string]$Mode = "new",
  [int]$Seconds = 90,
  [string[]]$CacheDir = @("$env:APPDATA\OxiTide\config\cache")
)

$start = Get-Date
$elapsed = { "{0:N1}s" -f ((Get-Date) - $start).TotalSeconds }
$folders = $CacheDir -join ", "

function Get-Watched([string]$Filter) {
  foreach ($dir in $CacheDir) {
    Get-ChildItem $dir -Filter $Filter -File -ErrorAction SilentlyContinue
  }
}

if ($Mode -eq "new") {
  $known = @{}
  Get-Watched "*.fmp4" | ForEach-Object { $known[$_.FullName] = $true }
  Write-Host "baseline: $($known.Count) cached tracks in $folders; watching $Seconds s"
  while (((Get-Date) - $start).TotalSeconds -lt $Seconds) {
    Get-Watched "*.fmp4" | ForEach-Object {
      if (-not $known.ContainsKey($_.FullName)) {
        $known[$_.FullName] = $true
        $meta = Test-Path ($_.FullName + ".meta")
        Write-Host ("[{0}] new {1}  {2:N1} MB  sidecar={3}" -f (& $elapsed), $_.FullName, ($_.Length / 1MB), $meta)
      }
    }
    Start-Sleep -Milliseconds 500
  }
} else {
  # Files already there (a stale .part, another app's temp file) are not reported.
  $baseline = @{}
  Get-Watched "*.part" | ForEach-Object { $baseline[$_.FullName] = $true }
  $present = @{}
  Write-Host "watching for spill and .part files in $folders for $Seconds s"
  while (((Get-Date) - $start).TotalSeconds -lt $Seconds) {
    Get-Watched "*.part" | ForEach-Object {
      if (-not $baseline.ContainsKey($_.FullName) -and -not $present.ContainsKey($_.FullName)) {
        $present[$_.FullName] = $true
        Write-Host ("[{0}] appeared {1}  {2:N1} MB" -f (& $elapsed), $_.FullName, ($_.Length / 1MB))
      }
    }
    foreach ($path in @($present.Keys)) {
      if (-not (Test-Path $path)) {
        $present.Remove($path)
        Write-Host ("[{0}] gone {1}" -f (& $elapsed), $path)
      }
    }
    Start-Sleep -Milliseconds 250
  }
}
Write-Host "done"
