//! #229: sha256 as 64 lowercase hex digits — of bytes, or of a file read off
//! the async runtime at a bounded rate.

use std::io::Read;
use std::path::Path;
use std::time::Instant;

use sha2::{Digest, Sha256};

/// One read while hashing a file.
pub(crate) const CHUNK: usize = 1 << 20;

pub fn sha256_hex(bytes: &[u8]) -> String {
    hex(&Sha256::digest(bytes))
}

fn hex(digest: &[u8]) -> String {
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

/// The sha256 of the file at `path`, read on a blocking thread at no more
/// than `max_bytes_per_s` (0 = no limit).
pub async fn sha256_file(path: &Path, max_bytes_per_s: u64) -> std::io::Result<String> {
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || {
        let mut file = std::fs::File::open(&path)?;
        let mut hasher = Sha256::new();
        let mut buf = vec![0u8; CHUNK];
        let started = Instant::now();
        let mut done: u64 = 0;
        loop {
            let n = file.read(&mut buf)?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
            done += n as u64;
            let pause = super::throttle::wait_for(done, started.elapsed(), max_bytes_per_s);
            std::thread::sleep(pause);
        }
        Ok(hex(&hasher.finalize()))
    })
    .await
    .map_err(std::io::Error::other)?
}

#[cfg(test)]
#[path = "hash_tests.rs"]
mod tests;
