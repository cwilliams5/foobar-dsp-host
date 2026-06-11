# Manual smoke checklist

The lab's interactive layer — **modal** config dialogs (foobar configs freeze the worker while
open), kill-to-dismiss, A/B — is the one thing no automated check verifies (`foo_dsp_ref`
deliberately has no config popup, and egui resists automation). This checklist encodes the real
failures found by hand-testing; **run it before every release** and after any change to the
engine thread, the worker's CFG path, or the kill-to-dismiss flow.

Launch: `run-lab.bat`. Have 2–3 real x64 components in `components\` (Noise Sharpening / VLevel /
Loudness are the canonical trio).

1. **Chain basics:** add two DSPs → Apply → hear the processed loop; A/B toggles instantly;
   IN/OUT meters move; Δ dB reported.
2. **Modal config round-trip:** ⚙ a stage → its OWN dialog opens (the worker is frozen — audio
   keeps playing from the pre-processed buffer) → adjust → OK → the track re-processes and the
   change is audible; the preset persists across a lab restart (`presets/`).
3. **The kill-to-dismiss scenario (the Y/Z bug):** add X and Y → Apply → open Y's config →
   *while it's open*, add Z and remove X and Y → Apply → the open dialog is dismissed
   automatically and the resulting chain is **Z only**. No stuck dialog, no stale chain.
4. **Remove-while-config-open:** open a stage's config, click its Del → the dialog is dismissed,
   the rack marks edited; Apply rebuilds cleanly.
5. **Crash containment:** if a known-crasher is available (SoX-class), apply it → the lab
   survives and reports the failure; the rack can be rebuilt.
6. **x86 routing (if a 32-bit component is present):** a chain of x86 components builds via the
   Win32 worker; a mixed-arch chain is refused with the teaching message.

Any failure here is a release blocker. New interactive failure class → add it here, and ask
whether any part can become a pregate check or a wire-level test first (checks-over-rules).
