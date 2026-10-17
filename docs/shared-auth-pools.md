# Shared authentication pool policy

Authentication pools belong to authentication configuration, not model presets.
Define routing policy once and explicitly reference it from each preset. Account
credentials remain in their vaults; enrolled membership, cooldowns and interactive
preference remain managed runtime state. Migration does not read or rewrite them.

## Shared file

For example, `~/.config/bcode/auth-pools.toml`:

```toml
schema_version = 1

[auth.pools.openai]
provider_plugin_id = "bcode.openai-compatible"
automatic_provider = "openai"
automatic_group = "subscription"

[auth.pools.openai.priming]
enabled = true
```

Retain your existing strategy, priming, preferred profile, quota hints, and declared
members. The automatic identities opt this pool into default enrollment for the
registered provider compatibility group; a pool name alone does not imply that.
Only one automatic default per provider/owner/group is supported.

Each model preset explicitly references the shared file at its **top level**, before
any table header:

```toml
auth_pool_config = "../auth-pools.toml"

[model]
auth_pool = "openai"
```

Relative paths resolve against the declaring preset's directory, not the current
working directory. Absolute paths also work. `BCODE_CONFIG`, CLI-selected files and
exclusive file loads all honor this explicit reference. No new ambient global file
is loaded. Existing configuration layer precedence remains unchanged.

The shared document is versioned and contains only `schema_version` and
`[auth.pools]`. It cannot import files, declare accounts, plugins, model settings or
contexts. Unknown versions/fields, missing files and duplicate local/shared pool
definitions fail closed. Inline TOML and transported effective configuration cannot
carry unresolved file references; transports contain already resolved policy.
Named contexts still use only their own declared auth policy and do not inherit
these global pools or runtime accounts. Context-local definitions are not migrated.

## Reviewed migration

Use the newly built executable (older binaries do not understand the reference):

```sh
bcode auth pool migrate-config \
  --destination /home/me/.config/bcode/auth-pools.toml \
  --source /home/me/.config/bcode/providers/one.toml /home/me/.config/bcode/providers/two.toml
```

This is a dry run. It reports paths and pool names, never configuration contents.
Add `--apply` after review. To also mark one pool as automatic/default, add:

```sh
--default-pool openai --provider openai
```

The enabled provider plugin supplies and validates the compatibility identity;
provider-specific group inference is not hardcoded into migration.

Migration consolidates only identical definitions. Conflicting strategies, members,
preferences, or priming settings require explicit resolution, not guessed merging.
It preserves unrelated TOML and comments, leaves model references intact, locks all
changed files in path order, checks reviewed source bytes, and retains private
`*.toml.pool-migration.bak` files. It does not authenticate or contact a model.
Destination parents must already exist. Repeated migration is a no-op.

### Interruption and declarative configuration

Publication is per-file, not a multi-file transaction. Shared policy is published
first, presets next, and an optional automatic-default marker last. An interruption
can leave duplicate local/shared definitions; loading then fails closed. Replan the
same migration to finish. Backups are never overwritten; if a different older backup
exists, preserve it under another name before retrying. I/O errors may mean a prefix
of the plan was applied: inspect reported files before retrying.

Symlinks (including Home Manager/Nix-generated configuration) are rejected rather
than replaced. For declarative presets, change their generator to emit one shared
file plus the explicit reference, remove repeated global pool declarations, and
activate that configuration only after installing the new binary/plugins. Never
edit Nix store files. No credential reenrollment is needed.

Restart the application/daemon to adopt the new configuration. Already transported
client contexts retain their configuration snapshots. This composition change does
not expand live credential-acquisition authority.
