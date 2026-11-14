# Disposable Daemons and Dormant Clients

Daemons are disposable. A daemon exists to do work; when it has none it shuts itself down,
whether or not clients are connected. Clients never keep a daemon alive by being open: an idle
client sends nothing to the daemon and holds no connection that it needs. When real work is
needed again, the client starts a daemon of its own build (from its pinned image) and carries on.
The whole cycle is handled by the client–daemon communication layer, so frontends (the TUI, the
code review, plugin surfaces) get it without special handling.

This refines the [daemon lifecycle](daemon-lifecycle-architecture.md): its idle stop and its
connection policy.

## Why

Before this design, daemons ran for days after their last use:

* **Idle shutdown counted connections.** The idle watcher refused to stop while any client was
  connected (`idle_shutdown_blocker_with_client_allowance(0)`). An open TUI holds several
  connections (a session stream, pooled request sockets, activity watches, followed sessions) for
  its whole life, so a TUI left open in a background tab pinned its build's daemon indefinitely.
* **Clients kept the connection busy while idle.** The TUI polled submission-queue status every
  30 seconds, and every stream had a reconnect loop that never gave up. Because the TUI is an
  auto-start client, each reconnect could start a replacement daemon.
* **Disconnected clients were not always released.** The daemon cancels only an allow-list of
  request types when a client disconnects; any other request still in flight kept the connection
  handler, and so the client registration, alive after the client process was gone. The
  activity-watch pull is such a request: a long poll that may wait indefinitely. Long polls now
  end when their client disconnects or the daemon stops, and a client that disconnects during
  any other request stops counting as connected at once.
* **Released native sessions kept their harness.** A managed native session (a Claude Code
  harness process) could outlive the release of its session, and the daemon's native-activity
  tracker kept counting it as live work.

## Idle is about work

A daemon is idle when it has no work. Work is:

* running or queued model turns and queued session commands;
* active runtime work items;
* pending interactive work (permission prompts and other recovered interaction owners);
* session migrations in progress;
* managed native sessions with live work (a running turn, unsettled background work) for a
  session the daemon still has loaded;
* running plugin tasks;
* nonterminal workflow runs, including runs waiting on a retry schedule;
* active X-ray observation.

Connected clients are not work. A connection says that a client might want something, not that
the daemon is doing anything.

When the daemon has been idle for `daemon.idle_shutdown_after_secs` (default 900), it reserves
shutdown admission (no new work is admitted until it commits or rolls back), checks the blockers
again, then announces `DaemonEvent::Shutdown { reason: Idle }` to every connected client and
exits. A request that arrives during the reservation is refused before it has any effect, with
`daemon_stopping`; the client sends it again, to the new daemon or to this one if it stayed.

## Dormant clients

`bcode_client` tracks the daemon's **presence**, shared by every clone of a client: *live*, or
*dormant* after the daemon said it stopped. Frontends do not poll, reconnect or start daemons
themselves; the client does it for them:

* **The announcement** (`Event::DaemonShutdown`) on any connection makes the client dormant. On a
  stream it surfaces as `ClientError::DaemonStopped`, never as an event to deliver.
* **Streams wait.** `ClientConnection::recv_event` (and the watchers built on it: sessions,
  runtime work, workflow runs, catalog) waits while dormant, sending nothing, then reconnects,
  restores its subscriptions and attachment, and delivers a resync. Frontend loops that receive
  without reconnecting wait on `BcodeClient::daemon_live` instead of retrying.
* **Real requests start it.** A request goes through `connect`, which starts this build's daemon
  (from its pinned image) when none is running, and makes the client live again. A request the
  daemon refused because it was stopping (`daemon_stopping`), or met by the announcement instead
  of an answer, was never handled, so it is sent again to the new daemon.
* **Another process wakes it too.** While dormant, `daemon_live` watches the daemon's endpoint
  (its socket file) with filesystem notifications, so a daemon of this build started by another
  process wakes the waiting streams. No timer is involved.
* **Unannounced loss** (a crash) is replaced as before, with a bounded number of attempts.

Idle clients send nothing: the TUI's periodic queue-status poll is gone (queue status follows the
events that change it), telemetry is dropped while dormant, viewport inputs (which go to running
tools) are not sent, and pooled request sockets close after a short idle period.

## Following a session another build runs (option A)

A daemon of this client's build is noticed through its endpoint (above). A session run by
*another* build's daemon is not, and that daemon's session database cannot be read either:
opening it takes an exclusive file lock, and multi-process access is off on purpose. So the
owner publishes, and other builds follow what it publishes (`bcode_session_feed`):

* `sessions/feed/<id>/marker.json`, the **activity marker**: the owner's identity (build,
  artifact, protocol, storage epoch, instance, process), whether it owns the session now, whether
  a model turn or runtime work is running, a generation, and the feed and event schema versions.
  It is rewritten atomically when work starts or stops and when the owner takes or gives up the
  session. It lives beside the lease directory, not in it, because lease maintenance prunes the
  records there.
* `sessions/feed/<id>/events.jsonl`, the **session feed**: one record per committed event,
  appended after the commit, each with a SHA-256 checksum of the event's JSON, so a torn or
  damaged line is never trusted (it reads as a gap). A gap record marks events the owner could not
  publish (it lagged behind its commits). Past 8 MiB the file starts over with a gap record.

The daemon publishes every session it commits events for (`session_feed_publisher`). Readers
follow with filesystem notifications, no timers:

* `BcodeClient::session_owner` reads the marker: no live owner (none, given up, or its process is
  gone), a daemon of this build, another build, or a newer build whose format this one cannot read.
* A TUI whose daemon goes dormant while it shows a session follows that session's feed until its
  own daemon is live again: events committed elsewhere (by any build) appear as they come, past
  the last one the view had. Another build's owner shows "Running in another Bcode build"; a newer
  format, "Updated by a newer Bcode build; open it with the current build"; a gap, that the view is
  incomplete until reopened. Once its daemon is back, the view resynchronizes from it as usual.
* The feed carries committed history only (whole turns, not live text), and it is a published
  view, not the session's history: it may rotate or be lost, and it never affects the database.

## Option B, later: a cross-build observation protocol

Option B adds live text across builds: a small, versioned, read-only observation protocol that
every daemon serves to clients of any build, negotiated by version. It is designed in now so it
needs no reshaping:

* The activity marker names the owner's protocol version and instance; an observation endpoint
  is one more field in its versioned format.
* Followers already merge a second source of committed events into a view by sequence, and
  updates are versioned; live deltas are one more kind of update.

The exception to "never contact another build's daemon" is only ever read-only observation.
Writing to a session or owning its runtime stays same-build.

## Builds that meet each other's data

* **An old client acts on a session a newer build migrated forward:** the old build cannot read
  it; the client says so and offers to open it with the current build.
* **An old client acts on a session a newer build's daemon is running:** it cannot write until
  that run ends and the lease is released; meanwhile it follows the feed.
* **Old clients keep working on their own sessions.** A client pins its daemon image (a shared
  lock that cleanup respects) and can rebuild it from its retained executable handle, so an old
  tab restarts its own daemon whenever it does real work.

## Open: managed native sessions

A daemon was seen held for hours by its native-activity tracker ("managed session with live
native work") after its clients left. The tracker clears only on native terminal or settlement
evidence, and that evidence is ingested by an observation watch that runs only while a client is
attached to the session (it is admitted with that client's runtime context, including its
credentials). When the last attached client leaves mid-work, nothing ingests the evidence that the
work ended, so the session stays "busy" and pins the daemon, and its history stops following the
native session until someone attaches again. Fixing this means ingesting without an attached
client: either keeping the departed client's runtime context for that purpose, or giving the
daemon its own admission identity. That is a decision about credentials, still to be made.

## Diagnosability

The idle watcher's log names what blocks it, and since clients no longer block idleness, every
blocker it names is work. `bcode server status --verbose` lists each connected client: its name,
its process id (as the client reports it in its `Hello`), and how long it has been connected and
idle.

## Edge cases

| Case | Behavior |
|---|---|
| A request races idle shutdown | refused with `daemon_stopping` before any effect; the client starts a daemon and sends it again |
| The daemon crashes | Replaced with bounded attempts, then reported |
| A request races idle shutdown and the daemon stays (blockers changed) | the refusal is retried and the same daemon answers |
| A permission prompt waits on the user | work: the daemon stays |
| A workflow run waits for its retry | work: the daemon stays until the run settles |
| Several clients of one build | one client's work keeps the daemon; the others receive events while it is up |
| Two builds want to own one session | the lease decides; the other follows the feed |
| One-shot commands, review, plugin surfaces | same presence through `bcode_client` |
| Separate state or config locations | separate daemons, unaffected |
