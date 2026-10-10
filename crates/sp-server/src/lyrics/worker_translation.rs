//! Translation-side lyrics worker logic (#152), split from `worker.rs` (which
//! is at the 1000-line airuleset cap): the gender-aware `translate_track`
//! primitive + `apply_translations`, plus the stale-translation retranslate
//! pass that re-runs EN→SK under the current prompt/gender without touching
//! alignment or `lyrics_pipeline_version`.

use std::time::{Duration, Instant};

use sp_core::lyrics::LyricsTrack;
use tracing::{debug, info, warn};

use crate::db::models::{
    fetch_next_stale_translation, get_translation_gender, stamp_translation_version,
};
use crate::lyrics::LYRICS_TRANSLATION_VERSION;
use crate::lyrics::translator::{self, SpeakerGender};
use crate::lyrics::worker::LyricsWorker;

impl LyricsWorker {
    /// Apply per-line translations to `track`. Empty strings leave `sk = None`.
    #[cfg_attr(test, mutants::skip)]
    pub(crate) fn apply_translations(track: &mut LyricsTrack, translations: Vec<String>) {
        for (line, sk_text) in track.lines.iter_mut().zip(translations) {
            line.sk = if sk_text.is_empty() {
                None
            } else {
                Some(sk_text)
            };
        }
        track.language_translation = "sk".into();
    }

    /// EN→SK step of the pipeline. Silent on failure — the UI degrades to
    /// English-only. Claude-only by design: the user pays a Max Plus
    /// subscription (unlimited at that tier) and Gemini quota is reserved for
    /// alignment; a refusal is fixed by tuning `translator::build_prompt`, not
    /// a Gemini fallback. `gender` picks masculine (default) / feminine Slovak
    /// first-person forms (#152).
    #[cfg_attr(test, mutants::skip)]
    pub(crate) async fn translate_track(
        &self,
        track: &mut LyricsTrack,
        youtube_id: &str,
        gender: SpeakerGender,
    ) {
        let Some(ai_client) = &self.ai_client else {
            return;
        };
        if !self.translation_allowed(youtube_id).await {
            return;
        }
        match translator::translate_via_claude(ai_client, track, gender).await {
            Ok(translations) => Self::apply_translations(track, translations),
            Err(e) => warn!("worker: Claude translation failed for {youtube_id}: {e}"),
        }
    }

    /// #144: `e` is Claude's content filter refusing this song's translation
    /// (`ai::retry::content_filtered`)? Then it is final — the same text is
    /// refused again and every refusal costs the proxy's 60 s cooldown — so
    /// the song keeps its English lines, is skipped by the missing pass for
    /// the rest of this run, and no backoff holds the next song (one WARN).
    pub(crate) async fn translation_refused(&self, youtube_id: &str, e: &anyhow::Error) -> bool {
        if !crate::ai::retry::content_filtered(e) {
            return false;
        }
        warn!(
            youtube_id,
            "lyrics_worker: Claude's content filter refused this song's translation — it keeps its English lines, the next song goes on"
        );
        let mut backoff = self.retry_backoff.lock().await;
        backoff.refused_translations.insert(youtube_id.to_string());
        true
    }

    /// #229 item C: the translation of `youtube_id` calls Claude — paid AI
    /// — so it runs only while this node's switch is on (`paid_ai::enabled`,
    /// read live); while it is off the song's translation is held
    /// (`paid_ai::hold`, one INFO per song): its SK lines come with a peer's
    /// copy, or once the switch is on. Asked only for a song the pass would
    /// translate (picked, its track read), so the status names no
    /// translation that is not waiting.
    pub(crate) async fn translation_allowed(&self, youtube_id: &str) -> bool {
        if crate::paid_ai::enabled(&self.pool).await {
            return true;
        }
        crate::paid_ai::hold(crate::paid_ai::Held::Translation, youtube_id);
        false
    }

    /// Resolve a video's translation gender (#152). `'f'` → Female; everything
    /// else (NULL / `'m'` / unknown) → Male, the catalog default.
    #[cfg_attr(test, mutants::skip)]
    pub(crate) async fn resolve_gender(&self, video_id: i64) -> SpeakerGender {
        match get_translation_gender(&self.pool, video_id).await {
            Ok(Some(g)) if g == "f" => SpeakerGender::Female,
            _ => SpeakerGender::Male,
        }
    }

    /// Re-translate ONE stale-translation song (SK produced under an older
    /// `LYRICS_TRANSLATION_VERSION`) under the current prompt + the song's
    /// gender, rewrite the `sk` lines in the persisted JSON, and stamp the
    /// version. Translation only — no alignment, no `lyrics_pipeline_version`
    /// change. Shares the Claude-translation backoff with
    /// `retry_missing_translations` so a refusing / rate-limited Claude is not
    /// hammered. Runs only when the priority queue is empty (lowest priority).
    #[cfg_attr(test, mutants::skip)]
    pub(crate) async fn retranslate_next_stale(&self) {
        let Some(ai_client) = &self.ai_client else {
            return;
        };
        {
            let backoff = self.retry_backoff.lock().await;
            if let Some(until) = backoff.silent_until
                && Instant::now() < until
            {
                return;
            }
        }
        let row = match fetch_next_stale_translation(&self.pool, LYRICS_TRANSLATION_VERSION).await {
            Ok(Some(r)) => r,
            _ => return,
        };
        let video_id = row.id;
        let youtube_id = row.youtube_id.clone();
        let lyrics_path = self.cache_dir.join(format!("{youtube_id}_lyrics.json"));
        let content = match tokio::fs::read_to_string(&lyrics_path).await {
            Ok(c) => c,
            Err(e) => {
                // has_lyrics=1 but no JSON on disk: stamp forward so we don't
                // spin on an unreadable row (nothing to re-translate).
                debug!("retranslate: read failed for {youtube_id}: {e}");
                let _ = stamp_translation_version(&self.pool, video_id, LYRICS_TRANSLATION_VERSION)
                    .await;
                return;
            }
        };
        let mut track: LyricsTrack = match serde_json::from_str(&content) {
            Ok(t) => t,
            Err(e) => {
                debug!("retranslate: parse failed for {youtube_id}: {e}");
                let _ = stamp_translation_version(&self.pool, video_id, LYRICS_TRANSLATION_VERSION)
                    .await;
                return;
            }
        };
        // #229 item C: asked once the track is read (review round 15): an
        // unreadable track is only stamped forward, never held.
        if !self.translation_allowed(&youtube_id).await {
            return;
        }
        let gender = self.resolve_gender(video_id).await;
        info!(
            "lyrics_worker: retranslating {youtube_id} (gender={gender:?}, v{LYRICS_TRANSLATION_VERSION})"
        );
        match translator::translate_via_claude(ai_client, &track, gender).await {
            Ok(translations) => {
                Self::apply_translations(&mut track, translations);
                let json = serde_json::to_vec(&track).unwrap_or_default();
                if let Err(e) = tokio::fs::write(&lyrics_path, &json).await {
                    warn!("retranslate: write failed for {youtube_id}: {e}");
                    return;
                }
                let _ = stamp_translation_version(&self.pool, video_id, LYRICS_TRANSLATION_VERSION)
                    .await;
                {
                    let mut backoff = self.retry_backoff.lock().await;
                    backoff.consecutive_failures = 0;
                    backoff.silent_until = None;
                }
                info!("lyrics_worker: retranslation done for {youtube_id}");
            }
            Err(e) if crate::ai::retry::content_filtered(&e) => {
                // #144: final — stamped forward like a done one, so the
                // stale pass moves on; no backoff.
                self.translation_refused(&youtube_id, &e).await;
                let _ = stamp_translation_version(&self.pool, video_id, LYRICS_TRANSLATION_VERSION)
                    .await;
            }
            Err(e) => {
                // Leave the version unstamped (retry after backoff). Mirrors
                // `retry_missing_translations`' exponential backoff so a
                // refusing / rate-limited Claude is not hammered every tick.
                debug!("lyrics_worker: retranslation failed for {youtube_id}: {e}");
                let mut backoff = self.retry_backoff.lock().await;
                backoff.consecutive_failures = backoff.consecutive_failures.saturating_add(1);
                let attempt_index = backoff.consecutive_failures.saturating_sub(1).min(4);
                let secs = 60u64.saturating_mul(1u64 << attempt_index).min(600);
                backoff.silent_until = Some(Instant::now() + Duration::from_secs(secs));
            }
        }
    }
}
