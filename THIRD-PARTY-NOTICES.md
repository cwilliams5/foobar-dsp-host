# Third-party notices

This repository's original code is MIT (`LICENSE`). It bundles / depends on the following.

## foobar2000 SDK — `sdk/`  (BSD-2-Clause)

Copyright (c) 2002-2025, Peter Pawlowski. Full text: `sdk/sdk-license.txt`. The `pfc` and `libPPUI`
sublibraries carry their own (zlib-style) notices in their folders.

The 2.x SDK is BSD-2-Clause: redistribution in source and binary form is permitted provided the
copyright notice and disclaimer are retained. The SDK is vendored so the host builds without a separate
download.

**Local modification:** `sdk/foobar2000/shared/filedialogs_vista.cpp` is patched to remove its only ATL
dependency (two `CComPtr` → the SDK's own `pfc::com_ptr_t`) so `shared.dll` builds without the Visual
Studio "C++ ATL" component. The change is marked `RESONANCE PATCH`; behavior is unchanged.

## Rust dependencies (the `lab/` reference client)

| Crate | License |
|---|---|
| `eframe` / `egui` | MIT OR Apache-2.0 |
| `cpal` | Apache-2.0 |
| `arc-swap` | MIT OR Apache-2.0 |
| `symphonia` (and codecs) | MPL-2.0 |

Canonical license text for each is on crates.io and in each crate's source. `symphonia` is MPL-2.0
(file-level copyleft): using it as a dependency does not affect the license of this repo's own code;
MPL-covered files remain MPL and their source is publicly available.

## foobar2000 components

DSP components (`.fb2k-component` / `foo_dsp_*.dll`) that this host loads are **third-party** and remain
under their own licenses. **None are included in this repository.** You supply your own.

## `sample/demo.wav`

An original synthesized clip created for this repo, dedicated to the public domain (CC0). See
`sample/README.md`.

## Trademark

"foobar2000" is a trademark of its respective owner. This project is unaffiliated with and not endorsed
by foobar2000; it describes interoperability only.
