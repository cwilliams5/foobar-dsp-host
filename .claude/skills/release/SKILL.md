---
name: release
description: Cut a GitHub release - full gate, version bump, Release builds (both arches), package the worker zip with the required BSD-2 SDK attribution, tag, gh release create. Stops before any cargo publish (crates.io is gated).
disable-model-invocation: true
---

# /release — cut a GitHub release

Deliberate-invocation only. Releases ship the worker binaries to non-Rust consumers (any
language that can spawn a process + speak the framed protocol).

## Preconditions (verify ALL before touching versions)

1. Working tree clean, on `main`, synced with `origin/main`.
2. `./build.ps1 Debug` artifacts present and `./test-all.ps1` green.
3. **Ask the user to confirm they have run `docs/SMOKE.md`** on this build — modal-dialog
   behavior cannot be auto-verified (`foo_dsp_ref` has no popup by design). Do not proceed on
   an unconfirmed smoke.

## 1. Version

Ask the user: patch (`0.1.0 → 0.1.1`, compatible fix) or minor (`0.1.0 → 0.2.0`, breaking — in
0.x the minor is the breaking slot). Bump:

- `crates/client/Cargo.toml` `version`.
- The C++ worker's handshake **ident string** in `host/main.cpp` (`"foo_dsp_host-worker X.Y.Z (..-bit)"`).
- NOTE: a wire-visible change additionally requires the `VOCAB_VERSION` bump in BOTH the Rust
  const and the C++ literals + `docs/PROTOCOL.md` (pregate cross-checks) — crate semver and wire
  versions are separate axes.

Commit: `release: vX.Y.Z`.

## 2. Release builds + verification

```powershell
./build.ps1 Release          # both worker arches + shared.dll + foo_dsp_ref
cargo build --release -p foobar-dsp-host
# Verify the RELEASE worker (tests above ran Debug): point the live suite at it.
$env:FOO_DSP_HOST_WORKER = "$PWD\host\build\x64\Release\foo_dsp_host.exe"
cargo test -p foobar-dsp-host --test worker_integration
Remove-Item Env:FOO_DSP_HOST_WORKER
```
Require: the suite green (handshake/ident, recoverable errors, passthrough, exit contracts run
against the Release worker; the foo_dsp_ref gain test uses the Debug-path DLL fixture — the
worker under test is what matters here).

## 3. Package — BSD-2 attribution is REQUIRED

The bundled foobar2000 SDK is BSD-2-Clause: **binary redistributions must reproduce its
copyright notice.** Every zip MUST contain `sdk-license.txt` (copy from `sdk/`) and
`THIRD-PARTY-NOTICES.md`. Never strip these.

One zip: `foobar-dsp-host-vX.Y.Z-windows.zip`:

```
x64/foo_dsp_host.exe        x64/shared.dll        x64/foo_dsp_ref.dll
x86/foo_dsp_host.exe        x86/shared.dll        x86/foo_dsp_ref.dll
LICENSE   THIRD-PARTY-NOTICES.md   sdk-license.txt   README-RELEASE.txt
```

(x86 binaries from `host\build\Win32\Release\` + `host\ref_dsp\build\Win32\Release\`.)
`README-RELEASE.txt` (write it): one paragraph per file + "the worker must run with shared.dll
beside it" + "drive it from any language: docs/PROTOCOL.md (+ the canonical transport spec
linked from docs/TRANSPORT.md)" + the repo URL.

Stage in a temp dir, `Compress-Archive`, verify the zip lists exactly those files.

## 4. Tag + publish the release

```powershell
git tag vX.Y.Z
git push origin main --tags
gh release create vX.Y.Z .\foobar-dsp-host-vX.Y.Z-windows.zip `
  --title "vX.Y.Z" --notes "<3-6 bullet summary of changes since the last tag (git log <prev>..HEAD)>"
```

## 5. STOP — no cargo publish

crates.io publishing is GATED: the downstream integration (Resonance) must prove the API first,
AND `tagpipe` must be on crates.io so this crate's git dependency can become a version
dependency (crates.io rejects git deps). When the gate opens, the publish follows the
maintainer's playbook; this skill grows that step then. Until then: GitHub releases only.
