#!/usr/bin/env python3
"""Mock OpenAI-compatible upstream for local Hydra demos.

Usage:
    python3 scripts/mock_upstream.py [port]     # default port 8000

Endpoints:
    POST /v1/chat/completions  -> fixed non-streaming completion JSON
    GET  /v1/models            -> fixed model list

Any POST body is accepted; Hydra forwards the JSON response verbatim to the
caller. Example wiring for the containerized demo (see README "PostgreSQL"):

    docker run -d --name hydra-demo-mock --network hydra-demo \
      -v $(pwd)/scripts/mock_upstream.py:/srv/mock.py:ro -w /srv \
      python:3-alpine python mock.py 8000
"""
import json
import sys
from http.server import BaseHTTPRequestHandler, HTTPServer

COMPLETION = {
    "id": "chatcmpl-mock-1",
    "object": "chat.completion",
    "created": 1700000000,
    "model": "demo-model",
    "choices": [
        {
            "index": 0,
            "message": {"role": "assistant", "content": "Hello from the mock upstream"},
            "finish_reason": "stop",
        }
    ],
    "usage": {"prompt_tokens": 2, "completion_tokens": 3, "total_tokens": 5},
}


class Handler(BaseHTTPRequestHandler):
    def _send(self, payload):
        body = json.dumps(payload).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_POST(self):
        self.rfile.read(int(self.headers.get("Content-Length", 0)))
        self._send(COMPLETION)

    def do_GET(self):
        if self.path == "/v1/models":
            self._send({"object": "list", "data": [{"id": "demo-model", "object": "model"}]})
        else:
            self.send_error(404)

    def log_message(self, fmt, *args):
        sys.stderr.write("[mock-upstream] " + fmt % args + "\n")


if __name__ == "__main__":
    port = int(sys.argv[1]) if len(sys.argv) > 1 else 8000
    sys.stderr.write(f"[mock-upstream] listening on :{port}\n")
    HTTPServer(("0.0.0.0", port), Handler).serve_forever()
