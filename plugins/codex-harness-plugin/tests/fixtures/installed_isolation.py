#!/usr/bin/env python3
"""Explicit installed protocol-only acceptance; synthetic settings, no credentials.

Never sends thread/turn/login/account/refresh requests. An empty table is not a
settings-source exclusion. This fixture intentionally proves it on the exact artifact.
"""
import hashlib
import json
import os
import selectors
import subprocess
import tempfile
import time

EXECUTABLE = "/nix/store/6h8n09ic7cvcici5wq16h8rl5490y5hh-codex-dev-0.164.0/bin/codex"
SHA256 = "028fe6b5f4c0ffb910d1cc8bcae41f6d22115d05c5dea6170a484ee2f0f4f946"


def main():
    digest = hashlib.sha256()
    with open(EXECUTABLE, "rb") as executable:
        for block in iter(lambda: executable.read(65536), b""):
            digest.update(block)
    assert digest.hexdigest() == SHA256, "installed artifact changed"
    with tempfile.TemporaryDirectory(prefix="bcode-codex-isolation-") as temporary:
        root = os.path.realpath(temporary)
        with open(os.path.join(root, "config.toml"), "w", encoding="utf-8") as config:
            config.write(
                'model_provider = "synthetic"\n'
                '[model_providers.synthetic]\nname = "synthetic"\n'
                'base_url = "https://synthetic.invalid/v1"\nwire_api = "responses"\n'
                '[mcp_servers.synthetic]\ncommand = "/usr/bin/false"\nenabled = false\n'
            )
        env = {"HOME": root, "CODEX_HOME": root, "PATH": "/usr/bin:/bin", "LANG": "en_US.UTF-8"}
        process = subprocess.Popen(
            [EXECUTABLE, "app-server", "--listen", "stdio://",
             "-c", "mcp_servers={}", "-c", "model_providers={}",
             "-c", 'model_provider="openai"', "-c", "notify=[]",
             "-c", "features.multi_agent=false"],
            env=env, cwd="/", stdin=subprocess.PIPE, stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL,
        )
        selector = selectors.DefaultSelector()
        selector.register(process.stdout, selectors.EVENT_READ)
        pending = bytearray()
        total_bytes = 0

        def call(identity, method, params):
            nonlocal total_bytes
            assert method in ("initialize", "config/read")
            process.stdin.write((json.dumps({"id": identity, "method": method, "params": params}) + "\n").encode())
            process.stdin.flush()
            deadline = time.monotonic() + 8
            while time.monotonic() < deadline:
                if not selector.select(0.1):
                    continue
                chunk = os.read(process.stdout.fileno(), 4096)
                assert chunk, "protocol closed"
                total_bytes += len(chunk)
                assert total_bytes <= 65536, "protocol output exceeds bound"
                pending.extend(chunk)
                while b"\n" in pending:
                    line, _, rest = pending.partition(b"\n")
                    pending[:] = rest
                    response = json.loads(line)
                    if response.get("id") == identity:
                        assert "result" in response, "protocol rejected"
                        return response["result"]
            raise AssertionError("protocol deadline")

        try:
            initialized = call(1, "initialize", {
                "clientInfo": {"name": "bcode_isolation_protocol", "version": "0.1.0"},
                "capabilities": {"experimentalApi": True},
            })
            assert initialized.get("codexHome") == root, "namespace mismatch"
            process.stdin.write(b'{"method":"initialized"}\n')
            process.stdin.flush()
            config = call(2, "config/read", {"includeLayers": False, "cwd": "/"})["config"]
            assert config["model_provider"] == "openai", "scalar override unsupported"
            assert "synthetic" in config["mcp_servers"], "MCP merge semantics changed: reassess isolation"
            assert "synthetic" in config["model_providers"], "provider merge semantics changed: reassess isolation"
            print("exact artifact: scalar override works; empty MCP/provider tables retain inherited entries")
        finally:
            selector.close()
            process.kill()
            process.wait(timeout=3)
            process.stdin.close()
            process.stdout.close()


if __name__ == "__main__":
    main()
