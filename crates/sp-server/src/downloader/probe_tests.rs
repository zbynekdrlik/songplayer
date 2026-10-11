//! Tests for the YouTube probe (#232): its arguments and its report.

use std::path::Path;

use super::*;

#[test]
fn the_probe_resolves_with_the_downloads_selector_and_prints_the_format() {
    let args = probe_args(
        "bv*[height<=1440]",
        Some(Path::new("/data/cookies.txt")),
        "https://www.youtube.com/watch?v=gq-4FVRr_ow",
    );
    let args: Vec<&str> = args.iter().map(|a| a.to_str().unwrap()).collect();
    assert_eq!(
        args,
        [
            "-f",
            "bv*[height<=1440]",
            "--socket-timeout",
            "30",
            "--print",
            "SPFMT|%(format_id)s|%(vcodec)s|%(width)s|%(height)s|%(fps)s",
            "--cookies",
            "/data/cookies.txt",
            "https://www.youtube.com/watch?v=gq-4FVRr_ow",
        ]
    );
    // The video stage only simulates: never the download's `after_move:`,
    // which would download. Same fields as the download's line.
    assert!(!format::FORMAT_PROBE_PRINT.starts_with("after_move:"));
    assert_eq!(
        format::FORMAT_PRINT.strip_prefix("after_move:"),
        Some(format::FORMAT_PROBE_PRINT)
    );
}

#[test]
fn with_no_cookie_file_the_url_follows_the_print() {
    let args = probe_args("spec", None, "url");
    let args: Vec<&str> = args.iter().map(|a| a.to_str().unwrap()).collect();
    assert_eq!(
        args[args.len() - 2..],
        [
            "SPFMT|%(format_id)s|%(vcodec)s|%(width)s|%(height)s|%(fps)s",
            "url"
        ]
    );
}

/// SNV's answer of 10.10.2026.
#[test]
fn a_resolved_format_is_ok() {
    let r = report(
        "gq-4FVRr_ow",
        1440,
        true,
        true,
        "SPFMT|399|av01.0.08M.08|1920|1080|30\n",
        "",
        2643,
    );
    assert_eq!(
        r,
        YoutubeProbeReport {
            ok: true,
            youtube_id: "gq-4FVRr_ow".into(),
            cap: 1440,
            cookies: true,
            format: Some(DownloadedFormat {
                format_id: "399".into(),
                codec: Some("av01.0.08M.08".into()),
                width: Some(1920),
                height: Some(1080),
                fps: Some(30.0),
            }),
            error: None,
            elapsed_ms: 2643,
        }
    );
}

/// A failed yt-dlp names its last `ERROR:` line (the bot check), not a
/// later warning.
#[test]
fn a_failed_probe_names_yt_dlps_last_error_line() {
    let stderr = "WARNING: [youtube] cookies are no longer valid\n\
                  ERROR: [youtube] gq-4FVRr_ow: Sign in to confirm you're not a bot\n\
                  WARNING: trailing\n";
    let r = report("gq-4FVRr_ow", 1440, true, false, "", stderr, 900);
    assert!(!r.ok);
    assert_eq!(r.format, None);
    assert_eq!(
        r.error.as_deref(),
        Some("ERROR: [youtube] gq-4FVRr_ow: Sign in to confirm you're not a bot")
    );
}

#[test]
fn a_failure_with_no_error_line_names_its_last_line_or_says_so() {
    let r = report("x", 720, false, false, "", "\nsomething broke\n\n", 1);
    assert_eq!(r.error.as_deref(), Some("something broke"));
    assert!(!r.cookies);
    let silent = report("x", 720, false, false, "", "  \n", 1);
    assert_eq!(
        silent.error.as_deref(),
        Some("yt-dlp failed with no message")
    );
}

/// A long error is cut at 400 characters (on a char boundary).
#[test]
fn a_long_error_is_cut() {
    let stderr = format!("ERROR: {}", "é".repeat(500));
    let r = report("x", 720, true, false, "", &stderr, 1);
    assert_eq!(r.error.unwrap().chars().count(), 400);
}

/// yt-dlp exited 0 but printed no format line: not ok.
#[test]
fn an_exit_with_no_format_line_is_no_format() {
    let r = report("x", 1440, true, true, "something else\n", "", 5);
    assert!(!r.ok);
    assert_eq!(r.error.as_deref(), Some("yt-dlp printed no format line"));
}

#[test]
fn a_refused_probe_carries_its_reason() {
    let r = refused("x", 1440, true, "yt-dlp is not ready yet".into());
    assert!(!r.ok && r.format.is_none() && r.elapsed_ms == 0);
    assert_eq!(r.error.as_deref(), Some("yt-dlp is not ready yet"));
}

#[test]
fn the_report_serializes_for_the_gate() {
    let r = report("x", 1440, true, true, "SPFMT|399|NA|1920|1080|30", "", 7);
    let v = serde_json::to_value(&r).unwrap();
    assert_eq!(v["format"]["height"], 1080);
    assert_eq!(v["format"]["codec"], serde_json::Value::Null);
    assert_eq!(v["ok"], true);
}
