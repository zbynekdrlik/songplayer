//! A query parameter that is a whole count in `1..=max`: the audio fault
//! ledger's `hours` (#257) and the usage summary's `days` (#256) are read
//! by this one parse. Digits only (no sign, no space, no decimal point), so
//! what is refused never depends on what Rust's `parse` happens to accept
//! (`"+5"`), and a refusal never needs to repeat the value sent.

/// `raw` as a whole number in `1..=max`; `default` when the parameter is
/// absent; `None` for anything else.
pub fn whole_count(raw: Option<&str>, default: u32, max: u32) -> Option<u32> {
    let Some(text) = raw else {
        return Some(default);
    };
    if !text.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    text.parse::<u32>().ok().filter(|n| (1..=max).contains(n))
}

#[cfg(test)]
#[path = "query_param_tests.rs"]
mod tests;
