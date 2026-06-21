$svc = 'actions.runner.TheStonedGamer-ArcadeLauncher-Unified-Client.pc-win-runner'
"svc: " + (((Get-Service $svc -ErrorAction SilentlyContinue).Status) -as [string])
"runnerExists: " + (Test-Path C:\actions-runner\.runner)
"credsExists: " + (Test-Path C:\actions-runner\.credentials)
"listenerProc: " + (((Get-Process Runner.Listener -ErrorAction SilentlyContinue).Id) -join ',')
$d = Get-ChildItem C:\actions-runner\_diag\Runner_*.log -ErrorAction SilentlyContinue | Sort-Object LastWriteTime | Select-Object -Last 1
if ($d) { "--- $($d.Name) tail ---"; Get-Content $d.FullName -Tail 6 } else { "no runner diag" }
