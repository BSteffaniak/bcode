# Git workflow blocks and commit reconciliation

The Git plugin owns deterministic repository preparation, commit composition, commit dispatch, and commit-status reconciliation. Generic workflow and server packages only validate and route the manifest-declared typed contracts.

## Preparation

`git.prepare` is read-only and idempotent. Its request carries bounded include/exclude prefixes and a maximum changed-path count. The owner resolves the invocation workspace as the repository, captures the exact repository root and current HEAD, reads porcelain status without staging or modifying files, applies prefix policy, and returns an ordered bounded changed-path set.

Preparation output is a snapshot, not authorization. Later composition and dispatch preserve its exact repository root, HEAD, and paths. If the path set is empty, workflow policy must explicitly choose failure or a no-op branch.

## Commit composition

`git.compose-commit` is read-only and deterministic. It combines exact preparation output with a typed proposed message and explicit no-changes policy. The result is either:

* `ready`, carrying the exact `git.commit` request; or
* `no_changes`, carrying no mutating request.

The proposed message has a bounded single-line title and bounded description. Paths are unique, non-empty, repository-relative, and bounded. Composition does not stage or commit.

A commit-message agent, when configured, remains read-only. It receives the exact repository root, expected HEAD, and changed paths; uses a strict typed output schema; and cannot invoke `git.commit`. Required skill resolution fails before model dispatch. Optional-skill fallback is template policy and must be explicit.

## Exact mutation approval and dispatch

`git.commit` is mutating and declares repository/git-ref write claims, exact grant required, and `repair_required` reconciliation. Approval binds the complete normalized request checksum plus definition, run, node, activation, workspace snapshot, plugin/block/version/operation, resource claims, and reconciliation class.

Immediately before commit, the Git owner re-verifies:

* repository root and expected HEAD;
* requested changed paths;
* the complete staged path set, rejecting unrelated pre-staged files.

The owner uses exact path-limited commit arguments, lets Git hooks run normally, and returns previous HEAD, new commit hash, and exact committed paths. A hook failure or stale repository fact returns failure without pretending a commit occurred.

Prepared intent is durable before owner invocation. Owner acceptance receipt is durable before terminal observation. The validated typed commit result is persisted before activation completion and downstream materialization. Automatic replay cannot create a second commit.

## Ambiguity and explicit reconciliation

If the daemon loses the terminal result after owner acceptance, the attempt becomes `repair_required`. It is never replayed automatically.

`git.commit-status` is a read-only idempotent owner query using expected HEAD and exact paths. It reports:

* `not_committed`: HEAD still equals expected HEAD;
* `candidate_commit`: HEAD advanced and the candidate commit paths match;
* `diverged`: repository evidence does not prove the exact intended commit.

The result includes actual HEAD, actual candidate paths, and bounded guidance. Operators compare it with the persisted dispatch identity, intent, receipt, and approval scope. Explicit repair then confirms success with the typed commit output, confirms failure/cancellation, or abandons the attempt for a later explicit retry. Abandonment does not itself dispatch.

Normal list, status, open, and history paths remain bounded and non-mutating. They do not run Git commands, replay complete workflow history, or silently resolve ambiguity.
