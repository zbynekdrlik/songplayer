//! Drop "phantom clusters" — short sequences of low-confidence words that
//! WhisperX's language model emits as filler during sustained vocals or
//! instrumental gaps. These look like real transcribed lyrics but they were
//! hallucinated, not heard.
//!
//! Universal acoustic-only signature (no song-specific tuning, no lyric-text
//! inspection):
//!
//! - cluster length ≥ 2 words
//! - every cluster word's confidence < 0.75 (a single high-conf word breaks
//!   the cluster — protects real phrases that contain a single low-conf
//!   hiccup)
//! - gap from previous real word > 700 ms (cluster starts after a clear
//!   silence — not part of an ongoing sung phrase)
//! - gap to next real word > 3000 ms (cluster is followed by a clear
//!   silence — not a brief pause inside a sung phrase)
//! - cluster average confidence < 0.70
//!
//! Both-sided silence is the key. A real sung phrase pulls neighbouring
//! words within ~300 ms, so a cluster floating in 700/3000 ms gulfs is
//! almost certainly an LM-fallback artifact. The avg-confidence cap rules
//! out clean monosyllabic sung interjections (which usually score >0.85).
//!
//! id=132 2:57 evidence (Holy Forever, Chris Tomlin):
//!   real sustained 'holy' end=175711, gap 1121 ms,
//!   "Cause"(0.687)+"your"(0.483)+"name"(0.604) avg=0.591,
//!   gap 7491 ms to next real word at 186505.
//! All five conditions met → cluster dropped. The merger no longer matches
//! these phantom tokens to "Your name is the highest" prefix, so the wall
//! stays on "Holy forever" through the sustained note.

use super::AsrWord;

const GAP_BEFORE_MIN_MS: u32 = 700;
const GAP_AFTER_MIN_MS: u32 = 3000;
const MIN_CLUSTER_LEN: usize = 2;
/// Every cluster word must be below this — a single high-conf word breaks
/// the cluster and prevents dropping a real phrase that happens to contain
/// one low-conf hiccup.
const MAX_WORD_CONF: f32 = 0.75;
/// Cluster's average confidence must be below this. Tighter than per-word
/// to leave headroom for borderline real words.
const MAX_AVG_CONF: f32 = 0.70;

pub(super) fn drop_phantom_clusters(words: &mut Vec<AsrWord>) {
    if words.len() < MIN_CLUSTER_LEN {
        return;
    }

    let mut to_drop: Vec<bool> = vec![false; words.len()];
    let mut clusters_dropped: u32 = 0;
    let mut words_dropped: u32 = 0;

    let mut i = 0;
    while i < words.len() {
        // A phantom cluster starts at a low-confidence word that follows a
        // clear silence (>700 ms) — i.e. the first word in a new run after
        // an audible gap.
        if words[i].confidence >= MAX_WORD_CONF {
            i += 1;
            continue;
        }
        let gap_before = if i == 0 {
            u32::MAX // song start — count as silence
        } else {
            words[i].start_ms.saturating_sub(words[i - 1].end_ms)
        };
        if gap_before < GAP_BEFORE_MIN_MS {
            i += 1;
            continue;
        }

        // Cluster extends as long as words stay LOW-confidence AND inner
        // gaps stay tight. The first high-confidence word OR first wide
        // inner gap ends the cluster.
        let mut j = i + 1;
        while j < words.len() {
            if words[j].confidence >= MAX_WORD_CONF {
                break;
            }
            let inner_gap = words[j].start_ms.saturating_sub(words[j - 1].end_ms);
            if inner_gap >= GAP_BEFORE_MIN_MS {
                break;
            }
            j += 1;
        }
        let len = j - i;
        if len < MIN_CLUSTER_LEN {
            i = j.max(i + 1);
            continue;
        }

        let gap_after = if j == words.len() {
            u32::MAX
        } else {
            words[j].start_ms.saturating_sub(words[j - 1].end_ms)
        };
        if gap_after < GAP_AFTER_MIN_MS {
            i = j;
            continue;
        }

        let sum: f32 = words[i..j].iter().map(|w| w.confidence).sum();
        let avg = sum / (len as f32);
        if avg >= MAX_AVG_CONF {
            i = j;
            continue;
        }

        for k in i..j {
            to_drop[k] = true;
        }
        clusters_dropped += 1;
        words_dropped += len as u32;
        tracing::debug!(
            cluster_start_ms = words[i].start_ms,
            cluster_end_ms = words[j - 1].end_ms,
            len,
            avg_conf = avg,
            gap_before_ms = gap_before,
            gap_after_ms = gap_after,
            "description_merge: dropping phantom cluster"
        );
        i = j;
    }

    let mut k = 0;
    words.retain(|_| {
        let keep = !to_drop[k];
        k += 1;
        keep
    });

    if clusters_dropped > 0 {
        tracing::info!(
            clusters_dropped,
            words_dropped,
            "description_merge: phantom-cluster filter active"
        );
    }
}
