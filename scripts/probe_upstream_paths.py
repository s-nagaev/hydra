#!/usr/bin/env python3
"""Unauthenticated path probe of an upstream host (no credentials sent).

Distinguishes "path exists, auth required" (401/403) from "path does not
exist" (404) for the correct and the doubled-v1 chat path — the exact
evidence needed when a channel base URL ends in `/v1` and calls come back
with an upstream 404: if `/v1/v1/chat/completions` returns the same body
the client saw, the router doubled the version segment.

Usage:
    python3 scripts/probe_upstream_paths.py https://api.example.com
"""
import json
import sys
import urllib.error
import urllib.request


def probe(base: str, path: str) -> None:
    request = urllib.request.Request(base + path, method="POST")
    data = json.dumps({"model": "x", "messages": []}).encode()
    request.add_header("Content-Type", "application/json")
    try:
        with urllib.request.urlopen(request, data=data, timeout=15) as response:
            status, body = response.status, response.read().decode(errors="replace")
    except urllib.error.HTTPError as error:
        status, body = error.code, error.read().decode(errors="replace")
    except Exception as error:  # transport-level failure is evidence too
        print(f"unauth POST {path} -> TRANSPORT-ERROR: {error}")
        return
    print(f"unauth POST {path} -> {status} {body[:300]}")


def main() -> int:
    if len(sys.argv) != 2:
        print(__doc__, file=sys.stderr)
        return 2
    base = sys.argv[1].rstrip("/")
    probe(base, "/v1/chat/completions")
    probe(base, "/v1/v1/chat/completions")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
