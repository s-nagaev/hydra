# Changelog

## Unreleased

### Added

- Client IP is captured for every logged request: the direct peer address, or the first untrusted `X-Forwarded-For` hop when Hydra runs behind a reverse proxy declared in `HYDRA_TRUSTED_PROXIES` (Caddy, Nginx, ...).
- `GET /admin/logs/{id}` returns a single request log with the client IP and the stored request and response bodies when capture was on.
- The admin UI logs page has a drill-down page per row showing status, token usage, duration, client IP, error text and the captured bodies, with a back link that restores the list view's filters.
- Flood cap on anonymous 401 spam: after `HYDRA_FLOOD_LIMIT` failed key authentications from one client IP within `HYDRA_FLOOD_WINDOW_SECS`, further requests from that IP are rejected without touching the database, and at most one log row per window is written for the capped phase.

### Changed

- Rejected requests (unknown or malformed key, flood-capped) are now written to the request log with a nullable key and channel reference, so failed authentications appear next to successful calls.

### Fixed

- A malformed id in the admin UI log detail URL now renders the styled "invalid log id" error banner instead of a bare server rejection.
