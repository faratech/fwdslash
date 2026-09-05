#!/usr/bin/env python3
"""Loopback regression fixture for the HTTP/JSON contract in #80.

This never contacts Partner Center.  It models the response guard used by
check-store-submission.yml and proves that an HTTP error or non-object JSON
cannot be rendered as an empty successful submission diagnostic.
"""

import json
import subprocess
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer


class Fixture(BaseHTTPRequestHandler):
    def do_GET(self):
        responses = {
            "/ok": (200, {"id": "9P51CM0MTMK2", "status": "Pending"}),
            "/denied": (401, {"code": "Unauthorized", "message": "denied"}),
            "/array": (200, ["not", "an", "object"]),
        }
        status, body = responses.get(self.path, (404, {"code": "NotFound"}))
        encoded = json.dumps(body).encode("utf-8")
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(encoded)))
        self.end_headers()
        self.wfile.write(encoded)

    def log_message(self, _format, *_args):
        pass


def get_json(url: str) -> dict:
    """The workflow contract: only a 2xx JSON object reaches a renderer."""
    response = subprocess.run(
        ["curl", "-sS", "-w", "\\n%{http_code}", url],
        check=True,
        capture_output=True,
        text=True,
    ).stdout
    body, status = response.rsplit("\n", 1)
    if not status.startswith("2") or len(status) != 3:
        raise RuntimeError(f"HTTP {status}")
    parsed = json.loads(body)
    if not isinstance(parsed, dict):
        raise RuntimeError("response was not a JSON object")
    return parsed


def main() -> None:
    server = ThreadingHTTPServer(("127.0.0.1", 0), Fixture)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    base = f"http://127.0.0.1:{server.server_port}"
    try:
        assert get_json(base + "/ok")["id"] == "9P51CM0MTMK2"
        for path in ("/denied", "/array", "/missing"):
            try:
                get_json(base + path)
            except RuntimeError:
                pass
            else:
                raise AssertionError(f"{path} was accepted")
    finally:
        server.shutdown()
        thread.join()


if __name__ == "__main__":
    main()
