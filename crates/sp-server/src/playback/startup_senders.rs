//! #196: deterministic, restart-safe NDI sender startup.
//!
//! Root cause of the dark-wall-after-restart incident (19.9.2026): SongPlayer
//! created its NDI senders lazily / racily, so the SAME stream name could land
//! on a DIFFERENT TCP port after a restart. A DistroAV receiver reconnects a
//! stale source BY URL with the PINNED previous port, so it lands on the wrong
//! or a dead sender and stays at `connections=0`.
//!
//! The fix is deterministic sender identity: at startup, wait for the previous
//! instance's ports to be released, then create every active playlist's sender
//! in `playlist.id` ascending order (one at a time), so the NDI runtime hands
//! out the SAME name→port map every restart.
//!
//! This module holds the PURE pieces (port range, port-availability wait,
//! creation order) — unit-tested on Linux with no NDI runtime — and, in
//! `impl PlaybackEngine`, the orchestration that drives them on the box.

use std::time::Duration;

use sp_core::models::Playlist;
use tracing::{info, warn};

use super::PlaybackEngine;

// #196: the finder pass needs the NDI backend, which — like `SharedNdiBackend`
// and the engine's `ndi_backend` field — exists ONLY on Windows (Linux/CI has
// no NDI runtime). These imports + the finder fn/const below are gated so the
// Linux `clippy -D warnings` + `cargo test` jobs don't see an unused import or a
// missing field.
#[cfg(windows)]
use super::ndi_health::NdiHealthRegistry;
#[cfg(windows)]
use super::pipeline::SharedNdiBackend;
#[cfg(windows)]
use sp_ndi::NdiBackend;
#[cfg(windows)]
use std::sync::Arc;

/// Poll cadence for the startup port-availability wait.
const PORT_WAIT_POLL_INTERVAL: Duration = Duration::from_millis(250);
/// Max polls (≤ 10 s at 250 ms) before startup proceeds regardless (#196).
const PORT_WAIT_MAX_POLLS: u32 = 40;
/// How long to wait for one pipeline to report its sender ready before moving
/// on to the next (bounded so a stuck sender never wedges startup).
const SENDER_READY_TIMEOUT: Duration = Duration::from_secs(5);

/// #196: how long ONE `NDIlib_find` discovery pass polls for every startup
/// sender's advertised name→URL to appear (the ruling's ≤ 3 s). Windows-only
/// (the finder is a Windows NDI-runtime call).
#[cfg(windows)]
const FINDER_TIMEOUT_MS: u32 = 3000;

/// #196: after the first discovery pass, retry ONCE at +30 s (together with the
/// receiver self-check window) for any output whose URL had not yet appeared —
/// the SDK finder can take a moment to see a freshly-created local sender.
#[cfg(windows)]
const SENDER_URL_RETRY_DELAY: Duration = Duration::from_secs(30);

/// The first TCP port the NDI runtime assigns to a sender on this box. NDI
/// hands out ports sequentially from here in sender-creation order, which is
/// exactly why creation order must be deterministic (#196).
pub const NDI_PORT_BASE: u16 = 5960;

/// The port range to probe-bind before creating the first sender: the base
/// plus one port per active output plus a small margin (`base..=base+N+1`),
/// so an immediate restart waits until the previous instance released the
/// whole span it could have used.
pub fn ndi_port_range(n_outputs: usize) -> Vec<u16> {
    let last = NDI_PORT_BASE + n_outputs as u16 + 1;
    (NDI_PORT_BASE..=last).collect()
}

/// The active playlists to pre-create senders for, in DETERMINISTIC creation
/// order: `playlist.id` ascending, skipping any playlist with an empty NDI
/// output name. `get_active_playlists` already orders by id, but sorting here
/// makes the guarantee independent of the query and unit-testable.
pub fn ordered_active_for_startup(playlists: &[Playlist]) -> Vec<(i64, String)> {
    let mut out: Vec<(i64, String)> = playlists
        .iter()
        .filter(|p| !p.ndi_output_name.is_empty())
        .map(|p| (p.id, p.ndi_output_name.clone()))
        .collect();
    out.sort_by_key(|(id, _)| *id);
    out
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
/// interfaces. Used before creating senders so an immediate restart waits for
/// the previous instance's NDI listeners to be released (#196). Binds and
/// immediately drops each listener.
///
/// mutants::skip — I/O over the real network stack; not exercised on the
/// mutation runner. The pure wait loop it feeds (`wait_for_ports_free`) is
/// unit-tested with a fake prober.
#[cfg_attr(test, mutants::skip)]
pub fn ndi_ports_free(ports: &[u16]) -> bool {
    ports
        .iter()
        .all(|&p| std::net::TcpListener::bind(("0.0.0.0", p)).is_ok())
}

/// #196: read each startup sender's advertised `host:port` via ONE
/// `NDIlib_find` discovery pass and record it in the health registry, so
/// `/api/v1/ndi/health` carries the name→port map and the startup log prints
/// one `ndi: sender ready name=… url=…` line per output. A name that never
/// appears within the finder timeout gets a WARN. The blocking finder poll runs
/// on `spawn_blocking` so the async startup / the +30 s retry task is not
/// stalled. No-op when NDI is not configured (Linux/CI have no backend).
///
/// mutants::skip — I/O orchestration (spawn_blocking + the FFI finder); the
/// pure name→URL matching (`sp_ndi::find::match_source_urls`) is unit-tested.
/// Windows-only (uses the Windows-only `SharedNdiBackend`).
#[cfg(windows)]
#[cfg_attr(test, mutants::skip)]
pub(crate) async fn discover_and_record_sender_urls(
    backend: Option<SharedNdiBackend>,
    registry: Arc<NdiHealthRegistry>,
    ordered: Vec<(i64, String)>,
) {
    let Some(backend) = backend else {
        return;
    };
    if ordered.is_empty() {
        return;
    }
    let names: Vec<String> = ordered.iter().map(|(_, n)| n.clone()).collect();
    let discovered = tokio::task::spawn_blocking(move || {
        backend.discover_local_sources(&names, FINDER_TIMEOUT_MS)
    })
    .await
    .unwrap_or_default();
    let matched = sp_ndi::find::match_source_urls(&discovered, &ordered);
    for (id, name) in &ordered {
        match matched.iter().find(|(mid, _)| mid == id) {
            Some((_, url)) => {
                registry.set_sender_url(*id, Some(url.clone()));
                info!("ndi: sender ready name={name} url={url}");
            }
            None => warn!("ndi: sender {name} did not appear in NDI discovery within 3 s"),
        }
    }
}

impl PlaybackEngine {
    /// #196: create the NDI senders for all active playlists in DETERMINISTIC
    /// `playlist.id` order at startup, so the NDI runtime hands out the SAME
    /// name→port map on every restart (the fix for the dark-wall-after-restart
    /// incident). First waits for the previous instance's ports to be released
    /// (so an immediate restart gets the same assignment), then creates each
    /// sender one at a time, waiting for it to report ready before the next —
    /// serializing the `send_create` calls that otherwise raced across pipeline
    /// threads and shuffled the ports.
    ///
    /// mutants::skip — orchestration (real ports, threads, timeouts); the pure
    /// pieces (`ndi_port_range`, `ordered_active_for_startup`,
    /// `wait_for_ports_free`) are unit-tested, and the box acceptance proves
    /// the stable map end-to-end.
    #[cfg_attr(test, mutants::skip)]
    pub async fn create_startup_senders(&mut self, playlists: &[Playlist]) {
        let ordered = ordered_active_for_startup(playlists);
        if ordered.is_empty() {
            info!("ndi: no active playlists with an NDI output name — no startup senders");
            return;
        }

        // #196 item 4: seed the post-restart receiver self-check baseline from
        // the settings table (the per-output receiver counts persisted before
        // this process restarted) BEFORE any sender is created.
        let baseline = crate::db::models_ndi::all_last_receiver_counts(&self.pool).await;
        info!(
            outputs = baseline.len(),
            "ndi: seeded pre-restart receiver baseline for the post-restart self-check"
        );
        self.ndi_health_registry.seed_pre_restart_counts(baseline);

        // Port-availability wait — before creating the first sender. Runs on a
        // blocking thread so the ≤ 10 s poll (with real `thread::sleep`) never
        // stalls the async executor during startup.
        let ports = ndi_port_range(ordered.len());
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
                info!(?ports, "ndi: startup NDI ports free — creating senders")
            }
            PortWaitOutcome::FreeAfter(polls) => info!(
                ?ports,
                polls, "ndi: startup NDI ports freed after waiting for the previous instance"
            ),
            PortWaitOutcome::TimedOut(polls) => warn!(
                ?ports,
                polls,
                "ndi: startup NDI ports still busy after wait — proceeding (name→port map may shift this restart)"
            ),
        }

        // Serialized, id-ordered creation: create sender i, wait for it to
        // report ready (so its port is assigned) before creating sender i+1.
        for (id, name) in &ordered {
            self.create_and_record_sender(*id, name).await;
        }

        // #196 item 4: the senders are ready — start the +30 s post-restart
        // receiver self-check clock (cross-platform: the self-check runs off the
        // health registry, no NDI backend needed).
        self.ndi_health_registry.mark_senders_ready();

        // #196: now that every startup sender exists, read the advertised
        // name→port map via ONE NDIlib_find discovery pass and record it on the
        // health registry (the ruling — `NDIlib_send_get_source_name` leaves the
        // URL empty for a local sender). Retry once at +30 s for any that had
        // not yet appeared to the finder. WINDOWS ONLY — the NDI backend
        // (`self.ndi_backend` / `SharedNdiBackend`) exists only on Windows.
        #[cfg(windows)]
        {
            let backend = self.ndi_backend.clone();
            let registry = self.ndi_health_registry.clone();
            discover_and_record_sender_urls(backend.clone(), registry.clone(), ordered.clone())
                .await;
            tokio::spawn(async move {
                tokio::time::sleep(SENDER_URL_RETRY_DELAY).await;
                discover_and_record_sender_urls(backend, registry, ordered).await;
            });
        }
    }

    /// #196: create one output's pipeline (idempotent) and, once its NDI sender
    /// reports ready, record the advertised URL in the health registry so it
    /// shows on `/api/v1/ndi/health`. Bounded wait — a stuck/absent sender never
    /// blocks past [`SENDER_READY_TIMEOUT`]. Used by the startup serializer (in
    /// id order) and by the runtime activate path. Returns the URL (if any).
    ///
    /// mutants::skip — I/O orchestration (oneshot + timeout + registry write);
    /// the pure ordering/port pieces are unit-tested and the box acceptance
    /// proves the recorded map end-to-end.
    #[cfg_attr(test, mutants::skip)]
    pub(crate) async fn create_and_record_sender(&mut self, id: i64, name: &str) -> Option<String> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.ensure_pipeline_inner(id, name, Some(tx));
        let url = match tokio::time::timeout(SENDER_READY_TIMEOUT, rx).await {
            Ok(Ok(u)) => u,
            Ok(Err(_)) => {
                // Pipeline already existed (closure not run) → URL already
                // recorded on an earlier create; keep it.
                return self.ndi_health_registry.sender_url(id);
            }
            Err(_) => {
                warn!(
                    playlist_id = id,
                    ndi_name = %name,
                    "ndi: sender did not report ready within timeout — continuing"
                );
                None
            }
        };
        self.ndi_health_registry.set_sender_url(id, url.clone());
        url
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

    #[test]
    fn port_range_is_base_through_base_plus_n_plus_one() {
        assert_eq!(ndi_port_range(3), vec![5960, 5961, 5962, 5963, 5964]);
    }

    #[test]
    fn port_range_zero_outputs_still_covers_base_pair() {
        assert_eq!(ndi_port_range(0), vec![5960, 5961]);
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
