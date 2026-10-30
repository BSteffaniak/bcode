#!/usr/bin/env python3
"""Run the actual WASM smoke fixture against an explicitly selected distribution.

Serves only the fixture and manifest-verified bundle files on an ephemeral
loopback port. No daemon, provider, credentials, or user session is involved.
"""

import argparse
import functools
import html
import importlib.util
import hashlib
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import mimetypes
from pathlib import Path
import re
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
    if (manifest["schema_version"] != 2 or set(files) != required
            or manifest.get("protocol") != {"session_view": 1, "browser_bridge": 1, "canvas": 1}):
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
    assets["/semantics.html"] = (root / "packages/tui-web/tests/committed-semantics.html").read_bytes()
    spec = importlib.util.spec_from_file_location('production_browser_runner', root / 'scripts/test-tui-web-e2e.py')
    runner = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(runner)
    server = ThreadingHTTPServer(("127.0.0.1", 0), functools.partial(FixtureHandler, assets=assets))
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    try:
        with tempfile.TemporaryDirectory(prefix="bcode-wasm-chromium-") as profile:
            command = [str(args.chromium), "--headless", "--disable-gpu", "--no-first-run",
                       f"--user-data-dir={profile}"]
            if args.no_sandbox:
                command.append("--no-sandbox")
            for fixture in ('smoke.html', 'semantics.html'):
                dom = runner.browser_dom(command, f'http://127.0.0.1:{server.server_port}/{fixture}')
                match = re.search(r'<pre id="result">([^<]*)</pre>', dom)
                message = html.unescape(match[1]) if match else 'browser did not report a result'
                if not message.startswith('PASS '):
                    raise RuntimeError(message)
                print(message)
        print(f"artifact sha256:{identity}")
    finally:
        server.shutdown()
        server.server_close()
        thread.join()


if __name__ == "__main__":
    main()
