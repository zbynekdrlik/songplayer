//! A log capture for tests: a `tracing_subscriber` fmt layer writing into a
//! buffer, installed for one test only (`tracing::subscriber::with_default`,
//! or `set_default` in an async test on the current-thread runtime), so a
//! test reads exactly the lines its own code logged. Every capture in this
//! crate goes through [`capturing`], which also installs the process-wide
//! no-op default the captures need (see there).

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
///
/// Review round 14: tracing caches each callsite's interest the first time
/// the callsite is hit. While a scoped capture is the ONLY registered
/// dispatcher, tracing-core asks only the CALLING thread's default for that
/// interest (`Dispatchers::has_just_one`, tracing-core 0.1.36
/// `callsite.rs`); a callsite first hit on another test's thread (no
/// default there) is cached as `never`, and the capture then misses its own
/// line on correct code. A global no-op default, installed once before the
/// first capture, keeps a second dispatcher registered: each callsite is
/// asked of every dispatcher (`sometimes`), registering a capture rebuilds
/// every cached interest, and each event asks the capturing thread's own
/// default.
pub(crate) fn capturing(cap: &Captured) -> impl tracing::Subscriber + Send + Sync + 'static {
    static GLOBAL_NO_OP: std::sync::Once = std::sync::Once::new();
    GLOBAL_NO_OP.call_once(|| {
        // An error = a global default is set already: a second dispatcher
        // is registered either way.
        let no_op = tracing::subscriber::NoSubscriber::new();
        let _ = tracing::subscriber::set_global_default(no_op);
    });
    let writer = cap.clone();
    tracing_subscriber::fmt()
        .with_writer(move || writer.clone())
        .with_ansi(false)
        .with_max_level(tracing::Level::DEBUG)
        .finish()
}
