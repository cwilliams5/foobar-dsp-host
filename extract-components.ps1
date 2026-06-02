# Extract component DLLs from every .fb2k-component (a zip) in components\ into _extract\x64 and
# _extract\x86 (plus any companion DLLs alongside, per arch), so the lab can load either bitness.
# Idempotent.
$ErrorActionPreference = 'Stop'
$root = $PSScriptRoot
Add-Type -AssemblyName System.IO.Compression.FileSystem
$ext64 = Join-Path $root "_extract\x64"
$ext86 = Join-Path $root "_extract\x86"
New-Item -ItemType Directory -Force $ext64, $ext86 | Out-Null
Get-ChildItem (Join-Path $root "components\*.fb2k-component") -ErrorAction SilentlyContinue | ForEach-Object {
    $z = [System.IO.Compression.ZipFile]::OpenRead($_.FullName)
    foreach ($e in $z.Entries) {
        if ([string]::IsNullOrEmpty($e.Name)) { continue }            # directory entry
        if ($e.FullName -like 'x64/*.dll') {
            [System.IO.Compression.ZipFileExtensions]::ExtractToFile($e, (Join-Path $ext64 $e.Name), $true)   # 64-bit payload
        } elseif (($e.FullName -notmatch '/') -and ($e.FullName -like '*.dll')) {
            [System.IO.Compression.ZipFileExtensions]::ExtractToFile($e, (Join-Path $ext86 $e.Name), $true)   # root = 32-bit payload
        }
    }
    $z.Dispose()
}
$n64 = @(Get-ChildItem $ext64 -Filter *.dll -ErrorAction SilentlyContinue).Count
$n86 = @(Get-ChildItem $ext86 -Filter *.dll -ErrorAction SilentlyContinue).Count
if ($n64 -or $n86) { Write-Host "Extracted: $n64 DLL(s) -> _extract\x64, $n86 DLL(s) -> _extract\x86" }
else { Write-Host "No components yet. Drop .fb2k-component files into the components\ folder and re-run." }
