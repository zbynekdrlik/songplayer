//! Branch coverage for the pure dub subtitle builder (`subtitles.rs`, #182 D3).

use super::*;

fn frag(t_ms: u64, text: &str) -> SkFragment {
    SkFragment {
        t_ms,
        text: text.to_string(),
    }
}

fn chunk(
    start_ms: u64,
    at_ms: Option<u64>,
    tempo: Option<f64>,
    en: &str,
    frags: Vec<SkFragment>,
) -> DubChunk {
    DubChunk {
        start_ms,
        at_ms,
        tempo,
        en: en.to_string(),
        sk_timed: frags,
    }
}

fn build(chunks: Vec<DubChunk>) -> LyricsTrack {
    transcripts_to_track(&DubTranscripts { chunks })
}

// ── Track-level shape ────────────────────────────────────────────────────────

#[test]
fn track_carries_the_live_translate_source_and_langs() {
    let t = build(vec![chunk(
        0,
        Some(0),
        Some(1.0),
        "hello",
        vec![frag(500, "ahoj")],
    )]);
    assert_eq!(t.source, "gemini-live-translate");
    assert_eq!(t.source, SOURCE_LIVE_TRANSLATE);
    assert_eq!(t.language_source, "en");
    assert_eq!(t.language_translation, "sk");
    // Line-level only: no synthesized word timings.
    assert!(t.lines.iter().all(|l| l.words.is_none()));
}

// ── Grouping: sentence punctuation ───────────────────────────────────────────

#[test]
fn line_closes_at_sentence_punctuation() {
    // Two sentences in one chunk → two lines, split at the '.'.
    let t = build(vec![chunk(
        0,
        Some(0),
        Some(1.0),
        "Hello world how are you",
        vec![frag(1000, "Ahoj svet."), frag(2000, "Ako sa mas?")],
    )]);
    assert_eq!(t.lines.len(), 2);
    assert_eq!(t.lines[0].sk.as_deref(), Some("Ahoj svet."));
    assert_eq!(t.lines[1].sk.as_deref(), Some("Ako sa mas?"));
}

#[test]
fn ellipsis_and_bang_and_question_all_close_a_line() {
    let t = build(vec![chunk(
        0,
        Some(0),
        Some(1.0),
        "",
        vec![
            frag(500, "Naozaj!"),
            frag(1000, "A potom…"),
            frag(1500, "Koniec?"),
            frag(2000, "trail"),
        ],
    )]);
    // Each of the first three fragments ends a sentence; the 4th closes as last.
    assert_eq!(t.lines.len(), 4);
}

// ── Grouping: 14-word cap ────────────────────────────────────────────────────

#[test]
fn line_closes_at_fourteen_words() {
    // 20 single-word fragments (trailing space so the joined line is
    // whitespace-separated), no punctuation, tight timing → the cap splits them
    // into a 14-word line then a 6-word line.
    let frags: Vec<SkFragment> = (0..20)
        .map(|i| frag(100 * (i as u64 + 1), &format!("slovo{i} ")))
        .collect();
    let t = build(vec![chunk(0, Some(0), Some(1.0), "", frags)]);
    assert_eq!(t.lines.len(), 2);
    // First line holds exactly 14 words, the second the remaining 6.
    assert_eq!(
        t.lines[0].sk.as_deref().unwrap().split_whitespace().count(),
        14
    );
    assert_eq!(
        t.lines[1].sk.as_deref().unwrap().split_whitespace().count(),
        6
    );
}

// ── Grouping: arrival-gap pause ──────────────────────────────────────────────

#[test]
fn line_closes_on_a_gap_over_1500ms() {
    // A > 1500 ms jump between fragment arrival times closes the line even with
    // no punctuation and few words.
    let t = build(vec![chunk(
        0,
        Some(0),
        Some(1.0),
        "",
        vec![
            frag(500, "slovo a"),
            frag(1200, "slovo b"), // +700 ms → same line
            frag(4000, "slovo c"), // +2800 ms → new line
            frag(4500, "slovo d"),
        ],
    )]);
    assert_eq!(t.lines.len(), 2);
    assert_eq!(t.lines[0].sk.as_deref(), Some("slovo aslovo b"));
    assert_eq!(t.lines[1].sk.as_deref(), Some("slovo cslovo d"));
}

#[test]
fn a_gap_of_exactly_1500ms_keeps_the_line_open() {
    // Boundary: the pause rule is STRICTLY greater than LINE_GAP_MS, so a
    // 1500 ms gap stays on the line and 1501 ms closes it.
    let at_limit = build(vec![chunk(
        0,
        Some(0),
        Some(1.0),
        "",
        vec![frag(500, "a"), frag(2000, " b")],
    )]);
    assert_eq!(at_limit.lines.len(), 1);
    let over = build(vec![chunk(
        0,
        Some(0),
        Some(1.0),
        "",
        vec![frag(500, "a"), frag(2001, " b")],
    )]);
    assert_eq!(over.lines.len(), 2);
}

// ── Timing: tempo mapping ────────────────────────────────────────────────────

#[test]
fn video_time_is_at_ms_plus_local_over_tempo() {
    // One line, chunk placed at 100 000 ms, sped up 2× → local 2000 ms plays in
    // 1000 ms of video time.
    let t = build(vec![chunk(
        0,
        Some(100_000),
        Some(2.0),
        "a b",
        vec![frag(2000, "x")],
    )]);
    assert_eq!(t.lines.len(), 1);
    assert_eq!(t.lines[0].start_ms, 100_000); // local_start 0 / 2 + at
    assert_eq!(t.lines[0].end_ms, 101_000); // local_end 2000 / 2 + at
}

#[test]
fn a_later_line_starts_where_the_previous_fragment_ended() {
    // Line 2 spans fragments 1..=2, so its window opens at t[0] (the fragment
    // BEFORE its first one), not at its own first fragment's arrival.
    let t = build(vec![chunk(
        0,
        Some(10_000),
        Some(1.0),
        "",
        vec![
            frag(1000, "Prvá."),
            frag(1800, " druhá"),
            frag(2600, " veta."),
        ],
    )]);
    assert_eq!(t.lines.len(), 2);
    assert_eq!(t.lines[0].end_ms, 11_000);
    assert_eq!(t.lines[1].start_ms, 11_000);
    assert_eq!(t.lines[1].end_ms, 12_600);
}

// ── Timing: clamping ─────────────────────────────────────────────────────────

#[test]
fn end_is_at_least_min_line_ms_after_start() {
    // A 100 ms line is stretched to the 400 ms floor.
    let t = build(vec![chunk(
        0,
        Some(0),
        Some(1.0),
        "hi",
        vec![frag(100, "ahoj")],
    )]);
    assert_eq!(t.lines[0].start_ms, 0);
    assert_eq!(t.lines[0].end_ms, 400);
}

#[test]
fn start_is_clamped_to_the_previous_line_end() {
    // Two chunks whose raw windows overlap → the 2nd line is pushed after the 1st.
    let t = build(vec![
        chunk(0, Some(0), Some(1.0), "one", vec![frag(500, "prve")]),
        chunk(0, Some(0), Some(1.0), "two", vec![frag(300, "druhe")]),
    ]);
    assert_eq!(t.lines.len(), 2);
    assert_eq!(t.lines[0].start_ms, 0);
    assert_eq!(t.lines[0].end_ms, 500);
    // Raw 2nd window is [0, 300] but is clamped to start at 500 and held ≥ 400 ms.
    assert_eq!(t.lines[1].start_ms, 500);
    assert_eq!(t.lines[1].end_ms, 900);
}

#[test]
fn multi_chunk_timeline_is_monotonic() {
    let t = build(vec![
        chunk(0, Some(0), Some(1.0), "a", vec![frag(1000, "jeden.")]),
        chunk(0, Some(2000), Some(1.0), "b", vec![frag(1000, "dva.")]),
        chunk(0, Some(4000), Some(1.0), "c", vec![frag(1000, "tri.")]),
    ]);
    assert_eq!(t.lines.len(), 3);
    let mut last = 0;
    for l in &t.lines {
        assert!(
            l.start_ms >= last,
            "start {} < prev end {}",
            l.start_ms,
            last
        );
        assert!(l.end_ms > l.start_ms);
        last = l.end_ms;
    }
    assert_eq!(t.lines[1].start_ms, 2000);
    assert_eq!(t.lines[2].start_ms, 4000);
}

// ── EN fraction snapping ─────────────────────────────────────────────────────

#[test]
fn en_reference_is_sliced_by_the_same_char_fraction_as_the_sk() {
    // SK: "Ahoj svet." (10 ch) + "Ako sa mas?" (11 ch) = 21 ch. Line 1 covers the
    // first 10/21 ≈ 0.476; line 2 the rest. EN "Hello world how are you" (word
    // start-fractions 0, .263, .526, .684, .842) splits Hello+world | how+are+you.
    let t = build(vec![chunk(
        0,
        Some(0),
        Some(1.0),
        "Hello world how are you",
        vec![frag(1000, "Ahoj svet."), frag(2000, "Ako sa mas?")],
    )]);
    assert_eq!(t.lines.len(), 2);
    assert_eq!(t.lines[0].en, "Hello world");
    assert_eq!(t.lines[1].en, "how are you");
}

#[test]
fn en_word_starting_exactly_on_the_line_boundary_goes_to_the_next_line() {
    // SK splits 3 | 3 chars → boundary fraction 0.5; EN "xx yy" → "yy" starts at
    // exactly 2/4 = 0.5. The slice is half-open `[a, b)`, so "yy" belongs to
    // line 2 only — never duplicated into line 1.
    let t = build(vec![chunk(
        0,
        Some(0),
        Some(1.0),
        "xx yy",
        vec![frag(1000, "ab."), frag(2000, "cd.")],
    )]);
    assert_eq!(t.lines.len(), 2);
    assert_eq!(t.lines[0].en, "xx");
    assert_eq!(t.lines[1].en, "yy");
}

#[test]
fn single_line_takes_the_whole_en_string() {
    let t = build(vec![chunk(
        0,
        Some(0),
        Some(1.0),
        "one two three",
        vec![frag(1000, "raz dva tri")],
    )]);
    assert_eq!(t.lines.len(), 1);
    assert_eq!(t.lines[0].en, "one two three");
}

// ── Empty EN → empty EN lines ────────────────────────────────────────────────

#[test]
fn empty_en_yields_empty_en_lines() {
    let t = build(vec![chunk(
        0,
        Some(0),
        Some(1.0),
        "",
        vec![frag(1000, "Ahoj."), frag(2000, "Svet.")],
    )]);
    assert_eq!(t.lines.len(), 2);
    assert!(t.lines.iter().all(|l| l.en.is_empty()));
    assert!(t.lines.iter().all(|l| l.sk.is_some()));
}

// ── Empty chunk → no lines ───────────────────────────────────────────────────

#[test]
fn chunk_without_sk_timed_yields_no_lines() {
    let t = build(vec![
        chunk(0, Some(0), Some(1.0), "some english", vec![]),
        chunk(0, Some(5000), Some(1.0), "more", vec![frag(500, "ahoj.")]),
    ]);
    // The empty chunk contributes nothing; only the second chunk's line remains.
    assert_eq!(t.lines.len(), 1);
    assert_eq!(t.lines[0].sk.as_deref(), Some("ahoj."));
}

#[test]
fn no_chunks_yields_an_empty_track() {
    let t = build(vec![]);
    assert!(t.lines.is_empty());
    assert_eq!(t.source, "gemini-live-translate");
}

// ── Legacy JSON without at_ms / tempo ────────────────────────────────────────

#[test]
fn legacy_chunk_uses_start_ms_and_unit_tempo() {
    // No at_ms / tempo (a pre-#182 transcripts JSON) → place at start_ms, tempo 1.
    let t = build(vec![chunk(
        5000,
        None,
        None,
        "hi",
        vec![frag(1000, "ahoj.")],
    )]);
    assert_eq!(t.lines.len(), 1);
    assert_eq!(t.lines[0].start_ms, 5000); // start_ms + 0/1.0
    assert_eq!(t.lines[0].end_ms, 6000); // start_ms + 1000/1.0
}

#[test]
fn non_positive_tempo_falls_back_to_unit() {
    // A degenerate tempo (0 or NaN) must not divide-by-zero / poison the timeline.
    let t = build(vec![chunk(
        0,
        Some(0),
        Some(0.0),
        "hi",
        vec![frag(1000, "ahoj.")],
    )]);
    assert_eq!(t.lines[0].end_ms, 1000);
}

// ── Deserialization from the real JSON shape ─────────────────────────────────

#[test]
fn deserializes_the_dub_transcripts_json_and_builds_lines() {
    let json = r#"{
        "engine": "gemini-live-translate",
        "target_lang": "sk",
        "chunks": [
            {"index": 0, "start_ms": 0, "end_ms": 60000, "at_ms": 0, "tempo": 1.0,
             "en": "Hello world", "sk": "Ahoj svet.",
             "sk_timed": [{"t_ms": 1000, "text": "Ahoj svet."}]}
        ]
    }"#;
    let parsed: DubTranscripts = serde_json::from_str(json).unwrap();
    let track = transcripts_to_track(&parsed);
    assert_eq!(track.lines.len(), 1);
    assert_eq!(track.lines[0].sk.as_deref(), Some("Ahoj svet."));
    assert_eq!(track.lines[0].en, "Hello world");
    assert_eq!(track.lines[0].start_ms, 0);
    assert_eq!(track.lines[0].end_ms, 1000);
}
