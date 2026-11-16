# Shared-definition packaging examples

The hello implementation is an export-free rlib exposing
`plugin_definition() -> bmux_plugin_sdk::PluginDefinition` through Bcode's
product adapter macro. Its old `static_plugin()` remains solely for legacy
consumers during migration; it is not the new dynamic packaging path.

Generate a thin dynamic wrapper from the repository root (Python 3.11+):

```sh
python3 scripts/package-plugin.py \
  --implementation-manifest examples/hello-plugin/Cargo.toml \
  --sdk-manifest /path/to/selected/bmux/packages/plugin-sdk/Cargo.toml \
  --output target/plugin-packaging/hello/backend --component backend
```

The script delegates to the selected BMUX tooling (workspace local SDK patch or
explicit `BMUX_PLUGIN_PACKAGER`); it does not maintain another ABI generator.
Add `--build` for compilation and `--install-root <directory> --declaration
examples/hello-plugin/bcode-plugin.toml` for immutable publication. Use the
repository development shell. No commands in this guide have been validated
in the current initial source stage.

`question-dynamic-plugin` is an explicit thin packaging target for the production
question definition; it does not redeclare the manifest or copy behavior.
`scripts/install-bundled-plugins.sh` uses the same generated packaging path,
accepts repeatable `--plugin <Cargo dependency>` or `--all`, and defaults to the
original filesystem/shell/OpenAI/default-agents selection. Frontend packaging
is explicit with `--component frontend`, never automatically shipped by a daemon.

Installation records logical plugin ID, requested component, embedded packaging
build identity and observed library/declaration hashes. It never activates,
changes selected runtime, overwrites old artifacts, or restarts a daemon.
Original declarations remain verbatim, so legacy loaders must not guess this
layout or reinterpret old runtime ABI declarations. Shared loader support and
loader-visible descriptor identity verification are integration requirements.
Production `static_bundled_plugins()` registrations now attach those same
`plugin_definition` factories via the host's `with_definition` hook; the old
vtable remains only for legacy product contribution compatibility. No old macro
exports are enabled behind features in the migrated example implementations.
Durable installation supports POSIX hosts only and rejects Windows hosts explicitly;
builds still select the platform artifact from Cargo JSON output.

Publication inspects the explicitly selected trusted artifact's inert shared
metadata and requires streaming, host services and live cancellation. It checks
logical ID/version, component, full declaration and required capabilities; receipts
retain contracts and full shared metadata. Native loading is not a sandbox and
cross-target artifacts need a compatible host for installation. Inventory via
`python3 scripts/package-plugin.py --discover-root <directory>` checks installed
hashes/metadata without loading code or selecting competing candidates. Runtime
host discovery of this immutable layout remains integration work.
