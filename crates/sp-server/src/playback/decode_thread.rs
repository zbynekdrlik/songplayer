//! The one way a video DECODE thread is started (#223 S0).
//!
//! Two threads decode through `sp_decoder::MediaFoundationVideoReader`: the
//! paced decode producer (`pipeline_paced.rs`, `paced-decode-<pid>`) and the
//! decode bench (`diag::decode_bench`, `decode-bench`). The bench's numbers
//! mean what playback sees only if both threads are scheduled alike, so both
//! start here.
//!
//! The scheduling today is the platform default: `CreateThread` starts a
//! thread at `THREAD_PRIORITY_NORMAL`, inside SongPlayer's
//! `HIGH_PRIORITY_CLASS` (#203, `process_start::set_high_priority_class`).
//! The decode thread is deliberately NOT raised. Only the grid-paced emit,
//! NDI input and VBAN threads are
//! (`pipeline::pipeline_audio::raise_thread_priority`, `mmcss`). A future
//! change to the decode thread's priority belongs in [`spawn_decode_thread`],
//! so the bench follows it.

use std::thread::JoinHandle;

/// Start a decode thread named `name`, scheduled as every decode thread is
/// (see the module doc). The caller keeps COM's STA rule: the thread that
/// opens a reader decodes and drops it.
pub fn spawn_decode_thread<F, T>(name: String, f: F) -> std::io::Result<JoinHandle<T>>
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    std::thread::Builder::new().name(name).spawn(f)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_thread_runs_the_body_under_its_name() {
        let handle = spawn_decode_thread("decode-bench".to_string(), || {
            std::thread::current().name().map(str::to_owned)
        })
        .expect("spawn");
        assert_eq!(handle.join().unwrap().as_deref(), Some("decode-bench"));
    }
}
