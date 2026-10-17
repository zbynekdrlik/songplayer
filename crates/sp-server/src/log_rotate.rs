//! #261 (no infinite archive): a log SongPlayer writes on its own stops
//! growing at [`ROTATE_AT_BYTES`]. The file is renamed to `<name>.1` (an
//! older `.1` is replaced) and a fresh one is started, so each such log
//! keeps at most two files of about 10 MiB:
//!
//! - the panic hook's crash records, `songplayer-panic.log` and
//!   `sp-audio-host-panic.log` (`panic_hook`: [`rotate_if_over`] when the
//!   hook is installed and before each record);
//! - the ASIO host's own log, `sp-audio-host.log` (a [`RotatingFile`],
//!   checked when it is opened and before each write, `audio_host/host_win.rs`).
//!
//! The daily logs (`songplayer.<date>.log`) keep their own bound of 14
//! files (`src-tauri/src/lib.rs`). A rotation that fails (the `.1` name
//! taken by a directory, a file held open elsewhere) never loses a line: the
//! log goes on in the same file and a later write tries again (once the
//! file grew by another sixteenth of the limit, never on every line).

use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

/// A log rotates once it holds this much: 10 MiB.
pub const ROTATE_AT_BYTES: u64 = 10_485_760;

/// The name a log rotates to: `<name>.1`.
pub fn rotated_path(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(".1");
    PathBuf::from(name)
}

/// Rename `path` to [`rotated_path`] when it holds `limit` bytes or more
/// (an older `.1` is replaced). `Ok(true)` when it rotated; a log that is
/// not there yet is `Ok(false)`.
pub fn rotate_if_over(path: &Path, limit: u64) -> io::Result<bool> {
    let len = match std::fs::metadata(path) {
        Ok(meta) => meta.len(),
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e),
    };
    if len < limit {
        return Ok(false);
    }
    std::fs::rename(path, rotated_path(path))?;
    Ok(true)
}

/// A log file that rotates as it grows: opened for appending (after a
/// rotation when it already holds `limit` bytes), and rotated before a write
/// that finds `limit` bytes in it. A write is never split across the two
/// files, so one formatted line stays whole.
pub struct RotatingFile {
    path: PathBuf,
    limit: u64,
    /// `None` only after a reopen failed; the next write opens it again.
    file: Option<File>,
    /// Bytes in the current file.
    written: u64,
    /// After a rotation that failed, the next try waits until the file
    /// holds this much (another sixteenth of the limit), never every line;
    /// every (re)open starts it afresh with the file's own count.
    retry_at: u64,
}

impl RotatingFile {
    /// `path` for appending, rotated first when it already holds `limit`
    /// bytes (a failed rotation appends to it as it is).
    pub fn open(path: &Path, limit: u64) -> io::Result<Self> {
        let _ = rotate_if_over(path, limit);
        let mut log = Self {
            path: path.to_path_buf(),
            limit,
            file: None,
            written: 0,
            retry_at: 0,
        };
        log.reopen()?;
        Ok(log)
    }

    /// (Re)open the file for appending and count what it holds.
    fn reopen(&mut self) -> io::Result<()> {
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        self.written = file.metadata()?.len();
        self.retry_at = 0;
        self.file = Some(file);
        Ok(())
    }

    /// Close the file, rename it to `.1` and start afresh; whatever the
    /// rename did, the log is open again after it (the same file when the
    /// rename failed).
    fn rotate(&mut self) -> io::Result<()> {
        self.file = None; // closed before its rename (Windows)
        let renamed = std::fs::rename(&self.path, rotated_path(&self.path));
        self.reopen()?;
        renamed
    }
}

impl Write for RotatingFile {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if self.file.is_none() {
            self.reopen()?;
        } else if self.written >= self.limit && self.written >= self.retry_at {
            self.retry_at = match self.rotate() {
                Ok(()) => 0,
                Err(_) => self.written.saturating_add(self.limit / 16),
            };
        }
        let file = self
            .file
            .as_mut()
            .ok_or_else(|| io::Error::other("the log file is not open"))?;
        let n = file.write(buf)?;
        self.written += n as u64;
        Ok(n)
    }

    // std's `File::flush` does nothing (its writes are unbuffered), so any
    // body here is equivalent: no mutant of it could be told apart.
    #[cfg_attr(test, mutants::skip)]
    fn flush(&mut self) -> io::Result<()> {
        self.file.as_mut().map_or(Ok(()), Write::flush)
    }
}

#[cfg(test)]
#[path = "log_rotate_tests.rs"]
mod tests;
