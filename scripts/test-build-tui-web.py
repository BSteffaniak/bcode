#!/usr/bin/env python3
"""Exercise the packaging shell with command doubles; never invokes Cargo or Nix."""
import hashlib
import io
import tarfile
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest


class BrowserCSetup(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="bcode-browser-c-test-")
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        (self.root / "scripts").mkdir()
        shutil.copyfile(Path(__file__).with_name("build-tui-web.sh"), self.root / "scripts/build-tui-web.sh")
        self.bin = self.root / "bin"
        self.bin.mkdir()
        self.headers = self.root / "registry/src/fixture-index/tree-sitter-language-0.1.9/wasm/include"
        self.headers.mkdir(parents=True)
        files = {}
        for name in ("stdlib.h", "stdint.h", "string.h", "stdio.h", "assert.h"):
            data = f"/* fixture {name} */\n".encode()
            (self.headers / name).write_bytes(data)
            files[f"wasm/include/{name}"] = hashlib.sha256(data).hexdigest()
        self.archive = self.root / "registry/cache/fixture-index/tree-sitter-language-0.1.9.crate"
        self.archive.parent.mkdir(parents=True)
        with tarfile.open(self.archive, "w:gz") as archive:
            for header in self.headers.glob("*.h"):
                data = header.read_bytes()
                info = tarfile.TarInfo("tree-sitter-language-0.1.9/wasm/include/" + header.name)
                info.size = len(data)
                archive.addfile(info, io.BytesIO(data))
        checksum = hashlib.sha256(self.archive.read_bytes()).hexdigest()
        (self.root / "Cargo.lock").write_text('''[[package]]
name = "wasm-bindgen"
version = "0.2.999"
[[package]]
name = "tree-sitter-language"
version = "0.1.9"
source = "registry+fixture"
checksum = "locked-hash"
'''.replace("locked-hash", checksum))
        self.lock = {"root": "root", "nodes": {"root": {"inputs": {"nixpkgs": "pkgs"}}, "pkgs": {"locked": {"type": "github", "owner": "NixOS", "repo": "nixpkgs", "rev": "fixed-revision", "narHash": "sha256-fixed"}}}}
        (self.root / "flake.lock").write_text(json.dumps(self.lock))
        bridge = self.root / "bmux/packages/web-renderer/browser"
        bridge.mkdir(parents=True)
        (bridge / "interactive-surface.js").touch()
        self.package = {"name": "tree-sitter-language", "version": "0.1.9", "source": "registry+fixture", "manifest_path": str(self.headers.parents[1] / "Cargo.toml")}
        self.metadata = {"packages": [self.package, {"name": "bmux_web_renderer", "manifest_path": str(bridge.parent / "Cargo.toml"), "source": "git+fixture#fixed"}], "target_directory": str(self.root / "unchanged-target")}
        self.metadata_path = self.root / "metadata.json"
        self.capture = self.root / "capture.json"
        self.clang = self.root / "pinned-clang"
        self.llvm = self.root / "pinned-llvm"
        for parent, name in ((self.clang, "clang"), (self.llvm, "llvm-ar")):
            (parent / "bin").mkdir(parents=True)
            self.executable(parent / "bin" / name, "#!/bin/sh\nexit 0\n")
        self.executable(self.bin / "cargo", '''#!/usr/bin/env python3
import json,os,sys
if sys.argv[1] == 'metadata':
    print(open(os.environ['FIXTURE_METADATA']).read())
else:
    assert sys.argv[1] == 'build', 'unexpected Cargo operation'
    json.dump({'args':sys.argv[1:],'env':dict(os.environ)},open(os.environ['FIXTURE_CAPTURE'],'w'))
    sys.exit(77)
''')
        self.executable(self.bin / "nix", '''#!/usr/bin/env python3
import os,sys,json
assert sys.argv[1:4] == ['build','--no-link','--print-out-paths']
assert 'fixed-revision' in sys.argv[-1] and 'sha256-fixed' in sys.argv[-1]
print(os.environ['FIXTURE_CLANG'])
print(os.environ['FIXTURE_LLVM'])
''')
        lib = self.root / "rustlib"
        lib.mkdir()
        (lib / "libstd-fixture.rlib").touch()
        self.executable(self.bin / "rustc", f"#!/bin/sh\nprintf '%s\\n' '{lib}'\n")
        self.executable(self.bin / "bindgen", "#!/bin/sh\necho 'wasm-bindgen 0.2.999'\n")
        self.env = {**os.environ, "PATH": f"{self.bin}:{os.environ['PATH']}", "BCODE_TUI_WEB_IN_NIX": "1", "WASM_BINDGEN": str(self.bin / "bindgen"), "FIXTURE_METADATA": str(self.metadata_path), "FIXTURE_CAPTURE": str(self.capture), "FIXTURE_CLANG": str(self.clang), "FIXTURE_LLVM": str(self.llvm), "CC": "native-cc-sentinel", "AR": "native-ar-sentinel", "CFLAGS": "native-flags-sentinel", "CARGO_TARGET_DIR": "unchanged-target-sentinel", "RUSTUP_TOOLCHAIN": "unchanged-rust-sentinel"}

    @staticmethod
    def executable(path, contents):
        path.write_text(contents)
        path.chmod(0o755)

    def run_builder(self):
        self.metadata_path.write_text(json.dumps(self.metadata))
        return subprocess.run(["bash", str(self.root / "scripts/build-tui-web.sh"), str(self.root / "new-output")], env=self.env, capture_output=True, text=True)

    def test_target_only_environment_and_locked_tools(self):
        result = self.run_builder()
        self.assertEqual(result.returncode, 77, result.stderr)
        capture = json.loads(self.capture.read_text())
        env = capture["env"]
        self.assertEqual(env["CC_wasm32_unknown_unknown"], str(self.clang / "bin/clang"))
        self.assertEqual(env["AR_wasm32_unknown_unknown"], str(self.llvm / "bin/llvm-ar"))
        self.assertEqual(env["CFLAGS_wasm32_unknown_unknown"], f"--target=wasm32-unknown-unknown -I{self.headers}")
        for key in ("CC", "AR", "CFLAGS", "CARGO_TARGET_DIR", "RUSTUP_TOOLCHAIN"):
            self.assertEqual(env[key], self.env[key])
        self.assertIn("wasm32-unknown-unknown", capture["args"])
        self.assertFalse((self.root / "new-output").exists())

    def test_tool_output_order_is_not_assumed(self):
        self.env["FIXTURE_CLANG"], self.env["FIXTURE_LLVM"] = self.env["FIXTURE_LLVM"], self.env["FIXTURE_CLANG"]
        result = self.run_builder()
        self.assertEqual(result.returncode, 77, result.stderr)
        env = json.loads(self.capture.read_text())["env"]
        self.assertEqual(env["CC_wasm32_unknown_unknown"], str(self.clang / "bin/clang"))
        self.assertEqual(env["AR_wasm32_unknown_unknown"], str(self.llvm / "bin/llvm-ar"))

    def test_missing_header_package(self):
        self.metadata["packages"].remove(self.package)
        self.assert_rejected("exactly one")

    def test_ambiguous_header_package(self):
        self.metadata["packages"].append(self.package)
        self.assert_rejected("exactly one")

    def test_missing_headers(self):
        (self.headers / "stdlib.h").unlink()
        self.assert_rejected("missing freestanding")

    def test_modified_headers(self):
        (self.headers / "stdlib.h").write_text("tampered")
        self.assert_rejected("header checksum mismatch")

    def test_wrong_package_checksum(self):
        self.archive.write_bytes(b"wrong archive")
        self.assert_rejected("checksum differs")

    def test_missing_tool(self):
        (self.llvm / "bin/llvm-ar").unlink()
        self.assert_rejected("tools missing or ambiguous")

    def test_ambiguous_tools(self):
        self.env["FIXTURE_LLVM"] += "\nextra-tool"
        self.assert_rejected("tools missing or ambiguous")

    def test_unpinned_nixpkgs(self):
        del self.lock["nodes"]["pkgs"]["locked"]["narHash"]
        (self.root / "flake.lock").write_text(json.dumps(self.lock))
        self.assert_rejected("immutable nixpkgs lock required")

    def assert_rejected(self, message):
        result = self.run_builder()
        self.assertNotEqual(result.returncode, 0)
        self.assertNotEqual(result.returncode, 77)
        self.assertIn(message, result.stderr)
        self.assertFalse(self.capture.exists())


if __name__ == "__main__":
    unittest.main()
