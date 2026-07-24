# Legacy-to-structured shell policy differential

The sanitized corpus records the known legacy failure classes and the reviewed structured result. This is explanatory review evidence, not a compatibility target: security defects must not be preserved.

## Security corrections: legacy allow to structured deny

* Newline-delimited denied sibling.
* Single-`&` background denied sibling.
* Denied leaves in pipelines, boolean lists, subshells, and command substitutions.
* Syntax errors, dynamic executable names, `eval`, `source`, and unsupported process substitution.
* Output redirections denied by write policy.

These differences close authorization bypasses or fail-closed gaps.

## Reviewed false-positive corrections: legacy deny to structured allow

* Quoted or escaped `|` in `rg`/`grep` arguments.
* Quoted `;` in `printf`, SQL, AWK, Git formats, and interpreter arguments when policy allows the command.
* Heredoc body text that resembles shell source.
* POSIX loops and conditionals whose executable leaves are allowed.
* Reviewed `git --no-pager` aliases when the more-specific intended subcommand rule wins.
* Static input redirections allowed by read policy.

Automatic allows are enabled only where the corpus supplies complete analysis and explicit allowing rules. Assignment-prefixed commands remain conservative and do not receive aliases.

## Correct denials retained

* Missing `printf` and `command -v` wildcard rules remain policy gaps.
* Broad interpreter rules retain their configured action.
* More-specific original deny rules beat additive aliases.

`packages/agent-policy/tests/shell_command_regression_corpus.rs` evaluates every corpus case through the structured analyzer and policy evaluator and asserts its reviewed decision. It also verifies that every case is classified and represented.
