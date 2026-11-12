# Session import plugins

Bcode can discover conversations created by other coding agents and import a selected conversation into a native Bcode session. The bundled distribution currently includes importers for Pi and OpenCode, and an importer for conversation files written by the Bcode SDK (`sdk-json`).

Import is a one-time copy, not synchronization. After import, continuation uses Bcode's selected provider, agent, tools, and permissions. External tool calls become inert history and are never replayed.

## Use the TUI

Open the session picker or run:

```text
/rescan-imports
```

Import candidates are labeled with their source. Selecting one loads it through the source plugin, creates a normal Bcode session, records import provenance, and switches to the new session. Warnings are shown when source data cannot be represented exactly.

## Use the CLI

List available import sources:

```sh
bcode session import sources
```

Discover candidates from one source:

```sh
bcode session import discover --source pi
bcode session import discover --source opencode
```

Include bounded source diagnostics:

```sh
bcode session import discover --source pi --diagnostics
```

Import one external session:

```sh
bcode session import open --source pi <external-session-id>
bcode session import open --source opencode <external-session-id>
```

## Import SDK JSON conversation files

Applications that embed the Bcode SDK keep conversations as `PersistedSession` JSON files (for
example through `LocalSessionStore`). The `sdk-json` source imports files you name; it has no
default roots and never scans directories:

```sh
bcode session import file --origin bdata --origin-name bdata conversations/*.json
```

* Each file becomes its own canonical session with import provenance and, when given, the
  application origin. Failures are reported per file and the other files continue.
* Files are only read. Importing the same content again returns the existing session; a file
  with the same session id but different content is reported as a conflict and nothing is merged.
* Text turns become messages, tool calls and results become inert history, images become a note,
  and persisted memories are imported as their messages, each with a warning.
* Disable it like any bundled plugin (`bcode.sdk-session-import`).

## Configuration

Session import and startup discovery are enabled by default:

```toml
[session_import]
enabled = true
auto_discover_on_startup = true
hide_already_imported = true

[session_import.pi]
enabled = true
path_mode = "defaults_and_custom"
paths = []

[session_import.opencode]
enabled = true
path_mode = "defaults_and_custom"
paths = []
```

`path_mode` accepts:

* `defaults_only`
* `custom_only`
* `defaults_and_custom`

Pi's default root is `~/.pi/agent/sessions`. The OpenCode importer checks `~/.local/share/opencode/opencode.db` and `~/.local/share/opencode/opencode-stable.db`. Set `custom_only` when Bcode should scan only explicitly listed roots.

Import plugins do not recursively scan arbitrary home directories. They inspect known application roots or configured paths, and they read source-owned files without modifying them.

## Normalization

The shared import contract can represent:

* user and assistant messages;
* tool requests and results;
* model and agent changes;
* context compaction;
* system messages.

When source data cannot be represented exactly, the importer emits an `ImportWarning` and may add a visible system message for continuity. For example, referenced image blocks may be reported as lossy when their source artifacts are not copied.

## Architecture boundary

Import plugins implement the versioned `bcode.session_import/v1` service. Plugins own source discovery and decoding; the Bcode server owns duplicate detection, provenance, and canonical session creation. Import plugins never write Bcode's canonical event history directly.
