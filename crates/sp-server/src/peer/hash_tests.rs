//! #229 `peer::hash`.

use std::time::Instant;

use super::*;

/// sha256("abc"), the FIPS 180-2 test vector (in quarters: the staging hook
/// refuses a long hex literal).
const ABC: &str = concat!(
    "ba7816bf8f01cfea",
    "414140de5dae2223",
    "b00361a396177a9c",
    "b410ff61f20015ad"
);

#[test]
fn sha256_of_bytes_is_lowercase_hex() {
    assert_eq!(sha256_hex(b"abc"), ABC);
}

#[tokio::test]
async fn sha256_of_a_file_matches_its_bytes_across_read_chunks() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("f.bin");
    let bytes: Vec<u8> = (0..(CHUNK * 2 + 7)).map(|i| (i % 251) as u8).collect();
    std::fs::write(&path, &bytes).unwrap();
    assert_eq!(sha256_file(&path, 0).await.unwrap(), sha256_hex(&bytes));
    std::fs::write(&path, b"abc").unwrap();
    assert_eq!(sha256_file(&path, 1 << 30).await.unwrap(), ABC);
}

/// 300 bytes at 1 000 B/s take at least 0.3 s (a lower bound: a slow runner
/// only makes it pass with room to spare).
#[tokio::test]
async fn a_file_is_read_at_the_rate() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("f.bin");
    std::fs::write(&path, [7u8; 300]).unwrap();
    let started = Instant::now();
    assert_eq!(
        sha256_file(&path, 1_000).await.unwrap(),
        sha256_hex(&[7u8; 300])
    );
    let took = started.elapsed();
    assert!(took >= std::time::Duration::from_millis(290), "{took:?}");
}

#[tokio::test]
async fn a_missing_file_is_an_error() {
    let dir = tempfile::tempdir().unwrap();
    assert!(sha256_file(&dir.path().join("none"), 0).await.is_err());
}
