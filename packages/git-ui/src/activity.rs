//! Running work, shown where it lands.
//!
//! Every queued operation records what it affects (branches, remotes, remote branches, tags),
//! and background loads record which view they fill. Views ask what is happening to the thing
//! they draw: a branch row shows `⠹ pushing 45%` while its push runs, `queued` while it waits,
//! and a brief `✓` or `✗` once it is done. The status bar summarizes the running operation.
//!
//! Local work shows only once it has run for the configured delay, so quick work never
//! flickers; network work shows at once. Automatic fetches show only in the status bar.

use std::collections::{BTreeMap, VecDeque};
use std::time::{Duration, Instant};

use bcode_git::models::{HeadState, Progress};
use bmux_tui::prelude::Span;
use bmux_tui::style::Style;
use bmux_tui_components::spinner::Spinner;

use crate::config::{ActivityConfig, SpinnerSetting};
use crate::data::Snapshot;
use crate::ops::Op;
use crate::theme::GitTheme;

/// Something drawn in a list that work can affect.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Subject {
    /// A local branch.
    Branch(String),
    /// A remote.
    Remote(String),
    /// A remote-tracking branch.
    RemoteBranch {
        /// Remote name.
        remote: String,
        /// Branch name on the remote.
        name: String,
    },
    /// A tag.
    Tag(String),
}

/// Background loading that fills a view.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Load {
    /// Panel data.
    Repository,
    /// The main view.
    Main,
    /// A commit's files.
    CommitFiles,
    /// A branch's commits.
    SubCommits,
    /// Older history.
    MoreCommits,
    /// Pull requests from the forge.
    PullRequests,
}

/// What an operation is doing, for display.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Description {
    /// Short verb for rows (`pushing`).
    pub verb: &'static str,
    /// Status-bar text (`Pushing main`).
    pub label: String,
    /// What it affects.
    pub subjects: Vec<Subject>,
}

#[derive(Debug, Clone)]
struct Task {
    description: Description,
    network: bool,
    automatic: bool,
    started: Option<Instant>,
}

#[derive(Debug, Clone)]
struct Outcome {
    subjects: Vec<Subject>,
    verb: &'static str,
    ok: bool,
    until: Instant,
}

/// What a row shows about work on its subject.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mark<'a> {
    /// Running: the verb and git's progress, when it reports some.
    Running {
        /// Short verb (`pushing`).
        verb: &'a str,
        /// Percent done.
        percent: Option<u8>,
    },
    /// Waiting behind other operations.
    Queued,
    /// Just finished: the verb, and whether it succeeded.
    Done(&'a str, bool),
}

/// Running, queued and just-finished work.
#[derive(Debug)]
pub struct Activity {
    epoch: Instant,
    spinner: Spinner,
    show_delay: Duration,
    result_for: Duration,
    tasks: VecDeque<Task>,
    loads: BTreeMap<Load, Instant>,
    outcomes: Vec<Outcome>,
    progress: Option<Progress>,
    /// Newest command-log record before the running operation started.
    records_before: Option<u64>,
}

impl Activity {
    /// Activity shown as `config` says, with diagnostics for unusable spinner settings.
    #[must_use]
    pub fn new(config: &ActivityConfig) -> (Self, Vec<String>) {
        let mut diagnostics = Vec::new();
        let interval = Duration::from_millis(config.rate_ms.max(1));
        let spinner = match &config.spinner {
            SpinnerSetting::Preset(name) => Spinner::named(name).unwrap_or_else(|| {
                diagnostics.push(format!(
                    "activity.spinner: unknown preset {name:?} (braille, dot, ascii); using braille"
                ));
                Spinner::braille()
            }),
            SpinnerSetting::Frames(frames) => Spinner::new(frames.iter().cloned(), interval)
                .unwrap_or_else(|| {
                    diagnostics.push("activity.spinner: no frames; using braille".into());
                    Spinner::braille()
                }),
        }
        .with_interval(interval);
        let spinner = if config.animate {
            spinner
        } else {
            Spinner::fixed("…")
        };
        (
            Self {
                epoch: Instant::now(),
                spinner,
                show_delay: Duration::from_millis(config.show_delay_ms),
                result_for: Duration::from_millis(config.result_ms),
                tasks: VecDeque::new(),
                loads: BTreeMap::new(),
                outcomes: Vec::new(),
                progress: None,
                records_before: None,
            },
            diagnostics,
        )
    }

    /// Queue an operation (`automatic` for work the user did not ask for, like auto-fetch).
    pub fn queue(&mut self, op: &Op, snapshot: &Snapshot, automatic: bool) {
        self.tasks.push_back(Task {
            description: describe(op, snapshot),
            network: op.is_network(),
            automatic,
            started: None,
        });
    }

    /// The next queued operation started, after command-log record `records_before`; its
    /// commands are the records after it.
    pub fn start(&mut self, records_before: Option<u64>) {
        self.records_before = records_before;
        if let Some(task) = self.tasks.iter_mut().find(|task| task.started.is_none()) {
            task.started = Some(Instant::now());
        }
        self.progress = None;
    }

    /// The running operation finished; what it affected shows the result for a moment.
    pub fn finish(&mut self, ok: bool) {
        let Some(position) = self.tasks.iter().position(|task| task.started.is_some()) else {
            return;
        };
        let Some(task) = self.tasks.remove(position) else {
            return;
        };
        self.progress = None;
        if !task.automatic && !task.description.subjects.is_empty() && !self.result_for.is_zero() {
            self.outcomes.push(Outcome {
                subjects: task.description.subjects,
                verb: task.description.verb,
                ok,
                until: Instant::now() + self.result_for,
            });
        }
    }

    /// Queued operations were dropped (after a failure).
    pub fn clear_queued(&mut self) {
        self.tasks.retain(|task| task.started.is_some());
    }

    /// A load began (an earlier one still running keeps its start).
    pub fn begin(&mut self, load: Load) {
        self.loads.entry(load).or_insert_with(Instant::now);
    }

    /// A load finished.
    pub fn end(&mut self, load: Load) {
        self.loads.remove(&load);
    }

    /// Whether command-log record `id` belongs to the running operation.
    #[must_use]
    pub fn owns_record(&self, id: u64) -> bool {
        self.running() && self.records_before.is_none_or(|before| id > before)
    }

    /// Git's latest progress for the running operation.
    pub fn set_progress(&mut self, progress: Option<Progress>) {
        self.progress = progress;
    }

    /// Whether an operation is running or queued.
    #[must_use]
    pub fn busy(&self) -> bool {
        !self.tasks.is_empty()
    }

    /// Whether an operation is running.
    #[must_use]
    pub fn running(&self) -> bool {
        self.tasks.iter().any(|task| task.started.is_some())
    }

    /// Whether anything may need animating or appear soon (redraw on every spinner frame).
    #[must_use]
    pub fn animating(&self) -> bool {
        !self.tasks.is_empty() || !self.loads.is_empty()
    }

    /// Drop result marks whose time is up. Returns whether any were dropped.
    pub fn expire(&mut self) -> bool {
        let now = Instant::now();
        let before = self.outcomes.len();
        self.outcomes.retain(|outcome| outcome.until > now);
        self.outcomes.len() != before
    }

    fn shown(&self, task: &Task, now: Instant) -> bool {
        task.started
            .is_some_and(|started| task.network || now - started >= self.show_delay)
    }

    fn percent(&self) -> Option<u8> {
        self.progress.as_ref().map(|progress| progress.percent)
    }

    /// What a row for `subject` shows, if anything.
    #[must_use]
    pub fn mark(&self, subject: &Subject) -> Option<Mark<'_>> {
        let now = Instant::now();
        let mut queued = false;
        for task in &self.tasks {
            if task.automatic || !task.description.subjects.contains(subject) {
                continue;
            }
            if task.started.is_none() {
                queued = true;
            } else if self.shown(task, now) {
                return Some(Mark::Running {
                    verb: task.description.verb,
                    percent: self.percent(),
                });
            }
        }
        if queued {
            return Some(Mark::Queued);
        }
        self.outcomes
            .iter()
            .rev()
            .find(|outcome| outcome.until > now && outcome.subjects.contains(subject))
            .map(|outcome| Mark::Done(outcome.verb, outcome.ok))
    }

    /// Spans for a row's mark, led by a space.
    #[must_use]
    pub fn row_spans(&self, subject: &Subject, theme: &GitTheme) -> Vec<Span> {
        match self.mark(subject) {
            Some(Mark::Running { verb, percent }) => {
                let mut spans = vec![
                    Span::raw(" "),
                    self.spinner_span(theme.activity),
                    Span::styled(format!(" {verb}"), theme.activity),
                ];
                if let Some(percent) = percent {
                    spans.push(Span::styled(format!(" {percent}%"), theme.activity));
                }
                spans
            }
            Some(Mark::Queued) => vec![Span::styled(" queued", theme.muted)],
            Some(Mark::Done(verb, true)) => {
                vec![Span::styled(format!(" ✓ {}", past(verb)), theme.success)]
            }
            Some(Mark::Done(_, false)) => vec![Span::styled(" ✗ failed", theme.error)],
            None => Vec::new(),
        }
    }

    /// Whether `load` has run long enough to show.
    #[must_use]
    pub fn loading(&self, load: Load) -> bool {
        self.loads
            .get(&load)
            .is_some_and(|started| started.elapsed() >= self.show_delay)
    }

    /// The current spinner frame, styled.
    #[must_use]
    pub fn spinner_span(&self, style: Style) -> Span {
        self.spinner.span(self.epoch.elapsed(), style)
    }

    /// Time each spinner frame shows.
    #[must_use]
    pub const fn interval(&self) -> Duration {
        self.spinner.interval()
    }

    /// Status-bar spans for the running operation: spinner, label, progress, elapsed time
    /// and how many wait behind it. Automatic work is muted.
    #[must_use]
    pub fn status_spans(&self, theme: &GitTheme) -> Option<Vec<Span>> {
        let now = Instant::now();
        let task = self.tasks.iter().find(|task| task.started.is_some())?;
        let started = task.started?;
        if !self.shown(task, now) {
            return None;
        }
        let style = if task.automatic {
            theme.muted
        } else {
            theme.activity
        };
        let mut spans = vec![
            self.spinner_span(style),
            Span::styled(format!(" {}", task.description.label), style),
        ];
        if let Some(progress) = &self.progress {
            spans.push(Span::styled(
                format!(" · {} {}%", progress.phase, progress.percent),
                if task.automatic {
                    theme.muted
                } else {
                    theme.text
                },
            ));
        }
        let elapsed = now - started;
        if elapsed >= Duration::from_secs(1) {
            spans.push(Span::styled(
                format!(" · {}s", elapsed.as_secs()),
                theme.muted,
            ));
        }
        let queued = self
            .tasks
            .iter()
            .filter(|task| task.started.is_none())
            .count();
        if queued > 0 {
            spans.push(Span::styled(format!("  +{queued} queued"), theme.muted));
        }
        spans.push(Span::raw(" "));
        Some(spans)
    }
}

/// The past tense of a row verb (`pushing` → `pushed`).
fn past(verb: &str) -> String {
    match verb {
        "working" => "done".to_owned(),
        "checking out" => "checked out".to_owned(),
        "merging" => "merged".to_owned(),
        "rebasing" => "rebased".to_owned(),
        "removing" => "removed".to_owned(),
        "renaming" => "renamed".to_owned(),
        "updating" => "updated".to_owned(),
        "deleting" => "deleted".to_owned(),
        other => other
            .strip_suffix("ing")
            .map_or_else(|| other.to_owned(), |stem| format!("{stem}ed")),
    }
}

fn current_branch(snapshot: &Snapshot) -> Option<String> {
    match &snapshot.head {
        Some(HeadState::Branch { name } | HeadState::Unborn { name }) => Some(name.clone()),
        _ => None,
    }
}

/// The local branch a push refspec pushes (`+src:dst` → `src`).
fn pushed_branch(refspec: &str) -> &str {
    let source = refspec.trim_start_matches('+');
    let source = source.split_once(':').map_or(source, |(source, _)| source);
    source.strip_prefix("refs/heads/").unwrap_or(source)
}

fn upstream_remote<'a>(snapshot: &'a Snapshot, branch: &str) -> Option<&'a str> {
    let upstream = snapshot
        .branches
        .iter()
        .find(|candidate| candidate.name == branch)?
        .upstream
        .as_deref()?;
    snapshot
        .remotes
        .iter()
        .map(|remote| remote.name.as_str())
        .find(|remote| {
            upstream
                .strip_prefix(remote)
                .is_some_and(|rest| rest.starts_with('/'))
        })
}

/// Branches tracking a branch of `remote` (any remote when `None`).
fn tracking(snapshot: &Snapshot, remote: Option<&str>) -> Vec<Subject> {
    snapshot
        .branches
        .iter()
        .filter(|branch| {
            branch.upstream.is_some()
                && remote
                    .is_none_or(|remote| upstream_remote(snapshot, &branch.name) == Some(remote))
        })
        .map(|branch| Subject::Branch(branch.name.clone()))
        .collect()
}

/// What a push does: the pushed branch, and the remote and remote branch it updates.
fn describe_push(
    options: &bcode_git::PushOptions,
    snapshot: &Snapshot,
    current: Option<&str>,
) -> (&'static str, String, Vec<Subject>) {
    let name = options
        .refspec
        .as_deref()
        .map(pushed_branch)
        .or(current)
        .map(str::to_owned);
    let mut subjects = Vec::new();
    if let Some(name) = &name {
        subjects.push(Subject::Branch(name.clone()));
        if let Some(remote) = options
            .remote
            .as_deref()
            .or_else(|| upstream_remote(snapshot, name))
        {
            subjects.push(Subject::Remote(remote.to_owned()));
            subjects.push(Subject::RemoteBranch {
                remote: remote.to_owned(),
                name: name.clone(),
            });
        }
    }
    let force = options.force || options.force_with_lease;
    let (verb, label) = if force {
        ("force pushing", "Force pushing")
    } else {
        ("pushing", "Pushing")
    };
    (
        verb,
        format!("{label} {}", name.as_deref().unwrap_or("HEAD")),
        subjects,
    )
}

/// What `op` does and affects, judged against the repository as last loaded.
#[must_use]
pub fn describe(op: &Op, snapshot: &Snapshot) -> Description {
    let current = current_branch(snapshot);
    let branch = |name: &str| vec![Subject::Branch(name.to_owned())];
    let current_subject = || {
        current
            .iter()
            .map(|name| Subject::Branch(name.clone()))
            .collect()
    };
    let (verb, label, subjects) = match op {
        Op::Push(options) => describe_push(options, snapshot, current.as_deref()),
        Op::Pull { .. } => (
            "pulling",
            format!("Pulling {}", current.as_deref().unwrap_or("HEAD")),
            current_subject(),
        ),
        Op::FastForward(target) => (
            "fast-forwarding",
            format!("Fast-forwarding {}", target.name),
            branch(&target.name),
        ),
        Op::Fetch(Some(remote)) => {
            let mut subjects = vec![Subject::Remote(remote.clone())];
            subjects.extend(tracking(snapshot, Some(remote)));
            ("fetching", format!("Fetching {remote}"), subjects)
        }
        Op::Fetch(None) => {
            let mut subjects: Vec<Subject> = snapshot
                .remotes
                .iter()
                .map(|remote| Subject::Remote(remote.name.clone()))
                .collect();
            subjects.extend(tracking(snapshot, None));
            ("fetching", "Fetching".to_owned(), subjects)
        }
        Op::DeleteRemoteBranch { remote, name } => (
            "deleting",
            format!("Deleting {remote}/{name}"),
            vec![Subject::RemoteBranch {
                remote: remote.clone(),
                name: name.clone(),
            }],
        ),
        Op::PushTag { remote, name } => (
            "pushing",
            format!("Pushing tag {name} to {remote}"),
            vec![Subject::Tag(name.clone()), Subject::Remote(remote.clone())],
        ),
        Op::Checkout(target) => (
            "checking out",
            format!("Checking out {target}"),
            branch(target),
        ),
        Op::CheckoutRemote(target) => (
            "checking out",
            format!("Checking out {}", target.name),
            vec![Subject::RemoteBranch {
                remote: target.remote.clone(),
                name: target.name.clone(),
            }],
        ),
        Op::DeleteBranch { name, .. } => ("deleting", format!("Deleting {name}"), branch(name)),
        Op::RenameBranch { old, new } => {
            ("renaming", format!("Renaming {old} to {new}"), branch(old))
        }
        Op::SetUpstream { branch: name, .. } => (
            "updating",
            format!("Setting upstream of {name}"),
            branch(name),
        ),
        Op::Merge { target, .. } => ("merging", format!("Merging {target}"), current_subject()),
        Op::Rebase(target) => (
            "rebasing",
            format!("Rebasing onto {target}"),
            current_subject(),
        ),
        Op::RemoveRemote(name) => (
            "removing",
            format!("Removing remote {name}"),
            vec![Subject::Remote(name.clone())],
        ),
        Op::SetRemoteUrl { name, .. } => (
            "updating",
            format!("Updating remote {name}"),
            vec![Subject::Remote(name.clone())],
        ),
        Op::DeleteTag(name) => (
            "deleting",
            format!("Deleting tag {name}"),
            vec![Subject::Tag(name.clone())],
        ),
        other => ("working", other.label(), Vec::new()),
    };
    Description {
        verb,
        label,
        subjects,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bcode_git::PushOptions;
    use bcode_git::models::{Branch, Remote};

    fn snapshot() -> Snapshot {
        let branch = |name: &str, upstream: Option<&str>| Branch {
            name: name.into(),
            oid: String::new(),
            is_head: name == "main",
            upstream: upstream.map(str::to_owned),
            ahead: 0,
            behind: 0,
            upstream_gone: false,
            commit_time: 0,
            summary: String::new(),
            worktree: None,
        };
        let remote = |name: &str| Remote {
            name: name.into(),
            fetch_url: None,
            push_url: None,
        };
        Snapshot {
            head: Some(HeadState::Branch {
                name: "main".into(),
            }),
            branches: vec![
                branch("main", Some("origin/main")),
                branch("fork-work", Some("fork/work")),
                branch("local", None),
            ],
            remotes: vec![remote("origin"), remote("fork")],
            ..Snapshot::default()
        }
    }

    #[test]
    fn pushes_affect_the_pushed_branch_and_its_remote() {
        let description = describe(&Op::Push(PushOptions::default()), &snapshot());
        assert_eq!(description.label, "Pushing main");
        assert_eq!(
            description.subjects,
            vec![
                Subject::Branch("main".into()),
                Subject::Remote("origin".into()),
                Subject::RemoteBranch {
                    remote: "origin".into(),
                    name: "main".into()
                },
            ]
        );
        let explicit = describe(
            &Op::Push(PushOptions {
                remote: Some("fork".into()),
                refspec: Some("+refs/heads/local:other".into()),
                force: true,
                ..PushOptions::default()
            }),
            &snapshot(),
        );
        assert_eq!(explicit.verb, "force pushing");
        assert_eq!(explicit.subjects[0], Subject::Branch("local".into()));
        assert_eq!(explicit.subjects[1], Subject::Remote("fork".into()));
    }

    #[test]
    fn fetching_a_remote_affects_the_branches_tracking_it() {
        let description = describe(&Op::Fetch(Some("fork".into())), &snapshot());
        assert_eq!(
            description.subjects,
            vec![
                Subject::Remote("fork".into()),
                Subject::Branch("fork-work".into()),
            ]
        );
        let all = describe(&Op::Fetch(None), &snapshot());
        assert_eq!(all.subjects.len(), 4);
        assert!(!all.subjects.contains(&Subject::Branch("local".into())));
    }

    fn config() -> ActivityConfig {
        ActivityConfig {
            show_delay_ms: 60_000,
            ..ActivityConfig::default()
        }
    }

    #[test]
    fn rows_show_running_network_work_at_once_and_local_work_after_the_delay() {
        let (mut activity, diagnostics) = Activity::new(&config());
        assert_eq!(diagnostics, Vec::<String>::new());
        let snapshot = snapshot();
        let main = Subject::Branch("main".into());
        activity.queue(&Op::Push(PushOptions::default()), &snapshot, false);
        activity.queue(&Op::Checkout("local".into()), &snapshot, false);
        assert_eq!(activity.mark(&main), Some(Mark::Queued));
        activity.start(None);
        activity.set_progress(Some(Progress {
            phase: "Writing objects".into(),
            percent: 45,
        }));
        assert_eq!(
            activity.mark(&main),
            Some(Mark::Running {
                verb: "pushing",
                percent: Some(45)
            })
        );
        let local = Subject::Branch("local".into());
        assert_eq!(activity.mark(&local), Some(Mark::Queued));
        activity.finish(true);
        assert_eq!(activity.mark(&main), Some(Mark::Done("pushing", true)));
        activity.start(None);
        // A local checkout within the delay shows nothing yet.
        assert_eq!(activity.mark(&local), None);
        activity.finish(false);
        assert_eq!(
            activity.mark(&local),
            Some(Mark::Done("checking out", false))
        );
        assert!(!activity.busy());
    }

    #[test]
    fn automatic_work_and_cleared_queues_leave_rows_alone() {
        let (mut activity, _) = Activity::new(&config());
        let snapshot = snapshot();
        let main = Subject::Branch("main".into());
        activity.queue(&Op::Fetch(None), &snapshot, true);
        activity.start(None);
        assert_eq!(activity.mark(&main), None);
        activity.finish(true);
        assert_eq!(activity.mark(&main), None);
        activity.queue(&Op::Pull { rebase: None }, &snapshot, false);
        activity.queue(&Op::Push(PushOptions::default()), &snapshot, false);
        activity.start(None);
        activity.clear_queued();
        activity.finish(false);
        assert!(!activity.busy());
    }

    #[test]
    fn finished_verbs_read_in_the_past_tense() {
        for (verb, done) in [
            ("pushing", "pushed"),
            ("force pushing", "force pushed"),
            ("fetching", "fetched"),
            ("pulling", "pulled"),
            ("fast-forwarding", "fast-forwarded"),
            ("checking out", "checked out"),
            ("deleting", "deleted"),
            ("working", "done"),
        ] {
            assert_eq!(past(verb), done);
        }
    }

    #[test]
    fn spinner_settings_are_checked() {
        let (_, diagnostics) = Activity::new(&ActivityConfig {
            spinner: SpinnerSetting::Preset("sparkles".into()),
            ..ActivityConfig::default()
        });
        assert_eq!(diagnostics.len(), 1);
        let (_, diagnostics) = Activity::new(&ActivityConfig {
            spinner: SpinnerSetting::Frames(Vec::new()),
            ..ActivityConfig::default()
        });
        assert_eq!(diagnostics.len(), 1);
    }
}
