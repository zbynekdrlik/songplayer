//! The playback pipelines at startup, and `SP-program`'s stable NDI port.
//!
//! #221 lane 3 (ROZHODNUTÉ 5877969167): a playlist pipeline has no NDI sender
//! of its own — it feeds the program bus — so creating one waits for nothing.
//! [`PlaybackEngine::create_startup_pipelines`] creates one per active
//! playlist, in `playlist.id` order, each in its row's playback mode (#225
//! unit 2).
//!
//! `SP-program` is SongPlayer's only NDI sender (`SP-program-MAX` goes out over
//! Spout), and it is created right after these pipelines, so the NDI runtime
//! hands it the base port ([`NDI_PORT_BASE`]) on every start. #196: DistroAV's
//! genlock build reconnects a stale source BY URL with the PINNED previous
//! port, so a restart must give the sender the same port again. Before the
//! sender is created, [`wait_for_program_ports`] waits (≤ 10 s) until the
//! previous instance has released the span ([`wait_for_ports_free`]), so an
//! immediate restart gets the same port.
//!
//! The pure pieces (port range, port-availability wait, creation order, the
//! row's mode) are unit-tested on Linux with no NDI runtime.

use std::time::Duration;

use sp_core::models::Playlist;
use sp_core::playback::PlaybackMode;
use tracing::{info, warn};

use super::PlaybackEngine;

/// Poll cadence for the startup port-availability wait.
const PORT_WAIT_POLL_INTERVAL: Duration = Duration::from_millis(250);
/// Max polls (≤ 10 s at 250 ms) before startup proceeds regardless (#196).
const PORT_WAIT_MAX_POLLS: u32 = 40;

/// The first port of this process's NDI runtime: `NDIlib_initialize` listens
/// on it, and the senders take the ports after it in creation order (#196,
/// #240: SongPlayer listens on 5960 + SP-program's 5961; 10 senders once
/// ended at 5970).
pub const NDI_PORT_BASE: u16 = 5960;

/// The NDI senders SongPlayer creates: `SP-program` alone (#221 lane 3).
pub const NDI_SENDERS: usize = 1;

/// The port range to probe-bind before creating the first sender: the
/// senders' own ports, `base+1..=base+N`, so an immediate restart waits until
/// the previous instance released them. Not the base (#240): this process's
/// own runtime already holds it — `NDIlib_initialize` runs when the engine
/// starts, before the wait — so it is never free to probe (SNV 10.10.2026:
/// `busy=[5960]`, 5961 free). Not a "margin" past them either: the next ports
/// belong to the box's other NDI processes (cg OBS's runtime + sender on
/// 5962 + 5963), which never free them, so a span naming one waited the full
/// bound at every start.
pub fn ndi_port_range(n_outputs: usize) -> Vec<u16> {
    let first = NDI_PORT_BASE.saturating_add(1);
    let last = NDI_PORT_BASE.saturating_add(u16::try_from(n_outputs).unwrap_or(u16::MAX));
    (first..=last).collect()
}

/// The active playlists to create pipelines for, in DETERMINISTIC creation
/// order: `playlist.id` ascending, skipping any playlist with an empty NDI
/// output name (no scene identity, `scene_catalog.rs`). `get_active_playlists`
/// already orders by id, but sorting here makes the guarantee independent of
/// the query and unit-testable.
pub fn ordered_active_for_startup(playlists: &[Playlist]) -> Vec<(i64, String)> {
    let mut out: Vec<(i64, String)> = playlists
        .iter()
        .filter(|p| !p.ndi_output_name.is_empty())
        .map(|p| (p.id, p.ndi_output_name.clone()))
        .collect();
    out.sort_by_key(|(id, _)| *id);
    out
}

/// #225 unit 2: the mode output `id`'s pipeline starts in at startup — its
/// playlist row's (`db::models_playlists::row_mode`); the default for an id
/// the list does not name.
pub fn startup_mode(playlists: &[Playlist], id: i64) -> PlaybackMode {
    playlists
        .iter()
        .find(|p| p.id == id)
        .map_or_else(PlaybackMode::default, |p| {
            crate::db::models_playlists::row_mode(p.id, &p.name, &p.playback_mode)
        })
}

/// Outcome of [`wait_for_ports_free`], surfaced in the startup log.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PortWaitOutcome {
    /// All ports were free on the first probe — no wait.
    FreeImmediately,
    /// Ports became free after this many poll iterations (each preceded by a sleep).
    FreeAfter(u32),
    /// Ports were still not all free after `max_polls` polls — the caller logs a
    /// WARN and proceeds anyway (never blocks startup past the bound).
    TimedOut(u32),
}

/// Poll `prober(ports)` until it reports every port free, or until `max_polls`
/// poll iterations have elapsed. `sleep` is called before each retry (the real
/// caller sleeps 250 ms; tests pass a no-op counter). Pure control flow so the
/// available-at-once / available-after-N / timeout branches are unit-tested
/// with a fake prober and no real sleeping.
pub fn wait_for_ports_free<P, S>(
    mut prober: P,
    ports: &[u16],
    max_polls: u32,
    mut sleep: S,
) -> PortWaitOutcome
where
    P: FnMut(&[u16]) -> bool,
    S: FnMut(),
{
    if prober(ports) {
        return PortWaitOutcome::FreeImmediately;
    }
    for attempt in 1..=max_polls {
        sleep();
        if prober(ports) {
            return PortWaitOutcome::FreeAfter(attempt);
        }
    }
    PortWaitOutcome::TimedOut(max_polls)
}

/// Real port prober: a port is "free" iff a TCP listener can bind it on all
/// interfaces. Binds and immediately drops the listener.
///
/// mutants::skip — I/O over the real network stack; not exercised on the
/// mutation runner. The pure wait loop it feeds (`wait_for_ports_free`) is
/// unit-tested with a fake prober.
#[cfg_attr(test, mutants::skip)]
pub fn ndi_port_free(port: u16) -> bool {
    std::net::TcpListener::bind(("0.0.0.0", port)).is_ok()
}

/// Every port of `ports` free ([`ndi_port_free`]).
#[cfg_attr(test, mutants::skip)] // I/O, as above
pub fn ndi_ports_free(ports: &[u16]) -> bool {
    ports.iter().all(|&p| ndi_port_free(p))
}

/// #196: wait (≤ 10 s, on a blocking thread so the async startup is never
/// stalled) until the previous instance has released `SP-program`'s port span,
/// then return so the sender is created on the same port as before. A span
/// still busy after the bound is a WARN, never a blocked start.
///
/// mutants::skip — orchestration over real ports and `spawn_blocking`; the
/// pure pieces it drives are unit-tested.
#[cfg_attr(test, mutants::skip)]
pub async fn wait_for_program_ports() {
    let ports = ndi_port_range(NDI_SENDERS);
    let outcome = {
        let ports = ports.clone();
        tokio::task::spawn_blocking(move || {
            wait_for_ports_free(ndi_ports_free, &ports, PORT_WAIT_MAX_POLLS, || {
                std::thread::sleep(PORT_WAIT_POLL_INTERVAL)
            })
        })
        .await
        .unwrap_or(PortWaitOutcome::TimedOut(PORT_WAIT_MAX_POLLS))
    };
    match outcome {
        PortWaitOutcome::FreeImmediately => {
            info!(?ports, "ndi: the SP-program port span is free")
        }
        PortWaitOutcome::FreeAfter(polls) => info!(
            ?ports,
            polls, "ndi: the SP-program port span freed after waiting for the previous instance"
        ),
        PortWaitOutcome::TimedOut(polls) => {
            // #240: name the ports still held, so a WARN shows WHICH one.
            let busy: Vec<u16> = ports
                .iter()
                .copied()
                .filter(|&p| !ndi_port_free(p))
                .collect();
            warn!(
                ?ports,
                ?busy,
                polls,
                "ndi: the SP-program port span still busy after the wait — proceeding (its port may shift this restart)"
            )
        }
    }
}

impl PlaybackEngine {
    /// Create a pipeline for every active playlist with an NDI output name, in
    /// `playlist.id` order, each in its row's playback mode (#225 unit 2).
    /// #221 lane 3: a pipeline has no NDI sender, so nothing here waits.
    pub fn create_startup_pipelines(&mut self, playlists: &[Playlist]) {
        let ordered = ordered_active_for_startup(playlists);
        for (id, name) in &ordered {
            let mode = startup_mode(playlists, *id);
            self.ensure_pipeline_inner(*id, name, mode);
        }
        info!(
            count = ordered.len(),
            "playback pipelines created for the active playlists"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pl(id: i64, ndi: &str) -> Playlist {
        Playlist {
            id,
            ndi_output_name: ndi.to_string(),
            is_active: true,
            ..Default::default()
        }
    }

    /// #225 unit 2: each startup output starts in its OWN row's mode.
    #[test]
    fn a_startup_output_starts_in_its_own_rows_mode() {
        let with_mode = |id: i64, mode: &str| Playlist {
            playback_mode: mode.to_string(),
            ..pl(id, &format!("SP-{id}"))
        };
        let playlists = vec![
            with_mode(1, "loop"),
            with_mode(2, "single"),
            with_mode(3, "shuffle"),
        ];

        assert_eq!(startup_mode(&playlists, 2), PlaybackMode::Single);
        assert_eq!(startup_mode(&playlists, 1), PlaybackMode::Loop);
        // An unknown stored value, and an id the list does not name.
        assert_eq!(startup_mode(&playlists, 3), PlaybackMode::Continuous);
        assert_eq!(startup_mode(&playlists, 9), PlaybackMode::Continuous);
    }

    /// #240: the span is SP-program's own sender port, 5961: 5960 is this
    /// process's own NDI runtime listener, taken at `NDIlib_initialize`
    /// (before the wait), so it is never free to probe (SNV, 10.10.2026:
    /// `busy=[5960]` while 5961 was free); and never a "margin" port past
    /// the senders — cg OBS's runtime + sender listen on 5962 + 5963.
    #[test]
    fn the_program_s_port_span_is_its_sender_port() {
        assert_eq!(NDI_SENDERS, 1);
        assert_eq!(ndi_port_range(NDI_SENDERS), vec![5961]);
    }

    #[test]
    fn port_range_is_the_ports_after_the_runtime_s() {
        assert_eq!(ndi_port_range(3), vec![5961, 5962, 5963]);
    }

    #[test]
    fn port_range_zero_outputs_is_empty() {
        assert_eq!(ndi_port_range(0), Vec::<u16>::new());
    }

    #[test]
    fn port_range_saturates_instead_of_overflowing_u16() {
        let ports = ndi_port_range(usize::MAX);
        assert_eq!(ports.first(), Some(&(NDI_PORT_BASE + 1)));
        assert_eq!(ports.last(), Some(&u16::MAX));
    }

    #[test]
    fn ordered_active_sorts_by_id_regardless_of_input_order() {
        let playlists = vec![pl(3, "SP-c"), pl(1, "SP-a"), pl(2, "SP-b")];
        assert_eq!(
            ordered_active_for_startup(&playlists),
            vec![
                (1, "SP-a".to_string()),
                (2, "SP-b".to_string()),
                (3, "SP-c".to_string()),
            ]
        );
    }

    #[test]
    fn ordered_active_skips_empty_ndi_name() {
        let playlists = vec![pl(1, "SP-a"), pl(2, ""), pl(3, "SP-c")];
        assert_eq!(
            ordered_active_for_startup(&playlists),
            vec![(1, "SP-a".to_string()), (3, "SP-c".to_string())]
        );
    }

    #[test]
    fn wait_free_immediately_never_sleeps() {
        let mut sleeps = 0u32;
        let outcome = wait_for_ports_free(|_| true, &[5960, 5961], 40, || sleeps += 1);
        assert_eq!(outcome, PortWaitOutcome::FreeImmediately);
        assert_eq!(sleeps, 0, "must not sleep when ports are already free");
    }

    #[test]
    fn wait_free_after_three_polls() {
        // Free on the 4th probe (initial + 3 retries).
        let mut calls = 0u32;
        let mut sleeps = 0u32;
        let outcome = wait_for_ports_free(
            |_| {
                calls += 1;
                calls >= 4
            },
            &[5960],
            40,
            || sleeps += 1,
        );
        assert_eq!(outcome, PortWaitOutcome::FreeAfter(3));
        assert_eq!(sleeps, 3, "one sleep before each of the 3 retries");
    }

    #[test]
    fn wait_times_out_after_max_polls() {
        let mut sleeps = 0u32;
        let outcome = wait_for_ports_free(|_| false, &[5960], 5, || sleeps += 1);
        assert_eq!(outcome, PortWaitOutcome::TimedOut(5));
        assert_eq!(sleeps, 5, "sleeps once per retry up to the bound");
    }
}
