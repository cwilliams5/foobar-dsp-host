# Automated tests must run HANDS-FREE: third-party components may open interactive UI
# (foobar DSP config popups are MODAL in the worker - a stuck dialog means a human is
# silently in the loop; this exact class bit the sibling winamp repo). Tests may only
# load the repo-built foo_dsp_ref. Third-party coverage is interactive by definition ->
# docs/SMOKE.md. This scans test sources for references to the components folder.
$repo = Split-Path $PSScriptRoot -Parent
$bad = @()
$testFiles = @(git -C $repo ls-files 'crates/*/tests/*.rs')
foreach ($f in $testFiles) {
    $hits = Select-String -Path (Join-Path $repo $f) -Pattern 'components[\\/]|foo_dsp_(?!ref)\w+\.dll|fb2k-component' -AllMatches
    foreach ($h in $hits) {
        $bad += ("{0}:{1}: references a third-party component: {2}" -f $f, $h.LineNumber, $h.Line.Trim())
    }
}
if ($bad.Count -gt 0) {
    $bad | Write-Output
    Write-Output "Gated tests may only use the repo-built foo_dsp_ref. Third-party/interactive coverage belongs in docs/SMOKE.md."
    exit 1
}
exit 0
