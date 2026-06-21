$d = Get-ChildItem C:\actions-runner\_diag\Runner_*.log | Sort-Object LastWriteTime | Select-Object -Last 1
Get-Content $d.FullName -Tail 8
