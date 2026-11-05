//! Opt-in offline acceptance of the actual eval CLI and configured production policy.
//! Requires a coordinator-built matching client/plugin image; never builds or uses
//! ambient daemon/configuration. Run explicitly with --ignored after selecting hashes.
#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};
use std::{
    fs::{self, File},
    io::Read as _,
    path::{Path, PathBuf},
    process::{Child, Command, ExitStatus, Stdio},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

const SUITE: &str = include_str!("data/xray-workflow/suite.toml");
const CONFIG: &str = include_str!("data/xray-workflow/config.toml");

struct Subject {
    binary: PathBuf,
    sha256: String,
    evidence: PathBuf,
    plugins: Option<PathBuf>,
}
impl Subject {
    fn selected() -> Self {
        let binary = PathBuf::from(
            std::env::var_os("BCODE_XRAY_EVAL_BINARY").expect("select explicit binary"),
        );
        let sha256 = std::env::var("BCODE_XRAY_EVAL_SHA256").expect("select binary SHA-256");
        let evidence = PathBuf::from(
            std::env::var_os("BCODE_XRAY_ACCEPTANCE_ARTIFACTS")
                .expect("select private evidence root"),
        );
        let plugins = std::env::var_os("BCODE_XRAY_EVAL_PLUGIN_ROOT").map(PathBuf::from);
        if let Some(root) = &plugins {
            assert!(
                root.is_absolute() && root.is_dir(),
                "explicit plugin root must be an absolute existing directory"
            );
        }
        assert!(binary.is_absolute() && evidence.is_absolute());
        assert!(evidence.is_dir());
        let mut file = File::open(&binary).unwrap();
        let mut digest = Sha256::new();
        let mut buffer = vec![0; 65536];
        loop {
            let length = file.read(&mut buffer).unwrap();
            if length == 0 {
                break;
            }
            digest.update(&buffer[..length]);
        }
        assert_eq!(
            hex::encode(digest.finalize()),
            sha256,
            "reject wrong artifact before invoking it"
        );
        Self {
            binary,
            sha256,
            evidence,
            plugins,
        }
    }
}

struct Fixture {
    root: PathBuf,
    binary: PathBuf,
    namespace: String,
    state_id: String,
    root_run_id: String,
}
impl Fixture {
    fn new(subject: &Subject, label: &str) -> Self {
        let root = tempfile::Builder::new()
            .prefix(&format!("eval-{label}-"))
            .tempdir_in(&subject.evidence)
            .unwrap()
            .keep();
        for directory in ["home", "state", "xdg/bcode", "workspace"] {
            fs::create_dir_all(root.join(directory)).unwrap();
        }
        fs::write(
            root.join("workspace/value.txt"),
            include_bytes!("data/xray-workflow/workspace/value.txt"),
        )
        .unwrap();
        // Optional dynamic image uses normal documented discovery. Static images
        // need no external plugin root; both modes must pass CLI preflight below.
        // Never inherit the caller's cwd/HOME/XDG plugin roots.
        if let Some(plugins) = &subject.plugins {
            #[cfg(unix)]
            std::os::unix::fs::symlink(plugins, root.join("xdg/bcode/plugins")).unwrap();
            #[cfg(not(unix))]
            panic!(
                "dynamic native plugin fixture requires Unix: {}",
                plugins.display()
            );
        }
        fs::write(root.join("config.toml"), CONFIG).unwrap();
        let state_id = bcode_config::runtime_scope_id(&root.join("state"), &root.join("xdg/bcode"));
        Self {
            root_run_id: format!("xray-eval-{state_id}"),
            state_id,
            root,
            binary: subject.binary.clone(),
            namespace: String::new(),
        }
    }

    fn identify(&mut self, subject: &Subject) {
        let status = self.invoke("identity", &["artifact-id"], Duration::from_secs(15));
        assert!(status.success());
        let artifact = fs::read_to_string(self.root.join("identity.stdout")).unwrap();
        let artifact = bcode_ipc::ArtifactId::parse(artifact.trim()).unwrap();
        self.namespace = bcode_ipc::daemon_namespace_for(&artifact);
        fs::write(self.root.join("subject.json"), serde_json::to_vec_pretty(&json!({
            "binary": subject.binary, "sha256": subject.sha256, "artifact_id": artifact.to_string(),
            "namespace": self.namespace, "state_location_id": self.state_id,
            "root_run_id": self.root_run_id, "plugin_root": subject.plugins,
        })).unwrap()).unwrap();
    }

    fn command(&self) -> Command {
        let mut command = Command::new(&self.binary);
        command
            .env_clear()
            .current_dir(&self.root)
            .env("PATH", "/usr/bin:/bin:/usr/sbin:/sbin")
            .env("HOME", self.root.join("home"))
            .env("XDG_CONFIG_HOME", self.root.join("xdg"))
            .env("BCODE_CONFIG", self.root.join("config.toml"))
            .env("BCODE_STARTUP_TRACE", "1")
            // BCODE_LOG overrides the startup-trace default. Keep these fixed,
            // secret-safe targets identical for every case; never enable payload logs.
            .env("BCODE_LOG", "off,bcode_client::startup=debug,bcode_server::startup=debug,bcode_plugin::startup=debug,bcode_daemon_lifecycle::startup=debug,bcode_server::xray_http=info")
            .env("BCODE_STATE_DIR", self.root.join("state"));
        command
    }

    fn invoke(&self, label: &str, args: &[&str], allowance: Duration) -> ExitStatus {
        let mut command = self.command();
        command.args(args);
        let mut child = spawn_logged(command, &self.root, label);
        let deadline = Instant::now() + allowance;
        loop {
            if let Some(status) = child.0.try_wait().unwrap() {
                fs::write(
                    self.root.join(format!("{label}.outcome.json")),
                    serde_json::to_vec_pretty(&json!({
                        "success": status.success(), "exit_code": status.code(),
                        "stdout": format!("{label}.stdout"), "stderr": format!("{label}.stderr"),
                    }))
                    .unwrap(),
                )
                .unwrap();
                return status;
            }
            if Instant::now() >= deadline {
                fs::write(
                    self.root.join(format!("{label}.outcome.json")),
                    serde_json::to_vec_pretty(&json!({
                        "success": false, "timed_out": true,
                        "effects_reconciled": false, "replayed": false,
                        "stdout": format!("{label}.stdout"),
                        "stderr": format!("{label}.stderr"),
                    }))
                    .unwrap(),
                )
                .unwrap();
                panic!(
                    "{label} timed out; private evidence: {}",
                    self.root.display()
                );
            }
            std::thread::sleep(Duration::from_millis(25));
        }
    }

    fn configure(&self, grant: Grant, deep: bool, expected: &str) {
        let mut suite: toml::Value = toml::from_str(SUITE).unwrap();
        suite["variants"][0]["metadata"]["xray_root_run_id"] =
            toml::Value::String(self.root_run_id.clone());
        suite["variants"][0]["metadata"]["workflow"]["expected_contains"] =
            toml::Value::Array(vec![toml::Value::String(expected.into())]);
        suite["variants"][0]["metadata"]["workflow"]["binary"] =
            toml::Value::String(self.binary.to_string_lossy().into_owned());
        fs::write(
            self.root.join("suite.toml"),
            toml::to_string(&suite).unwrap(),
        )
        .unwrap();
        let mut config: toml::Value = toml::from_str(CONFIG).unwrap();
        if !matches!(grant, Grant::Absent) {
            let now = u64::try_from(
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_millis(),
            )
            .unwrap();
            let policy = json!({"version":1,"grants":[{
                "actor_kind":"local_client", "root_run_id": if matches!(grant, Grant::ForeignRoot) { "foreign-root" } else { &self.root_run_id },
                "namespace":if matches!(grant, Grant::ForeignNamespace) { "foreign-namespace" } else { &self.namespace },
                "state_location_id":if matches!(grant, Grant::ForeignState) { "foreign-state" } else { &self.state_id },
                "recursive":!matches!(grant, Grant::NonRecursive), "deep_capture":deep,
                "expires_at_unix_ms": if matches!(grant, Grant::Expired) { now - 1 } else { now + 300_000 },
                "operations": if deep { vec!["observe_execution", "read_execution_observation", "finalize_execution_observation", "export_execution_observation"] }
                    else { vec!["observe_execution", "read_execution_observation", "finalize_execution_observation"] },
            }]});
            config["plugins"].as_table_mut().unwrap().insert(
                "config".into(),
                toml::Value::Table(toml::map::Map::from_iter([(
                    "bcode.workflow".into(),
                    toml::Value::try_from(json!({"execution_observation_policy":policy})).unwrap(),
                )])),
            );
        }
        fs::write(
            self.root.join("config.toml"),
            toml::to_string(&config).unwrap(),
        )
        .unwrap();
        assert!(
            self.invoke("eval-help", &["eval", "--help"], Duration::from_secs(15))
                .success(),
            "selected image lacks usable eval CLI; inspect {} before any execution/cleanup attempt",
            self.root.display()
        );
        assert!(
            self.invoke("goal-help", &["goal", "--help"], Duration::from_secs(15))
                .success(),
            "selected image lacks usable goal CLI; inspect {}",
            self.root.display()
        );
    }

    fn eval(&self, deep: bool, observed: bool) -> ExitStatus {
        let mut args = vec![
            "eval",
            "run",
            "suite.toml",
            "--output-root",
            "runs",
            "--run-id",
            "capture",
            "--json",
            "--fail-under-pass-rate",
            "1",
        ];
        if observed {
            args.push("--xray");
        }
        if deep && observed {
            args.push("--xray-deep");
        }
        self.invoke("eval", &args, Duration::from_secs(210))
    }

    fn repetition(&self) -> PathBuf {
        self.root
            .join("runs/capture/cases/scripted-loop/variants/single/repetitions/0001")
    }

    fn stop(&self, label: &str) {
        assert!(
            self.invoke(
                label,
                &["server", "stop-if-idle", "--wait"],
                Duration::from_secs(30)
            )
            .success(),
            "owned daemon release unverified; inspect {} (primary eval stdout/stderr also retained)",
            self.root.display()
        );
    }
}

#[derive(Clone, Copy)]
enum Grant {
    Exact,
    Absent,
    Expired,
    ForeignRoot,
    ForeignNamespace,
    ForeignState,
    NonRecursive,
}

struct OwnedChild(Child);
impl Drop for OwnedChild {
    fn drop(&mut self) {
        if self.0.try_wait().ok().flatten().is_none() {
            let _ = self.0.kill();
        }
        let _ = self.0.wait();
    }
}
fn spawn_logged(mut command: Command, root: &Path, label: &str) -> OwnedChild {
    command
        .stdin(Stdio::null())
        .stdout(File::create(root.join(format!("{label}.stdout"))).unwrap())
        .stderr(File::create(root.join(format!("{label}.stderr"))).unwrap());
    OwnedChild(command.spawn().expect("launch selected artifact"))
}
fn read_json(path: &Path) -> Value {
    let metadata = fs::metadata(path).unwrap_or_else(|error| panic!("required artifact {} unavailable: {error}; inspect fixture eval.stderr/eval.stdout and execution-failure.json", path.display()));
    assert!(metadata.len() <= 4 * 1024 * 1024, "bounded fixture JSON");
    let mut bytes = Vec::new();
    File::open(path)
        .unwrap()
        .take(4 * 1024 * 1024 + 1)
        .read_to_end(&mut bytes)
        .unwrap();
    assert!(bytes.len() <= 4 * 1024 * 1024, "bounded fixture JSON read");
    serde_json::from_slice(&bytes)
        .unwrap_or_else(|error| panic!("decode required artifact {}: {error}", path.display()))
}

/// Inspect only fixed recorder-generated paths in this fixture's private bundle.
/// Bound each file and each line; do not use manifest-controlled path joins.
#[allow(clippy::too_many_lines)]
fn assert_first_request_recorded(rep: &Path, root_run_id: &str) {
    use std::io::{BufRead as _, BufReader};
    let mut found = false;
    for (index, entry) in fs::read_dir(rep.join("xray-execution"))
        .unwrap()
        .enumerate()
    {
        assert!(index < 1024, "execution bundle inspection bound exceeded");
        let entry = entry.unwrap();
        let name = entry.file_name();
        let name = name.to_str().unwrap();
        if !name.starts_with("observation-") {
            continue;
        }
        assert!(entry.file_type().unwrap().is_dir());
        let manifest = read_json(&entry.path().join("manifest.json"));
        assert_eq!(manifest["state"], "finalized");
        assert_eq!(manifest["evidence_complete"], true);
        let file = File::open(entry.path().join("events.jsonl")).unwrap();
        assert!(file.metadata().unwrap().len() <= 16 * 1024 * 1024);
        let mut reader = BufReader::new(file);
        let mut first = true;
        let mut first_turn = None;
        let mut first_request_checked = false;
        let mut inspected_bytes = 0;
        loop {
            let mut line = Vec::new();
            let length = reader
                .by_ref()
                .take(1024 * 1024 + 1)
                .read_until(b'\n', &mut line)
                .unwrap();
            if length == 0 {
                break;
            }
            assert!(length <= 1024 * 1024);
            inspected_bytes += length;
            assert!(
                inspected_bytes <= 16 * 1024 * 1024,
                "event inspection bound exceeded"
            );
            let envelope: Value = serde_json::from_slice(&line).unwrap();
            if first {
                assert_eq!(envelope["sequence"], 1);
                assert_eq!(envelope["event"]["domain"], "xray");
                assert_eq!(envelope["event"]["name"], "ready");
                first = false;
            }
            let event = &envelope["event"];
            if event["domain"] == "session" && event["name"] == "model_turn_started" {
                assert_eq!(event["correlation"]["run_id"], root_run_id);
                first_turn.get_or_insert_with(|| {
                    event["correlation"]["turn_id"].as_str().unwrap().to_owned()
                });
            }
            if event["domain"] == "provider.fake"
                && event["name"] == "adapted_request"
                && !first_request_checked
            {
                assert_eq!(event["correlation"]["run_id"], root_run_id);
                assert_eq!(event["correlation"]["session_id"], manifest["session_id"]);
                assert_eq!(
                    event["correlation"]["turn_id"].as_str(),
                    first_turn.as_deref()
                );
                assert!(
                    first_turn.is_some(),
                    "request must follow the first canonical turn"
                );
                assert_eq!(event["sensitive"], true);
                assert_eq!(
                    event["capture"],
                    json!({"schema_version":1,"attachments":[]})
                );
                let data = &event["data"];
                assert_eq!(data["capture_format"], "bcode.provider.capture");
                assert_eq!(data["capture_version"], 1);
                assert_eq!(data["representation"], "redacted_json");
                assert_eq!(data["wire_equivalent"], false);
                assert_eq!(data["value"]["model_id"], "fake-echo");
                // The fake provider publishes normalized inline JSON, not a wire request.
                // Require the actual first-call workflow input in retained user text.
                let input = data["value"]["messages"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .filter(|message| message["role"] == "user")
                    .flat_map(|message| message["content"].as_array().unwrap())
                    .filter(|block| block["type"] == "text")
                    .filter_map(|block| block["text"].as_str())
                    .find_map(|text| {
                        text.split_once("<workflow-input-json>")
                            .and_then(|(_, rest)| rest.split_once("</workflow-input-json>"))
                            .map(|(input, _)| serde_json::from_str::<Value>(input).unwrap())
                    })
                    .expect("first provider request contains authored workflow input bytes");
                assert_eq!(
                    input["implementation_prompt"].as_str().unwrap().trim(),
                    "Return a scripted contribution and evaluate completion."
                );
                assert_eq!(input["iteration"], 1);
                assert_eq!(
                    input["stop_condition"],
                    "Scripted evaluator completes at iteration 1."
                );
                first_request_checked = true;
                found = true;
            }
        }
        assert!(!first);
    }
    assert!(
        found,
        "complete evidence must contain the actual root provider request, not only readiness/cutoff"
    );
}

fn assert_recorded(fixture: &Fixture, task_passed: bool) {
    let rep = fixture.repetition();
    let metrics = read_json(&rep.join("metrics.json"));
    assert_eq!(
        metrics["workflow.oracle_passed"],
        json!(f64::from(u8::from(task_passed)))
    );
    assert_eq!(metrics["eval.xray.evidence_complete"], json!(1.0));
    assert_eq!(metrics["eval.xray.ready_before_submission"], json!(1.0));
    // Scalar transition logging is independently opt-in. X-ray consent does not
    // enable durable scalar history; retain its truthful gap without weakening
    // the recursive model/session evidence requirements below.
    assert_eq!(metrics["eval.xray.workflow_children_complete"], json!(1.0));
    let history_capture = read_json(&rep.join("capture-completeness.json"));
    assert_eq!(
        history_capture["boundaries"]["observations"]["status"],
        "not_enabled"
    );
    assert_eq!(
        history_capture["boundaries"]["observations"]["complete"],
        false
    );
    assert_eq!(
        history_capture["boundaries"]["observations"]["semantic_history_complete"],
        false
    );
    let observations = read_json(&rep.join("workflow-observations-0000.json"));
    assert_eq!(observations["origin"], "not_enabled");
    assert_eq!(observations["observations"], json!([]));
    assert!(!rep.join("workflow-observations-error.json").exists());
    let prepared = read_json(&rep.join("admission-prepare.stdout"));
    assert_eq!(prepared["run_id"], fixture.root_run_id);
    assert!(prepared.get("runtime_work_id").is_none());
    let admitted = read_json(&rep.join("admission.json"));
    assert_eq!(admitted["run_id"], fixture.root_run_id);
    let aggregate = read_json(&rep.join("xray-execution/evidence.json"));
    assert_eq!(aggregate["evidence_complete"], true);
    assert!(aggregate["observed_sessions"].as_u64().unwrap() > 0);
    assert_eq!(aggregate["watch_ready"], true);
    assert_eq!(aggregate["watch_drained"], true);
    assert_eq!(
        aggregate["revoked"], false,
        "no involuntary capture revocation"
    );
    let run_sessions = aggregate["sessions"][&fixture.root_run_id]
        .as_object()
        .unwrap();
    assert!(!run_sessions.is_empty());
    for session in run_sessions.values() {
        let segments = session["segments"].as_array().unwrap();
        assert_ne!(segments.as_slice(), [] as [serde_json::Value; 0]);
        assert!(
            segments
                .iter()
                .all(|segment| segment["ready_before_submission"] == true)
        );
    }
    assert_first_request_recorded(&rep, &fixture.root_run_id);
    let manifest = read_json(&rep.join("xray/manifest.json"));
    assert_eq!(manifest["state"], "finalized");
    assert_eq!(manifest["evidence_complete"], true);
    assert!(fixture.root.join("runs/capture/summary.json").is_file());
}

fn record_phase(root: &Path, phase: &str, state: &str, failure: Option<&str>) {
    fs::write(
        root.join(format!("phase-{phase}.json")),
        serde_json::to_vec_pretty(&json!({
            "schema_version":1, "phase":phase, "state":state,
            "failure":failure, "admission_replayed":false,
        }))
        .unwrap(),
    )
    .unwrap();
}

fn case_phase<T>(root: &Path, phase: &str, work: impl FnOnce() -> T) -> std::thread::Result<T> {
    record_phase(root, phase, "started", None);
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(work));
    let failure = result.as_ref().err().map(|panic| {
        panic
            .downcast_ref::<String>()
            .map(String::as_str)
            .or_else(|| panic.downcast_ref::<&str>().copied())
            .unwrap_or("non-string test failure")
    });
    record_phase(
        root,
        phase,
        if result.is_ok() { "passed" } else { "failed" },
        failure,
    );
    result
}

#[test]
fn phase_failure_retains_prior_evidence_and_cleanup_result() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    let setup = case_phase(root, "setup", || ());
    let execution = case_phase(root, "execution", || {
        fs::write(
            root.join("admission.json"),
            b"{\"run_id\":\"acknowledged\"}",
        )
        .unwrap();
        panic!("transport interrupted after acknowledgment");
    });
    let cleanup = case_phase(root, "cleanup", || panic!("release unverified"));
    assert!(setup.is_ok());
    assert!(execution.is_err());
    assert!(cleanup.is_err());
    assert_eq!(read_json(&root.join("phase-setup.json"))["state"], "passed");
    assert_eq!(
        read_json(&root.join("phase-execution.json"))["state"],
        "failed"
    );
    assert_eq!(
        read_json(&root.join("phase-cleanup.json"))["state"],
        "failed"
    );
    assert_eq!(
        read_json(&root.join("admission.json"))["run_id"],
        "acknowledged"
    );
}

#[test]
fn started_phase_is_retained_before_work_can_interrupt() {
    let directory = tempfile::tempdir().unwrap();
    case_phase(directory.path(), "execution", || {
        let phase = read_json(&directory.path().join("phase-execution.json"));
        assert_eq!(phase["state"], "started");
        assert_eq!(phase["admission_replayed"], false);
    })
    .unwrap();
    assert_eq!(
        read_json(&directory.path().join("phase-execution.json"))["state"],
        "passed"
    );
}

/// Each test owns one attempt and one private scope. Never replay admission after
/// a missing response. Each phase is durable before the next phase begins, so
/// interruption or cleanup failure cannot erase the original execution evidence.
fn configure_historical(fixture: &Fixture) {
    let path = fixture.root.join("suite.toml");
    let mut suite: toml::Value = toml::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    suite["variants"][0]["metadata"]
        .as_table_mut()
        .unwrap()
        .remove("xray_root_run_id");
    fs::write(path, toml::to_string_pretty(&suite).unwrap()).unwrap();
}

fn run_case(label: &str, grant: Grant, deep: bool, expected: Option<bool>) {
    let subject = Subject::selected();
    let mut fixture = Fixture::new(&subject, label);
    for phase in ["setup", "execution", "finalization", "cleanup"] {
        record_phase(&fixture.root, phase, "not_started", None);
    }
    let historical = label == "historical-unobserved";
    let root = fixture.root.clone();
    let setup = case_phase(&root, "setup", || {
        fixture.identify(&subject);
        fixture.configure(
            grant,
            deep,
            if expected == Some(false) {
                "independent-oracle-must-not-match"
            } else {
                "evaluated"
            },
        );
        if historical {
            configure_historical(&fixture);
        }
        if label == "positive-warm" {
            assert!(
                fixture
                    .invoke(
                        "warm-status",
                        &["server", "status"],
                        // The warm precondition is a verified application response,
                        // not a second startup performance assertion. Use the same
                        // process watchdog as execution; startup budget races have
                        // deterministic coverage in the lifecycle owner.
                        Duration::from_secs(210)
                    )
                    .success()
            );
        }
        let fingerprints = ["config.toml", "suite.toml"].map(|name| {
            let bytes = fs::read(fixture.root.join(name)).unwrap();
            (name, hex::encode(Sha256::digest(bytes)))
        });
        fs::write(
            fixture.root.join("inputs.json"),
            serde_json::to_vec_pretty(&json!({"sha256": fingerprints})).unwrap(),
        )
        .unwrap();
    });
    let execution = if setup.is_ok() {
        Some(case_phase(&fixture.root, "execution", || {
            fixture.eval(true, !historical)
        }))
    } else {
        None
    };
    let finalization = match execution.as_ref() {
        Some(Ok(status)) => Some(case_phase(&fixture.root, "finalization", || {
            if historical {
                assert!(status.success());
                let admitted = read_json(&fixture.repetition().join("admission.json"));
                let run = admitted["run_id"].as_str().unwrap();
                assert!(
                    fixture
                        .invoke(
                            "historical-observations",
                            &["workflow", "observations", "--run-id", run],
                            Duration::from_secs(15)
                        )
                        .success()
                );
                let page = read_json(&fixture.root.join("historical-observations.stdout"));
                assert_eq!(page["origin"], "not_enabled");
                assert_eq!(page["observations"], json!([]));
                assert!(
                    !fixture
                        .repetition()
                        .join("xray-execution/evidence.json")
                        .exists()
                );
            } else if let Some(passed) = expected {
                assert_eq!(status.success(), passed, "task outcome differs from oracle");
                assert_recorded(&fixture, passed);
            } else {
                assert_nonadmission(&fixture, *status);
            }
        })),
        _ => None,
    };
    let cleanup = case_phase(&fixture.root, "cleanup", || fixture.stop("stop"));
    let passed = setup.is_ok()
        && execution.as_ref().is_some_and(Result::is_ok)
        && finalization.as_ref().is_some_and(Result::is_ok)
        && cleanup.is_ok();
    fs::write(fixture.root.join("case-outcome.json"),
        serde_json::to_vec_pretty(&json!({
            "schema_version":2,"case":label,"attempts":1,"root_run_id":fixture.root_run_id,
            "passed":passed,"admission_replayed":false,
            "phases":["phase-setup.json","phase-execution.json","phase-finalization.json","phase-cleanup.json"],
        })).unwrap()).unwrap();
    assert!(
        passed,
        "case {label} failed; inspect phase records: {}",
        fixture.root.display()
    );
}

fn assert_nonadmission(fixture: &Fixture, status: ExitStatus) {
    assert!(!status.success());
    let rep = fixture.repetition();
    let prepared = read_json(&rep.join("admission-prepare.stdout"));
    assert_eq!(prepared["run_id"], fixture.root_run_id);
    assert!(prepared.get("runtime_work_id").is_none());
    let failure = read_json(&rep.join("execution-failure.json"));
    assert!(
        failure["error"]
            .as_str()
            .is_some_and(|error| error.contains("observed_admission")),
        "must reach observed admission, not fail at unrelated startup/configuration"
    );
    // The current eval error deliberately preserves transport ambiguity. A missing
    // local receipt cannot prove denial. Fence late commits by verified release of
    // this fixture's exact daemon before one bounded public durable status lookup.
    // This may start a replacement matching daemon in this PRIVATE scope only.
    // Never cancel, resubmit, repair, inspect databases, or infer absence on error.
    fixture.stop("nonadmission-release");
    let reconciled = fixture.invoke(
        "nonadmission-status",
        &["workflow", "run-status", "--run-id", &fixture.root_run_id],
        Duration::from_secs(30),
    );
    let absence = reconciled.success()
        && read_json(&fixture.root.join("nonadmission-status.stdout")).is_null();
    fs::write(
        fixture.root.join("nonadmission-proof.json"),
        serde_json::to_vec_pretty(&json!({
            "schema_version": 1, "run_id": fixture.root_run_id,
            "admission_response": "unresolved_no_retry",
            "prior_owner_released": true,
            "durable_absence_after_release": absence,
            "admission_replayed": false,
        }))
        .unwrap(),
    )
    .unwrap();
    assert!(
        absence,
        "non-admission unresolved or run exists; no retry permitted"
    );
    assert!(!rep.join("admission.json").exists());
    assert!(!rep.join("xray-execution/evidence.json").exists());
}

macro_rules! acceptance_case {
    ($name:ident, $label:literal, $grant:expr, $deep:expr, $expected:expr) => {
        #[test]
        #[ignore = "coordinator runs the frozen matrix once, serially, with explicit artifact hashes"]
        fn $name() {
            run_case($label, $grant, $deep, $expected);
        }
    };
}

// Distinct admission cases, one task-failure independence case, and the full
// negative matrix once. Genuine recursive execution stays in recursive_server.
acceptance_case!(
    positive_cold,
    "positive-cold",
    Grant::Exact,
    true,
    Some(true)
);
acceptance_case!(
    positive_warm,
    "positive-warm",
    Grant::Exact,
    true,
    Some(true)
);
acceptance_case!(
    historical_unobserved,
    "historical-unobserved",
    Grant::Absent,
    false,
    Some(true)
);
acceptance_case!(
    task_failure,
    "task-failure",
    Grant::Exact,
    true,
    Some(false)
);
acceptance_case!(negative_absent, "absent", Grant::Absent, true, None);
acceptance_case!(negative_expired, "expired", Grant::Expired, true, None);
acceptance_case!(
    negative_root,
    "foreign-root",
    Grant::ForeignRoot,
    true,
    None
);
acceptance_case!(
    negative_namespace,
    "foreign-namespace",
    Grant::ForeignNamespace,
    true,
    None
);
acceptance_case!(
    negative_state,
    "foreign-state",
    Grant::ForeignState,
    true,
    None
);
acceptance_case!(
    negative_nonrecursive,
    "nonrecursive",
    Grant::NonRecursive,
    true,
    None
);
acceptance_case!(negative_shallow, "shallow", Grant::Exact, false, None);
