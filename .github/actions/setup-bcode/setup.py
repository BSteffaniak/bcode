"""GitHub output adapter for the verified distribution installer."""

import importlib.util
import os
from pathlib import Path
import shutil
import stat
import sys


def main():
    # Load only the installer shipped with this pinned Action, never checkout code.
    script = Path(__file__).resolve().parents[3] / "scripts/install-action-release.py"
    spec = importlib.util.spec_from_file_location("bcode_release_installer", script)
    installer = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(installer)
    destination = None
    published = False
    try:
        archive = Path(os.environ["BCODE_SETUP_ARCHIVE"])
        parent = Path(os.environ["RUNNER_TEMP"])
        if not archive.is_absolute() or not parent.is_absolute():
            raise ValueError("absolute paths required")
        workspace = Path(os.environ["GITHUB_WORKSPACE"])
        if not workspace.is_absolute():
            raise ValueError("absolute workspace required")
        workspace = workspace.resolve(strict=True)
        if not workspace.is_dir():
            raise ValueError("workspace must be a directory")
        archive = archive.resolve(strict=True)
        parent = parent.resolve(strict=True)
        if any(path == workspace or workspace in path.parents for path in (archive, parent)):
            raise ValueError("runner storage must be outside the checkout")
        if any(character in str(parent) for character in "\r\n"):
            raise ValueError("invalid output path")
        output_path = Path(os.environ["GITHUB_OUTPUT"])
        if not output_path.is_absolute():
            raise ValueError("absolute output path required")
        resolved_output = output_path.resolve()
        if resolved_output == workspace or workspace in resolved_output.parents:
            raise ValueError("output must be outside the checkout")
        destination = installer.install(archive, os.environ["BCODE_SETUP_SHA256"], parent)
        descriptor = os.open(
            output_path,
            os.O_WRONLY | os.O_APPEND | os.O_CREAT | os.O_NONBLOCK | os.O_NOFOLLOW,
            0o600,
        )
        with os.fdopen(descriptor, "a", encoding="utf-8") as output:
            if not stat.S_ISREG(os.fstat(output.fileno()).st_mode):
                raise ValueError("output must be a regular file")
            output.write(f"installation={destination}\nbinary={destination / 'bcode'}\n")
        published = True
    except (KeyError, OSError, ValueError, EOFError, installer.tarfile.TarError,
            installer.zlib.error):
        print("Bcode setup failed; verify the archive, digest and runner storage", file=sys.stderr)
        return 1
    finally:
        if destination is not None and not published:
            try:
                shutil.rmtree(destination)
            except OSError:
                print("Bcode setup cleanup failed; discard the runner", file=sys.stderr)
    return 0


if __name__ == "__main__":
    sys.exit(main())
