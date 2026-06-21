$ErrorActionPreference = 'Stop'
$log = 'C:\provision.log'
function Log($m){ "$([DateTime]::Now.ToString('HH:mm:ss')) $m" | Tee-Object -FilePath $log -Append }
$tmp = 'C:\prov'
New-Item -ItemType Directory -Force -Path $tmp | Out-Null
[Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12

try {
  Log 'START provisioning'

  # --- Git ---
  Log 'Downloading Git...'
  $gitUrl = 'https://github.com/git-for-windows/git/releases/download/v2.47.1.windows.1/Git-2.47.1-64-bit.exe'
  $gitExe = "$tmp\git-setup.exe"
  Invoke-WebRequest -Uri $gitUrl -OutFile $gitExe -UseBasicParsing
  Log 'Installing Git silently...'
  Start-Process -FilePath $gitExe -ArgumentList '/VERYSILENT','/NORESTART','/NOCANCEL','/SP-','/SUPPRESSMSGBOXES' -Wait
  Log 'Git done'

  # --- VS Build Tools (MSVC + Windows SDK) ---
  Log 'Downloading VS Build Tools bootstrapper...'
  $vsUrl = 'https://aka.ms/vs/17/release/vs_BuildTools.exe'
  $vsExe = "$tmp\vs_BuildTools.exe"
  Invoke-WebRequest -Uri $vsUrl -OutFile $vsExe -UseBasicParsing
  Log 'Installing VS Build Tools (this takes a while)...'
  $vsArgs = @(
    '--quiet','--wait','--norestart','--nocache',
    '--add','Microsoft.VisualStudio.Workload.VCTools',
    '--add','Microsoft.VisualStudio.Component.VC.Tools.x86.x64',
    '--add','Microsoft.VisualStudio.Component.Windows11SDK.22621',
    '--includeRecommended'
  )
  $p = Start-Process -FilePath $vsExe -ArgumentList $vsArgs -Wait -PassThru
  Log "VS Build Tools exit code: $($p.ExitCode)"

  # --- rustup (stable MSVC) ---
  Log 'Downloading rustup...'
  $ruUrl = 'https://win.rustup.rs/x86_64'
  $ruExe = "$tmp\rustup-init.exe"
  Invoke-WebRequest -Uri $ruUrl -OutFile $ruExe -UseBasicParsing
  Log 'Installing rust stable-msvc...'
  Start-Process -FilePath $ruExe -ArgumentList '-y','--default-toolchain','stable','--default-host','x86_64-pc-windows-msvc','--profile','minimal' -Wait
  Log 'rustup done'

  Log 'ALL DONE'
}
catch {
  Log "ERROR: $($_.Exception.Message)"
  Log "AT: $($_.ScriptStackTrace)"
}
