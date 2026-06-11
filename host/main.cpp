// foo_dsp_host — minimal x64 in-process host for foobar2000 DSP components.
//
// Single-DSP modes (one component DLL — the minimal teaching example):
//   foo_dsp_host <dll>                          list dsp_entries + render battery (default preset)
//   foo_dsp_host <dll> --config                 open the plugin's OWN config dialog (modal), then
//                                               render the battery with the configured preset
//   foo_dsp_host <dll> --preset-in <file>       render using a saved dsp_preset blob
//   foo_dsp_host <dll> --config --preset-out f  open config, save the resulting preset blob to f
//   foo_dsp_host <dll> --in <16bit.wav>         also run a real WAV through the DSP
//   foo_dsp_host <dll> --entry N                pick the Nth dsp_entry (default 0)
//   foo_dsp_host <dll> --no-render              skip the audio battery (e.g. config-only)
//
// Chain mode (N components in series, via the SDK's own dsp_manager — one process, one pass, f32):
//   foo_dsp_host --chain a.dll b.dll c.dll      render the battery through the whole chain, showing
//                                               the cumulative effect as each stage is added
//
// IPC worker (the reusable unit; always hosts a chain, N>=1): foo_dsp_host --worker  (see runWorker)
//
// Phase-0/1 spike. See ../RESEARCH.md + ../FINDINGS.md. BSD foobar2000 SDK vendored under ../sdk.
// Per-component FFI calls are wrapped in try/catch; the .vcxproj builds with /EHa so a hard fault
// (AV / fast-fail) in a plugin is contained + reported rather than killing the host (in-proc lab
// convenience; production isolates each plugin in a subprocess — see plan-dsp-host-bridge).

#include <SDK/foobar2000.h>
#include <SDK/component.h>
#include <SDK/dsp.h>
#include <SDK/dsp_manager.h> // the SDK's own DSP-chain driver (set_config + run) — chain hosting

#include <SDK/configStore.h> // fb2k::configStore (stubbed below)
#include <windows.h>
#include <cstdio>
#include <cstdint>
#include <cmath>
#include <vector>
#include <string>
#include <map>
#include <memory>
#include <io.h>
#include <fcntl.h>

extern "C" foobar2000_client* __cdecl foobar2000_get_interface(foobar2000_api*, HINSTANCE);

namespace {

struct Bucket { GUID guid; std::vector<service_factory_base*> factories; };

void guidStr(const GUID& g, char* out) {
    sprintf_s(out, 48, "%08lX-%04X-%04X-%02X%02X-%02X%02X%02X%02X%02X%02X", g.Data1, g.Data2, g.Data3,
              g.Data4[0], g.Data4[1], g.Data4[2], g.Data4[3], g.Data4[4], g.Data4[5], g.Data4[6], g.Data4[7]);
}

class HostApi : public foobar2000_api {
public:
    std::vector<std::unique_ptr<Bucket>> classes;
    HWND mainWnd = NULL;
    std::string profile = "file://C:\\Temp\\foo_dsp_host";

    Bucket* find(const GUID& g) { for (auto& b : classes) if (b->guid == g) return b.get(); return nullptr; }
    void registerList(service_factory_base* head) {
        for (service_factory_base* f = head; f; f = f->__internal__next) {
            const GUID& g = f->get_class_guid();
            Bucket* b = find(g);
            if (!b) { classes.push_back(std::make_unique<Bucket>()); b = classes.back().get(); b->guid = g; }
            b->factories.push_back(f);
        }
    }
    bool logMisses = false;
    service_class_ref service_enum_find_class(const GUID& g) override {
        Bucket* b = find(g);
        if (!b && logMisses) { char s[48]; guidStr(g, s); fprintf(stderr, "[miss] %s\n", s); }
        return (service_class_ref)b;
    }
    t_size service_enum_get_count(service_class_ref c) override { auto b = (const Bucket*)c; return b ? b->factories.size() : 0; }
    bool service_enum_create(service_ptr_t<service_base>& out, service_class_ref c, t_size i) override {
        auto b = (const Bucket*)c; if (!b || i >= b->factories.size()) return false;
        b->factories[i]->instance_create(out); return out.is_valid();
    }
    fb2k::hwnd_t get_main_window() override { return mainWnd; }
    bool assert_main_thread() override { return true; }
    bool is_main_thread() override { return true; }
    bool is_shutting_down() override { return false; }
    const char* get_profile_path() override { return profile.c_str(); }
    bool is_initializing() override { return false; }
    bool is_portable_mode_enabled() override { return true; }
    bool is_quiet_mode_enabled() override { return false; }
};

// Minimal fb2k::configStore — pass-through: get* return the caller's default, set*/delete* are no-ops.
// Enough for DSPs that read a stored default in get_default_preset() (the SoX/ART resamplers crashed
// here when the singleton lookup found no provider). Registered via the static factory below, which
// the host's own __internal__list registration harvests into the service registry.
class HostConfigStore : public fb2k::configStore {
public:
    fb2k::objRef acquireTransactionScope() override { return fb2k::objRef(); }
    void commitBlocking() override {}
    int64_t getConfigInt(const char*, int64_t defVal) override { return defVal; }
    void setConfigInt(const char*, int64_t) override {}
    void deleteConfigInt(const char*) override {}
    fb2k::stringRef getConfigString(const char*, fb2k::stringRef defVal) override { return defVal; }
    void setConfigString(const char*, const char*) override {}
    void deleteConfigString(const char*) override {}
    fb2k::memBlockRef getConfigBlob(const char*, fb2k::memBlockRef defVal) override { return defVal; }
    void setConfigBlob(const char*, const void*, size_t) override {}
    void setConfigBlob(const char*, fb2k::memBlockRef) override {}
    void deleteConfigBlob(const char*) override {}
    double getConfigFloat(const char*, double defVal) override { return defVal; }
    void setConfigFloat(const char*, double) override {}
    void deleteConfigFloat(const char*) override {}
    void addNotify(const char*, fb2k::configStoreNotify*) override {}
    void removeNotify(const char*, fb2k::configStoreNotify*) override {}
    fb2k::arrayRef listDomainValues(const char*, bool) override { return fb2k::arrayRef(); }
    void callNotify(const char*) override {}
};
static service_factory_single_t<HostConfigStore> g_configStoreFactory;

HostApi hostApi; // shared by the standalone modes (main) and the IPC worker (runWorker)

// ---------------- WAV I/O (16-bit PCM interleaved) ----------------
void writeWav16(const std::string& path, const std::vector<float>& inter, unsigned nch, unsigned srate) {
    if (nch == 0) nch = 2;
    FILE* f = nullptr; if (fopen_s(&f, path.c_str(), "wb") || !f) return;
    auto u32 = [&](uint32_t v) { fwrite(&v, 4, 1, f); }; auto u16 = [&](uint16_t v) { fwrite(&v, 2, 1, f); };
    uint32_t dataBytes = (uint32_t)inter.size() * 2;
    fwrite("RIFF", 1, 4, f); u32(36 + dataBytes); fwrite("WAVE", 1, 4, f);
    fwrite("fmt ", 1, 4, f); u32(16); u16(1); u16((uint16_t)nch); u32(srate); u32(srate * nch * 2); u16((uint16_t)(nch * 2)); u16(16);
    fwrite("data", 1, 4, f); u32(dataBytes);
    for (float s : inter) { long v = lrintf(s * 32767.0f); if (v > 32767) v = 32767; if (v < -32768) v = -32768; int16_t q = (int16_t)v; fwrite(&q, 2, 1, f); }
    fclose(f);
}
bool readWav16(const std::string& path, std::vector<float>& out, unsigned& nch, unsigned& srate) {
    FILE* f = nullptr; if (fopen_s(&f, path.c_str(), "rb") || !f) return false;
    char tag[4]; uint32_t sz; bool ok = false; uint16_t fmt = 0, ch = 2, bits = 16; uint32_t sr = 44100;
    fread(tag, 1, 4, f); fread(&sz, 4, 1, f); fread(tag, 1, 4, f); // RIFF <sz> WAVE
    while (fread(tag, 1, 4, f) == 4 && fread(&sz, 4, 1, f) == 1) {
        if (memcmp(tag, "fmt ", 4) == 0) {
            uint16_t blk; uint32_t br; fread(&fmt, 2, 1, f); fread(&ch, 2, 1, f); fread(&sr, 4, 1, f); fread(&br, 4, 1, f); fread(&blk, 2, 1, f); fread(&bits, 2, 1, f);
            if (sz > 16) fseek(f, sz - 16, SEEK_CUR);
        } else if (memcmp(tag, "data", 4) == 0) {
            if (fmt == 1 && bits == 16) {
                size_t n = sz / 2; out.resize(n);
                std::vector<int16_t> raw(n); fread(raw.data(), 2, n, f);
                for (size_t i = 0; i < n; i++) out[i] = raw[i] / 32768.0f;
                nch = ch ? ch : 2; srate = sr ? sr : 44100; ok = true;
            }
            break;
        } else { fseek(f, sz, SEEK_CUR); }
    }
    fclose(f); return ok;
}

// ---------------- dsp_preset blob file I/O ----------------
void savePreset(const std::string& path, const dsp_preset& p) {
    FILE* f = nullptr; if (fopen_s(&f, path.c_str(), "wb") || !f) return;
    GUID g = p.get_owner(); uint32_t sz = (uint32_t)p.get_data_size();
    fwrite(&g, sizeof(GUID), 1, f); fwrite(&sz, 4, 1, f); if (sz) fwrite(p.get_data(), 1, sz, f);
    fclose(f);
}
bool loadPreset(const std::string& path, dsp_preset_impl& p) {
    FILE* f = nullptr; if (fopen_s(&f, path.c_str(), "rb") || !f) return false;
    GUID g; uint32_t sz = 0; if (fread(&g, sizeof(GUID), 1, f) != 1 || fread(&sz, 4, 1, f) != 1) { fclose(f); return false; }
    std::vector<uint8_t> data(sz); if (sz) fread(data.data(), 1, sz, f); fclose(f);
    p.set_owner(g); p.set_data(data.data(), sz); return true;
}

// ---------------- signal generation + analysis ----------------
const unsigned SR = 44100, NCH = 2; const size_t FR = SR; // 1.0 s stereo
std::vector<float> genSine(float hz) { std::vector<float> v(FR * NCH); for (size_t k = 0; k < FR; k++) { float s = 0.5f * sinf(6.2831853f * hz * k / SR); v[k * 2] = s; v[k * 2 + 1] = s; } return v; }
std::vector<float> genSweep() { std::vector<float> v(FR * NCH); double ph = 0; for (size_t k = 0; k < FR; k++) { double f = 20.0 * pow(1000.0, (double)k / FR); ph += 6.2831853 * f / SR; float s = 0.5f * (float)sin(ph); v[k * 2] = s; v[k * 2 + 1] = s; } return v; }
std::vector<float> genNoise() { std::vector<float> v(FR * NCH); uint32_t r = 0x1234567u; for (size_t k = 0; k < FR * NCH; k++) { r = r * 1664525u + 1013904223u; v[k] = ((int)(r >> 9) / 4194304.0f - 1.0f) * 0.3f; } return v; }
double rmsDb(const std::vector<float>& x) { if (x.empty()) return -1000; double s = 0; for (float v : x) s += (double)v * v; double r = sqrt(s / x.size()); return r > 1e-9 ? 20.0 * log10(r) : -1000.0; }
double peakDb(const std::vector<float>& x) { double m = 0; for (float v : x) { double a = fabs(v); if (a > m) m = a; } return m > 1e-9 ? 20.0 * log10(m) : -1000.0; }

// Run one signal through a FRESH dsp instance (no state carryover between battery items).
bool processSignal(dsp_entry::ptr entry, const dsp_preset& preset, const std::vector<float>& in,
                   std::vector<float>& out, unsigned& outSr, unsigned& outCh) {
    service_ptr_t<dsp> d; if (!entry->instantiate(d, preset)) return false;
    dsp_chunk_list_impl list; list.add_item()->set_data_32(in.data(), in.size() / NCH, NCH, SR);
    dsp_track_t nullTrack; d->run(&list, nullTrack, dsp::FLUSH);
    out.clear(); outSr = SR; outCh = NCH;
    for (t_size i = 0; i < list.get_count(); i++) { audio_chunk* c = list.get_item(i); outSr = c->get_srate(); outCh = c->get_channels(); const audio_sample* dd = c->get_data(); out.insert(out.end(), dd, dd + c->get_sample_count() * c->get_channels()); }
    return true;
}

void renderOne(dsp_entry::ptr entry, const dsp_preset& preset, const char* label, const std::vector<float>& in, const std::string& stem) {
    std::vector<float> out; unsigned osr = SR, och = NCH;
    bool ok = false;
    try { ok = processSignal(entry, preset, in, out, osr, och); }
    catch (...) { printf("      [%s] run FAULTED (contained)\n", label); return; }
    if (!ok) { printf("      [%s] instantiate failed\n", label); return; }
    writeWav16(stem + "_in.wav", in, NCH, SR);
    writeWav16(stem + "_out.wav", out, och, osr);
    printf("      [%-7s] in: rms %6.2f peak %6.2f  ->  out: rms %6.2f peak %6.2f dBFS  (frames %zu->%zu, sr %u, ch %u)\n",
           label, rmsDb(in), peakDb(in), rmsDb(out), peakDb(out), in.size() / NCH, (size_t)(och ? out.size() / och : 0), osr, och);
}

// ---------------- chain hosting (dsp_manager) ----------------
// Loading a component is additive to the host's service registry (registerList appends factories), so
// loading TWO components leaves BOTH their dsp_entry services discoverable — which is exactly what the
// SDK's dsp_manager needs: it instantiates each chain stage by global GUID lookup (dsp_entry::g_instantiate)
// against that registry. So a chain is "load every member into this one process, then run dsp_manager
// over a dsp_chain_config of their presets" — one pass, one process, f32 throughout. We cache per path so
// a component used by several stages (or a rebuilt chain) is registered EXACTLY ONCE (a second registerList
// of the same factory list would double its factories in the bucket).
std::map<std::string, std::vector<dsp_entry::ptr>> g_loaded;
const std::vector<dsp_entry::ptr>& getComponentEntries(const std::string& path) {
    auto it = g_loaded.find(path);
    if (it != g_loaded.end()) return it->second;
    std::vector<dsp_entry::ptr> entries;
    HMODULE mod = LoadLibraryExA(path.c_str(), NULL, LOAD_WITH_ALTERED_SEARCH_PATH);
    if (mod) {
        auto gi = (foobar2000_client * (__cdecl*)(foobar2000_api*, HINSTANCE))GetProcAddress(mod, "foobar2000_get_interface");
        if (gi) {
            foobar2000_client* comp = gi(&hostApi, mod);
            comp->set_library_path(path.c_str(), "component"); comp->services_init(true);
            service_factory_base* head = comp->get_service_list();
            hostApi.registerList(head);
            // Collect THIS component's own dsp_entries (walk its module-local factory list, not the global
            // bucket — keeps per-component entry indexing unambiguous when several components are loaded).
            for (service_factory_base* f = head; f; f = f->__internal__next) {
                if (f->get_class_guid() == dsp_entry::class_guid) {
                    service_ptr_t<service_base> base; f->instance_create(base);
                    dsp_entry::ptr e; if (base.is_valid() && base->cast(e)) entries.push_back(e);
                }
            }
        }
    }
    auto& slot = g_loaded[path]; slot = std::move(entries); return slot;
}

// One-shot: run a signal through a whole chain config via a fresh dsp_manager (process + FLUSH-drain in
// one call — used by the standalone --chain battery so each item starts clean).
bool processChain(const dsp_chain_config& chain, const std::vector<float>& in,
                  std::vector<float>& out, unsigned& outSr, unsigned& outCh) {
    dsp_manager mgr; mgr.set_config(chain);
    dsp_chunk_list_impl list; if (!in.empty()) list.add_item()->set_data_32(in.data(), in.size() / NCH, NCH, SR);
    dsp_track_t nullTrack; mgr.run(&list, nullTrack, dsp::FLUSH, fb2k::noAbort);
    out.clear(); outSr = SR; outCh = NCH;
    for (t_size i = 0; i < list.get_count(); i++) { audio_chunk* c = list.get_item(i); outSr = c->get_srate(); outCh = c->get_channels(); const audio_sample* dd = c->get_data(); out.insert(out.end(), dd, dd + c->get_sample_count() * c->get_channels()); }
    return true;
}

// Standalone proof: build a chain from N component DLLs and show the CUMULATIVE effect as each stage is
// added (raw -> +stage1 -> +stage1+stage2 -> …). Demonstrates real in-process chaining via dsp_manager.
void runChainBattery(const std::vector<std::string>& dlls) {
    std::vector<dsp_preset_impl> presets; std::vector<std::string> names;
    for (auto& d : dlls) {
        const auto& es = getComponentEntries(d);
        if (es.empty()) { printf("[host] %s: no dsp_entry (skipped)\n", d.c_str()); continue; }
        dsp_preset_impl p; try { es[0]->get_default_preset(p); } catch (...) { p.set_owner(es[0]->get_guid()); }
        pfc::string8 nm; es[0]->get_name(nm);
        presets.push_back(p); names.push_back(nm.c_str());
    }
    if (presets.empty()) { printf("[host] no chain stages loaded\n"); return; }
    printf("[host] chain (%zu stages):", presets.size());
    for (size_t i = 0; i < names.size(); i++) printf(" %s\"%s\"", i ? "-> " : "", names[i].c_str());
    printf("\n[host] cumulative RMS dBFS as each stage is added:\n");

    const char* sigNames[] = { "sine1k", "sweep", "noise" };
    std::vector<float> sigs[] = { genSine(1000.0f), genSweep(), genNoise() };
    for (int s = 0; s < 3; s++) {
        printf("   %-7s raw %6.2f", sigNames[s], rmsDb(sigs[s]));
        dsp_chain_config_impl prefix;
        for (size_t k = 0; k < presets.size(); k++) {
            prefix.add_item(presets[k]);
            std::vector<float> out; unsigned osr = SR, och = NCH; bool ok = false;
            try { ok = processChain(prefix, sigs[s], out, osr, och); } catch (...) {}
            if (ok) printf("  | +%s %6.2f", names[k].c_str(), rmsDb(out)); else printf("  | +%s FAULT", names[k].c_str());
        }
        printf("\n");
    }
    // Dump the full-chain sweep so the result is audible / inspectable.
    std::vector<float> out; unsigned osr = SR, och = NCH; dsp_chain_config_impl full;
    for (auto& p : presets) full.add_item(p);
    if (processChain(full, sigs[1], out, osr, och)) {
        writeWav16("out_chain_sweep_in.wav", sigs[1], NCH, SR);
        writeWav16("out_chain_sweep_out.wav", out, och, osr);
        printf("[host] wrote out_chain_sweep_{in,out}.wav (%u Hz, %u ch, %zu->%zu frames)\n",
               osr, och, sigs[1].size() / NCH, (size_t)(och ? out.size() / och : 0));
    }
}

LRESULT CALLBACK WndProc(HWND h, UINT m, WPARAM w, LPARAM l) { return DefWindowProcW(h, m, w, l); }
HWND createHostWindow() {
    WNDCLASSEXW wc = { sizeof(wc) }; wc.lpfnWndProc = WndProc; wc.hInstance = GetModuleHandleW(NULL);
    wc.lpszClassName = L"ResonanceFoobarDSPHost"; wc.hCursor = LoadCursorW(NULL, IDC_ARROW);
    RegisterClassExW(&wc);
    HWND h = CreateWindowExW(0, wc.lpszClassName, L"Resonance foobar2000 DSP host (hidden)", WS_OVERLAPPEDWINDOW,
                             CW_USEDEFAULT, CW_USEDEFAULT, 480, 160, NULL, NULL, wc.hInstance, NULL);
    // Intentionally NOT shown. This window is only a dialog OWNER for show_config_popup. A visible
    // window in the worker would never pump messages (the worker blocks on the stdin IPC read) and
    // Windows would flag it "Not Responding". A hidden owner still parents the modal config dialog.
    return h;
}

struct Args { std::string dll, presetIn, presetOut, inWav; bool config = false, render = true; size_t entry = 0; };
Args parse(int argc, char** argv) {
    Args a; if (argc >= 2) a.dll = argv[1];
    for (int i = 2; i < argc; i++) { std::string s = argv[i];
        if (s == "--config") a.config = true;
        else if (s == "--no-render") a.render = false;
        else if (s == "--preset-in" && i + 1 < argc) a.presetIn = argv[++i];
        else if (s == "--preset-out" && i + 1 < argc) a.presetOut = argv[++i];
        else if (s == "--in" && i + 1 < argc) a.inWav = argv[++i];
        else if (s == "--entry" && i + 1 < argc) a.entry = (size_t)atoi(argv[++i]);
    }
    return a;
}

// ---------------- IPC worker (framed binary stdio; stderr = diagnostics) ----------------
// 4-byte tags, little-endian. The worker hosts a CHAIN of N>=1 DSPs (a single DSP is just N=1).
// parent->worker:
//   LOAD <u32 len,path><u32 srate><u32 nch><u32 entryIdx>            -- set chain to ONE stage (default preset)
//   CHAN <u32 srate><u32 nch><u32 count> then count x { <u32 len,path><u32 entryIdx><u32 len,blob> }
//                                                                    -- build an N-stage chain (blob empty = default)
//   RST                                                              -- drop instantiated DSPs; next PROC is fresh
//   PROC <u32 nframes><f32 nframes*nch>                              -- stream a block through the whole chain
//   FLSH                                                             -- end-of-stream drain (each stage's tail)
//   CFG  <u32 stage>  /  GPRE <u32 stage>  /  SPRE <u32 stage><u32 len,blob>   -- per-stage config/get/set preset
//   QUIT
// worker->parent: "LOK "<u32 status><u32 len,name>, "COK "<u32 count><count x str name>, "OK  ",
//   "POK "<u32 nframes><f32...>, "PRE "<u32 len,blob=GUID16+data>, "ERR "<u32 len,msg>.
// Audio = interleaved f32. PROC streams with flags=0 (state preserved); dsp_manager runs every stage in
// series in one pass. blob = 16-byte owner GUID + dsp_preset data.
bool rdN(void* p, size_t n) { return fread(p, 1, n, stdin) == n; }
uint32_t rdU32() { uint32_t v = 0; rdN(&v, 4); return v; }
std::string rdStr() { uint32_t n = rdU32(); std::string s(n, '\0'); if (n) rdN(&s[0], n); return s; }
void wrTag(const char* t) { fwrite(t, 1, 4, stdout); }
void wrU32(uint32_t v) { fwrite(&v, 4, 1, stdout); }
void wrBytes(const void* p, size_t n) { if (n) fwrite(p, 1, n, stdout); }
void wrStr(const std::string& s) { wrU32((uint32_t)s.size()); wrBytes(s.data(), s.size()); }
void wrErr(const char* m) { wrTag("ERR "); wrStr(m); fflush(stdout); }
std::string presetBlob(const dsp_preset& p) { std::string b; GUID g = p.get_owner(); b.append((const char*)&g, 16); b.append((const char*)p.get_data(), p.get_data_size()); return b; }

} // namespace

int runWorker() {
    _setmode(_fileno(stdin), _O_BINARY);
    _setmode(_fileno(stdout), _O_BINARY);
    setvbuf(stderr, nullptr, _IONBF, 0);
    SetErrorMode(SEM_FAILCRITICALERRORS | SEM_NOGPFAULTERRORBOX | SEM_NOOPENFILEERRORBOX); // no OS dialogs on a bad/crashing plugin

    // --- transport handshake (TRANSPORT.md v1; mandatory first exchange) ---------------
    // VERS must be the first message; we ALWAYS answer VOK with our versions + ident (the
    // PARENT enforces compatibility). Any other first tag: ERR + exit 2. Done before the
    // SDK spins up so a mismatched parent learns instantly.
    {
        char t[4];
        if (!rdN(t, 4) || memcmp(t, "VERS", 4) != 0) {
            wrErr("first message must be VERS (see TRANSPORT.md; parent predates the handshake?)");
            return 2;
        }
        (void)rdU32(); // parent transport_version — the parent enforces, we just answer
        (void)rdU32(); // parent vocab_version
        wrTag("VOK ");
        wrU32(1); // TRANSPORT_VERSION
        wrU32(1); // VOCAB_VERSION (this worker's command vocabulary)
        char ident[64];
        sprintf_s(ident, "foo_dsp_host-worker 0.1.0 (%u-bit)", (unsigned)(sizeof(void*) * 8));
        wrStr(ident);
        fflush(stdout);
    }

    foobar2000_client* self = foobar2000_get_interface(&hostApi, GetModuleHandleW(NULL));
    self->set_library_path("", "foo_dsp_host"); self->services_init(true);
    hostApi.registerList(service_factory_base::__internal__list);
    HWND wnd = createHostWindow(); hostApi.mainWnd = wnd;
    fprintf(stderr, "[worker] ready (%zu-bit)\n", sizeof(void*) * 8);

    // The worker hosts a CHAIN (N>=1 stages). dsp_manager owns the instantiated DSPs + runs them in series;
    // we keep the chain config + the per-stage dsp_entry (for config dialogs / names). A single DSP is just
    // a 1-stage chain — LOAD is exactly CHAN with count=1 and a default preset.
    dsp_manager manager;
    dsp_chain_config_impl chain;
    std::vector<dsp_entry::ptr> stageEntry; // parallel to chain items
    dsp_track_t nullTrack;
    unsigned srate = 44100, nch = 2; char tag[4];

    // Append one stage: component DLL + which dsp_entry in it (0 = the usual single entry) + optional preset
    // blob (GUID16 + data; empty = the entry's default). Returns false if the DLL/entry can't be resolved.
    auto addStage = [&](const std::string& dll, uint32_t eidx, const std::string& blob) -> bool {
        const std::vector<dsp_entry::ptr>& es = getComponentEntries(dll);
        if (eidx >= es.size()) return false;
        dsp_entry::ptr e = es[eidx];
        dsp_preset_impl p;
        if (blob.size() >= 16) { GUID g; memcpy(&g, blob.data(), 16); p.set_owner(g); p.set_data(blob.data() + 16, blob.size() - 16); }
        else { try { e->get_default_preset(p); } catch (...) { p.set_owner(e->get_guid()); } }
        chain.add_item(p); stageEntry.push_back(e); return true;
    };
    auto readChunksTo = [](dsp_chunk_list_impl& list, std::vector<float>& out) {
        for (t_size i = 0; i < list.get_count(); i++) { audio_chunk* c = list.get_item(i); const audio_sample* d = c->get_data(); out.insert(out.end(), d, d + c->get_sample_count() * c->get_channels()); }
    };

    while (rdN(tag, 4)) {
        if (!memcmp(tag, "LOAD", 4)) { // single-stage chain, default preset (back-compat spelling of CHAN 1)
            std::string path = rdStr(); srate = rdU32(); nch = rdU32(); uint32_t eidx = rdU32();
            chain.remove_all(); stageEntry.clear();
            try {
                if (!addStage(path, eidx, std::string())) { wrErr("no dsp_entry / load failed"); continue; }
                manager.set_config(chain);
                pfc::string8 nm; stageEntry[0]->get_name(nm);
                wrTag("LOK "); wrU32(1); wrStr(nm.c_str()); fflush(stdout);
            } catch (...) { wrErr("exception in LOAD"); }
        } else if (!memcmp(tag, "CHAN", 4)) { // build an N-stage chain atomically in one message
            srate = rdU32(); nch = rdU32(); uint32_t count = rdU32();
            struct Req { std::string dll; uint32_t eidx; std::string blob; };
            std::vector<Req> reqs(count);
            for (uint32_t i = 0; i < count; i++) { reqs[i].dll = rdStr(); reqs[i].eidx = rdU32(); reqs[i].blob = rdStr(); }
            chain.remove_all(); stageEntry.clear();
            bool ok = true;
            try { for (auto& rq : reqs) if (!addStage(rq.dll, rq.eidx, rq.blob)) { ok = false; break; } }
            catch (...) { ok = false; }
            if (!ok) { chain.remove_all(); stageEntry.clear(); wrErr("a chain stage failed to load"); continue; }
            manager.set_config(chain);
            wrTag("COK "); wrU32((uint32_t)chain.get_count());
            for (auto& e : stageEntry) { pfc::string8 nm; e->get_name(nm); wrStr(nm.c_str()); }
            fflush(stdout);
        } else if (!memcmp(tag, "RST ", 4)) { // drop instantiated DSPs; next PROC re-instantiates the chain fresh
            manager.close(); wrTag("OK  "); fflush(stdout);
        } else if (!memcmp(tag, "PROC", 4)) { // stream a block through the WHOLE chain (one dsp_manager pass)
            uint32_t nf = rdU32(); std::vector<float> in((size_t)nf * nch); if (nf) rdN(in.data(), (size_t)nf * nch * 4);
            std::vector<float> out;
            try {
                dsp_chunk_list_impl list; if (nf) list.add_item()->set_data_32(in.data(), nf, nch, srate);
                manager.run(&list, nullTrack, 0, fb2k::noAbort);
                readChunksTo(list, out);
            } catch (...) { wrErr("proc fault"); continue; }
            wrTag("POK "); wrU32((uint32_t)(nch ? out.size() / nch : 0)); wrBytes(out.data(), out.size() * 4); fflush(stdout);
        } else if (!memcmp(tag, "FLSH", 4)) { // end-of-stream drain: every stage emits its buffered tail
            std::vector<float> out;
            try {
                dsp_chunk_list_impl list; manager.run(&list, nullTrack, dsp::FLUSH, fb2k::noAbort);
                readChunksTo(list, out);
            } catch (...) { wrErr("flush fault"); continue; }
            wrTag("POK "); wrU32((uint32_t)(nch ? out.size() / nch : 0)); wrBytes(out.data(), out.size() * 4); fflush(stdout);
        } else if (!memcmp(tag, "CFG ", 4)) { // open stage's OWN config dialog, apply to the chain, return its preset
            uint32_t stage = rdU32();
            try {
                if (stage < chain.get_count() && stageEntry[stage].is_valid() && stageEntry[stage]->have_config_popup()) {
                    dsp_preset_impl p(chain.get_item(stage));
                    stageEntry[stage]->show_config_popup(p, wnd);
                    chain.replace_item(p, stage); manager.set_config(chain);
                    wrTag("PRE "); wrStr(presetBlob(p)); fflush(stdout);
                } else if (stage < chain.get_count()) {
                    wrTag("PRE "); wrStr(presetBlob(chain.get_item(stage))); fflush(stdout);
                } else wrErr("CFG stage out of range");
            } catch (...) { wrErr("config fault"); }
        } else if (!memcmp(tag, "GPRE", 4)) { // get stage's current preset blob
            uint32_t stage = rdU32();
            if (stage < chain.get_count()) { wrTag("PRE "); wrStr(presetBlob(chain.get_item(stage))); fflush(stdout); }
            else wrErr("GPRE stage out of range");
        } else if (!memcmp(tag, "SPRE", 4)) { // set stage's preset (GUID16 + data); recycles the rest of the chain
            uint32_t stage = rdU32(); std::string blob = rdStr();
            if (stage >= chain.get_count()) { wrErr("SPRE stage out of range"); continue; }
            dsp_preset_impl p;
            if (blob.size() >= 16) { GUID g; memcpy(&g, blob.data(), 16); p.set_owner(g); p.set_data(blob.data() + 16, blob.size() - 16); }
            try { chain.replace_item(p, stage); manager.set_config(chain); wrTag("OK  "); fflush(stdout); } catch (...) { wrErr("set preset failed"); }
        } else if (!memcmp(tag, "QUIT", 4)) { break; }
        else { wrErr("unknown tag"); }
    }
    return 0;
}

int main(int argc, char** argv) {
    setvbuf(stdout, nullptr, _IONBF, 0);
    for (int i = 1; i < argc; i++) if (std::string(argv[i]) == "--worker") return runWorker();

    // --chain a.dll b.dll c.dll : build a real N-stage DSP chain (via the SDK's dsp_manager) and render the
    // battery through it, showing the cumulative effect of each added stage. The standalone proof that
    // in-process chaining works (no .exe->.exe->.exe piping).
    std::vector<std::string> chainDlls;
    for (int i = 1; i < argc; i++) if (std::string(argv[i]) == "--chain") { for (int j = i + 1; j < argc && argv[j][0] != '-'; j++) chainDlls.push_back(argv[j]); }
    if (!chainDlls.empty()) {
        foobar2000_client* self = foobar2000_get_interface(&hostApi, GetModuleHandleW(NULL));
        self->set_library_path("", "foo_dsp_host"); self->services_init(true);
        hostApi.registerList(service_factory_base::__internal__list);
        HWND wnd = createHostWindow(); hostApi.mainWnd = wnd;
        runChainBattery(chainDlls);
        if (wnd) DestroyWindow(wnd);
        return 0;
    }

    Args args = parse(argc, argv);
    if (args.dll.empty()) { printf("usage: foo_dsp_host <component.dll> [--config] [--preset-in f] [--preset-out f] [--in wav] [--entry N] [--no-render]\n       foo_dsp_host --chain a.dll b.dll c.dll   (render a DSP chain)\n       foo_dsp_host --worker                    (IPC worker; LOAD/CHAN/PROC/...)\n"); return 2; }

    foobar2000_client* self = foobar2000_get_interface(&hostApi, GetModuleHandleW(NULL));
    self->set_library_path("", "foo_dsp_host"); self->services_init(true);
    hostApi.registerList(service_factory_base::__internal__list);

    HMODULE h = LoadLibraryExA(args.dll.c_str(), NULL, LOAD_WITH_ALTERED_SEARCH_PATH);
    if (!h) { printf("[host] LoadLibrary('%s') failed: %lu\n", args.dll.c_str(), GetLastError()); return 3; }
    auto getIface = (foobar2000_client * (__cdecl*)(foobar2000_api*, HINSTANCE))GetProcAddress(h, "foobar2000_get_interface");
    if (!getIface) { printf("[host] no foobar2000_get_interface export\n"); return 4; }
    foobar2000_client* comp = getIface(&hostApi, h);
    comp->set_library_path(args.dll.c_str(), "component"); comp->services_init(true);
    hostApi.registerList(comp->get_service_list());
    printf("[host] loaded '%s' (component client version=%u)\n", args.dll.c_str(), comp->get_version());

    Bucket* b = hostApi.find(dsp_entry::class_guid);
    size_t n = b ? b->factories.size() : 0;
    printf("[host] dsp_entry count = %zu\n", n);
    if (n == 0) return 0;
    if (args.entry >= n) args.entry = 0;

    service_ptr_t<service_base> base; b->factories[args.entry]->instance_create(base);
    dsp_entry::ptr entry; if (base.is_empty() || !base->cast(entry)) { printf("[host] cast to dsp_entry failed\n"); return 5; }
    pfc::string8 nm; entry->get_name(nm);
    printf("[host] entry[%zu] = \"%s\"  config_popup=%d\n", args.entry, nm.c_str(), (int)entry->have_config_popup());

    HWND wnd = createHostWindow(); hostApi.mainWnd = wnd;

    dsp_preset_impl preset;
    try { entry->get_default_preset(preset); } catch (...) { printf("[host] get_default_preset FAULTED (contained); using empty preset\n"); preset.set_owner(entry->get_guid()); }
    if (!args.presetIn.empty()) { if (loadPreset(args.presetIn, preset)) printf("[host] loaded preset (%u bytes) from %s\n", (unsigned)preset.get_data_size(), args.presetIn.c_str()); else printf("[host] could not load preset %s\n", args.presetIn.c_str()); }

    if (args.config) {
        if (entry->have_config_popup()) {
            printf("[host] opening the plugin's config dialog (modal) — adjust it, then close/OK...\n");
            bool changed = false;
            try { changed = entry->show_config_popup(preset, wnd); }
            catch (...) { printf("[host] config dialog FAULTED (contained)\n"); }
            printf("[host] config closed (changed=%d, preset now %u bytes)\n", (int)changed, (unsigned)preset.get_data_size());
            if (!args.presetOut.empty()) { savePreset(args.presetOut, preset); printf("[host] saved preset -> %s\n", args.presetOut.c_str()); }
        } else printf("[host] this DSP has no config popup\n");
    }

    if (args.render) {
        printf("[host] rendering battery through \"%s\":\n", nm.c_str());
        renderOne(entry, preset, "sine1k", genSine(1000.0f), "out_sine1k");
        renderOne(entry, preset, "sweep", genSweep(), "out_sweep");
        renderOne(entry, preset, "noise", genNoise(), "out_noise");
        if (!args.inWav.empty()) {
            std::vector<float> wav; unsigned wch = 0, wsr = 0;
            if (readWav16(args.inWav, wav, wch, wsr) && wch == NCH && wsr == SR) renderOne(entry, preset, "input", wav, "out_input");
            else printf("      [input] skipped (need 16-bit %u Hz %u-ch WAV; got ch=%u sr=%u)\n", SR, NCH, wch, wsr);
        }
    }

    if (wnd) DestroyWindow(wnd);
    printf("[host] done.\n");
    return 0;
}
