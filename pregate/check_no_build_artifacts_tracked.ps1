# Blocks build artifacts from being committed. Real failure: git add -A swept
# host/ref_dsp/build/ intermediates (.recipe files with absolute machine paths) into a
# commit; caught only by a manual scrub. Allowlist deliberate binaries with a reason.
$repo = Split-Path $PSScriptRoot -Parent
$deny = '\.(exe|dll|pdb|obj|lib|exp|ilk|iobj|ipdb|recipe|tlog)$'
$allow = @(
    'sdk/foobar2000/shared/shared-*.lib',                          # the vendored SDK's official import libs (BSD-2; the host links them)
    'sdk/foobar2000/foo_input_validator/foo_input_validator.dll'   # ships INSIDE the vendored SDK distribution; kept for drop fidelity
)
$bad = @(git -C $repo ls-files | Where-Object { $_ -match $deny } | Where-Object {
    $f = $_
    -not ($allow | Where-Object { $f -like $_ })
})
if ($bad.Count -gt 0) {
    $bad | ForEach-Object { Write-Output ("tracked build artifact: {0}" -f $_) }
    Write-Output "Remove it (git rm --cached <file>) and extend .gitignore."
    Write-Output "If the binary is deliberate, add it to this check's allowlist WITH a justification."
    exit 1
}
exit 0
