//! #275: what Arena's composition says about SongPlayer's clip, over a
//! composition trimmed from SNV's (15.10.2026, Arena 7.28.1, the main
//! session's read-only box read: the real layer / clip / group names and
//! ids, connected states, bypassed and opacity values; 8 of SNV's 18
//! `SP-program-MAX` clips; every param object, its id and `valuetype`, and
//! the composition's `video` block derived for the test, `testdata/`), and
//! variants of it. The `master` faders the coordinator read on SNV
//! (15.10.2026) keep their real id and value: the composition's
//! (1792013401987, 1.0), layer 4 = "L# Fill" `FILL_A` (1792013449453, 1.0)
//! and the group "G# Kontent" (1792013455710, 0.96484375); every other
//! master is derived (owner id + 4) at 1.0.

use std::time::{Duration, Instant};

use serde_json::{Value, json};
use sp_core::wall_clip::{ClipState, ClipView};

use super::*;

/// The composition as read on SNV, 15.10.2026: two `SP-program-MAX` clips
/// connected in the two "L# Fill" layers of the group "G# Kontent", six
/// more disconnected, the NDI background connected.
fn snv() -> Value {
    serde_json::from_str(include_str!("testdata/composition-snv-2026-10-15.json")).unwrap()
}

/// The two "L# Fill" layers holding the connected SongPlayer clips.
const FILL_A: i64 = 1_683_218_731_210;
const FILL_B: i64 = 1_683_218_732_011;
/// Their two connected clips ("Yt music").
const YT_A: i64 = 1_791_369_575_545;
const YT_B: i64 = 1_791_369_575_524;
/// The group "G# Kontent" holding both layers.
const KONTENT: i64 = 1_681_395_963_138;
/// The composition's own opacity and master params.
const COMP_OPACITY: i64 = 1_792_013_451_639;
const COMP_MASTER: i64 = 1_792_013_401_987;
/// Real master ids the coordinator read on SNV (15.10.2026): layer 4
/// (`FILL_A`, at 1.0) and the group "G# Kontent" (at 0.96484375).
const FILL_A_MASTER: i64 = 1_792_013_449_453;
const KONTENT_MASTER: i64 = 1_792_013_455_710;

fn yt_music() -> ClipView {
    ClipView {
        layer: "L# Fill".to_string(),
        clip: "Yt music".to_string(),
        source: "SP-program-MAX".to_string(),
    }
}

fn snv_bg() -> ClipView {
    ClipView {
        layer: "L # BG".to_string(),
        clip: "SNV BG".to_string(),
        source: "NDI Source".to_string(),
    }
}

/// The layer, group or clip with `id` (anywhere in the body).
fn item(body: &mut Value, id: i64) -> &mut Value {
    for list in ["layers", "layergroups"] {
        let entries = body[list].as_array().map_or(0, Vec::len);
        for i in 0..entries {
            if body[list][i]["id"] == id {
                return &mut body[list][i];
            }
            let clips = body[list][i]["clips"].as_array().map_or(0, Vec::len);
            for j in 0..clips {
                if body[list][i]["clips"][j]["id"] == id {
                    return &mut body[list][i]["clips"][j];
                }
            }
        }
    }
    panic!("no item {id} in the composition")
}

/// What the body says.
fn eval(body: &Value) -> Fact {
    let read = track(body);
    evaluate(&read.tracked, &read.values)
}

fn running(clips: Vec<ClipView>, ndi: Vec<ClipView>) -> Fact {
    Fact {
        state: ClipState::Running,
        clips,
        ndi_clips: ndi,
        why: None,
        reader_fault: false,
    }
}

fn none(ndi: Vec<ClipView>) -> Fact {
    Fact {
        state: ClipState::NotRunning,
        clips: Vec::new(),
        ndi_clips: ndi,
        why: None,
        reader_fault: false,
    }
}

#[test]
fn snv_runs_two_songplayer_clips_over_its_ndi_background() {
    let read = track(&snv());
    assert_eq!(
        evaluate(&read.tracked, &read.values),
        running(vec![yt_music(), yt_music()], vec![snv_bg()])
    );
    // 9 clips watched (8 SongPlayer, 1 NDI): their connected, their 4
    // layers' and 2 groups' bypassed + opacity + master, the composition's
    // opacity + master.
    let Tracked::Watched(watched) = &read.tracked else {
        panic!("{:?}", read.tracked);
    };
    assert_eq!(watched.clips.len(), 9);
    assert_eq!(watched.clips.iter().filter(|c| c.ndi).count(), 1);
    assert_eq!(watched.composition, vec![COMP_OPACITY, COMP_MASTER]);
    let params = read.tracked.params();
    assert_eq!(params.len(), 29);
    assert_eq!(
        params[0],
        KONTENT + 1,
        "id order: the group's bypassed first"
    );
    assert_eq!(params[28], KONTENT_MASTER);
    assert_eq!(read.values.len(), 29, "the body carries every value");
    let first = &watched.clips[0];
    assert_eq!(
        *first,
        TrackedClip {
            view: yt_music(),
            ndi: false,
            connected: YT_A + 1,
            layer: Visibility {
                bypassed: FILL_A + 1,
                opacity: FILL_A + 2,
                master: FILL_A_MASTER,
            },
            group: Some(Visibility {
                bypassed: KONTENT + 1,
                opacity: KONTENT + 2,
                master: KONTENT_MASTER,
            }),
        }
    );
    assert_eq!(
        read.values[&(YT_A + 1)],
        ParamValue::Text("Connected".to_string())
    );
    assert_eq!(read.values[&(FILL_A + 1)], ParamValue::Bool(false));
    assert_eq!(read.values[&(FILL_A + 2)], ParamValue::Number(1.0));
    // The group's master stands at 0.96484375 on SNV: it still shows.
    assert_eq!(
        read.values[&KONTENT_MASTER],
        ParamValue::Number(0.964_843_75)
    );
    assert_eq!(read.values[&COMP_MASTER], ParamValue::Number(1.0));
}

#[test]
fn after_a_relaunch_every_clip_is_disconnected_and_none_runs() {
    let mut body = snv();
    for layer in body["layers"].as_array_mut().unwrap() {
        for clip in layer["clips"].as_array_mut().unwrap() {
            clip["connected"]["value"] = json!("Disconnected");
        }
    }
    assert_eq!(eval(&body), none(Vec::new()));
}

#[test]
fn only_a_connected_songplayer_clip_counts() {
    let mut body = snv();
    item(&mut body, YT_A)["connected"]["value"] = json!("Disconnected");
    assert_eq!(eval(&body), running(vec![yt_music()], vec![snv_bg()]));
    item(&mut body, YT_B)["connected"]["value"] = json!("Disconnected");
    assert_eq!(eval(&body), none(vec![snv_bg()]));
    // Arena's other connected states: previewing alone is not on the wall.
    item(&mut body, YT_A)["connected"]["value"] = json!("Connected & previewing");
    item(&mut body, YT_B)["connected"]["value"] = json!("Previewing");
    assert_eq!(eval(&body), running(vec![yt_music()], vec![snv_bg()]));
    item(&mut body, YT_A)["connected"]["value"] = json!("Empty");
    assert_eq!(eval(&body), none(vec![snv_bg()]));
}

#[test]
fn a_bypassed_layer_hides_its_clip() {
    let mut body = snv();
    item(&mut body, FILL_A)["bypassed"]["value"] = json!(true);
    assert_eq!(eval(&body), running(vec![yt_music()], vec![snv_bg()]));
    item(&mut body, FILL_B)["bypassed"]["value"] = json!(true);
    assert_eq!(eval(&body), none(vec![snv_bg()]));
}

#[test]
fn a_layer_at_opacity_0_hides_its_clip() {
    let mut body = snv();
    item(&mut body, FILL_A)["video"]["opacity"]["value"] = json!(0.0);
    item(&mut body, FILL_B)["video"]["opacity"]["value"] = json!(0.0);
    assert_eq!(eval(&body), none(vec![snv_bg()]));
    // Faded half in still shows.
    item(&mut body, FILL_B)["video"]["opacity"]["value"] = json!(0.5);
    assert_eq!(eval(&body), running(vec![yt_music()], vec![snv_bg()]));
}

#[test]
fn its_group_bypassed_or_at_opacity_0_hides_it() {
    let mut body = snv();
    item(&mut body, KONTENT)["bypassed"]["value"] = json!(true);
    assert_eq!(
        eval(&body),
        none(vec![snv_bg()]),
        "the NDI background is in another group"
    );
    let mut body = snv();
    item(&mut body, KONTENT)["video"]["opacity"]["value"] = json!(0.0);
    assert_eq!(eval(&body), none(vec![snv_bg()]));
}

#[test]
fn the_composition_at_opacity_0_hides_everything() {
    let mut body = snv();
    body["video"]["opacity"]["value"] = json!(0.0);
    assert_eq!(eval(&body), none(Vec::new()));
}

/// The coordinator's box read (15.10.2026): every level carries a `master`
/// fader (0..1); like its `video.opacity`, each must be above 0.
#[test]
fn a_master_fader_at_0_hides_like_its_opacity() {
    let mut body = snv();
    item(&mut body, FILL_A)["master"]["value"] = json!(0.0);
    assert_eq!(eval(&body), running(vec![yt_music()], vec![snv_bg()]));
    item(&mut body, FILL_B)["master"]["value"] = json!(0.0);
    assert_eq!(
        eval(&body),
        none(vec![snv_bg()]),
        "both layers' masters down"
    );
    item(&mut body, FILL_B)["master"]["value"] = json!(0.5);
    assert_eq!(eval(&body), running(vec![yt_music()], vec![snv_bg()]));

    let mut body = snv();
    item(&mut body, KONTENT)["master"]["value"] = json!(0.0);
    assert_eq!(
        eval(&body),
        none(vec![snv_bg()]),
        "the NDI background is in another group"
    );
    // SNV's own group master, 0.96484375, shows (the fixture's value).
    item(&mut body, KONTENT)["master"]["value"] = json!(0.964_843_75);
    assert_eq!(
        eval(&body),
        running(vec![yt_music(), yt_music()], vec![snv_bg()])
    );

    let mut body = snv();
    body["master"]["value"] = json!(0.0);
    assert_eq!(eval(&body), none(Vec::new()), "the composition's master");
}

#[test]
fn a_layer_in_no_group_under_a_composition_with_no_opacity_still_shows() {
    let mut body = snv();
    body.as_object_mut().unwrap().remove("layergroups");
    body.as_object_mut().unwrap().remove("video");
    body.as_object_mut().unwrap().remove("master");
    let read = track(&body);
    assert_eq!(
        evaluate(&read.tracked, &read.values),
        running(vec![yt_music(), yt_music()], vec![snv_bg()])
    );
    assert_eq!(
        read.tracked.params().len(),
        21,
        "no group, no composition opacity or master"
    );
    let Tracked::Watched(watched) = &read.tracked else {
        panic!("{:?}", read.tracked);
    };
    assert!(watched.composition.is_empty());
    assert!(watched.clips.iter().all(|c| c.group.is_none()));
}

#[test]
fn a_composition_with_no_songplayer_clip_is_none() {
    let text_only = json!({"layers": [{
        "id": 1, "name": {"value": "L# Fit"},
        "clips": [{"id": 2, "name": {"value": "#sp-title"},
                   "connected": {"id": 3, "value": "Connected"},
                   "video": {"description": "Text Block"}}],
    }]});
    let read = track(&text_only);
    assert_eq!(read.tracked, Tracked::Watched(Watched::default()));
    assert_eq!(evaluate(&read.tracked, &read.values), none(Vec::new()));
    assert!(read.tracked.params().is_empty(), "nothing to poll");
}

#[test]
fn a_composition_not_loaded_yet_is_unknown() {
    for body in [json!({"layers": []}), json!({}), json!({"layers": null})] {
        let read = track(&body);
        assert_eq!(read, Read::default(), "{body}");
        let fact = evaluate(&read.tracked, &read.values);
        assert_eq!(fact.state, ClipState::Unknown);
        assert_eq!(
            fact.why.as_deref(),
            Some("Arena's composition is not loaded")
        );
        assert!(!fact.reader_fault, "Arena still loading is no reader fault");
        assert!(read.tracked.params().is_empty());
    }
}

#[test]
fn a_param_with_no_id_is_unreadable_and_unknown() {
    let mut body = snv();
    item(&mut body, FILL_A)["bypassed"]
        .as_object_mut()
        .unwrap()
        .remove("id");
    let read = track(&body);
    let why = "layer \"L# Fill\"'s bypassed carries no param id";
    assert_eq!(read.tracked, Tracked::Unreadable(why.to_string()));
    assert!(read.values.is_empty());
    assert!(read.tracked.params().is_empty(), "nothing to poll by id");
    // SongPlayer's own reader cannot read Arena's answer: a reader fault.
    assert_eq!(
        evaluate(&read.tracked, &read.values),
        Fact::reader_fault(why)
    );
    // A clip's connected and a group's opacity are needed too.
    let mut body = snv();
    item(&mut body, YT_B)["connected"]
        .as_object_mut()
        .unwrap()
        .remove("id");
    assert_eq!(
        track(&body).tracked,
        Tracked::Unreadable("clip \"Yt music\"'s connected carries no param id".to_string())
    );
    let mut body = snv();
    item(&mut body, KONTENT)["video"]["opacity"]
        .as_object_mut()
        .unwrap()
        .remove("id");
    assert_eq!(
        track(&body).tracked,
        Tracked::Unreadable("layer group \"G# Kontent\"'s opacity carries no param id".to_string())
    );
    let mut body = snv();
    body["video"]["opacity"]
        .as_object_mut()
        .unwrap()
        .remove("id");
    assert_eq!(
        track(&body).tracked,
        Tracked::Unreadable("the composition's opacity carries no param id".to_string())
    );
    // The masters are needed like the opacities.
    let mut body = snv();
    item(&mut body, FILL_A)["master"]
        .as_object_mut()
        .unwrap()
        .remove("id");
    assert_eq!(
        track(&body).tracked,
        Tracked::Unreadable("layer \"L# Fill\"'s master carries no param id".to_string())
    );
    let mut body = snv();
    body["master"].as_object_mut().unwrap().remove("id");
    assert_eq!(
        track(&body).tracked,
        Tracked::Unreadable("the composition's master carries no param id".to_string())
    );
}

#[test]
fn a_value_that_did_not_read_or_has_another_shape_is_unknown() {
    let read = track(&snv());
    for missing in [
        YT_B + 1,
        FILL_B + 1,
        FILL_B + 2,
        KONTENT + 2,
        COMP_OPACITY,
        FILL_A_MASTER,
        KONTENT_MASTER,
        COMP_MASTER,
    ] {
        let mut values = read.values.clone();
        values.remove(&missing);
        assert_eq!(
            evaluate(&read.tracked, &values),
            Fact::reader_fault(&format!("param {missing} did not read")),
        );
    }
    // A disconnected clip's layer still has to read: one rule for all.
    let mut values = read.values.clone();
    values.insert(YT_A + 1, ParamValue::Text("Disconnected".to_string()));
    values.insert(YT_B + 1, ParamValue::Text("Disconnected".to_string()));
    values.remove(&(FILL_A + 2));
    assert_eq!(evaluate(&read.tracked, &values).state, ClipState::Unknown);
    for (id, wrong) in [
        (YT_A + 1, ParamValue::Bool(true)),
        (FILL_A + 1, ParamValue::Number(0.0)),
        (FILL_A + 2, ParamValue::Text("1".to_string())),
        (COMP_OPACITY, ParamValue::Bool(false)),
        (COMP_MASTER, ParamValue::Bool(false)),
        (KONTENT_MASTER, ParamValue::Text("1".to_string())),
    ] {
        let mut values = read.values.clone();
        values.insert(id, wrong.clone());
        let fact = evaluate(&read.tracked, &values);
        assert_eq!(fact.state, ClipState::Unknown, "{wrong:?}");
        assert!(fact.reader_fault, "{wrong:?}");
        assert!(
            fact.why
                .as_deref()
                .is_some_and(|w| w.starts_with(&format!("param {id} is no "))),
            "{fact:?}"
        );
    }
}

#[test]
fn a_param_answer_reads_only_its_value() {
    let connected = r#"{"value":"Connected","valuetype":"ParamState","options":["Empty","Disconnected","Previewing","Connected","Connected & previewing"],"index":3,"id":1792013425423}"#;
    let bypassed = r#"{"valuetype":"ParamBoolean","id":1792013451694,"value":false}"#;
    let opacity = r#"{"id":1792013451639,"valuetype":"ParamRange","min":0.0,"max":1.0,"in":0.0,"out":1.0,"value":1.0}"#;
    let read = |text: &str| serde_json::from_str::<ParamAnswer>(text).map(|a| a.value);
    assert_eq!(
        read(connected).unwrap(),
        ParamValue::Text("Connected".to_string())
    );
    assert_eq!(read(bypassed).unwrap(), ParamValue::Bool(false));
    assert_eq!(read(opacity).unwrap(), ParamValue::Number(1.0));
    assert_eq!(read(r#"{"value":0}"#).unwrap(), ParamValue::Number(0.0));
    assert!(read(r#"{"value":null}"#).is_err());
    assert!(read(r#"{"id":5}"#).is_err());
    assert!(read(r#"{"value":[1]}"#).is_err());
}

#[test]
fn a_poll_is_due_every_30_s_with_something_to_read() {
    let base = Instant::now();
    let mut watch = Watch {
        tracked: track(&snv()).tracked,
        ..Watch::default()
    };
    assert!(watch.poll_due(base), "never read");
    watch.read_at = Some(base);
    assert!(!watch.poll_due(base + Duration::from_millis(29_999)));
    assert!(watch.poll_due(base + Duration::from_secs(30)));
    assert!(watch.poll_due(base + Duration::from_secs(31)));
    assert_eq!(POLL_EVERY, Duration::from_secs(30));
    // Nothing tracked (not loaded, no SongPlayer clip): never.
    let idle = Watch::default();
    assert!(!idle.poll_due(base));
    // Held after a refused id: not until the next full refresh.
    watch.held = true;
    assert!(!watch.poll_due(base + Duration::from_secs(60)));
}

#[test]
fn a_poll_has_a_2_s_budget_a_test_may_shorten() {
    assert_eq!(POLL_BUDGET, Duration::from_secs(2));
    assert_eq!(Watch::default().budget(), Duration::from_secs(2));
    let short = Watch {
        budget: Some(Duration::from_millis(200)),
        ..Watch::default()
    };
    assert_eq!(short.budget(), Duration::from_millis(200));
}

#[test]
fn a_reader_fault_is_unknown_with_its_reason() {
    let fact = Fact::reader_fault("param 7: error decoding response body");
    assert_eq!(fact.state, ClipState::Unknown);
    assert!(fact.reader_fault);
    assert_eq!(
        fact.why.as_deref(),
        Some("param 7: error decoding response body")
    );
    let unknown = Fact::unknown("no answer");
    assert!(!unknown.reader_fault);
    assert_eq!(unknown.why.as_deref(), Some("no answer"));
}

/// The owner's rules (#223, #259): the clip read never triggers, connects
/// or restores a clip and changes nothing in Arena: no write in its files.
#[test]
fn the_clip_read_writes_nothing_to_arena() {
    let sources = [
        ("sp_clips.rs", include_str!("sp_clips.rs")),
        ("sp_clip_store.rs", include_str!("sp_clip_store.rs")),
        ("driver_sp_clips.rs", include_str!("driver_sp_clips.rs")),
    ];
    for (file, text) in sources {
        for word in [
            ".put(",
            ".post(",
            ".patch(",
            ".delete(",
            "/clips/",
            "/connect",
            "/trigger",
            "/select",
            "set_text",
            "set_clip_opacity",
        ] {
            assert!(!text.contains(word), "{file} uses {word}");
        }
    }
}
