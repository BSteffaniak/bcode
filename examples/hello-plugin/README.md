# Hello plugin example

The hello crate is an rlib whose `plugin_definition()` comes from Bcode's
product author macro (`bcode_plugin_sdk::define_plugin!`). Bundled hosts register
that definition directly. The same crate packages itself as a dynamic library
through BMUX's packager: `define_plugin!` emits the entry symbol only in
packaged builds, and the crate declares the `dynamic-backend`/`dynamic-frontend`
role features. No wrapper crate is involved.

```sh
python3 scripts/package-plugin.py \
  --implementation-manifest examples/hello-plugin/Cargo.toml \
  --component backend --build
```

See [Dynamic plugin packaging](../../docs/dynamic-plugin-packaging.md) for:

- component features;
- installation (`scripts/install-bundled-plugins.sh`, `--install-root`);
- the test fixtures that package this crate for dynamic coverage.

Native loading is trusted, not a sandbox. Installation publishes immutable
artifacts and never activates them, changes the selected runtime or restarts a
daemon.
