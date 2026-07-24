# Plan-agent shell policy audit

This audit separates parser corrections from policy expansion. It is based on the sanitized historical failure-class corpus and the aggregate read-only historical baseline recorded in `fixtures/shell-command-analysis/README.md`.

## Decision

Do not add any new built-in plan-agent command allows in this hardening change.

The following proposed rules were reviewed and rejected as defaults:

* `printf *`: commonly read-only, but output redirection and substitution can create effects; structured redirection policy closes some, not all, composition risks.
* `command -v *`: narrow in ordinary use, but shell `command` can invoke commands when used without `-v` and wildcard matching is textual. Users can add exact or project-specific rules if needed.
* `printenv *`: normally read-only, but not required for parser correctness and remains a policy choice.
* `ps *`: normally observational, but platform-specific options and information exposure make it an explicit policy choice.
* `lsof *`: normally observational, but can expose sensitive process and file information and may be unavailable by default.

The parser must still classify all five correctly. A denial caused by the default `* = deny` rule is a policy configuration gap, not a parser defect.

## Existing broad-rule risks

No implicit code-level allows are introduced. Broad user rules for `python -c *`, `python3 -c *`, `cargo run *`, `go run *`, `find *`, `xargs *`, `curl *`, and `timeout *` can execute arbitrary or mutating behavior. These risks are documented in `docs/permissions.md`.

## Historical findings applied

The historical corpus exposed quote-insensitive splitting, missing newline/background boundaries, Git global-option mismatches, assignment-prefix mismatches, shell grammar/preamble mismatches, and heredoc mutation false positives. Those are analysis or canonicalization defects and are addressed structurally. Commands with complete analysis but no allow rule remain denied deliberately.
