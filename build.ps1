# Build the foobar2000 DSP host: the x64 + x86 worker exes AND shared.dll, all from the bundled
# BSD-licensed foobar2000 SDK source. No foobar2000 install and no VS "C++ ATL" component required
# (shared/filedialogs_vista.cpp is patched to drop its lone ATL dependency).
# Usage:  .\build.ps1 [Debug|Release]
$ErrorActionPreference = 'Stop'
$cfg = if ($args.Count -ge 1) { $args[0] } else { 'Debug' }
$root = $PSScriptRoot

$msbuild = "C:\Program Files\Microsoft Visual Studio\2022\Community\MSBuild\Current\Bin\MSBuild.exe"
if (-not (Test-Path $msbuild)) {
  $vswhere = "${env:ProgramFiles(x86)}\Microsoft Visual Studio\Installer\vswhere.exe"
  if (Test-Path $vswhere) { $msbuild = & $vswhere -latest -products * -find "MSBuild\**\Bin\MSBuild.exe" | Select-Object -First 1 }
}
if (-not $msbuild -or -not (Test-Path $msbuild)) { throw "MSBuild not found. Install VS2022 with the Desktop C++ workload (x64 + x86)." }

$sharedProj = "$root\sdk\foobar2000\shared\shared.vcxproj"
$hostProj   = "$root\host\foo_dsp_host.vcxproj"

foreach ($plat in 'x64', 'Win32') {
  Write-Host "`n=== shared.dll ($plat, from BSD SDK source) ==="
  & $msbuild $sharedProj /nologo /m /v:m /p:Configuration=$cfg /p:Platform=$plat /p:PlatformToolset=v143
  if ($LASTEXITCODE -ne 0) { throw "shared.vcxproj ($plat) build failed" }

  Write-Host "=== worker foo_dsp_host.exe ($plat) ==="
  & $msbuild $hostProj /nologo /m /v:m /p:Configuration=$cfg /p:Platform=$plat /p:PlatformToolset=v143
  if ($LASTEXITCODE -ne 0) { throw "foo_dsp_host.vcxproj ($plat) build failed" }

  $exeDir = "$root\host\build\$plat\$cfg"
  # Stage the shared.dll that MATCHES this platform (x64 OutDir has a platform subdir; Win32 does not).
  # Must be deterministic per-arch — "newest shared.dll" picks the wrong one on an incremental re-run.
  $sdPath = if ($plat -eq 'x64') { "$root\sdk\foobar2000\shared\x64\$cfg\shared.dll" } else { "$root\sdk\foobar2000\shared\$cfg\shared.dll" }
  if (-not (Test-Path $sdPath)) { throw "shared.dll not found for ${plat}: $sdPath" }
  Copy-Item $sdPath (Join-Path $exeDir 'shared.dll') -Force
  Write-Host "  -> $exeDir\foo_dsp_host.exe  (+ shared.dll [$plat])"
}
Write-Host "`nOK.  x64 worker: host\build\x64\$cfg\foo_dsp_host.exe   x86 worker: host\build\Win32\$cfg\foo_dsp_host.exe"
