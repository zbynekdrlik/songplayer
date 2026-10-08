//! A log capture for tests: a `tracing_subscriber` fmt layer writing into a
//! buffer, installed for one test only (`tracing::subscriber::with_default`,
//! or `set_default` in an async test on the current-thread runtime), so a
//! test reads exactly the lines its own code logged.

use std::sync::{Arc, Mutex};

/// The lines a scoped subscriber wrote.
#[derive(Clone, Default)]
pub(crate) struct Captured(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for Captured {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Captured {
    /// Everything written so far.
    pub(crate) fn text(&self) -> String {
        String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
    }

    /// The lines written so far that contain `needle` (a level such as
    /// `" WARN "` / `"DEBUG"`, or a message).
    pub(crate) fn lines_with(&self, needle: &str) -> Vec<String> {
        self.text()
            .lines()
            .filter(|l| l.contains(needle))
            .map(str::to_string)
            .collect()
    }
}

/// A subscriber writing every event at DEBUG and above into `cap`, with no
/// colours.
pub(crate) fn capturing(cap: &Captured) -> impl tracing::Subscriber + Send + Sync + 'static {
    let writer = cap.clone();
    tracing_subscriber::fmt()
        .with_writer(move || writer.clone())
        .with_ansi(false)
        .with_max_level(tracing::Level::DEBUG)
        .finish()
}
