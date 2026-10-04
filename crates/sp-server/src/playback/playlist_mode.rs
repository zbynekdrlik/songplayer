//! #225 unit 2: a playlist's playback mode has ONE persisted truth, its
//! `playlists.playback_mode` row. The RED commit carries only the tests; the
//! engine side lands with the fix.

#[cfg(test)]
#[path = "playlist_mode_tests.rs"]
mod tests;
