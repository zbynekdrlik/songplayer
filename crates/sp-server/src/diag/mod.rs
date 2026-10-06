//! Measurement benches behind `/api/v1/diag/*` (#223 S0). The main session
//! runs them on the box to decide a design gate. Playback never calls them.

pub mod decode_bench;
#[cfg(windows)]
mod decode_bench_mf;
