//! #233 (review): the box's one monotonic counter, in 100 ns — the clock
//! SongPlayer and the ASIO host both read, so an [`Anchor`] SongPlayer takes
//! means the same instant in the host (`remote_clock.rs`).
//!
//! Windows: QueryPerformanceCounter, one counter for every process on the
//! box. Elsewhere (the tests, where both sides run in one process): this
//! process's own monotonic clock from its first read.
//!
//! [`Anchor`]: crate::audio_host::remote_clock::Anchor

/// The counter now, 100 ns.
#[cfg_attr(test, mutants::skip)] // a clock read; what reads it is tested with an injected counter (remote_clock_tests.rs)
pub fn mono_100ns() -> i64 {
    counter_100ns()
}

#[cfg(windows)]
#[cfg_attr(test, mutants::skip)] // two kernel calls; dead on the Linux runner
fn counter_100ns() -> i64 {
    use windows_sys::Win32::System::Performance::{
        QueryPerformanceCounter, QueryPerformanceFrequency,
    };
    let (mut count, mut freq) = (0i64, 0i64);
    // SAFETY: both write one i64 through a valid pointer; they cannot fail
    // on Windows XP or later.
    unsafe {
        QueryPerformanceCounter(&mut count);
        QueryPerformanceFrequency(&mut freq);
    }
    (i128::from(count) * 10_000_000 / i128::from(freq.max(1))) as i64
}

#[cfg(not(windows))]
#[cfg_attr(test, mutants::skip)] // a clock read; what reads it is tested with an injected counter
fn counter_100ns() -> i64 {
    use std::sync::OnceLock;
    use std::time::Instant;
    static ORIGIN: OnceLock<Instant> = OnceLock::new();
    let elapsed = ORIGIN.get_or_init(Instant::now).elapsed();
    i64::try_from(elapsed.as_nanos() / 100).unwrap_or(i64::MAX)
}
