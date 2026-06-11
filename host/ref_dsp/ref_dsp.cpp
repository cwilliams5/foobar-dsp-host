// foo_dsp_ref — the repo's own known-good foobar2000 DSP component: a deterministic
// x0.5 gain (exactly -6.02 dB; bit-exact in IEEE f32, so tests can assert equality).
// No preset data, no config popup — the smallest real component the SDK can express,
// used by the client crate's integration tests + CI so audio-through-a-real-component
// is provable without any third-party DLLs.

#include <SDK/foobar2000.h>
#include <SDK/component.h>

DECLARE_COMPONENT_VERSION(
    "Reference Gain DSP",
    "0.1.0",
    "Deterministic x0.5 gain — foobar-dsp-host's known-good test fixture.");
VALIDATE_COMPONENT_FILENAME("foo_dsp_ref.dll");

namespace {

class dsp_ref_gain : public dsp_impl_base {
public:
    static GUID g_get_guid() {
        // {7F1A9E42-3C55-4B1D-9A06-52E1C5D6F0AB} — stable; identifies this DSP in chains.
        static const GUID g = {
            0x7f1a9e42, 0x3c55, 0x4b1d, { 0x9a, 0x06, 0x52, 0xe1, 0xc5, 0xd6, 0xf0, 0xab }
        };
        return g;
    }
    static void g_get_name(pfc::string_base& out) { out = "Reference Gain (x0.5)"; }

    bool on_chunk(audio_chunk* chunk, abort_callback&) override {
        audio_sample* d = chunk->get_data();
        const size_t n = (size_t)chunk->get_sample_count() * chunk->get_channels();
        for (size_t i = 0; i < n; ++i) d[i] *= (audio_sample)0.5;
        return true;
    }
    void on_endoftrack(abort_callback&) override {}
    void on_endofplayback(abort_callback&) override {}
    void flush() override {}
    double get_latency() override { return 0; }
    bool need_track_change_mark() override { return false; }
};

static dsp_factory_nopreset_t<dsp_ref_gain> g_dsp_ref_gain_factory;

// A second, identical-math entry under its OWN GUID. dsp_manager recycles instances of
// identical (GUID, preset) chain items — two copies of the SAME entry collapse to one
// application — so compound-gain tests chain A -> B instead (x0.25, still bit-exact).
class dsp_ref_gain_b : public dsp_ref_gain {
public:
    static GUID g_get_guid() {
        // {3D24C0F1-9A77-4E02-B41C-08D9E5A2517E}
        static const GUID g = {
            0x3d24c0f1, 0x9a77, 0x4e02, { 0xb4, 0x1c, 0x08, 0xd9, 0xe5, 0xa2, 0x51, 0x7e }
        };
        return g;
    }
    static void g_get_name(pfc::string_base& out) { out = "Reference Gain B (x0.5)"; }
};

static dsp_factory_nopreset_t<dsp_ref_gain_b> g_dsp_ref_gain_b_factory;

} // namespace
