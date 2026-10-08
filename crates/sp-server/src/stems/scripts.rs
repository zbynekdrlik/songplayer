//! The Python tool scripts the stem worker ships into `tools_dir` (#207).

/// `stem_worker.py` plus the modules it imports at load: `win_replace.py`, the
/// POSIX-semantics rename that publishes a stem sidecar while SongPlayer holds
/// the old one open (`os.replace` fails there with WinError 5 on Windows), and
/// `audio_window.py`, the window read and the streamed overlap-add it shares
/// with the lyrics worker (#233 release review). Embedded at compile time;
/// written by `StemWorker::ensure_script` through
/// [`crate::embedded_scripts::materialise`]. Pure.
pub(super) fn embedded_tool_scripts() -> [(&'static str, &'static str); 3] {
    [
        (
            "stem_worker.py",
            include_str!("../../../../scripts/stem_worker.py"),
        ),
        (
            "win_replace.py",
            include_str!("../../../../scripts/win_replace.py"),
        ),
        (
            "audio_window.py",
            include_str!("../../../../scripts/audio_window.py"),
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_stem_worker_ships_with_the_posix_rename_it_imports() {
        let [
            (worker_name, worker),
            (helper_name, helper),
            (window_name, window),
        ] = embedded_tool_scripts();
        assert_eq!(
            (worker_name, helper_name, window_name),
            ("stem_worker.py", "win_replace.py", "audio_window.py")
        );
        assert!(
            worker.contains("import audio_window as aw") && window.contains("class OverlapAdd"),
            "stem_worker.py and the audio_window module it imports"
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
