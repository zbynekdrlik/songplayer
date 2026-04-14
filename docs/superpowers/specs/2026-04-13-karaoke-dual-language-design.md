# Karaoke Dual-Language Lyrics — Design Spec

## Goal

Display synchronized karaoke lyrics in two languages (English original + Slovak translation) during video playback, with word-level highlighting. Lyrics are acquired automatically from online sources or local AI transcription, aligned to audio with high precision, translated via Gemini, and displayed on Resolume LED walls and the SongPlayer dashboard.

## Context

SongPlayer is used in a Slovak worship/event context where the audience is mixed-language. Many songs are in English but not all attendees are fluent. A bilingual karaoke display removes the comprehension barrier. The system must be fully automated — no manual lyrics entry, no per-song configuration.

## Architecture Overview

Two new subsystems:

1. **Lyrics Worker** — background processor that acquires, aligns, translates, and persists lyrics for each cached song. Runs sequentially, throttled, on the production machine.
2. **Lyrics Renderer** — real-time component that reads persisted lyrics and drives display surfaces (Resolume clips, dashboard panel) synchronized to playback position.

## Lyrics Acquisition Pipeline

### Source Waterfall

For each song, try sources in order until lyrics text is obtained:

1. **LRCLIB** (`lrclib.net`) — free, open API, no authentication. Returns line-synced LRC format (~3M entries). Query by artist + song name + duration. Best first source because it's free, reliable, and often has pre-synced timestamps.

2. **YouTube subtitles** — download via yt-dlp `--write-subs --sub-format json3 --sub-lang en`. Check for manual (creator/community) subtitles first, fall back to auto-generated. The json3 format contains segment-level timing. Many official worship channels have manual subtitle tracks.

3. **Qwen3-ASR-1.7B** (local GPU) — last resort when no lyrics text exists anywhere. Transcribes directly from the audio.flac sidecar. Apache 2.0 licensed. ~3.4GB VRAM in FP16. Handles singing + background music natively (14.6% WER on songs with BGM, far better than Whisper's 20-30%).

### Forced Alignment

After lyrics text is obtained, align to the audio for word-level timestamps:

- **Qwen3-ForcedAligner-0.6B** — Apache 2.0, ~1.2GB VRAM in FP16. Input: lyrics text + audio.flac. Output: per-word `start_ms` / `end_ms`. Average alignment error: 32-43ms (imperceptible for display). Handles singing voice natively.
- **Always run alignment** even when LRCLIB returned pre-synced LRC — LRCLIB only provides line-level timestamps, but we want word-level for the "bouncing ball" highlighting. Use LRCLIB text as input to the aligner.
- When YouTube json3 subtitles are used as source, still run forced alignment against the audio.flac for better precision.

### Translation

- **Gemini 2.5 Flash** via existing Gemini API integration.
- Translate the full song at once (not line-by-line) for grammatical coherence.
- Preserve exact line count — each Slovak line corresponds to the same English line.
- Worship glossary in system prompt:
  - Keep untranslated: Hallelujah, Hosanna, Amen, Selah, Maranatha, Emmanuel
  - Translate: Jesus→Ježiš, Christ→Kristus, Lord→Pán, God→Boh, grace→milosť, Holy Spirit→Duch Svätý, Lamb of God→Baránok Boží, salvation→spasenie, faith→viera, mercy→milosrdenstvo, glory→sláva, kingdom→kráľovstvo, cross→kríž
- Instruct model to keep lines concise (≤45 characters) for LED wall readability.
- Slovak text is typically 10-20% longer than English — prompt accounts for this.

## Storage

### Sidecar File

Each song with lyrics gets a `{youtube_id}_lyrics.json` sidecar file alongside the existing video/audio sidecars in the cache directory.

```json
{
  "version": 1,
  "source": "lrclib+aligner",
  "language_source": "en",
  "language_translation": "sk",
  "lines": [
    {
      "start_ms": 1500,
      "end_ms": 4200,
      "en": "Amazing grace how sweet the sound",
      "sk": "Predivná milosť jak ľúby to zvuk",
      "words": [
        { "text": "Amazing",  "start_ms": 1500, "end_ms": 1920 },
        { "text": "grace",    "start_ms": 1920, "end_ms": 2340 },
        { "text": "how",      "start_ms": 2340, "end_ms": 2520 },
        { "text": "sweet",    "start_ms": 2520, "end_ms": 2890 },
        { "text": "the",      "start_ms": 2890, "end_ms": 3050 },
        { "text": "sound",    "start_ms": 3050, "end_ms": 4200 }
      ]
    }
  ]
}
```

- `source`: indicates which sources contributed — e.g. `"lrclib"`, `"lrclib+aligner"`, `"youtube+aligner"`, `"asr+aligner"`
- `words`: always present — forced alignment runs for all sources to ensure word-level timestamps
- `sk`: present when translation succeeded, absent on translation failure (display shows EN only)

### Database

Migration V5 adds:

```sql
ALTER TABLE videos ADD COLUMN has_lyrics INTEGER NOT NULL DEFAULT 0;
ALTER TABLE videos ADD COLUMN lyrics_source TEXT;
```

- `has_lyrics = 1` when `{youtube_id}_lyrics.json` exists and is valid
- `lyrics_source`: `"lrclib"`, `"youtube"`, `"asr"`, or null

### Cache Scan

`self_heal_cache` startup scan extended to detect orphaned `_lyrics.json` files (lyrics file exists but no matching video+audio pair) and delete them. Also re-links lyrics files to DB rows.

## Lyrics Worker

### Lifecycle

- Spawns at server startup alongside download worker and reprocess worker.
- Runs in a dedicated tokio task with its own shutdown receiver.
- Processes songs sequentially, one at a time.
- 5-second pause between songs to avoid overloading production GPU/CPU.

### Processing Queue

```sql
SELECT v.id, v.youtube_id, v.song, v.artist, v.duration_ms, v.audio_file_path
FROM videos v
JOIN playlists p ON p.id = v.playlist_id
WHERE v.normalized = 1 AND v.has_lyrics = 0 AND p.is_active = 1
ORDER BY v.id
LIMIT 1
```

Processes all existing songs without lyrics + new songs as they're normalized. On first startup after upgrade, all 227 existing songs enter the queue.

### ML Model Management

Both Qwen3 models are managed via a Python helper script, similar to how yt-dlp/ffmpeg are managed as subprocesses:

- **Tools manager** extended to download:
  - Python (if not available) — or use system Python
  - A `lyrics_worker.py` script bundled with the app
  - Qwen3 model weights downloaded on first use via HuggingFace `huggingface-cli download`
- **Subprocess invocation**: `python lyrics_worker.py align --audio path.flac --text "lyrics..." --output path_lyrics.json`
- **VRAM management**: Models loaded per-invocation. The Python process starts, loads model, processes one song, exits. This frees VRAM between songs for Resolume.
- **Fallback**: If GPU is unavailable or out of memory, fall back to CPU (slower but functional).

### Worker Flow (per song)

1. Query DB for next unprocessed song (normalized=1, has_lyrics=0, active playlist)
2. **LRCLIB lookup**: `GET https://lrclib.net/api/get?artist_name={artist}&track_name={song}&duration={duration_s}` — if `syncedLyrics` field present, parse LRC timestamps
3. If LRCLIB miss: **YouTube subtitles**: run `yt-dlp --write-subs --write-auto-subs --sub-format json3 --sub-lang en --skip-download -o {tempdir}/{youtube_id} {url}`, parse json3 for text + timing
4. If YouTube miss: **Qwen3-ASR**: `python lyrics_worker.py transcribe --audio {audio_file_path} --output {temp_text}`
5. **Forced alignment** (always — ensures word-level timestamps regardless of source): `python lyrics_worker.py align --audio {audio_file_path} --text {text} --output {lyrics_json_path}`
6. **Gemini translation**: send full EN lyrics to Gemini with worship glossary prompt, receive SK translation, merge into lyrics JSON
7. **Persist**: write `{youtube_id}_lyrics.json`, update DB `has_lyrics=1, lyrics_source={source}`
8. Sleep 5 seconds, process next

### Error Handling

- LRCLIB API timeout/error: skip to next source (not fatal)
- YouTube subtitle download failure: skip to next source
- Qwen3-ASR failure (OOM, crash): log error, set `lyrics_source = "failed"`, move to next song. Retry on next startup.
- Forced alignment failure: fall back to line-level timestamps from source (degrade gracefully)
- Gemini translation failure: persist EN lyrics without SK translation. Display shows EN only.
- All sources fail: log, move on. Song plays without karaoke overlay (existing behavior).

## Lyrics Renderer

### Integration Point

The `PlaybackEngine` already broadcasts `ServerMsg::NowPlaying { playlist_id, video_id, position_ms, duration_ms, ... }` every 500ms. The lyrics renderer hooks into this existing broadcast.

### Server-Side Lyrics State

When a new video starts playing (`on_video_started`):

1. Check if `{youtube_id}_lyrics.json` exists (via `has_lyrics` DB flag)
2. If yes, load and parse the JSON into memory — store in `PlaylistPipeline` alongside `cached_song`/`cached_artist`
3. If no, set lyrics state to None (no karaoke for this song)

On each position broadcast:

1. Find the current lyrics line where `start_ms <= position_ms < end_ms`
2. If line changed since last broadcast, emit to display surfaces
3. Include word-level highlight index (which word is currently active based on position_ms within the line)

### WebSocket Message

New `ServerMsg` variant for lyrics updates:

```rust
ServerMsg::LyricsUpdate {
    playlist_id: i64,
    line_en: Option<String>,
    line_sk: Option<String>,
    prev_line_en: Option<String>,
    next_line_en: Option<String>,
    active_word_index: Option<usize>,
    word_count: Option<usize>,
}
```

Sent alongside `NowPlaying` on the same 500ms interval. Only sent when lyrics are available. `active_word_index` indicates which word in `line_en` should be highlighted.

### Resolume Output

Two new clip tokens discovered by the existing Resolume driver:

- **`#sp-subs`** — receives current English lyrics line as plain text
- **`#sp-subssk`** — receives current Slovak lyrics line as plain text

Uses the same `set_text_all()` parallel multi-clip update as `#sp-title`. Text updates on line change only (not every 500ms). When no lyrics are available or between lines, text is cleared to empty string.

No fade in/out animation for lyrics (unlike title display) — lyrics should appear/disappear instantly for readability.

### Dashboard Display

Inline karaoke panel inside each playlist card (Option A from mockup):

- **Previous line**: dimmed, small font
- **Current EN line**: bright white, larger font, active word highlighted in accent color (red/underline)
- **Current SK line**: blue, below EN line
- **Next line**: dimmed, small font
- Panel hidden when no lyrics available or playback stopped
- Per-playlist toggle to show/hide karaoke panel (stored as playlist setting)

Word highlighting: the `active_word_index` from the server determines which word gets the accent style. Words before the active index shown in medium brightness, words after shown dimmer — creating a "follow along" visual flow.

## sp-core Types

New shared types (WASM-safe):

```rust
pub struct LyricsLine {
    pub start_ms: u64,
    pub end_ms: u64,
    pub en: String,
    pub sk: Option<String>,
    pub words: Option<Vec<LyricsWord>>,
}

pub struct LyricsWord {
    pub text: String,
    pub start_ms: u64,
    pub end_ms: u64,
}

pub struct LyricsTrack {
    pub version: u32,
    pub source: String,
    pub lines: Vec<LyricsLine>,
}
```

## Python Helper Script

A `lyrics_worker.py` script bundled in the tools directory handles all ML inference:

```
lyrics_worker.py transcribe --audio <path> --output <path>    # Qwen3-ASR
lyrics_worker.py align --audio <path> --text <text> --output <path>  # Qwen3-ForcedAligner
lyrics_worker.py check-gpu                                     # Verify CUDA available
lyrics_worker.py download-models                               # Pre-download models
```

- Uses `transformers` + `torch` from PyTorch with CUDA
- Models cached in `{tools_dir}/models/` (HuggingFace cache)
- Falls back to CPU if CUDA unavailable
- JSON output to stdout or file

## Per-Playlist Karaoke Toggle

New playlist setting:

```sql
ALTER TABLE playlists ADD COLUMN karaoke_enabled INTEGER NOT NULL DEFAULT 1;
```

- Default: enabled for all playlists
- When disabled: lyrics worker still processes songs (pre-cached), but renderer doesn't send to Resolume or dashboard
- Controllable via dashboard settings and API

## API Routes

New endpoints under `/api/v1/`:

- `GET /api/v1/videos/{id}/lyrics` — returns lyrics JSON for a video
- `POST /api/v1/videos/{id}/lyrics/reprocess` — re-queue a video for lyrics processing
- `GET /api/v1/lyrics/status` — returns processing queue status (total, processed, pending)
- `PATCH /api/v1/playlists/{id}` — extended to accept `karaoke_enabled` field

## E2E Testing

### Playwright E2E

- Start playback of a song with known lyrics fixture
- Verify karaoke panel appears in dashboard with EN + SK text
- Verify word highlighting advances over time (check `active_word_index` changes)
- Verify karaoke panel hidden when playback stopped

### Unit Tests

- LRCLIB response parsing (synced and plain)
- YouTube json3 subtitle parsing
- Lyrics JSON serde roundtrip
- Line lookup by position_ms (binary search)
- Word index calculation within a line
- Gemini translation prompt construction
- Lyrics worker queue query

## Out of Scope

- OBS text source for lyrics (title bridge keeps showing song/artist)
- NDI burned-in text overlay
- Word-level highlighting for Slovak translation (only EN gets word-level)
- User-uploaded custom lyrics/translations
- Languages other than Slovak
- Real-time Whisper/ASR transcription during playback
- Musixmatch integration (gray-area API access)
