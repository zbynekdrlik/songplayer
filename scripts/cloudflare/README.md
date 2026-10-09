# Cloudflare Access — `sp.newlevel.media` (public SongPlayer dashboard)

The public dashboard is published through a Cloudflare Tunnel from `win-resolume`
(see the `win-resolume-ops` skill's tunnel section). The tunnel does **no
authentication** on its own, so a **Cloudflare Access** (Zero Trust) application
sits in front of the public hostname and requires an e-mail one-time PIN before
any request reaches the origin. The LAN path (`http://10.77.9.201:8920`,
`sp.local`) is untouched — Access is bound to the public hostname only.

Issue: zbynekdrlik/songplayer#155.

## What exists (created 2026-09-14, idempotent)

| Object | Value |
|---|---|
| Cloudflare account | `NEWLEVELMEDIA` — `8f3efbc0edbe05bd6fdcab10cd63876a` |
| Team domain | `newlevelchurch.cloudflareaccess.com` |
| Zone | `newlevel.media` — `b9019ca528e573e62c2a110a45f45c74` |
| Identity provider | One-time PIN (`onetimepin`) — `ac173e30-7766-4130-9449-0ea169928aef` (pre-existing, reused) |
| Access application | `SongPlayer dashboard · claude` — `0561e037-7321-4347-8930-e7b0cfeed7eb` |
| Application AUD | `6be724b849d6d467a432b1abac921d7e0048783dc86d995fd3d8040acd6b742a` |
| Policy | `SongPlayer owners (OTP email) · claude` — `131644e6-20a5-4917-a465-8f5774491692` |

Application settings: `type: self_hosted`, `domain: sp.newlevel.media`,
`session_duration: 720h` (30 days), `auto_redirect_to_identity: true`,
`allowed_idps` = the OTP IdP, **no path exclusions** (the whole hostname is
protected, so the `/api/v1/ws` WebSocket rides the same `CF_Authorization`
cookie). Policy: `decision: allow`, `precedence: 1`, `include` = the three owner
Gmail addresses (not listed here — this repo is public; read the live list with
the GET call in "Add or remove an allowed e-mail" below).

## spsnv / sppp — the per-site names (9.10.2026, songplayer #229)

The owner's naming, like `streamsnv` / `streampp`. `sp.newlevel.media` stays: PP's
peer exchange reaches SNV through it with a service token.

| Hostname | Tunnel | Connector | Origin | Access app |
|---|---|---|---|---|
| `spsnv.newlevel.media` | `sp-player` `0242c8d3-aa90-43e9-bbe3-897f53f86d03` (the one `sp.newlevel.media` uses) | win-resolume service `Cloudflared` | `http://localhost:8920` | `f60304f6-ccc1-440b-9330-78a23314b493` |
| `sppp.newlevel.media` | `sp-player-pp` `72c060fd-bff8-4470-90cc-d55be2296f83` (created 9.10.2026) | resolume-pp service `Cloudflared` (winget `Cloudflare.cloudflared`, `C:\Program Files (x86)\cloudflared`; token in `C:\ProgramData\cloudflared_tunnel_token.txt`, `--protocol http2` like SNV) | `http://localhost:8920` | `d8ca9d07-a044-4167-9e2a-7862f45aeb82` |

Both Access apps copy `sp.newlevel.media`'s settings and its policy's
`include` (the same three owners). Tunnel routes and DNS need the account's
full token (Cloudflare Tunnel + DNS edit), not the Access token below: the
Access token answers DNS with 403 and lists no tunnel. A route is the tunnel's
remote config (`GET`/`PUT /accounts/$ACC/cfd_tunnel/<id>/configurations`, insert
before the catch-all `http_status:404`) plus a proxied CNAME
`<id>.cfargotunnel.com`. Create the Access app FIRST, then the route and DNS.
Check: each hostname answers `302` to `newlevelchurch.cloudflareaccess.com`.

## Credentials — never printed, never committed

All calls use the account-owned API token (`cfat_` prefix) stored on the dev box
at `~/.secrets/cloudflare-newlevel-access`. It carries account-level
`Access: Apps and Policies: Edit` + `Access: Organizations, Identity Providers,
and Groups` permissions. **Never echo, log, or commit the token value** — read it
into a shell variable and trim whitespace:

```bash
CF=$(tr -d '[:space:]' < ~/.secrets/cloudflare-newlevel-access)
ACC=8f3efbc0edbe05bd6fdcab10cd63876a
APP=0561e037-7321-4347-8930-e7b0cfeed7eb
```

Verify the token works with the capability probe (never `/user/tokens/verify` —
it returns `Invalid API Token` on a valid `cfat_` token by design):

```bash
curl -s -H "Authorization: Bearer $CF" \
  "https://api.cloudflare.com/client/v4/zones" | jq '.success'
```

## Add or remove an allowed e-mail

The policy's `include` list is the allowlist. To change who can log in, read the
current policy, edit the `include` array, and PUT it back:

```bash
# Read the current policy
curl -s -H "Authorization: Bearer $CF" \
  "https://api.cloudflare.com/client/v4/accounts/$ACC/access/apps/$APP/policies/131644e6-20a5-4917-a465-8f5774491692" \
  | jq '.result'

# PUT the updated policy (keep name/decision/precedence; replace the include list)
curl -s -X PUT -H "Authorization: Bearer $CF" -H "Content-Type: application/json" \
  --data '{
    "name": "SongPlayer owners (OTP email) · claude",
    "decision": "allow",
    "precedence": 1,
    "include": [
      { "email": { "email": "owner-1@example.com" } },
      { "email": { "email": "owner-2@example.com" } },
      { "email": { "email": "owner-3@example.com" } }
    ]
  }' \
  "https://api.cloudflare.com/client/v4/accounts/$ACC/access/apps/$APP/policies/131644e6-20a5-4917-a465-8f5774491692" \
  | jq '{success, errors}'
```

The `owner-N@example.com` entries are placeholders — the PUT REPLACES the whole
`include` list, so first read the live list with the GET above and paste the
real allowlist plus/minus the one address you are changing (the actual owner
Gmail addresses are not committed to this public repo).

To grant a whole domain instead of individual e-mails, use an `email_domain`
rule (`{ "email_domain": { "domain": "example.com" } }`). Prefer explicit
per-e-mail entries here — the point of the ticket is per-person control.

## Verify

```bash
# Public hostname must 302 to the Access login (no dashboard bytes):
curl -sI https://sp.newlevel.media/            | grep -iE '^HTTP|^location'
curl -sI https://sp.newlevel.media/api/v1/status | grep -iE '^HTTP|^location'

# LAN path must still be 200 (run on win-resolume, read-only):
#   Invoke-WebRequest http://127.0.0.1:8920/api/v1/status  -> 200
```

The e-mail OTP login itself can only be completed by an allowlisted owner (an
automated agent has no mailbox). After login, confirm the dashboard AND its
live WebSocket (`/api/v1/ws`) both work.

## Rollback — remove Access, reopen the hostname

Deleting the application removes its policy too and returns `sp.newlevel.media`
to its previous open state:

```bash
curl -s -X DELETE -H "Authorization: Bearer $CF" \
  "https://api.cloudflare.com/client/v4/accounts/$ACC/access/apps/$APP" \
  | jq '{success, errors}'
```

## Future: service token for automated public-hostname checks (NOT created now)

E2E / monitoring that must hit the **public** hostname through Access needs a
Cloudflare Access **service token** (a client-id/secret pair sent as
`CF-Access-Client-Id` / `CF-Access-Client-Secret` headers), plus a second policy
with `decision: non_identity` (or `service_auth`) that includes that service
token. This is intentionally **not created yet** — current CI/monitoring hits the
box over the LAN (`http://127.0.0.1:8920` / `10.77.9.201:8920`), which bypasses
Access entirely, so no public-hostname machine credential is needed. Create one
only if a future check must go through the public URL:

```bash
# 1. Mint a service token (secret shown ONCE — persist it via the secret channel,
#    never commit it):
#    POST /accounts/$ACC/access/service_tokens  { "name": "songplayer-monitor · claude" }
# 2. Add a second policy to the app that includes it:
#    include: [ { "service_token": { "token_id": "<id>" } } ], decision: non_identity
```
