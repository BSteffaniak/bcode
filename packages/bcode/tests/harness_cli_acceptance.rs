//! Concrete CLI -> client -> daemon -> canonical managed application acceptance.
//! No argv bindings, output-pointer overrides, providers, or live native agents.
#![cfg(all(feature = "app", feature = "static-bundled-claude-code-plugin"))]
#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

#[path = "fixtures/harness/python.rs"]
mod fixture_python;

use serde_json::{Value, json};
use std::io::{Read as _, Seek as _};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const SESSION: &str = "11111111-1111-4111-8111-111111111111";
const CHILD: &str = "22222222-2222-4222-8222-222222222222";
#[cfg(all(unix, feature = "daemon-client"))]
const PROBE_SESSION: &str = "33333333-3333-4333-8333-333333333333";
#[cfg(all(unix, feature = "daemon-client"))]
const PROBE_CHILD: &str = "44444444-4444-4444-8444-444444444444";
const ATTEMPT: &str = "55555555-5555-4555-8555-555555555555";
const SECRET: &str = "fixture-secret-DO-NOT-PERSIST";
const BUDGET: u64 = 1_048_576;

struct Cli {
    root: tempfile::TempDir,
    config: PathBuf,
    offline: PathBuf,
    native: PathBuf,
    instance: Value,
}
struct Reply {
    ok: bool,
    value: Value,
    output: String,
}

fn response_failure_label(value: &Value) -> &'static str {
    if value["kind"]["kind"].as_str() != Some("failure") {
        return "not_failure";
    }
    match value["kind"]["response"].as_str() {
        Some("unsupported") => "unsupported",
        Some("invalid_request") => "invalid_request",
        Some("unavailable") => "unavailable",
        Some("stale_generation") => "stale_generation",
        Some("conflict") => "conflict",
        Some("authentication") => "authentication",
        Some("policy") => "policy",
        Some("uncertain") => "uncertain",
        _ => "unrecognized_failure",
    }
}

fn operation_label(args: &[&str]) -> &'static str {
    match args {
        ["artifact-id", ..] => "artifact_identity",
        ["server", "start", ..] => "server_start",
        ["server", "stop", ..] => "server_stop",
        ["harness", "start", ..] => "harness_start",
        ["harness", ..] => "harness_operation",
        ["session", ..] => "session_operation",
        _ => "other_operation",
    }
}

fn bounded_failure_code(stderr: &mut std::fs::File) -> &'static str {
    if stderr.rewind().is_err() {
        return "diagnostic_read_failed";
    }
    let mut prefix = Vec::new();
    if stderr.take(8192).read_to_end(&mut prefix).is_err() {
        return "diagnostic_read_failed";
    }
    // Never echo native/provider output, paths, arguments, transcripts, or arbitrary error codes.
    // Only a fixed classification of the first recognized public/runtime failure is emitted.
    let categories: &[(&[u8], &str)] = &[
        (b"daemon_start_timeout", "daemon_start_timeout"),
        (b"daemon_start_exited", "daemon_start_exited"),
        (b"daemon_start_health_check", "daemon_start_health_check"),
        (
            b"daemon_start_endpoint_occupied",
            "daemon_start_endpoint_occupied",
        ),
        (b"daemon_start_incompatible", "daemon_start_incompatible"),
        (b"daemon_start_connection", "daemon_start_connection"),
        (b"panicked at", "runtime_panic"),
        (b"stack overflow", "runtime_stack_overflow"),
        (b"unrecognized subcommand", "cli_subcommand_rejected"),
        (b"unexpected argument", "cli_argument_rejected"),
        (b"Permission denied", "permission_denied"),
        (b"No such file or directory", "missing_file"),
        (b"Address already in use", "endpoint_in_use"),
        (b"Connection refused", "connection_refused"),
        (b"error", "unclassified_error"),
    ];
    categories
        .iter()
        .filter_map(|(needle, code)| {
            prefix
                .windows(needle.len())
                .position(|window| window == *needle)
                .map(|position| (position, *code))
        })
        .min_by_key(|(position, _)| *position)
        .map_or("no_recognized_error_in_prefix", |(_, code)| code)
}

fn startup_phase_summary(root: &Path) -> Value {
    let mut reports = Vec::new();
    for slot in 0..32 {
        let path = root
            .join("state/startup-reports")
            .join(format!("{slot}.json"));
        let Ok(file) = std::fs::File::open(path) else {
            continue;
        };
        if !file.metadata().is_ok_and(|meta| meta.len() <= BUDGET) {
            continue;
        }
        let Ok(report) = serde_json::from_reader::<_, Value>(file.take(BUDGET + 1)) else {
            continue;
        };
        let mut phases = report["phases"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|phase| {
                let name = phase["name"].as_str()?;
                if name.len() > 128
                    || !name
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
                {
                    return None;
                }
                let start = phase["start_us"].as_u64()?;
                let end = phase["end_us"].as_u64();
                let duration = end
                    .unwrap_or_else(|| report["elapsed_us"].as_u64().unwrap_or(start))
                    .saturating_sub(start);
                Some((
                    duration,
                    json!({"name":name,"duration_us":duration,"complete":end.is_some()}),
                ))
            })
            .collect::<Vec<_>>();
        phases.sort_by_key(|(duration, _)| std::cmp::Reverse(*duration));
        reports.push(json!({"pid":report["pid"].as_u64(),"ready":report["ready"].as_bool(),
            "elapsed_us":report["elapsed_us"].as_u64(),"unattributed_us":report["unattributed_us"].as_u64(),
            "phases":phases.into_iter().take(8).map(|(_, phase)|phase).collect::<Vec<_>>() }));
    }
    json!(reports)
}

#[test]
fn startup_diagnostics_keep_phase_timing_without_native_or_path_material() {
    let root = tempfile::tempdir().unwrap();
    let directory = root.path().join("state/startup-reports");
    std::fs::create_dir_all(&directory).unwrap();
    std::fs::write(directory.join("0.json"), serde_json::to_vec(&json!({
        "pid":1,"ready":false,"elapsed_us":35000000,"unattributed_us":10,
        "private":SECRET,"phases":[
            {"name":"launcher.artifact_prepare","start_us":10,"end_us":30000000,"subject":SECRET},
            {"name":"/private/native/path","start_us":0,"end_us":35000000}
        ]
    })).unwrap()).unwrap();
    let summary = startup_phase_summary(root.path());
    assert_eq!(summary[0]["phases"][0]["name"], "launcher.artifact_prepare");
    assert_eq!(summary[0]["phases"][0]["duration_us"], 29999990);
    let encoded = serde_json::to_string(&summary).unwrap();
    assert!(!encoded.contains(SECRET));
    assert!(!encoded.contains("/private/native/path"));
}

#[test]
fn budget_failure_diagnostics_are_fixed_and_do_not_echo_output() {
    use std::io::Write as _;
    let mut stderr = tempfile::tempfile().unwrap();
    writeln!(
        stderr,
        "{SECRET}\nthread 'private-name' panicked at /private/path"
    )
    .unwrap();
    assert_eq!(bounded_failure_code(&mut stderr), "runtime_panic");
    assert_eq!(operation_label(&["server", "start"]), "server_start");
    assert_eq!(operation_label(&[SECRET]), "other_operation");
    assert_eq!(
        response_failure_label(&json!({"kind":{"kind":"failure","response":"conflict"}})),
        "conflict"
    );
    assert_eq!(
        response_failure_label(&json!({"kind":{"kind":"failure","response":SECRET}})),
        "unrecognized_failure"
    );
    let mut bounded = tempfile::tempfile().unwrap();
    bounded.write_all(&vec![b'x'; 8192]).unwrap();
    bounded.write_all(b"panicked at secret tail").unwrap();
    assert_eq!(
        bounded_failure_code(&mut bounded),
        "no_recognized_error_in_prefix"
    );
}

impl Cli {
    fn new(scenario: &str) -> Self {
        let root = tempfile::tempdir().unwrap();
        let native = root.path().join("native");
        std::fs::create_dir(&native).unwrap();
        std::fs::write(native.join("scenario"), scenario).unwrap();
        // Application CLI invocations incur process startup between live native
        // interactions. Opt in here only; standalone protocol/EOF fixtures keep
        // their short deadline and initialization remains independently bounded.
        std::fs::write(native.join("application-idle-seconds"), "120").unwrap();
        if scenario == "question" {
            std::fs::write(native.join("hold-after-answer"), "until-interrupt").unwrap();
        }
        let executable = root.path().join("offline-claude");
        let python = fixture_python::python();
        let interpreter = python
            .to_str()
            .expect("fixture shebang requires UTF-8 Python path");
        assert!(
            !interpreter.chars().any(char::is_whitespace) && interpreter.len() < 240,
            "resolved test Python path must fit a direct executable shebang"
        );
        let source = include_str!("fixtures/harness/claude_native.py");
        let mut source = source.to_owned();
        if scenario == "terminal_auth" {
            source = source.replace(
                "VERSION = \"bcode-offline-claude-fixture 1.0.0\"",
                "VERSION = \"2.1.308\"",
            );
            // Controls exist only in this generated offline executable. They neither alter
            // the admitted plan nor consult a real native account or credential store.
            source = source.replace(
                "effect(root, \"terminal-login\")",
                "effect(root, \"terminal-login\" + ((root / \"terminal-login.case\").read_text(encoding=\"ascii\") if (root / \"terminal-login.case\").exists() else \"\"))",
            );
            source = source.replace(
                "(root / \"terminal-login.finished\").touch()\n        return",
                "(root / \"terminal-login.finished\").touch()\n        raise SystemExit(int((root / \"terminal-login.exit-code\").read_text(encoding=\"ascii\")) if (root / \"terminal-login.exit-code\").exists() else 0)",
            );
        }
        let (_, body) = source.split_once('\n').unwrap();
        std::fs::write(&executable, format!("#!{interpreter}\n{body}")).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        let config = root.path().join("online.toml");
        let offline = root.path().join("offline.toml");
        // JSON string literals are valid TOML basic strings for these paths.
        let quoted = |path: &Path| serde_json::to_string(path.to_str().unwrap()).unwrap();
        let mut settings = include_str!("fixtures/harness/claude-config.toml.in")
            .replace("@EXECUTABLE@", &quoted(&executable))
            .replace("@NAMESPACE@", &quoted(&native));
        if scenario == "terminal_auth" {
            settings = settings.replace("bcode-offline-claude-fixture 1.0.0", "2.1.308");
        }
        if scenario == "multi_turn" {
            // Matches the configuration native setup generates by default.
            settings = settings.replace(
                "verified_session_state = true",
                "verified_session_state = false",
            );
        }
        settings.push_str("\n[metrics]\nstartup_reports = true\n");
        std::fs::write(&config, settings).unwrap();
        std::fs::write(
            &offline,
            "[plugins]\ndefault = \"none\"\nenabled = []\n\n[metrics]\nstartup_reports = true\n",
        )
        .unwrap();
        record_test_artifact(root.path());
        let instance = json!({"instance_id":"offline-claude","plugin_id":"bcode.claude-code",
            "executable":executable,"config_namespace":native,
            "auth":{"provider_id":"claude-code","profile":"offline-fixture",
                "credential_namespace":native,"custody":"native_managed"}});
        Self {
            root,
            config,
            offline,
            native,
            instance,
        }
    }

    fn run(&self, config: &Path, args: &[&str]) -> Reply {
        self.run_program(config, args, None)
    }

    fn run_program(&self, config: &Path, args: &[&str], wrapper: Option<&Path>) -> Reply {
        let mut out = tempfile::tempfile().unwrap();
        let mut err = tempfile::tempfile().unwrap();
        let started = Instant::now();
        let phase_path = self.native.join("initialize-replied.monotonic_ns");
        let initial_phase = std::fs::read_to_string(&phase_path).ok();
        let mut initialized_at = None;
        let measures_prepare = args.starts_with(&["harness", "start"]);
        let mut command = wrapper.map_or_else(
            || Command::new(env!("CARGO_BIN_EXE_bcode")),
            |path| {
                let mut command = Command::new(fixture_python::python());
                command
                    .arg(path)
                    .arg(&self.native)
                    .arg(env!("CARGO_BIN_EXE_bcode"));
                command
            },
        );
        let mut child = command
            .env_clear()
            .env("HOME", self.root.path())
            .env("XDG_CONFIG_HOME", self.root.path().join("config"))
            .env("XDG_DATA_HOME", self.root.path().join("data"))
            .env("PATH", "/usr/bin:/bin")
            .env("BCODE_CONFIG", config)
            .arg("--state-root")
            .arg(self.root.path().join("state"))
            .args(args)
            .current_dir(self.root.path())
            .stdin(Stdio::null())
            .stdout(Stdio::from(out.try_clone().unwrap()))
            .stderr(Stdio::from(err.try_clone().unwrap()))
            .spawn()
            .unwrap();
        // Only the PTY wrapper runs multiple cold CLI phases. Ordinary CLI
        // operations and the initialize-to-Prepared proof keep their bounds.
        let deadline =
            Instant::now() + Duration::from_secs(if wrapper.is_some() { 125 } else { 35 });
        let status = loop {
            if measures_prepare
                && initialized_at.is_none()
                && std::fs::read_to_string(&phase_path)
                    .ok()
                    .is_some_and(|phase| Some(phase) != initial_phase)
            {
                initialized_at = Some(Instant::now());
            }
            if let Some(status) = child.try_wait().unwrap() {
                break status;
            }
            if initialized_at.is_some_and(|phase: Instant| phase.elapsed() > Duration::from_secs(5))
            {
                let _ = child.kill();
                let _ = child.wait();
                panic!(
                    "managed start exceeded 5s after native initialize reply without identity; total={:?} native_timings={}",
                    started.elapsed(),
                    self.native_timings()
                );
            }
            let deadline_exceeded = Instant::now() >= deadline;
            let stdout_bytes = out.metadata().unwrap().len();
            let stderr_bytes = err.metadata().unwrap().len();
            let stdout_cap_exceeded = stdout_bytes > BUDGET;
            let stderr_cap_exceeded = stderr_bytes > BUDGET;
            if deadline_exceeded || stdout_cap_exceeded || stderr_cap_exceeded {
                let _ = child.kill();
                let _ = child.wait();
                let code = bounded_failure_code(&mut err);
                panic!(
                    "bounded isolated CLI failure: operation={} deadline_exceeded={deadline_exceeded} stdout_cap_exceeded={stdout_cap_exceeded} stderr_cap_exceeded={stderr_cap_exceeded} stdout_bytes={stdout_bytes} stderr_bytes={stderr_bytes} elapsed={:?} diagnostic_code={code} startup_phases={}",
                    operation_label(args),
                    started.elapsed(),
                    startup_phase_summary(self.root.path()),
                );
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        assert!(
            initialized_at.is_none_or(|phase| phase.elapsed() <= Duration::from_secs(5)),
            "managed start returned too late after initialize; total={:?} native_timings={}",
            started.elapsed(),
            self.native_timings()
        );
        eprintln!(
            "isolated CLI {:?}: total={:?} post_initialize={:?} native_timings={}",
            args.first(),
            started.elapsed(),
            initialized_at.map(|phase| phase.elapsed()),
            self.native_timings()
        );
        out.rewind().unwrap();
        err.rewind().unwrap();
        let mut stdout = String::new();
        let mut stderr = String::new();
        out.take(BUDGET + 1).read_to_string(&mut stdout).unwrap();
        err.take(BUDGET + 1).read_to_string(&mut stderr).unwrap();
        assert!(stdout.len() <= BUDGET as usize && stderr.len() <= BUDGET as usize);
        if args.starts_with(&["server", "start"]) {
            eprintln!(
                "isolated server startup phases={}",
                startup_phase_summary(self.root.path())
            );
        }
        let value = serde_json::from_str(&stdout).unwrap_or(Value::Null);
        let output = stdout + &stderr;
        assert!(!output.contains(SECRET), "secret leaked through CLI output");
        Reply {
            ok: status.success(),
            value,
            output,
        }
    }

    fn call(&self, command: &str, kind: Value, target: &str, parent: Option<&str>) -> Reply {
        // The application contract requires envelope correlation to equal the
        // retained mutation identity, not a command-name-generated request ID.
        let mutation_pointer = match kind["operation"].as_str().unwrap() {
            "open" => Some("/request/open_id"),
            "submit" => Some("/request/submission/submission_id"),
            "respond" => Some("/request/response/response_id"),
            "interrupt" | "close" => Some("/request/mutation_id"),
            _ => None,
        };
        let request_id = mutation_pointer.map_or_else(
            || format!("fixture-{command}"),
            |pointer| {
                kind.pointer(pointer)
                    .and_then(Value::as_str)
                    .expect("mutation payload must carry its retained identity")
                    .to_owned()
            },
        );
        let call = json!({"session_id":target,"parent_session_id":parent,
            "assistance_destination_session_id":null,
            "harness":{"schema_version":1,"request_id":request_id,
                "instance":self.instance,"kind":kind}});
        // Distinct request files let concurrent CLI calls run against one daemon.
        static REQUESTS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let sequence = REQUESTS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let path = self.root.path().join(format!("request-{sequence}.json"));
        std::fs::write(&path, serde_json::to_vec(&call).unwrap()).unwrap();
        self.run(
            &self.config,
            &["harness", command, "--request", path.to_str().unwrap()],
        )
    }

    fn native_timings(&self) -> Value {
        let mut values = serde_json::Map::new();
        for label in [
            "stream-started",
            "initialize-received",
            "initialize-replied",
            "first-user-received",
        ] {
            if let Ok(raw) =
                std::fs::read_to_string(self.native.join(format!("{label}.monotonic_ns")))
            {
                if let Ok(timestamp) = raw.parse::<u64>() {
                    values.insert(label.into(), json!(timestamp));
                }
            }
        }
        Value::Object(values)
    }

    fn open(&self, session_id: &str, parent: Option<&str>) -> Value {
        if !self.native.join("launch-0").exists() {
            // Separate executable/config startup and daemon readiness from the
            // native initialize-to-Prepared interval measured inside run().
            let control = self.run(&self.config, &["artifact-id"]);
            assert!(
                control.ok,
                "isolated executable control failed: {}",
                control.output
            );
            let boot = self.run(&self.config, &["server", "start"]);
            assert!(boot.ok, "isolated daemon startup failed: {}", boot.output);
            let ready = self.run(&self.config, &["server", "status"]);
            assert!(
                ready.ok,
                "isolated daemon readiness control failed: {}",
                ready.output
            );
        }
        let reply = self.call(
            "start",
            json!({"operation":"open","request":{
            "open_id":format!("open-{session_id}"),"session_id":session_id,"generation":1,
            "working_directory":self.root.path(),"resume":null}}),
            session_id,
            parent,
        );
        assert!(
            reply.ok,
            "managed start must return before native pre-turn identity exists; value={} output={} native_launched={} initialized={} user_submitted={} binding_emitted={} dummy_rejected={}",
            reply.value,
            reply.output,
            self.native.join("launch-0").exists(),
            self.native.join("initialized.waiting").exists(),
            self.native.join("submit.effect").exists(),
            self.native.join("binding-confirmed.emitted").exists(),
            self.native.join("unexpected-user.rejected").exists()
        );

        assert!(
            self.native.join("initialize-replied.monotonic_ns").exists(),
            "Prepared requires a real initialized live fixture transport; value={} output={}",
            reply.value,
            reply.output
        );
        assert_eq!(
            reply.value["kind"]["kind"], "prepared",
            "initialize is not native creation confirmation"
        );
        let prepared = reply.value["kind"]["response"].clone();
        assert_eq!(prepared["session_id"], session_id);
        assert_eq!(prepared["instance_id"], "offline-claude");
        assert_eq!(prepared["open_id"], format!("open-{session_id}"));
        assert!(
            prepared.get("native_session_id").is_none(),
            "no sentinel binding"
        );
        assert!(
            !self.native.join("submit.effect").exists(),
            "open must not submit a dummy prompt"
        );
        assert!(!self.native.join("binding-confirmed.emitted").exists());
        json!({"state":"prepared","session":prepared})
    }

    fn send(&self, target: &Value, text: &str) -> Reply {
        self.call("send", json!({"operation":"submit","request":{"target":target,
            "submission":{"submission_id":"retained-submit","attempt_id":ATTEMPT,
                "session_id":target["session"]["session_id"],"generation":target["session"]["generation"],
                "text":text,"placement":{"kind":"next_turn"}}}}),
            target["session"]["session_id"].as_str().unwrap(), None)
    }

    fn send_turn(
        &self,
        target: &Value,
        text: &str,
        submission_id: &str,
        attempt_id: &str,
    ) -> Reply {
        self.call("send", json!({"operation":"submit","request":{"target":target,
            "submission":{"submission_id":submission_id,"attempt_id":attempt_id,
                "session_id":target["session"]["session_id"],"generation":target["session"]["generation"],
                "text":text,"placement":{"kind":"next_turn"}}}}),
            target["session"]["session_id"].as_str().unwrap(), None)
    }

    /// Observe until the exact submission is terminal, promoting a confirmed binding.
    fn await_terminal(
        &self,
        target: &Value,
        mut after: Option<String>,
        submission_id: &str,
    ) -> (Value, Option<String>) {
        // Each isolated CLI observation is a cold process; allow several cycles.
        let deadline = Instant::now() + Duration::from_secs(90);
        let mut target = target.clone();
        loop {
            let page = self.observe(&target, after.as_deref());
            let mut terminal = false;
            for item in page["observations"].as_array().unwrap() {
                let kind = &item["kind"];
                if kind["kind"] == "binding_confirmed" {
                    target = json!({"state":"confirmed","session":kind["binding"]});
                }
                if kind["kind"] == "execution"
                    && kind["event"]["event"] == "submission_terminal"
                    && kind["event"]["attempt"]["submission_id"] == submission_id
                {
                    terminal = true;
                }
            }
            after = page["next_after"].as_str().map(str::to_owned);
            if terminal {
                return (target, after);
            }
            assert!(
                Instant::now() < deadline,
                "submission {submission_id} must reach a terminal observation; last page={page}"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn observe(&self, target: &Value, after: Option<&str>) -> Value {
        self.observe_waiting(target, after, 0)
    }

    fn observe_waiting(&self, target: &Value, after: Option<&str>, wait_millis: u32) -> Value {
        let reply = self.call(
            "observe",
            json!({"operation":"observe","request":{
            "target":target,"after":after,"limit":16,"wait_millis":wait_millis}}),
            target["session"]["session_id"].as_str().unwrap(),
            None,
        );
        assert!(
            reply.ok,
            "real observation application failed; value={} output={} native_timings={}",
            reply.value,
            reply.output,
            self.native_timings()
        );
        assert_eq!(reply.value["kind"]["kind"], "observations");
        reply.value["kind"]["response"].clone()
    }

    fn confirmed_question(&self, prepared: &Value) -> (Value, Value, Option<String>) {
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut after = None;
        let mut target = prepared.clone();
        let mut question = None;
        loop {
            let page = self.observe(&target, after.as_deref());
            for item in page["observations"].as_array().unwrap() {
                match item["kind"]["kind"].as_str() {
                    Some("binding_confirmed") => {
                        let confirmation = &item["kind"];
                        assert_eq!(confirmation["open_id"], prepared["session"]["open_id"]);
                        let binding = &confirmation["binding"];
                        for field in ["session_id", "instance_id", "generation", "native_version"] {
                            assert_eq!(binding[field], prepared["session"][field]);
                        }
                        assert!(!binding["native_session_id"].as_str().unwrap().is_empty());
                        if let Some(requested) =
                            prepared["session"]["requested_native_session_id"].as_str()
                        {
                            assert_eq!(binding["native_session_id"], requested);
                        }
                        target = json!({"state":"confirmed","session":binding});
                    }
                    Some("interaction")
                        if item["kind"]["interaction"]["kind"] == "question"
                            && item["kind"]["interaction"]["session_id"]
                                == prepared["session"]["session_id"]
                            && item["kind"]["interaction"]["generation"]
                                == prepared["session"]["generation"] =>
                    {
                        question = Some(item["kind"]["interaction"].clone());
                    }
                    _ => {}
                }
            }
            // Retain this page's successor even when returning immediately:
            // future live observations must not replay an earlier predecessor.
            after = page["next_after"].as_str().map(str::to_owned);
            if target["state"] == "confirmed" {
                if let Some(question) = question.take() {
                    return (target, question, after);
                }
            }
            assert!(
                Instant::now() < deadline,
                "native confirmation and question must be canonically observed"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn answer(&self, target: &Value, question: &Value, generation: u64) -> Reply {
        self.call(
            "respond",
            json!({"operation":"respond","request":{"target":target,
            "response":{"response_id":"retained-answer","request_id":question["request_id"],
            "session_id":target["session"]["session_id"],"generation":generation,
            "value":{"kind":"answer","choice_id":"0","text":""}}}}),
            target["session"]["session_id"].as_str().unwrap(),
            None,
        )
    }

    fn auth(&self, operation: &str, session_id: &str, sensitive: bool) -> Reply {
        let path = self.root.path().join("auth-request.json");
        // Auth route is not a HarnessRequest and cannot choose a method,
        // ownership fence, or positive native-inactivity assertion.
        let request = json!({"session_id":session_id,"instance_id":"offline-claude",
            "handle":null,"input":null,"sensitive_interaction":sensitive});
        std::fs::write(&path, serde_json::to_vec(&request).unwrap()).unwrap();
        self.run(
            &self.config,
            &[
                "harness",
                "auth",
                operation,
                "--request",
                path.to_str().unwrap(),
            ],
        )
    }

    fn stop(&self) {
        let reply = self.run(&self.config, &["server", "stop", "--force", "--yes"]);
        assert!(
            reply.ok,
            "isolated daemon failed to stop; value={} output={} native_timings={}",
            reply.value,
            reply.output,
            self.native_timings()
        );
    }

    fn reconcile(&self, target: &Value) -> Reply {
        let binding = if target["state"] == "confirmed" {
            target["session"].clone()
        } else {
            Value::Null
        };
        self.call("reconcile", json!({"operation":"reconcile","request":{
            "session_id":target["session"]["session_id"],"generation":target["session"]["generation"],
            "open_id":target["session"].get("open_id"),"submission_id":"retained-submit","attempt_id":ATTEMPT,
            "binding":binding}}),target["session"]["session_id"].as_str().unwrap(),None)
    }
}

impl Drop for Cli {
    fn drop(&mut self) {
        // Cleanup is bounded and non-panicking, including during failed tests.
        let child = Command::new(env!("CARGO_BIN_EXE_bcode"))
            .env_clear()
            .env("HOME", self.root.path())
            .env("BCODE_CONFIG", &self.config)
            .arg("--state-root")
            .arg(self.root.path().join("state"))
            .args(["server", "stop", "--force", "--yes"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
        let Ok(mut child) = child else {
            return;
        };
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            match child.try_wait() {
                Ok(Some(_)) => return,
                Ok(None) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(10))
                }
                _ => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return;
                }
            }
        }
    }
}

fn record_test_artifact(root: &Path) {
    use sha2::Digest as _;
    let mut source = std::fs::File::open(env!("CARGO_BIN_EXE_bcode")).unwrap();
    let mut hash = sha2::Sha256::new();
    let mut buffer = [0_u8; 65_536];
    loop {
        let n = source.read(&mut buffer).unwrap();
        if n == 0 {
            break;
        }
        hash.update(&buffer[..n]);
    }
    std::fs::write(
        root.join("test-artifact.sha256"),
        format!("{}\n", hex::encode(hash.finalize())),
    )
    .unwrap();
}

#[test]
fn managed_start_respond_stale_cancel_and_offline_bounded_history() {
    let cli = Cli::new("question");
    let binding = cli.open(SESSION, None);
    assert_eq!(
        cli.open(SESSION, None),
        binding,
        "retained open must reconcile"
    );
    let first = cli.send(&binding, "fixture-only");
    assert!(first.ok);
    assert_eq!(
        first.value["kind"]["kind"],
        "receipt",
        "first send response: failure={}",
        response_failure_label(&first.value)
    );
    let retry = cli.send(&binding, "fixture-only");
    assert!(retry.ok);
    assert_eq!(first.value, retry.value);
    assert!(!cli.send(&binding, "conflicting payload").ok);
    let (binding, question, live_after) = cli.confirmed_question(&binding);
    assert_eq!(question["kind"], "question");
    let generation = binding["session"]["generation"].as_u64().unwrap();
    assert!(!cli.answer(&binding, &question, generation + 1).ok);
    assert!(!cli.native.join("respond.effect").exists());
    let answer = cli.answer(&binding, &question, generation);
    assert!(
        answer.ok,
        "answer failed; value={} output={}",
        answer.value, answer.output
    );
    assert_eq!(
        answer.value["kind"]["kind"], "acknowledged",
        "answer did not acknowledge; value={} output={}",
        answer.value, answer.output
    );
    let again = cli.answer(&binding, &question, generation);
    assert!(
        again.ok,
        "answer retry failed; value={} output={}",
        again.value, again.output
    );
    assert_eq!(
        again.value["kind"]["kind"], "acknowledged",
        "answer retry did not reconcile; value={} output={}",
        again.value, again.output
    );
    assert!(!cli.native.join("respond.duplicate").exists());
    assert!(
        cli.native
            .join("answer-continuation-running.monotonic_ns")
            .exists(),
        "cancellation fixture must still have active native continuation after answer"
    );
    let cancel = cli.call(
        "cancel",
        json!({"operation":"interrupt","request":{
        "target":binding,"mutation_id":"retained-interrupt"}}),
        SESSION,
        None,
    );
    assert!(
        cancel.ok,
        "cancel failed; value={} output={}",
        cancel.value, cancel.output
    );
    assert_eq!(
        cancel.value["kind"]["kind"], "acknowledged",
        "cancel did not acknowledge; value={} output={}",
        cancel.value, cancel.output
    );
    assert!(cli.native.join("interrupt.effect").exists());
    // Drain and commit actual normalized observations before taking plugin offline.
    let _ = cli.observe(&binding, live_after.as_deref());
    let first_page = cli.run(
        &cli.config,
        &["harness", "history", SESSION, "--limit", "2"],
    );
    assert!(first_page.ok);
    let first_items = first_page.value["observations"].as_array().unwrap();
    assert_eq!(
        first_items.len(),
        2,
        "exercise a genuinely populated bounded page"
    );
    let cursor = first_page.value["next_after"].as_str().unwrap();
    let next_page = cli.run(
        &cli.config,
        &[
            "harness", "history", SESSION, "--after", cursor, "--limit", "2",
        ],
    );
    assert!(next_page.ok);
    for item in next_page.value["observations"].as_array().unwrap() {
        assert!(
            !first_items
                .iter()
                .any(|old| old["observation_id"] == item["observation_id"])
        );
    }
    cli.stop();
    let history = cli.run(
        &cli.offline,
        &["harness", "history", SESSION, "--limit", "2"],
    );
    assert!(
        history.ok,
        "canonical history must not require enabled native plugin; operation_ok={} failure={} startup_phases={}",
        history.ok,
        response_failure_label(&history.value),
        startup_phase_summary(cli.root.path())
    );
    assert!(history.value["observations"].as_array().unwrap().len() <= 2);
    assert_eq!(
        history.value["observations"], first_page.value["observations"],
        "offline history must retain the committed prefix"
    );
    assert!(history.value["completeness"].is_string());
}

#[test]
fn default_setup_configuration_admits_sequential_foreground_turns() {
    let cli = Cli::new("multi_turn");
    let prepared = cli.open(SESSION, None);
    let first = cli.send_turn(
        &prepared,
        "turn-1",
        "submit-1",
        "55555555-5555-4555-8555-000000000001",
    );
    assert_eq!(
        first.value["kind"]["kind"], "receipt",
        "first send: {}",
        first.value
    );
    let (confirmed, mut after) = cli.await_terminal(&prepared, None, "submit-1");
    assert_eq!(confirmed["state"], "confirmed");
    for turn in 2..=2 {
        let submission = format!("submit-{turn}");
        let reply = cli.send_turn(
            &confirmed,
            &format!("turn-{turn}"),
            &submission,
            &format!("55555555-5555-4555-8555-00000000000{turn}"),
        );
        assert_eq!(
            reply.value["kind"]["kind"], "receipt",
            "turn {turn} must be admitted after the previous foreground result; value={} output={}",
            reply.value, reply.output
        );
        after = cli.await_terminal(&confirmed, after, &submission).1;
        assert!(cli.native.join(format!("turn-{turn}.effect")).exists());
    }
    // A held observation waits for native output without blocking submission: the next
    // turn is admitted while it is held, and its output releases the page before the wait ends.
    const HOLD_MILLIS: u32 = 25_000;
    let (held, elapsed) = std::thread::scope(|scope| {
        let observer = scope.spawn(|| {
            let started = Instant::now();
            let page = cli.observe_waiting(&confirmed, after.as_deref(), HOLD_MILLIS);
            (page, started.elapsed())
        });
        std::thread::sleep(Duration::from_secs(8));
        let reply = cli.send_turn(
            &confirmed,
            "turn-3",
            "submit-3",
            "55555555-5555-4555-8555-000000000003",
        );
        assert_eq!(
            reply.value["kind"]["kind"], "receipt",
            "submission must not queue behind a held observation; value={} output={}",
            reply.value, reply.output
        );
        observer.join().unwrap()
    });
    assert!(
        held["observations"]
            .as_array()
            .is_some_and(|items| !items.is_empty()),
        "native output must release the held page; page={held}"
    );
    assert!(
        elapsed < Duration::from_millis(u64::from(HOLD_MILLIS)),
        "held page returned only at its deadline ({elapsed:?})"
    );
    let held_terminal = held["observations"].as_array().unwrap().iter().any(|item| {
        item["kind"]["event"]["event"] == "submission_terminal"
            && item["kind"]["event"]["attempt"]["submission_id"] == "submit-3"
    });
    if !held_terminal {
        let _ = cli.await_terminal(
            &confirmed,
            held["next_after"].as_str().map(str::to_owned),
            "submit-3",
        );
    }
    assert!(cli.native.join("turn-3.effect").exists());
    assert!(!cli.native.join("unexpected-user.rejected").exists());
    cli.stop();
}

#[test]
fn ordinary_child_managed_start_uses_actual_parentage_boundary() {
    let cli = Cli::new("cancel");
    let _ = cli.open(SESSION, None);
    let binding = cli.open(CHILD, Some(SESSION));
    assert_eq!(binding["session"]["session_id"], CHILD);
    let children = cli.run(
        &cli.config,
        &["session", "children", SESSION, "--limit", "2", "--json"],
    );
    assert!(children.ok);
    let items = children.value["children"].as_array().unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["id"], CHILD);
    assert_eq!(items[0]["parent_id"], SESSION);
    // Retrying as a top-level open must never silently reparent this session.
    let conflicting_parent = cli.call(
        "start",
        json!({"operation":"open","request":{
        "open_id":format!("open-{CHILD}"),"session_id":CHILD,"generation":1,
        "working_directory":cli.root.path(),"resume":null}}),
        CHILD,
        None,
    );
    if conflicting_parent.ok {
        let retained = cli.run(
            &cli.config,
            &["session", "children", SESSION, "--limit", "2", "--json"],
        );
        assert!(retained.ok);
        assert_eq!(retained.value["children"][0]["parent_id"], SESSION);
    }
}

#[test]
fn lost_native_ack_restart_reconciles_without_second_effect() {
    let cli = Cli::new("lost_ack");
    let binding = cli.open(SESSION, None);
    let first = cli.send(&binding, "fixture-only");
    assert!(
        first.ok,
        "initial delivery must have canonical receipt evidence"
    );
    assert_eq!(first.value["kind"]["kind"], "receipt");
    let deadline = Instant::now() + Duration::from_secs(10);
    while !cli.native.join("submit.effect").exists() {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(10));
    }
    cli.stop();
    let reconciled = cli.reconcile(&binding);
    // Unavailable/uncertain is honest when native process is gone; not-applied
    // is not established merely by a restart or missing native cache entry.
    if reconciled.ok {
        assert_eq!(reconciled.value["kind"]["kind"], "reconciled");
        assert!(
            matches!(
                reconciled.value["kind"]["response"]["state"].as_str(),
                Some("known" | "known_prepared" | "uncertain")
            ),
            "restart did not establish no-effect proof"
        );
    }
    let retry = cli.send(&binding, "fixture-only");
    assert!(
        retry.ok,
        "an identical recorded submission must return canonical evidence without native delivery; value={} output={}",
        retry.value, retry.output
    );
    assert_eq!(
        retry.value, first.value,
        "retained receipt must survive daemon restart"
    );
    assert!(!cli.native.join("submit.duplicate").exists());
    assert_eq!(
        std::fs::read(cli.native.join("submit.effect")).unwrap(),
        b"observed\n"
    );
}

#[test]
fn prepared_transport_can_be_interrupted_without_inventing_native_identity() {
    let cli = Cli::new("cancel");
    let prepared = cli.open(SESSION, None);
    let observed = cli.observe(&prepared, None);
    assert!(
        !observed["observations"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["kind"]["kind"] == "binding_confirmed")
    );
    let cancelled = cli.call(
        "cancel",
        json!({"operation":"interrupt","request":{
        "target":prepared,"mutation_id":"prepared-interrupt"}}),
        SESSION,
        None,
    );
    assert!(cancelled.ok);
    assert_eq!(cancelled.value["kind"]["kind"], "acknowledged");
    assert!(cli.native.join("interrupt.effect").exists());
    let closed = cli.call(
        "close",
        json!({"operation":"close","request":{
        "target":prepared,"mutation_id":"prepared-close"}}),
        SESSION,
        None,
    );
    assert!(closed.ok);
    assert_eq!(closed.value["kind"]["kind"], "acknowledged");
    assert!(!cli.native.join("submit.effect").exists());
    assert!(!cli.native.join("binding-confirmed.emitted").exists());
}

#[test]
fn process_loss_before_native_confirmation_does_not_promote_or_recreate_prepared_transport() {
    let cli = Cli::new("lost_before_confirmation");
    let prepared = cli.open(SESSION, None);
    let first = cli.send(&prepared, "fixture-only");
    let deadline = Instant::now() + Duration::from_secs(10);
    while !cli.native.join("submit.effect").exists() {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(!cli.native.join("binding-confirmed.emitted").exists());
    let launches_before = std::fs::read_dir(&cli.native)
        .unwrap()
        .filter_map(Result::ok)
        .filter(|item| item.file_name().to_string_lossy().starts_with("launch-"))
        .count();
    cli.stop();
    let reconciled = cli.reconcile(&prepared);
    if reconciled.ok {
        assert_eq!(reconciled.value["kind"]["kind"], "reconciled");
        assert_eq!(reconciled.value["kind"]["response"]["state"], "uncertain");
    }
    let retry = cli.send(&prepared, "fixture-only");
    if retry.ok {
        assert_eq!(retry.value["kind"]["kind"], "receipt");
        assert!(
            matches!(
                retry.value["kind"]["response"]["state"].as_str(),
                Some("delivery_attempted" | "uncertain")
            ),
            "no native acceptance proof exists"
        );
        if first.ok {
            assert_eq!(retry.value, first.value);
        }
    }
    // Even a fresh business ID is not authority to recreate a dead Prepared
    // transport. The request remains tied to the exact original live owner.
    let fresh = cli.call(
        "send",
        json!({"operation":"submit","request":{
        "target":prepared,"submission":{"submission_id":"not-replay-authority",
        "attempt_id":"new-attempt","session_id":SESSION,"generation":1,
        "text":"fixture-only","placement":{"kind":"next_turn"}}}}),
        SESSION,
        None,
    );
    assert!(!fresh.ok || fresh.value["kind"]["kind"] == "failure");
    assert!(!cli.native.join("submit.duplicate").exists());
    assert!(!cli.native.join("binding-confirmed.emitted").exists());
    let launches_after = std::fs::read_dir(&cli.native)
        .unwrap()
        .filter_map(Result::ok)
        .filter(|item| item.file_name().to_string_lossy().starts_with("launch-"))
        .count();
    assert_eq!(
        launches_after, launches_before,
        "Prepared cannot recreate native transport after process loss"
    );
    let history = cli.run(
        &cli.config,
        &["harness", "history", SESSION, "--limit", "16"],
    );
    assert!(history.ok);
    for item in history.value["observations"].as_array().unwrap() {
        assert_ne!(item["kind"]["kind"], "binding_confirmed");
    }
}

#[test]
fn sensitive_native_failure_does_not_enter_public_error_or_history() {
    let cli = Cli::new("sensitive_error");
    let binding = cli.open(SESSION, None);
    let result = cli.send(&binding, "fixture-only");
    assert!(!result.output.contains(SECRET));
    // Observe may correctly fail after the injected process loss. Neither path
    // may retain raw native diagnostics as canonical conversation content.
    let observed = cli.call(
        "observe",
        json!({"operation":"observe","request":{
        "target":binding,"after":null,"limit":16,"wait_millis":0}}),
        SESSION,
        None,
    );
    assert!(!observed.output.contains(SECRET));
    cli.stop();
    let history = cli.run(
        &cli.offline,
        &["harness", "history", SESSION, "--limit", "16"],
    );
    assert!(history.ok);
    assert!(!history.output.contains(SECRET));
}

#[test]
fn daemon_auth_status_is_sanitized_and_missing_native_admission_blocks_shared_begin() {
    let cli = Cli::new("cancel");
    let _ = cli.open(SESSION, None);
    let _ = cli.open(CHILD, Some(SESSION));
    let before = cli.run(
        &cli.config,
        &["harness", "history", SESSION, "--limit", "16"],
    );
    assert!(before.ok);
    for session_id in [SESSION, CHILD] {
        let ready = cli.auth("status", session_id, false);
        assert!(
            ready.ok,
            "native status inspection failed; value={} output={}",
            ready.value, ready.output
        );
        assert_eq!(ready.value["readiness"], "ready");
        assert_eq!(ready.value["custody"], "native_managed");
        assert_eq!(ready.value["namespace"]["plugin_id"], "bcode.claude-code");
        assert_eq!(
            ready.value["namespace"]["credential_namespace"],
            cli.native.to_str().unwrap()
        );
        assert!(ready.value.get("diagnostic").is_none());
        assert!(ready.value.get("handle").is_none());
    }
    // Readiness is deliberately independent of proof that no native login is
    // active. A supported status output supplies no method/inactivity evidence.
    std::fs::write(cli.native.join("auth-login-required"), "expired-fixture").unwrap();
    for session_id in [SESSION, CHILD] {
        let expired = cli.auth("status", session_id, false);
        assert!(
            expired.ok,
            "expired status inspection failed: {}",
            expired.output
        );
        assert_eq!(expired.value["readiness"], "login_required");
        let begin = cli.auth("begin", session_id, false);
        assert!(
            !begin.ok,
            "missing native inactivity evidence must not authorize fresh login; value={}",
            begin.value
        );
        assert!(begin.value.get("handle").is_none());
        let cancel = cli.auth("cancel", session_id, false);
        assert!(!cancel.ok, "no live admitted auth handle exists to cancel");
        let sensitive = cli.auth("status", session_id, true);
        assert!(
            !sensitive.ok,
            "ordinary CLI stdout must reject dedicated sensitive-interaction requests"
        );
    }
    assert!(!cli.native.join("unexpected-auth-login").exists());
    let after = cli.run(
        &cli.config,
        &["harness", "history", SESSION, "--limit", "16"],
    );
    assert!(after.ok);
    assert_eq!(
        before.value, after.value,
        "transient auth inspection/rejection must not become canonical history"
    );
    // Restart re-inspects native readiness, not a persisted callback or prior
    // daemon registry. No successful flow or credential reconciliation is faked.
    cli.stop();
    let restarted = cli.auth("status", SESSION, false);
    assert!(restarted.ok, "restart status failed: {}", restarted.output);
    assert_eq!(restarted.value["readiness"], "login_required");
    assert!(!cli.auth("begin", SESSION, false).ok);
    assert!(!cli.native.join("unexpected-auth-login").exists());
}

#[cfg(all(unix, feature = "daemon-client"))]
#[test]
fn client_terminal_handoff_is_offline_one_shot_and_history_clean() {
    const PROBE: &str = "BCODE_HARNESS_TERMINAL_PROBE";
    if std::env::var_os(PROBE).is_some() {
        install_terminal_probe_panic_hook();
        tokio::runtime::Runtime::new().unwrap().block_on(async {
            tokio::time::timeout(Duration::from_secs(90), terminal_claim_probe())
                .await
                .expect("same-socket claim probe exceeded bounded deadline");
        });
        return;
    }
    let cli = Cli::new("terminal_auth");
    let _ = cli.open(SESSION, None);
    let _ = cli.open(CHILD, Some(SESSION));
    // Native stream startup requires locally ready auth before initialize.
    // Expire only after both actual transports have negotiated Prepared; the
    // terminal handoff then restores readiness without fabricating initialization.
    std::fs::write(cli.native.join("auth-login-required"), "offline-expired").unwrap();
    let before = cli.run(
        &cli.config,
        &["harness", "history", SESSION, "--limit", "16"],
    );
    assert!(before.ok);
    let before_child = cli.run(&cli.config, &["harness", "history", CHILD, "--limit", "16"]);
    assert!(before_child.ok);
    let status = cli.auth("status", SESSION, false);
    assert!(status.ok);
    assert_eq!(status.value["readiness"], "login_required");
    let request = cli.root.path().join("terminal-request.json");
    std::fs::write(
        &request,
        serde_json::to_vec(&json!({
            "session_id":SESSION,"instance_id":"offline-claude",
            "interface_id":"bcode.claude-code.handoff/v1"
        }))
        .unwrap(),
    )
    .unwrap();
    // The shipped CLI requires both explicit consent and a real client terminal.
    // A bounded PTY driver observes native output but exports no transcript.
    let wrapper =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/harness/terminal_auth.py");
    let handoff = cli.run_program(
        &cli.config,
        &[
            "harness",
            "auth",
            "handoff",
            "--request",
            request.to_str().unwrap(),
            "--execute",
        ],
        Some(&wrapper),
    );
    assert!(
        handoff.ok,
        "dedicated offline terminal failed: {}",
        handoff.output
    );
    assert_eq!(
        handoff.value,
        json!({"terminal_ready":true,"duplicate_rejected":true,
        "native_executions":1,"dedicated_output_seen":true})
    );
    let ready = cli.auth("status", SESSION, false);
    assert!(ready.ok);
    assert_eq!(ready.value["readiness"], "ready");
    // Separate process isolates config/state globals and verifies the actual
    // Cargo-built daemon target rather than claiming this test binary's identity.
    std::fs::write(cli.native.join("auth-login-required"), "offline-expired").unwrap();
    // Reap removed the first terminal's fence. A fresh daemon must inspect the
    // fixture again, not revive the acknowledged flow's terminal registry entry.
    cli.stop();
    let expired = cli.auth("status", SESSION, false);
    assert!(expired.ok);
    assert_eq!(expired.value["readiness"], "login_required");
    // Readonly inspection above does not reacquire the old daemon's prepared
    // transport. Create distinct current-daemon sessions, never replay old opens.
    std::fs::remove_file(cli.native.join("auth-login-required")).unwrap();
    let _ = cli.open(PROBE_SESSION, None);
    let _ = cli.open(PROBE_CHILD, Some(PROBE_SESSION));
    std::fs::write(cli.native.join("auth-login-required"), "offline-expired").unwrap();
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "client_terminal_handoff_is_offline_one_shot_and_history_clean",
        ])
        .env_clear()
        .env(PROBE, "1")
        .env("BCODE_HARNESS_FIXTURE_PYTHON", fixture_python::python())
        .env("HOME", cli.root.path())
        .env("BCODE_STATE_DIR", cli.root.path().join("state"))
        .env("BCODE_CONFIG", &cli.config)
        .env("XDG_CONFIG_HOME", cli.root.path().join("config"))
        .env("XDG_DATA_HOME", cli.root.path().join("data"))
        .env("PATH", "/usr/bin:/bin")
        .current_dir(cli.root.path())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(94);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            let mut diagnostics = String::new();
            if let Ok(file) = std::fs::File::open(cli.root.path().join("terminal-probe-stages.txt"))
            {
                file.take(8192).read_to_string(&mut diagnostics).unwrap();
            }
            assert!(
                !diagnostics.contains(SECRET),
                "probe diagnostics must be secret-safe"
            );
            eprintln!(
                "same-socket probe exit={:?}\n{}",
                status.code(),
                diagnostics
            );
            assert!(
                status.success(),
                "same-socket one-plan/replay probe failed (payloads withheld)"
            );
            break;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("same-socket one-plan/replay probe exceeded watchdog");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let after = cli.run(
        &cli.config,
        &["harness", "history", SESSION, "--limit", "16"],
    );
    assert!(after.ok);
    assert_eq!(
        before.value, after.value,
        "terminal auth must not become canonical history"
    );
    assert!(!after.output.contains("handoff:"));
    assert!(!after.output.contains("interaction_id"));
    let after_child = cli.run(&cli.config, &["harness", "history", CHILD, "--limit", "16"]);
    assert!(after_child.ok);
    assert_eq!(
        before_child.value, after_child.value,
        "shared waiter auth must not become canonical history"
    );
    assert!(!cli.native.join("terminal-login.duplicate").exists());
}

#[cfg(all(unix, feature = "daemon-client"))]
fn install_terminal_probe_panic_hook() {
    // Preserve only numeric source location, never panic text or payload-bearing values.
    std::panic::set_hook(Box::new(|panic| {
        use std::io::Write as _;
        let Ok(root) = std::env::current_dir() else {
            return;
        };
        let path = root.join("terminal-probe-stages.txt");
        if let Ok(mut file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            && file.metadata().is_ok_and(|metadata| metadata.len() < 8192)
        {
            let line = panic.location().map_or(0, |location| location.line());
            let _ = writeln!(file, "probe:panic_line:{line}");
        }
    }));
}

#[cfg(all(unix, feature = "daemon-client"))]
fn terminal_probe_stage(stage: &str) {
    use std::io::Write as _;
    // Only source-authored labels enter this bounded sidecar. Child stdout,
    // panic output, error messages, handles and plans remain suppressed.
    let path = std::env::current_dir()
        .unwrap()
        .join("terminal-probe-stages.txt");
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .unwrap();
    assert!(
        file.metadata().unwrap().len() < 8192,
        "probe diagnostic bound"
    );
    writeln!(file, "{stage}").unwrap();
}

#[cfg(all(unix, feature = "daemon-client"))]
fn terminal_probe_error(error: &bcode_client::ClientError) {
    use bcode_client::ClientError as E;
    let label = match error {
        E::Server { code, .. } => match code.as_str() {
            "managed_session_uncertain" => "error:managed_session_uncertain",
            "managed_session_conflict" => "error:managed_session_conflict",
            "managed_session_stale_owner" => "error:managed_session_stale_owner",
            "managed_session_invalid" => "error:managed_session_invalid",
            "managed_session_unavailable" => "error:managed_session_unavailable",
            "managed_session_deadline" => "error:managed_session_deadline",
            "managed_session_storage_unavailable" => "error:managed_session_storage_unavailable",
            "managed_session_scope_mismatch" => "error:managed_session_scope_mismatch",
            "managed_session_sensitive_transport_required" => {
                "error:managed_session_sensitive_transport_required"
            }
            "managed_session_unsupported" => "error:managed_session_unsupported",
            "managed_session_continue_issue_uncertain" => "error:continue_issue_uncertain",
            "managed_session_continue_delivery_uncertain" => "error:continue_delivery_uncertain",
            "managed_session_ack_stale" => "error:ack_stale",
            "managed_session_ack_not_found" => "error:ack_not_found",
            "managed_session_ack_not_settled" => "error:ack_not_settled",
            _ => "error:server_other",
        },
        E::Transport(_) => "error:transport",
        E::Codec(_) => "error:codec",
        E::Protocol(_) => "error:protocol",
        E::IncompatibleDaemon { .. } => "error:incompatible_daemon",
        E::ConnectTimeout { .. } => "error:connect_timeout",
        E::RequestTimeout { .. } => "error:request_timeout",
        E::UnexpectedResponse => "error:unexpected_response",
        E::UnexpectedEnvelope => "error:unexpected_envelope",
        _ => "error:client_other",
    };
    terminal_probe_stage(label);
}

#[cfg(all(unix, feature = "daemon-client"))]
async fn terminal_auth_call(
    client: &mut bcode_client::ClientConnection,
    operation: bcode_ipc::harness::ManagedSessionOperation,
    value: &Value,
) -> Result<Value, bcode_client::ClientError> {
    let response = client
        .managed_session_auth(bcode_ipc::harness::ManagedSessionRequest {
            schema_version: bcode_ipc::harness::MANAGED_SESSION_VERSION,
            operation,
            request_json: serde_json::to_string(value).unwrap(),
        })
        .await
        .inspect_err(terminal_probe_error)?;
    let value: Value = serde_json::from_str(&response.response_json)
        .map_err(|_| bcode_client::ClientError::Protocol("invalid auth response".into()))?;
    terminal_probe_stage(if value.get("handle").is_some() {
        "response:flow_snapshot"
    } else if value.get("executable").is_some() {
        "response:native_plan"
    } else if value.is_array() {
        "response:interaction"
    } else if value.is_null() {
        "response:no_interaction"
    } else {
        "response:other"
    });
    Ok(value)
}

#[cfg(all(unix, feature = "daemon-client"))]
async fn diagnose_terminal_identity(executable: &Path, expected: Option<&bcode_ipc::ArtifactId>) {
    use tokio::io::AsyncReadExt as _;
    terminal_probe_stage("identity_diagnostic:start");
    let started = Instant::now();
    let child = tokio::process::Command::new(executable)
        .arg("artifact-id")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn();
    let Ok(mut child) = child else {
        terminal_probe_stage("identity_diagnostic:spawn_failed");
        return;
    };
    let Some(mut stdout) = child.stdout.take() else {
        terminal_probe_stage("identity_diagnostic:stdout_missing");
        let _ = child.kill().await;
        let _ = child.wait().await;
        return;
    };
    // Diagnostic only: distinguish an identity that arrives after the verifier's
    // five-second bound. Never use this result to construct or admit a target.
    let mut bytes = Vec::new();
    let result = tokio::time::timeout(Duration::from_secs(12), async {
        (&mut stdout).take(257).read_to_end(&mut bytes).await?;
        if bytes.len() > 256 {
            return Ok::<_, std::io::Error>(None);
        }
        child.wait().await.map(Some)
    })
    .await;
    terminal_probe_stage(if started.elapsed() > Duration::from_secs(5) {
        "identity_diagnostic:exceeded_verifier_5s"
    } else {
        "identity_diagnostic:within_verifier_5s"
    });
    match result {
        Err(_) => terminal_probe_stage("identity_diagnostic:timeout_12s"),
        Ok(Err(_)) => terminal_probe_stage("identity_diagnostic:io_failed"),
        Ok(Ok(None)) => terminal_probe_stage("identity_diagnostic:output_exceeds_256"),
        Ok(Ok(Some(status))) if !status.success() => {
            terminal_probe_stage("identity_diagnostic:exit_unsuccessful");
        }
        Ok(Ok(Some(_))) => match std::str::from_utf8(&bytes) {
            Err(_) => terminal_probe_stage("identity_diagnostic:non_utf8"),
            Ok(text) => match text.strip_suffix('\n') {
                None => terminal_probe_stage("identity_diagnostic:missing_newline"),
                Some(id) if id.contains(['\r', '\n']) => {
                    terminal_probe_stage("identity_diagnostic:multiple_lines");
                }
                Some(id) => match bcode_ipc::ArtifactId::parse(id) {
                    Err(_) => terminal_probe_stage("identity_diagnostic:invalid_artifact_id"),
                    Ok(id) => terminal_probe_stage(if expected == Some(&id) {
                        "identity_diagnostic:valid_matches_advertisement"
                    } else {
                        "identity_diagnostic:valid_advertisement_mismatch"
                    }),
                },
            },
        },
    }
    // Even on oversized output or timeout, reap this exact diagnostic process.
    if child.try_wait().ok().flatten().is_none() {
        let _ = child.kill().await;
    }
    let _ = child.wait().await;
}

#[cfg(all(unix, feature = "daemon-client"))]
async fn terminal_claim_probe() {
    use bcode_ipc::harness::ManagedSessionOperation as Op;
    terminal_probe_stage("setup:config");
    let root = std::env::current_dir().unwrap();
    let config = bcode_config::load_config_from_paths(&[root.join("online.toml")]).unwrap();
    let location = bcode_config::resolve_state_location_set(
        &config.state,
        &bcode_config::StateLocationSelection {
            root: Some(root.join("state")),
            profile: None,
        },
    )
    .unwrap()
    .primary()
    .clone();
    terminal_probe_stage("setup:executable_digest");
    let fixture_executable = PathBuf::from(env!("CARGO_BIN_EXE_bcode"));
    let digest = bcode_daemon_lifecycle::executable_sha256(&fixture_executable).unwrap();
    terminal_probe_stage("setup:canonical_daemon_executable");
    // Registry paths are discovery candidates, never authority. Select only
    // this fixture's exact bytes/scope, then let DaemonTarget independently
    // probe executable identity and verify the serving daemon's live handshake.
    let scope =
        bcode_config::runtime_scope_id(location.root(), &bcode_config::default_config_dir());
    let mut candidates = bcode_daemon_lifecycle::read_records_strict(location.root(), 64)
        .unwrap()
        .into_iter()
        .filter(|record| {
            record.executable_digest.as_deref() == Some(digest.as_str())
                && record.state_location_id.as_deref() == Some(scope.as_str())
        });
    let record = candidates
        .next()
        .expect("exact fixture daemon discovery candidate required");
    assert!(
        candidates.next().is_none(),
        "ambiguous fixture daemon discovery"
    );
    let advertised_artifact = record.artifact_id.clone();
    let executable = record
        .executable_path
        .expect("fixture daemon executable path required")
        .canonicalize()
        .expect("fixture daemon executable must exist");
    assert!(
        executable.is_absolute(),
        "fixture daemon executable must be absolute"
    );
    let images = root
        .join("state")
        .join("daemon-images")
        .canonicalize()
        .expect("fixture daemon image root must exist");
    assert!(
        executable.starts_with(&images),
        "daemon candidate must remain inside fixture image root"
    );
    assert!(
        bcode_daemon_lifecycle::executable_sha256(&executable).unwrap() == digest,
        "serving executable must equal selected fixture bytes"
    );
    let config_directory = bcode_config::default_config_dir();
    // An explicit BCODE_CONFIG file does not necessarily create the XDG
    // configuration directory. Target verification requires that real directory;
    // create it in this isolated fixture, preserving the exact scope/path.
    terminal_probe_stage("setup:config_directory");
    std::fs::create_dir_all(&config_directory).unwrap();
    terminal_probe_stage("setup:verify_target");
    let verification = bcode_daemon_lifecycle::DaemonTarget::verify(
        executable.clone(),
        digest,
        location,
        config_directory,
    )
    .await;
    if matches!(
        &verification,
        Err(bcode_daemon_lifecycle::DaemonTargetError::IdentityUnavailable)
    ) {
        diagnose_terminal_identity(&executable, advertised_artifact.as_ref()).await;
    }
    // Diagnostic evidence never substitutes for successful canonical verification.
    let target = verification
        .inspect_err(|error| {
            use bcode_daemon_lifecycle::DaemonTargetError as E;
            terminal_probe_stage(match error {
                E::InvalidSelection => "error:target_invalid_selection",
                E::IdentityUnavailable => "error:target_identity_unavailable",
                E::Unavailable => "error:target_unavailable",
                E::Incompatible => "error:target_incompatible",
            });
        })
        .unwrap();
    terminal_probe_stage("setup:client");
    let client = bcode_client::BcodeClient::for_daemon(target, &config, root)
        .inspect_err(terminal_probe_error)
        .unwrap();
    terminal_probe_stage("setup:owner_connect");
    let mut owner = client
        .connect("offline-terminal-owner")
        .await
        .inspect_err(terminal_probe_error)
        .unwrap();
    terminal_probe_stage("setup:waiter_connect");
    let mut waiter = client
        .connect("offline-terminal-waiter")
        .await
        .inspect_err(terminal_probe_error)
        .unwrap();
    terminal_completion_matrix(&mut owner, &mut waiter).await;
    let prepare = json!({"session_id":PROBE_SESSION,"instance_id":"offline-claude",
        "interface_id":"bcode.claude-code.handoff/v1"});
    terminal_probe_stage("owner:prepare");
    let first = terminal_auth_call(&mut owner, Op::AuthPrepare, &prepare)
        .await
        .unwrap();
    terminal_probe_stage("owner:repeat_prepare");
    let retained = terminal_auth_call(&mut owner, Op::AuthPrepare, &prepare)
        .await
        .unwrap();
    assert!(
        first["handle"] == retained["handle"],
        "repeat prepare reserves one namespace flow (payloads withheld)"
    );
    let mut shared = prepare.clone();
    shared["session_id"] = json!(PROBE_CHILD);
    terminal_probe_stage("waiter:prepare");
    let joined = terminal_auth_call(&mut waiter, Op::AuthPrepare, &shared)
        .await
        .unwrap();
    assert!(
        first["handle"] == joined["handle"],
        "same namespace must share reservation (payloads withheld)"
    );
    let mut selection = prepare;
    selection["handle"] = first["handle"].clone();
    selection["interaction_id"] = json!("");
    terminal_probe_stage("owner:interaction");
    let interaction = terminal_auth_call(&mut owner, Op::AuthStatus, &selection)
        .await
        .unwrap();
    selection["interaction_id"] = interaction[1]["request_id"].clone();
    assert!(
        selection["interaction_id"]
            .as_str()
            .is_some_and(|id| !id.is_empty())
    );
    terminal_probe_stage("owner:stale_claim_reject");
    let mut stale = selection.clone();
    stale["interaction_id"] = json!("stale-offline-interaction");
    assert!(
        terminal_auth_call(&mut owner, Op::AuthHandoff, &stale)
            .await
            .is_err()
    );
    terminal_probe_stage("owner:claim");
    let plan = terminal_auth_call(&mut owner, Op::AuthHandoff, &selection)
        .await
        .unwrap();
    terminal_probe_stage("owner:validate_plan");
    let validated: bcode_ipc::harness::ManagedNativeLoginPlan =
        serde_json::from_value(plan.clone()).unwrap();
    validated.validate().unwrap();
    assert_eq!(plan["method"]["execution"], "client_terminal");
    assert_eq!(plan["method"]["native_inactivity"], "unknown");
    assert_eq!(plan["expected_version"], "2.1.308");
    assert_eq!(
        plan["executable"],
        json!(std::env::current_dir().unwrap().join("offline-claude"))
    );
    assert_eq!(plan["arguments"], json!(["auth", "login"]));
    assert_eq!(
        plan["namespace"]["credential_namespace"],
        json!(std::env::current_dir().unwrap().join("native"))
    );
    assert_eq!(
        plan["environment"]["CLAUDE_CONFIG_DIR"],
        plan["namespace"]["credential_namespace"]
    );
    assert!(
        terminal_auth_call(&mut owner, Op::AuthContinue, &selection)
            .await
            .is_err(),
        "readiness continuation must wait for exact claimed-child reap"
    );
    terminal_probe_stage("owner:continue_before_reap_reject");
    assert!(
        terminal_auth_call(&mut owner, Op::AuthContinue, &selection)
            .await
            .is_err(),
        "continuation must not bypass exact child reap"
    );
    terminal_probe_stage("owner:repeat_claim_reject");
    assert!(
        terminal_auth_call(&mut owner, Op::AuthHandoff, &selection)
            .await
            .is_err()
    );
    terminal_probe_stage("waiter:claim_reject");
    let mut other_selection = selection.clone();
    other_selection["session_id"] = json!(PROBE_CHILD);
    assert!(
        terminal_auth_call(&mut waiter, Op::AuthHandoff, &other_selection)
            .await
            .is_err()
    );
    terminal_probe_stage("waiter:prepare_after_claim_reject");
    assert!(
        terminal_auth_call(&mut waiter, Op::AuthPrepare, &shared)
            .await
            .is_err()
    );
    // Claim was deliberately NOT executed. Never invent reap/inactivity to
    // clear its durable fence: disconnect leaves ambiguous delivery fail-closed.
    terminal_probe_stage("owner:disconnect");
    drop(owner);
    terminal_probe_stage("replacement:connect");
    let mut replacement = client
        .connect("offline-terminal-replacement")
        .await
        .inspect_err(terminal_probe_error)
        .unwrap();
    terminal_probe_stage("replacement:claim_reject");
    assert!(
        terminal_auth_call(&mut replacement, Op::AuthHandoff, &selection)
            .await
            .is_err()
    );
    terminal_probe_stage("replacement:prepare_reject");
    assert!(
        terminal_auth_call(&mut replacement, Op::AuthPrepare, &shared)
            .await
            .is_err()
    );
    terminal_probe_stage("complete");
}

#[cfg(all(unix, feature = "daemon-client"))]
async fn terminal_completion_matrix(
    owner: &mut bcode_client::ClientConnection,
    foreign: &mut bcode_client::ClientConnection,
) {
    let native = std::env::current_dir().unwrap().join("native");
    assert!(native.join("terminal-login.release").is_file());
    for exit_success in [true, false] {
        for ready in [true, false] {
            terminal_probe_stage(match (exit_success, ready) {
                (true, true) => "completion:success_ready",
                (true, false) => "completion:success_login_required",
                (false, true) => "completion:failure_ready",
                (false, false) => "completion:failure_login_required",
            });
            let case = format!("-completion-{}-{}", u8::from(exit_success), u8::from(ready));
            std::fs::write(native.join("terminal-login.case"), &case).unwrap();
            std::fs::write(
                native.join("terminal-login.exit-code"),
                if exit_success { "0" } else { "7" },
            )
            .unwrap();
            std::fs::write(native.join("auth-login-required"), "offline-expired").unwrap();
            terminal_completion_case(owner, foreign, &native, exit_success, ready).await;
            assert!(
                native
                    .join(format!("terminal-login{case}.effect"))
                    .is_file()
            );
            assert!(
                !native
                    .join(format!("terminal-login{case}.duplicate"))
                    .exists()
            );
        }
    }
    std::fs::remove_file(native.join("terminal-login.case")).unwrap();
    std::fs::remove_file(native.join("terminal-login.exit-code")).unwrap();
    // Preserve the original unexecuted-claim recovery probe's login-required premise.
    std::fs::write(native.join("auth-login-required"), "offline-expired").unwrap();
}

#[cfg(all(unix, feature = "daemon-client"))]
async fn terminal_completion_case(
    owner: &mut bcode_client::ClientConnection,
    foreign: &mut bcode_client::ClientConnection,
    native: &Path,
    exit_success: bool,
    ready: bool,
) {
    use bcode_ipc::harness::ManagedSessionOperation as Op;
    let prepare = json!({"session_id":PROBE_SESSION,"instance_id":"offline-claude",
        "interface_id":"bcode.claude-code.handoff/v1"});
    let first = terminal_auth_call(owner, Op::AuthPrepare, &prepare)
        .await
        .unwrap();
    let mut selection = prepare;
    selection["handle"] = first["handle"].clone();
    selection["interaction_id"] = json!("");
    let interaction = terminal_auth_call(owner, Op::AuthStatus, &selection)
        .await
        .unwrap();
    selection["interaction_id"] = interaction[1]["request_id"].clone();
    let plan = terminal_auth_call(owner, Op::AuthHandoff, &selection)
        .await
        .unwrap();
    let pinned = selection.clone();
    terminal_probe_stage("completion:claim_denials");
    assert_terminal_claim_denials(owner, foreign, &selection).await;
    terminal_probe_stage("completion:execute_plan");
    let status = execute_terminal_fixture_plan(plan, native).await;
    terminal_probe_stage("completion:physical_reap_returned");
    assert_eq!(status.code(), Some(if exit_success { 0 } else { 7 }));
    // Change only the offline producer's local readiness, not exit evidence or the claim.
    // Even a failed child can leave locally usable configuration; success need not do so.
    if !ready {
        std::fs::write(native.join("auth-login-required"), "offline-expired").unwrap();
    }
    assert_eq!(selection, pinned);
    finish_terminal_fixture_claim(owner, &selection, native, ready).await;
    assert_eq!(selection, pinned);
}

#[cfg(all(unix, feature = "daemon-client"))]
async fn finish_terminal_fixture_claim(
    owner: &mut bcode_client::ClientConnection,
    selection: &Value,
    native: &Path,
    ready: bool,
) {
    use bcode_ipc::harness::ManagedSessionOperation as Op;
    terminal_probe_stage("completion:report_reap");
    let reaped = terminal_auth_call(owner, Op::AuthReaped, selection)
        .await
        .unwrap();
    assert_eq!(reaped, json!({"reaped":true}));
    assert!(reaped.get("acknowledged").is_none());
    terminal_probe_stage("completion:readiness_continue");
    let inspected = terminal_auth_call(owner, Op::AuthContinue, selection)
        .await
        .unwrap();
    assert_eq!(
        inspected["readiness"],
        if ready { "ready" } else { "login_required" }
    );
    assert_eq!(
        inspected["phase"],
        if ready { "settled" } else { "pending" }
    );
    if !ready {
        assert!(
            terminal_auth_call(owner, Op::AuthAcknowledge, selection)
                .await
                .is_err()
        );
        // Explicitly reinspect changed fixture readiness; never relaunch the claimed child.
        std::fs::remove_file(native.join("auth-login-required")).unwrap();
        let settled = terminal_auth_call(owner, Op::AuthContinue, selection)
            .await
            .unwrap();
        assert_eq!(settled["readiness"], "ready");
        assert_eq!(settled["phase"], "settled");
    }
    terminal_probe_stage("completion:acknowledge");
    let acknowledged = terminal_auth_call(owner, Op::AuthAcknowledge, selection)
        .await
        .unwrap();
    assert_eq!(acknowledged, json!({"acknowledged":true}));
    assert!(
        terminal_auth_call(owner, Op::AuthReaped, selection)
            .await
            .is_err()
    );
    assert!(
        terminal_auth_call(owner, Op::AuthContinue, selection)
            .await
            .is_err()
    );
}

#[cfg(all(unix, feature = "daemon-client"))]
async fn assert_terminal_claim_denials(
    owner: &mut bcode_client::ClientConnection,
    foreign: &mut bcode_client::ClientConnection,
    selection: &Value,
) {
    use bcode_ipc::harness::ManagedSessionOperation as Op;
    assert!(
        terminal_auth_call(owner, Op::AuthContinue, selection)
            .await
            .is_err()
    );
    assert!(
        terminal_auth_call(foreign, Op::AuthReaped, selection)
            .await
            .is_err()
    );
    let mut stale = selection.clone();
    stale["handle"]["daemon_epoch"] = json!("stale-offline-epoch");
    for operation in [Op::AuthReaped, Op::AuthContinue, Op::AuthAcknowledge] {
        assert!(terminal_auth_call(owner, operation, &stale).await.is_err());
    }
}

#[cfg(all(unix, feature = "daemon-client"))]
async fn execute_terminal_fixture_plan(plan: Value, native: &Path) -> std::process::ExitStatus {
    terminal_probe_stage("completion:descriptor_decode");
    let plan: bcode_ipc::harness::ManagedNativeLoginPlan = serde_json::from_value(plan).unwrap();
    plan.validate().unwrap();
    terminal_probe_stage("completion:descriptor_verify");
    let digest = verify_terminal_fixture_plan(&plan, native).await;
    terminal_probe_stage("completion:pty_spawn");
    // A PTY supplies the exact returned native plan's terminal. The bounded driver owns
    // and physically waits for that child; no output or secret is returned to the test.
    let driver = r#"
import os, pty, subprocess, sys
master, slave = pty.openpty()
child = None
try:
    child = subprocess.Popen(sys.argv[1:], stdin=slave, stdout=slave, stderr=slave)
    os.close(slave)
    slave = None
    code = child.wait(timeout=10)
finally:
    if child is not None and child.poll() is None:
        child.kill()
        child.wait()
    if slave is not None:
        os.close(slave)
    os.close(master)
sys.exit(code if code >= 0 else 128 - code)
"#;
    terminal_probe_stage("completion:python_resolve");
    let python = fixture_python::python();
    terminal_probe_stage("completion:python_resolved");
    let status = tokio::time::timeout(
        Duration::from_secs(15),
        tokio::process::Command::new(python)
            .args(["-c", driver])
            .arg(&plan.executable)
            .args(&plan.arguments)
            .env_clear()
            .envs(&plan.environment)
            .current_dir(&plan.working_directory)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .status(),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(
        bcode_daemon_lifecycle::executable_sha256(&plan.executable).unwrap(),
        digest
    );
    status
}

#[cfg(all(unix, feature = "daemon-client"))]
async fn verify_terminal_fixture_plan(
    plan: &bcode_ipc::harness::ManagedNativeLoginPlan,
    native: &Path,
) -> String {
    let root = std::env::current_dir().unwrap();
    terminal_probe_stage("completion:descriptor_binding");
    assert_eq!(plan.executable, root.join("offline-claude"));
    assert_eq!(plan.working_directory, native);
    assert_eq!(
        plan.namespace.credential_namespace,
        native.to_str().unwrap()
    );
    assert_eq!(
        plan.environment.get("CLAUDE_CONFIG_DIR").unwrap(),
        native.to_str().unwrap()
    );
    assert_eq!(plan.arguments, ["auth", "login"]);
    assert_eq!(plan.namespace.plugin_id, "bcode.claude-code");
    assert_eq!(plan.method.method_id, "native-login");
    assert!(matches!(
        plan.method.execution,
        bcode_ipc::harness::ManagedNativeLoginExecution::ClientTerminal
    ));
    assert_eq!(plan.expected_version, "2.1.308");
    let digest = bcode_daemon_lifecycle::executable_sha256(&plan.executable).unwrap();
    terminal_probe_stage("completion:version_probe");
    let version = tokio::time::timeout(
        Duration::from_secs(10),
        tokio::process::Command::new(&plan.executable)
            .arg("--version")
            .env_clear()
            .envs(&plan.environment)
            .current_dir(&plan.working_directory)
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .output(),
    )
    .await
    .unwrap()
    .unwrap();
    terminal_probe_stage("completion:version_result");
    assert!(version.status.success());
    assert!(version.stdout.len() <= 4096);
    assert_eq!(
        std::str::from_utf8(&version.stdout).unwrap().trim(),
        plan.expected_version
    );
    assert_eq!(
        bcode_daemon_lifecycle::executable_sha256(&plan.executable).unwrap(),
        digest
    );
    digest
}

#[test]
fn malformed_secret_bearing_auth_and_oversized_requests_fail_without_echo() {
    let cli = Cli::new("cancel");
    let path = cli.root.path().join("sensitive.json");
    std::fs::write(&path, format!("{{\"secret\":\"{SECRET}\"}}")).unwrap();
    let bad = cli.run(
        &cli.config,
        &[
            "harness",
            "auth",
            "continue",
            "--request",
            path.to_str().unwrap(),
        ],
    );
    assert!(!bad.ok);
    assert!(!bad.output.contains(SECRET));
    std::fs::write(&path, vec![b'x'; 1_048_577]).unwrap();
    assert!(
        !cli.run(
            &cli.config,
            &["harness", "send", "--request", path.to_str().unwrap()]
        )
        .ok
    );
}
