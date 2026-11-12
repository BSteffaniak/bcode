# Review Discovery and Enrichment

Code review finds what to review (a pull request, the stack it belongs to, the branch you are on)
and what to say about it (a layer that needs a restack, a parent that disagrees with the forge)
from evidence spread across tools: the forge, git itself, and stacking tools such as Graphite,
git-town, ghstack, or spr. This document defines how that evidence reaches the review without
code review knowing any of those tools by name.

## Roles

Three roles, each a plugin service with a versioned interface. Plugin services cannot call each
other, so the client gathers their answers and passes them on (as it already does for
review-source providers).

| Role | Interface | Answers | Authority |
| --- | --- | --- | --- |
| Forge provider | `bcode.review_source/v1` | exact questions about pull requests: by number, by head branch, the open pull requests next to a branch, fetching their commits | authoritative |
| Discovery contributor | `bcode.review_discovery/v1` | which branches belong together (parent links, related branches) from local evidence | hints only |
| Enrichment | `bcode.review_discovery/v1` (same response) | annotations on branches: warnings and notes with a stable code | informational |

Discovery and enrichment share one operation because the same evidence usually yields both (a
stacking tool's metadata names a branch's parent and also knows whether it needs a restack).

### Evidence versus authority

The forge decides what a stack is: reviewers see the forge's pull requests, so the review must
match them. Contributors only suggest which branches to ask the forge about. A wrong or stale
hint costs one wasted lookup, never a wrong stack. When a contributor disagrees with the forge
(a stacking tool records a different parent than the pull request's base), the stack follows the
forge and the disagreement is shown as a warning naming the contributor.

### Failure

Every contributor is optional and fails soft: an error, a timeout, or an unreadable answer drops
that contributor's hints and adds a warning; resolution continues with the others. The forge's
own branch walk always works alone, so discovery only ever makes resolution faster or richer.

## `bcode.review_discovery/v1`

Contracts live in `packages/code-review/models` (`review_discovery.rs`) and carry no
tool-specific fields.

| Operation | Request → response |
| --- | --- |
| `review_discovery.manifest` | → contributor id (stable, used as the source of its hints and annotations) and label |
| `review_discovery.discover` | repository path, seed branches, trunk (when known) → `links`, `related`, `annotations` |

* **Seeds** are the branches the client cares about (a pull request's head branch, the current
  branch). A contributor answers about the seeds' neighborhood; it may return more when that is
  as cheap (a stacking tool's whole branch graph is one read).
* **`links`**: `branch → parent` pairs, optionally with the parent commit the contributor
  recorded.
* **`related`**: branch names likely in the same stack whose position the contributor does not
  know (for example, branches whose history contains the seed).
* **`annotations`**: `{ branch, severity (info | warning), code, message }`. Codes are stable per
  contributor (`needs_restack`, `invalid_metadata`, …) so the app can filter or act on them.

Contributors must be cheap and local: they read git state and tool metadata, not the network.
Bounds: a contributor returns at most 1,000 links and 1,000 related branches.

## How a stack resolves

1. The client knows (or looks up, one forge query) the requested pull request's head branch.
2. It asks every discovery contributor for that seed, in turn, and merges the answers: links are
   kept with their source, related branches are unioned, annotations are kept with their source.
3. The hints (the seed's connected component in the links, plus related branches) go to the
   forge provider with `review_source.stack.resolve` (`hints`).
4. The provider looks every hinted branch up in batched queries (many branches per request),
   then walks the stack in memory from those answers. A branch the hints missed is looked up on
   its own, so a missing or wrong hint changes the cost, never the result.
5. The client adds the warnings: annotations on the stack's branches, and links that disagree
   with the stack's actual parents.

The provider looks branches up in batches of 40 as the walk needs them: a branch it reaches is
looked up together with the next hinted branches not looked up yet, in the order discovery
ranked them (the seed's linked branches first, then related ones). Hints the walk never reaches
beyond the current batch cost nothing.

Measured on an 87-layer stack in a repository with over 2,000 open pull requests: the unhinted
walk took 88 sequential queries (33 s); with Graphite and git hints it takes the start lookup
and three batched requests (4–5 s).

## Fetching

Every layer needs its head and base commits under review-owned refs
(`refs/bcode-review/pull/<N>/{head,base}`).

* One open resolves each stack once and fetches each pull request once, however many sources
  (for example "step through every layer") name it.
* The client plans first: it resolves every source's stack, collects every pull request any
  source needs, then fetches them in one `review_source.pulls.fetch` per provider (when the
  provider advertises `batched.pulls_fetch`; otherwise one `review_source.pull.fetch` each).
* `review_source.pulls.fetch` takes the head and base commits the forge reported. Commits already
  present locally (always the case for your own stacks) are pointed at with one
  `git update-ref --stdin`, found with one `git cat-file --batch-check`; the rest come in one
  `git fetch` with all their refspecs. The 87-layer stack above, all local, takes under half a
  second (it took about 50 s as one fetch per layer).
* The GitHub provider keeps one client per repository for the plugin's lifetime, so a lookup,
  a resolution, and a fetch share a token and a connection.

## Contributors

### Git topology (`packages/code-review`, `discovery::git`)

From the seeds' tips: branches whose history contains a seed (layers above) and branches a seed
contains that the trunk does not (layers below), local and remote-tracking, without remote
prefixes. One `git for-each-ref` per seed; no tool required.

### Graphite (`packages/code-review-graphite`; `plugins/graphite-plugin` serves it to Bcode)

Reads Graphite's branch metadata from git (`refs/branch-metadata/<branch>`, one JSON blob per
tracked branch) with a single `git cat-file --batch`, so it needs neither the `gt` binary nor the
network:

* links: `parentBranchName` (with `parentBranchRevision`);
* `needs_restack` (warning): the recorded parent revision is no longer the parent branch's tip —
  the same rule `gt` applies (verified against `gt state` on 104 tracked branches);
* `invalid_metadata` (warning): `validationResult` other than `VALID`.

The metadata format is Graphite's internal one, so fields are read leniently: unknown fields are
ignored and a blob that does not parse is skipped. Graphite's "last submitted" record is not
used: pushes made outside `gt submit` do not update it, so it is not reliable evidence.

The `gt` CLI is not used for discovery: its documented commands print human-readable text one
branch at a time, and its JSON (`gt state`) is undocumented and takes seconds. Running stacking
commands (`gt restack`, `gt submit --stack`) from the review is a separate concern: actions, run
through the ordinary shell permission policy.

### Other tools

git-town (`git-town-branch.<branch>.parent` in git config), ghstack and spr (branch naming and
commit trailers), Sapling and Jujutsu each fit the same interface as their own plugin.

## Where warnings show

Discovery warnings join the stack's own warnings (`ReviewStack::warnings`), which the new-review
wizard lists on its stack step. A saved review re-resolves its stack on open with the same hints,
but the resolved sources the review surface receives do not carry stack warnings yet.

## Phases

1. Branch-walk stack resolution and lookups by number, URL, or head branch. Done.
2. One resolution and one fetch per pull request per open; batched, local-first fetching. Done.
3. Batched branch lookups in the forge provider, driven by hints. Done.
4. `bcode.review_discovery/v1`, the git topology contributor, and the Graphite plugin, with
   their warnings on the stack. Done.
5. Next: stack warnings in the review surface; a per-repository cache of resolved stacks,
   revalidated in the background (one batched query by pull request number); home suggestions
   built from discovery results; stacking-tool actions (`gt restack`, `gt submit --stack`) offered
   from the review through the shell permission policy.
