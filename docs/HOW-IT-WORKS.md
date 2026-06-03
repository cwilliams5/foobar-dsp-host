# How it works

How a foobar2000 DSP component is hosted outside foobar2000. This is the part that's usually called
impractical; it isn't, for the DSP subset.

## Why it has a hard reputation

A foobar2000 component isn't a flat C ABI like a Winamp DSP or a VST. It's a COM-like **C++ service**
model: components register reference-counted, GUID-queried service objects (`service_base`) via static
initializers, and expect the host to provide the foobar2000 **core** services. Hosting one means
satisfying that SDK contract — which sounds like "reimplement foobar2000." For a DSP specifically, the
contract you must satisfy is small.

## The handshake

Every component DLL exports exactly one function:

```cpp
foobar2000_client* __cdecl foobar2000_get_interface(foobar2000_api* api, HINSTANCE hIns);
```

The host calls it, passing a **host-implemented `foobar2000_api`**, and gets back a `foobar2000_client`
(implemented by the SDK code linked into the component). The host then:

1. `client->get_version()` — ABI gate (v2.x components report 81).
2. `client->set_library_path(path, name)`.
3. `client->get_service_list()` — returns the head of the component's static `service_factory_base`
   linked list (every service the component registered, including its `dsp_entry`).
4. `client->services_init(true)`.

The host walks that factory list into a registry keyed by service-class GUID. The host itself calls its
*own* linked copy of the same export at startup to bring host-side services online.

## The host surface is ~11 methods

The only interface the host must author is **`foobar2000_api`** — 11 methods, of which three do the real
work:

```cpp
service_class_ref service_enum_find_class(const GUID&);                       // \
bool   service_enum_create(service_ptr_t<service_base>&, service_class_ref, t_size); //  > service resolution
t_size service_enum_get_count(service_class_ref);                             // /
// + get_main_window, is_main_thread, assert_main_thread, is_shutting_down,
//   is_initializing, get_profile_path, is_portable_mode_enabled, is_quiet_mode_enabled  (trivial)
```

The SDK's own `service_factory_base::enum_*` forward to `g_foobar2000_api->service_enum_*`, so a
GUID→factories registry backing those three methods is the whole resolution mechanism. The other eight
are scalars/flags you stub.

## The SDK does the heavy lifting — you LINK it

You don't reimplement the DSP machinery; you compile the SDK's own source (it's BSD) and call it:

- **`dsp_entry`** — found in the registry by `dsp_entry::class_guid`; `get_name`, `get_default_preset`,
  `instantiate`, `show_config_popup`.
- **`dsp` / `dsp_manager`** — `dsp_manager` is the exact chain driver foobar2000 uses. The worker drives
  it directly (see [Chaining](#chaining)); the standalone single-DSP modes call `dsp_entry::instantiate` +
  `dsp::run` as the minimal example.
- **`audio_chunk_impl` / `dsp_chunk_list_impl` / `dsp_preset_impl`** — concrete SDK types you feed.

## The audio model

`audio_sample` is **interleaved `f32`** (no quantization), with a sample rate + channel mask per chunk.
You fill a `dsp_chunk_list_impl`, call `dsp::run(list, track, flags)`, then read the (possibly mutated)
list back — a DSP may add, drop, or resize chunks.

**End-of-stream drain:** streaming with `flags = 0` preserves DSP state across blocks, but look-ahead /
buffering DSPs (levelers, reverbs) hold their tail until the stream ends. Run once more with
`dsp::FLUSH` to drain it. Skip this and such DSPs render silent — the single most important gotcha.

## Chaining

Users chain DSPs (EQ → leveler → loudness), and the SDK hands you the chain machinery for free:
`dsp_manager` (vendored as `sdk/.../dsp_manager.cpp`) is the **exact driver foobar2000 uses for its own
DSP chain**. You don't pipe one process into another — you load every chain member into the *one* worker,
hand `dsp_manager` a `dsp_chain_config` (an ordered list of `dsp_preset`s, each carrying its owner GUID +
opaque blob), and call `run()`:

```cpp
dsp_manager mgr;
mgr.set_config(chain);                    // chain = ordered dsp_preset list
mgr.run(&chunkList, track, flags, abort); // instantiates each stage by GUID, runs them in series
```

- **One process, one pass, f32 throughout.** `dsp_manager::run` walks the chain, instantiating each stage
  via `dsp_entry::g_instantiate` (a global GUID lookup against the service registry) and feeding the chunk
  list through each in turn — no inter-stage quantization or re-serialization. It also gives you, for free,
  the recycle optimization (only re-instantiate a stage whose preset changed), per-stage latency
  accounting, and per-stage flush ordering.
- **The one requirement:** every chained component's DLL must be loaded + registered in the *same*
  process, because the lookup is process-local. The worker loads each once (it caches by path — a second
  `registerList` of the same factory list would double its factories) and `dsp_manager` finds them all.
- **A single DSP is just N=1.** The worker always drives a `dsp_manager`; hosting one DSP is a 1-stage
  chain (identical audio to a bare `dsp::run`, with cleaner re-init via `dsp_manager::close()`), so there
  is no separate "single" code path to drift.
- **What you DON'T get for free:** the whole-chain *editor* dialog (`dsp_config_manager::configure_popup`)
  is a foobar2000 **core service** implemented in the player, not in the linkable SDK — so the chain's
  ordering/add/remove UI is yours to build (the lab's rack is one example). Each stage's *own* config
  dialog (`dsp_entry::show_config_popup`) is component-side and works as usual.
- **Crash isolation vs. efficiency is an orthogonal knob.** One worker per chain (the default) is
  efficient but a crash in any stage drops the whole chain (the host still survives). One worker per DSP
  maximizes isolation at the cost of an IPC hop between stages. A chain that mixes x86 and x64 components
  *must* split across two workers (a process can't load both bitnesses). See `docs/PROTOCOL.md`.

## `configStore` stub

Most DSPs need nothing beyond the above. Some (e.g. resamplers) call `fb2k::configStore::get()` inside
`get_default_preset` to read a stored default rate; with no provider, the singleton lookup fails and they
bail. A **pass-through `configStore`** (return the caller's default, no-op writes), registered via a
static factory, fixes them. It's ~25 lines.

## The one real caveat: the track handle

`dsp::run`'s second argument is a `metadb_handle_ptr` (the current track). `dsp_impl_base` only *stores*
it, so metadata-agnostic DSPs — gain, EQ, resamplers, levelers, exciters, stereo tools — work with a
**null handle** (this is the SDK's own default). A DSP that genuinely inspects the track would need the
`metadb` subsystem stood up; that's a much larger effort and is out of scope here.

## `shared.dll`

Components import a handful of leaf utilities from `shared.dll` (pfc helpers, `audio_math`). It's part of
the SDK and builds from the bundled BSD source — **no foobar2000 install needed**. The SDK's
`shared/filedialogs_vista.cpp` was the only file using ATL (two `CComPtr` on one line); this repo patches
it to the SDK's own `pfc::com_ptr_t` so `shared.dll` builds **without the VS "C++ ATL" component**. The
patch is marked `RESONANCE PATCH` and behavior is identical.

## Isolation, and x86 vs x64

The plugin runs in a separate **worker process** for crash isolation (real plugins do crash — foobar's
own VST adapter is out-of-process for the same reason) and because a 64-bit process cannot `LoadLibrary`
a 32-bit DLL. foobar2000 v2 components are x64; v1.x components are x86. Two workers are built; pick the
one matching the component's arch. The parent drives whichever over the stdio protocol
(`docs/PROTOCOL.md`).

## Licensing reality

The foobar2000 **2.x SDK is BSD-2-Clause** — vendorable and even redistributable with attribution (the
1.x SDK's usage-restriction clause was dropped in 2.x). So no clean-room reimplementation is needed; the
SDK source is bundled under `sdk/`. Loading a third-party component over its ABI doesn't entangle your
license with the component's (the Ardour/Carla model). The components themselves are not redistributed
here.
