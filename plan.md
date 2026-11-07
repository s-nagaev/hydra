# Plan: Hydra POC — OpenAI-compatible API router

## Goal
Self-hosted OpenAI-compatible API router in Rust. Admin configures "redirects" (upstream URL + upstream API key + optional model whitelist). The router issues its own client API keys. Client calls `/v1/chat/completions` with a Hydra key; the router resolves the redirect, forwards the request substituting the upstream key (SSE streaming preserved for `stream: true`), logs request/response to the DB in the background, and returns the response to the client.

## Scope (POC)
- OpenAI Chat Completions only (`POST /v1/chat/completions`), streaming and non-streaming.
- Admin: single user, credentials from env vars at startup (`HYDRA_ADMIN_LOGIN`, `HYDRA_ADMIN_PASSWORD`), HTTP Basic Auth on all admin routes. No multi-user, no sessions.
- DB: SQLite via sqlx, **runtime queries only** (`sqlx::query(...)`, no `query!` macros — keeps clean builds and Docker builds without offline data).
- Out of scope for POC: Anthropic Messages / OpenAI Responses / Google formats, quotas/budgets, key expiry, failover, multi-channel load balancing, admin UI (curl / HTTP API only), CORS (no browser clients).

## Stack
Rust (stable), axum + tokio, reqwest with `rustls-tls` + `default-features = false` (needed for the static-binary Dockerfile), sqlx (SQLite, runtime queries), serde, tracing. New git repo at `/Users/sergio/Develop/personal/hydra`. No PR (no remote yet); work ends with clean local git history and green gates.

## Data model (reference: one-api channel/token/log, uni-api api.yaml)
- `channels` — id, name, base_url (upstream root, e.g. https://api.openai.com), api_key (upstream secret), allowed_models (NULL = all, otherwise JSON array), enabled, created_at.
- `api_keys` — id, key_hash (SHA-256, **UNIQUE**; full key shown once at creation), display_prefix, name, channel_id (FK, each issued key is bound to one channel/redirect), enabled, deleted_at (**NULL = alive; DELETE sets deleted_at, never touches enabled**), created_at. Auth lookup and PATCH exclude rows with deleted_at IS NOT NULL; `GET /admin/keys` shows the deleted state. This keeps disable (PATCH) and delete (DELETE) as distinct operations.
- `request_logs` — id, api_key_id (FK `ON DELETE RESTRICT` — soft delete makes this safe), channel_id, model, status (upstream HTTP code), prompt_tokens, completion_tokens (parsed from usage when present; NULL for streams unless the client sends `stream_options.include_usage`), duration_ms, request_body, response_body (both populated only when `HYDRA_LOG_BODIES=true`), error, created_at. Index on `(created_at, api_key_id, channel_id)` — leads with created_at (time-ordered pagination); per-key history queries acceptable without a dedicated index in POC. **SQLite WAL mode enabled at startup** (single writer lock otherwise contends with background log inserts).
- Channels: deleting a channel with bound keys → HTTP 409 (disable instead).
- Auth lookup by SHA-256 hash; plaintext keys never stored.

## API surface
- `POST /v1/chat/completions` (client key via `Authorization: Bearer hyd-...`)
- `GET /v1/models` (client key via Bearer; same 401/403 error mapping): whitelist set → return it as OpenAI list `{"object":"list","data":[{"id":...,"object":"model",...}]}`; whitelist NULL → proxy `GET {base_url}/v1/models` upstream with key substitution (same passthrough rules as chat completions).
- Admin (Basic Auth, `/admin`):
  - `GET /admin/channels`, `POST /admin/channels`, `GET/PATCH/DELETE /admin/channels/{id}` (upstream key edited via PATCH)
  - `POST /admin/keys` (returns full key once; body: channel_id, optional name), `GET /admin/keys`, `PATCH /admin/keys/{id}` (enable/disable), `DELETE /admin/keys/{id}` (soft delete)
  - `GET /admin/logs` (paginated, filter by key/channel/model)
  - `GET /admin/health` — public, no auth (liveness)
- Error mapping (OpenAI `{"error": {...}}` body shape on all proxy errors): unknown key → 401, disabled key → 403, model not in whitelist → 404 `model_not_found`, upstream errors passed through with original status.

## Tasks (in execution order)
0. **agents-md** — Write a high-quality `AGENTS.md` for the repo BEFORE any code: project purpose, stack + rationale, commands (build/test/fmt/clippy), code conventions (module layout, error handling, sqlx runtime-query policy, commit format), DB/feature conventions, Docker/CI expectations, ub-mode principles (first make it work, then make it right; log of actions). Model for authoring AND review: GLM 5.3 (owner-assigned). Reference: /Users/sergio/Develop/personal/chibi-tui/AGENTS.md if present. Dependencies: none.
1. **scaffold-and-schema** — Cargo project `hydra` (bin crate), axum+tokio skeleton with public `/admin/health`, graceful shutdown (`with_graceful_shutdown`), sqlx migrations for the 3 tables (constraints + indexes per Data model), `PRAGMA journal_mode=WAL` on pool init, config struct from env with fail-fast validation (bind addr, DB path, admin creds, HYDRA_LOG_BODIES), tracing init. **GitHub Actions CI from day one**: `.github/workflows/ci.yml` (fmt --check, clippy -D warnings, test; rust stable; repo will start private and may go public — no secrets in CI). Gates: `cargo fmt --check`, `cargo clippy -D warnings`, `cargo test`. Dependencies: task 0 (AGENTS.md governs all later code).
2. **admin-auth-and-channels** — Basic Auth extractor (constant-time compare, env creds) applied to admin routes, CRUD handlers for channels with validation (URL format, JSON model list). Gates as in task 1. Dependencies: task 1.
3. **key-management** — key CRUD: generation (`hyd-` + 32 random bytes base62), SHA-256 storage with UNIQUE enforcement, one-time full-key return, enable/disable, soft delete. Gates as in task 1. Dependencies: task 2.
4. **proxy-endpoint** — `POST /v1/chat/completions` and `GET /v1/models`: bearer-key lookup (hash, excluding deleted), error mapping per API surface, model whitelist check from the bound channel, forward to `{base_url}/v1/chat/completions` with upstream key substitution. Header hygiene: drop client `Authorization`, forward `Content-Type` and `Accept`, echo upstream `Content-Type` (event-stream for SSE). Single reused `reqwest::Client` in app state with connect/read timeouts. Non-streaming passthrough and SSE passthrough (reqwest `bytes_stream` → axum `Body::from_stream`, no buffering). Gates as in task 1. Dependencies: task 3.
5. **background-logging** — capture request body and response (tee streamed chunks into a bounded buffer, cap 1 MB, only when `HYDRA_LOG_BODIES=true`; zero overhead when false), spawn background insert into `request_logs` (usage/tokens/duration/error), never blocking or failing the client flow on log errors. Gates as in task 1. Dependencies: task 4.
6. **integration-tests** — end-to-end suite with a mock upstream (axum in-process): key resolution, unknown key → 401, disabled key → 403, non-whitelisted model → 404, streaming SSE passthrough integrity, non-stream forwarding, upstream 5xx passthrough, log rows written for stream and non-stream cases, admin auth (401 without creds), `GET /v1/models` (whitelist case and NULL→upstream-proxy case). Gates: `cargo test`, `fmt --check`, `clippy -D warnings` green on the whole crate. Dependencies: task 5.
7. **finalize** — README (setup, env vars, curl examples for admin + proxy flows; README quality matters — the project starts private but is intended to go public), `.env.example`, Dockerfile (multi-stage, static binary with rustls) — **the Docker image is a first-class final artifact**: verify the build if Docker is available locally, otherwise mark best-effort in the README; keep `.github/workflows/ci.yml` green (optionally add a Docker build job, no push). Initial git commits (`--no-gpg-sign`, no push). Dependencies: task 6.

## Open Questions
- None blocking. (PR/remote: repo is local-only for now; push target to be decided by the user.)

## Notes
- Commits: `--no-gpg-sign`, prefix format per owner convention (Feat:/Chore:/Fix:), no push, no attribution footers.
- Key→channel binding is 1:1 in POC (one issued key always routes to one redirect). Multi-channel per key = post-POC.
- Model whitelist semantics: NULL/empty = allow all; exact-match only (no wildcards in POC).
- Graceful shutdown drains background log writes before exit.
- Review history: 2026-11-07, round 1 (deepseek-v4.1-flash): sqlx runtime queries, UNIQUE key_hash + indexes + FK policy, soft-delete keys, OpenAI error shape mapping, header hygiene, task split 2→2+3, per-task gates, api_keys.name, rustls + fail-fast config, graceful shutdown, public /admin/health. Round 2 (glm-5.3): deleted_at column (disable vs delete kept distinct), channel route paths with {id}, minimal GET /v1/models added to task 4, WAL mode note, Accept header forwarding, index-lead note.
