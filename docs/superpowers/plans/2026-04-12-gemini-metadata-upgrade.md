# Gemini Metadata Upgrade Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Fix Gemini metadata extraction (currently 0% success rate) by upgrading the model, porting the proven Python prompt, adding artist name shortening, and fixing regex parser bugs.

**Architecture:** Upgrade `gemini-2.0-flash` → `gemini-2.5-flash`, port the detailed worship-music prompt from the legacy Python code, add a `shorten_artist()` post-processor for compact Resolume display, and fix 4 systematic regex parser bugs that cause 68% incorrect fallback results.

**Tech Stack:** Rust, regex, serde_json, reqwest, wiremock (tests), sqlx (SQLite)

---

## File Structure

| File | Responsibility | Action |
|------|---------------|--------|
| `crates/sp-server/src/metadata/gemini.rs` | Gemini API prompt + response parsing | Modify: replace prompt, add `shorten_artist()` post-processing |
| `crates/sp-server/src/metadata/parser.rs` | Title regex fallback parser | Modify: fix bugs A-D, add `normalize_title()` pre-processor |
| `crates/sp-server/src/lib.rs:291` | Default model string | Modify: one-line change `gemini-2.0-flash` → `gemini-2.5-flash` |

---

### Task 1: Fix regex parser — normalize delimiters (Bugs C & D)

**Files:**
- Modify: `crates/sp-server/src/metadata/parser.rs`

- [ ] **Step 1: Write failing tests for `//`, `||`, `—`, `–` delimiters**

Add these tests at the end of the `mod tests` block in `parser.rs`:

```rust
#[test]
fn double_slash_delimiter_parsed_as_pipe() {
    let m = parse_title("Lamb of God // Church of the City // Worship Together Session");
    assert_eq!(m.song, "Lamb of God");
    assert_eq!(m.artist, "Church of the City");
}

#[test]
fn double_pipe_delimiter_parsed() {
    let m = parse_title("Joy || IBC LIVE 2025");
    assert_eq!(m.song, "Joy");
    assert_eq!(m.artist, "IBC");
}

#[test]
fn em_dash_delimiter_parsed_as_dash() {
    let m = parse_title("Shelter In — VOUS Worship");
    assert_eq!(m.song, "Shelter In");
    assert_eq!(m.artist, "VOUS Worship");
}

#[test]
fn en_dash_delimiter_parsed_as_dash() {
    let m = parse_title("IMAGEN – Genock Gabriel");
    assert_eq!(m.song, "IMAGEN");
    assert_eq!(m.artist, "Genock Gabriel");
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p sp-server --lib metadata::parser -- double_slash em_dash en_dash double_pipe`
Expected: 4 FAILED

- [ ] **Step 3: Add `normalize_title()` function and call it at the top of `parse_title()`**

Add this function above `parse_title` in `parser.rs`, and two new regexes at the top with the other statics:

```rust
static DOUBLE_SLASH_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\s*//\s*").expect("compile"));
static DOUBLE_PIPE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\s*\|\|\s*").expect("compile"));

/// Normalize exotic delimiters to standard `|` or `-` before parsing.
fn normalize_title(title: &str) -> String {
    // Order matters: normalize // and || to | first, then em/en-dash to -
    let s = DOUBLE_SLASH_RE.replace_all(title, " | ").to_string();
    let s = DOUBLE_PIPE_RE.replace_all(&s, " | ").to_string();
    let s = s.replace('—', "-").replace('–', "-");
    s
}
```

Then change the start of `parse_title` to normalize first:

```rust
pub fn parse_title(title: &str) -> VideoMetadata {
    let title = title.trim();
    if title.is_empty() {
        return VideoMetadata {
            song: String::new(),
            artist: "Unknown Artist".into(),
            source: MetadataSource::Regex,
            gemini_failed: false,
        };
    }

    let title = &normalize_title(title);
    // ... rest of function unchanged
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p sp-server --lib metadata::parser`
Expected: ALL pass

- [ ] **Step 5: Commit**

```
git add crates/sp-server/src/metadata/parser.rs
git commit -m "fix(metadata): normalize // || — – delimiters in title parser"
```

---

### Task 2: Fix regex parser — multi-pipe artist selection (Bug B)

**Files:**
- Modify: `crates/sp-server/src/metadata/parser.rs`

- [ ] **Step 1: Write failing tests for 3-segment pipe titles**

```rust
#[test]
fn three_segment_pipe_takes_last_as_artist() {
    let m = parse_title("Supernatural Love | Show Me Your Glory - Live At Chapel | Planetshakers Official Music Video");
    assert_eq!(m.song, "Supernatural Love");
    assert_eq!(m.artist, "Planetshakers");
}

#[test]
fn three_segment_pipe_planetshakers_pattern() {
    let m = parse_title("Free Indeed | REVIVAL | Planetshakers Official Music Video");
    assert_eq!(m.song, "Free Indeed");
    assert_eq!(m.artist, "Planetshakers");
}

#[test]
fn worship_together_session_pattern() {
    let m = parse_title("My Father's World | Chris Tomlin | Worship Together Session");
    assert_eq!(m.song, "My Father's World");
    assert_eq!(m.artist, "Chris Tomlin");
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p sp-server --lib metadata::parser -- three_segment worship_together`
Expected: 3 FAILED

- [ ] **Step 3: Add multi-pipe handling before the existing PIPE_RE match**

Insert this block at the start of `parse_title`, right after the `normalize_title` call and before the `// Pattern 1` comment:

```rust
    // Multi-pipe: titles with 3+ pipe segments use "Song | Middle | Artist [suffix]"
    // Take the first segment as song, the LAST segment as artist candidate.
    let pipe_segments: Vec<&str> = title.split('|').collect();
    if pipe_segments.len() >= 3 {
        let song_raw = pipe_segments[0].trim().to_string();
        // Last segment is the artist (possibly with "Official Music Video" suffix)
        let last = pipe_segments.last().unwrap().trim();
        // Second-to-last is the artist if last is a known junk suffix
        let (artist_raw, _middle) = if is_junk_segment(last) && pipe_segments.len() > 3 {
            (pipe_segments[pipe_segments.len() - 2].trim().to_string(), last)
        } else {
            (last.to_string(), "")
        };
        let artist = clean_artist_suffix(&artist_raw);
        if artist.len() > 2 {
            let song = clean_song_title(&song_raw);
            return VideoMetadata {
                song,
                artist,
                source: MetadataSource::Regex,
                gemini_failed: false,
            };
        }
    }
```

And add these two helper functions:

```rust
/// Strip "Official Music Video", "Worship Together Session", etc. from an artist string.
fn clean_artist_suffix(artist: &str) -> String {
    let mut cleaned = artist.to_string();
    for re in TRAILING_PATTERNS.iter() {
        cleaned = re.replace_all(&cleaned, "").to_string();
    }
    // Also strip "Official Planetshakers" → "Planetshakers" etc.
    cleaned = Regex::new(r"(?i)^official\s+")
        .unwrap()
        .replace(&cleaned, "")
        .to_string();
    // Remove bracket content (feat., Live, etc.)
    cleaned = BRACKET_ROUND_RE.replace_all(&cleaned, "").to_string();
    cleaned = BRACKET_SQUARE_RE.replace_all(&cleaned, "").to_string();
    cleaned = WHITESPACE_RE.replace_all(&cleaned, " ").to_string();
    cleaned = TRAILING_JUNK_RE.replace_all(&cleaned, "").to_string();
    cleaned.trim().to_string()
}

/// Check if a pipe segment is a known non-artist junk segment.
fn is_junk_segment(s: &str) -> bool {
    let lower = s.trim().to_lowercase();
    lower.contains("worship together session")
        || lower.contains("official music video")
        || lower.contains("official video")
        || lower.contains("lyric video")
        || lower.starts_with("live")
        || lower.starts_with("recorded")
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p sp-server --lib metadata::parser`
Expected: ALL pass (including old tests — no regressions)

- [ ] **Step 5: Commit**

```
git add crates/sp-server/src/metadata/parser.rs
git commit -m "fix(metadata): handle multi-pipe titles by taking last segment as artist"
```

---

### Task 3: Fix regex parser — artist truncated at `(` (Bug A)

**Files:**
- Modify: `crates/sp-server/src/metadata/parser.rs`

- [ ] **Step 1: Write failing tests for truncated artist**

```rust
#[test]
fn pipe_artist_with_feat_paren_is_cleaned() {
    let m = parse_title("Keep On | Elevation Worship (feat. Davide Mutendji)");
    assert_eq!(m.song, "Keep On");
    assert_eq!(m.artist, "Elevation Worship");
}

#[test]
fn pipe_artist_with_live_paren_is_cleaned() {
    let m = parse_title("Get This Party Started | Planetshakers (Live)");
    assert_eq!(m.song, "Get This Party Started");
    assert_eq!(m.artist, "Planetshakers");
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p sp-server --lib metadata::parser -- feat_paren live_paren`
Expected: 2 FAILED (artist will be `"Elevation Worship ("` or `"Planetshakers ("`)

- [ ] **Step 3: Apply `clean_artist_suffix()` to the artist in the 2-segment pipe path**

In `parse_title`, change the existing `// Pattern 1: "Song | Artist"` block:

```rust
    // Pattern 1: "Song | Artist" (artist_first = false)
    if let Some(caps) = PIPE_RE.captures(title) {
        let song_raw = caps[1].trim().to_string();
        let artist_raw = caps[2].trim().to_string();
        let artist = clean_artist_suffix(&artist_raw);
        if artist.len() > 2 {
            let song = clean_song_title(&song_raw);
            return VideoMetadata {
                song,
                artist,
                source: MetadataSource::Regex,
                gemini_failed: false,
            };
        }
    }
```

The key change: `clean_artist_suffix(&artist_raw)` instead of using `artist` directly. This strips `(feat. ...)`, `(Live)`, trailing `(`, etc.

- [ ] **Step 4: Run all tests**

Run: `cargo test -p sp-server --lib metadata::parser`
Expected: ALL pass

- [ ] **Step 5: Commit**

```
git add crates/sp-server/src/metadata/parser.rs
git commit -m "fix(metadata): clean artist suffix to remove truncated parens and feat"
```

---

### Task 4: Add `shorten_artist()` function

**Files:**
- Modify: `crates/sp-server/src/metadata/parser.rs` (add the public function here since both Gemini and regex paths use it)

- [ ] **Step 1: Write tests for artist name shortening**

```rust
// ---- shorten_artist tests ----

#[test]
fn shorten_personal_name_two_words() {
    assert_eq!(shorten_artist("Michael Bethany"), "M. Bethany");
}

#[test]
fn shorten_personal_name_three_words() {
    assert_eq!(shorten_artist("Martin W Smith"), "M. W. Smith");
}

#[test]
fn shorten_does_not_abbreviate_band_with_worship() {
    assert_eq!(shorten_artist("Elevation Worship"), "Elevation Worship");
}

#[test]
fn shorten_does_not_abbreviate_single_word() {
    assert_eq!(shorten_artist("Planetshakers"), "Planetshakers");
}

#[test]
fn shorten_does_not_abbreviate_band_with_music() {
    assert_eq!(shorten_artist("Maverick City Music"), "Maverick City Music");
}

#[test]
fn shorten_does_not_abbreviate_vous_worship() {
    assert_eq!(shorten_artist("VOUS Worship"), "VOUS Worship");
}

#[test]
fn shorten_handles_comma_separated_artists() {
    assert_eq!(
        shorten_artist("SEU Worship, Roosevelt Stewart, Grace Shuffitt"),
        "SEU Worship, R. Stewart, G. Shuffitt"
    );
}

#[test]
fn shorten_does_not_abbreviate_ampersand_band() {
    assert_eq!(
        shorten_artist("Bethel Music & Kristene DiMarco"),
        "Bethel Music & K. DiMarco"
    );
}

#[test]
fn shorten_does_not_touch_all_caps_acronym() {
    assert_eq!(shorten_artist("TAYA"), "TAYA");
}

#[test]
fn shorten_handles_feat_with_personal_names() {
    assert_eq!(shorten_artist("Pat Barrett"), "P. Barrett");
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p sp-server --lib metadata::parser -- shorten_`
Expected: 10 FAILED (function doesn't exist)

- [ ] **Step 3: Implement `shorten_artist()`**

Add at the bottom of `parser.rs`, above `mod tests`:

```rust
/// Words that indicate a band/group name — never abbreviate these artists.
const BAND_INDICATORS: &[&str] = &[
    "worship", "music", "church", "choir", "band", "team", "united",
    "collective", "community", "ministry", "ministries", "ensemble",
    "orchestra", "rhythm", "heights", "city",
];

/// Shorten personal artist names to initials (e.g. "Michael Bethany" → "M. Bethany").
/// Band/group names are never abbreviated. Comma-separated lists are handled per-segment.
pub fn shorten_artist(artist: &str) -> String {
    if artist.contains(',') {
        // Comma-separated list: shorten each segment independently
        return artist
            .split(',')
            .map(|s| shorten_single_artist(s.trim()))
            .collect::<Vec<_>>()
            .join(", ");
    }
    if artist.contains('&') {
        // Ampersand-separated: shorten each segment independently
        return artist
            .split('&')
            .map(|s| shorten_single_artist(s.trim()))
            .collect::<Vec<_>>()
            .join(" & ");
    }
    shorten_single_artist(artist)
}

/// Shorten a single artist name (no commas/ampersands).
fn shorten_single_artist(name: &str) -> String {
    let words: Vec<&str> = name.split_whitespace().collect();

    // Single word or empty — never abbreviate
    if words.len() <= 1 {
        return name.to_string();
    }

    // Check if any word is a band indicator
    if words.iter().any(|w| {
        BAND_INDICATORS
            .iter()
            .any(|b| w.to_lowercase() == *b)
    }) {
        return name.to_string();
    }

    // More than 3 words without a band indicator is ambiguous — don't abbreviate
    if words.len() > 3 {
        return name.to_string();
    }

    // Abbreviate all words except the last
    let mut parts: Vec<String> = Vec::new();
    for (i, word) in words.iter().enumerate() {
        if i < words.len() - 1 {
            // Take first character as initial
            let initial: String = word.chars().next().map(|c| {
                format!("{}.", c.to_uppercase().next().unwrap_or(c))
            }).unwrap_or_default();
            parts.push(initial);
        } else {
            parts.push(word.to_string());
        }
    }
    parts.join(" ")
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p sp-server --lib metadata::parser`
Expected: ALL pass

- [ ] **Step 5: Commit**

```
git add crates/sp-server/src/metadata/parser.rs
git commit -m "feat(metadata): add shorten_artist() for compact Resolume title display"
```

---

### Task 5: Port Python prompt and upgrade model default

**Files:**
- Modify: `crates/sp-server/src/metadata/gemini.rs`
- Modify: `crates/sp-server/src/lib.rs:291`

- [ ] **Step 1: Write test for the new prompt structure**

Add to the `mod tests` block in `gemini.rs`:

```rust
#[test]
fn build_request_body_contains_worship_rules() {
    let provider = GeminiProvider::new("test-key".into(), "gemini-2.5-flash".into());
    let body = provider.build_request_body("dQw4w9WgXcQ", "Test Title");
    let prompt = body["contents"][0]["parts"][0]["text"].as_str().unwrap();

    // Key rules from the Python prompt must be present
    assert!(prompt.contains("worship"), "prompt must mention worship music");
    assert!(prompt.contains("album"), "prompt must mention album names");
    assert!(prompt.contains("medley"), "prompt must mention medleys");
    assert!(prompt.contains("HOLYGHOST"), "prompt must have HOLYGHOST example");
    assert!(prompt.contains("Planetshakers"), "prompt must have Planetshakers example");
    assert!(prompt.contains("Faithful Then / Faithful Now"), "prompt must have slash example");
    assert!(prompt.contains("shorten"), "prompt must mention artist shortening");
}

#[test]
fn build_request_body_has_google_search_tool() {
    let provider = GeminiProvider::new("test-key".into(), "gemini-2.5-flash".into());
    let body = provider.build_request_body("test", "Test");
    assert!(body["tools"][0]["google_search"].is_object());
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p sp-server --lib metadata::gemini -- build_request_body_contains build_request_body_has`
Expected: FAILED (current prompt doesn't contain worship/album/medley)

- [ ] **Step 3: Replace `build_request_body()` with the full Python-ported prompt**

Replace the entire `build_request_body` method in `gemini.rs`:

```rust
    /// Build the request body for the Gemini API.
    fn build_request_body(&self, video_id: &str, title: &str) -> Value {
        let video_url = format!("https://www.youtube.com/watch?v={video_id}");
        let prompt = format!(
            "Look up information about this YouTube video and extract the artist and song title:\n\
             URL: {video_url}\n\
             Title: \"{title}\"\n\
             \n\
             Use Google Search to find information about this specific YouTube video URL.\n\
             \n\
             CRITICAL: Respond with ONLY a valid JSON object. No explanatory text allowed.\n\
             \n\
             Return EXACTLY this format:\n\
             {{\"artist\": \"Primary Artist Name\", \"song\": \"Song Title\"}}\n\
             \n\
             IMPORTANT RULES:\n\
             1. Search for the YouTube URL to find the actual artist and song information\n\
             2. For worship/church music, identify the performing artist/band (not the church name)\n\
             3. Remove feat./ft./featuring from artist name\n\
             4. Remove (Official Video), (Live), etc from song titles\n\
             5. For single songs with \"/\" in their actual title (like \"Faithful Then / Faithful Now\"), keep the full title\n\
             6. NEVER include album names in the song title - return only the actual song name\n\
             7. If the video is a medley or contains multiple distinct songs, return ONLY the first song\n\
             8. If no artist found, return empty string for artist\n\
             9. For personal artist names, shorten first/middle names to initials keeping the last name full \
                (e.g. \"Michael Bethany\" → \"M. Bethany\", \"Chris Tomlin\" → \"C. Tomlin\"). \
                NEVER abbreviate band or group names (\"Elevation Worship\" stays \"Elevation Worship\")\n\
             \n\
             Examples:\n\
             - \"HOLYGHOST | Sons Of Sunday\" → {{\"artist\": \"Sons Of Sunday\", \"song\": \"HOLYGHOST\"}}\n\
             - \"'COME RIGHT NOW' | Official Video\" → {{\"artist\": \"Planetshakers\", \"song\": \"COME RIGHT NOW\"}}\n\
             - \"Supernatural Love | Show Me Your Glory - Live At Chapel | Planetshakers Official Music Video\" → {{\"artist\": \"Planetshakers\", \"song\": \"Supernatural Love\"}}\n\
             - \"Forever | Live At Chapel\" → {{\"artist\": \"K. Jobe\", \"song\": \"Forever\"}}\n\
             - \"The Blessing (Live) | Elevation Worship\" → {{\"artist\": \"Elevation Worship\", \"song\": \"The Blessing\"}}\n\
             - \"Faithful Then / Faithful Now | Elevation Worship\" → {{\"artist\": \"Elevation Worship\", \"song\": \"Faithful Then / Faithful Now\"}}\n\
             - \"There Is A King/What Would You Do | Live | Elevation Worship\" → {{\"artist\": \"Elevation Worship\", \"song\": \"There Is A King\"}}\n\
             - \"Pat Barrett - Count On You (Live)\" → {{\"artist\": \"P. Barrett\", \"song\": \"Count On You\"}}\n\
             \n\
             REMEMBER: Return ONLY valid JSON, nothing else. The song field should contain ONLY the song title, never album names or other metadata."
        );

        serde_json::json!({
            "system_instruction": {
                "parts": [{"text": "You are a JSON API that returns only valid JSON objects. Never include explanatory text, reasoning, or any content outside the JSON structure."}]
            },
            "contents": [
                {"role": "user", "parts": [{"text": prompt}]}
            ],
            "tools": [{"google_search": {}}],
            "generationConfig": {
                "temperature": 0.1,
                "candidateCount": 1
            }
        })
    }
```

- [ ] **Step 4: Apply `shorten_artist()` post-processing in `parse_response()`**

Add this import at the top of `gemini.rs`:

```rust
use super::parser::shorten_artist;
```

Then in `parse_response()`, apply shortening after extracting the artist:

```rust
    fn parse_response(text: &str) -> Result<VideoMetadata, MetadataError> {
        let json_str = extract_json(text)?;

        let parsed: Value = serde_json::from_str(&json_str)
            .map_err(|e| MetadataError::InvalidResponse(format!("JSON parse error: {e}")))?;

        let song = parsed
            .get("song")
            .and_then(|v| v.as_str())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .ok_or_else(|| MetadataError::InvalidResponse("missing 'song' field".into()))?;

        let artist_raw = parsed
            .get("artist")
            .and_then(|v| v.as_str())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .ok_or_else(|| MetadataError::InvalidResponse("missing 'artist' field".into()))?;

        // Post-process: ensure artist name is shortened even if Gemini didn't do it
        let artist = shorten_artist(&artist_raw);

        Ok(VideoMetadata {
            song,
            artist,
            source: MetadataSource::Gemini,
            gemini_failed: false,
        })
    }
```

- [ ] **Step 5: Update the default model in `lib.rs`**

In `crates/sp-server/src/lib.rs`, line 291, change:

```rust
        .unwrap_or_else(|| "gemini-2.0-flash".to_string());
```

to:

```rust
        .unwrap_or_else(|| "gemini-2.5-flash".to_string());
```

- [ ] **Step 6: Run all tests**

Run: `cargo test -p sp-server --lib metadata`
Expected: ALL pass

- [ ] **Step 7: Commit**

```
git add crates/sp-server/src/metadata/gemini.rs crates/sp-server/src/lib.rs
git commit -m "feat(metadata): port Python prompt with worship rules, upgrade to gemini-2.5-flash"
```

---

### Task 6: Apply `shorten_artist()` to regex parser output

**Files:**
- Modify: `crates/sp-server/src/metadata/parser.rs`

- [ ] **Step 1: Write test that verifies parser output is shortened**

```rust
#[test]
fn parser_output_shortens_personal_artist() {
    let m = parse_title("Pat Barrett - Count On You (Live)");
    assert_eq!(m.song, "Count On You");
    assert_eq!(m.artist, "P. Barrett");
}

#[test]
fn parser_output_does_not_shorten_band() {
    let m = parse_title("The Blessing | Elevation Worship");
    assert_eq!(m.song, "The Blessing");
    assert_eq!(m.artist, "Elevation Worship");
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p sp-server --lib metadata::parser -- parser_output_shortens parser_output_does_not`
Expected: `parser_output_shortens_personal_artist` FAILS (artist is `"Pat Barrett"` not `"P. Barrett"`)

- [ ] **Step 3: Apply `shorten_artist()` at each return point in `parse_title()`**

In every return path where `artist` is set (multi-pipe path, PIPE_RE path, DASH_RE path), wrap the artist:

```rust
artist: shorten_artist(&artist),
```

There are three return sites in `parse_title` that have `artist` (not `"Unknown Artist"`). Change each from:

```rust
                artist,
```

to:

```rust
                artist: shorten_artist(&artist),
```

Do NOT apply to the `"Unknown Artist"` fallback.

- [ ] **Step 4: Run all tests**

Run: `cargo test -p sp-server --lib metadata::parser`
Expected: ALL pass. Note: existing test `dash_format_basic` uses "Elevation Worship" which should NOT be shortened (band indicator "Worship"), so it should still pass.

- [ ] **Step 5: Commit**

```
git add crates/sp-server/src/metadata/parser.rs
git commit -m "feat(metadata): apply shorten_artist() to regex parser output"
```

---

### Task 7: Format check, full test run, version bump, push

**Files:**
- Modify: `VERSION`

- [ ] **Step 1: Check formatting**

Run: `cargo fmt --all --check`
Expected: no formatting issues (fix if needed with `cargo fmt --all`)

- [ ] **Step 2: Run full workspace test suite**

Run: `cargo test`
Expected: ALL pass

- [ ] **Step 3: Bump version**

Check current version:
```bash
cat VERSION
```

If it's still `0.11.0` (from the merged PR), bump to next dev version:
```bash
echo "0.12.0-dev.1" > VERSION
./scripts/sync-version.sh
```

If it's already a dev version higher than main, skip the bump.

- [ ] **Step 4: Commit version bump (if needed) and push**

```bash
git add VERSION Cargo.toml Cargo.lock src-tauri/Cargo.toml src-tauri/tauri.conf.json sp-ui/Cargo.toml
git commit -m "chore: bump version to 0.12.0-dev.1 for next development cycle"
git push origin dev
```

- [ ] **Step 5: Monitor CI**

Run: `gh run list --branch dev --limit 3`
Watch until all jobs pass. Fix any failures.

---

## Spec Coverage Checklist

| Spec Section | Task |
|-------------|------|
| 1. Upgrade to gemini-2.5-flash | Task 5 step 5 |
| 2. Port Python prompt | Task 5 steps 3-4 |
| 3. Shorten artist names | Task 4 (function) + Task 5 step 4 (Gemini) + Task 6 (parser) |
| 4. Fix regex parser Bug A | Task 3 |
| 4. Fix regex parser Bug B | Task 2 |
| 4. Fix regex parser Bug C | Task 1 |
| 4. Fix regex parser Bug D | Task 1 |
| 5. Update DB setting | Handled by default change in Task 5 — the `unwrap_or_else` default is what matters; the DB setting is an override |
| 6. Trigger reprocess | Automatic — reprocess worker runs every 30 min |
