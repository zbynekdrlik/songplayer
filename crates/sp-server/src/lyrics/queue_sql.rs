//! The lyrics queue's shared predicates (`v.` / `p.` aliases, matching the
//! selector's bucket queries in `reprocess.rs`): the one truth for what the
//! lyrics worker takes and for what the node exchange announces as this
//! node's queued lyrics (#229, `peer::queued`). In their own module because
//! `reprocess.rs` sits near the 1000-line cap.

/// The rows every selector bucket draws from, apart from their own recheck
/// time: on an active playlist, downloaded, and never a dub-requested video
/// (#182: a dubbed talk gets its EN/SK subtitles from the Live-session
/// transcript, `dabing::subtitles`, not the song-lyrics pipeline), nor the
/// #228 test item (`test_item::not_test_item!`). A macro so
/// [`LYRICS_ELIGIBLE`] and [`LYRICS_DUE`] are built from one text.
macro_rules! lyrics_eligible {
    () => {
        concat!(
            "p.is_active = 1 AND v.normalized = 1 \
             AND (v.dub_requested IS NULL OR v.dub_requested = 0) AND ",
            crate::test_item::not_test_item!()
        )
    };
}

/// The text of the macro above. In [`queued_later_where`] (#229).
pub(crate) const LYRICS_ELIGIBLE: &str = lyrics_eligible!();

/// [`LYRICS_ELIGIBLE`] past any retry backoff or recheck: the rows every
/// bucket takes NOW. Also in [`queued_where`] (#229).
pub(crate) const LYRICS_DUE: &str = concat!(
    lyrics_eligible!(),
    " AND (v.lyrics_next_attempt_at IS NULL \
     OR v.lyrics_next_attempt_at <= strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))"
);

/// Not parked by a terminal failure (`failed`, `empty`, `no_source`, the
/// `asr_gap` quarantine of #86, `unsupported_source`), unless it was parked
/// at an OLDER pipeline version (the worker may succeed now). Binds ONE `?`:
/// the current version. Buckets 1 and 2, and [`queued_where`] (#229).
pub(crate) const LYRICS_NOT_PARKED: &str = "(v.lyrics_source IS NULL \
     OR v.lyrics_source NOT IN ('failed', 'empty', 'no_source', 'asr_gap', 'unsupported_source') \
     OR v.lyrics_pipeline_version < ?)";

/// The rows' own conditions of buckets 1–3 (manual, null, stale), as one
/// `WHERE` body. Bucket 4 (the full-mix upgrade) is not in it: its row
/// already serves lyrics at the current version. Binds THREE `?`, each the
/// current pipeline version.
fn buckets_1_to_3() -> String {
    format!(
        "((v.lyrics_manual_priority = 1 AND {LYRICS_NOT_PARKED}) \
         OR ((v.has_lyrics IS NULL OR v.has_lyrics = 0) AND {LYRICS_NOT_PARKED} \
             AND v.lyrics_manual_priority = 0) \
         OR (v.has_lyrics = 1 AND v.lyrics_pipeline_version < ? \
             AND v.lyrics_manual_priority = 0))"
    )
}

/// The rows buckets 1–3 take NOW: the node exchange lists them as this
/// node's QUEUED lyrics (#229, `peer::queued`). Binds THREE `?`, each the
/// current pipeline version.
pub(crate) fn queued_where() -> String {
    format!("{LYRICS_DUE} AND {}", buckets_1_to_3())
}

/// The rows buckets 1–3 take once their own recheck time has come, whatever
/// it says now (#229): with their stems queued here, a row put back by
/// `WaitingForStems` is one the worker WILL take (`peer::queued`). Binds
/// THREE `?`, each the current pipeline version.
pub(crate) fn queued_later_where() -> String {
    format!("{LYRICS_ELIGIBLE} AND {}", buckets_1_to_3())
}
