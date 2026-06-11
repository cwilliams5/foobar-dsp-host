//! Live integration against the REAL C++ worker (`foo_dsp_host.exe --worker`, built by
//! `build.ps1`). Self-skips when the worker isn't built. No third-party components needed
//! — these prove the transport handshake, the recoverable-error path, empty-chain
//! processing, and the exit contracts. (Component-loading coverage lives in the lab's
//! `--selftest` + the ref-DSP test, which need DLLs.)

use std::path::{Path, PathBuf};

use foobar_dsp_host::{tagpipe, Worker};

fn worker_exe() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("FOO_DSP_HOST_WORKER") {
        let p = PathBuf::from(p);
        return p.is_file().then_some(p);
    }
    let p = Path::new(env!("CARGO_MANIFEST_DIR")).join(r"..\..\host\build\x64\Debug\foo_dsp_host.exe");
    p.is_file().then_some(p)
}

macro_rules! require_worker {
    () => {
        match worker_exe() {
            Some(p) => p,
            None => {
                eprintln!("skip: worker not built (run build.ps1, or set FOO_DSP_HOST_WORKER)");
                return;
            }
        }
    };
}

#[test]
fn handshake_empty_chain_session() {
    let exe = require_worker!();
    let mut w = Worker::spawn(&exe).expect("spawn + handshake");
    assert!(
        w.peer().ident.starts_with("foo_dsp_host-worker"),
        "ident: {}",
        w.peer().ident
    );
    assert_eq!(w.peer().transport_version, tagpipe::TRANSPORT_VERSION);
    assert_eq!(w.peer().vocab_version, foobar_dsp_host::VOCAB_VERSION);

    // A bogus component path is a RECOVERABLE error — ERR comes back, the worker lives.
    match w.load_single(Path::new(r"C:\does\not\exist\foo_dsp_nope.dll"), 44100, 2, 0) {
        Err(tagpipe::Error::Remote(msg)) => assert!(!msg.is_empty()),
        other => panic!("expected Remote error for bogus component, got {other:?}"),
    }

    // RST acks even with nothing instantiated.
    w.reset().expect("RST");

    // No chain configured → PROC passes audio through untouched; FLSH drains nothing.
    let block: Vec<f32> = (0..4096 * 2).map(|i| ((i as f32) * 0.011).sin() * 0.3).collect();
    let out = w.process(&block).expect("PROC").to_vec();
    assert_eq!(out, block, "empty chain must be a pass-through");
    let tail = w.drain().expect("FLSH").to_vec();
    assert!(tail.is_empty(), "empty chain has no tail, got {} samples", tail.len());

    // Caller-buffer variant agrees.
    let mut own = Vec::new();
    let frames = w.process_into(&block, &mut own).expect("process_into");
    assert_eq!(frames as usize, block.len() / 2);
    assert_eq!(own, block);

    let status = w.quit().expect("quit");
    assert!(status.success(), "worker exit: {status:?}");
}

/// Audio through a REAL component: the repo's own foo_dsp_ref (a deterministic ×0.5
/// gain — exactly representable in f32, so the assertion is bit-exact). Proves
/// component loading, chain building, preset round-trip shape, and processing — with
/// zero third-party DLLs (CI builds the fixture from the bundled BSD SDK).
#[test]
fn ref_component_chain_applies_exact_gain() {
    let exe = require_worker!();
    let ref_dll = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join(r"..\..\host\ref_dsp\build\x64\Debug\foo_dsp_ref.dll");
    if !ref_dll.is_file() {
        eprintln!("skip: foo_dsp_ref not built (run build.ps1)");
        return;
    }

    let mut w = Worker::spawn(&exe).expect("spawn + handshake");
    // (Entry enumeration order is an SDK registration detail — both ref entries are a
    // ×0.5 gain, so every audio assertion below is order-independent.)
    let names = w
        .build_chain(44100, 2, &[foobar_dsp_host::StageSpec::new(&ref_dll)])
        .expect("CHAN ref");
    assert_eq!(names.len(), 1);
    assert!(names[0].starts_with("Reference Gain"), "name: {}", names[0]);

    let block: Vec<f32> = (0..4096 * 2).map(|i| ((i as f32) * 0.007).sin() * 0.8).collect();
    let out = w.process(&block).expect("PROC").to_vec();
    let expected: Vec<f32> = block.iter().map(|&s| s * 0.5).collect();
    assert_eq!(out, expected, "x0.5 gain must be bit-exact");
    let tail = w.drain().expect("FLSH");
    assert!(tail.is_empty(), "gain DSP has no look-ahead tail");

    // Re-CHAN on a live worker requires RST before the next PROC (dsp_manager
    // re-instantiates lazily; the lab always resets before re-processing — see
    // PROTOCOL.md). Two DISTINCT stages (the component's two entries) compound to x0.25.
    let names = w
        .build_chain(
            44100,
            2,
            &[
                foobar_dsp_host::StageSpec::new(&ref_dll),
                foobar_dsp_host::StageSpec { entry_index: 1, ..foobar_dsp_host::StageSpec::new(&ref_dll) },
            ],
        )
        .expect("CHAN ref A+B");
    let mut sorted = names.clone();
    sorted.sort();
    assert_eq!(sorted, vec!["Reference Gain (x0.5)", "Reference Gain B (x0.5)"]);
    w.reset().expect("RST after re-CHAN");
    let out = w.process(&block).expect("PROC A+B").to_vec();
    let expected: Vec<f32> = block.iter().map(|&s| s * 0.25).collect();
    assert_eq!(out, expected, "two distinct stages must compound to x0.25");

    // Two copies of the SAME entry also compound (after RST).
    w.build_chain(
        44100,
        2,
        &[
            foobar_dsp_host::StageSpec::new(&ref_dll),
            foobar_dsp_host::StageSpec::new(&ref_dll),
        ],
    )
    .expect("CHAN ref x2");
    w.reset().expect("RST after re-CHAN x2");
    let out = w.process(&block).expect("PROC x2").to_vec();
    assert_eq!(out, expected, "two identical stages must also compound to x0.25");

    // The preset mechanism round-trips (the ref DSP's preset is owner-GUID + empty data).
    let blob = w.get_preset(0).expect("GPRE");
    assert_eq!(blob.len(), 16, "GUID16 + empty data");
    w.set_preset(0, &blob).expect("SPRE");

    let status = w.quit().expect("quit");
    assert!(status.success(), "worker exit: {status:?}");
}

#[test]
fn non_vers_first_message_gets_err_and_exit_2() {
    let exe = require_worker!();
    // Raw transport: send RST before VERS — the worker must ERR and exit 2.
    let mut p = tagpipe::WorkerProcess::spawn(
        &exe,
        ["--worker"],
        exe.parent(),
        tagpipe::StderrMode::Inherit,
    )
    .expect("spawn");
    tagpipe::frame::write_tag(&mut p.stdin, *b"RST ").expect("write");
    {
        use std::io::Write;
        p.stdin.flush().expect("flush");
    }
    match tagpipe::expect_reply(&mut p.stdout, *b"OK  ") {
        Err(tagpipe::Error::Remote(msg)) => assert!(msg.contains("VERS"), "msg: {msg}"),
        other => panic!("expected Remote(ERR), got {other:?}"),
    }
    let status = p.wait().expect("wait");
    assert_eq!(status.code(), Some(2), "handshake violation exit code");
}

#[test]
fn vocab_mismatch_detected_by_parent() {
    let exe = require_worker!();
    let mut p = tagpipe::WorkerProcess::spawn(
        &exe,
        ["--worker"],
        exe.parent(),
        tagpipe::StderrMode::Inherit,
    )
    .expect("spawn");
    match tagpipe::host_handshake(&mut p.stdin, &mut p.stdout, 9999) {
        Err(tagpipe::Error::Handshake(msg)) => {
            assert!(msg.contains("foo_dsp_host-worker"), "msg: {msg}")
        }
        other => panic!("expected Handshake error, got {other:?}"),
    }
    p.kill().expect("kill");
}
