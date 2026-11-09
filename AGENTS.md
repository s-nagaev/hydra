# AGENTS.md

## What this is

Hydra is a self-hosted, OpenAI-compatible API router written in Rust. An admin
configures "channels" (upstream base URL + upstream API key + optional model
whitelist); the router issues its own client keys (`hyd-` prefix). A client
calls `/v1/chat/completions` with a Hydra key; the router resolves the bound
channel, forwards the request with the upstream key substituted (SSE streaming
preserved), logs to the configured database in the background, and returns the
response.
**Current status: POC**: Chat Completions + `/v1/models` only, single admin
user, no failover. Beyond the proxy there is an admin UI under `/admin/ui`
(channels, keys, logs, a Stat tab for usage), optional prices per model on a
channel and an optional monthly spending limit per client key. This file is
the authority on conventions; read it before extending anything.

## Repo layout

```text
src/
├── main.rs          startup: config, tracing, DB pool (WAL), router, graceful shutdown
├── lib.rs           crate surface: modules, router assembly, AppState
├── config.rs        env-driven config struct, fail-fast validation
├── db/              pool init, migrations runner, runtime sqlx queries
├── http.rs          AppState and the router; http/ holds the handlers
├── http/            admin JSON CRUD, admin UI pages, proxy, auth, CSRF, client IP
├── cache.rs         in-memory key to channel resolution cache
├── flood.rs         in-memory flood cap on anonymous 401 spam
├── logging.rs       background request-log writer, body tee (opt-in, 1 MB cap),
│                     usage tee (tail window, always on)
├── cost.rs          request and month cost in integer micro-USD
├── prices.rs        per-channel model price table (USD input, micro-USD stored)
├── limits.rs        per-key monthly limit parsing and formatting
├── usage.rs         month-to-date aggregate per key, feed for the limit flags
├── limit_flags.rs   in-memory monthly-limit verdicts, invalidated by admin changes
├── month.rs         UTC month buckets used by the aggregates and the verdicts
└── keys.rs          client key generation and hashing
migrations/          sqlx migration files (SQLite)
migrations_pg/       sqlx migration files (PostgreSQL)
templates/           askama templates for the admin UI
assets/              vendored htmx and the admin stylesheet, embedded via include_bytes!
tests/               integration tests with in-process mock upstream
```

## Commands

```bash
cargo build                                     # build
cargo test                                       # unit + integration tests
cargo fmt --check                                # formatting gate
cargo clippy --all-targets -- -D warnings        # lint gate (warnings are errors)
docker build -t hydra .                          # multi-stage, static binary (rustls)
```

All three gates (fmt, clippy, test) must be green before any commit. Local
run: `cargo run` with env vars from `.env.example`: `HYDRA_BIND_ADDR`,
`HYDRA_DB_PATH`, `HYDRA_ADMIN_LOGIN`, `HYDRA_ADMIN_PASSWORD`,
`HYDRA_LOG_BODIES`. Config validation is fail-fast at startup: missing
required vars abort with a clear message, never a silent default.

## Code conventions

- **Errors:** `thiserror` enums for domain errors. Handlers return proper
  OpenAI error-shaped JSON responses (see API conventions), never `anyhow`
  bubbles at the HTTP boundary. Internal helper code may use `anyhow` only
  where no structured error reaches the client.
- **Types:** no `clippy::allow` without an inline justification comment;
  prefer fixing the lint. Type hints everywhere; no `unsafe`.
- **Modules:** `snake_case` file names matching module names; one concern per
  module. `//!` rationale docs on each module; plain `//` comments only for
  non-obvious invariants, and never provenance or AI-authorship mentions.
- **Async:** tokio everywhere; no blocking calls on async paths (SQLite work
  goes through sqlx's async pool). The single `reqwest::Client` is created
  once in app state (connect/read timeouts) and reused, never per request.
- **sqlx policy:** runtime queries only (`sqlx::query(...)`,
  `sqlx::query_as(...)`), and **no `query!`/`query_as!` macros.** Reason: no
  compile-time `DATABASE_URL`, no offline metadata to keep in sync, clean
  Docker builds. This is deliberate; do not "modernize" it.
- **SSE passthrough:** reqwest `bytes_stream()` → axum `Body::from_stream`,
  no buffering of the client response. The log tee is a separate bounded
  side buffer and must never delay the stream.

## Database conventions

- Migrations in the sqlx migrations directory; named
`<timestamp>_<snake_case_description>.sql`; schema changes always via a new
migration, never hand-edited DBs.
- `channels`: id, name, base_url, api_key, allowed_models (NULL = all models,
  otherwise JSON array, exact match, no wildcards), enabled, created_at,
  model_prices (NULL = no prices; JSON object keyed by model, four integer
  micro-USD-per-1M-token dimensions each; priced models must be a subset of
  `allowed_models` when that list is set).
- `api_keys`: key_hash (SHA-256, **UNIQUE**), display_prefix, name,
  channel_id (FK; key→channel is 1:1 in POC), enabled, deleted_at,
  monthly_limit_micro_usd (INTEGER NULL = unlimited; decimal USD at the API
  boundary, integer micro-USD in storage).
- **Soft delete:** `deleted_at` NULL = alive; `DELETE /admin/keys/{id}` sets
  `deleted_at`; `PATCH` (enable/disable) never touches it. Disable ≠ delete.
  Auth lookup and PATCH exclude soft-deleted rows; listing shows deleted
  state. Deleting a channel with bound keys → HTTP 409.
- `request_logs`: FK `api_key_id` with `ON DELETE RESTRICT` (safe because of
  soft delete); index leads with `created_at` for time-ordered pagination.
  `api_key_id` and `channel_id` are NULL for rejections that never resolved a
  key, and `client_ip` holds the resolved client address. Token columns are
  `prompt_tokens`, `completion_tokens`, `cached_input_tokens` and
  `cached_output_tokens`, all INTEGER NULL (NULL = the upstream reported
  nothing for that dimension, never a fabricated zero). A second index on
  `(api_key_id, created_at)` serves the per-key month-to-date aggregates; those
  queries filter with a `created_at` range, never with `substr()` in the
  WHERE clause.
- SQLite WAL mode enabled at pool init (single writer lock would otherwise
  contend with background log inserts).

## Resolution cache

- `src/cache.rs` keeps resolved (alive key, enabled channel) pairs in memory
  keyed by the key's SHA-256 hash; a fresh hit serves proxy auth with zero
  database queries, on both backends. Default TTL 3600 s via
  `HYDRA_CACHE_TTL_SECS`; `0` disables the cache.
- Only successful resolutions are cached; every rejection (unknown/disabled
  key, gone/disabled channel) stays uncached and keeps hitting the database.
- Admin mutations invalidate explicitly BEFORE and AFTER the write: key
  PATCH/DELETE by key id, channel PATCH/DELETE by channel id (scanning the
  entries). Rotating a channel's `base_url`/`api_key` must invalidate every
  entry bound to that channel. Every invalidation advances a cache epoch,
  and a refill is stored only when the epoch observed before the database
  read is still current, so an in-flight request cannot re-insert pre-write
  state after a mutation.
- The TTL is only a drift bound for out-of-band database edits; never rely
  on it for security flips: those go through explicit invalidation.
- DB-skip in tests is proven by pool swap (a closed pool makes any query
  fail), never by deleting rows: `request_logs.api_key_id` is
  `ON DELETE RESTRICT`.

## API conventions

- All proxy errors use the OpenAI body shape `{"error": {...}}`:
  unknown key → **401**; disabled key → **403**; model not in whitelist →
  **404 `model_not_found`**; upstream errors pass through with the original
  status. Same mapping for `GET /v1/models`.
- Whitelist semantics: `allowed_models` NULL/empty = allow all; exact match
  only. `GET /v1/models` returns the whitelist as an OpenAI list when set;
  proxies the upstream `/v1/models` when NULL.
- **Header hygiene on forwarding:** drop the client `Authorization`; forward
  `Content-Type` and `Accept`; substitute the upstream key as
  `Authorization: Bearer <upstream_key>`; echo the upstream `Content-Type`
  (e.g. `text/event-stream` for SSE). Nothing else hops across unless
  explicitly decided.
- Admin routes: HTTP Basic Auth (constant-time compare, env creds) on
  everything under `/admin` except `GET /admin/health` (public liveness).
  Admin errors are plain JSON, not OpenAI-shaped.

## Testing conventions

- Integration tests in `tests/` drive the real router with an **in-process
  mock upstream** (axum server on an ephemeral port), never network mocking
  libs, no external services.
- Suite must cover: unknown key → 401, disabled key → 403, non-whitelisted
  model → 404, SSE passthrough integrity, non-stream forwarding, upstream
  5xx passthrough, log rows for stream and non-stream cases, admin auth
  (401 without creds), `/v1/models` both whitelist and NULL→proxy cases,
  cached-token capture on buffered and on streaming responses, and the
  monthly limit gate (an over-budget key gets 429 on both proxy routes).
- Test names state the behavior they verify, descriptive snake_case:
  `unknown_key_returns_401`. Wrap awaits in `tokio::time::timeout` so
  deadlocks fail fast instead of hanging CI; every task ends with the three
  gates green.

## Commits

One-line commits, `PREFIX: Title`, imperative mood, capitalized title first
letter, and PREFIX is exactly one of `Fix`, `Chore`, `Feat`, `Refactor`, `Docs`,
`Revert` (no scope suffix, no lowercase form). Examples: `Feat: Add channel
CRUD handlers`. Commit with `--no-gpg-sign`. No attribution footers, no AI
mentions. Agents never push; no Test Plan sections.

## CI & Docker

`.github/workflows/ci.yml` (day one) runs on Rust stable: fmt `--check`,
clippy `-D warnings`, test, all with a plain checkout, no secrets (repo is
private now, intended public later: keep it secret-clean from the start). A
Docker build job is optional; if present it only builds, never pushes. The
Dockerfile is multi-stage (builder → minimal runtime) producing a static
binary via rustls (`default-features = false` for reqwest); the image is a
first-class artifact; keep the build green.

## ub-mode principles

When writing or changing code, follow in order. **First Make it Work**:
smallest edit that solves the problem, get the data shape right before the
logic, don't bolt new requirements onto old designs. **Then Make it Right**:
fix root causes, sequence work into verifiable units, test behavior not
implementation. **Follow Practice**: keep a log of every action and why;
commit in small ordered steps; send bulk output to files, keep summaries out
of the main thread. Never leave the gates un-green "for later".

## Security

- Secrets come only from env vars. Never commit `.env`; `.env.example`
  documents the variables without real values.
- Upstream API keys are stored in the DB (necessarily) but are **masked in
  every API response**: channel listings/patches return a display prefix
  only, never the full secret; same rule for any future log endpoints.
- Client keys: plaintext shown exactly once at creation; only SHA-256 hashes
  (`key_hash`, UNIQUE) are stored. No secrets in CI logs, error messages, or
  traces: truncate before anything reaches the client.
