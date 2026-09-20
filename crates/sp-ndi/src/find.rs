//! #196: read each sender's advertised `host:port` via the NDI SDK's own
//! discovery (`NDIlib_find`), not the sender-side `NDIlib_send_get_source_name`
//! (whose `p_url_address` is EMPTY for a local sender — the round-1 finding).
//!
//! A finder discovers ALL local NDI sources (`show_local_sources = true`) and
//! hands back, per source, the `p_ndi_name` (`"RESOLUME-SNV (SP-slow)"`) and
//! the `p_url_address` (`"10.77.9.201:5963"`) a DistroAV receiver reconnects to.
//! We open ONE finder after the startup senders exist, poll briefly until every
//! own name appears, record the `host:port` per output, and destroy it.
//!
//! This module holds the PURE name→URL matching (Linux-unit-tested with no NDI
//! runtime); the FFI driver that opens/polls/destroys a real finder lives on
//! `RealNdiBackend` (`sender_real.rs`), exercised only on the box.

use crate::source_url::parse_source_url;

/// Does a discovered NDI source `discovered_name` belong to our sender created
/// under the bare `own_bare` name (e.g. `"SP-slow"`)? NDI advertises a local
/// sender as `"<MACHINE> (<stream>)"`, so a discovered name matches when it is
/// exactly the bare name OR ends with `"(<bare>)"` — the machine-prefixed form.
/// Pure so the prefix / suffix / near-miss cases are unit-tested.
pub fn source_matches(discovered_name: &str, own_bare: &str) -> bool {
    let d = discovered_name.trim();
    let n = own_bare.trim();
    if n.is_empty() {
        return false;
    }
    d == n || d.ends_with(&format!("({n})"))
}

/// Match discovered `(name, url)` sources to our outputs `(playlist_id, bare
/// NDI name)` and return `(playlist_id, canonical host:port)` for every output
/// whose sender was discovered with a parseable URL. Outputs with no matching
/// source, or a source whose URL does not parse, are omitted (the caller logs a
/// WARN for those). First matching source wins. Pure — unit-tested on Linux.
pub fn match_source_urls(
    discovered: &[(String, String)],
    own: &[(i64, String)],
) -> Vec<(i64, String)> {
    let mut out = Vec::new();
    for (id, name) in own {
        for (dname, durl) in discovered {
            if source_matches(dname, name) {
                if let Some(url) = parse_source_url(durl) {
                    out.push((*id, url));
                }
                break;
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_exact_bare_name() {
        assert!(source_matches("SP-slow", "SP-slow"));
    }

    #[test]
    fn matches_machine_prefixed_advertised_name() {
        assert!(source_matches("RESOLUME-SNV (SP-slow)", "SP-slow"));
    }

    #[test]
    fn does_not_match_different_stream() {
        assert!(!source_matches("RESOLUME-SNV (SP-fast)", "SP-slow"));
    }

    #[test]
    fn does_not_match_prefix_of_a_longer_stream_name() {
        // "SP-slow" must NOT match a sender named "SP-slowmo".
        assert!(!source_matches("RESOLUME-SNV (SP-slowmo)", "SP-slow"));
        assert!(!source_matches("RESOLUME-SNV (SP-slow2)", "SP-slow"));
    }

    #[test]
    fn empty_own_name_never_matches() {
        // Guards the upstream-filtered empty-name case; a source literally named
        // "M ()" must not match an empty own name.
        assert!(!source_matches("M ()", ""));
        assert!(!source_matches("", ""));
    }

    #[test]
    fn surrounding_whitespace_tolerated() {
        assert!(source_matches("  RESOLUME-SNV (SP-slow)  ", " SP-slow "));
    }

    #[test]
    fn match_maps_ids_to_parsed_urls() {
        let discovered = vec![
            (
                "RESOLUME-SNV (SP-slow)".to_string(),
                "10.77.9.201:5963".to_string(),
            ),
            (
                "RESOLUME-SNV (SP-fast)".to_string(),
                "10.77.9.201:5964".to_string(),
            ),
        ];
        let own = vec![(4, "SP-slow".to_string()), (7, "SP-fast".to_string())];
        assert_eq!(
            match_source_urls(&discovered, &own),
            vec![
                (4, "10.77.9.201:5963".to_string()),
                (7, "10.77.9.201:5964".to_string()),
            ]
        );
    }

    #[test]
    fn match_omits_undiscovered_output() {
        let discovered = vec![(
            "RESOLUME-SNV (SP-slow)".to_string(),
            "10.77.9.201:5963".to_string(),
        )];
        let own = vec![(4, "SP-slow".to_string()), (9, "SP-alex".to_string())];
        // SP-alex was not discovered → omitted (its id must not appear).
        assert_eq!(
            match_source_urls(&discovered, &own),
            vec![(4, "10.77.9.201:5963".to_string())]
        );
    }

    #[test]
    fn match_omits_output_with_unparseable_url() {
        let discovered = vec![(
            "RESOLUME-SNV (SP-slow)".to_string(),
            "not-a-url".to_string(),
        )];
        let own = vec![(4, "SP-slow".to_string())];
        assert!(match_source_urls(&discovered, &own).is_empty());
    }

    #[test]
    fn match_strips_scheme_and_path_from_url() {
        let discovered = vec![(
            "RESOLUME-SNV (SP-slow)".to_string(),
            "ndi://10.77.9.201:5970/stream".to_string(),
        )];
        let own = vec![(4, "SP-slow".to_string())];
        assert_eq!(
            match_source_urls(&discovered, &own),
            vec![(4, "10.77.9.201:5970".to_string())]
        );
    }
}
