param([string]$Token)
$ErrorActionPreference = 'Continue'
$svc = 'actions.runner.TheStonedGamer-ArcadeLauncher-Unified-Client.pc-win-runner'
$dir = 'C:\actions-runner'
$repoUrl = 'https://github.com/TheStonedGamer/ArcadeLauncher-Unified-Client'

"== stop service =="
try { Stop-Service $svc -Force -ErrorAction Stop; "stopped" } catch { "stop: $($_.Exception.Message)" }
"== kill stray listeners =="
Get-Process Runner.Listener,RunnerService -ErrorAction SilentlyContinue | ForEach-Object { "kill $($_.Id)"; Stop-Process -Id $_.Id -Force }
Start-Sleep 3
"== delete service =="
$null = sc.exe delete $svc
Start-Sleep 2
"== local config remove =="
Set-Location $dir
& "$dir\config.cmd" remove --token $Token 2>&1
"remove exit: $LASTEXITCODE"
# Belt-and-suspenders: clear residual identity files if remove left them
foreach ($f in @('.runner','.credentials','.credentials_rsaparams','.service')) {
  if (Test-Path "$dir\$f") { Remove-Item "$dir\$f" -Force; "cleared $f" }
}
"== fresh config =="
& "$dir\config.cmd" --unattended --url $repoUrl --token $Token --name 'pc-win-runner' --labels 'self-hosted,Windows,X64' --work '_work' --runasservice 2>&1
"config exit: $LASTEXITCODE"
Start-Sleep 5
"== service status =="
(Get-Service $svc -ErrorAction SilentlyContinue).Status
