//! The Python tool scripts the stem worker ships into `tools_dir` (#207).

/// `stem_worker.py` plus the module it imports at load: `win_replace.py`, the
/// POSIX-semantics rename that publishes a stem sidecar while SongPlayer holds
/// the old one open (`os.replace` fails there with WinError 5 on Windows).
/// Embedded at compile time; written by `StemWorker::ensure_script` through
/// [`crate::embedded_scripts::materialise`]. Pure.
pub(super) fn embedded_tool_scripts() -> [(&'static str, &'static str); 2] {
    [
        (
            "stem_worker.py",
            include_str!("../../../../scripts/stem_worker.py"),
        ),
        (
            "win_replace.py",
            include_str!("../../../../scripts/win_replace.py"),
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_stem_worker_ships_with_the_posix_rename_it_imports() {
        let [(worker_name, worker), (helper_name, helper)] = embedded_tool_scripts();
        assert_eq!(
            (worker_name, helper_name),
            ("stem_worker.py", "win_replace.py")
        );
        assert!(
            worker.contains("import win_replace as wr"),
            "stem_worker.py does not import the shipped win_replace module"
        );
        assert!(
            helper.contains("FILE_RENAME_FLAG_POSIX_SEMANTICS"),
            "win_replace.py (the POSIX rename) is not shipped"
        );
    }
}
