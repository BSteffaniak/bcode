# Stack History

A pull-request stack has two axes: its layers, bottom to top, and time, as each layer is pushed
again. Reviewing a stack today shows one slice: every layer as it is now. This plan adds the
other axis, so a reviewer can see what changed in each layer, and across the stack, since any
earlier point: since their last review, since yesterday, since a particular push.

The central problem is noise. A stack tool's restack pushes every layer above a change, even
when only one layer changed. Rebasing a layer onto a moved parent changes its commits without
changing what it does. A naive "old head → new head" diff of a rebased layer shows everything
that landed below it. The design separates *what the author changed* from *what moved
underneath*, at every level.

## The model

* **Layer version:** one state of one pull request: its head commit, the commit it was based on,
  its base branch, when it was first seen, and how it came about.
* **Kind of change**, from the previous version of that layer:
  * *Content:* the layer's own change differs. Some file's patch differs, a file was added or
    dropped, or its commits were rewritten with different content.
  * *Rebase only:* every file's patch is identical (same patch ids); only the base moved.
  * *Commits added:* fast-forward on top of the previous head (a normal push).
  * *Retargeted:* the base branch changed (layers reordered, a layer below merged).
* **Stack push:** versions of several layers observed within a short window (two minutes by
  default). A `gt submit` that restacks 30 layers is one push: "2 layers changed, 28 rebased".
* **Version log:** every layer version ever seen, per repository and pull request number, shared
  by every review of it. It lives in the review store, not the cache: history cannot be
  recomputed once a force push drops the commits from the branch.

## Where versions come from

They are merged and deduplicated by head commit, each version keeping every source that saw it.

1. **The forge's timeline.** GitHub reports force pushes (the commits before and after), plain
   pushes (the commits added), and base changes, with times and who pushed. One aliased GraphQL
   request per batch of pull requests covers a whole stack, including history from before any
   review existed. The provider interface gains `review_source.pulls.history`; providers without
   it fall back to the other sources.
2. **Observation.** Every time a review resolves or confirms a stack, which it already does on
   every open, each layer's head and base are recorded. This works with any provider.
3. **Local reflogs.** For branches pushed or fetched locally, the reflogs of
   `refs/heads/<branch>` and `refs/remotes/<remote>/<branch>` hold every rebase. That covers the
   author's own stacks fully, and a reviewer's to the extent they fetched.
4. **Review passes.** The revisions recorded when a review pass completes (already kept as
   `reviewed_revisions`) mark which versions this reviewer saw.

Old versions' commits may no longer be on any branch. They are fetched by commit id in one
batched fetch; GitHub serves most of them. A version whose commits cannot be had stays on the
timeline as *unavailable*, with what is known about it (when, by whom, the kind of change when
the forge reported it), and cannot be compared.

## Comparing versions

Everything here works on commit ids, so every result is content-addressed and cached like
listings and hunks are today.

* **What changed in a layer** (the default comparison) between versions A and B:
  1. Each version's own change is `merge-base(base, head) → head`.
  2. Files whose patch id is the same in both are *unchanged by the push* and listed as such,
     collapsed. Patch ids already exist for viewed state.
  3. For the rest, the diff from A's head to B's head, with hunks that also appear in the base's
     own movement (A's base → B's base) marked as *from the rebase*. They are hidden by default
     and shown greyed on request.
  4. Files that only one version changes are shown as added to or dropped from the layer's change.
* **Other comparisons,** offered per layer:
  * *the full new version,* the normal layer diff, with lines new since A marked;
  * *raw head to head,* unfiltered, for when the filtering is in question;
  * *commits,* `git range-diff` of the two commit series: which commits were added, dropped,
    reworded, or changed.
* **Across the stack:**
  * *the stack at a time:* every layer pinned to its version at time T. This is "show me the
    stack as it was before yesterday's push".
  * *everything since T:* every layer's "what changed" since its version at T, as one review.
    This is the cross-section most reviews want: "what is new in this stack since I last looked".
  * *the whole stack since T:* trunk to top at T against trunk to top now, with trunk movement
    filtered out like base movement is for one layer.

Comparisons are review sources (a new source kind naming a layer and two versions, or a time),
so the review screen, file panel, viewed state, comments, and the loading work above apply to
them unchanged. A comment made on a comparison anchors to the newer version's lines.

## The view

* **The history panel** (a key away from any stack review): one row per layer, with the stack's
  pushes laid out left to right on a shared time axis. Each cell is a layer version, drawn by
  kind: content change, rebase only (faint), commits added, retargeted, reviewed by you (marked),
  unavailable. A restack push reads as one column, with its real changes standing out from the
  rebased layers around them.
* **Two pins,** *from* and *to*, chosen on the time axis or per cell; *to* defaults to now. With
  one layer selected, the review shows that layer's comparison. With a column (a push) or a
  range selected, it shows every layer that changed in it, each under its banner, skipping
  rebase-only layers unless asked.
* **Defaults that need no setup.** Opening a stack review you have reviewed before offers *since
  your last review* for every layer that changed since. The rail marks those layers with how much
  changed, and rebase-only layers say so instead of looking new.
* **Stepping through time,** like stepping through layers: next and previous version of the
  current layer, and next and previous push of the stack, with the document filling in place.

## Configuration

* Where versions come from: forge timeline, observation, and reflogs, each on or off.
* How long observed versions are kept (default: as long as the review exists, else 90 days).
* The stack-push window (two minutes).
* The default *from* pin: your last review, the previous version, or a time ("24 hours ago").
* Whether rebase-only layers and rebase hunks are hidden, shown faint, or shown plainly.
* Whitespace and rename handling, as for any diff.

## Also worth building

In rough order of value:

1. **Restack summaries on the rail and home:** "pushed 12 minutes ago: #52140 and #52151
   changed, 28 layers rebased". This is the cheapest high-value piece; it needs only the version
   log and patch ids.
2. **A file through the stack:** one file's history across the layers: which layer adds it,
   which change it, and which delete it, with each layer's hunks in order. It builds on the
   existing stack line attribution (`review.stack.attribution`).
3. **Outdated comments that follow the code:** comments anchored to an older version show where
   their lines went in the current one (or that they were removed), instead of just "outdated".
4. **Overlap map:** which layers touch the same files and lines. These are the places a change
   low in the stack will cause restack conflicts, and where review attention compounds.
5. **Moved code:** code that moved between layers or files (git's moved-line detection),
   shown as a move instead of a deletion here and an addition there.
6. **Checks per version:** CI status on each cell of the history panel, so a version that broke
   the build is visible on the timeline.
7. **Summaries per push:** an assistant-written "what changed in this push" per layer and per
   push, generated from the "what changed" comparison rather than the noisy one.

## Phases

1. **Version log and classification:** the store table, observation on every stack confirm,
   reflog import, `pulls.history` for GitHub, and change kinds from patch ids. Testable on
   synthetic restacked repositories (the budget test's fast-import chain, pushed twice). Item 1
   above (restack summaries) can ship with it.
2. **Comparison engine:** the per-layer "what changed" with rebase filtering, the other
   comparisons, range-diff, batched fetching of old commits, and caching. Tests compare against
   hand-built restacks: one layer edited and the rest rebased; a layer retargeted; a version
   unavailable.
3. **Comparison sources:** the new source kind, resolution, and "since your last review"
   defaults, so comparisons open as reviews before any new panel exists.
4. **History panel:** the matrix, pins, push columns, stepping through versions, and
   configuration.
5. **Stack-wide views:** the stack at a time, everything since a time, the whole stack since a
   time, and a file through the stack.
6. **The rest of "also worth building",** as wanted.

## Decisions

* The version log is per user, in the local review store. Sharing it with a team would need a
  server and is out of scope for now.
* The forge timeline is fetched when the history panel opens. Versions are also observed on
  every stack confirmation, which costs nothing extra, so the default open path is unchanged.

## Status

Built, with what differs from the plan:

* **Version log** (`store/history.rs`, `review.history.record` / `review.history.list`): per
  user in the review store, one row per pull request and head, merging what each source saw.
* **Sources:** every stack a review resolves or confirms is recorded as observed versions; branch
  reflogs are read from their files (`history::reflog`; `git reflog` for the reftable format), so
  each entry carries when the branch moved; GitHub's push history comes from
  `review_source.pulls.history` (`timelineItems`: force pushes, commits, base changes) when the
  history panel opens, with old versions' commits fetched into `refs/bcode-review/version/`.
* **Timeline and kinds** (`history::timeline`): one commit check, one history walk, and one
  `git merge-tree --stdin` for the whole stack. A version's fork point is its recorded base, else
  the closest version of the layer below in its history, else where it left the trunk. A version
  with no change of its own (a branch just created) is not a version. *Rebase only* is decided by
  replaying the old change on the new fork point: identical trees.
* **Comparisons** (`history::compare`) are `CommitRange` sources from a replay commit (the old
  change on the new base, made with `commit-tree`, on no branch) to the new head, so the review
  shows exactly what the author changed; a conflicting replay falls back to head against head and
  says so. One layer between versions, every layer between two times, everything since a time,
  the stack as it was, the whole stack since a time (trunk movement taken out), and since your
  last review.
* **Screen:** the history panel (`shift+b`): layers by pushes, cells by kind, reviewed versions in
  green, a cursor and a *from* pin; each comparison opens as its own review and closing it
  returns to the review it came from. The layer strip shows the last push ("last push 12m ago:
  #12 changed, 28 rebased"). *This file through the stack* and *files several layers change*
  (palette commands, no default keys) list the layers that change a file and jump to them.

Not built yet, from "also worth building": outdated comments that follow their code, moved-code
detection, checks per version, and summaries per push. The restack summary is on the layer strip,
not yet on home.
