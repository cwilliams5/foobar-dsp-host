# The wire versions live in THREE places that must agree: the Rust crate const, the C++
# worker's handshake literals, and the PROTOCOL.md claim. The handshake makes a code-side
# skew fail loudly at runtime; this catches all three statically so a vocab bump can't
# ship half-done across the language boundary.
$repo = Split-Path $PSScriptRoot -Parent
$fail = @()

function Get-Match([string]$path, [string]$pattern) {
    $m = Select-String -Path $path -Pattern $pattern | Select-Object -First 1
    if (-not $m) { return $null }
    return [int]$m.Matches[0].Groups[1].Value
}

$rustVocab = Get-Match (Join-Path $repo 'crates\client\src\lib.rs') 'pub const VOCAB_VERSION: u32 = (\d+)'
$cppTransport = Get-Match (Join-Path $repo 'host\main.cpp') 'wrU32\((\d+)\); // TRANSPORT_VERSION'
$cppVocab = Get-Match (Join-Path $repo 'host\main.cpp') 'wrU32\((\d+)\); // VOCAB_VERSION'
if ($null -eq $rustVocab) { Write-Output "could not parse VOCAB_VERSION from crates/client/src/lib.rs"; exit 1 }
if ($null -eq $cppTransport -or $null -eq $cppVocab) {
    Write-Output "could not parse the handshake literals from host/main.cpp (wrU32(N); // TRANSPORT_VERSION / VOCAB_VERSION)"
    exit 1
}

if ($cppVocab -ne $rustVocab) {
    $fail += ("C++ worker VOCAB_VERSION ({0}) != Rust crate VOCAB_VERSION ({1})" -f $cppVocab, $rustVocab)
}
if ($cppTransport -ne 1) {
    $fail += ("C++ worker TRANSPORT_VERSION ({0}) != 1 - a transport bump starts in the canonical spec (winamp-vst2-dsp-host/docs/TRANSPORT.md) and tagpipe, then lands here" -f $cppTransport)
}

$protoDoc = Get-Content (Join-Path $repo 'docs\PROTOCOL.md') -Raw
# Accepts both bolding styles: `VOCAB_VERSION` is **N**  /  **`VOCAB_VERSION` is N**
if ($protoDoc -notmatch ('VOCAB_VERSION.{0,8}is \*{0,2}' + $rustVocab + '(\*\*|\b)')) {
    $fail += ("docs/PROTOCOL.md does not declare 'VOCAB_VERSION is {0}'" -f $rustVocab)
}

if ($fail.Count -gt 0) {
    $fail | Write-Output
    Write-Output "A wire-version bump must update the Rust const, the C++ literals, and the doc in one commit."
    exit 1
}
exit 0
