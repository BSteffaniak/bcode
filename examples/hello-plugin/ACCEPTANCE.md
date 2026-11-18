# Shared native acceptance

BMUX owns the acceptance driver. `scripts/package-plugin-acceptance.py`
delegates to the driver of the same BMUX revision as the selected packager (the
git-pinned SDK checkout) and passes this worktree as `--bcode-root`. Every
artifact is a crate packaged wrapper-free with one role feature.

Generation only (no compilation):

```sh
python3 -B scripts/package-plugin-acceptance.py --output target/plugin-acceptance
```

Build the artifacts and run the identity harness:

```sh
nix develop --command python3 -B scripts/package-plugin-acceptance.py \
  --output target/plugin-acceptance --build --run-acceptance
```

This builds:

- BMUX's scoped-loader fixture;
- BMUX's hello;
- Bcode's file-references, filesystem and shell backends.

The driver records each artifact's exact Cargo JSON path and SHA-256 identity
evidence in `acceptance-artifacts.json`. No artifact is installed, selected in a
daemon or invoked by the driver.

Bcode's own dynamic test coverage does not depend on this driver.
`scripts/package-plugin-fixtures.py` packages the fixtures that
`cargo nextest run` sets up automatically; see
[Dynamic plugin packaging](../../docs/dynamic-plugin-packaging.md).
