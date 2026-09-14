//! Mutation-killing unit tests for `g35t_client.rs` PURE helpers
//! (`truncate`, `parse_offset_ms`). The HTTP functions carry
//! `#[cfg_attr(test, mutants::skip)]` and are deliberately not touched here.
//!
//! Wired into `g35t_client.rs` as a sibling `#[path]` test module.

use super::*;

// -------------------------------------------------------------------------
// truncate — line 74: `fn truncate(s, max) -> &str`
// -------------------------------------------------------------------------

/// Kills both `74:5` body replacements (`-> ""` and `-> "xyzzy"`). The real
/// helper returns the first `max` chars: `truncate("hello", 3)` == "hel",
/// and a `max` past the end returns the whole string. Neither equals "" nor
/// "xyzzy".
#[test]
fn truncate_returns_prefix_not_body_replacement() {
    assert_eq!(truncate("hello", 3), "hel");
    assert_eq!(truncate("hello", 10), "hello");
}

// -------------------------------------------------------------------------
// parse_offset_ms — line 403: `seconds < 0.0`
// -------------------------------------------------------------------------

/// Kills `403:40 < -> <=`. A zero offset "0s" parses to 0.0 seconds; the
/// unmutated `0.0 < 0.0` is false so it is accepted → `Some(0)`. Under `<=`,
/// `0.0 <= 0.0` is true → the offset is rejected → `None`.
#[test]
fn parse_offset_ms_zero_seconds_is_some_zero() {
    assert_eq!(parse_offset_ms("0s"), Some(0));
}

/// Supporting pins (not the specific `<`/`<=` killers, but they lock the
/// surrounding contract so the zero-boundary test reads unambiguously): a
/// positive offset scales to whole ms, a negative one is rejected.
#[test]
fn parse_offset_ms_positive_and_negative_contract() {
    assert_eq!(parse_offset_ms("5.2s"), Some(5200));
    assert_eq!(parse_offset_ms("-1s"), None);
}
