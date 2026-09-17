//! NDI source discovery — queries OBS for NDI inputs and matches against the
//! DB's active playlists to build the scene-detection map.
//!
//! This is the glue that was missing in the initial migration: `lib.rs`
//! used to create the `NdiSourceMap` as `HashMap::new()` and never populate
//! it, so [`scene::check_scene_items`] always returned an empty active set
//! and scene-driven playback never fired (issue #11).

use std::collections::HashMap;

use sqlx::{Row, SqlitePool};
use tokio_tungstenite::tungstenite::Message;
use tracing::{debug, info, warn};

use crate::obs::SharedWrite;
use crate::obs::dispatcher::{DEFAULT_RESPONSE_TIMEOUT, Dispatcher, DispatcherError};
use crate::obs::text::{
    get_input_list_request, get_input_settings_request, set_ndi_source_name_request,
};

/// Query OBS for its NDI inputs and return a map of
/// `OBS input name → playlist_id` for the playlists whose `ndi_output_name`
/// matches an input's `ndi_source_name` setting.
///
/// The map is keyed by the OBS input name (e.g. `"sp-fast_video"`) — that is
/// what [`scene::check_scene_items`] compares against. The playlist's
/// `ndi_output_name` (e.g. `"SP-fast"`) is only used as the join key against
/// the OBS input's `ndi_source_name` setting.
/// Rebuild the OBS-input-name → playlist-id map.
///
/// Returns `None` when the rebuild **cannot be trusted** — typically a
/// transient OBS query failure. Callers MUST preserve the previously-built
/// map in that case, otherwise a single WebSocket hiccup wipes scene
/// detection and every `CurrentProgramSceneChanged` becomes a no-op
/// (silent playback stall; this is what broke the 2026-04-19 event).
///
/// Returns `Some(HashMap)` with the fresh mapping when the rebuild ran
/// end-to-end. An empty map is still a valid `Some`: it means the DB
/// genuinely has no active playlists, or OBS genuinely has no NDI
/// source inputs — both legitimate steady states.
pub async fn rebuild_ndi_source_map(
    write: &SharedWrite,
    dispatcher: &Dispatcher,
    pool: &SqlitePool,
) -> Option<HashMap<String, i64>> {
    let mut map = HashMap::new();

    // #173: the advertised host case senders on THIS box announce (COMPUTERNAME),
    // read once. `None` on non-Windows / CI → normalization is skipped below.
    let advertised_host = advertised_ndi_host();

    let by_ndi_name = match load_playlist_ndi_names(pool).await {
        Ok(m) => m,
        Err(e) => {
            warn!("rebuild_ndi_source_map: failed to load playlists: {e}");
            return None;
        }
    };

    if by_ndi_name.is_empty() {
        debug!("rebuild_ndi_source_map: no active playlists with ndi_output_name");
        return Some(map);
    }

    let input_names = match fetch_ndi_input_names(write, dispatcher).await {
        Some(names) => names,
        None => {
            warn!(
                "rebuild_ndi_source_map: GetInputList returned nothing; \
                 keeping previous map so scene detection stays alive"
            );
            return None;
        }
    };

    if input_names.is_empty() {
        debug!("rebuild_ndi_source_map: OBS has no NDI source inputs");
        return Some(map);
    }

    for input_name in input_names {
        let sender_name = match fetch_input_ndi_sender_name(write, dispatcher, &input_name).await {
            Some(s) => s,
            None => {
                debug!(
                    "rebuild_ndi_source_map: input '{input_name}' has no ndi_source_name setting"
                );
                continue;
            }
        };

        // The NDI plugin stores the full network-visible name, e.g.
        // `"RESOLUME-SNV (SP-fast)"` — machine hostname + the stream name
        // in parentheses. Extract the stream portion so we can match
        // against the playlist's `ndi_output_name` which is just the
        // bare stream name SongPlayer gave to its NdiLib sender.
        let stream_name = extract_ndi_stream_name(&sender_name);

        if let Some(&playlist_id) = by_ndi_name.get(stream_name) {
            // #173: if the stored name is the right sender but the wrong host
            // CASE, rewrite it to the advertised form so DistroAV re-attaches
            // after a SongPlayer restart (its post-re-announce match is
            // case-sensitive). Only fires for a pure case variant; a no-op
            // otherwise (already canonical, or advertised host unknown).
            if let Some(host) = advertised_host.as_deref() {
                let advertised = format!("{host} ({stream_name})");
                if let Some(canonical) = canonical_sender_name(&sender_name, &advertised) {
                    if set_input_ndi_source_name(write, dispatcher, &input_name, &canonical).await {
                        info!(
                            "ndi: normalized input '{input_name}' sender host case → '{canonical}'"
                        );
                    } else {
                        warn!(
                            "ndi: failed to normalize input '{input_name}' sender host case to '{canonical}'"
                        );
                    }
                }
            }
            debug!(
                "rebuild_ndi_source_map: '{input_name}' → playlist {playlist_id} (NDI sender '{sender_name}', stream '{stream_name}')"
            );
            map.insert(input_name, playlist_id);
        } else {
            debug!(
                "rebuild_ndi_source_map: no playlist matches NDI sender '{sender_name}' (stream '{stream_name}')"
            );
        }
    }

    info!(count = map.len(), "rebuilt NDI source map from OBS + DB");
    Some(map)
}

/// Extract the stream portion from an NDI network name.
///
/// NDI network names follow the format `"MACHINE (stream)"` where the
/// machine hostname is outside the parentheses and the stream name
/// SongPlayer gave to its NdiLib sender is inside. This function returns
/// the slice between the **first** ` (` and the final `)`, so nested
/// parentheses inside the stream name are preserved.
///
/// If the input has no `" ("` delimiter or does not end with `)`, it is
/// returned verbatim.
///
/// # NDI network name format
///
/// The official NDI SDK formats source names as `"<machine> (<stream>)"`
/// where `<machine>` is the host that owns the sender and `<stream>` is
/// the human-readable name the sender passed to `NDIlib_send_create`.
/// OBS's NDI input plugin stores this exact string in its
/// `ndi_source_name` input setting. Matching against the bare `<stream>`
/// part is therefore required to link an OBS NDI input back to a
/// SongPlayer playlist whose `ndi_output_name` is just the stream label.
///
/// Examples:
/// - `"RESOLUME-SNV (SP-fast)"` → `"SP-fast"`
/// - `"machine (name with spaces)"` → `"name with spaces"`
/// - `"SP-fast"` → `"SP-fast"` (no parentheses — return as-is)
/// - `"weird (inner (nested))"` → `"inner (nested)"` (first `(` wins)
pub(crate) fn extract_ndi_stream_name(full: &str) -> &str {
    // Find the first `" ("` delimiter; require the string to end with
    // `)`. Fall back to the raw name for any shape we don't recognise.
    if full.ends_with(')') {
        if let Some(open) = full.find(" (") {
            let inner_start = open + 2;
            let inner_end = full.len() - 1;
            if inner_end > inner_start {
                return &full[inner_start..inner_end];
            }
        }
    }
    full
}

/// #173: GREEN sets this to `true`. The RED commit ships it `false` so the
/// case-variant rewrite is disabled and the `canonical_sender_name` unit tests
/// that expect a rewrite fail cleanly — the TIER-0 "one wrong constant" RED
/// pattern (`.claude/rules/rust-workspace.md`), no dead-code / clippy noise.
const REWRITE_CASE_VARIANTS: bool = false;

/// #173: decide whether an OBS NDI input's stored `ndi_source_name` should be
/// rewritten to the sender name NDI actually advertises.
///
/// Returns `Some(advertised)` iff `stored` names the SAME sender as
/// `advertised` but differs in ASCII **case alone** — e.g.
/// `stored = "resolume-snv (SP-slow)"`, `advertised = "RESOLUME-SNV (SP-slow)"`.
/// Returns `None` when they are byte-identical (already canonical) or name a
/// genuinely different sender (a different stream, or a host that is more than a
/// case variant). This is the guard that keeps the rewrite from ever renaming an
/// input to a different sender: the only thing it can change is host case, and
/// only toward the verified-correct advertised form.
pub(crate) fn canonical_sender_name(stored: &str, advertised: &str) -> Option<String> {
    if REWRITE_CASE_VARIANTS && stored != advertised && stored.eq_ignore_ascii_case(advertised) {
        Some(advertised.to_string())
    } else {
        None
    }
}

/// #173: the NDI-advertised host prefix for senders created on THIS box — the
/// machine's computer name as the NDI runtime announces it.
///
/// On win-resolume the NDI runtime advertises `"RESOLUME-SNV (<stream>)"`,
/// matching Windows' `COMPUTERNAME` (the NetBIOS name) EXACTLY, while
/// `gethostname()` reports the lowercase `resolume-snv` — the very source of the
/// #173 case mismatch. So we read `COMPUTERNAME`. When it is unset or empty
/// (Linux CI, a non-Windows box) we return `None` and normalization is skipped
/// (behaviour unchanged; never a wrong-case rewrite).
fn advertised_ndi_host() -> Option<String> {
    std::env::var("COMPUTERNAME").ok().filter(|s| !s.is_empty())
}

/// Load the `{ndi_output_name → playlist_id}` map for all active playlists.
async fn load_playlist_ndi_names(pool: &SqlitePool) -> Result<HashMap<String, i64>, sqlx::Error> {
    let rows = sqlx::query(
        "SELECT id, ndi_output_name FROM playlists \
         WHERE is_active = 1 AND ndi_output_name != ''",
    )
    .fetch_all(pool)
    .await?;

    let mut map = HashMap::with_capacity(rows.len());
    for row in &rows {
        let id: i64 = row.get("id");
        let ndi: String = row.get("ndi_output_name");
        map.insert(ndi, id);
    }
    Ok(map)
}

/// Issue `GetInputList` filtered to NDI sources and return the list of input
/// names. Returns `None` if the request failed or the response was malformed.
async fn fetch_ndi_input_names(
    write: &SharedWrite,
    dispatcher: &Dispatcher,
) -> Option<Vec<String>> {
    let req_id = uuid::Uuid::new_v4().to_string();
    let req = get_input_list_request(&req_id);
    let response = match dispatcher
        .send_and_await(
            write,
            req_id,
            Message::Text(req.to_string().into()),
            DEFAULT_RESPONSE_TIMEOUT,
        )
        .await
    {
        Ok(v) => v,
        Err(DispatcherError::Timeout) => {
            warn!("fetch_ndi_input_names: GetInputList timed out");
            return None;
        }
        Err(DispatcherError::Closed) => {
            warn!("fetch_ndi_input_names: dispatcher closed before reply");
            return None;
        }
    };
    let arr = response["d"]["responseData"]["inputs"].as_array()?;

    Some(
        arr.iter()
            .filter_map(|v| v["inputName"].as_str().map(|s| s.to_string()))
            .collect(),
    )
}

/// Issue `GetInputSettings` for a single input and extract the
/// `ndi_source_name` setting (the NDI sender name that the OBS input receives
/// from). Returns `None` if the setting is absent.
async fn fetch_input_ndi_sender_name(
    write: &SharedWrite,
    dispatcher: &Dispatcher,
    input_name: &str,
) -> Option<String> {
    let req_id = uuid::Uuid::new_v4().to_string();
    let req = get_input_settings_request(&req_id, input_name);
    let response = match dispatcher
        .send_and_await(
            write,
            req_id,
            Message::Text(req.to_string().into()),
            DEFAULT_RESPONSE_TIMEOUT,
        )
        .await
    {
        Ok(v) => v,
        Err(DispatcherError::Timeout) => {
            warn!("fetch_input_ndi_sender_name: GetInputSettings timed out for {input_name}");
            return None;
        }
        Err(DispatcherError::Closed) => {
            warn!("fetch_input_ndi_sender_name: dispatcher closed before reply for {input_name}");
            return None;
        }
    };
    response["d"]["responseData"]["inputSettings"]["ndi_source_name"]
        .as_str()
        .map(|s| s.to_string())
}

/// Outcome of a receiver-recovery nudge (#127), for logging.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ReapplyOutcome {
    /// Found the matching input and applied clear + restore.
    Applied,
    /// No OBS NDI input advertises the target stream name.
    NoMatch,
    /// An OBS query/set failed; the nudge could not be completed.
    Failed,
}

/// #127: nudge OBS to re-subscribe a stranded NDI receiver for `target_stream`
/// (the bare stream name, e.g. `"SP-slow"`).
///
/// Enumerates OBS's NDI inputs, finds the one whose `ndi_source_name`
/// advertises `target_stream` (via [`extract_ndi_stream_name`]), then clears
/// (`""`) and restores that field so DistroAV re-runs discovery. Re-applying
/// the *identical* value is a no-op for DistroAV — proven on issue #127 — so
/// the clear-then-restore is required. Receiver-side only; never a per-sender
/// `RecreateSender` (CLAUDE.md "Disabled subsystems", #60).
pub(crate) async fn reapply_ndi_input(
    write: &SharedWrite,
    dispatcher: &Dispatcher,
    target_stream: &str,
) -> ReapplyOutcome {
    let input_names = match fetch_ndi_input_names(write, dispatcher).await {
        Some(names) => names,
        None => {
            warn!(
                target = target_stream,
                "ndi-recovery: GetInputList returned nothing; cannot nudge"
            );
            return ReapplyOutcome::Failed;
        }
    };

    for input_name in input_names {
        let sender_name = match fetch_input_ndi_sender_name(write, dispatcher, &input_name).await {
            Some(s) => s,
            None => continue,
        };
        if extract_ndi_stream_name(&sender_name) != target_stream {
            continue;
        }

        info!(
            input_name = %input_name,
            sender_name = %sender_name,
            target = target_stream,
            "ndi-recovery: nudging stranded receiver (clear + restore ndi_source_name)"
        );

        // Clear the field — an empty ndi_source_name makes DistroAV drop the
        // dead subscription.
        let cleared = set_input_ndi_source_name(write, dispatcher, &input_name, "").await;
        // Restore the original network-visible name so the receiver
        // re-subscribes to the live sender.
        let restored =
            set_input_ndi_source_name(write, dispatcher, &input_name, &sender_name).await;

        if cleared && restored {
            info!(
                input_name = %input_name,
                "ndi-recovery: clear + restore applied; DistroAV will re-run discovery"
            );
            return ReapplyOutcome::Applied;
        }
        warn!(
            input_name = %input_name,
            cleared,
            restored,
            "ndi-recovery: nudge did not fully apply (OBS query/set failed)"
        );
        return ReapplyOutcome::Failed;
    }

    warn!(
        target = target_stream,
        "ndi-recovery: no OBS NDI input advertises this stream; cannot nudge"
    );
    ReapplyOutcome::NoMatch
}

/// Send a `SetInputSettings` that writes `ndi_source_name` on `input_name`.
/// Returns `true` iff OBS acknowledged success.
async fn set_input_ndi_source_name(
    write: &SharedWrite,
    dispatcher: &Dispatcher,
    input_name: &str,
    value: &str,
) -> bool {
    let req_id = uuid::Uuid::new_v4().to_string();
    let req = set_ndi_source_name_request(&req_id, input_name, value);
    match dispatcher
        .send_and_await(
            write,
            req_id,
            Message::Text(req.to_string().into()),
            DEFAULT_RESPONSE_TIMEOUT,
        )
        .await
    {
        Ok(response) => response["d"]["requestStatus"]["result"]
            .as_bool()
            .unwrap_or(false),
        Err(e) => {
            warn!(input_name, error = %e, "ndi-recovery: SetInputSettings failed");
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_ndi_stream_name_strips_machine_prefix() {
        assert_eq!(extract_ndi_stream_name("RESOLUME-SNV (SP-fast)"), "SP-fast");
        assert_eq!(extract_ndi_stream_name("WIN-BOX (SP-warmup)"), "SP-warmup");
        assert_eq!(
            extract_ndi_stream_name("dev-machine-1 (stream with spaces)"),
            "stream with spaces"
        );
    }

    #[test]
    fn extract_ndi_stream_name_passes_through_bare_names() {
        // Already bare — return as-is.
        assert_eq!(extract_ndi_stream_name("SP-fast"), "SP-fast");
        assert_eq!(extract_ndi_stream_name("no-parens"), "no-parens");
    }

    #[test]
    fn extract_ndi_stream_name_handles_empty_and_weird_inputs() {
        assert_eq!(extract_ndi_stream_name(""), "");
        // No space before the open paren → treat as opaque.
        assert_eq!(extract_ndi_stream_name("(just-parens)"), "(just-parens)");
        // find(" (") picks the FIRST ` (` so nested parens inside the
        // stream name are preserved, e.g. an SDK-generated fixup.
        assert_eq!(
            extract_ndi_stream_name("weird (inner (nested))"),
            "inner (nested)"
        );
        // A name that doesn't end with `)` is passed through untouched.
        assert_eq!(
            extract_ndi_stream_name("machine (incomplete"),
            "machine (incomplete"
        );
        // Empty parenthesised portion is passed through.
        assert_eq!(extract_ndi_stream_name("machine ()"), "machine ()");
    }

    #[test]
    fn canonical_sender_name_rewrites_a_pure_case_variant() {
        // The #173 dark-wall case: stored lowercase host, advertised uppercase.
        assert_eq!(
            canonical_sender_name("resolume-snv (SP-slow)", "RESOLUME-SNV (SP-slow)"),
            Some("RESOLUME-SNV (SP-slow)".to_string()),
        );
        // A mixed-case host that still folds to the advertised host.
        assert_eq!(
            canonical_sender_name("Resolume-Snv (SP-worship)", "RESOLUME-SNV (SP-worship)"),
            Some("RESOLUME-SNV (SP-worship)".to_string()),
        );
    }

    #[test]
    fn canonical_sender_name_none_when_already_canonical() {
        // Byte-identical → nothing to rewrite.
        assert_eq!(
            canonical_sender_name("RESOLUME-SNV (SP-fast)", "RESOLUME-SNV (SP-fast)"),
            None,
        );
    }

    #[test]
    fn canonical_sender_name_none_for_a_different_sender() {
        // Different stream — not a case variant, must NOT be rewritten.
        assert_eq!(
            canonical_sender_name("RESOLUME-SNV (SP-fast)", "RESOLUME-SNV (SP-slow)"),
            None,
        );
        // Genuinely different host (more than a case difference) — must NOT be
        // rewritten to a foreign sender.
        assert_eq!(
            canonical_sender_name("OTHER-BOX (SP-slow)", "RESOLUME-SNV (SP-slow)"),
            None,
        );
    }

    #[tokio::test]
    async fn load_playlist_ndi_names_returns_active_with_non_empty_output() {
        let pool = crate::db::create_memory_pool().await.unwrap();
        crate::db::run_migrations(&pool).await.unwrap();

        sqlx::query(
            "INSERT INTO playlists (id, name, youtube_url, ndi_output_name, is_active) \
             VALUES (1, 'a', 'u1', 'SP-a', 1), \
                    (2, 'b', 'u2', 'SP-b', 1), \
                    (3, 'c', 'u3', '', 1), \
                    (4, 'd', 'u4', 'SP-d', 0)",
        )
        .execute(&pool)
        .await
        .unwrap();

        let map = load_playlist_ndi_names(&pool).await.unwrap();
        assert_eq!(map.len(), 2);
        assert_eq!(map.get("SP-a"), Some(&1));
        assert_eq!(map.get("SP-b"), Some(&2));
        // playlist 3 has empty output name — excluded.
        // playlist 4 is inactive — excluded.
        assert!(!map.contains_key("SP-d"));
    }
}
