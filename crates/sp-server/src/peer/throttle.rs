//! #229: reads and transfers at a bounded rate. The serving node's uplink
//! also carries the live stream, and its disk also feeds the wall.

use std::time::Duration;

/// The longest single pause. Never reached at the rates in use (a 64 KiB
/// served chunk at the 1 Mbit/s minimum cap = 0.52 s, a 1 MiB hashed read at
/// 40 MiB/s = 25 ms); it bounds a stall or a wrong rate, so a transfer or a
/// hash never hangs.
pub const MAX_PAUSE: Duration = Duration::from_secs(2);

/// How long to pause after `done` bytes in `elapsed` so that the average
/// stays at or under `rate` bytes/s, at most [`MAX_PAUSE`] at a time (the
/// next pause takes the rest). `rate` 0 = no limit.
pub fn wait_for(done: u64, elapsed: Duration, rate: u64) -> Duration {
    if rate == 0 {
        return Duration::ZERO;
    }
    let due_us = u128::from(done) * 1_000_000 / u128::from(rate);
    let due = Duration::from_micros(u64::try_from(due_us).unwrap_or(u64::MAX));
    due.saturating_sub(elapsed).min(MAX_PAUSE)
}

/// Mbit/s as bytes/s.
pub fn mbps_to_bytes(mbps: u32) -> u64 {
    u64::from(mbps) * 125_000
}

/// `body`, sent at no more than `rate` bytes/s (0 = as it comes).
pub fn throttled(body: axum::body::Body, rate: u64) -> axum::body::Body {
    use futures::StreamExt;
    if rate == 0 {
        return body;
    }
    let started = tokio::time::Instant::now();
    let chunks = futures::stream::unfold(
        (body.into_data_stream(), 0u64),
        move |(mut stream, sent)| async move {
            let chunk = stream.next().await?;
            let sent = sent + chunk.as_ref().map_or(0, |b| b.len() as u64);
            tokio::time::sleep(wait_for(sent, started.elapsed(), rate)).await;
            Some((chunk, (stream, sent)))
        },
    );
    axum::body::Body::from_stream(chunks)
}

#[cfg(test)]
#[path = "throttle_tests.rs"]
mod tests;
