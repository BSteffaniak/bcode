//! Exercises the public eval runner against an explicitly selected offline Bcode artifact.
//! Run in a scrubbed environment with `BCODE_QUALITY_EVAL_BINARY` pointing at the validator's
//! exact fresh artifact containing the bundled fake provider. Never resolves a PATH binary.
#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

use bcode_eval::load_suite;
use bcode_eval::{EvalRunOptions, run_suite};
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};

fn find_artifact(root: &Path, name: &str) -> PathBuf {
    for entry in fs::read_dir(root).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            let found = find_artifact_optional(&path, name);
            if let Some(found) = found {
                return found;
            }
        } else if path.file_name().is_some_and(|file| file == name) {
            return path;
        }
    }
    panic!("runner did not produce {name}");
}
fn find_artifact_optional(root: &Path, name: &str) -> Option<PathBuf> {
    for entry in fs::read_dir(root).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            if let Some(found) = find_artifact_optional(&path, name) {
                return Some(found);
            }
        } else if path.file_name().is_some_and(|file| file == name) {
            return Some(path);
        }
    }
    None
}
fn read_json(path: &Path) -> serde_json::Value {
    serde_json::from_slice(&fs::read(path).unwrap()).unwrap()
}

#[test]
#[ignore = "requires an explicitly selected fresh offline Bcode artifact; run serially in scrubbed environment"]
fn workflow_eval_runner_preserves_oracle_across_five_optional_arms() {
    let binary = PathBuf::from(
        std::env::var_os("BCODE_QUALITY_EVAL_BINARY").expect("explicit offline artifact required"),
    );
    assert!(binary.is_absolute() && binary.is_file());
    let root = tempfile::Builder::new()
        .prefix("automatic-quality-runner-")
        .tempdir_in("/Volumes/ehdd/bcode-eval-artifacts/tmp")
        .expect("authorized external fixture artifact directory")
        .keep();
    println!(
        "retained offline eval fixture artifacts: {}",
        root.display()
    );
    let fixture = root.join("fixture");
    fs::create_dir(&fixture).unwrap();
    fs::write(fixture.join("answer.txt"), "wrong answer\n").unwrap();
    let mut baseline_labels = None;
    let mut baseline_facts = None;
    for arm in [
        "global_off",
        "feature_off",
        "auto_no_key",
        "auto_error",
        "auto_eligible",
    ] {
        let suite_path = write_arm_suite(&root, &fixture, &binary, arm);
        let mut options = EvalRunOptions::new(suite_path);
        options.output_root = root.join("runs");
        options.run_id = Some(arm.into());
        let result = run_suite(&options).expect("actual runner must finish and capture artifacts");
        let directory = options.output_root.join(arm);
        assert_runner_artifacts(
            &directory,
            &fixture,
            arm,
            &mut baseline_labels,
            &mut baseline_facts,
        );
        assert_eq!(
            result.variants[0].cases[0].repetitions[0].exit_code,
            Some(1)
        );
    }
}

fn write_arm_suite(root: &Path, fixture: &Path, binary: &Path, arm: &str) -> PathBuf {
    let mut settings = BTreeMap::new();
    if arm != "auto_no_key" {
        settings.insert("env.QUALITY_OFFLINE_TOKEN", "offline-test-only".to_owned());
    }
    if arm == "auto_error" {
        settings.insert(
            "env.FAKE_JUDGEMENT_YES_NO_PROBABILITIES",
            "not-json".to_owned(),
        );
    }
    let settings_toml = toml::to_string(&settings).expect("string-only offline auth settings");
    let config = format!(
        r#"
[plugins]
enabled = ["bcode.fake-provider", "bcode.filesystem", "bcode.default-agents", "bcode.loop"]
[model]
provider_plugin_id = "bcode.fake-provider"
model_id = "fake-echo"
[model.settings]
fake_structured_output_json = "loop_until:1"
[model.prompt_cache]
mode = "off"
[judgement]
mode = "{}"
eval_categorization = "{}"
tool_result_triage = "off"
context_reranking = "off"
progress_monitoring = "off"
recommendations = "off"
session_categorization = "off"
[judgement.selection]
provider_plugin_id = "bcode.fake-provider"
model_id = "fake-judgement"
auth_profile = "quality-offline"
[auth.profiles.quality-offline]
backend = "aws"
owner_plugin_id = "bcode.fake-provider"
provider_id = "fake"
scheme = "api_key"
[auth.profiles.quality-offline.map.api_key]
env = "QUALITY_OFFLINE_TOKEN"
[auth.profiles.quality-offline.settings]
{settings_toml}
"#,
        if arm == "global_off" { "off" } else { "auto" },
        if arm == "feature_off" { "off" } else { "auto" }
    );
    let config_path = root.join(format!("{arm}.toml"));
    fs::write(&config_path, config).unwrap();
    let suite_path = root.join(format!("{arm}-suite.toml"));
    let config_path =
        serde_json::to_string(config_path.to_str().expect("fixture config UTF-8")).unwrap();
    let binary = serde_json::to_string(binary.to_str().expect("selected artifact UTF-8")).unwrap();
    let fixture = serde_json::to_string(fixture.to_str().expect("fixture path UTF-8")).unwrap();
    let suite_toml = format!(
        r#"
schema_version = 1
id = "quality-runner"
name = "Offline actual workflow runner"
[run]
repetitions = 1
timeout_ms = 30000
isolation = "temp_copy"
randomize_case_order = false
fail_fast = false
[[variants]]
id = {arm:?}
executor = "workflow"
[variants.env]
BCODE_CONFIG = {config_path}
[variants.metadata.workflow]
binary = {binary}
strategy = "no-delegation"
stop_condition = "The fixture is inspected; return a completion decision."
max_iterations = 1
concurrency = 1
worker_attempts = 1
expected_contains = ["independent expected answer absent from the fixture"]
answer_file = "answer.txt"
preserve_files = ["answer.txt"]
[[cases]]
id = "measured-artifact-failure"
fixture = {fixture}
prompt = "Inspect answer.txt and report what it contains. Do not change it."
[[cases.judges]]
type = "command"
command = "test \"$(cat answer.txt)\" = \"independent expected answer absent from the fixture\""
expected_exit_code = 0
required = true
"#
    );
    fs::write(&suite_path, suite_toml).unwrap();
    load_suite(&suite_path).expect("authored TOML must satisfy the public suite contract");
    suite_path
}

fn assert_degraded_capture_and_completed_model(directory: &Path) -> serde_json::Value {
    let inspection = read_json(&find_artifact(directory, "inspection.json"));
    let unavailable = inspection["unavailable_collections"].clone();
    assert!(
        unavailable
            .as_array()
            .expect("explicit capture availability")
            .contains(&serde_json::json!("events"))
    );
    let history_error = read_json(&find_artifact(directory, "history-capture-error.json"));
    assert_eq!(
        history_error,
        "suite validation failed: incomplete history collections: events"
    );
    let events_error = read_json(&find_artifact(directory, "events-capture-error.json"));
    assert!(
        events_error
            .as_str()
            .expect("normalized capture failure")
            .contains("workflow_unavailable")
    );
    // Session transcripts are bounded runner exports, not reads or repairs of canonical storage.
    let transcript = fs::read_to_string(find_artifact(directory, "transcript-0.jsonl")).unwrap();
    let finished = transcript
        .lines()
        .filter_map(|line| {
            let event: serde_json::Value =
                serde_json::from_str(line).expect("exported session event");
            event["kind"].get("model_turn_finished").cloned()
        })
        .collect::<Vec<_>>();
    assert!(
        !finished.is_empty(),
        "actual ordinary model execution must settle"
    );
    for turn in finished {
        assert_eq!(
            turn["outcome"], "completed",
            "fixture model must not fail: {turn}"
        );
    }
    serde_json::json!({"unavailable_collections":unavailable,"history_error":history_error,"events_error":events_error})
}

fn assert_runner_artifacts(
    directory: &Path,
    fixture: &Path,
    arm: &str,
    baseline_labels: &mut Option<serde_json::Value>,
    baseline_facts: &mut Option<serde_json::Value>,
) {
    if let Some(failure) = find_artifact_optional(directory, "execution-failure.json") {
        let failure = read_json(&failure);
        panic!(
            "actual workflow executor failed before oracle capture; retained artifacts={} normalized failure={failure}",
            directory.display()
        );
    }
    let diagnosis = read_json(&find_artifact(directory, "diagnosis.json"));
    let oracle = read_json(&find_artifact(directory, "oracle.json"));
    assert_eq!(oracle["passed"], false);
    assert_eq!(oracle["artifact_correct"], false);
    assert_eq!(oracle["preservation_passed"], true);
    assert_eq!(oracle["capture_complete"], false);
    let capture_limits = assert_degraded_capture_and_completed_model(directory);
    assert_eq!(oracle["timed_out"], false);
    assert_eq!(
        fs::read(fixture.join("answer.txt")).unwrap(),
        b"wrong answer\n"
    );
    if let Some(labels) = baseline_labels.as_ref() {
        assert_eq!(&diagnosis["labels"], labels);
    } else {
        *baseline_labels = Some(diagnosis["labels"].clone());
    }
    let measured_facts = serde_json::json!({"oracle":oracle,"capture_limits":capture_limits});
    if let Some(facts) = baseline_facts.as_ref() {
        assert_eq!(
            &measured_facts, facts,
            "advice cannot change measured outcome or conceal unavailable capture"
        );
    } else {
        *baseline_facts = Some(measured_facts);
    }
    if arm == "auto_eligible" {
        assert!(
            diagnosis.get("advisory_model_labels").is_some(),
            "eligible real CLI advice must be attached"
        );
    } else {
        assert!(
            diagnosis.get("advisory_model_labels").is_none(),
            "unavailable advice preserves diagnosis"
        );
    }
    println!("actual workflow eval runner {arm}: oracle={oracle} diagnosis={diagnosis}");
}
