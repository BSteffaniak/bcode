# Antigravity managed harness plugin

Native `agy acp --stdio --disable-telemetry` backend for the shared `bcode.harness/v3`
contract, not a model provider. Verified against Antigravity CLI `1.5.15`.

## Integration and API

* Plugin ID `bcode.antigravity-harness`; auth provider `bcode.antigravity` (native Google OAuth
  custody only for v1; ambient `GEMINI_API_KEY` / `GOOGLE_API_KEY` are rejected).
* `plugin_definition()` is the plugin's single shared definition; bundled hosts register
  it directly and dynamic packaging wraps the same definition.
* Services declared in `bcode-plugin.toml`:
  * `bcode.harness/v3` — managed sessions;
  * `bcode.harness.candidates/v1` — setup discovery (`discover_candidates`, `prepare_candidate`);
  * `bcode.harness.models/v1` — `agy models` discovery and exact selected open;
  * `bcode.harness.recovery/v1` — exact resume preflight (`session/resume`) and physical process
    release evidence;
  * `bcode.harness.terminal-auth/v1` — client-terminal `agy acp --login` preparation.
* Native tools (`run_command`, `read_file`, `write_file`, `edit_file`, `grep_search`,
  `find_by_name`, `list_dir`, `read_url_content`, `search_web`) are declared via `[[native_tools]]`
  and `[[presentation.tools]]`.

## Configuration

`plugins.config."bcode.antigravity-harness"` decodes into `config::Config`:

```toml
[instances.antigravity]
executable    = "/etc/profiles/per-user/me/bin/agy"
gemini_dir    = "/home/me/.gemini"
exact_version = "1.5.15"
```

## Tests

* `cargo test -p bcode_antigravity_harness_plugin` — unit tests and offline fake ACP server
  (`tests/fixtures/acp_server.py`) covering open, resume, turn submission, streaming chunks,
  permission callbacks, cancellation, mid-session model/mode switching, overflow fencing, and
  physical reap recovery.
* `tests/native_subscription.rs` (`#[ignore]`) — live opt-in turn against installed `agy`.
