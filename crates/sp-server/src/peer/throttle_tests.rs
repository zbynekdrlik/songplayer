//! #229 `peer::throttle`: the rate math of the hasher's reads and the peer
//! API's uploads.

use std::time::Duration;

use super::*;

#[test]
fn the_wait_keeps_the_average_at_the_rate() {
    let ms = Duration::from_millis;
    assert_eq!(wait_for(0, ms(0), 1_000), ms(0));
    assert_eq!(wait_for(1_000, ms(0), 1_000), ms(1_000));
    assert_eq!(wait_for(250, ms(0), 1_000), ms(250));
    assert_eq!(wait_for(1_000, ms(400), 1_000), ms(600));
    assert_eq!(wait_for(1_000, ms(1_000), 1_000), ms(0));
    assert_eq!(
        wait_for(1_000, ms(2_000), 1_000),
        ms(0),
        "behind never waits"
    );
    assert_eq!(wait_for(3, ms(0), 2), ms(1_500));
}

/// Rate 0 is no limit. A count far ahead of the rate does not overflow and
/// pauses at most 2 s at a time: a stall, or a wrong rate, never hangs a
/// transfer (or a test) for longer.
#[test]
fn rate_zero_is_no_limit_and_a_pause_is_at_most_two_seconds() {
    let ms = Duration::from_millis;
    assert_eq!(wait_for(u64::MAX, Duration::ZERO, 0), Duration::ZERO);
    assert_eq!(wait_for(u64::MAX, Duration::ZERO, 1), ms(2_000));
    assert_eq!(wait_for(2_500, ms(0), 1_000), ms(2_000), "2.5 s due");
    assert_eq!(wait_for(2_500, ms(600), 1_000), ms(1_900));
}

#[test]
fn mbit_per_second_in_bytes() {
    assert_eq!(mbps_to_bytes(1), 125_000);
    assert_eq!(mbps_to_bytes(20), 2_500_000);
    assert_eq!(mbps_to_bytes(10_000), 1_250_000_000);
}

#[tokio::test(start_paused = true)]
async fn a_throttled_body_keeps_its_bytes_and_its_rate() {
    use axum::body::{Body, Bytes};
    let chunks: Vec<Result<Bytes, std::io::Error>> = vec![
        Ok(Bytes::from(vec![1u8; 1_000])),
        Ok(Bytes::from(vec![2u8; 1_000])),
        Ok(Bytes::from(vec![3u8; 500])),
    ];
    let body = Body::from_stream(futures::stream::iter(chunks));
    let started = tokio::time::Instant::now();
    let out = axum::body::to_bytes(throttled(body, 1_000), usize::MAX)
        .await
        .unwrap();
    assert_eq!(out.len(), 2_500);
    assert_eq!((out[0], out[1_000], out[2_499]), (1, 2, 3));
    assert_eq!(
        started.elapsed(),
        Duration::from_millis(2_500),
        "2 500 bytes at 1 000 B/s"
    );
}

#[tokio::test(start_paused = true)]
async fn rate_zero_passes_the_body_as_it_comes() {
    let started = tokio::time::Instant::now();
    let body = axum::body::Body::from(vec![7u8; 4_096]);
    let out = axum::body::to_bytes(throttled(body, 0), usize::MAX)
        .await
        .unwrap();
    assert_eq!(out.len(), 4_096);
    assert_eq!(started.elapsed(), Duration::ZERO);
}
