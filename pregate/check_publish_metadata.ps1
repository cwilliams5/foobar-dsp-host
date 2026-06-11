# Every workspace member is either publish = false or fully publishable (description +
# license + repository). Guards the crates.io publish surface. Extra rule here: the
# publishable client crate may NOT keep a git dependency at publish time - flagged as a
# reminder, enforced for real by cargo publish itself.
$repo = Split-Path $PSScriptRoot -Parent
$fail = @()
$members = @()
$members += Get-ChildItem (Join-Path $repo 'crates') -Directory | ForEach-Object { Join-Path $_.FullName 'Cargo.toml' }
$members += Join-Path $repo 'lab\Cargo.toml'
foreach ($toml in $members) {
    if (-not (Test-Path $toml)) { continue }
    $raw = Get-Content $toml -Raw
    $name = if ($raw -match 'name\s*=\s*"([^"]+)"') { $Matches[1] } else { $toml }
    if ($raw -match 'publish\s*=\s*false') { continue }
    foreach ($field in 'description', 'license', 'repository') {
        if ($raw -notmatch ($field + '\s*=')) {
            $fail += ("{0}: publishable crate missing '{1}'" -f $name, $field)
        }
    }
}
if ($fail.Count -gt 0) {
    $fail | Write-Output
    Write-Output "Add the missing metadata, or mark the crate 'publish = false' with a comment saying why."
    exit 1
}
exit 0
