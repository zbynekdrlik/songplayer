//! The Windows half of the decode bench (#223 S0): the bench thread's body.
//!
//! It opens the sample with `sp_decoder::MediaFoundationVideoReader` on this
//! thread (COM's STA rule: the thread that opens a reader decodes and drops
//! it), reads the stream's facts, runs [`measure`] unpaced, and logs one line
//! at the start and one at the end (a WARN when the decoder failed). The
//! decisions are in `decode_bench.rs`, Linux-tested; this is the glue to the
//! real reader, covered by the Windows job's router test on the decoder's
//! fixture (`api/diag_tests.rs`).

use std::path::Path;
use std::time::{Duration, Instant};

use sp_decoder::{MediaFoundationVideoReader, VideoStream};
use tracing::{info, warn};

use super::decode_bench::{BenchReport, StreamFacts, measure};

/// Decode `path` for at most `max_wall` and report what one picture cost.
/// A decoder error ends the run and is in the report; nothing here panics on
/// one.
#[cfg_attr(test, mutants::skip)]
pub(crate) fn bench_file(path: &Path, file: &str, max_wall: Duration) -> BenchReport {
    let thread_priority = Some(current_thread_priority());
    let bytes = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    let opening = Instant::now();
    let opened = MediaFoundationVideoReader::open(path);
    let open_ms = u64::try_from(opening.elapsed().as_millis()).unwrap_or(u64::MAX);
    let mut reader = match opened {
        Ok(reader) => reader,
        Err(e) => {
            warn!(file, bytes, open_ms, error = %e, "decode-bench: the file did not open");
            return BenchReport::open_failed(file, format!("open: {e}"), open_ms, thread_priority);
        }
    };
    let facts = StreamFacts {
        width: reader.width(),
        height: reader.height(),
        // The reader plays an unknown rate at 29.97; the gate must not judge it.
        frame_rate: if reader.frame_rate_known() {
            Some(reader.frame_rate())
        } else {
            None
        },
        codec: reader.codec(),
    };
    info!(
        file,
        bytes,
        width = facts.width,
        height = facts.height,
        codec = facts.codec.as_deref().unwrap_or("?"),
        frame_rate = ?facts.frame_rate,
        open_ms,
        max_wall_s = max_wall.as_secs(),
        thread_priority = thread_priority.unwrap_or_default(),
        "decode-bench: start"
    );
    let start = Instant::now();
    let run = measure(&mut reader, max_wall, || start.elapsed());
    // The reader is dropped on the thread that opened it (COM STA).
    drop(reader);
    let report = BenchReport::from_run(file, facts, run, open_ms, thread_priority);
    if report.error.is_some() {
        warn!(
            "decode-bench: a decoder error ended the run: {}",
            report.summary()
        );
    } else {
        info!("decode-bench: done: {}", report.summary());
    }
    report
}

/// The calling thread's priority, relative to SongPlayer's priority class
/// (`THREAD_PRIORITY_NORMAL` = 0).
#[cfg_attr(test, mutants::skip)]
fn current_thread_priority() -> i32 {
    use windows_sys::Win32::System::Threading::{GetCurrentThread, GetThreadPriority};
    // SAFETY: GetCurrentThread returns the calling thread's pseudo-handle,
    // and GetThreadPriority only reads that thread's priority.
    unsafe { GetThreadPriority(GetCurrentThread()) }
}
