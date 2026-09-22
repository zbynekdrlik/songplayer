//! #168 — materialise an APP-OWNED venv interpreter and inject the mimalloc
//! override into it.
//!
//! On the box, `lyrics_venv\Scripts\python.exe` is CPython's venv REDIRECTOR
//! (`venvlauncher.exe`); the interpreter that actually runs torch is its child,
//! the SYSTEM `C:\Program Files\Python312\python.exe`. An allocator override
//! must live in a binary the app OWNS — never the shared system exe — so this
//! module replaces the redirector with a real, copies-layout interpreter INSIDE
//! the venv (CPython's `getpath` honours `pyvenv.cfg`'s `home` for a real
//! interpreter placed in the venv dir — the pre-3.7.2 "copies" layout, still
//! supported, so `pip.exe` / `is_ready` / every spawn keep working), then runs
//! mimalloc's `minject --inplace` on that copy.
//!
//! The pure decisions ([`needs_recopy`], [`inject_plan`]) are unit-tested on
//! Linux; the file copies and the `minject` invocation are the Windows-only
//! integration seam, called from `bootstrap.rs` after `is_ready`. Every failure
//! is WARN-and-continue — a missing DLL or a failed inject never blocks the
//! bootstrap; the child just runs unretained, exactly as before #168.

use std::time::SystemTime;

/// A file's size + modification time — the identity used to decide whether a
/// materialised copy is still current with its source.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FileStamp {
    pub len: u64,
    pub mtime: SystemTime,
}

/// Whether to run `minject` on the venv interpreter.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InjectPlan {
    /// Do not inject — the reason (no resources staged, or already injected).
    Skip(&'static str),
    /// Run `minject --inplace` to make the mimalloc override the first import.
    Inject,
}

/// Pure: does `dst` need to be (re)copied from `src`? True when `dst` is missing
/// (never materialised), or differs from `src` in length or mtime (a system
/// Python patch update re-materialises the copy on the next boot).
pub fn needs_recopy(src: FileStamp, dst: Option<FileStamp>) -> bool {
    match dst {
        None => true,
        Some(d) => d.len != src.len && d.mtime != src.mtime,
    }
}

/// Pure: whether to `minject` the mimalloc override into the venv interpreter.
/// Skip when the staged resources are missing (nothing to inject) or when the
/// override is already the first import (idempotent — `minject -l` decides);
/// otherwise inject.
pub fn inject_plan(resources_present: bool, already_injected: bool) -> InjectPlan {
    if !resources_present {
        InjectPlan::Skip("mimalloc resources not staged")
    } else if !already_injected {
        InjectPlan::Skip("mimalloc.dll already the first import")
    } else {
        InjectPlan::Inject
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn stamp(len: u64, secs: u64) -> FileStamp {
        FileStamp {
            len,
            mtime: SystemTime::UNIX_EPOCH + Duration::from_secs(secs),
        }
    }

    #[test]
    fn needs_recopy_true_when_dst_missing() {
        assert!(needs_recopy(stamp(100, 5), None));
    }

    #[test]
    fn needs_recopy_true_on_size_mismatch() {
        assert!(needs_recopy(stamp(100, 5), Some(stamp(200, 5))));
    }

    #[test]
    fn needs_recopy_true_on_mtime_mismatch() {
        assert!(needs_recopy(stamp(100, 5), Some(stamp(100, 9))));
    }

    #[test]
    fn needs_recopy_false_when_equal() {
        assert!(!needs_recopy(stamp(100, 5), Some(stamp(100, 5))));
    }

    #[test]
    fn inject_plan_skips_without_resources() {
        assert!(matches!(inject_plan(false, false), InjectPlan::Skip(_)));
        assert!(matches!(inject_plan(false, true), InjectPlan::Skip(_)));
    }

    #[test]
    fn inject_plan_skips_when_already_injected() {
        assert!(matches!(inject_plan(true, true), InjectPlan::Skip(_)));
    }

    #[test]
    fn inject_plan_injects_when_present_and_not_injected() {
        assert_eq!(inject_plan(true, false), InjectPlan::Inject);
    }
}
