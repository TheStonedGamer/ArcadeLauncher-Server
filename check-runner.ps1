$svc = 'actions.runner.TheStonedGamer-ArcadeLauncher-Unified-Client.pc-win-runner'
try { Restart-Service $svc -Force; "service restarted" } catch { "restart error: $($_.Exception.Message)" }
Start-Sleep 8
(Get-Service $svc).Status | ForEach-Object { "service status: $_" }
$d = Get-ChildItem C:\actions-runner\_diag\Runner_*.log -ErrorAction SilentlyContinue | Sort-Object LastWriteTime | Select-Object -Last 1
if ($d) {
  "--- $($d.Name) tail ---"
  Get-Content $d.FullName -Tail 15
} else { 'NO DIAG LOG' }
