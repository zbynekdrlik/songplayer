//! Parser for Gemini's `(MM:SS.x --> MM:SS.x) text` timed-line output.

use regex::Regex;
use std::sync::OnceLock;

#[derive(Debug, Clone, PartialEq)]
pub struct ParsedLine {
    pub start_ms: u64,
    pub end_ms: u64,
    pub text: String,
}

fn timed_line_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"^\((\d{1,2}):(\d{1,2}(?:\.\d+)?)\s*-->\s*(\d{1,2}):(\d{1,2}(?:\.\d+)?)\)\s*(.+)$",
        )
        .expect("static regex compiles")
    })
}

/// Parse a Gemini response body into a list of timed lines.
/// Skips blank lines and comment lines (lines starting with `#`).
pub fn parse_timed_lines(raw: &str) -> Vec<ParsedLine> {
    let mut out = Vec::new();
    for line in raw.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some(caps) = timed_line_re().captures(line) else {
            continue;
        };
        let s_min: u64 = caps[1].parse().unwrap_or(0);
        let s_sec: f64 = caps[2].parse().unwrap_or(0.0);
        let e_min: u64 = caps[3].parse().unwrap_or(0);
        let e_sec: f64 = caps[4].parse().unwrap_or(0.0);
        let start_ms = (s_min * 60_000) + (s_sec * 1000.0) as u64;
        let end_ms = (e_min * 60_000) + (e_sec * 1000.0) as u64;
        let text = caps[5].trim().to_string();
        out.push(ParsedLine {
            start_ms,
            end_ms,
            text,
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_basic_lines() {
        let out = parse_timed_lines(
            "(00:17.2 --> 00:20.0) I could search all this world,\n\
             (00:20.3 --> 00:26.5) I still find there is no one like You\n",
        );
        assert_eq!(out.len(), 2);
        assert_eq!(
            out[0],
            ParsedLine {
                start_ms: 17_200,
                end_ms: 20_000,
                text: "I could search all this world,".into()
            }
        );
        assert_eq!(out[1].start_ms, 20_300);
        assert_eq!(out[1].end_ms, 26_500);
    }

    #[test]
    fn skips_no_vocals_sentinel() {
        let out = parse_timed_lines("# no vocals\n(00:05.0 --> 00:07.0) hi\n");
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].text, "hi");
    }

    #[test]
    fn skips_blank_lines() {
        let out = parse_timed_lines("\n\n(00:01.0 --> 00:02.0) x\n\n");
        assert_eq!(out.len(), 1);
    }

    #[test]
    fn ignores_lines_without_timing_format() {
        let out = parse_timed_lines(
            "Here are the lyrics:\n\
             (00:01.0 --> 00:02.0) valid line\n\
             ```\n\
             bare text without timing\n",
        );
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].text, "valid line");
    }

    #[test]
    fn handles_minutes_only_no_decimal_seconds() {
        let out = parse_timed_lines("(01:23 --> 01:25) no decimals\n");
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].start_ms, 83_000);
        assert_eq!(out[0].end_ms, 85_000);
    }

    #[test]
    fn trims_trailing_whitespace_from_text() {
        let out = parse_timed_lines("(00:01.0 --> 00:02.0) trailing spaces   \n");
        assert_eq!(out[0].text, "trailing spaces");
    }
}
