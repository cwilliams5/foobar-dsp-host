# Pregate: auto-discovers pregate/check_*.ps1 and runs them; any failure blocks everything.
# Philosophy: prefer checks over rules (machines enforce; rules explain judgment); when a new
# class of bug appears, write the check FIRST while the bug is live proof, then fix. If a check
# catches a legitimate error, fix the CODE, not the check. Budget: < 30 seconds total.
$ErrorActionPreference = 'Continue'
$checksDir = Join-Path $PSScriptRoot 'pregate'
$checks = Get-ChildItem $checksDir -Filter 'check_*.ps1' | Sort-Object Name
$failed = 0
$sw = [System.Diagnostics.Stopwatch]::StartNew()
Write-Host ("--- Pregate: {0} check(s) ---" -f $checks.Count)
foreach ($c in $checks) {
    $out = & $c.FullName 2>&1
    if ($LASTEXITCODE -ne 0) {
        $failed++
        Write-Host ("[FAIL] {0}" -f $c.Name)
        $out | ForEach-Object { Write-Host ("       {0}" -f $_) }
    } else {
        Write-Host ("[ ok ] {0}" -f $c.Name)
    }
}
$sw.Stop()
Write-Host ("--- Pregate: {0}/{1} passed in {2:n1}s ---" -f ($checks.Count - $failed), $checks.Count, $sw.Elapsed.TotalSeconds)
if ($failed -gt 0) { exit 1 }
exit 0
