#!/usr/bin/env python3
"""Real WASM/browser acceptance against an explicit test-owned loopback backend.

Does not launch Bcode, discover a daemon, read provider config, or build anything.
The Rust integration test owns backend lifetime, isolated state and canonical checks.
Only /api/session-view routes are proxied; no backend response is manufactured.
"""

import argparse
import functools
import hashlib
import html
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import http.client
import json
import mimetypes
from pathlib import Path
import re
import subprocess
import tempfile
import threading
from urllib.parse import urlsplit


MAX_BYTES = 8 * 1024 * 1024
REQUIRED = {
    "index.html", "bootstrap.js", "browser-session.js", "interactive-surface.js",
    "bcode_tui_web.js", "bcode_tui_web_bg.wasm",
}


def verified_assets(bundle):
    """Verify exact distribution inventory and hashes before starting Chromium."""
    manifest = json.loads((bundle / "manifest.json").read_text())
    files = manifest["files"]
    if manifest["schema_version"] != 1 or set(files) != REQUIRED:
        raise ValueError("unsupported bundle inventory/version")
    identity = hashlib.sha256(
        json.dumps(files, sort_keys=True, separators=(",", ":")).encode()
    ).hexdigest()
    if identity != manifest["artifact_sha256"]:
        raise ValueError("bundle identity mismatch")
    assets = {}
    for name, info in files.items():
        path = bundle / name
        if path.is_symlink() or not path.is_file():
            raise ValueError(f"invalid bundle path: {name}")
        data = path.read_bytes()
        if len(data) != info["bytes"] or hashlib.sha256(data).hexdigest() != info["sha256"]:
            raise ValueError(f"bundle mismatch: {name}")
        assets["/bundle/" + name] = data
    return assets, identity


def backend_address(value):
    """Only explicit numeric loopback addresses, without credentials or redirects."""
    parsed = urlsplit(value)
    if (parsed.scheme != "http" or parsed.hostname != "127.0.0.1"
            or not parsed.port or parsed.username or parsed.password
            or parsed.path not in ("", "/") or parsed.query or parsed.fragment):
        raise ValueError("backend must be explicit http://127.0.0.1:PORT")
    return parsed.hostname, parsed.port


class Handler(BaseHTTPRequestHandler):
    def __init__(self, *args, assets, backend, **kwargs):
        self.assets = assets
        self.backend = backend
        super().__init__(*args, **kwargs)

    def log_message(self, *_args):
        # Launch tokens must not enter request logs.
        pass

    def respond(self, status, data, content_type):
        self.send_response(status)
        self.send_header("Content-Type", content_type)
        self.send_header("Content-Length", str(len(data)))
        self.send_header("Cache-Control", "no-store")
        self.end_headers()
        self.wfile.write(data)

    def do_GET(self):
        self.handle_request()

    def do_POST(self):
        self.handle_request()

    def handle_request(self):
        path = urlsplit(self.path).path
        if path.startswith("/api/session-view/"):
            connection = http.client.HTTPConnection(*self.backend, timeout=20)
            try:
                length = int(self.headers.get("Content-Length", "0"))
                if length < 0 or length > 65536 or self.headers.get("Transfer-Encoding"):
                    self.send_error(413)
                    return
                body = self.rfile.read(length) if length else None
                connection.request(self.command, self.path, body=body,
                                   headers={"Content-Type": "application/json"})
                response = connection.getresponse()
                data = response.read(MAX_BYTES + 1)
                if len(data) > MAX_BYTES:
                    self.send_error(502)
                    return
                self.respond(response.status, data,
                             response.getheader("Content-Type", "application/octet-stream"))
            except (OSError, ValueError, http.client.HTTPException):
                self.send_error(502)
            finally:
                connection.close()
            return
        if self.command != "GET" or path not in self.assets:
            self.send_error(404)
            return
        mime = mimetypes.guess_type(path)[0] or "application/octet-stream"
        if path.endswith(".js"):
            mime = "text/javascript"
        elif path.endswith(".wasm"):
            mime = "application/wasm"
        self.respond(200, self.assets[path], mime)


def run(args):
    backend = backend_address(args.backend)
    chromium = args.chromium.resolve(strict=True)
    if not args.chromium.is_absolute() or not chromium.is_file():
        raise ValueError("Chromium must be an explicit absolute executable")
    assets, identity = verified_assets(args.bundle.resolve(strict=True))
    root = Path(__file__).resolve().parent.parent
    assets["/backend-e2e.html"] = (root / "packages/tui-web/tests/backend-e2e.html").read_bytes()
    assets["/fixture.json"] = json.dumps({"session": args.session, "token": args.token}).encode()
    server = ThreadingHTTPServer(("127.0.0.1", 0), functools.partial(
        Handler, assets=assets, backend=backend))
    server.daemon_threads = True
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    try:
        with tempfile.TemporaryDirectory(prefix="bcode-e2e-chromium-") as profile:
            command = [str(chromium), "--headless", "--disable-gpu", "--no-first-run",
                       "--disable-background-networking", "--disable-component-update",
                       f"--user-data-dir={profile}", "--dump-dom", "--timeout=60000",
                       "--virtual-time-budget=45000"]
            if args.no_sandbox:
                command.append("--no-sandbox")
            command.append(f"http://127.0.0.1:{server.server_port}/backend-e2e.html")
            completed = subprocess.run(command, capture_output=True, text=True,
                                       timeout=90, check=True)
        match = re.search(r'<pre id="result">(.*?)</pre>', completed.stdout, re.DOTALL)
        if match is None:
            raise RuntimeError("Chromium did not emit browser acceptance evidence")
        report = json.loads(html.unescape(match[1]))
        if report.get("passed") is not True:
            raise RuntimeError(report.get("error", "browser did not pass"))
        report["bundle_sha256"] = identity
        report["chromium_sha256"] = hashlib.sha256(chromium.read_bytes()).hexdigest()
        return report
    finally:
        server.shutdown()
        server.server_close()
        thread.join(timeout=5)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--chromium", type=Path, required=True)
    parser.add_argument("--bundle", type=Path, required=True)
    parser.add_argument("--backend", required=True)
    parser.add_argument("--session", required=True)
    parser.add_argument("--token", required=True)
    parser.add_argument("--no-sandbox", action="store_true",
                        help="explicit container-only opt-out; default retains sandbox")
    args = parser.parse_args()
    if not re.fullmatch(r"[0-9a-fA-F-]{36}", args.session) or not args.token:
        parser.error("explicit fixture session/token required")
    print(json.dumps(run(args), sort_keys=True))


if __name__ == "__main__":
    main()
