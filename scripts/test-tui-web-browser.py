#!/usr/bin/env python3
"""Run the actual WASM smoke fixture against an explicitly selected distribution.

Serves only the fixture and manifest-verified bundle files on an ephemeral
loopback port. No daemon, provider, credentials, or user session is involved.
"""

import argparse
import functools
import hashlib
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import mimetypes
from pathlib import Path
import re
import subprocess
import tempfile
import threading
from urllib.parse import urlsplit


def verified_assets(bundle):
    """Fail closed before serving an incomplete or modified distribution."""
    manifest = json.loads((bundle / "manifest.json").read_text())
    required = {
        "index.html", "bootstrap.js", "browser-session.js", "interactive-surface.js",
        "bcode_tui_web.js", "bcode_tui_web_bg.wasm",
    }
    files = manifest["files"]
    if manifest["schema_version"] != 1 or set(files) != required:
        raise ValueError("unsupported or incomplete bundle manifest")
    identity = hashlib.sha256(
        json.dumps(files, sort_keys=True, separators=(",", ":")).encode()
    ).hexdigest()
    if identity != manifest["artifact_sha256"]:
        raise ValueError("bundle identity mismatch")
    assets = {}
    for name, info in files.items():
        data = (bundle / name).read_bytes()
        if len(data) != info["bytes"] or hashlib.sha256(data).hexdigest() != info["sha256"]:
            raise ValueError(f"bundle file mismatch: {name}")
        assets["/bundle/" + name] = data
    return assets, identity


class FixtureHandler(BaseHTTPRequestHandler):
    def __init__(self, *args, assets, **kwargs):
        self.assets = assets
        super().__init__(*args, **kwargs)

    def do_GET(self):
        path = urlsplit(self.path).path
        data = self.assets.get(path)
        if data is None:
            self.send_error(404)
            return
        mime = "application/wasm" if path.endswith(".wasm") else mimetypes.guess_type(path)[0]
        self.send_response(200)
        self.send_header("Content-Type", mime or "application/octet-stream")
        self.send_header("Content-Length", str(len(data)))
        self.send_header("Cache-Control", "no-store")
        self.end_headers()
        self.wfile.write(data)

    def log_message(self, *_args):
        pass


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("bundle", type=Path)
    parser.add_argument("--chromium", required=True, type=Path)
    parser.add_argument("--no-sandbox", action="store_true", help="Explicit opt-in for restricted test environments")
    args = parser.parse_args()
    if not args.chromium.is_absolute() or not args.chromium.is_file():
        parser.error("--chromium must select an absolute executable path")
    assets, identity = verified_assets(args.bundle.resolve())
    root = Path(__file__).resolve().parent.parent
    assets["/smoke.html"] = (root / "packages/tui-web/tests/chromium-smoke.html").read_bytes()
    server = ThreadingHTTPServer(("127.0.0.1", 0), functools.partial(FixtureHandler, assets=assets))
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    try:
        with tempfile.TemporaryDirectory(prefix="bcode-wasm-chromium-") as profile:
            command = [str(args.chromium), "--headless", "--disable-gpu", "--no-first-run",
                       f"--user-data-dir={profile}", "--dump-dom", "--timeout=20000",
                       "--virtual-time-budget=10000"]
            if args.no_sandbox:
                command.append("--no-sandbox")
            command.append(f"http://127.0.0.1:{server.server_port}/smoke.html")
            result = subprocess.run(command, capture_output=True, text=True, timeout=45, check=True)
        match = re.search(r'<pre id="result">([^<]*)</pre>', result.stdout)
        if match is None or not match[1].startswith("PASS "):
            raise RuntimeError(match[1] if match else "browser did not report a result")
        print(f"{match[1]}\nartifact sha256:{identity}")
    finally:
        server.shutdown()
        server.server_close()
        thread.join()


if __name__ == "__main__":
    main()
