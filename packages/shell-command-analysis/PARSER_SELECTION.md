# POSIX parser selection

## Selected backend

`brush-parser` 0.4.0 is selected for the initial POSIX shell analysis backend.

Evidence recorded on 2026-07-24:

* crates.io reports 0.4.0 as the current stable release.
* `cargo info brush-parser@0.4.0` reports MIT licensing and `rust-version: 1.88.0`; Bcode's stable toolchain in this workspace is Rust 1.95.0.
* The dependency is added with `default-features = false` and is private to `bcode_shell_command_analysis`.
* Its direct normal dependency impact is `bon`, `cached`, `indenter`, `insta`, `peg`, `thiserror`, `tracing`, and `utf8-chars`.
* The parser exposes explicit `posix_mode` and `sh_mode` options, complete-program parsing, parser-owned source spans, and a structured AST with simple commands, pipelines, lists, background separators, groups, subshells, control flow, functions, substitutions, assignments, and redirections.
* `packages/shell-command-analysis/tests/parser_compatibility.rs` parses the complete sanitized corpus with POSIX and `sh` modes enabled. Every case expected to parse succeeded; the intentional syntax-error case failed; the non-POSIX process-substitution case is deliberately excluded from accepted POSIX syntax and remains fail-closed.
* An independent `sh -n` comparison accepted every corpus case expected to be POSIX syntax and rejected the intentional syntax-error case. On this macOS platform, `/bin/sh -n` unexpectedly accepts process-substitution syntax even though it is outside the selected POSIX contract; Bcode therefore follows the explicit POSIX boundary and marks it incomplete rather than inheriting that host extension.

## Acceptance rationale

The spike found no corpus-blocking parse failures. The AST distinguishes heredocs and quoted words from shell command boundaries and contains source spans, which are necessary for extraction from original source rather than parser `Display` output. The implementation still must prove exhaustive traversal: any AST variant that is not explicitly adapted will produce incomplete analysis and never automatic allow.

## Rejected alternatives

No secondary parser was evaluated because the leading candidate met the compatibility gate. The parser remains replaceable behind Bcode-owned request/result models; a future replacement must pass the same corpus and completeness invariants.
