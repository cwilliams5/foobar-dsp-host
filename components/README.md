# components/

Drop your foobar2000 DSP components here, then run `run-lab.bat` (or `extract-components.ps1`).

- Accepted: **`.fb2k-component`** files. The build extracts each one's `x64/` DLL payload into
  `_extract/`, which the lab loads.
- **Nothing here is committed.** These are third-party plugins under their own licenses — bring your own.

Where to get DSP components: the official directory at
<https://www.foobar2000.org/components/tag/DSP> — e.g. a graphic equalizer, a resampler, loudness
compensation, stereo tools. Download the `.fb2k-component` and drop it in this folder.

32-bit-only (x86) components need the x86 worker; the build produces both x64 and x86.
