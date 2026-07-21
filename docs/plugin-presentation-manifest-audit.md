# Plugin Presentation Manifest Audit

This inventory covers every bundled plugin manifest that declares `[[visual_adapters]]` or `[[tui_surfaces]]`. It records the exact producer schemas and TUI surface kinds that remain during the hard cutover; it does not claim those legacy declarations have been migrated.

The canonical machine-readable inventory is [`scripts/plugin-presentation-manifest-inventory.tsv`](../scripts/plugin-presentation-manifest-inventory.tsv). Run:

```sh
scripts/check-plugin-presentation-manifests.sh
```

The check parses every `plugins/*/bcode-plugin.toml`, rejects duplicate schemas or surface kinds within a plugin, and fails when a declaration is added, removed, reordered, or renamed without an explicit inventory update. The inventory currently covers 39 visual adapters and 18 TUI surfaces across 16 plugins.

## Cutover interpretation

* Visual-adapter manifest entries are legacy platform-routing declarations until their producers emit generic contributions and platform-owned registries select adapters solely by producer schema/version.
* TUI-surface entries are legacy base-plugin registry declarations until the injected platform-extension registry replaces them.
* An inventory update acknowledges and classifies a manifest change; it does not satisfy the corresponding producer migration or old-contract removal checkbox.
* Plugins with neither declaration are intentionally absent from the inventory and are still scanned, so adding either declaration fails the check.
