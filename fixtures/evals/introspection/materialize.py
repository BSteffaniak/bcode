"""Materialize manifests without changing checkout files or provider selection."""
import json
import pathlib
import sys

source = pathlib.Path(__file__).resolve().parent
binary = pathlib.Path(sys.argv[1])
destination = pathlib.Path(sys.argv[2])
if not binary.is_absolute() or not binary.is_file():
    raise SystemExit("an absolute identified executable is required")
destination.mkdir(parents=True, exist_ok=True)
for manifest in source.glob("*.toml"):
    text = manifest.read_text().replace("@IDENTIFIED_BINARY@", str(binary))
    text = text.replace("{{suite_dir}}", str(source))
    # JSON string quoting is compatible with TOML basic strings, including spaces.
    for name in ("tiny", "separable", "handoff"):
        text = text.replace('fixture = "workspaces/' + name + '"',
                            'fixture = ' + json.dumps(str(source / "workspaces" / name)))
    # Paths also occur inside single-quoted shell words. Reject ambiguous quoting.
    if any(c in str(source) + str(binary) for c in ("'", '"', "\n", "\r", "\\")):
        raise SystemExit("unsupported shell/TOML quoting in fixture or executable path")
    (destination / manifest.name).write_text(text)
