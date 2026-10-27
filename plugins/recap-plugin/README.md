# Recap plugin

`bcode.recap` contributes `recap.create`, `recap.refine`, `recap.export`, and `recap.inspect` tools and corresponding `recap-*` slash commands. All operations call `bcode_recap` directly; no standalone recap executable is launched.

## Model admission

Loading a plugin does not authorize provider spending. Creation/refinement additionally require administrator-selected model-host admission in the Bcode configuration:

```toml
[plugins.config."bcode.recap".model_host]
allowed_plugin_id = "bcode.recap"
model_profile = "your-configured-profile"
supported_account_use = true
max_turns = 32
```

Only set `supported_account_use` after verifying the selected account permits this usage. The current model-host admission boundary accepts one configured integration/profile at a time; multiple conflicting declarations fail closed. Do not copy this declaration alongside another integration's active model-host declaration without resolving that selection. Ordinary tool policy and user permissions still apply.

## Operations

Operations are confined to the canonical workspace and use its `.recap` store. Each request includes a stable `id`; create additionally takes `source` and optionally a workspace-confined `transcript` VTT/SRT path, and refine takes `focus`. See the discoverable tool/command schemas for exact arguments. Source files must be inside the authorized workspace; YouTube acquisition requires `yt-dlp`, and media processing requires `ffmpeg` and `ffprobe`. Missing tools, missing captions, or unavailable inference fail explicitly.

Export writes `.recap/<id>.html`, a portable snapshot requiring no running Bcode session to read. Existing exports are protected against accidental overwrite. The independently usable `recap` executable offers more explicit configuration/export controls.

Disabling the plugin removes its contributions without affecting the standalone library/executable. Tool exposure can be disabled independently of user commands through ordinary plugin configuration.

Model-callable creation additionally requires explicit `process.ffmpeg` and `process.ffprobe` tool enablement and permission; YouTube acquisition also requires `process.yt-dlp`. Enabling `recap.create` alone does not authorize subprocesses. Per-spawn admission evaluates literal executable/argv and resource facts under the original authorized agent. Commands are explicit user operations; they still require model-host admission. Do not enable broad process policy merely to avoid a permission prompt.

For headless user commands, use `bcode plugin command ... --deadline-ms 180000` when generation needs more than the default 30 seconds. Explicit deadlines are bounded to 900,000 ms. A reported unknown outcome is not permission to retry; inspect the stored recap and invocation settlement first.

## Building

```sh
cargo build -p bcode_recap_plugin
```

Native installation requires the built library and a platform-correct `bcode-plugin.toml` runtime library name. Static Bcode builds enable `static-bundled-recap-plugin`; aggregate bundled builds include it. Dynamic and static discovery remain subject to the configured plugin selection.

See `docs/recap-architecture.md` for document, persistence, export, and security boundaries.
