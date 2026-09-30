//! #136: a stem / dub job's input, re-read AFTER the job holds the heavy slot.
//!
//! A job is picked, then queues for the one heavy slot behind a running
//! separation, dub or lyrics step, for minutes. The metadata repair can rename
//! the song meanwhile (`downloader::cache::rename_song_files`: the audio and
//! the stems named after it move to the new name). A job that kept the paths
//! it read at pick time then opened a file that no longer existed, and took a
//! penalised failure (`record_stem_deferral` / `record_dub_deferral`).

#[cfg(test)]
#[path = "song_input_tests.rs"]
mod tests;
