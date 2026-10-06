//! The bare stream name of an NDI source (#212's `input.stream`), a child
//! module of `ndi_input.rs`. Pure.
//!
//! #221 lane 3 moved it here from `obs/ndi_discovery.rs`, deleted with the
//! NDI source map of the per-playlist senders; the NDI input "OBS manuál"
//! is its one user left.

/// Extract the stream name from a full NDI source name.
///
/// The NDI SDK formats source names as `"<machine> (<stream>)"`, where
/// `<machine>` is the host that owns the sender and `<stream>` the name the
/// sender passed to `NDIlib_send_create`. If the input has no `" ("`
/// delimiter or does not end with `)`, it is returned verbatim.
///
/// Examples:
/// - `"RESOLUME-SNV (SP-program)"` → `"SP-program"`
/// - `"machine (name with spaces)"` → `"name with spaces"`
/// - `"SP-fast"` → `"SP-fast"` (no parentheses — return as-is)
/// - `"weird (inner (nested))"` → `"inner (nested)"` (first `(` wins)
pub(crate) fn extract_ndi_stream_name(full: &str) -> &str {
    // Find the first `" ("` delimiter; require the string to end with
    // `)`. Fall back to the raw name for any shape we don't recognise.
    if full.ends_with(')')
        && let Some(open) = full.find(" (")
    {
        let inner_start = open + 2;
        let inner_end = full.len() - 1;
        if inner_end > inner_start {
            return &full[inner_start..inner_end];
        }
    }
    full
}

#[cfg(test)]
mod tests {
    use super::extract_ndi_stream_name;

    #[test]
    fn extract_ndi_stream_name_strips_machine_prefix() {
        assert_eq!(extract_ndi_stream_name("CG-OBS (OBS manuál)"), "OBS manuál");
        assert_eq!(extract_ndi_stream_name("WIN-BOX (SP-warmup)"), "SP-warmup");
        assert_eq!(
            extract_ndi_stream_name("dev-machine-1 (stream with spaces)"),
            "stream with spaces"
        );
    }

    #[test]
    fn extract_ndi_stream_name_passes_through_bare_names() {
        assert_eq!(extract_ndi_stream_name("SP-fast"), "SP-fast");
        assert_eq!(extract_ndi_stream_name("no-parens"), "no-parens");
    }

    #[test]
    fn extract_ndi_stream_name_handles_empty_and_weird_inputs() {
        assert_eq!(extract_ndi_stream_name(""), "");
        // No space before the open paren → treat as opaque.
        assert_eq!(extract_ndi_stream_name("(just-parens)"), "(just-parens)");
        // find(" (") picks the FIRST ` (` so nested parens inside the
        // stream name are preserved.
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
        // One character inside is extracted (the `>` boundary).
        assert_eq!(extract_ndi_stream_name("m (x)"), "x");
    }
}
