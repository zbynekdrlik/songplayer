# AI and lyrics-source providers

The external services SongPlayer calls, what for, which model, which
setting holds the credential, and which post-deploy gate checks each one
live (#232; the owner's rule of 29.9.2026: every external provider and
model gets a live check). Verified on 10.10.2026 against the code, the
settings on SNV and PP and Gemini's model list. Never print a key: read
key NAMES only.

## Paid AI (the `paid_ai_enabled` switch; OFF at PP, see `.claude/rules/paid-ai.md`)

| Provider | Used for | Model | Credential (setting) | Live gate |
|---|---|---|---|---|
| Claude through CLIProxyAPI (port 18787, an OAuth credential the proxy holds) | metadata (first), lyrics text cleanup, translation EN→SK, the description extraction, each song's Spotify track id (`spotify_resolver`, once per song) | `claude-opus-5-5` (`sp_core::config::DEFAULT_AI_MODEL`) | none in SongPlayer's DB | ci.yml "Verify AI proxy healthy and not churning": a real completion with the model SongPlayer sends, and no newer `claude-opus-*` listed |
| Google Gemini | metadata fallback (after Claude) | `gemini-3.1-pro-preview` (the stored `gemini_model`, newest Gemini Pro on 10.10.2026; SNV and PP store it, so a new `DEFAULT_GEMINI_MODEL` reaches them only through a settings PATCH) | `gemini_api_key` (a list; one key per request) | `e2e/post-deploy-metadata.spec.ts` (`POST /api/v1/metadata/probe`) |
| Google Gemini | lyrics: the one transcript per song (base tier and the reference gate) | `gemini-3.5-transcribe` (`g35t_client::MODEL_SLUG`) | `gemini_api_key` | `e2e/post-deploy-g35t.spec.ts` (`POST /api/v1/lyrics/g35t/probe`) |
| Google Gemini Live | dabing (Slovak dub) | `gemini-3.5-live-translate-preview` (`dub_model`, `DEFAULT_DUB_MODEL`) | `gemini_api_key` (the first key) | none yet: #232 unit 2 |

## Free lyrics sources

| Provider | Used for | Credential | Live gate |
|---|---|---|---|
| Genius | community lyrics (gather's fallback, the title search) | `genius_access_token` | `e2e/post-deploy-genius.spec.ts` (#232, through `POST /api/v1/lyrics/probe-sources`): fails on any error answer — a refused search (a dead token, a block, a limit), an unreadable answer, a refused song page, a matched page with no lyric |
| LRCLIB | synced / plain lyrics | none | none (public; outages are common, a gate would block deploys on a third party) |
| lyrics.ovh | plain lyrics | none | none (public) |
| YouTube (yt-dlp) | captions, descriptions, downloads | the cookie file (`.claude/rules/youtube-cookies.md`) | none yet: only the deno check (`tools.js_runtime_ok`), which passes on a bot check; a download / cookie gate is the next #232 unit |
| Spotify lyrics proxy | line-synced lyrics: the worker resolves each song's track id through Claude by itself (above), then fetches the proxy | none | none (a public, unauthenticated third-party proxy; a gate would block deploys on its outages, like LRCLIB) |

## Stored but unused (delete with the key rotation, #232 / #229; not before 21.10.2026)

`assemblyai_api_key`, `replicate_api_token`, `dashscope_api_key`,
`elevenlabs_api_key`, `openrouter_api_key`, `soniox_api_key`: no code reads
them. Their DB rows go with the owner's key revocation (one owner action).
