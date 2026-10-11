//! #223 S13: the upgrade's status line and the cap's labels.

use super::*;

#[test]
fn the_status_reads_from_the_routes_answer() {
    let view: VideoUpgradeView = serde_json::from_str(
        r#"{"enabled":true,"cap":2160,"pending":339,"upgraded":3,"no_better":4,
            "refused":0,"failed":0,"busy":0,"rolled_back":0,"paused_until_ms":null,
            "waiting":null,"last":{"youtube_id":"x","outcome":"no_better"}}"#,
    )
    .unwrap();
    assert_eq!(
        view,
        VideoUpgradeView {
            enabled: true,
            cap: 2160,
            pending: 339,
            upgraded: 3,
            no_better: 4,
            ..VideoUpgradeView::default()
        }
    );
    assert_eq!(
        view.summary_sk(),
        "vylepšené 3 · bez vyššej kvality 4 · čaká 339"
    );
}

#[test]
fn the_other_counts_show_only_when_not_zero_then_why_it_waits() {
    let view = VideoUpgradeView {
        upgraded: 1,
        no_better: 2,
        pending: 3,
        refused: 4,
        failed: 5,
        busy: 6,
        rolled_back: 7,
        waiting: Some("held".into()),
        ..VideoUpgradeView::default()
    };
    assert_eq!(
        view.summary_sk(),
        "vylepšené 1 · bez vyššej kvality 2 · čaká 3 · odmietnuté 4 · chyba 5 · \
         obsadené prehrávaním 6 · vrátené späť 7 — pozastavené pred bohoslužbou (sp-90s)"
    );
}

#[test]
fn every_upgrade_wait_has_its_slovak_reason() {
    let reasons = [
        ("off", "vypnuté"),
        ("held", "pozastavené pred bohoslužbou (sp-90s)"),
        ("download_due", "najprv sťahuje nové piesne"),
        ("paused", "pozastavené: YouTube žiada overenie"),
        ("spacing", "medzi dvoma videami čaká 2 minúty"),
        ("low_disk", "málo miesta na disku (pod 50 GiB)"),
        ("no_disk_reading", "nevie zistiť voľné miesto na disku"),
        ("no_tools", "čaká na nástroje (yt-dlp)"),
        ("nothing_to_do", "všetky videá sú skontrolované"),
    ];
    for (waiting, sk) in reasons {
        assert_eq!(waiting_sk(waiting), sk);
    }
    assert_eq!(waiting_sk("new_reason"), "new_reason");
}

#[test]
fn every_cap_choice_has_its_label() {
    assert_eq!(
        cap_label(""),
        "Automaticky (4K s dekódovaním na GPU, inak 1440)"
    );
    assert_eq!(cap_label("2160"), "2160 (4K)");
    for plain in ["1440", "1080", "720"] {
        assert_eq!(cap_label(plain), plain);
    }
    assert_eq!(cap_label("1800"), "1800 (vlastné)");
}
