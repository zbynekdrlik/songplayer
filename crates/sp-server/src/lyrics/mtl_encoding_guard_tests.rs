//! Structural guards (#137) that the two halves of the mtl-aligner UTF-8
//! defence stay in place:
//!
//!   1. the PRODUCTION spawn (`mtl_aligner.rs`) keeps `PYTHONUTF8=1` on the
//!      run.py child, and
//!   2. the DIRECT/eval invocation is covered by an `open()` UTF-8 shim
//!      inside `run.py::install_compat_shims()`.
//!
//! Wired from `mtl_aligner.rs` via `#[path] #[cfg(test)] mod
//! mtl_encoding_guard_tests;`, mirroring the `bootstrap_tests_numpy_pin.rs`
//! source-text guard pattern. `include_str!` resolves relative to THIS
//! file (`crates/sp-server/src/lyrics/`); CRLF-normalised for the Windows
//! CI checkout.

/// The production path must keep forcing UTF-8 on the run.py subprocess —
/// this is what protects live alignment from cp1252 crashes on non-ASCII
/// reference text. Losing this line silently regresses #137.
#[test]
fn mtl_aligner_keeps_pythonutf8_env() {
    let src = include_str!("mtl_aligner.rs").replace("\r\n", "\n");
    assert!(
        src.contains("cmd.env(\"PYTHONUTF8\", \"1\")"),
        "mtl_aligner.rs must keep `cmd.env(\"PYTHONUTF8\", \"1\")` before the run.py spawn (#137)"
    );
}

/// The direct/eval `python run.py` path has no PYTHONUTF8 env, so the
/// caller-independent belt-and-braces fix is an `open()` shim that defaults
/// text-mode reads to UTF-8 inside `install_compat_shims()`. Upstream
/// `wrapper.preprocess_lyrics()` opens the reference file with a bare
/// `open()`, which on Windows falls back to cp1252 and crashes on smart
/// quotes / em-dashes.
#[test]
fn run_py_defaults_open_to_utf8_inside_compat_shims() {
    let src = include_str!("../../../../eval/lyrics/aligners/lyrics_alignment_mtl/run.py")
        .replace("\r\n", "\n");
    let shims_pos = src
        .find("def install_compat_shims")
        .expect("run.py must define install_compat_shims()");
    let shim_pos = src.find("builtins.open").expect(
        "install_compat_shims() must monkeypatch builtins.open to default text mode to UTF-8 (#137)",
    );
    let return_pos = src
        .find("return wrapper")
        .expect("install_compat_shims() returns the wrapper module");
    assert!(
        shims_pos < shim_pos && shim_pos < return_pos,
        "the builtins.open UTF-8 shim must live inside install_compat_shims() (#137)"
    );
    assert!(
        src.contains("encoding=\"utf-8\""),
        "the open() shim must default to encoding=\"utf-8\" (#137)"
    );
}
