---
paths:
  - "crates/sp-core/src/config.rs"
  - "crates/sp-server/src/api/settings*.rs"
  - "crates/sp-server/src/peer/config*.rs"
  - "sp-ui/src/components/settings_form.rs"
  - "sp-ui/src/api.rs"
  - "e2e/mock-api.mjs"
  - "e2e/settings-*.spec.ts"
  - "e2e/post-deploy-settings-masked.spec.ts"
  - "eval/dubbing/voice_band_measure.py"
---

# Secret settings (#229): masked in the API, in clear only in the database

The settings table holds API keys, tokens and passwords, and
`GET /api/v1/settings` answers anyone on the LAN without a login. So the API
never shows a secret; only the database holds it in clear.

## Which settings are secret

- ONE list: `sp_core::config::SECRET_SETTINGS` = `gemini_api_key`,
  `genius_access_token`, `obs_websocket_password`, `peer_api_key`,
  `remote_ws_password`.
- `is_secret_setting(key)` = on that list, OR named `*_key` / `*_token` /
  `*_password` / `*_secret` (`SECRET_SETTING_SUFFIXES`). Why the name rule:
  the table keeps every row ever written (a retired provider's
  `replicate_api_token` / `assemblyai_api_key` — #159 deleted the code, not
  the rows), and GET lists every row.
- A new secret goes on the list. If its name lacks those suffixes, also give
  it its own `is_secret_setting` test: otherwise only the list's exact-slice
  test (`the_secret_settings_list`) pins it. Update the two JS copies of the
  rule with it: `e2e/mock-api.mjs` (`SECRET_SETTINGS`,
  `SECRET_SETTING_SUFFIXES`) and `e2e/post-deploy-settings-masked.spec.ts`
  (the same two lists).
- `peers` (the exchange's peer list, `peer-exchange.md`) is not on the list:
  its secrets sit INSIDE its JSON, masked field by field.

## GET (`api/settings.rs::shown`)

- A non-blank secret reads `********` (`SECRET_MASK`). A blank one reads as
  stored: it reveals nothing, and the form shows an empty field.
- `peers` reads with each peer's `key` and `cf_client_secret` as the mask
  (`peer::config::shown_peers`). A stored value that does not parse as a peer
  list reads as `********` whole: it is never echoed.

## PATCH (`api/settings.rs::prepare`)

- A value exactly the mask for a masked setting (a secret, or `peers`) keeps
  the stored value: nothing is written for that key, and the exchange checks
  do not count it as sent. Any other value replaces the stored one; `""`
  clears it.
- A peer sent back inside `peers` with a masked `key` / `cf_client_secret`
  takes the stored secret of the stored peer of the same name
  (`peer::config::unmask_peers`); a masked secret with no such stored peer is
  refused.
- A masked peer secret stays with what it was stored for: a masked `key`
  only with the stored `base_url`, a masked `cf_client_secret` only with the
  stored `base_url` AND `cf_client_id`; else 400 ("send its key again" /
  "send cf_client_secret again", naming only the peer;
  `a_masked_peer_key_cannot_follow_a_new_base_url`). The PATCH has no login
  and the router answers any origin (`CorsLayer::permissive`), so a page on
  the LAN could otherwise re-point a peer at its own host and have the node
  send it the stored key and Cloudflare token. A secret sent in clear is
  taken as sent (`peer/config_tests.rs`:
  `unmask_takes_the_stored_secret_of_the_same_peer`,
  `a_masked_cf_secret_stays_with_its_client_id`).
- An exchange setting that does not hold (`node_name`, `peer_api_key`,
  `peers`: `peer::config::checked`) refuses the whole PATCH: 400 with the
  reason, and NOTHING is written. The reason names keys, peers and positions,
  never a secret (a JSON error gives only line and column: serde's own text
  can quote the input).
- Success = 204 with no body.
- `UpdateSettingsRequest` has no `Debug`: its map carries secrets in clear.

## Tests never print a secret, not even a fixture one

- Fixture secrets contain `example` (the staging scan's placeholder rule)
  and stay short; no hex or long alphanumeric runs.
- A failing assertion must not print a value that holds one: `unwrap_err()`
  on an `Ok` holding a peer list becomes `match … { Ok(_) => panic!(…) }`,
  and a no-echo check (`!text.contains(KEY)`) runs BEFORE any assertion
  whose message prints the response text.

## Who reads the secrets in clear

- The workers — lyrics (Gemini, Genius), metadata, dabing, the OBS client,
  the Companion facade — read the database (`db::models::get_setting`), never
  the API, so the mask never reaches them.
- A new reader of a secret reads the database the same way. Through the API
  it would get `********`.

## The Nastavenia form (`sp-ui` `settings_form.rs`)

- Its password fields show the mask. An untouched field sends the mask back
  unchanged on save, which keeps the stored secret.
- It saves through `api::patch_json_empty`, because the 204 has no body.
  `patch_json` parsed the empty body as JSON and showed "Chyba pri ukladaní"
  after every successful save on the box until #229.

## The e2e mock (`e2e/mock-api.mjs`)

- It mirrors the server: masked GET (the same list and name rule), the
  keep-on-mask PATCH, 204 with no body. A mock that answers what the server
  never does hides UI bugs: the "Chyba pri ukladaní" above went unseen
  because the mock answered the PATCH with a JSON body.
- `peers` is not modelled (the dashboard does not use it).
- Mock spec: `e2e/settings-secrets.spec.ts`.

## Eval and ops scripts never read a key through GET

- GET shows `********`, so a key read through it is the mask.
- On win-resolume: read the key read-only from the database inside Python
  (`eval-run\read_gemini_key.py`, written with `FileWrite`: PowerShell
  mangles an inline `python -c`), straight into the process env (never
  echoed, never on a command line).
- On dev1: put it in `GEMINI_API_KEY` through the secret channel
  (`python3 ~/devel/airuleset/airuleset.py secret exec GEMINI_API_KEY --
  <cmd>`).
- The recipes: `dubbing-eval.md` ("Reading the Gemini key") and
  `lyrics-eval-backends.md` (the one-call run).
- CI's "Seed settings" step only asks whether `gemini_api_key` is empty (the
  mask is not), and logs no length or value.

## Post-deploy gate

`e2e/post-deploy-settings-masked.spec.ts`, read-only: on the box every
secret setting in `GET /api/v1/settings` reads `""` or `********`,
`gemini_api_key` reads the mask, and Nastavenia's Gemini "API kľúč" field
shows the mask in a password input; `GET /api/v1/exchange/status` answers
200 with `config_error` null, a boolean `serving` and a `peers` list (the
exchange router is merged in and the box's exchange settings hold). It
never PATCHes and never saves. A failure names the key or field, never the
value, and the spec records no trace: a failed run's trace would carry the
response body.
