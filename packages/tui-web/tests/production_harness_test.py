#!/usr/bin/env python3
"""Harness-only real Chromium tests; these do not claim product acceptance."""
import os
import functools
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import importlib.util
import json
from pathlib import Path
import tempfile
import threading
import io
import unittest
from unittest import mock

ROOT = Path(__file__).resolve().parents[3]
SPEC = importlib.util.spec_from_file_location('browser_runner', ROOT / 'scripts/test-tui-web-e2e.py')
RUNNER = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(RUNNER)
CHROMIUM = os.environ.get('BCODE_E2E_CHROMIUM')


class Handler(BaseHTTPRequestHandler):
    def __init__(self, *args, assets, **kwargs):
        self.assets = assets
        super().__init__(*args, **kwargs)

    def do_GET(self):
        data = self.assets.get(self.path)
        self.send_response(200 if data else 404)
        self.send_header('Content-Type', 'text/javascript' if self.path.endswith('.mjs') else 'text/html')
        self.end_headers()
        self.wfile.write(data or b'not found')

    def log_message(self, *_args):
        pass


class ResponseBounds(unittest.TestCase):
    def test_asset_budget_does_not_expand_default_api_budget(self):
        payload = b"x" * (8 * 1024 * 1024 + 1)
        response = mock.MagicMock()
        response.status = 200
        response.getheaders.return_value = []
        response.read.side_effect = lambda size: io.BytesIO(payload).read(size)
        connection = mock.MagicMock()
        connection.getresponse.return_value = response
        with mock.patch.object(RUNNER.http.client, "HTTPConnection", return_value=connection):
            with self.assertRaisesRegex(ValueError, "unbounded backend response"):
                RUNNER.request(("127.0.0.1", 1), "GET", "/api/test")
            self.assertEqual(
                RUNNER.request(("127.0.0.1", 1), "GET", "/tui/test.wasm",
                               max_bytes=len(payload))[2], payload)
            with self.assertRaisesRegex(ValueError, "unbounded backend response"):
                RUNNER.request(("127.0.0.1", 1), "GET", "/tui/test.wasm",
                               max_bytes=len(payload) - 1)
        self.assertEqual(connection.close.call_count, 3)


class BrowserCleanup(unittest.TestCase):
    def test_exited_parent_still_stops_owned_descendants(self):
        process = mock.Mock(pid=12345)
        process.wait.return_value = 0
        with mock.patch.object(RUNNER.os, 'killpg') as kill:
            RUNNER.stop_browser_group(process)
        self.assertEqual(kill.call_args_list, [
            mock.call(12345, RUNNER.signal.SIGTERM),
            mock.call(12345, RUNNER.signal.SIGKILL),
        ])
        self.assertEqual(process.wait.call_count, 2)

    def test_parent_timeout_escalates_and_reaps(self):
        process = mock.Mock(pid=12345)
        process.wait.side_effect = [RUNNER.subprocess.TimeoutExpired('chromium', 10), 0]
        with mock.patch.object(RUNNER.os, 'killpg') as kill:
            RUNNER.stop_browser_group(process)
        self.assertEqual(kill.call_args_list[-1], mock.call(12345, RUNNER.signal.SIGKILL))
        self.assertEqual(process.wait.call_count, 2)

    def test_missing_group_is_already_stopped(self):
        process = mock.Mock(pid=12345)
        with mock.patch.object(RUNNER.os, 'killpg', side_effect=ProcessLookupError):
            RUNNER.stop_browser_group(process)
        self.assertEqual(process.wait.call_count, 2)

    def test_cleanup_failure_is_not_success(self):
        process = mock.Mock(pid=12345)
        with mock.patch.object(RUNNER.os, 'killpg', side_effect=PermissionError('owned group')):
            with self.assertRaises(PermissionError):
                RUNNER.stop_browser_group(process)


class HarnessTests(unittest.TestCase):
    @unittest.skipUnless(CHROMIUM, 'set BCODE_E2E_CHROMIUM to an explicit Chromium executable')
    def test_real_private_pipe_committed_pixels_and_screenshot(self):
        # Deliberately a harness probe, not BMUX or a fabricated product fixture.
        page = b'''<canvas id="canvas" width="40" height="40"></canvas><pre id="result">pending</pre>
        <script type="module">
        import {committedCheckpoint} from '/production-evidence.mjs';
        try {
          const context = canvas.getContext('2d');
          context.fillStyle = 'white'; context.fillRect(0,0,40,40);
          context.fillStyle = 'red'; context.fillRect(10,10,20,20);
          const checkpoint = await committedCheckpoint('probe', canvas);
          result.textContent = JSON.stringify({passed:true,checkpoint});
        } catch(error) { result.textContent = JSON.stringify({passed:false,error:String(error)}); }
        </script>'''
        assets = {'/probe.html': page, '/production-evidence.mjs':
                  (ROOT / 'packages/tui-web/tests/production-evidence.mjs').read_bytes()}
        server = ThreadingHTTPServer(('127.0.0.1', 0), functools.partial(Handler, assets=assets))
        thread = threading.Thread(target=server.serve_forever, daemon=True)
        thread.start()
        try:
            with tempfile.TemporaryDirectory() as temporary:
                root = Path(temporary)
                dom = RUNNER.browser_dom([CHROMIUM, '--headless', '--disable-gpu', '--no-first-run',
                    f'--user-data-dir={root / "profile"}'],
                    f'http://127.0.0.1:{server.server_port}/probe.html', root / 'evidence')
                self.assertIn('&quot;passed&quot;:true', dom)
                evidence = json.loads((root / 'evidence/probe.json').read_text())
                self.assertEqual(evidence['ink'], 400)
                self.assertEqual((evidence['width'], evidence['height']), (40, 40))
                self.assertEqual(len(evidence['pixel_sha256']), 64)
                self.assertTrue((root / 'evidence/probe.png').read_bytes().startswith(b'\x89PNG\r\n\x1a\n'))
                self.assertEqual(len(evidence['screenshot_sha256']), 64)
        finally:
            server.shutdown()
            server.server_close()
            thread.join()

    def test_backend_address_confines_credential_target(self):
        for address in ('http://evil.invalid:80', 'https://127.0.0.1:1234',
                        'http://user@127.0.0.1:1234', 'http://127.0.0.1:1234/path'):
            with self.subTest(address=address), self.assertRaises(ValueError):
                RUNNER.backend_address(address)
        self.assertEqual(RUNNER.backend_address('http://127.0.0.1:1234'), ('127.0.0.1', 1234))


if __name__ == '__main__':
    unittest.main()
