//! Branch coverage for the pure dub subtitle builder (`subtitles.rs`, #182 D3).

use super::*;

fn frag(t_ms: u64, text: &str) -> SkFragment {
    SkFragment {
        t_ms,
        text: text.to_string(),
    }
}

fn en_frag(t_ms: u64, text: &str) -> EnFragment {
    EnFragment {
        t_ms,
        text: text.to_string(),
    }
}

/// A chunk with NO `en_timed` (the EN is added by [`with_en`] where a test needs it).
fn chunk(
    start_ms: u64,
    at_ms: Option<u64>,
    tempo: Option<f64>,
    frags: Vec<SkFragment>,
) -> DubChunk {
    DubChunk {
        start_ms,
        at_ms,
        tempo,
        en_timed: Vec::new(),
        sk_timed: frags,
    }
}

fn with_en(mut c: DubChunk, en: Vec<EnFragment>) -> DubChunk {
    c.en_timed = en;
    c
}

fn build(chunks: Vec<DubChunk>) -> LyricsTrack {
    transcripts_to_track(&DubTranscripts { chunks })
}

// ── Track-level shape ────────────────────────────────────────────────────────

#[test]
fn track_carries_the_live_translate_source_and_langs() {
    let t = build(vec![chunk(0, Some(0), Some(1.0), vec![frag(500, "ahoj")])]);
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
    let t = build(vec![chunk(0, Some(0), Some(1.0), frags)]);
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
        vec![frag(500, "a"), frag(2000, " b")],
    )]);
    assert_eq!(at_limit.lines.len(), 1);
    let over = build(vec![chunk(
        0,
        Some(0),
        Some(1.0),
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
    let t = build(vec![chunk(0, Some(0), Some(1.0), vec![frag(100, "ahoj")])]);
    assert_eq!(t.lines[0].start_ms, 0);
    assert_eq!(t.lines[0].end_ms, 400);
}

#[test]
fn overlapping_chunk_windows_trim_the_earlier_line_no_drift() {
    // #182 item 7: two chunks whose raw windows overlap. The later line anchors
    // to its TRUE start (300) — it is NOT pushed after the first line's
    // MIN-extended end (the old drift semantics). The first line's displayed end
    // is trimmed back to the second's start so they do not overlap.
    // (Old behaviour was line0 [0,500], line1 [500,900] — the drift this fixes.)
    let t = build(vec![
        chunk(0, Some(0), Some(1.0), vec![frag(500, "prve")]),
        chunk(0, Some(300), Some(1.0), vec![frag(400, "druhe")]),
    ]);
    assert_eq!(t.lines.len(), 2);
    assert_eq!(t.lines[0].start_ms, 0);
    assert_eq!(
        t.lines[0].end_ms, 300,
        "trimmed to the next line's true start"
    );
    assert_eq!(
        t.lines[1].start_ms, 300,
        "anchored to its true start — no drift"
    );
    assert_eq!(t.lines[1].end_ms, 700);
}

// ── #182 review fixes (release code review, items 7–9) ───────────────────────

#[test]
fn a_run_of_short_lines_does_not_push_a_later_line_late() {
    // Five 100 ms lines then a normal line. Under the old MIN-extension-feeds-
    // prev_end logic each short line stretched to 400 ms and shoved the next
    // later, accumulating > 1.5 s of drift by the 6th line. The two-pass timing
    // anchors every line to its TRUE start, so the 6th line still starts at 500.
    let t = build(vec![chunk(
        0,
        Some(0),
        Some(1.0),
        vec![
            frag(100, "a."),
            frag(200, "b."),
            frag(300, "c."),
            frag(400, "d."),
            frag(500, "e."),
            frag(2000, "koniec."),
        ],
    )]);
    assert_eq!(t.lines.len(), 6);
    let starts: Vec<u64> = t.lines.iter().map(|l| l.start_ms).collect();
    assert_eq!(
        starts,
        vec![0, 100, 200, 300, 400, 500],
        "no accumulated drift"
    );
    // No overlap and monotonic starts.
    for w in t.lines.windows(2) {
        assert!(w[0].end_ms <= w[1].start_ms, "lines must not overlap");
        assert!(w[1].start_ms >= w[0].start_ms, "starts monotonic");
    }
    assert_eq!(
        t.lines[5].start_ms, 500,
        "the normal line keeps its true start"
    );
    assert_eq!(t.lines[5].end_ms, 2000);
}

#[test]
fn starts_stay_monotonic_when_a_later_chunk_goes_backwards() {
    // A later chunk placed EARLIER on the video timeline must not produce a line
    // starting before the previous line — the start is clamped up to the
    // previous start (monotonic even when at_ms/t_ms goes backwards).
    let t = build(vec![
        chunk(0, Some(10_000), Some(1.0), vec![frag(1000, "neskor.")]),
        chunk(0, Some(0), Some(1.0), vec![frag(500, "skor.")]),
    ]);
    assert_eq!(t.lines.len(), 2);
    let starts: Vec<u64> = t.lines.iter().map(|l| l.start_ms).collect();
    assert!(
        starts[1] >= starts[0],
        "starts monotonic even going backwards"
    );
    assert_eq!(starts[0], 10_000);
    assert_eq!(
        starts[1], 10_000,
        "the backwards line clamps up to the prev start"
    );
    for l in &t.lines {
        assert!(
            l.end_ms > l.start_ms,
            "every line shows for a positive duration"
        );
    }
}

#[test]
fn a_blank_sk_group_is_skipped() {
    // A pure-whitespace fragment group (closed by a > 1500 ms gap) yields no
    // line; only the real group becomes a subtitle (#182 item 9).
    let t = build(vec![chunk(
        0,
        Some(0),
        Some(1.0),
        vec![frag(500, "   "), frag(3000, "ozaj.")],
    )]);
    assert_eq!(t.lines.len(), 1);
    assert_eq!(t.lines[0].sk.as_deref(), Some("ozaj."));
}

#[test]
fn an_all_blank_transcript_yields_no_lines() {
    // Every fragment blank → no subtitle lines (subtitles_store then stores
    // nothing and returns 0).
    let t = build(vec![chunk(
        0,
        Some(0),
        Some(1.0),
        vec![frag(1000, "  "), frag(2000, "   ")],
    )]);
    assert!(t.lines.is_empty(), "all-blank SK → no subtitle lines");
}

#[test]
fn tempo_is_clamped_to_the_supported_range() {
    // tempo below 0.25 clamps to 0.25 (local/0.25 = local*4); above 4.0 to 4.0.
    let below = build(vec![chunk(0, Some(0), Some(0.1), vec![frag(1000, "x.")])]);
    // 1000 / 0.25 = 4000 (would be 1000/0.1 = 10000 unclamped).
    assert_eq!(below.lines[0].end_ms, 4000, "tempo < 0.25 clamps to 0.25");
    let at_low = build(vec![chunk(0, Some(0), Some(0.25), vec![frag(1000, "x.")])]);
    assert_eq!(
        at_low.lines[0].end_ms, 4000,
        "0.25 is the low boundary, kept"
    );
    let above = build(vec![chunk(0, Some(0), Some(10.0), vec![frag(8000, "x.")])]);
    // 8000 / 4.0 = 2000 (would be 8000/10 = 800 unclamped).
    assert_eq!(above.lines[0].end_ms, 2000, "tempo > 4.0 clamps to 4.0");
    let at_high = build(vec![chunk(0, Some(0), Some(4.0), vec![frag(8000, "x.")])]);
    assert_eq!(
        at_high.lines[0].end_ms, 2000,
        "4.0 is the high boundary, kept"
    );
}

#[test]
fn multi_chunk_timeline_is_monotonic() {
    let t = build(vec![
        chunk(0, Some(0), Some(1.0), vec![frag(1000, "jeden.")]),
        chunk(0, Some(2000), Some(1.0), vec![frag(1000, "dva.")]),
        chunk(0, Some(4000), Some(1.0), vec![frag(1000, "tri.")]),
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

// ── No `en_timed` → empty EN lines ───────────────────────────────────────────

#[test]
fn no_en_timed_yields_empty_en_lines() {
    let t = build(vec![chunk(
        0,
        Some(0),
        Some(1.0),
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
        chunk(0, Some(0), Some(1.0), vec![]),
        chunk(0, Some(5000), Some(1.0), vec![frag(500, "ahoj.")]),
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
    let t = build(vec![chunk(5000, None, None, vec![frag(1000, "ahoj.")])]);
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
             "en_timed": [{"t_ms": 200, "text": "Hello"}, {"t_ms": 600, "text": " world"}],
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

// ── #184 H3: EN timed by the input transcription, whole-sentence assignment ──

#[test]
fn en_timed_missing_in_the_json_gives_empty_en() {
    // A transcript written before H3 has only the untimed `en` string: its lines
    // get NO EN (the char-fraction guess is deleted) until the video is re-dubbed.
    let json = r#"{"chunks": [
        {"index": 0, "start_ms": 0, "end_ms": 9000, "at_ms": 0, "tempo": 1.0,
         "en": "Hello world. Bye.", "sk": "Ahoj svet. Zbohom.",
         "sk_timed": [{"t_ms": 1000, "text": "Ahoj svet."}, {"t_ms": 2000, "text": "Zbohom."}]}
    ]}"#;
    let parsed: DubTranscripts = serde_json::from_str(json).unwrap();
    let track = transcripts_to_track(&parsed);
    assert_eq!(track.lines.len(), 2);
    assert!(track.lines.iter().all(|l| l.en.is_empty()));
    assert!(track.lines.iter().all(|l| l.sk.is_some()));
}

#[test]
fn en_stays_within_its_own_chunk() {
    // Chunk 1's EN arrives late (9 500 on the video, nearer chunk 2's line at
    // 10 000), but it translates chunk 1's input, so it stays on chunk 1's line.
    let t = build(vec![
        with_en(
            chunk(0, Some(0), Some(1.0), vec![frag(1000, "Jeden.")]),
            vec![en_frag(9500, "One.")],
        ),
        with_en(
            chunk(0, Some(10_000), Some(1.0), vec![frag(1000, "Dva.")]),
            vec![en_frag(0, "Two.")],
        ),
    ]);
    let got: Vec<&str> = t.lines.iter().map(|l| l.en.as_str()).collect();
    assert_eq!(got, vec!["One.", "Two."]);
}

// ── #184 round H step 2: the ONE continuous-session chunk ────────────────────

#[test]
fn one_continuous_session_chunk_builds_a_monotonic_bilingual_track() {
    // Exactly the shape `dub_worker.py::build_transcripts` writes for the ONE
    // continuous Live session: a single chunk covering the whole video
    // (`at_ms` 0, `tempo` 1.0) whose `sk_timed` and `en_timed` are both
    // VIDEO-timeline times (arrival − t0 − the measured latency, H4), so the
    // EN and SK of the same speech are synchronous. Each EN sentence lands WHOLE
    // on the SK line whose content interval it overlaps most (#184 H5).
    let json = r#"{
        "engine": "gemini-live-translate",
        "target_lang": "sk",
        "chunks": [
            {"index": 0, "start_ms": 0, "end_ms": 2160000, "at_ms": 0, "tempo": 1.0,
             "en": "Hello brothers. Today we will talk about faith. Amen.",
             "sk": "Ahoj bratia. Dnes budeme hovoriť o viere. Amen.",
             "en_timed": [
                {"t_ms": 3900, "text": "Hello brothers."},
                {"t_ms": 5100, "text": "Today we will"},
                {"t_ms": 5900, "text": " talk about faith."},
                {"t_ms": 8900, "text": "Amen."}
             ],
             "sk_timed": [
                {"t_ms": 4000, "text": "Ahoj bratia."},
                {"t_ms": 5200, "text": "Dnes budeme"},
                {"t_ms": 6000, "text": " hovoriť o viere."},
                {"t_ms": 9000, "text": "Amen."}
             ]}
        ]
    }"#;
    let parsed: DubTranscripts = serde_json::from_str(json).unwrap();
    assert_eq!(parsed.chunks.len(), 1);
    let track = transcripts_to_track(&parsed);
    assert_eq!(track.source, SOURCE_LIVE_TRANSLATE);
    let got: Vec<(u64, u64, &str, Option<&str>)> = track
        .lines
        .iter()
        .map(|l| (l.start_ms, l.end_ms, l.en.as_str(), l.sk.as_deref()))
        .collect();
    assert_eq!(
        got,
        vec![
            (0, 4000, "Hello brothers.", Some("Ahoj bratia.")),
            (
                4000,
                6000,
                "Today we will talk about faith.",
                Some("Dnes budeme hovoriť o viere.")
            ),
            (6000, 9000, "Amen.", Some("Amen.")),
        ]
    );
    // Monotonic, non-overlapping, every line bilingual.
    for pair in track.lines.windows(2) {
        assert!(pair[0].start_ms <= pair[1].start_ms);
        assert!(pair[0].end_ms <= pair[1].start_ms);
    }
    assert!(
        track
            .lines
            .iter()
            .all(|l| !l.en.is_empty() && l.sk.is_some())
    );
}

// ── #184 H5: content-interval overlap, sentences split inside a fragment ─────

fn sent(start_ms: u64, end_ms: u64, text: &str) -> EnSentence {
    EnSentence {
        start_ms,
        end_ms,
        text: text.to_string(),
    }
}

fn assign(sentences: &[EnSentence], lines: &[(u64, u64)]) -> Vec<String> {
    assign_en_sentences(sentences, lines)
}

#[test]
fn content_end_is_the_next_fragment_capped_at_1500ms() {
    let t = [1000, 2000, 5000, 6500];
    assert_eq!(content_end(&t, 0), 2000); // the next fragment, within the cap
    assert_eq!(content_end(&t, 1), 3500); // next 3000 ms later → capped at +1500
    assert_eq!(content_end(&t, 2), 6500); // next exactly +1500 → that fragment
    assert_eq!(content_end(&t, 3), 8000); // no next fragment → +1500
}

#[test]
fn a_fragment_splits_after_sentence_punctuation_followed_by_whitespace() {
    assert_eq!(
        split_sentences_inside(" Good to see you, Nathan. And"),
        vec![" Good to see you, Nathan.", " And"]
    );
    // Every mark splits, the multi-byte `…` included; the pieces keep their
    // own spaces so they concatenate back to the fragment.
    assert_eq!(
        split_sentences_inside("Wait… ok! Go? Yes. "),
        vec!["Wait…", " ok!", " Go?", " Yes.", " "]
    );
    // A mark NOT followed by whitespace (a number, the fragment's own end) does
    // not split.
    assert_eq!(split_sentences_inside("at 5.30 in."), vec!["at 5.30 in."]);
    assert_eq!(split_sentences_inside("plain"), vec!["plain"]);
    assert!(split_sentences_inside("").is_empty());
}

#[test]
fn en_sentences_split_inside_a_fragment_and_both_parts_belong_to_it() {
    // „ Good to see you, Nathan. And" ends one sentence and starts the next in
    // the SAME fragment (video 344): the first ends at that fragment's content
    // end, the second starts at that fragment's time.
    let got = en_sentences(&[
        en_frag(59_735, " you are a mom."),
        en_frag(63_797, " Good to see you, Nathan. And"),
        en_frag(64_719, " then you can also scan"),
        en_frag(65_719, " that QR code."),
    ]);
    assert_eq!(
        got,
        vec![
            sent(59_735, 61_235, "you are a mom."),
            sent(63_797, 64_719, "Good to see you, Nathan."),
            sent(63_797, 67_219, "And then you can also scan that QR code."),
        ]
    );
}

#[test]
fn en_sentences_stay_whole_across_fragments_and_skip_blank_fragments() {
    // A blank fragment neither starts nor ends a sentence's interval; an
    // unfinished trailing sentence is still a sentence.
    let got = en_sentences(&[
        en_frag(0, " "),
        en_frag(1000, "First one"),
        en_frag(1300, " in."),
        en_frag(2000, "  "),
        en_frag(5000, "and then"),
    ]);
    assert_eq!(
        got,
        vec![
            sent(1000, 2000, "First one in."),
            sent(5000, 6500, "and then")
        ]
    );
}

#[test]
fn blank_only_en_yields_no_sentence() {
    assert!(en_sentences(&[en_frag(1000, "  "), en_frag(2000, " ")]).is_empty());
    assert!(en_sentences(&[]).is_empty());
}

#[test]
fn a_sentence_goes_to_the_line_it_overlaps_most_not_the_nearest_start() {
    // The sentence overlaps line 0 by 2000 ms and line 1 by 500 ms. Its start
    // (8000) AND its midpoint (9250) are both nearer line 1 — overlap decides.
    let got = assign(
        &[sent(8000, 10_500, "A.")],
        &[(0, 10_000), (10_000, 11_000)],
    );
    assert_eq!(got, vec!["A.", ""]);
}

#[test]
fn without_overlap_the_nearest_midpoint_wins() {
    let lines = [(0, 1000), (5000, 6000), (9000, 10_000)];
    // In the gap [3000, 4000): midpoint 3500 is 3000 from line 0's, 2000 from
    // line 1's → line 1 (the first candidate, line 0, would be wrong).
    assert_eq!(
        assign(&[sent(3000, 4000, "A.")], &lines),
        vec!["", "A.", ""]
    );
    // After every line → the last one.
    assert_eq!(
        assign(&[sent(20_000, 21_000, "A.")], &lines),
        vec!["", "", "A."]
    );
}

#[test]
fn an_overlap_tie_goes_to_the_earlier_line() {
    let lines = [(0, 2000), (2000, 4000)];
    // 500 ms in each line → the earlier one.
    assert_eq!(assign(&[sent(1500, 2500, "A.")], &lines), vec!["A.", ""]);
    // One ms more in the later line → the later one.
    assert_eq!(assign(&[sent(1500, 2501, "A.")], &lines), vec!["", "A."]);
}

#[test]
fn a_midpoint_tie_goes_to_the_earlier_line() {
    let lines = [(0, 1000), (4000, 5000)];
    // Midpoint 2500 is exactly 2000 from both line midpoints → the earlier line.
    assert_eq!(assign(&[sent(2000, 3000, "A.")], &lines), vec!["A.", ""]);
    // Half a ms later → the later line.
    assert_eq!(assign(&[sent(2001, 3000, "A.")], &lines), vec!["", "A."]);
}

#[test]
fn assignment_never_goes_back_before_the_previous_sentences_line() {
    // The second sentence overlaps only line 0, but the first already went to
    // line 1, so it stays on line 1.
    let got = assign(
        &[sent(5000, 6000, "Late."), sent(0, 1000, "Early.")],
        &[(0, 1000), (5000, 6000)],
    );
    assert_eq!(got, vec!["", "Late. Early."]);
}

#[test]
fn several_sentences_share_a_line_and_no_lines_drop_the_en() {
    assert_eq!(
        assign(
            &[sent(0, 500, "One."), sent(500, 900, "Two.")],
            &[(0, 1000), (5000, 6000)]
        ),
        vec!["One. Two.", ""]
    );
    assert_eq!(assign(&[sent(0, 500, "One.")], &[]), Vec::<String>::new());
    assert_eq!(assign(&[], &[(0, 1000)]), vec![""]);
}

#[test]
fn en_timed_is_mapped_through_at_ms_and_tempo() {
    // at_ms 100 000, tempo 2.0: SK „Prvá." (local 2000) → 101 000, „Druhá."
    // (local 4000) → 102 000; the EN at the same local times lands on the same
    // video times, so each sentence overlaps its own line. Without the tempo the
    // EN would sit at 102 000 / 104 000 (line 1 for „One."); without at_ms at
    // 1000 / 2000 (both on line 0).
    let c = with_en(
        chunk(
            0,
            Some(100_000),
            Some(2.0),
            vec![frag(2000, "Prvá."), frag(4000, "Druhá.")],
        ),
        vec![en_frag(2000, "One."), en_frag(4000, " Two.")],
    );
    let t = build(vec![c]);
    let got: Vec<(u64, &str)> = t
        .lines
        .iter()
        .map(|l| (l.start_ms, l.en.as_str()))
        .collect();
    assert_eq!(got, vec![(100_000, "One."), (101_000, "Two.")]);
}

#[test]
fn legacy_chunk_maps_en_from_start_ms() {
    // No at_ms / tempo → the EN is placed at start_ms + t_ms, like the SK.
    let c = with_en(
        chunk(
            50_000,
            None,
            None,
            vec![frag(1000, "Prvá."), frag(9000, "Druhá.")],
        ),
        vec![en_frag(1000, "One."), en_frag(9000, " Two.")],
    );
    let t = build(vec![c]);
    let got: Vec<(u64, &str)> = t
        .lines
        .iter()
        .map(|l| (l.start_ms, l.en.as_str()))
        .collect();
    assert_eq!(got, vec![(50_000, "One."), (51_000, "Two.")]);
}

/// The EN of the line whose SK is exactly `sk` (trimmed).
fn en_of(track: &LyricsTrack, sk: &str) -> String {
    track
        .lines
        .iter()
        .find(|l| l.sk.as_deref().map(str::trim) == Some(sk))
        .unwrap_or_else(|| panic!("no SK line {sk:?}"))
        .en
        .trim()
        .to_string()
}

#[test]
fn the_video_344_session_log_pairs_the_six_named_sk_lines() {
    // Real data (#184 H5): 27–100 s of video 344's session log (52 SK + 48 EN
    // fragments, already on the video timeline). With the nearest DISPLAYED
    // start the EN landed a line late — „Rád ťa vidím, Nathan." is displayed
    // from 59 672 ms but its fragment arrived at 63 922 ms — and „ Good to see
    // you, Nathan. And" was one sentence. These six pairs are the acceptance
    // set; not every line of the window pairs right yet (e.g. „Bartlesville
    // Oklahoma." gets no EN — the split-off part of „Good morning. Bartlesville"
    // starts at that fragment's earlier time and overlaps „Dobré ráno." more).
    let t: DubTranscripts =
        serde_json::from_str(include_str!("testdata/dub344_window_27_100s.json")).unwrap();
    assert_eq!(t.chunks.len(), 1);
    assert_eq!(t.chunks[0].sk_timed.len(), 52);
    assert_eq!(t.chunks[0].en_timed.len(), 48);
    let track = transcripts_to_track(&t);
    let pairs = [
        ("Rád ťa vidím, Nathan.", "Good to see you, Nathan."),
        (
            "A potom môžeš naskenovať aj ten QR kód.",
            "And then you can also scan that QR code.",
        ),
        (
            "a podať žiadosť o modlitbu.",
            "Um and get a prayer request in.",
        ),
        (
            "Takže toto je každý utorok až piatok.",
            "So this is every Tuesday through Friday.",
        ),
        (
            "o 5:30 ráno, ale asi ty si mama.",
            "at 5:30 in the morning, but I guess you you are a mom.",
        ),
        ("Dobré ráno z Montrealu.", "Good morning from Montreal."),
    ];
    for (sk, en) in pairs {
        assert_eq!(en_of(&track, sk), en, "EN of the SK line {sk:?}");
    }
}
