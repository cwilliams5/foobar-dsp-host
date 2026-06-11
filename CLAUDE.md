# CLAUDE.md — foobar-dsp-host

## What this is

A host for real **foobar2000 DSP components** (`foo_dsp_*`) outside foobar2000 — the first of its
kind. A C++ worker (`host/`, `foo_dsp_host.exe --worker`, built from the bundled BSD-2 SDK by
`build.ps1`, x64 + Win32) hosts whole chains via the SDK's own `dsp_manager`; the **`foobar-dsp-host`**
Rust crate (`crates/client`) drives it over framed stdio on the **`tagpipe`** transport (a git-pinned
dep from the sibling `winamp-vst2-dsp-host` repo). The egui `lab` is the reference consumer.
**This repo is the canonical home.** Downstream (the Resonance media player) depends on the crate and
vendors a pinned `host/`+`sdk/` snapshot: fixes land HERE first. Never "just patch it downstream."

## Build & test

- C++ side: `./build.ps1 [Debug|Release]` (MSBuild; both arches + `shared.dll` + the `foo_dsp_ref`
  fixture). Rust side: plain cargo (workspace at the root; `lab/` is a member).
- **`./test-all.ps1` is the whole gate**: worker presence → cargo build → pregate → `cargo test`
  (incl. LIVE integration vs the real worker + bit-exact `foo_dsp_ref` gains) → lab selftest.
- The worker must run with `shared.dll` beside it (the build stages it); the crate's spawn defaults
  the cwd accordingly. Components' companion DLLs are staged via `stage_companion_dlls`.
- The egui lab cannot be auto-verified, and `foo_dsp_ref` deliberately has no config popup — so ALL
  modal-dialog behavior needs the human checklist: `docs/SMOKE.md` — required before releases.

## Protocol invariants (learned the hard way)

- **After a re-`CHAN` on a live worker, send `RST ` before the next `PROC`** — `dsp_manager`
  reconciles instances lazily; the PREVIOUS chain keeps running until reset. (Cost a debugging round:
  "stages don't compound" was really this.)
- **Config popups are modal IN THE WORKER** — the worker is frozen while one is open; no other
  request can be serviced. The only out-of-process dismissal is `killer().kill()` + a fresh worker
  (the lab's kill-to-dismiss on rack edits). Any consumer's editor flow must do the same.
- `dsp_entry` enumeration order is LIFO vs declaration order — never assert entry order in tests.
- A 64-bit worker cannot load a 32-bit component: route by `dll_arch` (the crate's `PeArch`);
  chains are single-arch per worker.

## Wire changes (TATTOO)

The **canonical transport spec lives in the sibling repo** (`winamp-vst2-dsp-host/docs/TRANSPORT.md`);
`docs/TRANSPORT.md` here is a pointer — never duplicate the spec. Any wire-visible vocabulary change
MUST ship together: bump `VOCAB_VERSION` in `crates/client/src/lib.rs` **and** the C++ literals in
`host/main.cpp`'s handshake block, update `docs/PROTOCOL.md`, and keep worker + crate in lockstep
(the handshake's exact-match policy makes skew fail loudly at spawn — by design). The pregate
cross-checks all three.

## Pregate

`./pregate.ps1` auto-discovers `pregate/check_*.ps1` and blocks everything on failure (budget <30s).
**Prefer checks over rules** — machines enforce; rules explain judgment. New class of bug → write the
check FIRST while the bug is live proof, then fix. **Cage-modifier warning:** if a check catches a
legitimate error, fix the code, not the check — weakening a check to pass is a primary human review
point.

## Hard rules (the judgment kind)

- **`.ps1` files stay pure ASCII** — `run-lab.bat` invokes Windows PowerShell 5.1, which reads
  BOM-less files as CP-1252 (an em-dash byte parses as a closing quote and broke `build.ps1` once).
  Pregate-enforced.
- The vendored SDK is **BSD-2** — binary redistributions (releases!) MUST reproduce its copyright
  notice (`sdk/foobar2000/shared/../sdk-license.txt` ships in every release zip). The `/release`
  skill handles this; don't strip it.
- The SDK source itself is tracked **deliberately**; its prebuilt `shared-*.lib` import libs are the
  only tracked binaries (allowlisted in the artifacts check). Everything else built stays gitignored.
- Releases: use the `/release` skill. crates.io publishing is GATED until the downstream integration
  proves the API AND `tagpipe` is published (this crate's git dep must become a version dep first).
