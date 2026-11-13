---
paths:
  - "crates/sp-server/src/ai/**"
  - "crates/sp-core/src/config.rs"
  - "crates/sp-core/src/ai_tasks*.rs"
  - "crates/sp-server/src/api/ai_openrouter*.rs"
  - "sp-ui/src/components/ai_task_settings.rs"
  - "e2e/*openrouter*.ts"
  - "e2e/settings-ai-tasks.spec.ts"
---
# Text AI: every text task on OpenRouter (#321)

Every text AI task — the EN→SK translation, the lyrics register check,
the metadata chain's text provider, the YouTube-description extraction,
the scraped-lyrics cleanup and the Spotify track-id resolution — goes to
OpenRouter's OpenAI-compatible chat API. The owner's ruling (#232,
13.11.2026) and the blind translation A/B (#321 comment 6856550942:
Claude Opus 5.5 7.61, Gemini 4 Hydrogen 7.50, Carbon 7.59 over 17 with
one failed song — "no worse") flipped it; the Claude-via-CLIProxyAPI path
is DELETED, not kept as a fallback (the proxy spawn / adopt / watchdog /
login routes, `/api/v1/ai/status`, `DEFAULT_AI_MODEL` and the `ai_model`
setting, the `claude_proxy` provider choice, CI's proxy gate,
`scripts/cliproxy/`). Never re-add a second transport beside it.

## The client (`ai/client.rs` + its child `ai/route.rs`, `ai/openrouter.rs`)

- `AiClient::new(pool)` (production, `lib.rs`): OpenRouter at
  `sp_core::ai_tasks::OPENROUTER_API_URL`. Every call reads, LIVE from the
  database (no restart): the task's model `ai_model_<task>` (blank =
  `DEFAULT_OPENROUTER_MODEL` = `google/gemini-4-hydrogen`), the paid-AI
  switch (`paid_ai::enabled`, asked again at the transport: refused with
  `paid_ai::OFF_REASON`, nothing sent) and `openrouter_api_key` (blank =
  refused naming the setting, nothing sent).
- A text call site names its `AiTask` (`sp_core::ai_tasks`): `chat_for(task,
  …)` / `chat_json_for`. A log line about a call names the model the call
  USED: read it once (`model(task)`), send with `chat_with(task, &model,
  …)` (the translator's refusal log). An error carries `"<task> via
  OpenRouter (<model>)"` as its context: format it with `{e:#}` where it
  is reported (the metadata provider does), else only the context shows.
- An old stored `ai_provider_<task>` row (`claude_proxy` / `openrouter`)
  is no longer read: every task goes to OpenRouter. A settings PATCH stores
  a task's model trimmed (`ai_tasks::checked_setting`); any other key
  passes as before.
- `ai/openrouter.rs` is the ONE transport: Bearer key, the request
  `{model, messages, temperature 0.1, max_tokens}`, the answer decoded into
  typed structs (never `serde_json::Value`'s own Deserialize), an error
  naming the status + `redacted_excerpt` (the key cut out BEFORE the
  300-char cut), never the key.
- Tests: `crate::ai::test_support::{ai_client_at, ai_client_on,
  ai_client_unused}` — a client on a mock server root (`{uri}/v1`) with
  its own in-memory DB holding a test key (paid AI on) and no real waits;
  a call site's test mocks `POST {root}/chat/completions`.

## Retries (`ai/retry.rs`, #145)

- 429 / 5xx: the answer's `Retry-After` (whole seconds, ≤ 120 s), else
  5 / 20 / 60 s — 3 retries, 85 s in all. Anything else fails at once.
- #321 (the A/B's failed Carbon song): a 2xx whose
  `choices[0].message.content` is empty or absent is a typed `NoText`,
  retried on the same fallback waits (`RetryPolicy::fallback_delay`); the
  last one fails `"OpenRouter answered no text … from <model>, every
  attempt"`, never a silent empty result.
- OpenRouter's moderation refusal (a 403 naming `reasons` /
  `flagged_input` / moderation, `is_moderation_refusal`) is typed
  `ContentFiltered`: final, never retried. A caller asks
  `retry::content_filtered(&err)` by TYPE (never by text): the scraped-
  lyrics cleanup leaves out that candidate (`text_candidate`) and
  remembers the refusal for that raw text (`lyrics::cleanup_refusal`), a
  refused translation keeps the English lines and the missing pass skips
  it for the run (`LyricsWorker::translation_refused`), the register check
  answers refused.
- The policy is injected: production `RetryPolicy::SPANNING`; tests
  `RetryPolicy::NO_WAIT` (`AiClient::with_retry_policy`).
- A failing call holds its worker 85 s on the fallback waits, up to 360 s
  with the largest `Retry-After` (3 × 120 s). The metadata probe bounds
  each provider at 180 s.

## Prompts and refusals

The translator's and the register check's prompts are bare neutral tasks
(numbered lines in, numbered lines out): the "story" framing was refused
by the Claude flagships (#145), and a refusal is fixed in
`translator::build_prompt`, never by a fallback model. A refusal is
classified + logged `kind="refusal"` with the model id
(`translator::classify_zero_translation`). Any prompt change is A/B'd
blind before it ships (below).

## Live gate and the A/B

- `POST /api/v1/ai/openrouter/probe {"model"?}` (always 200, `ok:false` +
  why) sends one tiny completion through the tasks' own call;
  `e2e/post-deploy-openrouter.spec.ts` runs it on SNV (decision
  `e2e/openrouter-gate.ts`); PP's subset does not (paid AI off).
  The metadata chain's text provider is also probed live by
  `e2e/post-deploy-metadata.spec.ts` (`[openrouter, gemini]`).
- The blind translation A/B — the evidence for any later model change:
  `POST /api/v1/ai/translate-ab {"youtube_ids", "candidates"?: [{model}]}`
  (OpenRouter models; default the first run's three,
  `translate_ab::DEFAULT_CANDIDATES`; 409 while paid AI is off or a run
  holds it), `GET` = the state. Every song is read (lyrics file, row, ≥ 1
  line) BEFORE the first paid call. It writes
  `<cache>/ab/translate-<ms>/blind.md` (letters shuffled afresh PER SONG
  from an OS-random seed; a failed call is a neutral `(chyba)`) and
  `key.json` (per song, letter → model + the call's error) apart — open
  the key only after grading. Each call writes ≤ `CALL_MAX_TOKENS` (8 000);
  a `RunGuard` frees the slot if the task dies.
- Model choice: the owner's rule is the newest model, re-verified AS OF
  TODAY on OpenRouter's model list, switched per task in Nastavenia (no
  code change) only after a blind A/B shows it no worse.

## Nastavenia

"Textové AI (OpenRouter)" (`ai_task_settings.rs`): one model field per
task (placeholder = the default), each sent only when changed against the
loaded value (`ai_tasks::form_values` / `settings_to_send`); mock spec
`settings-ai-tasks.spec.ts`.

## The providers table (#232)

`docs/ai-providers.md` lists every external AI / lyrics provider, its
model, its credential setting and the post-deploy gate that checks it
live. A change of provider, model or gate updates it in the same push.

## Leftovers on the boxes

A SongPlayer from before #321 spawned `CLIProxyAPI.exe` as a child; the
deploy steps (ci.yml "Deploy SongPlayer" + the E2E restart,
deploy-pp.yml "Stop SongPlayer") still `taskkill` such a leftover, so it
never runs on unowned; those lines go once SNV and PP both run a build of
0.93.0 or later. `C:\ProgramData\SongPlayer\cache\.cli-proxy-api\` (the old
Claude OAuth tokens) and the old `ai_model` / `ai_api_url` /
`ai_provider_<task>` settings rows are no longer read by anything; the
session's revocation and the rows' removal are the owner's / a migration's
follow-up, never a deploy step.
