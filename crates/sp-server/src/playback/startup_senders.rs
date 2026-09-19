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

use sp_core::models::Playlist;

/// The first TCP port the NDI runtime assigns to a sender on this box. NDI
/// hands out ports sequentially from here in sender-creation order, which is
/// exactly why creation order must be deterministic (#196).
pub const NDI_PORT_BASE: u16 = 5960;

/// The port range to probe-bind before creating the first sender: the base
/// plus one port per active output plus a small margin (`base..=base+N+1`),
/// so an immediate restart waits until the previous instance released the
/// whole span it could have used.
pub fn ndi_port_range(n_outputs: usize) -> Vec<u16> {
    // RED stub (#196) — real impl lands in the GREEN commit.
    let _ = n_outputs;
    Vec::new()
}

/// The active playlists to pre-create senders for, in DETERMINISTIC creation
/// order: `playlist.id` ascending, skipping any playlist with an empty NDI
/// output name. `get_active_playlists` already orders by id, but sorting here
/// makes the guarantee independent of the query and unit-testable.
pub fn ordered_active_for_startup(playlists: &[Playlist]) -> Vec<(i64, String)> {
    // RED stub (#196) — real impl lands in the GREEN commit.
    let _ = playlists;
    Vec::new()
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
    // RED stub (#196) — real impl lands in the GREEN commit.
    let _ = (&mut prober, ports, max_polls, &mut sleep);
    PortWaitOutcome::TimedOut(0)
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
