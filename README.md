# Hydra

Hydra is a self-hosted, OpenAI-compatible API router. An admin defines upstream
"channels" (base URL, upstream API key, optional model whitelist), and the
router issues its own client keys so the upstream secret never leaves the
server. Clients call `/v1/chat/completions` and `/v1/models` with a Hydra key;
Hydra resolves the channel bound to that key, substitutes the upstream key,
forwards the request, and passes the response through unchanged, streaming
included.

Status: proof of concept. Chat Completions and `/v1/models` only, one admin
user, no failover or quotas.

## Features

- Chat Completions passthrough, non-streaming and streaming (SSE), with the
  upstream response forwarded byte-for-byte
- `GET /v1/models`: a channel with a whitelist gets a locally built OpenAI
  model list; a channel without one proxies the upstream listing
- Own client keys with a `hyd-` prefix; only the SHA-256 hash is stored, the
  plaintext appears exactly once, in the creation response
- Each key is bound to one channel; channels can restrict requests to an
  exact-match model whitelist (no wildcards, `null` or empty means allow all)
- Background request logging into the configured database (SQLite or
  PostgreSQL): status, token usage, duration, errors; bodies are opt-in via
  `HYDRA_LOG_BODIES` and capped at 1 MB per side
- Single admin over HTTP Basic Auth on `/admin/*`; upstream keys are masked in
  every response
- SQLite in WAL mode by default, PostgreSQL via `HYDRA_DATABASE_URL`; migrations
  apply automatically at startup for both backends

## Quickstart

Requires Rust stable.

```bash
cp .env.example .env    # then set HYDRA_ADMIN_LOGIN and HYDRA_ADMIN_PASSWORD
cargo run
```

### 1. Create a channel

The whitelist is optional; omit `allowed_models` to allow every model the
upstream serves.

```bash
curl -u admin:change-me -X POST http://127.0.0.1:8080/admin/channels \
  -H 'Content-Type: application/json' \
  -d '{
    "name": "openai",
    "base_url": "https://api.openai.com",
    "api_key": "sk-upstream-secret",
    "allowed_models": ["gpt-4o", "gpt-4o-mini"]
  }'
```

Response, 201 Created. The upstream key never comes back, only a masked
preview:

```json
{
  "id": 1,
  "name": "openai",
  "base_url": "https://api.openai.com",
  "api_key_masked": "sk-u...cret",
  "has_api_key": true,
  "allowed_models": ["gpt-4o", "gpt-4o-mini"],
  "enabled": true,
  "created_at": "2026-11-07 23:59:59"
}
```

### 2. Issue a client key

```bash
curl -u admin:change-me -X POST http://127.0.0.1:8080/admin/keys \
  -H 'Content-Type: application/json' \
  -d '{"channel_id": 1, "name": "ci-bot"}'
```

Response, 201 Created. This is the only place the full key is ever shown:

```json
{
  "id": 1,
  "key": "hyd-4f9dEg2xQ7aZpKm3Wv8cLbN1rTtY6hJ5sD0uRfIeAq",
  "display_prefix": "hyd-4f9dEg2x",
  "channel_id": 1,
  "name": "ci-bot",
  "enabled": true,
  "created_at": "2026-11-07 23:59:59"
}
```

### 3. Call chat completions

```bash
curl http://127.0.0.1:8080/v1/chat/completions \
  -H "Authorization: Bearer hyd-4f9dEg2xQ7..." \
  -H 'Content-Type: application/json' \
  -d '{
    "model": "gpt-4o",
    "messages": [{"role": "user", "content": "Say hello"}]
  }'
```

The response is the upstream response, unchanged. Streaming is the same call
with `"stream": true`:

```bash
curl -N http://127.0.0.1:8080/v1/chat/completions \
  -H "Authorization: Bearer hyd-4f9dEg2xQ7..." \
  -H 'Content-Type: application/json' \
  -d '{
    "model": "gpt-4o",
    "messages": [{"role": "user", "content": "Tell me a joke"}],
    "stream": true
  }'
```

Model discovery works with the same key:

```bash
curl http://127.0.0.1:8080/v1/models \
  -H "Authorization: Bearer hyd-4f9dEg2xQ7..."
```

## Admin API

All routes under `/admin/*` require HTTP Basic Auth except `GET /admin/health`,
which is public liveness. Admin errors use a plain `{"error": "..."}` body.

| Method | Path | Purpose |
| --- | --- | --- |
| POST | `/admin/channels` | create a channel |
| GET | `/admin/channels` | list channels |
| GET, PATCH, DELETE | `/admin/channels/{id}` | fetch, edit (also `enabled`, and `allowed_models: null` clears it), delete |
| POST | `/admin/keys` | issue a key, full key in the response once |
| GET | `/admin/keys` | list keys, deleted state included |
| PATCH | `/admin/keys/{id}` | enable or disable, rename |
| DELETE | `/admin/keys/{id}` | soft delete; the key stops authenticating but its row and logs stay |
| GET | `/admin/health` | liveness, no auth |

Two invariants worth knowing: disabling a key and deleting it are different
operations (PATCH sets `enabled`, DELETE sets `deleted_at`), and a channel
with bound keys refuses deletion with 409; disable it instead.

## Swagger UI

The binary serves an embedded Swagger UI at
[http://127.0.0.1:8080/swagger-ui](http://127.0.0.1:8080/swagger-ui); the
raw OpenAPI document is at `/api-docs/openapi.json`. The page itself is
public; authenticated calls still need credentials. Click Authorize and
enter either the admin login and password (`basic_auth`, for the `/admin/*`
routes) or a `hyd-` client key (`bearer_auth`, for the `/v1/*` routes), then
"Try it out" works directly against the running instance. `GET /admin/health`
needs no auth, neither in the UI nor from a client.

## Admin UI

A server-rendered admin UI lives at `/admin/ui`: open
[http://127.0.0.1:8080/admin/ui](http://127.0.0.1:8080/admin/ui) and
authenticate with the same Basic Auth credentials as the JSON API. Pages are
compiled from askama templates with vendored htmx 2.0.11 (self-hosted, no
CDN, no JavaScript build step) and a small custom stylesheet, desktop-first
by design. Every mutating UI request is checked against same-origin browser
metadata (`Sec-Fetch-Site` and `Origin`) and rejected with 403 when it is
cross-site.

![Channels page](docs/screenshots/channels.png)

The channels and keys pages are fully usable: create and edit channels with
the same validation messages the JSON API produces, enable or disable them,
and delete them (a channel with bound keys refuses deletion with the count
of those keys and an offer to disable it instead, which preserves the keys
and their request logs). On the keys page, issue client keys, toggle them,
and soft-delete them (request logs referencing a deleted key are preserved).

Every interaction swaps a server-rendered fragment into the page; no action
navigates away. When a key is issued, a modal shows the full key exactly
once: a clipboard copy button, a ready-made curl snippet, and a required "I
have stored the key" checkbox before the modal can be closed. Only a SHA-256
hash of the key is stored, so after that modal there is no way to display
the key again; losing it means issuing a new one.

The logs page lists every proxied request with server-side pagination
(prev/next, page numbers, page size) and filters for key, channel, model,
status class (2xx/4xx/5xx/errors) and time period (presets or a custom
range); the whole view state lives in the URL, so any filtered view can be
reloaded, shared or revisited with the browser back button. Rows show
colored status badges and a small "bodies" marker when request or response
bodies were captured; body content itself is never rendered.

![Keys page](docs/screenshots/keys.png)

![Logs page](docs/screenshots/logs.png)

## Configuration

Configuration comes from environment variables. At startup the binary also
loads a `.env` file from the working directory if one is present, which is
what the quickstart above relies on; variables already set in the process
environment always take precedence over the file. The lookup is relative to
the working directory, `/data` in the Docker image, though containers
normally get their configuration through `docker run -e` or compose instead.

| Variable | Default | Meaning |
| --- | --- | --- |
| `HYDRA_BIND_ADDR` | `127.0.0.1:8080` | HTTP listener address |
| `HYDRA_DATABASE_URL` | unset | Database target, see below |
| `HYDRA_DB_PATH` | `./hydra.db` | SQLite file path, created on first run; used only when `HYDRA_DATABASE_URL` is absent |
| `HYDRA_ADMIN_LOGIN` | required | admin user for `/admin/*` Basic Auth |
| `HYDRA_ADMIN_PASSWORD` | required | admin password, compared in constant time |
| `HYDRA_LOG_BODIES` | `false` | store request and response bodies in logs, 1 MB cap per side |

Missing or empty required variables abort startup with an error naming the
variable.

`HYDRA_DATABASE_URL` picks the backend by scheme: `sqlite:<path>` selects
SQLite (equivalent to setting `HYDRA_DB_PATH=<path>`), and
`postgres://user:password@host:5432/dbname` selects PostgreSQL. When the
variable is unset, the legacy `HYDRA_DB_PATH` (or its default) keeps SQLite as
the backend, so existing deployments keep working unchanged. Any other scheme
aborts startup. Because the PostgreSQL URL carries credentials, Hydra never
logs it: it is masked as `postgres://<redacted>` everywhere a human might see
it, and an unsupported scheme fails without echoing the value.

## PostgreSQL

SQLite suits a single instance with modest traffic and is the default; choose
PostgreSQL when several Hydra instances must share one database, when you want
server-side backups and monitoring already in place for a Postgres server, or
when the write volume outgrows a single file. The schema is identical in
shape, the tables and indexes match one-to-one, and migrations for the chosen
backend apply automatically at startup — nothing to run by hand.

With Docker Compose, a profile-gated stack brings up PostgreSQL plus a second
Hydra instance wired to it, while the default `docker compose up -d` stays
SQLite-only:

```bash
docker compose --profile postgres up -d
# postgres:   internal service, port not published
# hydra:      SQLite instance on host port 8080 (unchanged)
# hydra-pg:   PostgreSQL instance on host port 8081
```

The compose PostgreSQL service uses a named volume and a `pg_isready`
healthcheck; the `hydra-pg` instance waits for it to be healthy before
starting. Without Docker, point `HYDRA_DATABASE_URL` at any reachable
PostgreSQL server:

```bash
HYDRA_DATABASE_URL=postgres://postgres:secret@db.internal:5432/hydra cargo run
```

The PostgreSQL integration tests are gated behind `HYDRA_TEST_PG_URL` and
skipped (they print a note and pass) when the variable is unset, so plain
`cargo test` never needs a database server. To run them against a real
PostgreSQL, e.g. in Docker:

```bash
docker run -d --name hydra-pg-test -e POSTGRES_PASSWORD=hydra -p 54329:5432 postgres:16-alpine
HYDRA_TEST_PG_URL=postgres://postgres:hydra@127.0.0.1:54329/postgres cargo test --test postgres
```

## Error semantics

Every proxy error uses the OpenAI body shape, so existing client libraries
can parse it:

```json
{"error": {"message": "...", "type": "invalid_request_error", "code": "..."}}
```

| Status | Code | When |
| --- | --- | --- |
| 401 | `invalid_api_key` | missing, malformed, unknown or deleted key |
| 403 | `key_disabled` | key exists but is disabled |
| 404 | `model_not_found` | requested model is not in the channel whitelist |
| 413 | null | chat body over 32 MiB |
| 502 | null | bound channel is gone, or the upstream could not be reached |
| 503 | `channel_disabled` | the channel bound to the key is disabled |

Upstream responses, errors included, pass through with the upstream status and
body unchanged.

## Docker

Migrations are embedded in the binary at compile time (`sqlx::migrate!`), so
the runtime image needs no extra files and applies them on startup.

```bash
docker build -t hydra .

docker run -d --name hydra -p 8080:8080 \
  -e HYDRA_ADMIN_LOGIN=admin \
  -e HYDRA_ADMIN_PASSWORD=change-me \
  -v hydra-data:/data \
  hydra
```

The image listens on `0.0.0.0:8080`, stores the database at `/data/hydra.db`
(put a volume there), runs as a non-root user and ships a `HEALTHCHECK` on
`/admin/health`. With compose, copy `.env.example` to `.env`, set the admin
credentials and run `docker compose up -d`; the compose file overrides the
bind address and DB path for the container. The default compose run is
SQLite-only; see the PostgreSQL section above for the `--profile postgres`
variant.

## Development

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

The three gates above need no database server: the PostgreSQL integration
tests skip themselves unless `HYDRA_TEST_PG_URL` is set (see the PostgreSQL
section for the exact command).

CI (`.github/workflows/ci.yml`) runs these three gates on Rust stable plus a
Docker image build; it never pushes. A fourth job runs the PostgreSQL
integration suite against a Postgres service container, so the gated path is
exercised on every push. `AGENTS.md` at the repo root documents the
conventions that govern changes to this code, including commit format.

The admin UI embeds `assets/htmx.min.js` (vendored htmx 2.0.11) and
`assets/admin.css` into the binary via `include_bytes!`; to upgrade htmx,
replace the file in `assets/` and update the version comment in
`src/http/admin_ui.rs`.

## Roadmap

None of this is implemented yet; it is where Hydra is meant to grow.

- Anthropic Messages API and OpenAI Responses API passthrough
- Google request and response formats
- Multi-channel keys: one key bound to several channels with failover
- Quotas and budgets per key

License: TBD.
