# Hydra

Hydra is a self-hosted, OpenAI-compatible API router. An admin defines upstream
"channels" (base URL, upstream API key, optional model whitelist), and the
router issues its own client keys so the upstream secret never leaves the
server. Clients call `/v1/chat/completions` and `/v1/models` with a Hydra key;
Hydra resolves the channel bound to that key, substitutes the upstream key,
forwards the request, and passes the response through unchanged, streaming
included.

Status: proof of concept. Chat Completions and `/v1/models` only, one admin
user, no failover.

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
  PostgreSQL): status, token usage including cached tokens, duration,
  errors; bodies are opt-in via `HYDRA_LOG_BODIES` and capped at 1 MB per
  side, while token usage is parsed with or without that flag
- Single admin over HTTP Basic Auth on `/admin/*`; upstream keys are masked in
  every response
- SQLite in WAL mode by default, PostgreSQL via `HYDRA_DATABASE_URL`; migrations
  apply automatically at startup for both backends
- In-memory resolution cache: a fresh hit serves a proxy call without touching
  the database; admin changes invalidate instantly (`HYDRA_CACHE_TTL_SECS`)
- Flood cap on anonymous 401 spam: after `HYDRA_FLOOD_LIMIT` failed
  authentications per client IP per window, bad-key requests are answered
  401 without any database access (`HYDRA_FLOOD_WINDOW_SECS`)
- Client IP attribution per request: the direct peer address, or the first
  untrusted `X-Forwarded-For` hop when Hydra runs behind a reverse proxy
  declared in `HYDRA_TRUSTED_PROXIES` (Caddy, Nginx, ...)
- Request log drill-down: `GET /admin/logs/{id}` and a detail page in the
  admin UI with status, token usage, duration, client IP, error text and
  the captured bodies
- Optional prices per model on a channel (USD per 1M tokens, stored as exact
  integer micro-USD) and an optional monthly spending limit per client key,
  enforced with a 429 `Monthly limit reached`
- A **Stat** admin tab with month-to-date token usage and computed cost per
  key, broken down by model and selectable by month

## Quickstart

Requires Rust stable.

```bash
cp .env.example .env    # then set HYDRA_ADMIN_LOGIN and HYDRA_ADMIN_PASSWORD
cargo run
```

### 1. Create a channel

The whitelist is optional; omit `allowed_models` to allow every model the
upstream serves.

The `base_url` may or may not end in `/v1`: Hydra forwards to
`<base_url>/v1/chat/completions` and drops a redundant `/v1` tail on the
stored URL, so both `https://api.openai.com` and the OpenAI-SDK style
`https://api.openai.com/v1` reach the same upstream path.

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
| GET | `/admin/logs` | list request logs, paginated, filterable by key, channel, model, status class and time window |
| GET | `/admin/logs/{id}` | fetch one log; includes the client IP and the stored bodies when capture was on |
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
bodies were captured; body content itself is never rendered. Each row
links to a drill-down page (`/admin/ui/logs/{id}`) with the full record:
status, token usage, duration, client IP, error text and the captured
request and response bodies, plus a back link that restores the exact
filters and page of the list view.

Channels carry an optional price table, edited on the channels page as one
line per model, `model: input, output, cached_input, cached_output`, each
amount in USD per 1M tokens. The channels table marks a channel "limitable"
when it is enabled, its model whitelist is set, and every model in that
whitelist carries all four prices; those prices feed the per-key cost and
monthly limit described below.

The Stat tab (`/admin/ui/stats`) shows month-to-date token usage per key,
grouped by model, with input, output, cached input, cached output, request
count and computed cost, for a month chosen with a picker or the
previous/next links.

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
| `HYDRA_CACHE_TTL_SECS` | `3600` | lifetime of the key→channel resolution cache, in seconds; `0` disables the cache |
| `HYDRA_TRUSTED_PROXIES` | unset | comma-separated IPv4/IPv6 CIDR networks of reverse proxies in front of Hydra; a request from one of them has its client IP taken from `X-Forwarded-For`. Invalid entries abort startup |
| `HYDRA_FLOOD_LIMIT` | `60` | failed key authentications per client IP per window before the flood cap; `0` disables the cap |
| `HYDRA_FLOOD_WINDOW_SECS` | `60` | length of the flood-cap window in seconds; must be at least 1 |

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

## Resolution cache

Resolving a client key to its channel otherwise costs two `SELECT`s per proxy
call. Hydra keeps successful resolutions in an in-process cache
(`src/cache.rs`) keyed by the SHA-256 hash of the presented key; a fresh hit
answers the proxy auth path without a single database query, over either
backend.

`HYDRA_CACHE_TTL_SECS` sets the entry lifetime (default 3600); `0` disables
the cache entirely, so every call hits the database.

What the cache guarantees:

- Only successful resolutions are cached: an alive key bound to an enabled
  channel. Unknown keys, disabled keys, deleted keys and gone or disabled
  channels are never cached and keep consulting the database.
- Admin mutations invalidate explicitly, before and after the write: key
  PATCH/DELETE drops that key's entries, channel PATCH/DELETE drops every
  entry bound to the channel. Each invalidation also advances a cache
  epoch, and a refill is only stored when the epoch observed before the
  resolution was read is still current, so a proxy request that was in
  flight across a mutation cannot re-insert the pre-write state. Rotating
  a channel's `base_url` or upstream `api_key` therefore takes effect on
  the very next call, never at TTL expiry.
- The TTL is a safety net for out-of-band edits (direct database writes that
  bypass the admin API, such as another process sharing the database): such
  a change becomes visible at most `HYDRA_CACHE_TTL_SECS` seconds late.
  Security flips done through the admin API are never delayed by the TTL,
  including against requests that were already in flight when the flip
  happened.

## Client IP

Every request log row stores the client IP the request was attributed to.
By default this is the direct peer address of the connection. When Hydra
runs behind a reverse proxy (Caddy, Nginx, ...), every row would otherwise
carry the proxy's address; set `HYDRA_TRUSTED_PROXIES` to the
comma-separated CIDR networks your proxies live in (a bare IP counts as a
single host), for example `HYDRA_TRUSTED_PROXIES=127.0.0.1/32,10.0.0.0/8`.
For a request whose direct peer falls inside a trusted network, the client
IP is then taken from the `X-Forwarded-For` header: the first entry of the
list that is not part of the trusted set. When clients reach Hydra
directly, leave the variable unset: a client-sent `X-Forwarded-For` header
is then ignored entirely, and the direct peer address is recorded. Invalid
entries abort startup. The flood cap counts failures against the same
resolved address, so clients behind one proxy are capped individually.

## Flood cap

Failed key authentications are the one proxy path that can never be cached:
an unknown key costs one or two `SELECT`s per request, every time. To bound
the cost of anonymous 401 spam, Hydra counts failed authentications
(missing, malformed or unknown key) per client IP in a fixed window of
`HYDRA_FLOOD_WINDOW_SECS` seconds (default 60). Once an IP has accumulated
`HYDRA_FLOOD_LIMIT` failures (default 60) inside the current window,
further requests from that IP are answered with the same 401 an unknown key
gets, but the key lookup never runs: no cache probe, no database query.
Admission is a single atomic step, so even a parallel burst of requests
cannot slip past the gate against a stale counter: at most
`HYDRA_FLOOD_LIMIT` requests per window are being processed outside the
gate at any time.

Details that are fixed by design:

- The capped response is byte-identical to the unknown-key 401, so the cap
  never becomes an oracle distinguishing a bad key from a flood-capped
  address.
- Once an IP is capped, every request from that IP is rejected for the rest
  of the window, requests carrying a valid API key included: legitimate
  traffic behind the same NAT or proxy address is locked out too, and the
  body still says "Invalid API key provided" even though the key itself is
  fine.
- Only failed authentications end up counted. A request being processed
  holds one budget slot until its authentication settles: an invalid key
  keeps the slot as a recorded failure, while successes, disabled keys
  (403) and storage failures release it. In-flight reservations count
  toward the limit (that is what bounds a parallel burst), so with a small
  limit a burst of legitimate concurrent requests from one IP can
  transiently be capped; with the default limit of 60 this only affects
  clients holding more than 60 simultaneous requests.
- The capped phase writes at most one `request_logs` row per window per IP
  (status 401, error text "flood cap: ...", request body captured only when
  `HYDRA_LOG_BODIES` is on). The first `HYDRA_FLOOD_LIMIT` failures are
  logged normally, one row each.
- The failure counter and the log-row budget reset only when the window
  expires. Capped requests change no state: they neither extend the window
  nor reset the count.
- Counting uses the same client IP resolution as the request log (direct
  peer, or the first untrusted `X-Forwarded-For` hop behind a configured
  trusted proxy, see `HYDRA_TRUSTED_PROXIES`), so clients behind one proxy
  are capped individually, not as one shared address.
- The state is in-memory per process and bounded (at most 65 536 tracked
  IPs; expired entries are evicted, and a map kept full by an IP-rotating
  flood evicts an arbitrary live entry). It resets on restart, and `0` for
  `HYDRA_FLOOD_LIMIT` disables the cap entirely.

One ordering caveat: the chat body is buffered (up to the 32 MiB body
limit) before a missing or bad key is answered, so an unauthenticated
request always pays the body buffering, and a request whose body exceeds
32 MiB is rejected with 413 even when its key is also invalid. This is the
same behavior as for ordinary unknown-key rejections; the flood cap runs
before the key lookup but after the body extractor.


## Token usage and monthly limits

Every logged request carries the token counts the upstream reported: prompt,
completion, and the cached shares of both when the vendor publishes them
(OpenAI's `prompt_tokens_details.cached_tokens`, DeepSeek's
`prompt_cache_hit_tokens`). Usage is parsed whether or not `HYDRA_LOG_BODIES`
captures bodies, for buffered and streaming responses alike: a stream is read
through a rolling 64 KiB tail window, so the usage object that arrives in the
final chunk is still parsed however large the response is. A dimension the
upstream does not report stays NULL and renders as `-` in the log views rather
than as a zero, and a request with no usage at all is left out of the token
sums while still counting as a request.

A channel can carry a price table, so a request can be expressed in money. The
JSON API takes it as `model_prices`, decimal USD per 1M tokens per dimension
(`input`, `output`, `cached_input`, `cached_output`); the admin UI takes the
same data as one line per model,
`model: input, output, cached_input, cached_output`. Prices are stored as
integer micro-USD per 1M tokens and every cost is computed with integer
arithmetic, so no float takes part in the numbers the router stores. The cost
of a request follows OpenAI's convention that `prompt_tokens` already contains
the cached tokens:

```text
cost = (prompt - cached_input) * input_price
     + cached_input * cached_input_price
     + completion * output_price
     + cached_output * cached_output_price
     (prices in micro-USD per 1M tokens, the sum divided by 1M once)
```

A model the channel does not price costs nothing, and a request whose upstream
reported no usage contributes nothing.

### Monthly limit per key

`monthly_limit_usd` on a client key caps that key's spending for the calendar
month (UTC). The value is decimal USD with at most six decimal places and at
most $1B, and it is stored as integer micro-USD like every other amount in the
router; omit it, or send `null`, for an unlimited key. Once the key's
month-to-date cost reaches the limit, `/v1/chat/completions` and `/v1/models`
both answer

```json
{"error": {"message": "Monthly limit reached", "type": "rate_limit_error", "code": null}}
```

with status 429, until the month rolls over: the new month starts against an
empty budget with no timer and no sweep. Here is a key with a 50.00 USD limit
in one call:

```bash
curl -u admin:change-me -X POST http://127.0.0.1:8080/admin/keys \
  -H 'Content-Type: application/json' \
  -d '{"channel_id": 1, "name": "ci-bot", "monthly_limit_usd": "50.00"}'
```

Enforcement reads an in-memory verdict rather than the aggregate: after every
logged response the key's month-to-date spend is recomputed in the background
and stored as one flag, which the proxy path reads as a hash lookup. Any admin
change to a key, its limit or its existence drops the verdict immediately, so
a new limit is in force from the key's next logged response.

A limit is enforced only when the bound channel can be priced: the channel
must be enabled, resolve to a fixed model list, and carry all four prices for
every model in that list. A limit on a channel that does not qualify is stored
and not enforced, which the admin UI marks with `limit inactive: channel prices
incomplete` on the keys page and the Stat tab. This is deliberate: prices are
often filled in after the key is issued.

The Stat tab (`/admin/ui/stats`) is where the numbers are visible: month-to-date
usage per key, grouped by model, with input, output, cached input, cached output
tokens, request count and computed cost in USD, plus a total row for all keys.
The month is selectable with a picker and previous/next links.

### Known limitations

All of these are deliberate, and each one favors speed or simplicity over an
exact enforcement boundary:

- Requests whose upstream reports no usage (a stream without
  `stream_options.include_usage`, or an upstream that sends no usage object at
  all) contribute nothing to the aggregates and therefore nothing to the
  limit.
- A key crossing the limit can overshoot by the one or two requests already in
  flight while its verdict was being recomputed.
- The verdict lives in memory only, so a restart clears it: after a restart the
  first requests of a limited key pass until the background recompute has run
  once.
- Adding or editing a channel's prices does not re-evaluate an existing verdict
  by itself; enforcement follows the next logged response of that key.
- Streaming usage is read from a rolling 64 KiB tail window. A stream whose
  usage object sits further than that from the end of the response is not
  counted.
- A key with a limit on a non-limitable channel is stored but not enforced, as
  described above.


## PostgreSQL

SQLite suits a single instance with modest traffic and is the default; choose
PostgreSQL when several Hydra instances must share one database, when you want
server-side backups and monitoring already in place for a Postgres server, or
when the write volume outgrows a single file. The schema is identical in
shape, the tables and indexes match one-to-one, and migrations for the chosen
backend apply automatically at startup, so there is nothing to run by hand.

With Docker Compose, a profile-gated stack brings up PostgreSQL plus a second
Hydra instance wired to it, while the default `docker compose up -d` stays
SQLite-only:

```bash
docker compose --profile postgres up -d
# postgres:   internal service, port not published
# hydra:      SQLite instance on host port 8080 (unchanged)
# hydra-postgres: PostgreSQL instance on host port 8081
```

The compose PostgreSQL service uses a named volume and a `pg_isready`
healthcheck; the `hydra-postgres` instance waits for it to be healthy before
starting. Without Docker, point `HYDRA_DATABASE_URL` at any reachable
PostgreSQL server:

```bash
HYDRA_DATABASE_URL=postgres://postgres:secret@db.internal:5432/hydra cargo run
```

For a self-contained end-to-end check against real PostgreSQL, `scripts/mock_upstream.py`
plays the upstream OpenAI API (stdlib only, any POST to `/v1/chat/completions` gets a
fixed completion back):

```bash
docker build -t hydra:local .
docker network create hydra-demo
docker run -d --name hydra-demo-pg --network hydra-demo \
  -e POSTGRES_DB=hydra -e POSTGRES_PASSWORD=hydra postgres:16-alpine
docker run -d --name hydra-demo-mock --network hydra-demo \
  -v "$(pwd)/scripts/mock_upstream.py:/srv/mock.py:ro" -w /srv \
  python:3-alpine python mock.py 8000
docker run -d --name hydra-demo --network hydra-demo -p 18080:8080 \
  -e HYDRA_DATABASE_URL=postgres://postgres:hydra@hydra-demo-pg:5432/hydra \
  -e HYDRA_ADMIN_LOGIN=admin -e HYDRA_ADMIN_PASSWORD=change-me hydra:local

curl -sS -u admin:change-me -X POST http://127.0.0.1:18080/admin/channels \
  -H 'Content-Type: application/json' \
  -d '{"name":"mock","base_url":"http://hydra-demo-mock:8000","api_key":"sk-upstream-secret","allowed_models":["demo-model"]}'
KEY=$(curl -sS -u admin:change-me -X POST http://127.0.0.1:18080/admin/keys \
  -H 'Content-Type: application/json' \
  -d '{"channel_id":1,"name":"demo-bot"}' | python3 -c 'import json,sys;print(json.load(sys.stdin)["key"])')
curl -sS http://127.0.0.1:18080/v1/chat/completions \
  -H "Authorization: Bearer $KEY" -H 'Content-Type: application/json' \
  -d '{"model":"demo-model","messages":[{"role":"user","content":"Say hello"}]}'
docker exec hydra-demo-pg psql -U postgres -d hydra \
  -c "SELECT id, channel_id, model, status FROM request_logs;"

docker rm -f hydra-demo hydra-demo-mock hydra-demo-pg && docker network rm hydra-demo
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
| 429 | null | the key reached its monthly limit (`type` is `rate_limit_error`) |
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
- Shared limit state across several instances instead of per-process flags

License: TBD.
