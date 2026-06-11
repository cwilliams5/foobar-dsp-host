# The whole gate, in order: worker presence -> cargo build -> pregate -> tests -> selftest.
# Run after every change; CI runs the same sequence. Any failure stops the run.
# (The C++ side rebuilds via ./build.ps1 - slow, so it is NOT run here; this gate fails
# loudly if the worker is missing rather than letting live tests silently self-skip.)
$ErrorActionPreference = 'Stop'
$repo = $PSScriptRoot
Push-Location $repo
try {
    Write-Host "=== [1/5] worker binaries present? ==="
    $worker = Join-Path $repo 'host\build\x64\Debug\foo_dsp_host.exe'
    if (-not (Test-Path $worker)) {
        Write-Host ("missing: {0}" -f $worker)
        Write-Host "build the C++ side first: ./build.ps1 Debug"
        exit 1
    }
    Write-Host "ok"

    Write-Host "=== [2/5] cargo build (workspace) ==="
    cargo build --workspace
    if ($LASTEXITCODE -ne 0) { exit 1 }

    Write-Host "=== [3/5] pregate ==="
    & (Join-Path $repo 'pregate.ps1')
    if ($LASTEXITCODE -ne 0) { exit 1 }

    Write-Host "=== [4/5] cargo test (incl. live worker + foo_dsp_ref bit-exact gains) ==="
    cargo test -p foobar-dsp-host
    if ($LASTEXITCODE -ne 0) { exit 1 }

    Write-Host "=== [5/5] lab selftest (headless; self-skips chain depth without components) ==="
    cargo run -p foobar-dsp-lab -- --selftest
    if ($LASTEXITCODE -ne 0) { exit 1 }

    Write-Host "=== test-all: ALL GREEN ==="
    exit 0
}
finally {
    Pop-Location
}
