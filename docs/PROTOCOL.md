# Worker protocol

The worker (`foo_dsp_host.exe --worker`) is the reusable unit. It speaks a tiny framed protocol over
**stdin** (parent → worker) and **stdout** (worker → parent). **stderr** is human-readable diagnostics
only — never parse it. Any language that can spawn a process and read/write its pipes can drive it.

**The worker hosts a *chain* of N ≥ 1 DSPs.** All stages are loaded into the one worker process and run in
series, in a single pass, by the SDK's own `dsp_manager` — one host↔worker round-trip per audio block for
the whole chain (no `.exe→.exe→.exe` piping, no per-stage re-serialization). A single DSP is just the N=1
case (`LOAD` is exactly `CHAN` with one stage and a default preset).

## Framing

- **Tags** are exactly **4 ASCII bytes** (note the trailing spaces on short tags, e.g. `"CFG "`,
  `"OK  "`, `"RST "`).
- Integers are **little-endian `u32`**.
- **Strings** = `u32` byte-length followed by that many **UTF-8** bytes.
- **Audio** = interleaved **`f32`** (`L R L R …`), little-endian, `frames × channels` samples.
- A **preset blob** = 16-byte owner GUID + the DSP's opaque `dsp_preset` data (empty data is valid).

A message is a tag followed by its payload. Requests and replies are 1:1 and synchronous (send one,
read one), except the modal `CFG ` (see below).

## Parent → worker

| Tag | Payload | Meaning |
|---|---|---|
| `CHAN` | `u32` sampleRate · `u32` channels · `u32` count, then `count`×{ `str` dllPath · `u32` entryIndex · `str` presetBlob } | Build an N-stage chain. Each stage loads its component, picks its `entryIndex`-th `dsp_entry`, and uses the given preset (empty blob = the entry's default). Replaces any current chain. |
| `LOAD` | `str` dllPath · `u32` sampleRate · `u32` channels · `u32` entryIndex | Convenience: set the chain to a **single** stage with its default preset (≡ `CHAN` count=1). |
| `RST ` | — | Drop the instantiated DSPs; the next `PROC` re-instantiates the whole chain fresh (use before re-processing a track after a config/seek). The chain config is kept. |
| `PROC` | `u32` frames · `f32[frames × channels]` | Stream one audio block through the **whole chain** (state preserved across calls). |
| `FLSH` | — | End-of-stream drain: every stage emits its buffered/look-ahead tail. |
| `CFG ` | `u32` stage | Open *that stage's* own modal config dialog (parented to a hidden host window), apply the result to the chain. Blocks until the user closes it. |
| `GPRE` | `u32` stage | Get that stage's current preset. |
| `SPRE` | `u32` stage · `str` presetBlob | Set that stage's preset (recycles the rest of the chain unchanged). |
| `QUIT` | — | Exit the worker. |

## Worker → parent

| Tag | Payload | Meaning |
|---|---|---|
| `COK ` | `u32` count, then `count`×`str` dspName | Chain built (reply to `CHAN`). |
| `LOK ` | `u32` status (1 = ok) · `str` dspName | Single-stage chain set (reply to `LOAD`). |
| `OK  ` | — | Generic ack (reply to `RST ` / `SPRE`). |
| `POK ` | `u32` framesOut · `f32[framesOut × channels]` | Processed audio (reply to `PROC` and `FLSH`). |
| `PRE ` | `str` presetBlob | A stage's preset blob (reply to `CFG ` / `GPRE`). |
| `ERR ` | `str` message | The preceding request failed (non-fatal; worker stays alive). |

## Semantics

- A chain member's component DLL is `LoadLibrary`'d **once** and its services registered into the host's
  registry; `dsp_manager` then instantiates each stage by owner-GUID lookup against that registry. Two
  stages of the same component are fine (each is a separate DSP instance). Re-`CHAN` replaces the chain
  config — but on a worker that has already processed audio, **send `RST ` after a re-`CHAN`, before the
  next `PROC`**: `dsp_manager` reconciles instances lazily and the previous chain keeps running until the
  reset re-instantiates the new one (empirically verified; the reference lab always RSTs before
  re-processing).
- `PROC` streams with the foobar `dsp::run` flag = 0 (state preserved). Look-ahead / buffering DSPs
  (levelers, reverbs) hold their tail until end-of-stream — send **`FLSH`** once after the last `PROC` to
  drain it (`dsp::FLUSH`). To re-process the same track (e.g. after `CFG `), send **`RST `** first so the
  whole chain re-instantiates cleanly, then `PROC` again.
- `POK`/`FLSH` may return a **different frame count** than was sent (resamplers, latency). A real-time
  consumer should buffer; the reference lab pre-processes the whole track then plays.
- A worker **crash** (a misbehaving plugin) closes the pipe — detect EOF on the worker's stdout and treat
  the chain as failed. This is the isolation guarantee. **Process granularity is your choice:** one worker
  per chain (efficient — the default here) *or* one worker per DSP (max isolation), with the host splicing
  block I/O between them. A 64-bit worker can't load 32-bit (x86) components, so a chain that mixes arches
  needs one worker per arch with the host bridging the boundary.

## Example (pseudocode)

```
spawn foo_dsp_host.exe --worker             # cwd must contain shared.dll
send  CHAN 44100 2 3
        "C:\...\foo_dsp_xgeq.dll"   0 ""     # stage 1 (default preset)
        "C:\...\foo_dsp_vlevel.dll" 0 ""     # stage 2
        "C:\...\foo_loudness_dsp.dll" 0 ""   # stage 3
recv  COK 3 "Graphic Equalizer" "VLevel" "Loudness Compensation DSP"
loop over the track in blocks:
  send PROC <frames> <f32 interleaved>
  recv POK <framesOut> <f32 interleaved>     # streamed through all 3 stages
send  FLSH ;  recv POK <tail>
# tweak stage 2 live:  send CFG 1 ; recv PRE <blob> ; send RST ; re-run PROC…
send  QUIT
```

The worker must run with its **working directory containing `shared.dll`** (the build stages it next to
`foo_dsp_host.exe`), and any component companion DLLs (e.g. `soxr64.dll`) reachable on the DLL search
path.
