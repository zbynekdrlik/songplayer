//! #230: the background hold's pure decisions and its health segment.

use super::*;

#[test]
fn the_hold_scene_holds_and_the_release_scene_releases_any_case() {
    assert_eq!(press_of(Some("sp-90s")), Press::Hold);
    assert_eq!(press_of(Some("SP-90s")), Press::Hold);
    assert_eq!(press_of(Some(" sp-90s ")), Press::Hold);
    assert_eq!(press_of(Some("sp-slow")), Press::Release);
    assert_eq!(press_of(Some("SP-Slow")), Press::Release);
}

#[test]
fn any_other_scene_or_none_does_nothing() {
    assert_eq!(press_of(Some("sp-fast")), Press::Other);
    assert_eq!(press_of(Some("Blank")), Press::Other);
    assert_eq!(press_of(Some("sp-90s-2")), Press::Other);
    assert_eq!(press_of(Some("")), Press::Other);
    assert_eq!(press_of(None), Press::Other);
}

#[test]
fn the_constants_are_the_owners_rule() {
    assert_eq!(HOLD_SCENE, "sp-90s");
    assert_eq!(RELEASE_SCENE, "sp-slow");
    assert_eq!(HOLD_FOR_S, 4 * 60 * 60);
    assert_eq!(SETTING_BACKGROUND_HOLD_UNTIL, "background_hold_until_ms");
}

#[test]
fn a_stored_end_is_a_positive_whole_number_of_ms() {
    assert_eq!(parse_until(Some("1791608681366")), Some(1_791_608_681_366));
    assert_eq!(parse_until(Some(" 123 ")), Some(123));
    assert_eq!(parse_until(Some("1")), Some(1));
    assert_eq!(parse_until(Some("0")), None);
    assert_eq!(parse_until(Some("-5")), None);
    assert_eq!(parse_until(Some("soon")), None);
    assert_eq!(parse_until(Some("")), None);
    assert_eq!(parse_until(None), None);
}

#[test]
fn held_until_the_end_and_free_at_it() {
    assert!(held_at(Some(10_000), 0));
    assert!(held_at(Some(10_000), 9_999));
    assert!(!held_at(Some(10_000), 10_000));
    assert!(!held_at(Some(10_000), 10_001));
    assert!(!held_at(None, 0));
}

#[test]
fn remaining_seconds_round_up_and_are_zero_when_free() {
    let until = 1_000_000;
    assert_eq!(remaining_s(Some(until), until - 14_400_000), 14_400);
    assert_eq!(remaining_s(Some(until), until - 1_001), 2);
    assert_eq!(remaining_s(Some(until), until - 1_000), 1);
    assert_eq!(remaining_s(Some(until), until - 1), 1);
    assert_eq!(remaining_s(Some(until), until), 0);
    assert_eq!(remaining_s(Some(until), until + 5_000), 0);
    assert_eq!(remaining_s(None, until), 0);
}

fn held(remaining_s: u64, held_jobs: &[&str]) -> BackgroundHold {
    BackgroundHold {
        held: true,
        until_utc_ms: Some(1_791_608_681_366),
        remaining_s,
        hold_scene: HOLD_SCENE.to_string(),
        release_scene: RELEASE_SCENE.to_string(),
        held_jobs: held_jobs.iter().map(|j| j.to_string()).collect(),
    }
}

#[test]
fn no_segment_while_not_held() {
    assert_eq!(label(&BackgroundHold::default()), None);
    let free = BackgroundHold {
        held: false,
        ..held(0, &["download"])
    };
    assert_eq!(label(&free), None);
}

#[test]
fn the_segment_names_the_time_left_in_whole_minutes() {
    let text = |s| label(&held(s, &[])).unwrap().1;
    assert_eq!(text(14_400), "Pozadie: pozastavené (ešte 4 h)");
    assert_eq!(text(14_280), "Pozadie: pozastavené (ešte 3 h 58 min)");
    assert_eq!(text(14_221), "Pozadie: pozastavené (ešte 3 h 58 min)");
    assert_eq!(text(3_601), "Pozadie: pozastavené (ešte 1 h 1 min)");
    assert_eq!(text(3_600), "Pozadie: pozastavené (ešte 1 h)");
    assert_eq!(text(720), "Pozadie: pozastavené (ešte 12 min)");
    assert_eq!(text(61), "Pozadie: pozastavené (ešte 2 min)");
    assert_eq!(text(60), "Pozadie: pozastavené (ešte 1 min)");
    assert_eq!(text(1), "Pozadie: pozastavené (ešte 1 min)");
}

#[test]
fn the_segment_is_amber_and_its_tooltip_names_trigger_release_and_what_waits() {
    let (tone, _, tip) = label(&held(14_400, &[])).unwrap();
    assert_eq!(tone, HealthTone::Warn);
    assert_eq!(
        tip,
        "Na programe bola scéna sp-90s: nové sťahovanie a spracovanie na pozadí sa nespustí, \
         kým na program nepôjde sp-slow alebo neuplynú 4 h (rozbehnutá práca dobehne). \
         Zatiaľ nič nečaká."
    );
    let (_, _, tip) = label(&held(14_400, &["download", "lyrics", "peer", "later"])).unwrap();
    assert!(
        tip.ends_with("Čaká: sťahovanie, texty, výmena so susedným uzlom, later."),
        "{tip}"
    );
}

#[test]
fn every_job_has_its_slovak_name() {
    let names = [
        ("sync", "synchronizácia playlistov"),
        ("download", "sťahovanie"),
        ("lyrics", "texty"),
        ("stems", "stopy"),
        ("dub", "dabing"),
        ("metadata", "oprava názvov"),
        ("peer", "výmena so susedným uzlom"),
        ("ytdlp_update", "aktualizácia yt-dlp"),
        ("video_upgrade", "vylepšenie videí na 4K"),
    ];
    for (job, sk) in names {
        assert_eq!(job_sk(job), sk);
    }
    assert_eq!(job_sk("other"), "other");
}

#[test]
fn the_status_round_trips_through_json() {
    let h = held(5, &["stems"]);
    let json = serde_json::to_string(&h).unwrap();
    assert_eq!(serde_json::from_str::<BackgroundHold>(&json).unwrap(), h);
    let v: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(v["held"], true);
    assert_eq!(v["until_utc_ms"], 1_791_608_681_366_i64);
    assert_eq!(v["remaining_s"], 5);
    assert_eq!(v["hold_scene"], "sp-90s");
    assert_eq!(v["release_scene"], "sp-slow");
    assert_eq!(v["held_jobs"][0], "stems");
}
