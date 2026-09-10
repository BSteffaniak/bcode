# Contributing

Bcode is early-alpha software. Start with a focused issue or pull request describing the user-visible problem, current behavior, proposed change, and how you tested it. Do not include credentials or private session data.

## Development

Use stable Rust and Git. Follow [Getting started](docs/getting-started.md) for the terminal-agent build; the full distribution adds native build requirements. Read [AGENTS.md](AGENTS.md) and applicable [INVARIANTS.md](INVARIANTS.md) sections before changing architecture.

Keep changes domain-owned: reusable data contracts belong in their domain's model crates, behavior belongs in the owning service/plugin, and presentation must not leak into frontend-neutral APIs.

## Validation

For Rust changes, format first, then run the applicable checks:

```sh
cargo fmt
cargo check --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

Run tests for affected packages and the architecture guards listed in `AGENTS.md`. For example, SDK changes should exercise the lean SDK without application features; session changes require persistence/ownership checks. Report exactly what ran, including any blocked platform-specific checks.

Docs-only changes do not require runtime validation. Check relative links, CLI examples, default features, and the distinction between public releases and source-only functionality. CLI/configuration references are generated from code; do not create a competing hand-maintained full reference.

## Security

Use [SECURITY.md](SECURITY.md) for private reports. Permission policy, native plugins, and executed tools have distinct trust boundaries; describe them explicitly rather than claiming the agent is an OS sandbox.
