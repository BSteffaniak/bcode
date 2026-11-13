# Review Threads: Cards, Viewed, Resolved and Folded

Comment threads in the review (your drafts, pull request threads, imported threads) get a real
frame, and three states that today are tangled get separated:

* **Resolved:** whether the discussion is settled.
* **Viewed:** whether *you* have read it as it is now.
* **Folded:** whether it shows as one line.

## What is wrong today

* **No structure.** A thread is a header line, indented markdown and a row of links, on the same
  canvas as the code. Where it starts and ends, and which comment is whose, is hard to see. A
  pull request thread above a banner reads as part of the banner.
* **Resolved means hidden.**
  * A resolved thread does not draw its comments even when opened (`thread_rows` and
    `remote_thread_rows` skip the body when resolved).
  * `fold_resolved` folds it too.
  * So *resolve* is the only way to put a thread away. Resolving is a statement to everyone
    ("this is settled", and on a forge, a shared state change); putting a thread away is a
    private act. Using one for the other is wrong both ways: you resolve threads you only wanted
    out of sight, and you cannot read a resolved one without reopening it.
* **Folding forgets why.** `collapsed_threads` keeps a fold forever. A thread you folded gets a
  new reply, and you never see it.

## The three states

| State | Whose | Where it lives | Changes when |
|---|---|---|---|
| **Resolved** | everyone's | the forge for pull request threads; the draft's `resolved_at_ms` for yours | you or anyone resolves or reopens it |
| **Viewed** | yours | the workspace, next to viewed files | you mark it; it lapses when a new comment arrives |
| **Folded** | yours, presentation | the presentation state | by itself from viewed (and resolved, by setting), or by hand |

The rules:

* **Viewed is pinned to the content.** A mark records the thread's last comment (id, and the
  count of comments). A new comment makes the mark stale. The thread is then *unviewed with N
  new*, opens, and says what is new. This is exactly what viewed files do when their diff
  changes.
* **Viewed never touches resolved, and resolved never touches viewed.**
  * Someone resolving a thread does not mark it read for you.
  * You marking it viewed tells nobody anything.
  * One convenience: *you* resolving a thread also marks it viewed, since you read it to decide.
    This is a setting (`threads.resolve_marks_viewed`, default true).
* **Folding follows viewed, like files.**
  * By itself, a thread is folded when viewed (`threads.fold_viewed`, default true), or when
    resolved and `fold_resolved` is set (default true).
  * Folding or opening by hand sticks until the thread is marked viewed or unviewed again.
  * A new comment from someone else opens the thread again, even one folded by hand
    (`threads.new_comments = "reopen"`, the default). This is the case today's folds get wrong.
    With `"keep_folded"`, the thread stays folded and its header says *N new*.
* **Opened, a resolved thread shows everything.** Resolved no longer hides comments; it is a
  chip and a quieter frame.

So "I've read this, get it out of my way" is **Viewed** (`v`, or the checkbox): it folds,
persists, and comes back by itself when someone replies. "This is settled" is **Resolve**, as
the forge means it.

## The card

Every thread is a card, framed with the rounded border the rest of Bcode uses. The frame costs
no extra rows: the header sits in the top border and the buttons in the bottom border, where
today they are rows of their own.

Open, unviewed, waiting on you:

```text
        ╭─ ▾ 💬 ana · 3d ago · 2 comments ── waiting on you ── 1 new ──────────── ○ Viewed ─╮
        │ ana · 3d ago                                                                      │
        │ This couples the parser to the storage layer; could it take an interface instead? │
        ├───────────────────────────────────────────────────────────────────────────────────┤
        │ you · 1d ago                                                            • new     │
        │ Agreed, I'll split it into an interface and two implementations.                  │
        ╰─ Reply · Resolve · Open ↗ · Ask Bcode ────────────────────── outdated · L12-18 ─╯
```

Folded (viewed, or by hand), one row:

```text
        ╶─ ▸ 💬 ana · 2 comments ─ resolved ─────────────────────────────────── ✓ Viewed ─╴
```

The parts:

* **Top border:**
  * the fold chevron (a click folds or opens);
  * the author and age of the first comment, and the comment count;
  * chips: *waiting on you*, *N new*, *resolved*, *outdated*, the placement (*moved with its
    code*, *on an older version*, *on a file this layer no longer changes*), and the held-reply
    count;
  * on the right, the **Viewed** checkbox (`○ Viewed` / `✓ Viewed`), clickable.

  Chips drop from the end when the row is narrow; the chevron, author and checkbox stay.
* **Each comment:**
  * a byline (author, age, and *edited* or *• new* when they apply), then the markdown body;
  * comments are separated by a `├───┤` divider;
  * your held replies say *held for your review*;
  * your local drafts have no byline, since they are all yours, only the age.
* **Bottom border:** the buttons the thread allows, left, and the line reference and
  outdated reason, right.
  * Pull request threads: Reply, Resolve/Reopen, Open ↗, Ask Bcode.
  * Your drafts: Reply, Edit, Delete, Resolve/Reopen, Ask Bcode.
  * Suggestions keep their Accept, Refine and Reject row inside the card, above the bottom
    border.
* **Frame color says what needs you:**
  * the accent for unviewed;
  * the warning color for *waiting on you*;
  * the muted border for viewed;
  * the success color, dimmed, for resolved.

  Under `color_depth = "none"`, unviewed threads draw a bold frame.
* **Width and placement:**
  * The card starts at the code's text column (past the gutter), so it hangs under the line it
    is about.
  * It is at most 110 columns wide, with the wrap width following, so long comments stay
    readable on wide screens.
  * On a review-level or layer-level thread it starts at the left edge.
* **Narrow and plain:**
  * Under 40 columns the side borders go and only the top and bottom rules stay.
  * The ASCII icon set draws `+`, `-` and `|`.
* **Themeable:** roles `source.thread.border` (viewed), `source.thread.unviewed`,
  `source.thread.waiting` and `source.thread.resolved`. Unset ones are derived as above. Like
  `source.file.*`, they are not derived by the theme seed, so the review's own derivation
  applies.
* **Roomy** (`threads.card = "roomy"`): plain top and bottom borders, with the header and the
  buttons on rows of their own inside the frame (two more rows per thread).

Your drafts, imported threads, pull request threads and AI answer cards all use the same frame
(`rows/thread_card.rs`).

**AI answer cards** are cards like the rest:
* The header is `✦ Bcode`, its status (with the spinner while it works), its usage, and the
  Viewed checkbox.
* The body is sections: the question, the turn, a permission prompt, and suggestions.
* The frame is the accent while unviewed, the warning color while it needs you, the error
  color when it failed, and muted once viewed.
* Bcode's answer counts as a comment from someone else. A card is keyed `ai:<exchange id>`,
  and its one "comment" is the exchange's latest update, so a new answer (a follow-up) makes a
  viewed card unviewed again.
* `z` and the chevron fold it like a thread. **More**/**Less** still show the whole answer or
  its preview.

## Interaction

* **`v` on a thread** toggles Viewed. Elsewhere in a file it still marks the file. A thread
  marked viewed folds, and the cursor stays on it.
* **`z`** folds or opens by hand, as today.
* **Resolve** stays on its key and button.
* **Mouse:** the chevron, the Viewed checkbox, and every button. A click anywhere else on a
  folded card opens it.
* **Palette:**
  * *Mark every thread viewed* (and in the current file);
  * *Open every thread*;
  * *Fold every thread*.

  `shift+z` keeps folding or opening every thread.
* **Motions:** thread motions keep stopping on folded threads.
  * `attention_next` goes through unviewed files, then **unviewed threads** (new), then threads
    waiting on you.
  * *Open thread* motions (`alt+]` and the like) stay about resolution.
* **Threads panel:**
  * each row shows ○/✓ viewed and the new count;
  * the filters gain *unviewed*, after *waiting on you*.
* **Header bar:** next to `viewed 12/15` (files), a `threads 3/7` count of viewed threads appears
  when the review has threads.
* **The pull request bar and cards:** *N new* counts unviewed comments across the pull
  request's threads. It is still driven by `conversation_seen` for arrival toasts. Marking every
  thread viewed clears it.

## Model

* **Core (`bcode_code_review::threads`):**
  * `ThreadViewed { Unviewed, Viewed, Stale { new } }`, from a mark and the thread's comments.
    Your own comments never make it stale.
  * `NewComments { Reopen, KeepFolded }` and `FoldRules`.
  * `folded(hand, viewed, resolved, comments, rules)` decides whether a thread shows folded.
* **Models:**
  * `ThreadViewMark { thread_id, comment_ids, viewed_at_ms }`.
  * `ThreadFoldMark { folded, comments }`: a fold by hand and how many comments the thread had.
* **Workspace (`ReviewWorkspace::viewed_threads`):** the view marks, saved with viewed files in
  their own column (store migration 025). Unsaved reviews keep them with their viewed files.
* **Presentation state:**
  * `thread_folds` holds the folds by hand.
  * `collapsed_threads` from before reads as folds by hand without a count, which never lapse.
* **UI (`surface/threads.rs`):**
  * `refresh_threads` computes each thread's viewed state and which threads show folded. It
    runs whenever notes, marks, folds or stored viewed state change.
  * Pull request comments the viewer wrote are known from the provider's `is_viewer`.
* **Provider neutrality:** viewed is local; no provider call. Nothing about it names a forge.

## Configuration

These live in `[plugins.config."bcode.code_review".conversation.threads]`:

* `fold_viewed` (default true);
* `resolve_marks_viewed` (default true);
* `new_comments`: `"reopen"` (the default) or `"keep_folded"`;
* `card`: `"compact"` (the default) or `"roomy"`;
* `width` (default 110; `0` means the full width).

`fold_resolved` stays in `conversation` (default true): resolved threads fold by themselves.

## Status

Implemented, not committed.

* **States:**
  * view marks, staleness, the fold rules, hand folds with their comment count;
  * persistence, with old folds read in;
  * `v` on threads and the checkbox;
  * palette actions to mark every thread viewed or unviewed, and to fold or open every thread;
  * resolving marking viewed, and resolved threads showing their comments when open.
* **The card:**
  * the compact and roomy layouts and the folded row;
  * dividers and bylines with *• new*;
  * state colors, the width cap, the left edge for review- and layer-level threads;
  * narrow and ASCII fallbacks, and the theme roles.
* **Around it:**
  * AI answer cards on the frame, viewed and folded like threads;
  * the threads panel's viewed marks and *unviewed* filter;
  * the `threads N/M` header count (threads and answers): a click, or the
    `unviewed_thread_next` action, goes to the next one not viewed;
  * attention through unviewed threads and answers, between unviewed files and open threads;
  * the pull request bar's *N new*:
    * reviews and conversation comments since the pull request was last looked at;
    * in its threads, what view marks say: nothing in a viewed thread, the comments since in
      one viewed before, and comments since last looked at in one never viewed.
* **Tests:**
  * core tests for staleness and every fold rule;
  * screen tests for:
    * the card frame, viewing to fold, and a reply from someone else reopening it;
    * `keep_folded` keeping it folded with *1 new*;
    * the bar's *1 new* clearing once viewed;
    * the roomy layout, and persistence;
    * an AI card on the frame: viewed folds it, `z` opens it, and the header count finds it.
