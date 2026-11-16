# Changelog

## Unreleased

### Added

- Hourly charts on the stats pages: a range of one or two days, the Today shortcut included, plots one bar per hour of usage, cost and requests, labelled with the clock hour, while a longer range keeps one bar per day. The chart headings name the bucket they span.
- A mean response time chart on the Requests page: one bar per hour of a short range and one per day of a longer one, showing the average duration of the requests the current filters select.
- Text search on the logs page: a Search box narrows the listing to the requests whose captured request or response body, or whose recorded error, contains the typed text, case-insensitively and with `%` and `_` matched as ordinary characters. It combines with the existing filters and pagination, and the needle rides in the URL like the rest of the view state, so a searched view stays shareable.

## 1.2.0 (2026-11-15)

### Added

- Dark theme for the admin UI, following the GitHub palette: the panel and canvas greys, the borders and the text colors of that scale. The pages follow the colour preference of the operating system on their own, and the sun and moon button in the header stores a choice of its own that overrides it, applied before the first paint so a page never flashes the other palette.
- Chart and status colors that follow the theme of the page: the token, cost and requests bars, the grid lines, the axis labels and the legend of the stats pages are painted on the server with the palette of the theme in force, so both palettes stay readable. A chart takes its palette from the theme cookie the request carries, so the first stats page of a browser that has not stored a theme yet keeps the light palette until the next navigation.
- Two pages in the admin UI in place of the single Stat tab: Tokens (`/admin/ui/stats`), which keeps the usage table and the token and cost charts, and Requests (`/admin/ui/stats/requests`), which holds the request figures. Each one carries its own entry in the top navigation.
- Charts on the admin UI Tokens tab: one bar per day of the selected date range for usage, one for cost. The token bars stack input tokens under output tokens, the cost bars are priced from the channels exactly as the table is, and both hover titles carry the day's figures.
- Date-range filter on both stats pages: a From and a To day picker, plus Today, Current week and Current month shortcut buttons that fill the two pickers. The key and model filters combine with the range on either page, the Requests tab adds a status class filter of its own, an empty range shows the current month to date, and every view is shareable through the URL.
- Requests tab of the admin UI: one bar per day of the selected date range, stacked by the exact response status codes with a legend that names each color, and a status code table under it listing every code of the range with its class, its request count and its share of the requests the current filters select. A request that never reached an upstream is listed as n/a.
- Status class filter on the Requests tab, narrowing the page as a whole, its chart and its table alike. A class reads the same way here and in the log listing.

### Changed

- Each stats page now runs only the queries its own content needs: the Tokens page reads the usage and price aggregates, the Requests page the status aggregates.

## 1.1.1 (2026-11-15)

### Added

- Detail pages for channels and keys: clicking a row in either table opens a card with the full entity details and the actions on it.
- Switching a key's channel from its detail page with a selector, so the key can be moved to another channel without deleting it.
- Request and response text on the log detail page, each in its own collapsible block above the captured body. The text is extracted from the stored body at render time, supports Chat Completions, Responses and Anthropic shapes including streamed ones, and falls back silently when no text is detectable.

### Changed

- Rows in the channels and keys tables link to the detail page instead of exposing row actions.

### Removed

- The Actions columns in the channels and keys tables and the `/admin/ui/{channels,keys}/{id}/edit` routes they served.

## 1.1.0 (2026-11-13)

### Added

- The site root (`GET /`) redirects to the admin UI, so a browser that opens the bare host reaches the login page instead of a 404. Proxied routes, the admin JSON API and the UI's own assets keep their paths.
- OpenAI Responses passthrough at `POST /v1/responses`, on the same channel, key and limit machinery as chat, with the Responses usage counts read for the log, the cost and the monthly gate.
- Anthropic Messages passthrough at `POST /v1/messages`: the key may arrive in `x-api-key` or in a bearer header, the required `anthropic-version` and the optional `anthropic-beta` are forwarded as sent, and the request body and upstream response pass through untouched.
- A fifth per-model price dimension, `cache_write_input`, for the tokens an upstream writes into its cache. It is set independently of the input price, so the provider's own figure is entered rather than derived, and a table that names no such rate keeps working and bills those tokens at zero.
- Cache-write tokens are now stored per request and shown in the admin UI Stat tab next to the input, output and cached columns.
- Admin UI session login: `/admin/ui` asks for the admin credentials on a login page and sets a 7-day session cookie (`HttpOnly`, `Secure`, `SameSite=Lax`, scoped to `/admin/ui`), signed with a key derived from `HYDRA_ADMIN_PASSWORD`; `HYDRA_SESSION_SECRET` overrides that key, so sessions survive a password rotation. Basic Auth still works for the JSON API and for scripts.
- Request and response bodies on the admin log detail page open in collapsible blocks, with JSON pretty-printed, streaming frames listed one per line and unicode escapes decoded to their characters.
- A "Behind a reverse proxy" README section with a worked Caddy example and the trust-boundary note, plus `HYDRA_TRUSTED_PROXIES` documented in `.env.example` and `docker-compose.yml`.

### Changed

- Cost accounting on Messages no longer undercounts cached calls: the Anthropic prompt counts input plus cache reads plus cache writes before it is priced, so the read share is charged at the read rate and the write share at the write rate.
- Hydra's own rejections on `/v1/messages` use the Anthropic error envelope, with the error family derived from the status. Chat, Responses and model listing keep the OpenAI envelope.
- Model prices may be given with four or five dimensions, in the JSON API and in the admin UI price editor, which renders five amounts and accepts either.
- The admin UI sends a browser without a session to the login page instead of the Basic Auth dialog; the JSON API keeps the `WWW-Authenticate` challenge and the session cookie does not open it.

### Fixed

- The action buttons in an admin table row stay on one line instead of wrapping when the row is narrow.

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
