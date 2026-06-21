param([string]$Token)
$ErrorActionPreference = 'Stop'
$log = 'C:\runner-setup.log'
function Log($m){ "$([DateTime]::Now.ToString('HH:mm:ss')) $m" | Tee-Object -FilePath $log -Append }
[Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12

$ver = '2.335.1'
$dir = 'C:\actions-runner'
$url = "https://github.com/actions/runner/releases/download/v$ver/actions-runner-win-x64-$ver.zip"
$zip = "C:\actions-runner-win-x64-$ver.zip"
$repoUrl = 'https://github.com/TheStonedGamer/ArcadeLauncher-Unified-Client'

try {
  Log 'START runner setup'
  New-Item -ItemType Directory -Force -Path $dir | Out-Null
  if (-not (Test-Path $zip)) {
    Log "Downloading runner $ver..."
    Invoke-WebRequest -Uri $url -OutFile $zip -UseBasicParsing
  }
  Log 'Extracting...'
  Add-Type -AssemblyName System.IO.Compression.FileSystem
  Get-ChildItem $dir -Recurse -ErrorAction SilentlyContinue | Remove-Item -Recurse -Force -ErrorAction SilentlyContinue
  [System.IO.Compression.ZipFile]::ExtractToDirectory($zip, $dir)
  Log 'Configuring runner as a service...'
  Push-Location $dir
  & "$dir\config.cmd" --unattended --replace --url $repoUrl --token $Token --name 'pc-win-runner' --labels 'self-hosted,Windows,X64' --work '_work' --runasservice 2>&1 | Tee-Object -FilePath $log -Append
  Pop-Location
  Log "config exit: $LASTEXITCODE"
  Log 'DONE runner setup'
}
catch {
  Log "ERROR: $($_.Exception.Message)"
}
