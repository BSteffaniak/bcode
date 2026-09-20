# CI coverage

`Build and Test` uses a pinned Clippier action to discover affected Cargo packages
and generate feature/OS jobs. Linux is the workspace default. Package-local
`clippier.toml` OS lists replace that default, so native-platform exceptions must
retain Ubuntu explicitly. Windows covers native IPC, process trees, PTYs, paths,
credential custody, and locking; macOS additionally covers daemon identity,
locking, and credential custody. Portable models do not need separate native jobs.

Actions concurrency is bounded independently of feature chunking. Do not reduce
coverage by slicing the generated matrix or filtering out failing features. Manual
runs accept package selection and a reproduction seed; an empty selection runs the
whole workspace. The matrix summary explains affected packages. Structured failures
are uploaded for reproduction and aggregated into `workflow-failures-summary`.

Workspace formatting/check/clippy and offline prompt-cache evaluation remain on
Linux. Resolved architecture graphs for all three platforms also run on Linux;
these are dependency checks, not cross-compilation. Native PTY acceptance and
Windows contract/release-artifact checks remain separate from per-package checks.
The Windows job no longer repeats workspace-wide compilation and testing.

Release YAML source spelling is not a contract. Windows CI validates the actual
packaged archive, checksum, runtime payload, and extracted executable instead.
Validate workflow syntax with actionlint and exercise Clippier locally before
pushing matrix changes. Confirm the resulting matrix and terminal CI conclusions
on the pushed commit; local matrix generation alone does not prove CI succeeds.
