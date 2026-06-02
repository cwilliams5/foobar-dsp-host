// Resonance foobar2000 DSP Lab — egui GUI orchestrator.
//
// Pick a foobar2000 DSP, Load it (spawns an ISOLATED subprocess worker that hosts the plugin),
// hear your track stream through it live, A/B bypass↔processed instantly, watch IN/OUT level meters,
// and click "Open config…" to pop the plugin's OWN settings dialog and re-process. A worker crash
// drops the pipe; the lab survives. Worker IPC runs on a background thread so the UI never freezes.

use arc_swap::ArcSwap;
use eframe::egui;
use std::error::Error;
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
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
struct Worker { child: Child, w: BufWriter<std::process::ChildStdin>, r: BufReader<std::process::ChildStdout> }
impl Worker {
    fn spawn(exe: &Path, workdir: &Path) -> std::io::Result<Worker> {
        let mut child = Command::new(exe).arg("--worker").current_dir(workdir)
            .stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::inherit()).spawn()?;
        let w = BufWriter::new(child.stdin.take().unwrap());
        let r = BufReader::new(child.stdout.take().unwrap());
        Ok(Worker { child, w, r })
    }
    fn tag(&mut self, t: &[u8; 4]) -> std::io::Result<()> { self.w.write_all(t) }
    fn u32(&mut self, v: u32) -> std::io::Result<()> { self.w.write_all(&v.to_le_bytes()) }
    fn bytes(&mut self, b: &[u8]) -> std::io::Result<()> { self.w.write_all(b) }
    fn str(&mut self, s: &str) -> std::io::Result<()> { self.u32(s.len() as u32)?; self.bytes(s.as_bytes()) }
    fn flush(&mut self) -> std::io::Result<()> { self.w.flush() }
    fn rd_tag(&mut self) -> std::io::Result<[u8; 4]> { let mut t = [0u8; 4]; self.r.read_exact(&mut t)?; Ok(t) }
    fn rd_u32(&mut self) -> std::io::Result<u32> { let mut b = [0u8; 4]; self.r.read_exact(&mut b)?; Ok(u32::from_le_bytes(b)) }
    fn rd_bytes(&mut self, n: usize) -> std::io::Result<Vec<u8>> { let mut v = vec![0u8; n]; self.r.read_exact(&mut v)?; Ok(v) }
    fn rd_str(&mut self) -> std::io::Result<String> { let n = self.rd_u32()? as usize; Ok(String::from_utf8_lossy(&self.rd_bytes(n)?).into_owned()) }
    fn read_pcm(&mut self, nch: usize) -> Result<Vec<f32>, String> {
        let nf = self.rd_u32().map_err(|e| e.to_string())? as usize;
        let bytes = self.rd_bytes(nf * nch * 4).map_err(|e| e.to_string())?;
        Ok(bytes.chunks_exact(4).map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]])).collect())
    }
    fn load(&mut self, dll: &str, sr: u32, nch: u32, idx: u32) -> Result<String, String> {
        self.tag(b"LOAD").and_then(|_| self.str(dll)).and_then(|_| self.u32(sr)).and_then(|_| self.u32(nch)).and_then(|_| self.u32(idx)).and_then(|_| self.flush()).map_err(|e| e.to_string())?;
        match &self.rd_tag().map_err(|e| e.to_string())? {
            b"LOK " => { let _ = self.rd_u32().map_err(|e| e.to_string())?; self.rd_str().map_err(|e| e.to_string()) }
            b"ERR " => Err(self.rd_str().unwrap_or_default()),
            o => Err(format!("unexpected {:?}", o)),
        }
    }
    fn process(&mut self, block: &[f32], frames: u32, nch: usize) -> Result<Vec<f32>, String> {
        self.tag(b"PROC").and_then(|_| self.u32(frames)).map_err(|e| e.to_string())?;
        let mut raw = Vec::with_capacity(block.len() * 4);
        for s in block { raw.extend_from_slice(&s.to_le_bytes()); }
        self.bytes(&raw).and_then(|_| self.flush()).map_err(|e| e.to_string())?;
        match &self.rd_tag().map_err(|e| e.to_string())? { b"POK " => self.read_pcm(nch), b"ERR " => Err(self.rd_str().unwrap_or_default()), o => Err(format!("unexpected {:?}", o)) }
    }
    fn drain(&mut self, nch: usize) -> Result<Vec<f32>, String> {
        self.tag(b"FLSH").and_then(|_| self.flush()).map_err(|e| e.to_string())?;
        match &self.rd_tag().map_err(|e| e.to_string())? { b"POK " => self.read_pcm(nch), b"ERR " => Err(self.rd_str().unwrap_or_default()), o => Err(format!("unexpected {:?}", o)) }
    }
    fn config(&mut self) -> Result<Vec<u8>, String> {
        self.tag(b"CFG ").and_then(|_| self.flush()).map_err(|e| e.to_string())?;
        match &self.rd_tag().map_err(|e| e.to_string())? { b"PRE " => { let n = self.rd_u32().map_err(|e| e.to_string())? as usize; self.rd_bytes(n).map_err(|e| e.to_string()) } o => Err(format!("unexpected {:?}", o)) }
    }
    fn set_preset(&mut self, blob: &[u8]) -> Result<(), String> {
        self.tag(b"SPRE").and_then(|_| self.u32(blob.len() as u32)).and_then(|_| self.bytes(blob)).and_then(|_| self.flush()).map_err(|e| e.to_string())?;
        match &self.rd_tag().map_err(|e| e.to_string())? { b"OK  " => Ok(()), b"ERR " => Err(self.rd_str().unwrap_or_default()), o => Err(format!("unexpected {:?}", o)) }
    }
    fn quit(&mut self) { let _ = self.tag(b"QUIT"); let _ = self.flush(); let _ = self.child.wait(); }
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
fn preprocess(w: &mut Worker, raw: &[f32]) -> Result<Vec<f32>, String> {
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
    bypass: AtomicBool,
    playing: AtomicBool,
    proc_ready: AtomicBool,
    pos: AtomicUsize,
    in_rms: AtomicU32,
    out_rms: AtomicU32,
    sr: AtomicU32,
}

enum EngineMsg { Load(PathBuf), Config, Quit }

fn engine_thread(rx: Receiver<EngineMsg>, shared: Arc<Shared>, status: Arc<Mutex<String>>, worker_x64: PathBuf, worker_x86: PathBuf) {
    let sr = shared.sr.load(Relaxed);
    let set = |s: String| *status.lock().unwrap() = s;
    let mut worker: Option<Worker> = None;
    let mut cur_dll: Option<PathBuf> = None;
    while let Ok(msg) = rx.recv() {
        match msg {
            EngineMsg::Load(dll) => {
                if let Some(mut w) = worker.take() { w.quit(); }
                shared.proc_ready.store(false, Relaxed);
                shared.bypass.store(true, Relaxed);
                // Pick the worker matching the component's architecture (a 32-bit DLL needs the x86 worker).
                let is_x86 = dll_arch(&dll) == 0x14C;
                let worker_exe = if is_x86 { &worker_x86 } else { &worker_x64 };
                if !worker_exe.exists() {
                    set(format!("{} is {}-bit but the {} worker isn't built — build.ps1 builds both", fname(&dll), if is_x86 { "32" } else { "64" }, if is_x86 { "x86" } else { "x64" }));
                    continue;
                }
                let worker_dir = worker_exe.parent().unwrap();
                stage_companions(dll.parent().unwrap_or(worker_dir), worker_dir); // arch-matched companions (e.g. soxr) next to the worker
                set(format!("loading {} ({}) …", fname(&dll), if is_x86 { "x86" } else { "x64" }));
                let mut w = match Worker::spawn(worker_exe, worker_dir) { Ok(w) => w, Err(e) => { set(format!("worker spawn failed: {e}")); continue; } };
                match w.load(&dll.to_string_lossy(), sr, 2, 0) {
                    Ok(name) => {
                        // lab persistence: restore this component's saved preset, if any
                        let mut restored = "";
                        if let Ok(blob) = std::fs::read(preset_path(&dll)) { if !blob.is_empty() && w.set_preset(&blob).is_ok() { restored = " (restored saved)"; } }
                        set(format!("“{name}” — processing track …{restored}"));
                        let raw = shared.raw.load_full();
                        match preprocess(&mut w, &raw) {
                            Ok(proc) => {
                                let delta = rms_db(&proc) - rms_db(&raw);
                                shared.proc.store(Arc::new(proc));
                                shared.proc_ready.store(true, Relaxed);
                                shared.bypass.store(false, Relaxed);
                                set(format!("“{name}” ready — processed {delta:+.2} dB vs raw"));
                                cur_dll = Some(dll.clone());
                                worker = Some(w);
                            }
                            Err(e) => { set(format!("process failed: {e}")); w.quit(); }
                        }
                    }
                    Err(e) => { set(format!("load failed: {e} (an x86-only component needs the x86 worker)")); w.quit(); }
                }
            }
            EngineMsg::Config => {
                if let Some(w) = worker.as_mut() {
                    set("opening the plugin’s config dialog — adjust + OK …".into());
                    match w.config() {
                        Ok(blob) => {
                            // lab persistence: remember this component's settings across runs
                            if let Some(d) = &cur_dll { let pf = preset_path(d); if let Some(parent) = pf.parent() { let _ = std::fs::create_dir_all(parent); } let _ = std::fs::write(&pf, &blob); }
                            let raw = shared.raw.load_full();
                            match preprocess(w, &raw) {
                                Ok(proc) => { let delta = rms_db(&proc) - rms_db(&raw); shared.proc.store(Arc::new(proc)); shared.bypass.store(false, Relaxed); set(format!("saved + reprocessed — {delta:+.2} dB vs raw")); }
                                Err(e) => set(format!("reprocess failed: {e}")),
                            }
                        }
                        Err(e) => set(format!("config failed: {e}")),
                    }
                } else { set("load a DSP first".into()); }
            }
            EngineMsg::Quit => { if let Some(mut w) = worker.take() { w.quit(); } break; }
        }
    }
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

// Copy companion DLLs (e.g. soxr64.dll) next to the worker so loaded components resolve them.
// Lab persistence: where a component's saved preset lives — one opaque dsp_preset blob per component,
// in presets/ (gitignored). The "real" question of where presets live in an app is the integration's job.
fn preset_path(dll: &Path) -> PathBuf {
    let stem = dll.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| "dsp".into());
    proto_root().join("presets").join(format!("{stem}.preset"))
}

fn stage_companions(src_dir: &Path, worker_dir: &Path) {
    if let Ok(rd) = std::fs::read_dir(src_dir) {
        for e in rd.flatten() {
            let p = e.path();
            let nm = fname(&p);
            // companion DLLs only — never a component (foo*) and never overwrite the worker's own shared.dll
            if p.extension().map_or(false, |x| x == "dll") && !nm.starts_with("foo") && !nm.eq_ignore_ascii_case("shared.dll") {
                let _ = std::fs::copy(&p, worker_dir.join(p.file_name().unwrap()));
            }
        }
    }
}

// Read a DLL's PE machine type: 0x8664 = x64, 0x14C = x86 (0 on error / not a PE).
fn dll_arch(p: &Path) -> u16 {
    use std::io::Read;
    let mut f = match std::fs::File::open(p) { Ok(f) => f, Err(_) => return 0 };
    let mut b = [0u8; 1024];
    let n = f.read(&mut b).unwrap_or(0);
    if n < 0x40 || &b[0..2] != b"MZ" { return 0; }
    let e = u32::from_le_bytes([b[0x3C], b[0x3D], b[0x3E], b[0x3F]]) as usize;
    if e + 6 > n || &b[e..e + 4] != b"PE\0\0" { return 0; }
    u16::from_le_bytes([b[e + 4], b[e + 5]])
}

// ----------------------------- egui app -----------------------------
struct LabApp {
    shared: Arc<Shared>,
    status: Arc<Mutex<String>>,
    tx: Sender<EngineMsg>,
    comps: Vec<PathBuf>,
    names: Vec<String>,
    selected: usize,
    stream_err: Option<String>,
    _stream: Option<cpal::Stream>,
}
impl LabApp {
    fn new(shared: Arc<Shared>, status: Arc<Mutex<String>>, tx: Sender<EngineMsg>, comps: Vec<PathBuf>) -> Self {
        let names = comps.iter().map(|p| fname(p)).collect();
        let (stream, stream_err) = match build_stream(shared.clone()) { Ok(s) => (Some(s), None), Err(e) => (None, Some(e)) };
        LabApp { shared, status, tx, comps, names, selected: 0, stream_err, _stream: stream }
    }
}
fn meter(ui: &mut egui::Ui, label: &str, db: f32) {
    let frac = ((db + 60.0) / 60.0).clamp(0.0, 1.0);
    ui.horizontal(|ui| {
        ui.label(format!("{label:>3}"));
        ui.add(egui::ProgressBar::new(frac).desired_width(280.0).text(format!("{db:>6.1} dBFS")));
    });
}
impl eframe::App for LabApp {
    fn update(&mut self, ctx: &egui::Context, _f: &mut eframe::Frame) {
        ctx.request_repaint(); // animate meters
        egui::CentralPanel::default().show(ctx, |ui| {
            ui.heading("Resonance · foobar2000 DSP Lab");
            ui.label(egui::RichText::new("real foobar2000 DSP components, hosted in an isolated worker process").italics().weak());
            ui.separator();

            ui.horizontal(|ui| {
                let names = self.names.clone();
                egui::ComboBox::from_id_source("dsp").width(300.0)
                    .selected_text(names.get(self.selected).cloned().unwrap_or_default())
                    .show_index(ui, &mut self.selected, names.len(), |i| names[i].clone());
                if ui.button("Load").clicked() {
                    if let Some(p) = self.comps.get(self.selected) { let _ = self.tx.send(EngineMsg::Load(p.clone())); }
                }
            });

            ui.add_space(4.0);
            ui.label(self.status.lock().unwrap().clone());
            if let Some(e) = &self.stream_err { ui.colored_label(egui::Color32::YELLOW, format!("⚠ no audio output ({e}) — A/B + meters still run")); }
            ui.separator();

            // transport + A/B
            let playing = self.shared.playing.load(Relaxed);
            ui.horizontal(|ui| {
                if ui.button(if playing { "⏸  Pause" } else { "▶  Play" }).clicked() { self.shared.playing.store(!playing, Relaxed); }
                ui.separator();
                let mut bypass = self.shared.bypass.load(Relaxed);
                let ready = self.shared.proc_ready.load(Relaxed);
                ui.label("Monitor:");
                ui.selectable_value(&mut bypass, true, "A · Bypass (raw)");
                ui.add_enabled_ui(ready, |ui| { ui.selectable_value(&mut bypass, false, "B · Processed"); });
                self.shared.bypass.store(if !ready { true } else { bypass }, Relaxed);
                ui.separator();
                ui.add_enabled_ui(ready, |ui| { if ui.button("Open config…").clicked() { let _ = self.tx.send(EngineMsg::Config); } });
            });

            ui.add_space(8.0);
            meter(ui, "IN", 20.0 * f32::from_bits(self.shared.in_rms.load(Relaxed)).max(1e-9).log10());
            meter(ui, "OUT", 20.0 * f32::from_bits(self.shared.out_rms.load(Relaxed)).max(1e-9).log10());

            // position
            let sr = self.shared.sr.load(Relaxed).max(1) as f32;
            let total = self.shared.raw.load().len() / 2;
            let cur = self.shared.pos.load(Relaxed) / 2;
            ui.add_space(6.0);
            ui.label(format!("{:>5.1}s / {:>5.1}s   (loops)", cur as f32 / sr, total as f32 / sr));
            ui.add_space(8.0);
            ui.label(egui::RichText::new("A/B is instant (same play position). “Open config…” pops the plugin’s own dialog, then re-processes.").weak());
        });
    }
}

fn main() -> Result<(), Box<dyn Error>> {
    let root = proto_root();
    let worker_x64 = root.join("host").join("build").join("x64").join("Debug").join("foo_dsp_host.exe");
    let worker_x86 = root.join("host").join("build").join("Win32").join("Debug").join("foo_dsp_host.exe");
    let extract = root.join("_extract");

    // source: --src/--wav <file>, else test.{mp3,flac,wav,ogg} in THIS folder, else a tone
    let argv: Vec<String> = std::env::args().collect();
    let probe = argv.iter().any(|x| x == "--probe");
    let mut src_path: Option<PathBuf> = None;
    let mut a = 1; while a < argv.len() { if (argv[a] == "--src" || argv[a] == "--wav") && a + 1 < argv.len() { src_path = Some(PathBuf::from(&argv[a + 1])); a += 1; } a += 1; }
    if src_path.is_none() { for c in ["sample/demo.wav", "test.mp3", "test.flac", "test.wav", "test.ogg"] { let p = root.join(c); if p.exists() { src_path = Some(p); break; } } }
    let dev_rate = device_sr();
    let (decoded, src_sr) = match &src_path {
        Some(p) => decode_audio(p).or_else(|_| read_wav(p)).unwrap_or_else(|e| { eprintln!("decode {} failed: {e} — using tone", p.display()); tone() }),
        None => { eprintln!("no test.mp3/.flac/.wav in {} — using a 220 Hz tone", root.display()); tone() }
    };
    let raw = resample_stereo(&decoded, src_sr, dev_rate);
    let sr = dev_rate;
    println!("source: {} | {} frames @ {} Hz -> {} Hz device", src_path.as_ref().map(|p| fname(p)).unwrap_or_else(|| "tone".into()), decoded.len() / 2, src_sr, dev_rate);
    if probe { println!("PROBE: decoded {} frames @ {}Hz -> {} frames @ {}Hz, rms {:.2} dBFS — OK", decoded.len() / 2, src_sr, raw.len() / 2, sr, rms_db(&raw)); return Ok(()); }

    // Components live in _extract\x64 and _extract\x86 (filled by extract-components.ps1). Prefer the x64
    // build of a component; fall back to x86 (x86-only components). One dropdown entry per name — the
    // engine reads each DLL's PE header at Load time and spawns the matching-arch worker.
    let scan = |dir: PathBuf| -> Vec<PathBuf> { std::fs::read_dir(&dir).map(|rd| rd.filter_map(|e| e.ok()).map(|e| e.path())
        .filter(|p| p.extension().map_or(false, |x| x == "dll") && fname(p).starts_with("foo")).collect()).unwrap_or_default() };
    let mut comps: Vec<PathBuf> = scan(extract.join("x64"));
    for p in scan(extract.join("x86")) { if !comps.iter().any(|q| fname(q) == fname(&p)) { comps.push(p); } }
    comps.sort_by_key(|p| fname(p));
    if comps.is_empty() { return Err("no components — drop .fb2k-component files in components\\ then run extract-components.ps1 (or just run-lab.bat)".into()); }

    let shared = Arc::new(Shared {
        raw: ArcSwap::from_pointee(raw), proc: ArcSwap::from_pointee(Vec::new()),
        bypass: AtomicBool::new(true), playing: AtomicBool::new(true), proc_ready: AtomicBool::new(false),
        pos: AtomicUsize::new(0), in_rms: AtomicU32::new(0), out_rms: AtomicU32::new(0), sr: AtomicU32::new(sr),
    });
    let status = Arc::new(Mutex::new(String::from("pick a DSP and press Load")));
    let (tx, rx) = mpsc::channel();
    let engine = { let s = shared.clone(); let st = status.clone(); let (wx64, wx86) = (worker_x64.clone(), worker_x86.clone()); std::thread::spawn(move || engine_thread(rx, s, st, wx64, wx86)) };

    let tx_quit = tx.clone();
    let opts = eframe::NativeOptions { viewport: egui::ViewportBuilder::default().with_inner_size([560.0, 420.0]).with_title("Resonance foobar2000 DSP Lab"), ..Default::default() };
    let app_shared = shared.clone();
    eframe::run_native("Resonance foobar2000 DSP Lab", opts, Box::new(move |_cc| Ok(Box::new(LabApp::new(app_shared, status, tx, comps)))))
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
