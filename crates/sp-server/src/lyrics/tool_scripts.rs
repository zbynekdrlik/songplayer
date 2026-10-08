//! The Python scripts the lyrics worker ships into `tools_dir` (#207, #233
//! release review).

/// `lyrics_worker.py` plus the module it imports at load: `audio_window.py`,
/// the window read and the streamed overlap-add it shares with the stem
/// worker. Embedded at compile time; written by `LyricsWorker::ensure_script`
/// on every start, so a deploy never runs the new script against a stale or
/// missing helper. Pure.
pub(super) fn embedded_tool_scripts() -> [(&'static str, &'static str); 2] {
    [
        (
            "lyrics_worker.py",
            include_str!("../../../../scripts/lyrics_worker.py"),
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
    fn the_lyrics_worker_ships_with_the_audio_window_module_it_imports() {
        let [(worker_name, worker), (helper_name, helper)] = embedded_tool_scripts();
        assert_eq!(
            (worker_name, helper_name),
            ("lyrics_worker.py", "audio_window.py")
        );
        assert!(
            worker.contains("import audio_window as aw"),
            "lyrics_worker.py does not import the shipped audio_window module"
        );
        assert!(
            helper.contains("class OverlapAdd"),
            "audio_window.py (the streamed overlap-add) is not shipped"
        );
    }
}
