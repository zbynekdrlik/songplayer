//! ASR alignment path — runs AssemblyAI U3-Pro + Claude-merge for songs
//! whose `gather_sources` returns ONLY untimed text candidates (genius,
//! lrclib-untimed, etc.). See
//! `docs/superpowers/specs/2026-05-19-asr-path-aai-claude-merge-design.md`.

pub mod aai_backend;
pub mod claude_merge;
pub mod fallback;
pub mod merge_prompt;
pub mod resolver;

/// DB settings key that stores the AssemblyAI API token. Read per-song in
/// the worker so operators can configure without a restart. Same pattern
/// as `replicate_api_token`.
pub const ASSEMBLYAI_API_KEY_SETTING: &str = "assemblyai_api_key";

#[cfg(test)]
#[path = "tests.rs"]
mod tests;
