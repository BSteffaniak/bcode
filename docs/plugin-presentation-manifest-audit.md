# Plugin Presentation Manifest Audit

## Renderer-owned theme presentation

Native terminal visual adapters and full-screen plugin surfaces may receive a `PluginTuiTheme` from
the host. It contains generic canvas/text/border/focus/selection styles, semantic source and diff
styles, an RGB syntax palette, and a stable resolved fingerprint. This is presentation input only:
it cannot affect discovery, routing, authorization, dispatch, artifact interpretation, or persisted
tool outcomes.

The existing concrete terminal style path remains the compatibility fallback when a host does not
supply a theme. Adapters should prefer the semantic source/diff/syntax values when present and retain
readable defaults otherwise. Styled-row caches must include the fingerprint or invalidate when it
changes. Renderer-specific richness never removes the generic structured fallback, and bundled
plugins remain disableable.

Plugin manifests under `plugins/*/bcode-plugin.toml` own their routing declarations. There is no
second machine-readable inventory to synchronize with them. Renderer tests exercise registered
adapters with fixtures and verify generic fallback behavior rather than pinning registry counts.

## Cutover interpretation

* Visual-adapter manifest entries are legacy platform-routing declarations until their producers emit generic contributions and platform-owned registries select adapters solely by producer schema/version.
* TUI-surface entries are legacy base-plugin registry declarations until the injected platform-extension registry replaces them.
* Updating a manifest does not by itself prove producer migration or old-contract removal.
