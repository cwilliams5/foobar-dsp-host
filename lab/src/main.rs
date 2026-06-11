// Resonance foobar2000 DSP Lab — egui GUI orchestrator (DSP rack / chain edition).
//
// Build a CHAIN of foobar2000 DSPs (add components, reorder, remove), hosted together in ONE isolated
// worker process and run in series via the SDK's own dsp_manager. Hear your track stream through the
// whole chain live, A/B bypass↔processed instantly, watch IN/OUT level meters, and click a stage's ⚙ to
// pop that plugin's OWN settings dialog and re-process. A worker crash drops the pipe; the lab survives.
// Worker IPC runs on a background thread so the UI never freezes.
//
//   --selftest         headless: build a chain over the worker IPC, prove each stage compounds, exit 0/1
//   --shot <png>       headless: build a default chain, render the rack, save a screenshot, exit
//   --src/--wav <file> source audio (else test.{mp3,flac,wav,ogg} in the repo root, else a tone)

use arc_swap::ArcSwap;
use eframe::egui;
use foobar_dsp_host::{dll_arch, stage_companion_dlls, PeArch, StageSpec};
use std::error::Error;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering::Relaxed};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use symphonia::core::audio::SampleBuffer;
use symphonia::core::codecs::DecoderOptions;
use symphonia::core::formats::FormatOptions;
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;

const ROOT: &str = env!("CARGO_MANIFEST_DIR");
fn proto_root() -> PathBuf { Path::new(ROOT).parent().unwrap().to_path_buf() }
fn fname(p: &Path) -> String { p.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default() }

// ----------------------------- worker IPC client -----------------------------
// All worker IPC goes through the foobar-dsp-host client crate (this lab is its first
// consumer). These thin wrappers keep the engine's historical String-error call shapes.
// The worker hosts a CHAIN of N>=1 DSPs: CHAN builds it, PROC streams blocks through the
// whole chain, FLSH drains look-ahead tails, RST re-instantiates fresh, CFG <stage> opens
// a stage's own config dialog. See docs/PROTOCOL.md.
struct Worker(foobar_dsp_host::Worker);
impl Worker {
    fn spawn(exe: &Path, workdir: &Path) -> std::io::Result<Worker> {
        foobar_dsp_host::Worker::spawn_with(
            exe,
            &foobar_dsp_host::SpawnOptions { working_dir: Some(workdir.to_path_buf()), ..Default::default() },
        )
        .map(Worker)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e.to_string()))
    }
    // Build an N-stage chain in one message: each stage = (component dll, optional preset blob; empty = default).
    fn chain(&mut self, stages: &[(String, Vec<u8>)], sr: u32, nch: u32) -> Result<Vec<String>, String> {
        let specs: Vec<StageSpec> = stages
            .iter()
            .map(|(dll, blob)| StageSpec { dll: dll.into(), entry_index: 0, preset: blob.clone() })
            .collect();
        self.0.build_chain(sr, nch, &specs).map_err(|e| e.to_string())
    }
    fn reset(&mut self) -> Result<(), String> { // drop instantiated DSPs; next PROC re-instantiates the chain fresh
        self.0.reset().map_err(|e| e.to_string())
    }
    fn process(&mut self, block: &[f32], _frames: u32, _nch: usize) -> Result<Vec<f32>, String> {
        let mut out = Vec::new();
        self.0.process_into(block, &mut out).map_err(|e| e.to_string())?;
        Ok(out)
    }
    fn drain(&mut self, _nch: usize) -> Result<Vec<f32>, String> {
        let mut out = Vec::new();
        self.0.drain_into(&mut out).map_err(|e| e.to_string())?;
        Ok(out)
    }
    fn config(&mut self, stage: u32) -> Result<Vec<u8>, String> {
        self.0.configure(stage).map_err(|e| e.to_string())
    }
    fn quit(self) { let _ = self.0.quit(); }
}

// ----------------------------- WAV + DSP helpers -----------------------------
fn read_wav(path: &Path) -> Result<(Vec<f32>, u32), String> {
    let d = std::fs::read(path).map_err(|e| e.to_string())?;
    if d.len() < 44 || &d[0..4] != b"RIFF" || &d[8..12] != b"WAVE" { return Err("not RIFF/WAVE".into()); }
    let (mut ch, mut sr, mut bits) = (2u16, 44100u32, 16u16);
    let mut pos = 12usize; let mut samples: Vec<f32> = Vec::new();
    while pos + 8 <= d.len() {
        let id = &d[pos..pos + 4];
        let sz = u32::from_le_bytes([d[pos + 4], d[pos + 5], d[pos + 6], d[pos + 7]]) as usize;
        let body = pos + 8;
        if id == b"fmt " && body + 16 <= d.len() {
            ch = u16::from_le_bytes([d[body + 2], d[body + 3]]);
            sr = u32::from_le_bytes([d[body + 4], d[body + 5], d[body + 6], d[body + 7]]);
            bits = u16::from_le_bytes([d[body + 14], d[body + 15]]);
        } else if id == b"data" {
            let end = (body + sz).min(d.len());
            if bits != 16 { return Err(format!("unsupported {} bit WAV", bits)); }
            for c in d[body..end].chunks_exact(2) { samples.push(i16::from_le_bytes([c[0], c[1]]) as f32 / 32768.0); }
            break;
        }
        pos = body + sz + (sz & 1);
    }
    let stereo = if ch == 1 { samples.iter().flat_map(|&s| [s, s]).collect() } else { samples };
    Ok((stereo, sr))
}
fn rms_db(x: &[f32]) -> f32 { if x.is_empty() { return -120.0; } let s: f64 = x.iter().map(|&v| (v as f64) * (v as f64)).sum(); let r = (s / x.len() as f64).sqrt(); if r > 1e-9 { 20.0 * (r as f32).log10() } else { -120.0 } }

// Stream the whole track through the chain: RST (fresh) -> PROC blocks -> FLSH (drain tails). One pass.
fn preprocess(w: &mut Worker, raw: &[f32]) -> Result<Vec<f32>, String> {
    w.reset()?; // re-instantiate the chain fresh so re-processing (after a config/reorder) is deterministic
    const BLK: usize = 8192;
    let mut out = Vec::with_capacity(raw.len());
    let mut i = 0;
    while i < raw.len() {
        let frames = ((raw.len() - i) / 2).min(BLK);
        if frames == 0 { break; }
        out.extend(w.process(&raw[i..i + frames * 2], frames as u32, 2)?);
        i += frames * 2;
    }
    out.extend(w.drain(2)?);
    Ok(out)
}

// ----------------------------- shared playback state -----------------------------
struct Shared {
    raw: ArcSwap<Vec<f32>>,  // source track (set once) — bypass + IN meter
    proc: ArcSwap<Vec<f32>>, // processed (engine sets) — OUT when not bypassed
    chain_names: ArcSwap<Vec<String>>, // real DSP names of the APPLIED chain (engine fills; UI labels rows)
    bypass: AtomicBool,
    playing: AtomicBool,
    proc_ready: AtomicBool,
    pos: AtomicUsize,
    in_rms: AtomicU32,
    out_rms: AtomicU32,
    sr: AtomicU32,
}

// A chain edit (rebuild) or a per-stage config request. The UI owns the chain ORDER + names; the engine
// owns the worker + per-stage presets (persisted to presets/, keyed by component).
enum EngineMsg { SetChain(Vec<PathBuf>), ConfigStage(usize), Quit }

fn engine_thread(rx: Receiver<EngineMsg>, shared: Arc<Shared>, status: Arc<Mutex<String>>, worker_x64: PathBuf, worker_x86: PathBuf) {
    let sr = shared.sr.load(Relaxed);
    let set = |s: String| *status.lock().unwrap() = s;
    let mut worker: Option<Worker> = None;
    let mut cur_chain: Vec<PathBuf> = Vec::new();
    while let Ok(msg) = rx.recv() {
        match msg {
            EngineMsg::SetChain(dlls) => {
                if let Some(w) = worker.take() { w.quit(); }
                cur_chain.clear();
                shared.proc_ready.store(false, Relaxed);
                shared.bypass.store(true, Relaxed);
                shared.chain_names.store(Arc::new(Vec::new()));
                if dlls.is_empty() { set("empty chain — add a DSP to start".into()); continue; }
                // One worker process per chain; all stages must share an arch (a 64-bit process can't load a
                // 32-bit DLL — a cross-arch chain would need a second worker + an IPC hop between them).
                let arch = dll_arch(&dlls[0]);
                if let Some(bad) = dlls.iter().find(|d| dll_arch(d) != arch) {
                    set(format!("chain mixes x86 + x64 ({} differs) — the worker & protocol support it, but cross-arch needs one worker per arch with blocks routed between them; this demo keeps a chain single-arch (that router is the host app's job — see docs/PROTOCOL.md)", fname(bad))); continue;
                }
                let is_x86 = arch == PeArch::X86;
                let worker_exe = if is_x86 { &worker_x86 } else { &worker_x64 };
                if !worker_exe.exists() { set(format!("the {} worker isn't built — build.ps1 builds both", if is_x86 { "x86" } else { "x64" })); continue; }
                let worker_dir = worker_exe.parent().unwrap();
                for d in &dlls { stage_companion_dlls(d.parent().unwrap_or(worker_dir), worker_dir); }
                set(format!("building chain of {} ({}) …", dlls.len(), if is_x86 { "x86" } else { "x64" }));
                let mut w = match Worker::spawn(worker_exe, worker_dir) { Ok(w) => w, Err(e) => { set(format!("worker spawn failed: {e}")); continue; } };
                let stages: Vec<(String, Vec<u8>)> = dlls.iter().map(|d| (d.to_string_lossy().into_owned(), load_persisted(d))).collect();
                match w.chain(&stages, sr, 2) {
                    Ok(names) => {
                        let raw = shared.raw.load_full();
                        match preprocess(&mut w, &raw) {
                            Ok(proc) => {
                                let delta = rms_db(&proc) - rms_db(&raw);
                                shared.proc.store(Arc::new(proc));
                                shared.proc_ready.store(true, Relaxed);
                                shared.bypass.store(false, Relaxed);
                                set(format!("chain ready: {}  ({delta:+.2} dB vs raw)", names.join(" · ")));
                                shared.chain_names.store(Arc::new(names));
                                cur_chain = dlls;
                                worker = Some(w);
                            }
                            Err(e) => { set(format!("process failed: {e}")); w.quit(); }
                        }
                    }
                    Err(e) => { set(format!("chain build failed: {e}")); w.quit(); }
                }
            }
            EngineMsg::ConfigStage(i) => {
                if let Some(w) = worker.as_mut() {
                    if i >= cur_chain.len() { set("stage no longer in the chain — Apply first".into()); continue; }
                    set(format!("opening stage {}’s config dialog — adjust + OK …", i + 1));
                    match w.config(i as u32) {
                        Ok(blob) => {
                            save_persisted(&cur_chain[i], &blob); // remember this component's settings across runs
                            let raw = shared.raw.load_full();
                            match preprocess(w, &raw) {
                                Ok(proc) => { let delta = rms_db(&proc) - rms_db(&raw); shared.proc.store(Arc::new(proc)); shared.bypass.store(false, Relaxed); set(format!("stage {} reconfigured — chain now {delta:+.2} dB vs raw", i + 1)); }
                                Err(e) => set(format!("reprocess failed: {e}")),
                            }
                        }
                        Err(e) => set(format!("config failed: {e}")),
                    }
                } else { set("apply a chain first".into()); }
            }
            EngineMsg::Quit => { if let Some(w) = worker.take() { w.quit(); } break; }
        }
    }
}

// Per-component preset persistence: one opaque dsp_preset blob per component, in presets/ (gitignored).
// (Two stages of the SAME component share the file — fine for a lab; the "real" answer is per-slot state,
// or foobar's whole-chain dsp_chain_config::to_blob — see docs/HOW-IT-WORKS.md.)
fn preset_path(dll: &Path) -> PathBuf {
    let stem = dll.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| "dsp".into());
    proto_root().join("presets").join(format!("{stem}.preset"))
}
fn load_persisted(dll: &Path) -> Vec<u8> { std::fs::read(preset_path(dll)).unwrap_or_default() }
fn save_persisted(dll: &Path, blob: &[u8]) {
    let pf = preset_path(dll);
    if let Some(parent) = pf.parent() { let _ = std::fs::create_dir_all(parent); }
    let _ = std::fs::write(&pf, blob);
}

// Use the device's OWN default config (WASAPI shared mode dictates the mix format — forcing 44.1/2/f32
// fails on a 48 kHz device). Source is resampled to this rate up front; here we just map stereo -> the
// device's channel count and convert f32 -> the device's sample type.
fn build_stream(shared: Arc<Shared>) -> Result<cpal::Stream, String> {
    use cpal::traits::{DeviceTrait, HostTrait};
    let device = cpal::default_host().default_output_device().ok_or("no output device")?;
    let supp = device.default_output_config().map_err(|e| e.to_string())?;
    let config: cpal::StreamConfig = supp.config();
    match supp.sample_format() {
        cpal::SampleFormat::F32 => build_typed::<f32>(&device, &config, shared),
        cpal::SampleFormat::I16 => build_typed::<i16>(&device, &config, shared),
        cpal::SampleFormat::U16 => build_typed::<u16>(&device, &config, shared),
        other => Err(format!("unsupported sample format {other:?}")),
    }
}
fn build_typed<T>(device: &cpal::Device, config: &cpal::StreamConfig, shared: Arc<Shared>) -> Result<cpal::Stream, String>
where T: cpal::SizedSample + cpal::FromSample<f32> + Send + 'static {
    use cpal::traits::{DeviceTrait, StreamTrait};
    let devch = (config.channels as usize).max(1);
    let s = shared.clone();
    let stream = device.build_output_stream(config, move |data: &mut [T], _| {
        let frames = data.len() / devch;
        let silence = |d: &mut [T]| { for x in d.iter_mut() { *x = T::from_sample(0.0f32); } };
        if !s.playing.load(Relaxed) { silence(data); return; }
        let rawb = s.raw.load_full();
        let playb = if s.bypass.load(Relaxed) { rawb.clone() } else { s.proc.load_full() };
        let n = playb.len();
        if n < 2 { silence(data); s.out_rms.store(0f32.to_bits(), Relaxed); s.in_rms.store(0f32.to_bits(), Relaxed); return; }
        let rn = rawb.len();
        let mut p = s.pos.load(Relaxed) % n; if p & 1 == 1 { p -= 1; }
        let (mut so, mut si) = (0f64, 0f64);
        for f in 0..frames {
            let l = playb[p]; let r = if p + 1 < n { playb[p + 1] } else { l };
            so += (l as f64) * (l as f64) + (r as f64) * (r as f64);
            if rn >= 2 { let il = rawb[p % rn]; let ir = rawb[(p + 1) % rn]; si += (il as f64) * (il as f64) + (ir as f64) * (ir as f64); }
            let base = f * devch;
            for c in 0..devch { data[base + c] = T::from_sample(if c == 0 { l } else if c == 1 { r } else { 0.0 }); }
            p += 2; if p >= n { p = 0; }
        }
        s.pos.store(p, Relaxed);
        let cnt = (frames.max(1) * 2) as f64;
        s.out_rms.store(((so / cnt).sqrt() as f32).to_bits(), Relaxed);
        s.in_rms.store(((si / cnt).sqrt() as f32).to_bits(), Relaxed);
    }, move |e| eprintln!("cpal: {e}"), None).map_err(|e| e.to_string())?;
    stream.play().map_err(|e| e.to_string())?;
    Ok(stream)
}

fn device_sr() -> u32 {
    use cpal::traits::{DeviceTrait, HostTrait};
    cpal::default_host().default_output_device().and_then(|d| d.default_output_config().ok()).map(|c| c.sample_rate().0).unwrap_or(44100)
}

// Decode any symphonia-supported file (mp3/flac/wav/ogg…) -> interleaved stereo f32 + its sample rate.
fn decode_audio(path: &Path) -> Result<(Vec<f32>, u32), String> {
    let file = std::fs::File::open(path).map_err(|e| e.to_string())?;
    let mss = MediaSourceStream::new(Box::new(file), Default::default());
    let mut hint = Hint::new();
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) { hint.with_extension(ext); }
    let probed = symphonia::default::get_probe().format(&hint, mss, &FormatOptions::default(), &MetadataOptions::default()).map_err(|e| e.to_string())?;
    let mut format = probed.format;
    let track = format.default_track().ok_or("no audio track")?;
    let track_id = track.id;
    let sr = track.codec_params.sample_rate.unwrap_or(44100);
    let mut decoder = symphonia::default::get_codecs().make(&track.codec_params, &DecoderOptions::default()).map_err(|e| e.to_string())?;
    let mut out: Vec<f32> = Vec::new();
    while let Ok(packet) = format.next_packet() {
        if packet.track_id() != track_id { continue; }
        if let Ok(decoded) = decoder.decode(&packet) {
            let spec = *decoded.spec();
            let ch = spec.channels.count().max(1);
            let mut sb = SampleBuffer::<f32>::new(decoded.capacity() as u64, spec);
            sb.copy_interleaved_ref(decoded);
            for fr in sb.samples().chunks(ch) { let l = fr[0]; let r = if ch > 1 { fr[1] } else { fr[0] }; out.push(l); out.push(r); }
        }
        if out.len() >= 90 * sr as usize * 2 { break; } // cap ~90s: snappy Load + modest memory; the lab loops
    }
    if out.is_empty() { return Err("decoded 0 samples".into()); }
    Ok((out, sr))
}

// Linear-resample interleaved stereo from `from` Hz to `to` Hz.
fn resample_stereo(src: &[f32], from: u32, to: u32) -> Vec<f32> {
    if from == to || src.len() < 2 { return src.to_vec(); }
    let frames = src.len() / 2;
    let ratio = to as f64 / from as f64;
    let out_frames = ((frames as f64) * ratio) as usize;
    let mut out = Vec::with_capacity(out_frames * 2);
    for i in 0..out_frames {
        let pos = i as f64 / ratio;
        let i0 = (pos.floor() as usize).min(frames - 1);
        let i1 = (i0 + 1).min(frames - 1);
        let frac = (pos - i0 as f64) as f32;
        for c in 0..2 { let a = src[i0 * 2 + c]; let b = src[i1 * 2 + c]; out.push(a + (b - a) * frac); }
    }
    out
}

// (Companion-DLL staging + PE-arch probing moved into the foobar-dsp-host crate:
// stage_companion_dlls / dll_arch / PeArch.)

// ----------------------------- minimal PNG writer (no deps) -----------------------------
// Writes RGB8 as a PNG using a zlib "stored" (uncompressed) stream — enough to save a screenshot of the
// rack for the docs without pulling the `image`/`png` crates into a reference repo.
fn crc32(buf: &[u8]) -> u32 {
    let mut c: u32 = 0xFFFF_FFFF;
    for &b in buf { c ^= b as u32; for _ in 0..8 { c = if c & 1 != 0 { (c >> 1) ^ 0xEDB8_8320 } else { c >> 1 }; } }
    !c
}
fn adler32(buf: &[u8]) -> u32 {
    let (mut a, mut b): (u32, u32) = (1, 0);
    for &x in buf { a = (a + x as u32) % 65521; b = (b + a) % 65521; }
    (b << 16) | a
}
fn png_chunk(out: &mut Vec<u8>, typ: &[u8; 4], data: &[u8]) {
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    let start = out.len();
    out.extend_from_slice(typ);
    out.extend_from_slice(data);
    let crc = crc32(&out[start..]);
    out.extend_from_slice(&crc.to_be_bytes());
}
fn write_png_rgb(path: &Path, w: usize, h: usize, rgb: &[u8]) -> std::io::Result<()> {
    let mut raw = Vec::with_capacity(h * (1 + w * 3)); // filter byte 0 per scanline
    for y in 0..h { raw.push(0); raw.extend_from_slice(&rgb[y * w * 3..(y + 1) * w * 3]); }
    let mut zlib = vec![0x78u8, 0x01]; // zlib header
    let mut i = 0;
    while i < raw.len() {
        let n = (raw.len() - i).min(65535);
        zlib.push(if i + n >= raw.len() { 1 } else { 0 }); // BFINAL on the last block
        zlib.extend_from_slice(&(n as u16).to_le_bytes());
        zlib.extend_from_slice(&(!(n as u16)).to_le_bytes());
        zlib.extend_from_slice(&raw[i..i + n]);
        i += n;
    }
    zlib.extend_from_slice(&adler32(&raw).to_be_bytes());
    let mut out = vec![0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A];
    let mut ihdr = Vec::new();
    ihdr.extend_from_slice(&(w as u32).to_be_bytes());
    ihdr.extend_from_slice(&(h as u32).to_be_bytes());
    ihdr.extend_from_slice(&[8, 2, 0, 0, 0]); // 8-bit, color type 2 (RGB), deflate, no filter, no interlace
    png_chunk(&mut out, b"IHDR", &ihdr);
    png_chunk(&mut out, b"IDAT", &zlib);
    png_chunk(&mut out, b"IEND", &[]);
    std::fs::write(path, out)
}

// ----------------------------- egui app (the rack) -----------------------------
fn stage_label(p: &Path) -> String { p.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| fname(p)) }

struct LabApp {
    shared: Arc<Shared>,
    status: Arc<Mutex<String>>,
    tx: Sender<EngineMsg>,
    comps: Vec<PathBuf>,
    names: Vec<String>,
    selected: usize,
    chain: Vec<(PathBuf, String)>, // user-built order + fallback label (file stem)
    dirty: bool,                   // chain edited since the last Apply
    stream_err: Option<String>,
    _stream: Option<cpal::Stream>,
    shot_path: Option<PathBuf>,    // --shot: capture the rack to this PNG then exit
    shot_phase: u8,
    shot_frames: u32,
}
impl LabApp {
    fn new(shared: Arc<Shared>, status: Arc<Mutex<String>>, tx: Sender<EngineMsg>, comps: Vec<PathBuf>, shot_path: Option<PathBuf>) -> Self {
        let names = comps.iter().map(|p| fname(p)).collect();
        let (stream, stream_err) = match build_stream(shared.clone()) { Ok(s) => (Some(s), None), Err(e) => (None, Some(e)) };
        let mut app = LabApp { shared, status, tx, comps, names, selected: 0, chain: Vec::new(), dirty: false, stream_err, _stream: stream, shot_path: shot_path.clone(), shot_phase: 0, shot_frames: 0 };
        if shot_path.is_some() {
            // build a default chain for the screenshot (Noise Sharpening → VLevel → Loudness, if present)
            for needle in ["delta", "vlevel", "loudness"] {
                if let Some(p) = app.comps.iter().find(|p| fname(p).to_lowercase().contains(needle)) { app.chain.push((p.clone(), stage_label(p))); }
            }
            if app.chain.is_empty() { for p in app.comps.iter().take(3) { app.chain.push((p.clone(), stage_label(p))); } }
            app.apply_chain();
        }
        app
    }
    fn apply_chain(&mut self) {
        let _ = self.tx.send(EngineMsg::SetChain(self.chain.iter().map(|(p, _)| p.clone()).collect()));
        self.dirty = false;
    }
}
fn meter(ui: &mut egui::Ui, label: &str, db: f32) {
    let frac = ((db + 60.0) / 60.0).clamp(0.0, 1.0);
    ui.horizontal(|ui| {
        ui.label(format!("{label:>3}"));
        ui.add(egui::ProgressBar::new(frac).desired_width(300.0).text(format!("{db:>6.1} dBFS")));
    });
}
impl eframe::App for LabApp {
    fn update(&mut self, ctx: &egui::Context, _f: &mut eframe::Frame) {
        ctx.request_repaint(); // animate meters
        egui::CentralPanel::default().show(ctx, |ui| {
            ui.heading("Resonance · foobar2000 DSP Lab");
            ui.label(egui::RichText::new("a chain of real foobar2000 DSPs, hosted together in one isolated worker (dsp_manager)").italics().weak());
            ui.separator();

            // --- Available components + Add ---
            ui.horizontal(|ui| {
                ui.label("Available:");
                let names = self.names.clone();
                egui::ComboBox::from_id_source("dsp").width(280.0)
                    .selected_text(names.get(self.selected).cloned().unwrap_or_default())
                    .show_index(ui, &mut self.selected, names.len(), |i| names[i].clone());
                if ui.button("➕  Add to chain").clicked() {
                    if let Some(p) = self.comps.get(self.selected) { self.chain.push((p.clone(), self.names[self.selected].clone())); self.dirty = true; }
                }
            });
            ui.separator();

            // --- The chain rack ---
            let ready = self.shared.proc_ready.load(Relaxed);
            ui.horizontal(|ui| {
                ui.strong(format!("Chain ({})", self.chain.len()));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let mut bypass = self.shared.bypass.load(Relaxed);
                    ui.add_enabled_ui(ready, |ui| { ui.selectable_value(&mut bypass, false, "B · Processed"); });
                    ui.selectable_value(&mut bypass, true, "A · Bypass (raw)");
                    ui.label("Monitor:");
                    self.shared.bypass.store(if !ready { true } else { bypass }, Relaxed);
                });
            });

            let mut to_remove: Option<usize> = None;
            let (mut to_up, mut to_down, mut to_cfg): (Option<usize>, Option<usize>, Option<usize>) = (None, None, None);
            if self.chain.is_empty() {
                ui.label(egui::RichText::new("    add components above to build a chain →").weak());
            } else {
                let n = self.chain.len();
                let live = ready && !self.dirty; // ⚙ + the per-stage dialog only make sense once the chain is applied
                let live_names = self.shared.chain_names.load_full(); // real DSP names of the applied chain
                for i in 0..n {
                    let label = if live { live_names.get(i).cloned() } else { None }.unwrap_or_else(|| self.chain[i].1.clone());
                    ui.horizontal(|ui| {
                        ui.monospace(format!("{:>2}.", i + 1));
                        ui.label(label);
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui.button("Del").on_hover_text("remove from chain").clicked() { to_remove = Some(i); }
                            ui.add_enabled_ui(i + 1 < n, |ui| { if ui.button("Dn").on_hover_text("move down").clicked() { to_down = Some(i); } });
                            ui.add_enabled_ui(i > 0, |ui| { if ui.button("Up").on_hover_text("move up").clicked() { to_up = Some(i); } });
                            ui.add_enabled_ui(live, |ui| { if ui.button("⚙ Config").clicked() { to_cfg = Some(i); } });
                        });
                    });
                }
            }
            // apply edits after the loop (avoid aliasing self.chain during iteration)
            if let Some(i) = to_remove { self.chain.remove(i); self.dirty = true; }
            if let Some(i) = to_up { self.chain.swap(i, i - 1); self.dirty = true; }
            if let Some(i) = to_down { self.chain.swap(i, i + 1); self.dirty = true; }
            if let Some(i) = to_cfg { let _ = self.tx.send(EngineMsg::ConfigStage(i)); }

            ui.add_space(4.0);
            ui.horizontal(|ui| {
                let apply = egui::Button::new(if self.dirty { "▶  Apply chain  •" } else { "▶  Apply chain" });
                let apply = if self.dirty { apply.fill(egui::Color32::from_rgb(60, 110, 60)) } else { apply };
                if ui.add_enabled(!self.chain.is_empty(), apply).clicked() { self.apply_chain(); }
                if self.dirty { ui.label(egui::RichText::new("edited — Apply to (re)build + hear it").weak()); }
            });

            ui.add_space(4.0);
            ui.label(self.status.lock().unwrap().clone());
            if let Some(e) = &self.stream_err { ui.colored_label(egui::Color32::YELLOW, format!("⚠ no audio output ({e}) — A/B + meters still run")); }
            ui.separator();

            // --- transport + meters ---
            let playing = self.shared.playing.load(Relaxed);
            ui.horizontal(|ui| {
                if ui.button(if playing { "⏸  Pause" } else { "▶  Play" }).clicked() { self.shared.playing.store(!playing, Relaxed); }
            });
            ui.add_space(6.0);
            meter(ui, "IN", 20.0 * f32::from_bits(self.shared.in_rms.load(Relaxed)).max(1e-9).log10());
            meter(ui, "OUT", 20.0 * f32::from_bits(self.shared.out_rms.load(Relaxed)).max(1e-9).log10());

            let sr = self.shared.sr.load(Relaxed).max(1) as f32;
            let total = self.shared.raw.load().len() / 2;
            let cur = self.shared.pos.load(Relaxed) / 2;
            ui.add_space(6.0);
            ui.label(format!("{:>5.1}s / {:>5.1}s   (loops)", cur as f32 / sr, total as f32 / sr));
            ui.add_space(6.0);
            ui.label(egui::RichText::new("A/B is instant (same play position). ⚙ opens that stage’s own dialog, then re-processes the whole chain.").weak());
        });

        // --shot: once the default chain has processed, capture egui's OWN framebuffer (the reliable way to
        // screenshot a winit/egui window — external grabs fail when it's occluded) and exit.
        if self.shot_path.is_some() {
            self.shot_frames += 1;
            match self.shot_phase {
                0 => {
                    if self.shared.proc_ready.load(Relaxed) { self.shared.bypass.store(false, Relaxed); } // show "B · Processed"
                    if (self.shared.proc_ready.load(Relaxed) && self.shot_frames > 24) || self.shot_frames > 1200 {
                        ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot);
                        self.shot_phase = 1;
                    }
                }
                1 => {
                    let img = ctx.input(|i| i.raw.events.iter().find_map(|e| if let egui::Event::Screenshot { image, .. } = e { Some(image.clone()) } else { None }));
                    if let Some(img) = img {
                        let [w, h] = img.size;
                        let mut rgb = Vec::with_capacity(w * h * 3);
                        for px in &img.pixels { let a = px.to_array(); rgb.extend_from_slice(&[a[0], a[1], a[2]]); }
                        let path = self.shot_path.clone().unwrap();
                        match write_png_rgb(&path, w, h, &rgb) { Ok(_) => println!("SHOT: wrote {} ({}x{})", path.display(), w, h), Err(e) => eprintln!("SHOT: write failed: {e}") }
                        std::process::exit(0);
                    } else if self.shot_frames > 1800 { eprintln!("SHOT: no screenshot event arrived"); std::process::exit(2); }
                }
                _ => {}
            }
            ctx.request_repaint();
        }
    }
}

// ----------------------------- headless self-test -----------------------------
// Prove the WORKER-IPC chain path end-to-end: build incrementally longer chains over CHAN/PROC/FLSH and
// show each added stage compounding on the previous stage's output. Exit 0 if the chain processes (and
// the full chain differs from the raw input), 1 otherwise. No display / no audio device required.
fn run_selftest(worker_x64: &Path, comps: &[PathBuf], raw: &[f32], sr: u32) -> i32 {
    // Prefer a few components that visibly do something at default; else just take the first available.
    let pick = |needle: &str| comps.iter().find(|p| fname(p).to_lowercase().contains(needle) && dll_arch(p) == PeArch::X64).cloned();
    let mut chosen: Vec<PathBuf> = ["delta", "vlevel", "loudness"].iter().filter_map(|n| pick(n)).collect();
    if chosen.len() < 2 { chosen = comps.iter().filter(|p| dll_arch(p) == PeArch::X64).take(3).cloned().collect(); }
    if chosen.is_empty() { eprintln!("SELFTEST: no x64 components found"); return 1; }
    if !worker_x64.exists() { eprintln!("SELFTEST: x64 worker not built ({})", worker_x64.display()); return 1; }
    let worker_dir = worker_x64.parent().unwrap();
    for d in &chosen { stage_companion_dlls(d.parent().unwrap_or(worker_dir), worker_dir); }
    let mut w = match Worker::spawn(worker_x64, worker_dir) { Ok(w) => w, Err(e) => { eprintln!("SELFTEST: spawn failed: {e}"); return 1; } };

    println!("SELFTEST: cumulative RMS dBFS as each stage is added (raw {:.2}):", rms_db(raw));
    let mut last: Vec<f32> = Vec::new();
    for k in 1..=chosen.len() {
        let stages: Vec<(String, Vec<u8>)> = chosen[..k].iter().map(|d| (d.to_string_lossy().into_owned(), Vec::new())).collect();
        let names = match w.chain(&stages, sr, 2) { Ok(n) => n, Err(e) => { eprintln!("SELFTEST: CHAN failed: {e}"); w.quit(); return 1; } };
        let out = match preprocess(&mut w, raw) { Ok(o) => o, Err(e) => { eprintln!("SELFTEST: process failed: {e}"); w.quit(); return 1; } };
        println!("  chain[{k}] {:<48} out {:>7.2}  ({:+.2} vs raw)", names.join(" → "), rms_db(&out), rms_db(&out) - rms_db(raw));
        last = out;
    }
    w.quit();
    // Success = the full chain actually changed the signal (and produced audio).
    let changed = last.len() >= 2 && last.iter().zip(raw.iter()).any(|(a, b)| (a - b).abs() > 1e-6);
    if changed { println!("SELFTEST: PASS — {}-stage chain processed over the worker IPC", chosen.len()); 0 }
    else { eprintln!("SELFTEST: FAIL — chain output == raw (no processing)"); 1 }
}

fn main() -> Result<(), Box<dyn Error>> {
    let root = proto_root();
    let worker_x64 = root.join("host").join("build").join("x64").join("Debug").join("foo_dsp_host.exe");
    let worker_x86 = root.join("host").join("build").join("Win32").join("Debug").join("foo_dsp_host.exe");
    let extract = root.join("_extract");

    let argv: Vec<String> = std::env::args().collect();
    let probe = argv.iter().any(|x| x == "--probe");
    let selftest = argv.iter().any(|x| x == "--selftest");
    let mut src_path: Option<PathBuf> = None;
    let mut shot_path: Option<PathBuf> = None;
    let mut a = 1; while a < argv.len() {
        if (argv[a] == "--src" || argv[a] == "--wav") && a + 1 < argv.len() { src_path = Some(PathBuf::from(&argv[a + 1])); a += 1; }
        else if argv[a] == "--shot" && a + 1 < argv.len() { shot_path = Some(PathBuf::from(&argv[a + 1])); a += 1; }
        a += 1;
    }
    if src_path.is_none() { for c in ["sample/demo.wav", "test.mp3", "test.flac", "test.wav", "test.ogg"] { let p = root.join(c); if p.exists() { src_path = Some(p); break; } } }
    let dev_rate = if selftest { 44100 } else { device_sr() };
    let (decoded, src_sr) = match &src_path {
        Some(p) => decode_audio(p).or_else(|_| read_wav(p)).unwrap_or_else(|e| { eprintln!("decode {} failed: {e} — using tone", p.display()); tone() }),
        None => { eprintln!("no test.mp3/.flac/.wav in {} — using a 220 Hz tone", root.display()); tone() }
    };
    let raw = resample_stereo(&decoded, src_sr, dev_rate);
    let sr = dev_rate;
    println!("source: {} | {} frames @ {} Hz -> {} Hz", src_path.as_ref().map(|p| fname(p)).unwrap_or_else(|| "tone".into()), decoded.len() / 2, src_sr, dev_rate);
    if probe { println!("PROBE: decoded {} frames @ {}Hz -> {} frames @ {}Hz, rms {:.2} dBFS — OK", decoded.len() / 2, src_sr, raw.len() / 2, sr, rms_db(&raw)); return Ok(()); }

    // Components live in _extract\x64 and _extract\x86 (extract-components.ps1), or flat in _extract.
    let scan = |dir: PathBuf| -> Vec<PathBuf> { std::fs::read_dir(&dir).map(|rd| rd.filter_map(|e| e.ok()).map(|e| e.path())
        .filter(|p| p.extension().map_or(false, |x| x == "dll") && fname(p).starts_with("foo")).collect()).unwrap_or_default() };
    let mut comps: Vec<PathBuf> = scan(extract.join("x64"));
    for p in scan(extract.join("x86")) { if !comps.iter().any(|q| fname(q) == fname(&p)) { comps.push(p); } }
    if comps.is_empty() { comps = scan(extract.clone()); } // flat layout fallback
    comps.sort_by_key(|p| fname(p));
    if comps.is_empty() { return Err("no components — drop .fb2k-component files in components\\ then run extract-components.ps1 (or just run-lab.bat)".into()); }

    if selftest { std::process::exit(run_selftest(&worker_x64, &comps, &raw, sr)); }

    let shared = Arc::new(Shared {
        raw: ArcSwap::from_pointee(raw), proc: ArcSwap::from_pointee(Vec::new()), chain_names: ArcSwap::from_pointee(Vec::new()),
        bypass: AtomicBool::new(true), playing: AtomicBool::new(true), proc_ready: AtomicBool::new(false),
        pos: AtomicUsize::new(0), in_rms: AtomicU32::new(0), out_rms: AtomicU32::new(0), sr: AtomicU32::new(sr),
    });
    let status = Arc::new(Mutex::new(String::from("pick a DSP, Add it, build a chain, then Apply")));
    let (tx, rx) = mpsc::channel();
    let engine = { let s = shared.clone(); let st = status.clone(); let (wx64, wx86) = (worker_x64.clone(), worker_x86.clone()); std::thread::spawn(move || engine_thread(rx, s, st, wx64, wx86)) };

    let tx_quit = tx.clone();
    let opts = eframe::NativeOptions { viewport: egui::ViewportBuilder::default().with_inner_size([600.0, 560.0]).with_title("Resonance foobar2000 DSP Lab"), ..Default::default() };
    let app_shared = shared.clone();
    let shot = shot_path.clone();
    eframe::run_native("Resonance foobar2000 DSP Lab", opts, Box::new(move |_cc| Ok(Box::new(LabApp::new(app_shared, status, tx, comps, shot)))))
        .map_err(|e| format!("eframe: {e}"))?;

    let _ = tx_quit.send(EngineMsg::Quit);
    let _ = engine.join();
    Ok(())
}

fn tone() -> (Vec<f32>, u32) {
    let sr = 44100u32; let n = (sr * 3) as usize; let mut v = Vec::with_capacity(n * 2);
    for k in 0..n { let s = 0.4 * (std::f32::consts::TAU * 220.0 * k as f32 / sr as f32).sin(); v.push(s); v.push(s); }
    (v, sr)
}
