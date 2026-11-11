# Changelog

## 1.0.0 (2026-11-09)

### Added

- Optional prices per model on a channel, in USD per 1M tokens for the input, output, cached input and cached output dimensions. Prices are editable from the admin UI and the JSON API, and are stored as exact integer micro-USD.
- Optional monthly spending limit per client key, in USD. A key that reaches its limit is answered `429 Monthly limit reached` on `/v1/chat/completions` and `/v1/models` until the month rolls over.
- Stat tab in the admin UI: month-to-date token usage and computed cost per key, grouped by model, with input, output, cached input, cached output, request count and a total row, for a month chosen with a picker or the previous/next links.
- Cached input and cached output tokens reported by the upstream are captured per request, shown in the request log and priced in the statistics.
- Client IP is captured for every logged request: the direct peer address, or the first untrusted `X-Forwarded-For` hop when Hydra runs behind a reverse proxy declared in `HYDRA_TRUSTED_PROXIES` (Caddy, Nginx, ...).
- `GET /admin/logs/{id}` returns a single request log with the client IP and the stored request and response bodies when capture was on.
- The admin UI logs page has a drill-down page per row showing status, token usage, duration, client IP, error text and the captured bodies, with a back link that restores the list view's filters.
- Flood cap on anonymous 401 spam: after `HYDRA_FLOOD_LIMIT` failed key authentications from one client IP within `HYDRA_FLOOD_WINDOW_SECS`, further requests from that IP are rejected without touching the database, and at most one log row per window is written for the capped phase.

### Changed

- Streaming responses record token usage without `HYDRA_LOG_BODIES`: the usage object that arrives in the last chunk of a stream is parsed for every stream, not only when body capture is on.
- A monthly limit is stored on a key whose channel cannot be priced yet and starts being enforced once that channel has a complete price list, so a limit can be set before prices exist.
- Rejected requests (unknown or malformed key, flood-capped) are now written to the request log with a nullable key and channel reference, so failed authentications appear next to successful calls.

### Fixed

- Token usage on streams larger than the body capture window is no longer lost: usage is read from a rolling tail window of the stream instead of from the capped body buffer.
- A malformed id in the admin UI log detail URL now renders the styled "invalid log id" error banner instead of a bare server rejection.
