# Shell command analysis regression corpus

This directory contains sanitized shell programs derived from failure classes observed in historical Bcode sessions. It intentionally contains no session IDs, paths, prompts, outputs, or other private session content.

`corpus.json` is the reviewed behavioral contract for POSIX shell analysis and command-policy evaluation. Each case records:

* `classification`: whether the legacy failure was a parser-boundary defect, canonicalization defect, policy configuration gap, or a dynamic/unsupported construct that must fail closed.
* `expected.commands`: executable subjects in source order. Keywords, assignment prefixes, redirection operands, and heredoc bodies are not executable subjects.
* `expected.redirections`: syntax-derived filesystem effects. Heredoc bodies are represented as heredocs, never reparsed as shell source.
* `expected.completeness`: `complete`, `incomplete`, or `error`.
* `expected.policy`: the expected aggregate policy result under the case's ordered rules. Missing matches use `default_action` and are policy outcomes rather than parser failures.

## Invariants

1. Every execution-capable leaf is emitted exactly once or the analysis is not complete.
2. Source spans are UTF-8 byte ranges into the unchanged original source.
3. Quoted or escaped separators do not create executable subjects.
4. Newlines, `;`, `&`, `&&`, `||`, and pipelines create independently evaluable execution leaves where shell grammar permits.
5. Assignment prefixes are metadata on a command and are not executable identities.
6. Heredoc bodies are data and are never traversed as shell programs.
7. Input and output redirections are evaluated through read and write policy respectively.
8. Command decisions and redirection decisions aggregate as `deny > ask > allow`.
9. Syntax errors, unsupported execution constructs, dynamic command identities, incomplete traversal, and exceeded limits never aggregate to `allow`.
10. Canonical candidates are additive: the original subject remains evaluable and an alias cannot conceal a more restrictive match.
11. Missing wildcard rules are policy configuration gaps, not parser defects.

## Historical baseline

The read-only investigation scanned 324 historical session databases (excluding the investigation session) and observed 339 plan-agent shell denials in 92 sessions. It conservatively classified 260 likely false positives in 79 sessions. Of the denials, 257 matched `*`, 66 matched `git *`, and 16 came from the mutation heuristic. Candidate categories overlapped: 166 command/root mismatches, 59 Git global-option mismatches, 21 environment-prefix mismatches, 17 shell-grammar/preamble mismatches, and 11 quoted/heredoc mutation misclassifications. 160 denials in 60 sessions contained separators inside quoted strings.

These aggregate counts are retained for differential validation; private historical command text is not included.
