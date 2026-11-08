#!/usr/bin/env python3
"""Classify proxy 404s on a live Hydra box by their body origin.

Usage (against the local hydra-poc, admin creds default to admin/admin):

    HYDRA_PROBE_KEY=hyd-... python3 scripts/probe_404.py [--base http://127.0.0.1:8080]

Probes, each labeled with the 404 class it evidences:
- GET  /admin/channels, /admin/keys     channel whitelist + base_url (Basic auth)
- GET  /admin/logs                     request_logs rows for recent calls
- POST /v1/chat/completions            one call per model given via --models
                                        (defaults: whitelist + "gpt-4o" from the
                                        Swagger example, class (a))
- POST /v1/chat/completions/           trailing-slash route miss, class (c)
- GET  /v1/chat/completions            wrong-method 405, class (c')

Classification rule: a 404 body carrying Hydra's `model_not_found` code is
class (a) (Hydra whitelist); any other 404 body is class (b) (upstream
passthrough); an empty 404 body is class (c) (axum router, no log row).

No secrets are hardcoded: the client key comes from HYDRA_PROBE_KEY, admin
creds from HYDRA_ADMIN_LOGIN / HYDRA_ADMIN_PASSWORD.
"""
import argparse
import base64
import json
import os
import sys
import urllib.error
import urllib.request


def auth_headers(token: str) -> dict[str, str]:
    return {"Authorization": token}


def probe(base: str, method: str, path: str, headers: dict[str, str],
          body: dict | None = None) -> tuple[int, str]:
    request = urllib.request.Request(base + path, method=method)
    for name, value in headers.items():
        request.add_header(name, value)
    data = None
    if body is not None:
        data = json.dumps(body).encode()
        request.add_header("Content-Type", "application/json")
    try:
        with urllib.request.urlopen(request, data=data, timeout=15) as response:
            return response.status, response.read().decode(errors="replace")
    except urllib.error.HTTPError as error:
        return error.code, error.read().decode(errors="replace")
    except Exception as error:  # transport-level failure is evidence too
        return 0, f"TRANSPORT-ERROR: {error}"


def show(label: str, status: int, raw: str, limit: int = 1200) -> None:
    print(f"=== {label} -> HTTP {status}")
    print(raw[:limit])
    print()


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--base", default="http://127.0.0.1:8080")
    parser.add_argument("--models", nargs="*",
                        help="models to POST; defaults to the whitelist plus gpt-4o")
    args = parser.parse_args()

    key = os.environ.get("HYDRA_PROBE_KEY")
    if not key:
        print("HYDRA_PROBE_KEY (a Hydra client key) is required", file=sys.stderr)
        return 2
    login = os.environ.get("HYDRA_ADMIN_LOGIN", "admin")
    password = os.environ.get("HYDRA_ADMIN_PASSWORD", "admin")
    basic = base64.b64encode(f"{login}:{password}".encode()).decode()

    # Channel config: whitelist and base_url explain what a 404 can mean.
    status, raw = probe(args.base, "GET", "/admin/channels",
                        auth_headers(f"Basic {basic}"))
    show("GET /admin/channels", status, raw)
    channels = json.loads(raw) if status == 200 else []
    whitelists = [c.get("allowed_models") or [] for c in channels]
    base_urls = [c.get("base_url") for c in channels]

    status, raw = probe(args.base, "GET", "/admin/keys", auth_headers(f"Basic {basic}"))
    show("GET /admin/keys", status, raw)

    status, raw = probe(args.base, "GET", "/admin/logs?page=1&page_size=20",
                        auth_headers(f"Basic {basic}"))
    show("GET /admin/logs (newest 20)", status, raw, limit=4000)

    models = args.models or sorted({m for w in whitelists for m in w} | {"gpt-4o"})
    bearer = auth_headers(f"Bearer {key}")
    for model in models:
        body = {"model": model,
                "messages": [{"role": "user", "content": "Say hi in one word."}]}
        status, raw = probe(args.base, "POST", "/v1/chat/completions", bearer, body)
        show(f"POST /v1/chat/completions model={model}", status, raw)

    status, raw = probe(args.base, "POST", "/v1/chat/completions/", bearer,
                        {"model": "gpt-4o", "messages": [{"role": "user", "content": "x"}]})
    show("POST /v1/chat/completions/ (trailing slash, class c)", status, raw)

    status, raw = probe(args.base, "GET", "/v1/chat/completions", bearer)
    show("GET /v1/chat/completions (wrong method, class c')", status, raw)

    status, raw = probe(args.base, "GET", "/admin/logs?page=1&page_size=10",
                        auth_headers(f"Basic {basic}"))
    show("GET /admin/logs after probes", status, raw, limit=4000)

    if base_urls:
        print(f"channel base_url(s): {base_urls} — if one ends in /v1, "
              "a forwarded path /v1/v1/chat/completions means the upstream "
              "sees a doubled version segment")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
