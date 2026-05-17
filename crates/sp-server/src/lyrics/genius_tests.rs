//! Tests for `lyrics::genius`. Sibling file referenced by `genius.rs`
//! under `#[path = "genius_tests.rs"] #[cfg(test)] mod tests;` to keep
//! `genius.rs` under the 1000-line airuleset cap.

use super::*;

#[test]
fn extract_lyrics_from_basic_container() {
    let html = r#"
    <html><body>
    <div data-lyrics-container="true" class="Lyrics__Container">
        [Verse 1]<br/>Line one<br>Line two<br/>
    </div>
    </body></html>
    "#;
    let track = extract_lyrics_from_html(html).expect("found lyrics");
    assert_eq!(track.source, "genius");
    let lines: Vec<&str> = track.lines.iter().map(|l| l.en.as_str()).collect();
    assert_eq!(lines, vec!["Line one", "Line two"]);
}

#[test]
fn extract_handles_multiple_containers_and_entities() {
    let html = r#"
    <div data-lyrics-container="true">Can&#39;t stop<br>the feeling</div>
    <div data-lyrics-container="true">[Chorus]<br/>Sing it &amp; mean it</div>
    "#;
    let track = extract_lyrics_from_html(html).expect("found lyrics");
    let lines: Vec<&str> = track.lines.iter().map(|l| l.en.as_str()).collect();
    assert_eq!(
        lines,
        vec!["Can't stop", "the feeling", "Sing it & mean it"]
    );
}

#[test]
fn extract_returns_none_when_no_container() {
    let html = "<html><body>no lyrics markers here</body></html>";
    assert!(extract_lyrics_from_html(html).is_none());
}

#[test]
fn section_label_detection() {
    assert!(is_section_label("[Verse 1]"));
    assert!(is_section_label("[Chorus]"));
    assert!(is_section_label("[Pre-Chorus: Artist]"));
    assert!(!is_section_label("Verse 1"));
    assert!(!is_section_label("Just a lyric line"));
}

#[test]
fn genius_banner_detection() {
    // Real banners observed on Genius pages (2026-04-23 live fetch
    // of https://genius.com/Youth-alive-the-deep-lyrics).
    assert!(is_genius_banner("1 ContributorThe Deep Lyrics"));
    assert!(is_genius_banner("37 ContributorsJesus Be The Name Lyrics"));
    assert!(is_genius_banner("Translations Türkçe Français Lyrics"));
    // False positives we must avoid:
    assert!(!is_genius_banner("Just a lyric")); // no contributor
    assert!(!is_genius_banner("Contributor")); // not ending in lyrics
    assert!(!is_genius_banner("I wrote these lyrics for you")); // real lyric
}

#[test]
fn extract_strips_contributor_banner() {
    let html = r#"
    <div data-lyrics-container="true">3 ContributorsTHE DEEP Lyrics<br/>I can't comprehend<br/>How You love</div>
    "#;
    let track = extract_lyrics_from_html(html).expect("found lyrics");
    let lines: Vec<&str> = track.lines.iter().map(|l| l.en.as_str()).collect();
    assert_eq!(lines, vec!["I can't comprehend", "How You love"]);
}

/// 2026-05-11 regression: planetboom Saints lyrics container starts with a
/// nested header div (contributor button + SVG icons + close-div) BEFORE the
/// actual `<br/>`-separated lyric lines. The pre-fix scraper used a naive
/// `find("</div>")` and stopped at the first nested close, dropping every
/// real lyric line. find_matching_div_close must count `<div` opens and
/// `</div>` closes to land on the outer container's close.
#[test]
fn extract_skips_nested_header_div_and_captures_full_lyrics() {
    let html = r#"
    <div data-lyrics-container="true"><div class="header"><button><svg><path d="M1"></path></svg></button></div>I'm not a sinner<br/>I'm a saint<br/>I am a believer</div>
    "#;
    let track = extract_lyrics_from_html(html).expect("found lyrics");
    let lines: Vec<&str> = track.lines.iter().map(|l| l.en.as_str()).collect();
    assert_eq!(
        lines,
        vec!["I'm not a sinner", "I'm a saint", "I am a believer"],
        "nested header div must not truncate the lyric capture"
    );
}

/// Verifies the depth counter handles MULTIPLE sibling nested divs (e.g.
/// header div + an annotation div) before the real lyric content. Real
/// Genius pages separate nested header / annotation content from the
/// lyric body with `<br/>` tags inside the outer container — replicate
/// that shape so `strip_html_preserving_breaks` produces clean lines.
#[test]
fn extract_handles_multiple_sibling_nested_divs() {
    let html = r#"
    <div data-lyrics-container="true"><div>A</div><div>B</div><div class="annotation"><div>nested</div>note</div><br/>Real line one<br/>Real line two</div>
    "#;
    let track = extract_lyrics_from_html(html).expect("found lyrics");
    let lines: Vec<&str> = track.lines.iter().map(|l| l.en.as_str()).collect();
    // The real lyric lines AFTER all nested-div content must be captured
    // verbatim — proving the depth counter walked past every nested
    // close before declaring the outer container closed.
    assert!(
        lines.contains(&"Real line one"),
        "depth counter must reach 'Real line one' (got: {lines:?})"
    );
    assert!(
        lines.contains(&"Real line two"),
        "depth counter must reach 'Real line two' (got: {lines:?})"
    );
}

#[test]
fn strip_html_preserves_line_breaks() {
    assert_eq!(strip_html_preserving_breaks("a<br/>b<br>c"), "a\nb\nc");
    assert_eq!(strip_html_preserving_breaks("<p>hi</p>"), "hi\n");
}

#[test]
fn pick_song_url_prefers_matching_artist() {
    let resp = SearchResponse {
        response: SearchResponseInner {
            hits: vec![
                SearchHit {
                    hit_type: "song".into(),
                    result: HitResult {
                        url: "https://genius.com/wrong-artist-song".into(),
                        primary_artist: Some(ArtistRef {
                            name: Some("Wrong Artist".into()),
                        }),
                    },
                },
                SearchHit {
                    hit_type: "song".into(),
                    result: HitResult {
                        url: "https://genius.com/right-artist-song".into(),
                        primary_artist: Some(ArtistRef {
                            name: Some("Right Artist".into()),
                        }),
                    },
                },
            ],
        },
    };
    assert_eq!(
        pick_song_url(&resp, "right artist").as_deref(),
        Some("https://genius.com/right-artist-song")
    );
}

#[test]
fn pick_song_url_returns_none_when_no_artist_match() {
    // STRICT artist match: a hit without any `primary_artist.name`
    // containing the requested artist must NOT win. Behavior changed
    // 2026-05-13 (Urban-d Who-do-you-serve incident): the previous
    // fallback-to-first-hit allowed wrong-artist pages whose lyrics
    // happened to contain the searched song's title as a reference.
    let resp = SearchResponse {
        response: SearchResponseInner {
            hits: vec![SearchHit {
                hit_type: "song".into(),
                result: HitResult {
                    url: "https://genius.com/first-hit".into(),
                    primary_artist: None,
                },
            }],
        },
    };
    assert_eq!(pick_song_url(&resp, "unknown"), None);
}

#[test]
fn pick_song_url_rejects_wrong_artist_even_when_url_looks_like_lyrics() {
    // Regression for 2026-05-13 Jireh (id=81) wrong-text incident:
    // Genius search for "New Heights Worship Jireh" returned
    // genius.com/Urban-d-who-do-you-serve-lyrics as a hit_type=song with
    // primary_artist=Urban-d. The old fallback-to-first-hit logic
    // accepted it because the URL looked song-like and the bad-pattern
    // filter doesn't trigger. The correct behavior is None — no artist
    // match, no result.
    let resp = SearchResponse {
        response: SearchResponseInner {
            hits: vec![SearchHit {
                hit_type: "song".into(),
                result: HitResult {
                    url: "https://genius.com/Urban-d-who-do-you-serve-lyrics".into(),
                    primary_artist: Some(ArtistRef {
                        name: Some("Urban-d".into()),
                    }),
                },
            }],
        },
    };
    assert_eq!(pick_song_url(&resp, "New Heights Worship"), None);
}

#[test]
fn pick_song_url_returns_none_when_artist_arg_is_empty() {
    let resp = SearchResponse {
        response: SearchResponseInner {
            hits: vec![SearchHit {
                hit_type: "song".into(),
                result: HitResult {
                    url: "https://genius.com/some-song-lyrics".into(),
                    primary_artist: Some(ArtistRef {
                        name: Some("Whoever".into()),
                    }),
                },
            }],
        },
    };
    assert_eq!(pick_song_url(&resp, ""), None);
    assert_eq!(pick_song_url(&resp, "   "), None);
}

/// Kills the `+=` → `-=`, `+=` → `*=` TIMEOUT mutants on line 257, and
/// the `+` → `*` MISSED mutant on `close_rel + 1` (line 257:28). Real
/// output is `"foo\nbar\nbaz"`; any arithmetic change either hangs
/// (TIMEOUT) or produces a different string (off-by-one leaves `>`
/// characters, or miscounts `close_rel + 1`).
#[test]
fn replace_case_insensitive_tag_handles_mixed_case_br() {
    let input = "foo<br>bar<BR>baz<Br/>end";
    let out = replace_case_insensitive_tag(input, "br", "\n");
    assert_eq!(out, "foo\nbar\nbaz\nend");
}

/// Kills `+=` → TIMEOUT mutants on line 263 (the non-tag char-advance
/// path). Exercised by any input that contains non-`<br` characters,
/// but we assert the exact output so the `+` → `*` MISSED mutant
/// on `close_rel + 1` is also killed on inputs with interleaved tags.
///
/// Multi-byte UTF-8 exercises `c.len_utf8()` — Slovak `á` is 2 bytes,
/// so mutations to the char-advance arithmetic would corrupt the
/// string slice or infinite-loop.
#[test]
fn replace_case_insensitive_tag_preserves_utf8_between_tags() {
    let input = "náš<br>dom";
    let out = replace_case_insensitive_tag(input, "br", "\n");
    assert_eq!(out, "náš\ndom");
}

/// Kills the three mutants on `replace_case_insensitive_close_tag`
/// (line 278: `+=` → `-=`/`*=` + `+` → `*`; line 283: `+=` → `*=`).
/// Real output is `"a\nb\nc"`; any mutation either hangs or produces
/// garbage output.
#[test]
fn replace_case_insensitive_close_tag_handles_closing_paragraph() {
    let input = "a</p>b</P>c";
    let out = replace_case_insensitive_close_tag(input, "p", "\n");
    assert_eq!(out, "a\nb\nc");
}

/// UTF-8 variant for the close-tag function — guards line 283
/// (`i += c.len_utf8()`) against mutants that would corrupt
/// multi-byte char boundaries.
#[test]
fn replace_case_insensitive_close_tag_preserves_utf8() {
    let input = "ľúto</p>sme";
    let out = replace_case_insensitive_close_tag(input, "p", "\n");
    assert_eq!(out, "ľúto\nsme");
}

/// Kills `&&` → `||` on `is_section_label` line 290. Under `||`,
/// any line starting with `[` OR ending with `]` would be labelled
/// as a section (and filtered out of lyrics). This test covers
/// inputs that match exactly one of the two conditions — they
/// must NOT be labelled.
#[test]
fn is_section_label_requires_both_brackets() {
    // Starts with `[` but does not end with `]` — NOT a section label.
    assert!(
        !is_section_label("[Verse 1"),
        "missing closing `]` must not be a section label"
    );
    // Ends with `]` but does not start with `[` — NOT a section label.
    assert!(
        !is_section_label("Verse 1]"),
        "missing opening `[` must not be a section label"
    );
    // Neither — obvious non-label.
    assert!(!is_section_label("just a lyric line"));
    // Both — correctly labelled.
    assert!(is_section_label("[Chorus]"));
}

/// Kills the line 190:29 `+` → `-` mutant on
/// `search_from = close + "</div>".len()`. With `-`, `search_from`
/// moves BACKWARD, which either infinite-loops or re-processes the
/// same div. Two-container input with distinct content lets us
/// assert the exact line order.
#[test]
fn extract_lyrics_from_two_containers_advances_correctly() {
    let html = r#"
    <div data-lyrics-container="true">alpha<br>bravo</div>
    <div data-lyrics-container="true">charlie<br>delta</div>
    "#;
    let track = extract_lyrics_from_html(html).expect("found lyrics");
    let lines: Vec<&str> = track.lines.iter().map(|l| l.en.as_str()).collect();
    assert_eq!(
        lines,
        vec!["alpha", "bravo", "charlie", "delta"],
        "two containers must be extracted in order with no duplicates"
    );
}

#[test]
fn genius_url_is_non_song_page_flags_known_bad_slugs() {
    let bad = [
        "https://genius.com/Christian-genius-june-2021-singles-release-calendar-annotated",
        "https://genius.com/Genius-2024-release-schedule-annotated",
        "https://genius.com/Maverick-city-music-discography-annotated",
        "https://genius.com/Spotify-top-songs-2024",
        "https://genius.com/Best-worship-songs-playlist",
    ];
    for url in bad {
        assert!(
            genius_url_is_non_song_page(url),
            "expected non-song flag for {url}"
        );
    }
}

#[test]
fn genius_url_is_non_song_page_allows_real_song_slugs() {
    let good = [
        "https://genius.com/Maverick-city-music-jireh-lyrics",
        "https://genius.com/Chris-tomlin-jesus-saves-lyrics",
        "https://genius.com/Planetshakers-the-house-lyrics",
    ];
    for url in good {
        assert!(
            !genius_url_is_non_song_page(url),
            "expected per-song slug to pass for {url}"
        );
    }
}

#[test]
fn pick_song_url_rejects_release_calendar_hit_in_favor_of_real_song() {
    // Regression for 2026-05-13 Jireh probe: Genius search returned a
    // "Christian-genius-...release-calendar-annotated" page as a
    // hit_type=song result, which then got picked up and downstream Claude
    // cleanup correctly bailed at no-lyrics. The non-song-page filter must
    // skip such hits even when they appear before the real song result.
    let resp = SearchResponse {
        response: SearchResponseInner {
            hits: vec![
                SearchHit {
                    hit_type: "song".into(),
                    result: HitResult {
                        url:
                            "https://genius.com/Christian-genius-june-2021-singles-release-calendar-annotated"
                                .into(),
                        primary_artist: Some(ArtistRef {
                            name: Some("Christian Genius".into()),
                        }),
                    },
                },
                SearchHit {
                    hit_type: "song".into(),
                    result: HitResult {
                        url: "https://genius.com/Maverick-city-music-jireh-lyrics".into(),
                        primary_artist: Some(ArtistRef {
                            name: Some("Maverick City Music".into()),
                        }),
                    },
                },
            ],
        },
    };
    assert_eq!(
        pick_song_url(&resp, "Maverick City Music").as_deref(),
        Some("https://genius.com/Maverick-city-music-jireh-lyrics")
    );
}

#[test]
fn pick_song_url_returns_none_when_only_non_song_pages_match() {
    let resp = SearchResponse {
        response: SearchResponseInner {
            hits: vec![SearchHit {
                hit_type: "song".into(),
                result: HitResult {
                    url: "https://genius.com/Genius-2025-singles-release-calendar-annotated".into(),
                    primary_artist: Some(ArtistRef {
                        name: Some("Genius".into()),
                    }),
                },
            }],
        },
    };
    assert_eq!(pick_song_url(&resp, "Genius"), None);
}

#[test]
fn pick_song_url_returns_none_when_artist_match_is_non_song_and_song_is_wrong_artist() {
    // #93 follow-up: covers the third combo that the existing
    // `pick_song_url_rejects_release_calendar_hit_in_favor_of_real_song`
    // and `pick_song_url_returns_none_when_only_non_song_pages_match`
    // tests don't exercise.
    //
    // Hit 1: bad URL (non-song page, filter trips) BUT primary_artist
    // matches the search. Filter must reject before the artist match
    // can return Some.
    // Hit 2: good URL BUT primary_artist does NOT match. Artist gate
    // must reject.
    // Expected: None — neither hit individually satisfies both
    // criteria.
    let resp = SearchResponse {
        response: SearchResponseInner {
            hits: vec![
                SearchHit {
                    hit_type: "song".into(),
                    result: HitResult {
                        url:
                            "https://genius.com/Maverick-city-music-2025-singles-release-calendar-annotated"
                                .into(),
                        primary_artist: Some(ArtistRef {
                            name: Some("Maverick City Music".into()),
                        }),
                    },
                },
                SearchHit {
                    hit_type: "song".into(),
                    result: HitResult {
                        url: "https://genius.com/Other-artist-jireh-lyrics".into(),
                        primary_artist: Some(ArtistRef {
                            name: Some("Other Artist".into()),
                        }),
                    },
                },
            ],
        },
    };
    assert_eq!(pick_song_url(&resp, "Maverick City Music"), None);
}

#[test]
fn pick_song_url_skips_non_song_hit_types() {
    // Genius returns other hit types (e.g. "lyric", "album"); we must
    // ignore them so we don't try to scrape a non-lyrics page.
    let resp = SearchResponse {
        response: SearchResponseInner {
            hits: vec![
                SearchHit {
                    hit_type: "album".into(),
                    result: HitResult {
                        url: "https://genius.com/album-not-song".into(),
                        primary_artist: Some(ArtistRef {
                            name: Some("Artist".into()),
                        }),
                    },
                },
                SearchHit {
                    hit_type: "song".into(),
                    result: HitResult {
                        url: "https://genius.com/actual-song".into(),
                        primary_artist: Some(ArtistRef {
                            name: Some("Artist".into()),
                        }),
                    },
                },
            ],
        },
    };
    assert_eq!(
        pick_song_url(&resp, "artist").as_deref(),
        Some("https://genius.com/actual-song")
    );
}

// -----------------------------------------------------------------
// find_matching_div_close mutation-killers
//
// The nested-div parser landed in commit 78858a0 to fix the Saints
// truncation bug. The two existing extract_* tests exercise it via
// the public HTML scraper, but mutation testing surfaced 7 surviving
// mutants on the bound checks (lines 257, 264, 275) and the
// separator-char OR chain (line 277). These tests call the helper
// directly with synthetic byte sequences that distinguish real from
// mutated behavior on each guarded condition.

#[test]
fn find_matching_div_close_recognizes_div_followed_by_close_bracket() {
    // Kills 277:53 (`==` for b'>') and the OR mutants gating it.
    let html = "<div>noop</div>OUTER</div>";
    let close_pos = find_matching_div_close(html, 0).expect("must find outer close");
    assert_eq!(close_pos, html.rfind("</div>").unwrap());
}

#[test]
fn find_matching_div_close_recognizes_div_followed_by_whitespace() {
    // Kills 277 `==` mutants for each of space / tab / newline / CR
    // and the OR mutants gating them. Each iteration places a fresh
    // nested `<div{sep}...>` inside the outer container; mutated
    // code would fail to count the open and mis-detect the close.
    for sep in [' ', '\t', '\n', '\r'] {
        let html = format!("<div{sep}class=\"x\">noop</div>OUTER</div>");
        let close_pos = find_matching_div_close(&html, 0)
            .unwrap_or_else(|| panic!("must find outer close with sep={sep:?}"));
        assert_eq!(close_pos, html.rfind("</div>").unwrap(), "with sep={sep:?}");
    }
}

#[test]
fn find_matching_div_close_rejects_non_div_tag_starts() {
    // Kills 277 mutants on the separator-char check: `<divider>` shares
    // the `<div` prefix but the next byte is `i` (not whitespace / `>`),
    // so depth must NOT increment. With a mutated `||` → `&&` the
    // function would treat `<divider>` as a nested open, causing the
    // outer close to be mis-detected.
    let html = "<divider>noop</divider>OUTER</div>";
    let close_pos = find_matching_div_close(html, 0).expect("outer close must be found");
    assert_eq!(close_pos, html.rfind("</div>").unwrap());
}

#[test]
fn find_matching_div_close_returns_none_on_unbalanced_html() {
    // Kills 257:13 (`<` → `<=`): mutated while bound `i <= bytes.len()`
    // would read bytes[bytes.len()] on the loop exit iteration → OOB
    // panic. Real code exits cleanly and returns None.
    let html = "<div><div>noise without any close";
    assert!(find_matching_div_close(html, 0).is_none());
}

#[test]
fn find_matching_div_close_handles_trailing_open_at_buffer_end() {
    // Kills 275:27 (`<` → `<=`): the open-branch bound check. With
    // `<=`, when the string ends with `<div` (exactly 4 bytes), the
    // mutant admits the branch, slice access bytes[i..i+4] succeeds,
    // then bytes[i + open.len()] reads past the buffer → panic. Real
    // code rejects the open and returns None.
    let html = "<div>content<div";
    assert_eq!(find_matching_div_close(html, 5), None);
}

#[test]
fn find_matching_div_close_finds_close_when_initial_i_is_small() {
    // Kills 264:14 (`+` → `-`) and 275:14 (`+` → `-`): with subtraction,
    // `i - close.len()` (or `i - open.len()`) underflows in usize for
    // small i, making the bound check always false. The close-branch
    // would be skipped entirely and the function would return None
    // instead of finding the close at byte 5.
    let html = "abcde</div>";
    let close_pos = find_matching_div_close(html, 0)
        .expect("must find </div> when i is smaller than close.len()");
    assert_eq!(close_pos, 5);
}
