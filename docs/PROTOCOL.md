# Worker protocol

The worker (`foo_dsp_host.exe --worker`) is the reusable unit. It speaks a tiny framed protocol over
**stdin** (parent → worker) and **stdout** (worker → parent). **stderr** is human-readable diagnostics
only — never parse it. Any language that can spawn a process and read/write its pipes can drive it.

## Framing

- **Tags** are exactly **4 ASCII bytes** (note the trailing spaces on short tags, e.g. `"CFG "`,
  `"OK  "`).
- Integers are **little-endian `u32`**.
- **Strings** = `u32` byte-length followed by that many **UTF-8** bytes.
- **Audio** = interleaved **`f32`** (`L R L R …`), little-endian, `frames × channels` samples.

A message is a tag followed by its payload. Requests and replies are 1:1 and synchronous (send one,
read one), except the modal `CFG ` (see below).

## Parent → worker

| Tag | Payload | Meaning |
|---|---|---|
| `LOAD` | `str` dllPath · `u32` sampleRate · `u32` channels · `u32` entryIndex | Load the component DLL, find its `entryIndex`-th `dsp_entry`, instantiate it with its default preset. |
| `SPRE` | `str` blob (16-byte owner GUID + preset bytes) | Set the preset and re-instantiate the DSP. |
| `PROC` | `u32` frames · `f32[frames × channels]` | Process one audio block (streaming — DSP state is preserved across calls). |
| `FLSH` | — | Drain the DSP's buffered/look-ahead tail at end-of-stream. |
| `CFG ` | — | Open the plugin's own modal config dialog (parented to a hidden host window). Blocks until the user closes it. |
| `GPRE` | — | Get the current preset. |
| `QUIT` | — | Exit the worker. |

## Worker → parent

| Tag | Payload | Meaning |
|---|---|---|
| `LOK ` | `u32` status (1 = ok) · `str` dspName | Load succeeded. |
| `OK  ` | — | Generic ack (e.g. after `SPRE`). |
| `POK ` | `u32` framesOut · `f32[framesOut × channels]` | Processed audio (reply to `PROC` and `FLSH`). |
| `PRE ` | `str` blob | A preset blob (reply to `CFG ` / `GPRE`). |
| `ERR ` | `str` message | The preceding request failed (non-fatal; worker stays alive). |

## Semantics

- One **persistent** `dsp` instance lives per `LOAD`/`SPRE`. `PROC` calls stream through it with state
  preserved (foobar `dsp::run` flags = 0).
- Look-ahead / buffering DSPs (levelers, reverbs) hold their tail until end-of-stream. Send **`FLSH`**
  once after the last `PROC` to drain it (it runs `dsp::run` with the `FLUSH` flag). **After `FLSH` the
  DSP is spent** — re-instantiate via `SPRE` / `CFG ` / a fresh `LOAD` before more `PROC`.
- `POK`/`FLSH` may return a **different frame count** than was sent (resamplers, latency). A real-time
  consumer should buffer; the reference lab pre-processes the whole track then plays.
- A worker **crash** (a misbehaving plugin) closes the pipe — detect EOF on the worker's stdout and
  treat the slot as failed. This is the isolation guarantee; run one worker per plugin.

## Example (pseudocode)

```
spawn foo_dsp_host.exe --worker          # cwd must contain shared.dll
send  LOAD "C:\...\foo_dsp_xgeq.dll" 44100 2 0
recv  LOK 1 "Graphic Equalizer"
loop over the track in blocks:
  send PROC <frames> <f32 interleaved>
  recv POK <framesOut> <f32 interleaved>
send  FLSH ;  recv POK <tail>
# optional: send CFG ; recv PRE <blob> ; SPRE <blob> ; re-run
send  QUIT
```

The worker must run with its **working directory containing `shared.dll`** (the build stages it next to
`foo_dsp_host.exe`), and any component companion DLLs (e.g. `soxr64.dll`) reachable on the DLL search
path.
