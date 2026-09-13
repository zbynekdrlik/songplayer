//! Per-song SK translation gender + version queries (#152).
//!
//! Split out of `models.rs` to keep that file under the 1000-line airuleset
//! cap; re-exported from `models.rs` via `pub use models_translation::*;` so
//! call sites use `crate::db::models::set_translation_gender`, etc.
//!
//! Independent of `lyrics_pipeline_version` / alignment: setting a gender or
//! bumping `LYRICS_TRANSLATION_VERSION` triggers a translation-only re-pass
//! (one Claude call, `sk` lines rewritten in place), never re-alignment.

#[path = "models_tests_translation.rs"]
#[cfg(test)]
mod tests_translation;
