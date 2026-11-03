//! #289: the `audio_low_latency` switch's read, check and send.

use super::*;

#[test]
fn the_key_is_audio_low_latency_and_no_secret() {
    assert_eq!(SETTING_AUDIO_LOW_LATENCY, "audio_low_latency");
    assert!(!crate::config::is_secret_setting(SETTING_AUDIO_LOW_LATENCY));
}

#[test]
fn only_true_reads_on() {
    assert!(!low_latency(None), "unset = OFF");
    assert!(!low_latency(Some("")), "blank = OFF");
    assert!(!low_latency(Some("  ")));
    assert!(!low_latency(Some("false")));
    assert!(!low_latency(Some("yes")), "anything else = OFF");
    assert!(!low_latency(Some("1")));
    assert!(!low_latency(Some("truex")));
    assert!(low_latency(Some("true")));
    assert!(low_latency(Some(" TRUE\n")), "trimmed, any case");
}

#[test]
fn the_patch_takes_true_false_or_blank_only() {
    assert_eq!(
        checked(SETTING_AUDIO_LOW_LATENCY, "true"),
        Ok("true".into())
    );
    assert_eq!(
        checked(SETTING_AUDIO_LOW_LATENCY, " False "),
        Ok("false".into())
    );
    assert_eq!(checked(SETTING_AUDIO_LOW_LATENCY, ""), Ok(String::new()));
    assert_eq!(checked(SETTING_AUDIO_LOW_LATENCY, "  "), Ok(String::new()));
    for bad in ["1", "yes", "on", "truee"] {
        assert_eq!(
            checked(SETTING_AUDIO_LOW_LATENCY, bad),
            Err("audio_low_latency must be true or false".to_string()),
            "{bad}"
        );
    }
    assert_eq!(
        checked("gemini_model", " x "),
        Ok(" x ".into()),
        "other keys pass"
    );
}

#[test]
fn a_save_sends_the_switch_only_when_it_changed() {
    assert_eq!(low_latency_to_send(None, false), None);
    assert_eq!(low_latency_to_send(Some(""), false), None);
    assert_eq!(low_latency_to_send(None, true), Some("true".into()));
    assert_eq!(low_latency_to_send(Some("true"), true), None);
    assert_eq!(
        low_latency_to_send(Some("true"), false),
        Some("false".into())
    );
    assert_eq!(
        low_latency_to_send(Some("junk"), false),
        None,
        "junk reads OFF"
    );
}

#[test]
fn the_texts_name_the_latency_and_the_measurement() {
    assert_eq!(LABEL_SK, "Nízka latencia zvuku (~15 ms)");
    assert!(TOOLTIP_SK.contains("65 ms"));
    assert!(TOOLTIP_SK.contains("camera-box"));
}
