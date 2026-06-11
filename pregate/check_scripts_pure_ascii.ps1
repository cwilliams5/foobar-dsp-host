# Tracked .ps1 files must be pure ASCII. Real failure: an em-dash in build.ps1 broke the
# user's Windows PowerShell 5.1 (run via a .bat launcher) at PARSE time - 5.1 reads
# BOM-less files as CP-1252, where the em-dash's 0x94 byte decodes to a smart closing
# quote that terminates strings early. pwsh 7 (UTF-8 default) masks the problem, so it
# only breaks for the USER. (.bat files are exempt: their unicode is in cmd-tolerant REM
# lines; revisit if a .bat string ever breaks.)
$repo = Split-Path $PSScriptRoot -Parent
$bad = @()
foreach ($f in (git -C $repo ls-files '*.ps1')) {
    $bytes = [System.IO.File]::ReadAllBytes((Join-Path $repo $f))
    for ($i = 0; $i -lt $bytes.Length; $i++) {
        if ($bytes[$i] -gt 127) {
            $bad += ("{0}: non-ASCII byte 0x{1:X2} at offset {2}" -f $f, $bytes[$i], $i)
            break
        }
    }
}
if ($bad.Count -gt 0) {
    $bad | Write-Output
    Write-Output "Replace with ASCII equivalents (em-dash -> '-', smart quotes -> straight quotes)."
    exit 1
}
exit 0
