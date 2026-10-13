//! Opt-in, bounded observations for the fixed-artifact fan-out investigation.
use std::{
    path::{Path, PathBuf},
    sync::{Mutex, OnceLock},
    time::Duration,
};

const MAX_OBSERVATIONS: usize = 4096;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FanoutAdmissionStage {
    AdmissionLock,
    DriverPrepare,
    OwnerPlan,
    ExecutionAuthority,
    CheckOwner,
    RunStatus,
    ChildCreate,
    ChildLookup,
    SessionLink,
    Submit,
    OwnerPersistence,
    Catalog,
    RepositoryDiscovery,
    DynamicContext,
    SystemPrompt,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FanoutRepositoryProbe {
    DiscoverRoot,
    Branch,
    Status,
    Revision,
    Upstream,
    Worktree,
    ProjectInstructions,
    RepositoryInvariants,
    Other,
}

#[derive(Clone, Debug)]
pub enum Observation {
    Stage {
        session_id: Option<crate::SessionId>,
        dispatch_identity: Option<String>,
        stage: FanoutAdmissionStage,
        elapsed: Duration,
    },
    ChildCreation(bcode_session::child_creation_observation::ChildCreationObservation),
    RepositoryProbe {
        probe: FanoutRepositoryProbe,
        elapsed: Duration,
        completed: bool,
    },
}

#[derive(Debug)]
pub struct Snapshot {
    pub(crate) experiment: String,
    pub(crate) observations: Vec<Observation>,
    pub(crate) dropped: usize,
}

struct Active {
    run_id: String,
    workspace: PathBuf,
    snapshot: Snapshot,
}

fn collector() -> &'static Mutex<Option<Active>> {
    static COLLECTOR: OnceLock<Mutex<Option<Active>>> = OnceLock::new();
    COLLECTOR.get_or_init(|| Mutex::new(None))
}

pub struct Experiment {
    id: String,
}

impl Experiment {
    pub(crate) fn begin(id: &str, run_id: &str, workspace: &Path) -> Self {
        let workspace = workspace.canonicalize().expect("experiment workspace");
        let mut active = collector().lock().expect("collector");
        assert!(active.is_none(), "fanout probes must run serially");
        *active = Some(Active {
            run_id: run_id.into(),
            workspace,
            snapshot: Snapshot {
                experiment: id.into(),
                observations: Vec::new(),
                dropped: 0,
            },
        });
        drop(active);
        Self { id: id.into() }
    }

    pub(crate) fn snapshot(&self) -> Snapshot {
        let guard = collector().lock().expect("collector");
        let active = guard.as_ref().expect("active experiment");
        assert_eq!(active.snapshot.experiment, self.id);
        let snapshot = Snapshot {
            experiment: self.id.clone(),
            observations: active.snapshot.observations.clone(),
            dropped: active.snapshot.dropped,
        };
        drop(guard);
        snapshot
    }
}

impl Drop for Experiment {
    fn drop(&mut self) {
        let mut active = collector()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if active
            .as_ref()
            .is_some_and(|active| active.snapshot.experiment == self.id)
        {
            *active = None;
        }
    }
}

fn append(active: &mut Active, observation: Observation) {
    if active.snapshot.observations.len() < MAX_OBSERVATIONS {
        active.snapshot.observations.push(observation);
    } else {
        active.snapshot.dropped = active.snapshot.dropped.saturating_add(1);
    }
}

pub fn child_creation_observer(
    run_id: &str,
) -> bcode_session::child_creation_observation::ChildCreationObserver {
    let run_id = run_id.to_owned();
    std::sync::Arc::new(move |observation| {
        let mut active = collector()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(active) = active.as_mut().filter(|active| active.run_id == run_id) {
            append(active, Observation::ChildCreation(observation));
        }
    })
}

pub struct StageTimer {
    run_id: String,
    session_id: Option<crate::SessionId>,
    dispatch_identity: Option<String>,
    stage: FanoutAdmissionStage,
    started: std::time::Instant,
}

impl StageTimer {
    pub(crate) fn new(
        run_id: &str,
        session_id: Option<crate::SessionId>,
        dispatch_identity: Option<&str>,
        stage: FanoutAdmissionStage,
    ) -> Self {
        Self {
            run_id: run_id.into(),
            session_id,
            dispatch_identity: dispatch_identity.map(str::to_owned),
            stage,
            started: std::time::Instant::now(),
        }
    }
}

impl Drop for StageTimer {
    fn drop(&mut self) {
        record_fanout_stage(
            &self.run_id,
            self.session_id,
            self.dispatch_identity.as_deref(),
            self.stage,
            self.started.elapsed(),
        );
    }
}

pub struct RepositoryTimer {
    cwd: PathBuf,
    probe: FanoutRepositoryProbe,
    started: std::time::Instant,
    pub(crate) completed: bool,
}

impl RepositoryTimer {
    pub(crate) fn for_probe(cwd: &Path, probe: FanoutRepositoryProbe) -> Self {
        Self {
            cwd: cwd.into(),
            probe,
            started: std::time::Instant::now(),
            completed: false,
        }
    }

    pub(crate) fn new(cwd: &Path, args: &[&str]) -> Self {
        let probe = match args.first().copied() {
            Some("branch") => FanoutRepositoryProbe::Branch,
            Some("status") => FanoutRepositoryProbe::Status,
            Some("log") => FanoutRepositoryProbe::Revision,
            Some("rev-list") => FanoutRepositoryProbe::Upstream,
            Some("worktree") => FanoutRepositoryProbe::Worktree,
            Some("rev-parse") => FanoutRepositoryProbe::DiscoverRoot,
            _ => FanoutRepositoryProbe::Other,
        };
        Self {
            cwd: cwd.into(),
            probe,
            started: std::time::Instant::now(),
            completed: false,
        }
    }
}

impl Drop for RepositoryTimer {
    fn drop(&mut self) {
        record_fanout_repository_probe(
            &self.cwd,
            self.probe,
            self.started.elapsed(),
            self.completed,
        );
    }
}

pub fn record_fanout_stage(
    run_id: &str,
    session_id: Option<crate::SessionId>,
    dispatch_identity: Option<&str>,
    stage: FanoutAdmissionStage,
    elapsed: Duration,
) {
    let mut active = collector()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(active) = active.as_mut().filter(|active| active.run_id == run_id) {
        append(
            active,
            Observation::Stage {
                session_id,
                dispatch_identity: dispatch_identity.map(|id| id.chars().take(256).collect()),
                stage,
                elapsed,
            },
        );
    }
}

pub fn record_fanout_repository_probe(
    cwd: &Path,
    probe: FanoutRepositoryProbe,
    elapsed: Duration,
    completed: bool,
) {
    let mut active = collector()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    // Callers pass the already-resolved workspace; observation adds no filesystem probes.
    if let Some(active) = active.as_mut().filter(|active| active.workspace == cwd) {
        append(
            active,
            Observation::RepositoryProbe {
                probe,
                elapsed,
                completed,
            },
        );
    }
}

#[test]
fn collector_is_scoped_bounded_and_released() {
    let root = tempfile::tempdir().unwrap();
    let experiment = Experiment::begin("collector-unit", "collector-run", root.path());
    record_fanout_stage(
        "foreign-run",
        None,
        None,
        FanoutAdmissionStage::Submit,
        Duration::ZERO,
    );
    for _ in 0..=MAX_OBSERVATIONS {
        record_fanout_stage(
            "collector-run",
            None,
            None,
            FanoutAdmissionStage::Submit,
            Duration::ZERO,
        );
    }
    let snapshot = experiment.snapshot();
    assert_eq!(snapshot.observations.len(), MAX_OBSERVATIONS);
    assert_eq!(snapshot.dropped, 1);
    drop(experiment);
    let next = Experiment::begin("collector-next", "collector-run", root.path());
    assert!(next.snapshot().observations.is_empty());
}
