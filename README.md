# Hydra

Hydra is a self-hosted, OpenAI-compatible API router. An admin defines upstream
"channels" (base URL, upstream API key, optional model whitelist), and the
router issues its own client keys so the upstream secret never leaves the
server. Clients call `/v1/chat/completions` and `/v1/models` with a Hydra key;
Hydra resolves the channel bound to that key, substitutes the upstream key,
forwards the request, and passes the response through unchanged, streaming
included.

Status: proof of concept. Chat Completions and `/v1/models` only, one admin
user, no failover, quotas or UI.

## Features

- Chat Completions passthrough, non-streaming and streaming (SSE), with the
  upstream response forwarded byte-for-byte
- `GET /v1/models`: a channel with a whitelist gets a locally built OpenAI
  model list; a channel without one proxies the upstream listing
- Own client keys with a `hyd-` prefix; only the SHA-256 hash is stored, the
  plaintext appears exactly once, in the creation response
- Each key is bound to one channel; channels can restrict requests to an
  exact-match model whitelist (no wildcards, `null` or empty means allow all)
- Background request logging into SQLite: status, token usage, duration,
  errors; bodies are opt-in via `HYDRA_LOG_BODIES` and capped at 1 MB per side
- Single admin over HTTP Basic Auth on `/admin/*`; upstream keys are masked in
  every response
- SQLite in WAL mode; migrations apply automatically at startup

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

## Configuration

| Variable | Default | Meaning |
| --- | --- | --- |
| `HYDRA_BIND_ADDR` | `127.0.0.1:8080` | HTTP listener address |
| `HYDRA_DB_PATH` | `./hydra.db` | SQLite file path, created on first run |
| `HYDRA_ADMIN_LOGIN` | required | admin user for `/admin/*` Basic Auth |
| `HYDRA_ADMIN_PASSWORD` | required | admin password, compared in constant time |
| `HYDRA_LOG_BODIES` | `false` | store request and response bodies in logs, 1 MB cap per side |

Missing or empty required variables abort startup with an error naming the
variable.

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
bind address and DB path for the container.

## Development

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

CI (`.github/workflows/ci.yml`) runs these three gates on Rust stable plus a
Docker image build; it never pushes. `AGENTS.md` at the repo root documents the
conventions that govern changes to this code, including commit format.

License: TBD.
