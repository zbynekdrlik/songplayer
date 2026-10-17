//! #256: the usage log's pure rules: the wire and its bounds, the client's
//! rules (exclusion, a control's name, the batch buffer, which page views
//! count), the summary and Nastavenia's line.

use super::{
    BATCH_REFUSAL, DAYS_REFUSAL, MAX_EVENTS, PageViews, UsageBatch, UsageBuffer, UsageEvent,
    UsageKind, UsageRow, UsageSummary, check_batch, clip, control_name, excluded, parse_batch,
    parse_days, settings_line, span_days, window_start,
};

const DAY: i64 = 86_400_000;
const HOUR: i64 = 3_600_000;
const NOW: i64 = 1_791_608_681_366;

fn event(ts_ms: i64, control: &str) -> UsageEvent {
    UsageEvent {
        ts_ms,
        page: "/".to_string(),
        control: control.to_string(),
        kind: UsageKind::Click,
    }
}

fn row(control: &str, count: i64, first_ms: i64) -> UsageRow {
    UsageRow {
        page: "/".to_string(),
        control: control.to_string(),
        kind: "click".to_string(),
        count,
        days_used: 1,
        first_ms,
        last_ms: NOW,
        on_air: 0,
    }
}

#[test]
fn every_kind_has_its_wire_name() {
    let table = [
        (UsageKind::Click, "click"),
        (UsageKind::Change, "change"),
        (UsageKind::Submit, "submit"),
        (UsageKind::Page, "page"),
    ];
    for (kind, name) in table {
        assert_eq!(kind.as_str(), name);
        assert_eq!(serde_json::to_string(&kind).unwrap(), format!("\"{name}\""));
    }
}

#[test]
fn a_batch_goes_on_the_wire_with_these_field_names() {
    let batch = UsageBatch {
        events: vec![UsageEvent::new(7, "/live", "nav-live", UsageKind::Page)],
    };
    assert_eq!(
        serde_json::to_string(&batch).unwrap(),
        r#"{"events":[{"ts_ms":7,"page":"/live","control":"nav-live","kind":"page"}]}"#
    );
}

#[test]
fn clip_keeps_the_first_120_characters_not_bytes() {
    assert_eq!(clip(&"á".repeat(130)), "á".repeat(120));
    assert_eq!(clip(&"x".repeat(120)), "x".repeat(120));
    assert_eq!(clip("nav-settings"), "nav-settings");
}

#[test]
fn an_event_is_recorded_clipped() {
    let recorded = UsageEvent::new(5, &"p".repeat(121), &"c".repeat(125), UsageKind::Change);
    assert_eq!(
        recorded,
        UsageEvent {
            ts_ms: 5,
            page: "p".repeat(120),
            control: "c".repeat(120),
            kind: UsageKind::Change,
        }
    );
}

#[test]
fn a_control_is_named_by_its_testid_else_by_its_tag() {
    assert_eq!(control_name(Some("nav-settings"), "BUTTON"), "nav-settings");
    assert_eq!(control_name(None, "BUTTON"), "<button>");
    assert_eq!(control_name(Some(""), "H1"), "<h1>", "an empty id is no id");
    assert_eq!(
        control_name(None, "path"),
        "<path>",
        "an SVG tag is lower case"
    );
    assert_eq!(control_name(Some(&"t".repeat(130)), "DIV"), "t".repeat(120));
}

#[test]
fn an_automated_or_marked_browser_records_nothing() {
    assert!(!excluded(false, None), "a person's browser records");
    assert!(excluded(true, None), "navigator.webdriver (Playwright)");
    assert!(excluded(false, Some("1")), "the sp_no_usage key");
    assert!(excluded(false, Some("")), "the key with any value");
    assert!(excluded(true, Some("1")));
}

#[test]
fn a_batch_of_200_events_of_120_characters_is_taken() {
    let mut events = vec![event(1, &"c".repeat(120)); MAX_EVENTS];
    events[0].page = "é".repeat(120);
    events[1].ts_ms = 0;
    assert_eq!(events.len(), 200);
    assert_eq!(check_batch(&UsageBatch { events }), Ok(()));
    assert_eq!(check_batch(&UsageBatch { events: Vec::new() }), Ok(()));
}

#[test]
fn a_batch_over_its_bounds_is_refused_whole() {
    let over = vec![event(1, "x"); 201];
    assert_eq!(
        check_batch(&UsageBatch { events: over }),
        Err(BATCH_REFUSAL)
    );
    let mut long_page = vec![event(1, "x"); 3];
    long_page[2].page = "p".repeat(121);
    assert_eq!(
        check_batch(&UsageBatch { events: long_page }),
        Err(BATCH_REFUSAL)
    );
    let mut long_control = vec![event(1, "x"); 3];
    long_control[1].control = "c".repeat(121);
    assert_eq!(
        check_batch(&UsageBatch {
            events: long_control
        }),
        Err(BATCH_REFUSAL)
    );
    let mut before_1970 = vec![event(1, "x"); 3];
    before_1970[0].ts_ms = -1;
    assert_eq!(
        check_batch(&UsageBatch {
            events: before_1970
        }),
        Err(BATCH_REFUSAL)
    );
}

#[test]
fn a_stamp_after_its_arrival_is_capped_at_the_servers_clock() {
    let body = format!(
        r#"{{"events":[{{"ts_ms":{},"page":"/","control":"a","kind":"click"}},{{"ts_ms":{},"page":"/","control":"b","kind":"click"}},{{"ts_ms":{},"page":"/","control":"c","kind":"click"}}]}}"#,
        NOW + DAY,
        NOW - HOUR,
        NOW
    );
    let batch = parse_batch(body.as_bytes(), NOW).unwrap();
    let stamps: Vec<i64> = batch.events.iter().map(|e| e.ts_ms).collect();
    assert_eq!(stamps, [NOW, NOW - HOUR, NOW]);
}

#[test]
fn a_body_is_read_typed_whatever_else_it_carries() {
    let body = br#"{"events":[{"ts_ms":7,"page":"/live","control":"nav-live","kind":"click"}],"extra":{"a":[1,{"b":2}]}}"#;
    assert_eq!(
        parse_batch(body, NOW),
        Ok(UsageBatch {
            events: vec![UsageEvent {
                ts_ms: 7,
                page: "/live".to_string(),
                control: "nav-live".to_string(),
                kind: UsageKind::Click,
            }],
        })
    );
}

#[test]
fn a_body_that_is_no_batch_is_refused() {
    let bodies: [&[u8]; 8] = [
        b"not json",
        b"{}",
        br#"{"events":null}"#,
        br#"{"events":{}}"#,
        br#"{"events":[{"ts_ms":1,"page":"/","control":"x","kind":"hover"}]}"#,
        br#"{"events":[{"ts_ms":1,"page":"/","kind":"click"}]}"#,
        br#"{"events":[{"ts_ms":"1","page":"/","control":"x","kind":"click"}]}"#,
        br#"{"events":[{"ts_ms":1,"page":"/","control":null,"kind":"click"}]}"#,
    ];
    for body in bodies {
        assert_eq!(
            parse_batch(body, NOW),
            Err(BATCH_REFUSAL),
            "{}",
            String::from_utf8_lossy(body)
        );
    }
    let long = format!(
        r#"{{"events":[{{"ts_ms":1,"page":"{}","control":"x","kind":"click"}}]}}"#,
        "p".repeat(121)
    );
    assert_eq!(
        parse_batch(long.as_bytes(), NOW),
        Err(BATCH_REFUSAL),
        "checked too"
    );
}

#[test]
fn the_buffer_hands_back_a_full_batch_at_200_events() {
    let mut buffer = UsageBuffer::default();
    for ts in 0..199 {
        assert_eq!(buffer.push(event(ts, "x")), None, "{ts}");
    }
    let full = buffer
        .push(event(199, "x"))
        .expect("the 200th event fills it");
    assert_eq!(full.events.len(), 200);
    assert_eq!((full.events[0].ts_ms, full.events[199].ts_ms), (0, 199));
    assert_eq!(buffer.take(), None, "the full batch left the buffer");
    assert_eq!(buffer.push(event(200, "y")), None);
    assert_eq!(
        buffer.take(),
        Some(UsageBatch {
            events: vec![event(200, "y")]
        })
    );
}

#[test]
fn the_buffer_sends_what_it_holds_once_and_nothing_when_empty() {
    let mut buffer = UsageBuffer::default();
    assert_eq!(buffer.take(), None);
    assert_eq!(buffer.push(event(1, "a")), None);
    assert_eq!(buffer.push(event(2, "b")), None);
    assert_eq!(
        buffer.take(),
        Some(UsageBatch {
            events: vec![event(1, "a"), event(2, "b")]
        })
    );
    assert_eq!(buffer.take(), None);
}

#[test]
fn days_are_a_whole_number_from_1_to_400_by_default_all() {
    assert_eq!(parse_days(None), Ok(400));
    assert_eq!(parse_days(Some("1")), Ok(1));
    assert_eq!(parse_days(Some("30")), Ok(30));
    assert_eq!(parse_days(Some("400")), Ok(400));
    for bad in [
        "0",
        "401",
        "",
        "abc",
        "-1",
        "+5",
        "1.5",
        " 7",
        "99999999999",
    ] {
        assert_eq!(parse_days(Some(bad)), Err(DAYS_REFUSAL), "{bad:?}");
    }
}

#[test]
fn a_window_starts_its_days_before_now() {
    assert_eq!(window_start(NOW, 1), NOW - DAY);
    assert_eq!(window_start(NOW, 7), NOW - 7 * DAY);
    assert_eq!(window_start(NOW, 400), NOW - 400 * DAY);
}

#[test]
fn the_summary_totals_its_rows() {
    let rows = vec![
        row("a", 5, NOW - 3 * DAY),
        row("b", 2, NOW - 9 * DAY),
        row("c", 1, NOW - DAY),
    ];
    let summary = UsageSummary::from_rows(NOW, 30, rows.clone());
    assert_eq!(
        summary,
        UsageSummary {
            now_ms: NOW,
            days: 30,
            from_ms: NOW - 30 * DAY,
            events: 8,
            first_ms: Some(NOW - 9 * DAY),
            rows,
        }
    );
    let empty = UsageSummary::from_rows(NOW, 7, Vec::new());
    assert_eq!(
        (empty.events, empty.first_ms, empty.from_ms),
        (0, None, NOW - 7 * DAY)
    );
}

#[test]
fn the_span_counts_whole_days_up() {
    let table = [
        (NOW, 1),
        (NOW - 1, 1),
        (NOW - HOUR, 1),
        (NOW - DAY, 1),
        (NOW - DAY - 1, 2),
        (NOW - 25 * HOUR, 2),
        (NOW - 4 * DAY - 1, 5),
        (NOW + DAY, 1),
    ];
    for (first, days) in table {
        assert_eq!(span_days(first, NOW), days, "{}", NOW - first);
    }
}

#[test]
fn nastavenia_says_how_many_events_over_how_many_days() {
    let summary = |events: i64, first_ms: Option<i64>| UsageSummary {
        now_ms: NOW,
        days: 400,
        from_ms: NOW - 400 * DAY,
        events,
        first_ms,
        rows: Vec::new(),
    };
    let table = [
        (0, None, "Záznam používania: zatiaľ žiadna udalosť"),
        (1, Some(NOW - HOUR), "Záznam používania: 1 udalosť za 1 deň"),
        (
            3,
            Some(NOW - 25 * HOUR),
            "Záznam používania: 3 udalosti za 2 dni",
        ),
        (
            5,
            Some(NOW - 4 * DAY - 1),
            "Záznam používania: 5 udalostí za 5 dní",
        ),
        (
            1234,
            Some(NOW - 11 * DAY - 1),
            "Záznam používania: 1234 udalostí za 12 dní",
        ),
    ];
    for (events, first_ms, line) in table {
        assert_eq!(settings_line(&summary(events, first_ms)), line);
    }
}

#[test]
fn a_page_view_counts_only_once_the_page_can_be_seen() {
    let mut views = PageViews::default();
    assert!(views.changed(true), "a page shown in sight counts at once");
    assert!(!views.came_into_view(true), "nothing owed");
    assert!(!views.changed(false), "the app's hidden window: not yet");
    assert!(!views.came_into_view(false), "still not seen");
    assert!(views.came_into_view(true), "seen: the owed view counts");
    assert!(!views.came_into_view(true), "once");
    assert!(!views.changed(false));
    assert!(views.changed(true), "a change in sight counts at once");
    assert!(!views.came_into_view(true), "and owes nothing");
}
