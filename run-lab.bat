@echo off
rem ============================================================================
rem foobar-dsp-host — build everything and launch the live A/B lab.
rem
rem Drop any foobar2000 DSP components (.fb2k-component) into the components\ folder
rem first. Then double-click this. In the window: pick a DSP, Play, toggle A/B
rem (Bypass vs Processed), Open config to use the plugin's own dialog. Plays the
rem CC0 clip in sample\ (or drop your own test.mp3/.flac/.wav in this folder).
rem
rem Requires: Visual Studio 2022 with the Desktop C++ workload (x64 + x86), and
rem Rust/cargo. No foobar2000 install and no ATL component needed.
rem ============================================================================
cd /d "%~dp0"

echo [1/3] Building x64 + x86 workers + shared.dll (from the bundled BSD SDK)...
powershell -NoProfile -ExecutionPolicy Bypass -File "%~dp0build.ps1" Debug || goto err

echo [2/3] Extracting components + building the lab...
powershell -NoProfile -ExecutionPolicy Bypass -File "%~dp0extract-components.ps1" || goto err
cargo build --manifest-path "%~dp0lab\Cargo.toml" || goto err

echo [3/3] Launching the lab...
"%~dp0lab\target\x86_64-pc-windows-msvc\debug\foobar_dsp_lab.exe"
goto :eof

:err
echo.
echo Build/setup failed. Ensure VS2022 (Desktop C++, x64 + x86) and Rust/cargo are installed.
pause
