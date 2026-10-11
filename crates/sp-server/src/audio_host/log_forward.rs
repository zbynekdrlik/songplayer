//! #233: the ASIO host's log lines, forwarded to SongPlayer's log.
//!
//! The host formats every tracing event twice (`host_win.rs`): into its own
//! file (`sp-audio-host.log`, kept when the pipe is down) and through
//! [`PipeLogs`], a `MakeWriter` whose writer collects ONE formatted event
//! and, when dropped, queues it as a [`Report::Log`] with the event's level
//! (never waits: a full report queue drops the line; the file has it). The
//! writer task sends it to SongPlayer, which re-logs it at that level under
//! the target `sp_audio_host` (`client.rs`).

use std::io;

use tokio::sync::mpsc;
use tracing::{Level, Metadata};
use tracing_subscriber::fmt::MakeWriter;

use crate::audio_host::protocol::{HostLevel, Report, log_line};

/// A tracing level as the pipe carries it.
pub fn host_level(level: &Level) -> HostLevel {
    if *level == Level::ERROR {
        HostLevel::Error
    } else if *level == Level::WARN {
        HostLevel::Warn
    } else if *level == Level::INFO {
        HostLevel::Info
    } else if *level == Level::DEBUG {
        HostLevel::Debug
    } else {
        HostLevel::Trace
    }
}

/// The host's log layer's writer factory (module doc).
#[derive(Clone)]
pub struct PipeLogs {
    tx: mpsc::Sender<Report>,
}

impl PipeLogs {
    pub fn new(tx: mpsc::Sender<Report>) -> Self {
        Self { tx }
    }
}

/// One formatted event on its way to the pipe.
pub struct PipeLine {
    tx: mpsc::Sender<Report>,
    level: HostLevel,
    buf: Vec<u8>,
}

impl io::Write for PipeLine {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.buf.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Drop for PipeLine {
    fn drop(&mut self) {
        let line = log_line(&self.buf);
        if !line.is_empty() {
            let _ = self.tx.try_send(Report::Log {
                level: self.level,
                line,
            });
        }
    }
}

impl<'a> MakeWriter<'a> for PipeLogs {
    type Writer = PipeLine;

    fn make_writer(&'a self) -> PipeLine {
        self.line(HostLevel::Info)
    }

    fn make_writer_for(&'a self, meta: &Metadata<'_>) -> PipeLine {
        self.line(host_level(meta.level()))
    }
}

impl PipeLogs {
    /// An empty line at `level` (`PipeLine` has a `Drop`: no struct update).
    fn line(&self, level: HostLevel) -> PipeLine {
        PipeLine {
            tx: self.tx.clone(),
            level,
            buf: Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use super::*;

    #[test]
    fn every_level_keeps_its_name_on_the_pipe() {
        let table = [
            (Level::ERROR, HostLevel::Error),
            (Level::WARN, HostLevel::Warn),
            (Level::INFO, HostLevel::Info),
            (Level::DEBUG, HostLevel::Debug),
            (Level::TRACE, HostLevel::Trace),
        ];
        for (level, want) in table {
            assert_eq!(host_level(&level), want, "{level}");
        }
    }

    /// One event is one report, at its level, its line break gone; an empty
    /// event sends nothing.
    #[test]
    fn a_formatted_event_is_one_log_report_when_its_writer_drops() {
        let (tx, mut rx) = mpsc::channel(4);
        let logs = PipeLogs::new(tx);
        let mut w = logs.make_writer();
        w.write_all(b" WARN asio_out: a resync").unwrap();
        w.write_all(b" id=out-2\n").unwrap();
        w.flush().unwrap();
        assert!(rx.try_recv().is_err(), "nothing before the event ends");
        drop(w);
        assert_eq!(
            rx.try_recv().unwrap(),
            Report::Log {
                level: HostLevel::Info,
                line: " WARN asio_out: a resync id=out-2".into(),
            }
        );
        drop(logs.make_writer());
        assert!(rx.try_recv().is_err(), "an empty event sends nothing");
    }

    /// Through a real fmt layer: the event's level goes with its line.
    #[test]
    fn an_events_level_goes_with_its_line() {
        // The process-wide no-op default every scoped capture needs
        // (`test_log::capturing`); this test's callsite is its own.
        let _ = crate::test_log::capturing(&crate::test_log::Captured::default());
        let (tx, mut rx) = mpsc::channel(8);
        let subscriber = tracing_subscriber::fmt()
            .with_writer(PipeLogs::new(tx))
            .with_ansi(false)
            .without_time()
            .with_max_level(Level::TRACE)
            .finish();
        tracing::subscriber::with_default(subscriber, || {
            tracing::warn!(driver = "SG", "asio host: a forwarded line");
            tracing::debug!("asio host: a quiet line");
        });
        let Report::Log { level, line } = rx.try_recv().unwrap() else {
            panic!("not a log report");
        };
        assert_eq!(level, HostLevel::Warn);
        assert!(line.contains("asio host: a forwarded line"), "{line}");
        assert!(line.contains("SG"), "{line}");
        let Report::Log { level, .. } = rx.try_recv().unwrap() else {
            panic!("not a log report");
        };
        assert_eq!(level, HostLevel::Debug);
    }

    /// A full report queue drops the line and never waits.
    #[test]
    fn a_full_queue_drops_the_line() {
        let (tx, mut rx) = mpsc::channel(1);
        let logs = PipeLogs::new(tx);
        for line in [&b"one"[..], b"two"] {
            let mut w = logs.make_writer();
            w.write_all(line).unwrap();
        }
        assert_eq!(
            rx.try_recv().unwrap(),
            Report::Log {
                level: HostLevel::Info,
                line: "one".into(),
            }
        );
        assert!(rx.try_recv().is_err());
    }
}
