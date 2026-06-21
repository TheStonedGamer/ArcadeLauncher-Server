$env:Path = [Environment]::GetEnvironmentVariable('Path','Machine') + ';' + [Environment]::GetEnvironmentVariable('Path','User')
foreach ($c in @('git','rustc','cargo','rustup')) {
  $p = Get-Command $c -ErrorAction SilentlyContinue
  if ($p) { $v = (& $c --version 2>&1) | Select-Object -First 1; "OK  $c : $v" }
  else { "MISSING  $c" }
}
$vswhere = "${env:ProgramFiles(x86)}\Microsoft Visual Studio\Installer\vswhere.exe"
if (Test-Path $vswhere) {
  $vc = & $vswhere -products * -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath
  if ($vc) { "OK  MSVC VC tools at: $vc" } else { "MISSING  MSVC VC.Tools workload" }
} else { "MISSING  vswhere.exe" }
