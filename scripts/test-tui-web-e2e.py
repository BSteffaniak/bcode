#!/usr/bin/env python3
"""Run real Chromium directly against an explicit test-owned production Actix server.

No proxy, header injection, daemon discovery, provider config, or builds. The Rust
parent owns the isolated fake provider and verifies canonical history afterward.
"""

import argparse
import hashlib
import html
import http.client
import json
from pathlib import Path
import os
import re
import select
import time
import shutil
import subprocess
import tempfile
from urllib.parse import urlsplit

REQUIRED = {
    "index.html", "bootstrap.js", "browser-session.js", "interactive-surface.js",
    "bcode_tui_web.js", "bcode_tui_web_bg.wasm",
}
PROTOCOL = {"session_view": 1, "browser_bridge": 1, "canvas": 1}


def verified_assets(bundle):
    """Check bytes, exact inventory and compatibility, never source text."""
    manifest = json.loads((bundle / "manifest.json").read_text())
    files = manifest["files"]
    if (manifest["schema_version"] != 2 or set(files) != REQUIRED
            or manifest.get("protocol") != PROTOCOL):
        raise ValueError("unsupported bundle inventory/version/protocol")
    identity = hashlib.sha256(
        json.dumps(files, sort_keys=True, separators=(",", ":")).encode()
    ).hexdigest()
    if identity != manifest["artifact_sha256"]:
        raise ValueError("bundle identity mismatch")
    for name, info in files.items():
        path = bundle / name
        if path.is_symlink() or not path.is_file():
            raise ValueError(f"invalid bundle path: {name}")
        if not 0 < info["bytes"] <= 64 * 1024 * 1024:
            raise ValueError("invalid asset bounds")
        data = path.read_bytes()
        if len(data) != info["bytes"] or hashlib.sha256(data).hexdigest() != info["sha256"]:
            raise ValueError(f"bundle digest mismatch: {name}")
    return identity


def backend_address(value):
    address = urlsplit(value)
    if (address.scheme != "http" or address.hostname != "127.0.0.1"
            or not address.port or address.username or address.password
            or address.path or address.query or address.fragment):
        raise ValueError("backend must be explicit test-owned IPv4 loopback origin")
    return address.hostname, address.port


def request(backend, method, path, body=None, headers=None):
    connection = http.client.HTTPConnection(*backend, timeout=20)
    try:
        connection.request(method, path, body=body, headers=headers or {})
        response = connection.getresponse()
        data = response.read(8 * 1024 * 1024 + 1)
        if len(data) > 8 * 1024 * 1024:
            raise ValueError("unbounded backend response")
        return response.status, {key.lower(): value for key, value in response.getheaders()}, data
    finally:
        connection.close()


def security_checks(backend, args, identity):
    """Exercise actual middleware and route boundaries over sockets."""
    status, headers, data = request(backend, "GET", "/tui/compatibility.json")
    handshake = json.loads(data)
    assert status == 200 and handshake == {
        "schema_version": 1, "artifact_sha256": identity, "protocol": PROTOCOL,
    }, "production compatibility mismatch"
    status, _, unauthorized = request(backend, "GET", f"/api/session-view/{args.session}?token=wrong-token")
    assert status in (200, 401, 403), "unexpected authentication response"
    assert b"unauthorized" in unauthorized, "unauthenticated snapshot disclosed"
    for path in ("/tui/index.html", "/tui/browser-session.js", "/tui/bcode_tui_web_bg.wasm"):
        status, headers, _ = request(backend, "GET", path)
        assert status == 200, f"asset unavailable: {path}"
        assert headers.get("cache-control") == "no-store", "sensitive cache policy missing"
        assert headers.get("referrer-policy") == "no-referrer", "referrer policy missing"
    path = f"/api/session-view/{args.session}?token={args.token}"
    status, headers, data = request(backend, "GET", path, headers={"Origin": "https://evil.invalid"})
    assert status == 200, "foreign read did not reach the application CORS policy"
    assert "access-control-allow-origin" not in headers, "foreign CORS granted"
    status, headers, _ = request(backend, "OPTIONS", path, headers={
        "Origin": "https://evil.invalid", "Access-Control-Request-Method": "POST",
        "Access-Control-Request-Headers": "content-type",
    })
    assert status in (400, 403), f"foreign preflight accepted: {status}"
    assert "access-control-allow-origin" not in headers, "foreign preflight CORS granted"
    submit = f"/api/session-view/submit-message?token={args.token}"
    status, _, _ = request(backend, "POST", submit, b"x" * 65537,
                           {"Content-Type": "application/json"})
    assert status == 413, "oversized streamed request accepted"
    connection = http.client.HTTPConnection(*backend, timeout=20)
    try:
        connection.request("POST", submit, body=iter([b"x" * 32768] * 3),
                           headers={"Content-Type": "application/json"}, encode_chunked=True)
        response = connection.getresponse()
        assert response.status == 413, "chunked request bypassed body limit"
        response.read(65536)
    finally:
        connection.close()
    status, _, data = request(backend, "POST", submit, b"{}", {"Content-Type": "text/plain"})
    assert (status in (400, 415) or b"invalid_content_type" in data
            or b"invalid_request" in data), "content-type accepted"
    return ["production Actix compatibility", "asset cache/referrer policy",
            "foreign read has no CORS grant and foreign preflight is rejected", "body and content-type limits"]


def browser_dom(command, url):
    """Drive real Chromium over its private CDP pipe using wall-clock deadlines."""
    command_read, command_write = os.pipe()
    response_read, response_write = os.pipe()
    launch = ["bash", "-c", 'exec 3<&"$1" 4>&"$2"; shift 2; exec "$@"',
              "chromium-pipe", str(command_read), str(response_write),
              *command, "--remote-debugging-pipe", "about:blank"]
    try:
        with tempfile.TemporaryFile() as errors, subprocess.Popen(
                launch, pass_fds=(command_read, response_write),
                stdout=subprocess.DEVNULL, stderr=errors) as process:
            os.close(command_read)
            os.close(response_write)
            command_read = response_write = None
            pending = bytearray()
            sequence = 0
            deadline = time.monotonic() + 150

            def call(method, params, session=None):
                nonlocal sequence
                sequence += 1
                message = {"id": sequence, "method": method, "params": params}
                if session is not None:
                    message["sessionId"] = session
                os.write(command_write, json.dumps(message).encode() + b"\0")
                while time.monotonic() < deadline:
                    if b"\0" not in pending:
                        if not select.select([response_read], [], [], 1)[0]:
                            continue
                        data = os.read(response_read, 65536)
                        if not data:
                            raise RuntimeError("Chromium CDP pipe closed")
                        pending.extend(data)
                        continue
                    raw, _, rest = pending.partition(b"\0")
                    pending[:] = rest
                    reply = json.loads(raw)
                    if reply.get("id") == sequence:
                        if "error" in reply:
                            raise RuntimeError(reply["error"])
                        return reply["result"]
                raise TimeoutError("Chromium behavioral acceptance deadline exceeded")

            try:
                target = call("Target.createTarget", {"url": "about:blank"})["targetId"]
                session = call("Target.attachToTarget", {"targetId": target, "flatten": True})["sessionId"]
                call("Page.navigate", {"url": url}, session)
                while time.monotonic() < deadline:
                    result = call("Runtime.evaluate", {
                        "expression": "document.querySelector('#result')?.textContent",
                        "returnByValue": True,
                    }, session)
                    value = result.get("result", {}).get("value")
                    if isinstance(value, str) and value != "pending":
                        return f'<pre id="result">{html.escape(value)}</pre>'
                    time.sleep(0.05)
                raise TimeoutError("Chromium did not finish behavioral acceptance")
            finally:
                process.terminate()
                try:
                    process.wait(timeout=10)
                except subprocess.TimeoutExpired:
                    process.kill()
                    process.wait()
    finally:
        for descriptor in (command_read, command_write, response_read, response_write):
            if descriptor is not None:
                os.close(descriptor)


def run(args):
    backend = backend_address(args.backend)
    chromium = args.chromium.resolve(strict=True)
    if not args.chromium.is_absolute() or not chromium.is_file():
        raise ValueError("Chromium must be an explicit absolute executable")
    identity = verified_assets(args.bundle.resolve(strict=True))
    security = security_checks(backend, args, identity)
    with tempfile.TemporaryDirectory(prefix="bcode-e2e-chromium-") as profile:
        command = [str(chromium), "--headless", "--disable-gpu", "--no-first-run",
                   "--disable-background-networking", "--disable-component-update",
                   "--host-resolver-rules=MAP * ~NOTFOUND, EXCLUDE 127.0.0.1",
                   f"--user-data-dir={profile}"]
        if args.no_sandbox:
            command.append("--no-sandbox")
        dom = browser_dom(command, f"{args.backend}/backend-e2e.html")
    match = re.search(r'<pre id="result">(.*?)</pre>', dom, re.DOTALL)
    if match is None:
        raise RuntimeError("Chromium did not emit browser acceptance evidence")
    report = json.loads(html.unescape(match[1]))
    if report.get("passed") is not True:
        raise RuntimeError(report.get("error", "browser did not pass"))
    report["bundle_sha256"] = identity
    report["chromium_sha256"] = hashlib.sha256(chromium.read_bytes()).hexdigest()
    report["security"] = security
    return report


def self_test(bundle):
    """Exercise tamper rejection and rollback on a real, already built distribution."""
    identity = verified_assets(bundle)
    with tempfile.TemporaryDirectory(prefix="bcode-bundle-check-") as temporary:
        root = Path(temporary) / "bundle"
        shutil.copytree(bundle, root)
        manifest = json.loads((root / "manifest.json").read_text())
        bootstrap = (root / "bootstrap.js").read_bytes()
        path = root / "manifest.json"
        def publish():
            path.write_text(json.dumps(manifest))
        def rejected():
            try:
                verified_assets(root)
            except ValueError:
                return
            raise AssertionError("invalid distribution accepted")
        publish()
        assert verified_assets(root) == identity
        for key in PROTOCOL:
            manifest["protocol"][key] = 2
            publish()
            rejected()
            manifest["protocol"][key] = 1
        publish()
        (root / "bootstrap.js").write_bytes(b"mixed release")
        rejected()
        (root / "bootstrap.js").write_bytes(bootstrap)
        assert verified_assets(root) == identity
        manifest["artifact_sha256"] = "0" * 64
        publish()
        rejected()
        manifest["artifact_sha256"] = identity
        del manifest["files"]["browser-session.js"]
        publish()
        rejected()
    for address in ("https://127.0.0.1:1", "http://evil.invalid:1", "http://127.0.0.1:1/path", "http://u@127.0.0.1:1"):
        try:
            backend_address(address)
        except ValueError:
            continue
        raise AssertionError("unsafe backend accepted")
    assert backend_address("http://127.0.0.1:1234") == ("127.0.0.1", 1234)
    print("offline bundle tamper, mixed-version, rollback and confinement checks passed")


def main():
    import sys
    if sys.argv[1:2] == ["--self-test"]:
        parser = argparse.ArgumentParser(description="Validate a real built distribution")
        parser.add_argument("--self-test", action="store_true")
        parser.add_argument("--bundle", type=Path, required=True)
        self_test(parser.parse_args().bundle.resolve(strict=True))
        return
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--chromium", type=Path, required=True)
    parser.add_argument("--bundle", type=Path, required=True)
    parser.add_argument("--backend", required=True)
    parser.add_argument("--session", required=True)
    parser.add_argument("--token", required=True)
    parser.add_argument("--no-sandbox", action="store_true")
    args = parser.parse_args()
    if not re.fullmatch(r"[0-9a-fA-F-]{36}", args.session) or not args.token:
        parser.error("explicit fixture session/token required")
    print(json.dumps(run(args), sort_keys=True))


if __name__ == "__main__":
    main()
