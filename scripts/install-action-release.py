#!/usr/bin/env python3
"""Verify and install a pinned Unix Bcode distribution into private runner storage.

The expected digest must come from trusted workflow configuration, not a checksum
fetched alongside an untrusted archive. This installs files; it never runs them.
"""

import argparse
import gzip
import hashlib
import hmac
import os
from pathlib import Path, PurePosixPath
import re
import shutil
import stat
import tarfile
import tempfile
import zlib

MAX_ARCHIVE_BYTES = 512 * 1024 * 1024
MAX_EXTRACTED_BYTES = 2 * 1024 * 1024 * 1024
MAX_ENTRIES = 10000
# Bound the entire tar stream, including headers/PAX metadata that tarfile may
# allocate while parsing, before handing it to the archive parser.
MAX_TAR_BYTES = MAX_EXTRACTED_BYTES + 64 * 1024 * 1024


def install(archive, expected_sha256, parent):
    """Return a newly created installation directory; remove it on failure.

    Archive and parent must be in trusted runner-owned storage, not the checkout.
    Only regular files/directories are accepted, with bounded aggregate size.
    """
    if not re.fullmatch(r"[0-9a-f]{64}", expected_sha256):
        raise ValueError("expected SHA-256 must be 64 lowercase hexadecimal digits")
    # Copy once into private storage: hash and extract the same snapshot even if
    # the supplied path changes. Never extract over existing runner files.
    with tempfile.TemporaryDirectory(prefix="bcode-archive-", dir=parent) as scratch:
        snapshot = Path(scratch) / "release.tar.gz"
        digest = hashlib.sha256()
        size = 0
        descriptor = os.open(archive, os.O_RDONLY | os.O_NONBLOCK)
        with os.fdopen(descriptor, "rb") as source, snapshot.open("xb") as target:
            metadata = os.fstat(source.fileno())
            if not stat.S_ISREG(metadata.st_mode):
                raise ValueError("release archive must be a regular file")
            if metadata.st_size > MAX_ARCHIVE_BYTES:
                raise ValueError("release archive exceeds size limit")
            while chunk := source.read(1024 * 1024):
                size += len(chunk)
                if size > MAX_ARCHIVE_BYTES:
                    raise ValueError("release archive exceeds size limit")
                digest.update(chunk)
                target.write(chunk)
        if not hmac.compare_digest(digest.hexdigest(), expected_sha256):
            raise ValueError("release checksum mismatch")
        expanded = Path(scratch) / "release.tar"
        total_tar_bytes = 0
        with gzip.open(snapshot, "rb") as source, expanded.open("xb") as target:
            while chunk := source.read(1024 * 1024):
                total_tar_bytes += len(chunk)
                if total_tar_bytes > MAX_TAR_BYTES:
                    raise ValueError("expanded archive exceeds size limit")
                target.write(chunk)
        with tarfile.open(expanded, "r:") as bundle:
            members = []
            names = set()
            total = 0
            for member in bundle:
                if len(members) >= MAX_ENTRIES:
                    raise ValueError("too many release entries")
                path = PurePosixPath(member.name)
                if (path.is_absolute() or ".." in path.parts
                        or "\\" in member.name or "\x00" in member.name
                        or not (member.isfile() or member.isdir())):
                    raise ValueError("unsafe release entry")
                name = str(path)
                if name in names or (name == "." and not member.isdir()):
                    raise ValueError("duplicate or invalid release entry")
                names.add(name)
                total += member.size
                if member.size < 0 or total > MAX_EXTRACTED_BYTES:
                    raise ValueError("release contents exceed size limit")
                members.append((member, path))
            if not any(str(path) == "bcode" and member.isfile()
                       and member.mode & 0o111 for member, path in members):
                raise ValueError("release lacks executable bcode")
            destination = Path(tempfile.mkdtemp(prefix="bcode-release-", dir=parent))
            try:
                for member, path in members:
                    output = destination.joinpath(*path.parts)
                    if member.isdir():
                        output.mkdir(parents=True, exist_ok=True, mode=0o700)
                    else:
                        output.parent.mkdir(parents=True, exist_ok=True, mode=0o700)
                        with bundle.extractfile(member) as source, output.open("xb") as target:
                            shutil.copyfileobj(source, target)
                        # No archive ownership, setuid, or group/world permissions.
                        output.chmod(0o700 if member.mode & 0o111 else 0o600)
                return destination
            except BaseException:
                shutil.rmtree(destination)
                raise


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--archive", type=Path, required=True)
    parser.add_argument("--sha256", required=True)
    parser.add_argument("--parent", type=Path, required=True,
                        help="existing private runner-owned directory outside the checkout")
    args = parser.parse_args()
    try:
        destination = install(args.archive, args.sha256, args.parent)
    except (OSError, ValueError, EOFError, tarfile.TarError, zlib.error):
        # Do not echo archive-controlled filenames or exception text into CI logs.
        parser.exit(1, "Bcode release verification/install failed\n")
    print(destination)


if __name__ == "__main__":
    main()
