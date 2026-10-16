//! #280: the bare interjections the wall and the Presenter leave out.
//!
//! The base-tier transcript and Genius keep every sung `oh` / `yeah` / `hey`
//! as words, and the translator renders them literally (`Och`, `Áno`,
//! `Jeah`). The owner saw "One more time we say" + "Oh." shown as "Ešte raz
//! hovoríme Och." (16.10.2026). On SNV 1353 EN lines (4.1 %) of 349 songs
//! were only interjections (design 6223125533).
//!
//! The display plan reads each source line through [`shown`], after its
//! parenthesised asides went:
//!
//! - **The English line is the judge.** A line whose EN words are ALL
//!   interjections ([`EN`]) leaves the plan in both languages; the previous
//!   display line holds through its time by the plan's hold rules.
//! - **Edges.** A run of interjections at the start of a line goes when its
//!   last word carries a mark (`Yeah, let's dance.` → `Let's dance.`), and
//!   one at the end when the word before it does (`like you. Oh,` →
//!   `like you.`). At least [`MIN_KEPT`] words must stay, else the line
//!   stays whole (`Oh, God.`). So `Say whoa oh.` and `Oh Lord, you're
//!   beautiful` stay whole: no mark separates their interjections.
//! - **Slovak follows an English cut, never alone:** at most as many words at
//!   that edge, each from [`SK`], under the same rule. A Slovak `Áno` for an
//!   English "Yes" stays.
//!
//! Display-only: the stored JSON, the karaoke view and the pipeline are
//! untouched.

#[cfg(test)]
#[path = "display_plan_interjections_tests.rs"]
mod tests;
