param([string]$Token)
$ErrorActionPreference = 'Stop'
$dir = 'C:\actions-runner'
$zip = 'C:\actions-runner-win-x64-2.335.1.zip'
$repoUrl = 'https://github.com/TheStonedGamer/ArcadeLauncher-Unified-Client'

"== kill stray listeners =="
Get-Process Runner.Listener -ErrorAction SilentlyContinue | ForEach-Object { Stop-Process -Id $_.Id -Force }
Start-Sleep 2
"== wipe dir =="
if (Test-Path $dir) { Remove-Item $dir -Recurse -Force }
New-Item -ItemType Directory -Force -Path $dir | Out-Null
"== verify zip =="
"zipSize: " + (Get-Item $zip).Length
"== extract =="
Add-Type -AssemblyName System.IO.Compression.FileSystem
[System.IO.Compression.ZipFile]::ExtractToDirectory($zip, $dir)
"binCount: " + (Get-ChildItem "$dir\bin" -File | Measure-Object).Count
"dllPresent: " + (Test-Path "$dir\bin\System.ServiceProcess.ServiceController.dll")
"== configure as service =="
Set-Location $dir
& "$dir\config.cmd" --unattended --replace --url $repoUrl --token $Token --name 'pc-win-runner' --labels 'self-hosted,Windows,X64' --work '_work' --runasservice 2>&1
"config exit: $LASTEXITCODE"
Start-Sleep 5
$svc = 'actions.runner.TheStonedGamer-ArcadeLauncher-Unified-Client.pc-win-runner'
"svcStatus: " + (((Get-Service $svc -ErrorAction SilentlyContinue).Status) -as [string])
