#!/usr/bin/env python3
"""Offline behavioral tests for the Actions distribution installer."""

import gzip
import hashlib
import importlib.util
import io
import os
from pathlib import Path
import subprocess
import sys
import tarfile
import tempfile
import unittest
from unittest.mock import patch

SPEC = importlib.util.spec_from_file_location(
    "installer", Path(__file__).with_name("install-action-release.py"))
INSTALLER = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(INSTALLER)


class InstallTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        workspace = tempfile.TemporaryDirectory()
        self.addCleanup(workspace.cleanup)
        self.workspace = Path(workspace.name)
        environment = patch.dict(os.environ, {"GITHUB_WORKSPACE": str(self.workspace)})
        environment.start()
        self.addCleanup(environment.stop)

    def archive(self, extra=()):
        archive = self.root / "release.tar.gz"
        with tarfile.open(archive, "w:gz") as bundle:
            for name, content, kind in [
                    ("./bcode", b"binary", tarfile.REGTYPE), *extra]:
                member = tarfile.TarInfo(name)
                member.type = kind
                member.mode = 0o6755
                member.size = len(content) if kind == tarfile.REGTYPE else 0
                member.linkname = "../outside"
                bundle.addfile(member, io.BytesIO(content))
        return archive, hashlib.sha256(archive.read_bytes()).hexdigest()

    def test_preserves_runtime_layout_and_strips_privileged_modes(self):
        archive, digest = self.archive([
            ("./bcode-runtimes/ocr/library.so", b"runtime", tarfile.REGTYPE)])
        destination = INSTALLER.install(archive, digest, self.root)
        self.assertEqual((destination / "bcode").read_bytes(), b"binary")
        runtime = destination / "bcode-runtimes/ocr/library.so"
        self.assertEqual(runtime.read_bytes(), b"runtime")
        self.assertEqual(runtime.stat().st_mode & 0o7777, 0o700)
        self.assertEqual(destination.stat().st_mode & 0o777, 0o700)

    def test_checksum_mismatch_creates_no_installation(self):
        archive, _ = self.archive()
        with self.assertRaisesRegex(ValueError, "checksum mismatch"):
            INSTALLER.install(archive, "0" * 64, self.root)
        self.assertEqual(list(self.root.iterdir()), [archive])

    def test_unsafe_entries_fail_before_extraction(self):
        for name, kind in [("../escape", tarfile.REGTYPE),
                           ("/absolute", tarfile.REGTYPE),
                           ("link", tarfile.SYMTYPE),
                           ("hard", tarfile.LNKTYPE),
                           ("fifo", tarfile.FIFOTYPE),
                           ("bcode", tarfile.REGTYPE)]:
            with self.subTest(name=name):
                archive, digest = self.archive([(name, b"bad", kind)])
                with self.assertRaises(ValueError):
                    INSTALLER.install(archive, digest, self.root)
                self.assertEqual(list(self.root.iterdir()), [archive])

    def test_file_directory_collisions_are_rejected_before_installation(self):
        for entries in [
                [("plugins", b"file", tarfile.REGTYPE),
                 ("plugins/provider.so", b"library", tarfile.REGTYPE)],
                [("plugins/provider.so", b"library", tarfile.REGTYPE),
                 ("plugins", b"file", tarfile.REGTYPE)],
                [("bcode/child", b"file", tarfile.REGTYPE)]]:
            with self.subTest(entries=entries):
                archive, digest = self.archive(entries)
                with self.assertRaisesRegex(ValueError, "conflicts with directory"):
                    INSTALLER.install(archive, digest, self.root)
                self.assertEqual(list(self.root.iterdir()), [archive])

    def test_missing_executable_is_rejected(self):
        archive = self.root / "release.tar.gz"
        with tarfile.open(archive, "w:gz") as bundle:
            member = tarfile.TarInfo("bcode")
            member.mode = 0o600
            bundle.addfile(member, io.BytesIO())
        digest = hashlib.sha256(archive.read_bytes()).hexdigest()
        with self.assertRaisesRegex(ValueError, "lacks executable"):
            INSTALLER.install(archive, digest, self.root)
        self.assertEqual(list(self.root.iterdir()), [archive])

    def test_compressed_padding_is_bounded_before_tar_parsing(self):
        archive = self.root / "release.tar.gz"
        archive.write_bytes(gzip.compress(b"\x00" * 100000))
        digest = hashlib.sha256(archive.read_bytes()).hexdigest()
        with patch.object(INSTALLER, "MAX_TAR_BYTES", 4096):
            with self.assertRaisesRegex(ValueError, "expanded archive"):
                INSTALLER.install(archive, digest, self.root)
        self.assertEqual(list(self.root.iterdir()), [archive])

    def test_truncated_gzip_cleans_snapshot(self):
        archive, _ = self.archive()
        archive.write_bytes(archive.read_bytes()[:-8])
        digest = hashlib.sha256(archive.read_bytes()).hexdigest()
        with self.assertRaises(EOFError):
            INSTALLER.install(archive, digest, self.root)
        self.assertEqual(list(self.root.iterdir()), [archive])

    def test_cli_failures_are_sanitized(self):
        for content in [b"not-gzip-secret-canary", b"\x1f\x8b\x08\x00" + b"\x00" * 6 + b"\x07"]:
            with self.subTest(content=content):
                archive = self.root / "secret-canary.tar.gz"
                archive.write_bytes(content)
                digest = hashlib.sha256(content).hexdigest()
                result = subprocess.run(
                    [sys.executable, "-B", str(Path(INSTALLER.__file__).resolve()),
                     "--archive", str(archive), "--sha256", digest,
                     "--parent", str(self.root)],
                    capture_output=True, timeout=5, check=False)
                self.assertEqual(result.returncode, 1)
                self.assertEqual(result.stdout, b"")
                self.assertEqual(result.stderr, b"Bcode release verification/install failed\n")
                self.assertEqual(list(self.root.iterdir()), [archive])

    def test_action_setup_outputs_verified_binary_without_execution(self):
        archive, digest = self.archive()
        output = self.root / "outputs"
        adapter = Path(__file__).resolve().parents[1] / ".github/actions/setup-bcode/setup.py"
        result = subprocess.run(
            [sys.executable, "-B", str(adapter)],
            env={**os.environ, "BCODE_SETUP_ARCHIVE": str(archive),
                 "BCODE_SETUP_SHA256": digest, "RUNNER_TEMP": str(self.root),
                 "GITHUB_OUTPUT": str(output)},
            capture_output=True, timeout=5, check=False)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout, b"")
        values = dict(line.split("=", 1) for line in output.read_text().splitlines())
        self.assertEqual(Path(values["binary"]).read_bytes(), b"binary")
        self.assertEqual(Path(values["binary"]).parent, Path(values["installation"]))

    def test_action_rejects_checkout_storage_and_symlink_aliases(self):
        archive, digest = self.archive()
        checkout_archive = self.workspace / "release.tar.gz"
        checkout_archive.write_bytes(archive.read_bytes())
        alias = self.root / "checkout-alias"
        alias.symlink_to(self.workspace, target_is_directory=True)
        adapter = Path(__file__).resolve().parents[1] / ".github/actions/setup-bcode/setup.py"
        for source, parent in [
                (checkout_archive, self.root),
                (alias / "release.tar.gz", self.root),
                (archive, self.workspace),
                (archive, alias)]:
            with self.subTest(source=source, parent=parent):
                output = self.root / "outputs"
                result = subprocess.run(
                    [sys.executable, "-I", "-B", str(adapter)],
                    env={**os.environ, "BCODE_SETUP_ARCHIVE": str(source),
                         "BCODE_SETUP_SHA256": digest, "RUNNER_TEMP": str(parent),
                         "GITHUB_OUTPUT": str(output)},
                    capture_output=True, timeout=5, check=False)
                self.assertEqual(result.returncode, 1)
                self.assertEqual(result.stdout, b"")
                self.assertEqual(result.stderr,
                                 b"Bcode setup failed; verify the archive, digest and runner storage\n")
                self.assertFalse(output.exists())
                self.assertEqual(set(self.root.iterdir()), {archive, alias})
                self.assertEqual(list(self.workspace.iterdir()), [checkout_archive])

    def test_action_rejects_invalid_workspace_without_installing(self):
        archive, digest = self.archive()
        output = self.root / "outputs"
        adapter = Path(__file__).resolve().parents[1] / ".github/actions/setup-bcode/setup.py"
        for workspace in [None, "", "relative", str(self.root / "missing"), str(archive)]:
            with self.subTest(workspace=workspace):
                environment = {**os.environ, "BCODE_SETUP_ARCHIVE": str(archive),
                               "BCODE_SETUP_SHA256": digest, "RUNNER_TEMP": str(self.root),
                               "GITHUB_OUTPUT": str(output)}
                environment.pop("GITHUB_WORKSPACE", None)
                if workspace is not None:
                    environment["GITHUB_WORKSPACE"] = workspace
                result = subprocess.run(
                    [sys.executable, "-I", "-B", str(adapter)], env=environment,
                    capture_output=True, timeout=5, check=False)
                self.assertEqual(result.returncode, 1)
                self.assertEqual(result.stdout, b"")
                self.assertEqual(result.stderr,
                                 b"Bcode setup failed; verify the archive, digest and runner storage\n")
                self.assertEqual(list(self.root.iterdir()), [archive])

    def test_action_ignores_python_startup_injection(self):
        archive, digest = self.archive()
        output = self.root / "outputs"
        marker = self.root / "startup-ran"
        injection = self.root / "python-path"
        injection.mkdir()
        (injection / "sitecustomize.py").write_text(
            f"from pathlib import Path\nPath({str(marker)!r}).write_text('ran')\n")
        adapter = Path(__file__).resolve().parents[1] / ".github/actions/setup-bcode/setup.py"
        result = subprocess.run(
            [sys.executable, "-I", "-B", str(adapter)],
            env={**os.environ, "PYTHONPATH": str(injection),
                 "BCODE_SETUP_ARCHIVE": str(archive), "BCODE_SETUP_SHA256": digest,
                 "RUNNER_TEMP": str(self.root), "GITHUB_OUTPUT": str(output)},
            cwd=injection, capture_output=True, timeout=5, check=False)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertFalse(marker.exists())
        self.assertTrue(output.is_file())

    def test_action_rejects_checkout_and_relative_output_paths(self):
        archive, digest = self.archive()
        workspace = self.root / "checkout"
        workspace.mkdir()
        output = workspace / "output"
        output.write_bytes(b"preserve\n")
        adapter = Path(__file__).resolve().parents[1] / ".github/actions/setup-bcode/setup.py"
        for output_path in [str(output), "relative-output"]:
            with self.subTest(output_path=output_path):
                result = subprocess.run(
                    [sys.executable, "-I", "-B", str(adapter)],
                    env={**os.environ, "BCODE_SETUP_ARCHIVE": str(archive),
                         "BCODE_SETUP_SHA256": digest, "RUNNER_TEMP": str(self.root),
                         "GITHUB_WORKSPACE": str(workspace), "GITHUB_OUTPUT": output_path},
                    cwd=workspace, capture_output=True, timeout=5, check=False)
                self.assertEqual(result.returncode, 1)
                self.assertEqual(result.stdout, b"")
                self.assertEqual(result.stderr,
                                 b"Bcode setup failed; verify the archive, digest and runner storage\n")
                self.assertEqual(output.read_bytes(), b"preserve\n")
                self.assertEqual(set(workspace.iterdir()), {output})
                self.assertEqual(set(self.root.iterdir()), {archive, workspace})

    def test_action_output_failure_removes_installation(self):
        archive, digest = self.archive()
        adapter = Path(__file__).resolve().parents[1] / ".github/actions/setup-bcode/setup.py"
        result = subprocess.run(
            [sys.executable, "-B", str(adapter)],
            env={**os.environ, "BCODE_SETUP_ARCHIVE": str(archive),
                 "BCODE_SETUP_SHA256": digest, "RUNNER_TEMP": str(self.root),
                 "GITHUB_OUTPUT": str(self.root)},
            capture_output=True, timeout=5, check=False)
        self.assertEqual(result.returncode, 1)
        self.assertEqual(result.stdout, b"")
        self.assertEqual(result.stderr,
                         b"Bcode setup failed; verify the archive, digest and runner storage\n")
        self.assertEqual(list(self.root.iterdir()), [archive])

    @unittest.skipUnless(hasattr(os, "mkfifo"), "Unix archive installer")
    def test_fifo_archive_fails_without_waiting_for_writer(self):
        archive = self.root / "archive.fifo"
        os.mkfifo(archive)
        result = subprocess.run(
            [sys.executable, "-B", str(Path(INSTALLER.__file__).resolve()),
             "--archive", str(archive), "--sha256", "0" * 64,
             "--parent", str(self.root)],
            capture_output=True, timeout=5, check=False)
        self.assertEqual(result.returncode, 1)
        self.assertEqual(result.stdout, b"")
        self.assertEqual(result.stderr, b"Bcode release verification/install failed\n")
        self.assertEqual(list(self.root.iterdir()), [archive])

    @unittest.skipUnless(hasattr(os, "mkfifo"), "Unix Action adapter")
    def test_fifo_output_fails_without_waiting_and_cleans_installation(self):
        archive, digest = self.archive()
        output = self.root / "output.fifo"
        os.mkfifo(output)
        adapter = Path(__file__).resolve().parents[1] / ".github/actions/setup-bcode/setup.py"
        result = subprocess.run(
            [sys.executable, "-B", str(adapter)],
            env={**os.environ, "BCODE_SETUP_ARCHIVE": str(archive),
                 "BCODE_SETUP_SHA256": digest, "RUNNER_TEMP": str(self.root),
                 "GITHUB_OUTPUT": str(output)},
            capture_output=True, timeout=5, check=False)
        self.assertEqual(result.returncode, 1)
        self.assertEqual(result.stdout, b"")
        self.assertEqual(result.stderr,
                         b"Bcode setup failed; verify the archive, digest and runner storage\n")
        self.assertEqual(set(self.root.iterdir()), {archive, output})

    @unittest.skipUnless(hasattr(os, "O_NOFOLLOW"), "Unix Action adapter")
    def test_symlink_output_preserves_target_and_cleans_installation(self):
        archive, digest = self.archive()
        target = self.root / "target"
        target.write_bytes(b"preserve existing content\n")
        output = self.root / "output-link"
        output.symlink_to(target)
        adapter = Path(__file__).resolve().parents[1] / ".github/actions/setup-bcode/setup.py"
        result = subprocess.run(
            [sys.executable, "-B", str(adapter)],
            env={**os.environ, "BCODE_SETUP_ARCHIVE": str(archive),
                 "BCODE_SETUP_SHA256": digest, "RUNNER_TEMP": str(self.root),
                 "GITHUB_OUTPUT": str(output)},
            capture_output=True, timeout=5, check=False)
        self.assertEqual(result.returncode, 1)
        self.assertEqual(result.stdout, b"")
        self.assertEqual(result.stderr,
                         b"Bcode setup failed; verify the archive, digest and runner storage\n")
        self.assertEqual(target.read_bytes(), b"preserve existing content\n")
        self.assertTrue(output.is_symlink())
        self.assertEqual(set(self.root.iterdir()), {archive, output, target})

    def test_resource_limits(self):
        archive, digest = self.archive()
        for setting in ["MAX_ARCHIVE_BYTES", "MAX_EXTRACTED_BYTES", "MAX_ENTRIES"]:
            with self.subTest(setting=setting), patch.object(INSTALLER, setting, 0):
                with self.assertRaises(ValueError):
                    INSTALLER.install(archive, digest, self.root)
                self.assertEqual(list(self.root.iterdir()), [archive])

    def test_extraction_io_failure_cleans_partial_installation(self):
        archive, digest = self.archive()
        with patch.object(INSTALLER.shutil, "copyfileobj", side_effect=OSError("disk full")):
            with self.assertRaises(OSError):
                INSTALLER.install(archive, digest, self.root)
        self.assertEqual(list(self.root.iterdir()), [archive])


if __name__ == "__main__":
    unittest.main()
