//! Host real **foobar2000 DSP components** (`foo_dsp_*`) outside foobar2000.
//!
//! foobar2000's component model is a COM-like C++ service SDK, so the plugin runs in the
//! repo's C++ **worker** (`foo_dsp_host.exe --worker`, built by `build.ps1` from the
//! bundled BSD-licensed SDK) — an isolated child process this crate spawns and drives
//! over framed stdio. A component crash kills the worker, not you: every call then
//! returns [`Error::WorkerGone`] and your process carries on.
//!
//! The worker hosts a **chain** of N ≥ 1 DSPs, run in series by the SDK's own
//! `dsp_manager` — one [`Worker::process`] round-trip drives the whole chain.
//!
//! ```no_run
//! use std::path::Path;
//! use foobar_dsp_host::{StageSpec, Worker};
//!
//! # fn main() -> foobar_dsp_host::Result<()> {
//! let mut w = Worker::spawn(Path::new(r"host\build\x64\Debug\foo_dsp_host.exe"))?;
//! let names = w.build_chain(44100, 2, &[
//!     StageSpec::new(r"C:\components\foo_dsp_xgeq.dll"),
//!     StageSpec::new(r"C:\components\foo_dsp_vlevel.dll"),
//! ])?;
//! println!("chain: {}", names.join(" -> "));
//! let block = vec![0.0f32; 8192 * 2];          // interleaved L R L R …
//! let processed = w.process(&block)?.to_vec(); // one IPC round-trip runs the whole chain
//! let tail = w.drain()?.to_vec();              // end-of-stream: look-ahead tails
//! # let _ = (processed.len(), tail.len());
//! # Ok(())
//! # }
//! ```
//!
//! **Worker placement:** the worker must run with `shared.dll` beside it (the build
//! stages it next to the exe) — [`Worker::spawn`] therefore defaults the child's working
//! directory to the exe's folder. Components' companion DLLs (e.g. `soxr64.dll`) must be
//! resolvable too; [`stage_companion_dlls`] copies them beside the worker like the lab
//! does.
//!
//! **Architecture routing:** a 64-bit worker cannot load a 32-bit component. Probe a
//! component with [`dll_arch`] and spawn the matching worker
//! (`host\build\x64\…` / `host\build\Win32\…`). A chain must be single-arch; running a
//! mixed-arch rack means one worker per arch with the host splicing blocks between them —
//! that router is the consuming application's job, not this crate's.
//!
//! **State & config ownership:** the crate carries the *mechanism* — opaque
//! `dsp_preset` blobs ([`Worker::get_preset`] / [`Worker::set_preset`] /
//! [`StageSpec::preset`], and [`Worker::configure`] returns the blob the user's dialog
//! produced). *Where* blobs persist is your application's policy. Blobs are opaque:
//! round-trip them, never parse them.
//!
//! Threading: requests are synchronous and single-flight. [`Worker::configure`] opens the
//! component's own **modal** dialog and blocks until the user closes it — call it from a
//! thread that may wait.

use std::ffi::OsString;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::process::ExitStatus;

pub use tagpipe;
pub use tagpipe::{Error, PeerInfo, Result, TRANSPORT_VERSION};

use tagpipe::{expect_reply, frame, StderrMode, WorkerProcess};

/// Version of this worker's command vocabulary, exchanged in the transport handshake
/// (the C++ worker answers with its own; the handshake enforces an exact match).
pub const VOCAB_VERSION: u32 = 1;

/// The worker binary's file name (per-arch copies live in their own folders).
pub const WORKER_EXE_NAME: &str = "foo_dsp_host.exe";

/// Request/reply tags of the worker vocabulary (see `docs/PROTOCOL.md`).
pub mod tags {
    use tagpipe::Tag;
    pub const CHAN: Tag = *b"CHAN";
    pub const LOAD: Tag = *b"LOAD";
    pub const RST: Tag = *b"RST ";
    pub const PROC: Tag = *b"PROC";
    pub const FLSH: Tag = *b"FLSH";
    pub const CFG: Tag = *b"CFG ";
    pub const GPRE: Tag = *b"GPRE";
    pub const SPRE: Tag = *b"SPRE";
    // Replies.
    pub const COK: Tag = *b"COK ";
    pub const LOK: Tag = *b"LOK ";
    pub const OK: Tag = *b"OK  ";
    pub const POK: Tag = *b"POK ";
    pub const PRE: Tag = *b"PRE ";
}

/// One stage of a chain: a component DLL, which `dsp_entry` inside it (0 = the usual
/// single entry), and an optional opaque `dsp_preset` blob (empty = the entry's default).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StageSpec {
    pub dll: PathBuf,
    pub entry_index: u32,
    /// Opaque `dsp_preset` blob (16-byte owner GUID + data) from a previous
    /// [`Worker::configure`]/[`Worker::get_preset`]. Empty = the entry's default preset.
    pub preset: Vec<u8>,
}

impl StageSpec {
    /// A stage with entry 0 and the default preset.
    pub fn new(dll: impl Into<PathBuf>) -> StageSpec {
        StageSpec {
            dll: dll.into(),
            entry_index: 0,
            preset: Vec::new(),
        }
    }
}

/// Spawn-time options.
#[derive(Debug, Default, Clone)]
pub struct SpawnOptions {
    /// The worker's working directory. `None` (default) = the worker exe's own folder —
    /// required so it finds `shared.dll`, which the build stages beside it.
    pub working_dir: Option<PathBuf>,
    /// Capture the worker's stderr (diagnostics) instead of inheriting it — take the pipe
    /// via [`Worker::take_stderr`] and drain it on a thread.
    pub capture_stderr: bool,
}

/// A live worker hosting one DSP chain.
///
/// Every method is one synchronous request/reply. If the worker dies (a component
/// crash), the in-flight and all subsequent calls return [`Error::WorkerGone`] — check
/// [`Worker::try_wait`], drop the chain, carry on.
pub struct Worker {
    proc_: WorkerProcess,
    peer: PeerInfo,
    channels: u32,
    out_buf: Vec<f32>,
}

impl Worker {
    /// Spawns `worker_exe --worker` (cwd = the exe's folder) and performs the version
    /// handshake. See [`Worker::spawn_with`].
    pub fn spawn(worker_exe: &Path) -> Result<Worker> {
        Worker::spawn_with(worker_exe, &SpawnOptions::default())
    }

    /// Spawns the worker with options and performs the mandatory transport handshake
    /// (exact match on transport + vocabulary versions; on mismatch the worker is killed
    /// and the error names who speaks what).
    pub fn spawn_with(worker_exe: &Path, opts: &SpawnOptions) -> Result<Worker> {
        let cwd: Option<PathBuf> = opts
            .working_dir
            .clone()
            .or_else(|| worker_exe.parent().map(Path::to_path_buf));
        let stderr_mode = if opts.capture_stderr {
            StderrMode::Capture
        } else {
            StderrMode::Inherit
        };
        let mut proc_ = WorkerProcess::spawn(
            worker_exe,
            [OsString::from("--worker")],
            cwd.as_deref(),
            stderr_mode,
        )
        .map_err(Error::Io)?;
        let peer = match proc_.handshake(VOCAB_VERSION) {
            Ok(p) => p,
            Err(e) => {
                let _ = proc_.kill(); // mismatched or broken worker — don't leave it running
                return Err(e);
            }
        };
        Ok(Worker {
            proc_,
            peer,
            channels: 2,
            out_buf: Vec::new(),
        })
    }

    /// What the worker reported in the handshake (versions + ident, e.g.
    /// `"foo_dsp_host-worker 0.1.0 (64-bit)"`).
    pub fn peer(&self) -> &PeerInfo {
        &self.peer
    }

    /// OS process id of the worker (diagnostics).
    pub fn id(&self) -> u32 {
        self.proc_.id()
    }

    /// Builds an N-stage chain atomically (replaces any current chain). Returns each
    /// stage's real DSP name. A failing stage aborts the whole build with its error.
    pub fn build_chain(
        &mut self,
        sample_rate: u32,
        channels: u32,
        stages: &[StageSpec],
    ) -> Result<Vec<String>> {
        let w = &mut self.proc_.stdin;
        frame::write_tag(w, tags::CHAN)?;
        frame::write_u32(w, sample_rate)?;
        frame::write_u32(w, channels)?;
        frame::write_u32(w, stages.len() as u32)?;
        for s in stages {
            frame::write_string(w, &s.dll.to_string_lossy())?;
            frame::write_u32(w, s.entry_index)?;
            frame::write_blob(w, &s.preset)?;
        }
        use std::io::Write;
        w.flush().map_err(Error::from)?;

        expect_reply(&mut self.proc_.stdout, tags::COK)?;
        let n = frame::read_u32(&mut self.proc_.stdout)?;
        let mut names = Vec::with_capacity(n as usize);
        for _ in 0..n {
            names.push(frame::read_string(&mut self.proc_.stdout)?);
        }
        self.channels = channels.max(1);
        Ok(names)
    }

    /// Convenience: a single-stage chain with the entry's default preset
    /// (≡ [`Worker::build_chain`] with one default stage). Returns the DSP's name.
    pub fn load_single(
        &mut self,
        dll: &Path,
        sample_rate: u32,
        channels: u32,
        entry_index: u32,
    ) -> Result<String> {
        let w = &mut self.proc_.stdin;
        frame::write_tag(w, tags::LOAD)?;
        frame::write_string(w, &dll.to_string_lossy())?;
        frame::write_u32(w, sample_rate)?;
        frame::write_u32(w, channels)?;
        frame::write_u32(w, entry_index)?;
        use std::io::Write;
        w.flush().map_err(Error::from)?;

        expect_reply(&mut self.proc_.stdout, tags::LOK)?;
        let _status = frame::read_u32(&mut self.proc_.stdout)?; // 1 on the success path
        let name = frame::read_string(&mut self.proc_.stdout)?;
        self.channels = channels.max(1);
        Ok(name)
    }

    /// Drops the instantiated DSPs; the next [`Worker::process`] re-instantiates the
    /// whole chain fresh (use before re-processing a track after a config change).
    /// The chain *config* is kept.
    pub fn reset(&mut self) -> Result<()> {
        let w = &mut self.proc_.stdin;
        frame::write_tag(w, tags::RST)?;
        use std::io::Write;
        w.flush().map_err(Error::from)?;
        expect_reply(&mut self.proc_.stdout, tags::OK)
    }

    /// Streams one interleaved f32 block through the whole chain (state preserved across
    /// calls) and returns the processed block. The output frame count may differ from the
    /// input (resamplers, look-ahead latency) — a real-time consumer should buffer. The
    /// returned slice borrows an internal buffer reused across calls; copy it out to keep
    /// it past the next call, or use [`Worker::process_into`].
    pub fn process(&mut self, samples: &[f32]) -> Result<&[f32]> {
        self.proc_flsh(Some(samples))?;
        Ok(&self.out_buf)
    }

    /// [`Worker::process`] into a caller-owned buffer; returns the output frame count.
    pub fn process_into(&mut self, samples: &[f32], out: &mut Vec<f32>) -> Result<u32> {
        self.write_proc(Some(samples))?;
        self.read_pcm_into(out)
    }

    /// End-of-stream drain: every stage emits its buffered/look-ahead tail. Send once
    /// after the last [`Worker::process`] of a stream.
    pub fn drain(&mut self) -> Result<&[f32]> {
        self.proc_flsh(None)?;
        Ok(&self.out_buf)
    }

    /// [`Worker::drain`] into a caller-owned buffer; returns the frame count.
    pub fn drain_into(&mut self, out: &mut Vec<f32>) -> Result<u32> {
        self.write_proc(None)?;
        self.read_pcm_into(out)
    }

    fn proc_flsh(&mut self, samples: Option<&[f32]>) -> Result<()> {
        self.write_proc(samples)?;
        let mut buf = std::mem::take(&mut self.out_buf);
        let res = self.read_pcm_into(&mut buf);
        self.out_buf = buf;
        res.map(|_| ())
    }

    fn write_proc(&mut self, samples: Option<&[f32]>) -> Result<()> {
        let w = &mut self.proc_.stdin;
        match samples {
            Some(s) => {
                debug_assert_eq!(s.len() % self.channels.max(1) as usize, 0);
                frame::write_tag(w, tags::PROC)?;
                frame::write_u32(w, (s.len() / self.channels.max(1) as usize) as u32)?;
                frame::write_f32(w, s)?;
            }
            None => frame::write_tag(w, tags::FLSH)?,
        }
        use std::io::Write;
        w.flush().map_err(Error::from)
    }

    fn read_pcm_into(&mut self, out: &mut Vec<f32>) -> Result<u32> {
        expect_reply(&mut self.proc_.stdout, tags::POK)?;
        let frames = frame::read_u32(&mut self.proc_.stdout)?;
        out.resize(frames as usize * self.channels.max(1) as usize, 0.0);
        frame::read_f32_into(&mut self.proc_.stdout, out)?;
        Ok(frames)
    }

    /// Opens stage `stage`'s OWN config dialog (parented to the worker's hidden window),
    /// applies the result to the chain, and returns the resulting opaque preset blob —
    /// persist it and hand it back via [`StageSpec::preset`] / [`Worker::set_preset`].
    ///
    /// **Blocks until the user closes the dialog** (foobar2000 DSP config popups are
    /// modal *inside the worker* — the worker is frozen for the dialog's lifetime, so no
    /// other request can be serviced meanwhile). To **cancel** an open dialog from another
    /// thread — e.g. the user removed the plugin or rebuilt the chain — call
    /// [`Worker::killer`]`().kill()`: terminating the worker is the only way to dismiss a
    /// modal popup from out-of-process, and this call then returns [`Error::WorkerGone`].
    /// (The reference lab does exactly this; build a fresh worker for the new chain.)
    pub fn configure(&mut self, stage: u32) -> Result<Vec<u8>> {
        let w = &mut self.proc_.stdin;
        frame::write_tag(w, tags::CFG)?;
        frame::write_u32(w, stage)?;
        use std::io::Write;
        w.flush().map_err(Error::from)?;
        expect_reply(&mut self.proc_.stdout, tags::PRE)?;
        frame::read_blob(&mut self.proc_.stdout)
    }

    /// Reads stage `stage`'s current opaque preset blob (no dialog).
    pub fn get_preset(&mut self, stage: u32) -> Result<Vec<u8>> {
        let w = &mut self.proc_.stdin;
        frame::write_tag(w, tags::GPRE)?;
        frame::write_u32(w, stage)?;
        use std::io::Write;
        w.flush().map_err(Error::from)?;
        expect_reply(&mut self.proc_.stdout, tags::PRE)?;
        frame::read_blob(&mut self.proc_.stdout)
    }

    /// Sets stage `stage`'s preset from an opaque blob (the rest of the chain is kept).
    pub fn set_preset(&mut self, stage: u32, blob: &[u8]) -> Result<()> {
        let w = &mut self.proc_.stdin;
        frame::write_tag(w, tags::SPRE)?;
        frame::write_u32(w, stage)?;
        frame::write_blob(w, blob)?;
        use std::io::Write;
        w.flush().map_err(Error::from)?;
        expect_reply(&mut self.proc_.stdout, tags::OK)
    }

    /// Graceful shutdown: sends `QUIT` and waits for exit.
    pub fn quit(mut self) -> io::Result<ExitStatus> {
        let _ = frame::write_tag(&mut self.proc_.stdin, tagpipe::TAG_QUIT);
        {
            use std::io::Write;
            let _ = self.proc_.stdin.flush();
        }
        self.proc_.wait()
    }

    /// Hard-kill (your timeout policy: kill → blocked reads return [`Error::WorkerGone`]).
    pub fn kill(&mut self) -> io::Result<()> {
        self.proc_.kill()
    }

    /// Non-blocking exit check — `Some(status)` once the worker has exited.
    pub fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        self.proc_.try_wait()
    }

    /// A detached, `Send + Sync` kill handle for watchdog patterns (valid even after the
    /// `Worker` drops).
    pub fn killer(&self) -> io::Result<tagpipe::WorkerKiller> {
        self.proc_.killer()
    }

    /// The worker's stderr pipe, when spawned with [`SpawnOptions::capture_stderr`].
    pub fn take_stderr(&mut self) -> Option<std::process::ChildStderr> {
        self.proc_.stderr.take()
    }
}

/// A DLL's PE machine architecture — route components to the matching worker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PeArch {
    X64,
    X86,
    /// Not a readable PE file (or an exotic machine type).
    Unknown,
}

/// Reads a DLL's PE header machine type. A 64-bit worker cannot load a 32-bit component
/// (and vice versa) — spawn the worker whose arch matches.
pub fn dll_arch(path: &Path) -> PeArch {
    let mut f = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(_) => return PeArch::Unknown,
    };
    let mut b = [0u8; 1024];
    let n = f.read(&mut b).unwrap_or(0);
    if n < 0x40 || &b[0..2] != b"MZ" {
        return PeArch::Unknown;
    }
    let e = u32::from_le_bytes([b[0x3C], b[0x3D], b[0x3E], b[0x3F]]) as usize;
    if e + 6 > n || &b[e..e + 4] != b"PE\0\0" {
        return PeArch::Unknown;
    }
    match u16::from_le_bytes([b[e + 4], b[e + 5]]) {
        0x8664 => PeArch::X64,
        0x014C => PeArch::X86,
        _ => PeArch::Unknown,
    }
}

/// Copies a component's companion DLLs (e.g. `soxr64.dll`) beside the worker so the
/// loaded component can resolve them — skips `foo_*` components themselves and
/// `shared.dll` (already staged by the build). Call before building a chain whose
/// components ship companions.
pub fn stage_companion_dlls(component_dir: &Path, worker_dir: &Path) {
    let Ok(rd) = std::fs::read_dir(component_dir) else {
        return;
    };
    for e in rd.flatten() {
        let p = e.path();
        let name = p
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        let is_dll = p
            .extension()
            .is_some_and(|x| x.eq_ignore_ascii_case("dll"));
        if is_dll && !name.to_ascii_lowercase().starts_with("foo") && !name.eq_ignore_ascii_case("shared.dll") {
            if let Some(fname) = p.file_name() {
                let _ = std::fs::copy(&p, worker_dir.join(fname));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    /// Hand-build worker replies and check the client parses them — the C++ worker is the
    /// other half (exercised live in tests/worker_integration.rs).
    #[test]
    fn chain_reply_parses() {
        let mut wire = Vec::new();
        frame::write_tag(&mut wire, tags::COK).unwrap();
        frame::write_u32(&mut wire, 2).unwrap();
        frame::write_string(&mut wire, "Graphic Equalizer").unwrap();
        frame::write_string(&mut wire, "VLevel").unwrap();

        let mut r = Cursor::new(wire);
        expect_reply(&mut r, tags::COK).unwrap();
        let n = frame::read_u32(&mut r).unwrap();
        let names: Vec<String> = (0..n).map(|_| frame::read_string(&mut r).unwrap()).collect();
        assert_eq!(names, vec!["Graphic Equalizer", "VLevel"]);
    }

    #[test]
    fn err_reply_surfaces_as_remote() {
        let mut wire = Vec::new();
        tagpipe::write_err(&mut wire, "a chain stage failed to load").unwrap();
        match expect_reply(&mut Cursor::new(wire), tags::COK) {
            Err(Error::Remote(m)) => assert_eq!(m, "a chain stage failed to load"),
            other => panic!("expected Remote, got {other:?}"),
        }
    }

    #[test]
    fn preset_blob_is_opaque_bytes() {
        // PRE carries a length-prefixed blob (GUID16 + data) — parsed as raw bytes only.
        let blob: Vec<u8> = (0..24).collect();
        let mut wire = Vec::new();
        frame::write_tag(&mut wire, tags::PRE).unwrap();
        frame::write_blob(&mut wire, &blob).unwrap();
        let mut r = Cursor::new(wire);
        expect_reply(&mut r, tags::PRE).unwrap();
        assert_eq!(frame::read_blob(&mut r).unwrap(), blob);
    }

    #[test]
    fn dll_arch_unknown_for_non_pe() {
        let dir = std::env::temp_dir().join("fdh_arch_test");
        let _ = std::fs::create_dir_all(&dir);
        let p = dir.join("not_a_dll.dll");
        std::fs::write(&p, b"definitely not PE").unwrap();
        assert_eq!(dll_arch(&p), PeArch::Unknown);
        assert_eq!(dll_arch(Path::new(r"C:\does\not\exist.dll")), PeArch::Unknown);
    }
}
