//! #223 S9a: the format selector and the `max_resolution` cap (`format.rs`).

use super::*;

const DASH: &str = "[dynamic_range=SDR][protocol=https][vcodec!^=avc1]";
const HLS: &str = "[dynamic_range=SDR][protocol*=m3u8][vcodec^=avc1]";

/// At 1440: the 1440, 1080 and 720 tiers (DASH before HLS in each), then
/// the four with no lower bound — exactly the selector checked through the
/// box's yt-dlp (#223 comment 6099757719).
#[test]
fn the_selector_walks_the_tiers_from_the_cap_down() {
    let expected = [
        format!("bv*[height<=1440][height>=1440]{DASH}"),
        format!("bv*[height<=1440][height>=1440]{HLS}"),
        format!("bv*[height<=1440][height>=1080]{DASH}"),
        format!("bv*[height<=1440][height>=1080]{HLS}"),
        format!("bv*[height<=1440][height>=720]{DASH}"),
        format!("bv*[height<=1440][height>=720]{HLS}"),
        format!("bv*[height<=1440]{DASH}"),
        format!("bv*[height<=1440]{HLS}"),
        "bv*[height<=1440][dynamic_range=SDR]".to_string(),
        "bv*[height<=1440]".to_string(),
    ]
    .join("/");
    assert_eq!(format_spec(1440), expected);
}

/// THE DEEP (`xrhVLX6vwPk`): VP9 only at 360p, H.264 1080p over HLS. The
/// 1080 tier's HLS alternative comes before any alternative with no lower
/// bound, so its 360p VP9 never wins (D8's untiered selector picked it).
#[test]
fn a_higher_h264_over_hls_beats_a_lower_vp9() {
    let spec = format_spec(1440);
    let hls_1080 = spec
        .find(&format!("[height>=1080]{HLS}"))
        .expect("the 1080 tier's HLS alternative");
    let dash_any = spec
        .find(&format!("bv*[height<=1440]{DASH}"))
        .expect("the DASH alternative with no floor");
    assert!(hls_1080 < dash_any, "{spec}");
}

#[test]
fn only_the_tiers_under_the_cap_are_walked() {
    assert!(format_spec(2160).starts_with(&format!("bv*[height<=2160][height>=2160]{DASH}/")));
    let at_1080 = format_spec(1080);
    assert!(!at_1080.contains("[height>=1440]") && !at_1080.contains("[height>=2160]"));
    assert!(at_1080.starts_with(&format!("bv*[height<=1080][height>=1080]{DASH}/")));
    let at_480 = format_spec(480);
    assert!(!at_480.contains("[height>="), "{at_480}");
    assert_eq!(at_480.split('/').count(), 4, "{at_480}");
}

#[test]
fn h264_is_only_ever_asked_over_hls() {
    for alternative in format_spec(2160).split('/') {
        if alternative.contains("vcodec^=avc1") {
            assert!(alternative.contains("protocol*=m3u8"), "{alternative}");
        }
    }
}

#[test]
fn the_cap_is_the_setting_clamped_or_1440() {
    assert_eq!(max_resolution(None), 1440);
    assert_eq!(max_resolution(Some("")), 1440);
    assert_eq!(max_resolution(Some("abc")), 1440);
    assert_eq!(max_resolution(Some("2160")), 2160);
    assert_eq!(max_resolution(Some(" 1080 ")), 1080);
    assert_eq!(max_resolution(Some("4320")), 2160);
    assert_eq!(max_resolution(Some("100")), 480);
    assert_eq!(max_resolution(Some("480")), 480);
}

#[test]
fn a_patch_of_max_resolution_takes_480_to_2160_or_empty() {
    assert_eq!(checked("max_resolution", " 2160 "), Ok("2160".to_string()));
    assert_eq!(checked("max_resolution", "480"), Ok("480".to_string()));
    assert_eq!(checked("max_resolution", ""), Ok(String::new()));
    for refused in ["479", "2161", "abc", "-1", "1440.5"] {
        let reason = checked("max_resolution", refused).unwrap_err();
        assert!(
            reason.contains("480") && reason.contains("2160"),
            "{reason}"
        );
    }
    assert_eq!(checked("gemini_model", "abc"), Ok("abc".to_string()));
}
