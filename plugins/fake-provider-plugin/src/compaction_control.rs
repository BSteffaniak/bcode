//! Opt-in scratch-only controls for deterministic compaction integration tests.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use bcode_model::{ModelTurnRequest, ProviderError};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::{FakeTurn, fake_structured_output_error, finish_fake_text_response, last_user_text};

#[path = "compaction_progressive.rs"]
mod progressive;

const MAX_WAIT_MS: u64 = 300_000;
const REPAIR_SUFFIX: &str = "\n\nRepair: the response was invalid, incomplete, or oversized. Return a shorter valid complete JSON synthesis with supported source references and explicit supersession. Do not clip source coverage.";

pub fn source_prompt(prompt: &str) -> &str {
    let prompt = prompt.strip_suffix(REPAIR_SUFFIX).unwrap_or(prompt);
    // Targeted repair carries diagnostics before the unchanged attributed base prompt.
    // Never let rejected provider text become fixture source evidence.
    if prompt.starts_with("Repair the previous rejected response. Reason: ") {
        return prompt.split_once("\n\n").map_or(prompt, |(_, base)| base);
    }
    prompt
}

struct Control {
    directory: PathBuf,
    pause: bool,
    timeout: Duration,
    outcome: String,
    correction: bool,
    has_carried_scope: bool,
}

impl Control {
    fn prepare(request: &ModelTurnRequest) -> Result<Self, String> {
        Self::prepare_with_prefix(request, "attempt-")
    }

    fn prepare_with_prefix(request: &ModelTurnRequest, prefix: &str) -> Result<Self, String> {
        let settings = &request.provider_context.settings;
        let root = Path::new(&settings["fake_compaction_control_dir"]);
        if !root.is_absolute() || !root.is_dir() {
            return Err(
                "compaction control directory must be an existing absolute scratch directory"
                    .into(),
            );
        }
        let canonical_root = root
            .canonicalize()
            .map_err(|_| "cannot resolve compaction scratch directory")?;
        if canonical_root != root {
            return Err("compaction scratch root must be canonical without symlink aliases".into());
        }
        let root = canonical_root;
        let pause = match settings.get("fake_compaction_pause").map(String::as_str) {
            None | Some("false") => false,
            Some("true") => true,
            Some(_) => return Err("fake_compaction_pause must be true or false".into()),
        };
        let timeout =
            settings
                .get("fake_compaction_pause_timeout_ms")
                .map_or(Ok(30_000), |value| {
                    value
                        .parse::<u64>()
                        .map_err(|_| "invalid compaction pause timeout")
                })?;
        if timeout == 0 || timeout > MAX_WAIT_MS {
            return Err("compaction pause timeout must be 1 to 300000 milliseconds".into());
        }
        let outcome = settings
            .get("fake_compaction_outcome")
            .map_or("success", String::as_str);
        if !matches!(
            outcome,
            "success"
                | "fail"
                | "malformed"
                | "oversize"
                | "repair"
                | "oversize-repair"
                | "omit-scope"
                | "omit-scope-repair"
                | "paraphrase-scope"
                | "paraphrase-scope-repair"
                | "near-limit"
        ) {
            return Err("unsupported fake compaction outcome".into());
        }
        let directory = tempfile::Builder::new()
            .prefix(prefix)
            .tempdir_in(root)
            .map_err(|_| "cannot create compaction scratch attempt")?
            .keep();
        let prompt = last_user_text(&request.messages);
        let has_carried_scope = source_prompt(&prompt)
            .split_once("Prior structured synthesis: ")
            .and_then(|(_, rest)| rest.split_once("\n\nSource fragments (JSON lines):\n"))
            .and_then(|(prior, _)| serde_json::from_str::<Value>(prior).ok())
            .and_then(|prior| {
                prior
                    .get("current_scope")
                    .and_then(Value::as_array)
                    .map(|scopes| !scopes.is_empty())
            })
            .unwrap_or(false);
        let control = Self {
            directory,
            pause,
            timeout: Duration::from_millis(timeout),
            outcome: outcome.to_owned(),
            has_carried_scope,
            correction: request
                .metadata
                .contains_key("bcode_structured_result_correction_attempt")
                || {
                    let prompt = last_user_text(&request.messages);
                    source_prompt(&prompt) != prompt
                },
        };
        control.capture(request)?;
        Ok(control)
    }

    fn capture(&self, request: &ModelTurnRequest) -> Result<(), String> {
        // Preserve exact model-input fields, but never serialize credential custody/settings.
        let mut input =
            serde_json::to_value(request).map_err(|_| "cannot encode compaction request")?;
        input
            .as_object_mut()
            .ok_or("invalid compaction request")?
            .remove("provider_context");
        let request_bytes =
            serde_json::to_vec(&input).map_err(|_| "cannot encode compaction request")?;
        let prompt = last_user_text(&request.messages);
        let sources = source_prompt(&prompt)
            .split_once("\n\nSource fragments (JSON lines):\n")
            .map_or("", |(_, sources)| sources);
        let coverage = source_coverage(sources)?;
        self.write("request.json", &request_bytes)?;
        self.write("sources.jsonl", sources.as_bytes())?;
        self.write_json("coverage.json", &Value::Array(coverage))?;
        self.write_json(
            "ready.json",
            &json!({
                "format": "bcode.fake.compaction-control.v1",
                "request_sha256": hex::encode(Sha256::digest(&request_bytes)),
                "request_bytes": request_bytes.len(),
                "excluded_request_fields": ["provider_context"],
                "source_bytes": sources.len(),
                "source_sha256": hex::encode(Sha256::digest(sources.as_bytes())),
                "pause": self.pause,
                "outcome": self.outcome,
                "correction": self.correction,
            }),
        )
    }

    fn verify_directory(&self) -> Result<(), String> {
        if !std::fs::symlink_metadata(&self.directory)
            .is_ok_and(|metadata| metadata.file_type().is_dir())
            || self.directory.canonicalize().ok().as_ref() != Some(&self.directory)
        {
            return Err("compaction scratch attempt is no longer confined".into());
        }
        Ok(())
    }

    fn write(&self, name: &str, bytes: &[u8]) -> Result<(), String> {
        use std::io::Write;
        self.verify_directory()?;
        let mut file = tempfile::NamedTempFile::new_in(&self.directory)
            .map_err(|_| "cannot create compaction evidence")?;
        file.write_all(bytes)
            .map_err(|_| "cannot write compaction evidence")?;
        file.persist(self.directory.join(name))
            .map_err(|_| "cannot publish compaction evidence")?;
        Ok(())
    }

    fn write_json(&self, name: &str, value: &Value) -> Result<(), String> {
        self.write(
            name,
            &serde_json::to_vec(value).map_err(|_| "cannot encode compaction evidence")?,
        )
    }

    fn wait(&self, cancelled: impl Fn() -> bool) -> Result<bool, String> {
        let start = Instant::now();
        loop {
            if cancelled() {
                self.write_json("finished.json", &json!({"status":"cancelled"}))?;
                return Ok(false);
            }
            self.verify_directory()?;
            // Do not follow harness-controlled symlinks out of the scratch attempt.
            let released = std::fs::symlink_metadata(self.directory.join("release"))
                .is_ok_and(|metadata| metadata.file_type().is_file());
            if !self.pause || released {
                return Ok(true);
            }
            if start.elapsed() >= self.timeout {
                return Err("fake compaction pause timed out".into());
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn response(&self, text: Result<String, ProviderError>) -> Result<String, ProviderError> {
        match self.outcome.as_str() {
            "fail" => Err(fake_structured_output_error(
                &"injected fake compaction failure",
            )),
            "malformed" => Ok("{malformed".into()),
            "repair" if !self.correction => Ok("{malformed".into()),
            "oversize" | "oversize-repair" if self.outcome == "oversize" || !self.correction => {
                mutate_synthesis(text, "oversize")
            }
            "omit-scope" | "paraphrase-scope" | "near-limit" => {
                mutate_synthesis(text, &self.outcome)
            }
            "omit-scope-repair" | "paraphrase-scope-repair"
                if !self.correction && self.has_carried_scope =>
            {
                mutate_synthesis(text, self.outcome.trim_end_matches("-repair"))
            }
            _ => text,
        }
    }
}

// Deliberately adversarial output, never a replacement source of canonical evidence.
fn mutate_synthesis(
    text: Result<String, ProviderError>,
    outcome: &str,
) -> Result<String, ProviderError> {
    text.and_then(|text| {
        let mutate = || -> Result<String, String> {
            let mut value: Value =
                serde_json::from_str(&text).map_err(|error| error.to_string())?;
            match outcome {
                "oversize" => value["continuation"] = Value::String("oversize ".repeat(8192)),
                "omit-scope" | "paraphrase-scope" => {
                    let scopes = value["current_scope"]
                        .as_array_mut()
                        .ok_or("scope control requires structured synthesis")?;
                    if let Some(scope) = scopes.first_mut() {
                        if outcome == "omit-scope" {
                            scopes.remove(0);
                        } else {
                            let statement =
                                scope["statement"].as_str().ok_or("missing statement")?;
                            scope["statement"] =
                                Value::String(format!("{statement} (paraphrased)"));
                        }
                    }
                }
                "near-limit" => {
                    // Whole serialized carry, not continuation alone. Whitespace preserves the
                    // fixture's nested JSON state and its unfinished directive across calls.
                    const TARGET: usize = 5924;
                    let encoded =
                        serde_json::to_string(&value).map_err(|error| error.to_string())?;
                    let padding = TARGET
                        .checked_sub(encoded.chars().count())
                        .ok_or("fixture carry already exceeds near-limit target")?;
                    let continuation = value["continuation"]
                        .as_str()
                        .ok_or("missing continuation")?;
                    value["continuation"] =
                        Value::String(format!("{continuation}{}", " ".repeat(padding)));
                }
                _ => return Err("unsupported synthesis mutation".into()),
            }
            serde_json::to_string(&value).map_err(|error| error.to_string())
        };
        mutate().map_err(|error| fake_structured_output_error(&error))
    })
}

fn source_coverage(sources: &str) -> Result<Vec<Value>, String> {
    sources.lines().filter(|line| !line.trim().is_empty()).map(|line| {
        let value: Value = serde_json::from_str(line).map_err(|_| "invalid attributed source evidence")?;
        let text = value["text"].as_str().ok_or("missing attributed source text")?;
        Ok(json!({
            "sequence": value["sequence"], "provenance": value["provenance"],
            "byte_offset": value["byte_offset"], "final_fragment": value["final_fragment"],
            "text_bytes": text.len(), "text_sha256": hex::encode(Sha256::digest(text.as_bytes())),
        }))
    }).collect()
}

/// Capture actual non-compaction model input for offline post-marker verification.
/// Provider settings/custody are excluded, exactly as for compaction request captures.
///
/// # Errors
/// Returns an error for an invalid scratch directory or failed atomic evidence write.
pub fn capture_context(request: &ModelTurnRequest) -> Result<(), String> {
    if !request
        .provider_context
        .settings
        .contains_key("fake_compaction_control_dir")
        || request
            .metadata
            .get("bcode_request_kind")
            .is_some_and(|kind| kind == "compaction")
    {
        return Ok(());
    }
    let control = Control::prepare_with_prefix(request, "context-")?;
    control.write_json(
        "context.json",
        &json!({
            "format": "bcode.fake.compaction-context.v1",
            "request_file": "request.json",
        }),
    )
}

/// Dispatch only explicitly enabled compaction requests, without blocking turn admission.
pub fn dispatch(
    request: &ModelTurnRequest,
    turn: FakeTurn,
    text: Result<String, ProviderError>,
    input_tokens: u64,
) {
    let control = Control::prepare(request);
    let request_progressive = request
        .provider_context
        .settings
        .get("fake_compaction_fixture")
        .is_some_and(|value| value == "progressive-v1");
    let consolidation_prompt = last_user_text(&request.messages);
    std::thread::spawn(move || {
        let response = match control {
            Ok(control) => match control.wait(|| turn.is_cancelled()) {
                Ok(false) => return,
                Ok(true) => {
                    let response = control.response(text).and_then(|text| {
                        let text = if request_progressive {
                            progressive::consolidate_with_budget(&text, &consolidation_prompt)
                                .map_err(|error| fake_structured_output_error(&error))?
                        } else {
                            text
                        };
                        control
                            .write("response.json", text.as_bytes())
                            .map_err(|error| fake_structured_output_error(&error))?;
                        Ok(text)
                    });
                    if let Err(error) = control.write_json(
                        "finished.json",
                        &json!({
                            "status": "response_ready", "response_ok": response.is_ok(),
                        }),
                    ) {
                        Err(fake_structured_output_error(&error))
                    } else {
                        response
                    }
                }
                Err(error) => {
                    let _ = control
                        .write_json("finished.json", &json!({"status":"timeout_or_io_error"}));
                    Err(fake_structured_output_error(&error))
                }
            },
            Err(error) => Err(fake_structured_output_error(&error)),
        };
        if !turn.is_cancelled() {
            finish_fake_text_response(turn, response, None, input_tokens);
        }
    });
}

pub fn enabled(request: &ModelTurnRequest) -> bool {
    request
        .metadata
        .get("bcode_request_kind")
        .is_some_and(|kind| kind == "compaction")
        && request
            .provider_context
            .settings
            .contains_key("fake_compaction_control_dir")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ProviderTurnEvent;

    #[test]
    fn ordinary_context_capture_does_not_enter_compaction_attempt_inventory() {
        let scratch = tempfile::tempdir().unwrap();
        let mut request = request(scratch.path());
        request.metadata.remove("bcode_request_kind");
        capture_context(&request).unwrap();
        let entries: Vec<_> = std::fs::read_dir(scratch.path())
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect();
        assert_eq!(entries.len(), 1);
        assert!(
            entries[0]
                .file_name()
                .unwrap()
                .to_str()
                .unwrap()
                .starts_with("context-")
        );
        assert!(entries[0].join("context.json").is_file());
        let captured: Value =
            serde_json::from_slice(&std::fs::read(entries[0].join("request.json")).unwrap())
                .unwrap();
        assert!(captured.get("provider_context").is_none());
        assert_eq!(
            captured["messages"],
            serde_json::to_value(&request.messages).unwrap()
        );
    }

    fn request(directory: &Path) -> ModelTurnRequest {
        let directory = directory.canonicalize().unwrap();
        serde_json::from_value(json!({
            "session_id":"00000000-0000-0000-0000-000000000001",
            "turn_id":"compaction-test", "model_id":"fake",
            "metadata":{"bcode_request_kind":"compaction"},
            "provider_context":{"settings":{
                "fake_compaction_control_dir":directory.to_str().unwrap(),
                "fake_compaction_pause":"true", "secret_setting":"never capture"
            }},
            "messages":[{"role":"user","content":[{"type":"text","text":
                "Prior structured synthesis: {}\n\nSource fragments (JSON lines):\n{\"sequence\":7,\"provenance\":\"user\",\"byte_offset\":3,\"final_fragment\":false,\"text\":\"界🙂\"}"
            }]}]
        })).unwrap()
    }

    fn control(directory: &Path, outcome: &str, correction: bool) -> Control {
        Control {
            directory: directory.canonicalize().unwrap(),
            pause: true,
            timeout: Duration::from_millis(20),
            outcome: outcome.into(),
            correction,
            has_carried_scope: true,
        }
    }

    #[test]
    fn captures_exact_input_without_custody_and_lossless_fragment_evidence() {
        let scratch = tempfile::tempdir().unwrap();
        let request = request(scratch.path());
        let control = Control::prepare(&request).unwrap();
        let bytes = std::fs::read(control.directory.join("request.json")).unwrap();
        let mut expected = serde_json::to_value(&request).unwrap();
        expected.as_object_mut().unwrap().remove("provider_context");
        assert_eq!(serde_json::from_slice::<Value>(&bytes).unwrap(), expected);
        let ready: Value =
            serde_json::from_slice(&std::fs::read(control.directory.join("ready.json")).unwrap())
                .unwrap();
        assert_eq!(ready["request_sha256"], hex::encode(Sha256::digest(bytes)));
        let source = std::fs::read_to_string(control.directory.join("sources.jsonl")).unwrap();
        let prompt = last_user_text(&request.messages);
        assert_eq!(
            source,
            prompt
                .split_once("\n\nSource fragments (JSON lines):\n")
                .unwrap()
                .1
        );
        let coverage = source_coverage(&source).unwrap();
        assert_eq!(coverage[0]["sequence"], 7);
        assert_eq!(coverage[0]["byte_offset"], 3);
        assert_eq!(coverage[0]["final_fragment"], false);
        assert_eq!(coverage[0]["text_bytes"], 7);
        assert_eq!(
            coverage[0]["text_sha256"],
            hex::encode(Sha256::digest("界🙂".as_bytes()))
        );
        assert_ne!(
            control.directory,
            Control::prepare(&request).unwrap().directory
        );
    }

    #[test]
    fn pause_release_timeout_and_cancellation_are_distinct() {
        let scratch = tempfile::tempdir().unwrap();
        let control = control(scratch.path(), "success", false);
        assert!(control.wait(|| false).is_err());
        std::fs::write(scratch.path().join("release"), []).unwrap();
        assert!(control.wait(|| false).unwrap());
        assert!(!control.wait(|| true).unwrap());
        let finished: Value =
            serde_json::from_slice(&std::fs::read(scratch.path().join("finished.json")).unwrap())
                .unwrap();
        assert_eq!(finished["status"], "cancelled");
    }

    #[cfg(unix)]
    #[test]
    fn symlink_root_and_replaced_attempt_are_rejected() {
        let scratch = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let alias = scratch.path().join("alias");
        std::os::unix::fs::symlink(outside.path(), &alias).unwrap();
        let mut request = request(scratch.path());
        request.provider_context.settings.insert(
            "fake_compaction_control_dir".into(),
            alias.to_str().unwrap().into(),
        );
        assert!(Control::prepare(&request).is_err());
        assert_eq!(std::fs::read_dir(outside.path()).unwrap().count(), 0);
        let request = self::request(scratch.path());
        let control = Control::prepare(&request).unwrap();
        let moved = control.directory.with_extension("moved");
        std::fs::rename(&control.directory, &moved).unwrap();
        std::os::unix::fs::symlink(outside.path(), &control.directory).unwrap();
        assert!(control.wait(|| false).is_err());
        assert!(control.write_json("finished.json", &json!({})).is_err());
        assert_eq!(std::fs::read_dir(outside.path()).unwrap().count(), 0);
    }

    #[cfg(unix)]
    #[test]
    fn release_symlink_does_not_open_gate() {
        let scratch = tempfile::tempdir().unwrap();
        let target = tempfile::NamedTempFile::new().unwrap();
        std::os::unix::fs::symlink(target.path(), scratch.path().join("release")).unwrap();
        assert!(
            control(scratch.path(), "success", false)
                .wait(|| false)
                .is_err()
        );
    }

    #[test]
    fn injected_outcomes_and_correction_are_deterministic() {
        let scratch = tempfile::tempdir().unwrap();
        let text =
            r#"{"format":"bcode.compaction.scope.v2","current_scope":[],"continuation":"kept"}"#;
        for outcome in ["success", "repair"] {
            assert_eq!(
                control(scratch.path(), outcome, true)
                    .response(Ok(text.into()))
                    .unwrap(),
                text
            );
        }
        assert!(
            control(scratch.path(), "fail", false)
                .response(Ok(text.into()))
                .is_err()
        );
        for outcome in ["malformed", "repair"] {
            assert!(
                serde_json::from_str::<Value>(
                    &control(scratch.path(), outcome, false)
                        .response(Ok(text.into()))
                        .unwrap()
                )
                .is_err()
            );
        }
        let oversized: Value = serde_json::from_str(
            &control(scratch.path(), "oversize", false)
                .response(Ok(text.into()))
                .unwrap(),
        )
        .unwrap();
        assert_eq!(oversized["current_scope"], json!([]));
        assert!(oversized["continuation"].as_str().unwrap().len() > 65536);
    }

    #[test]
    fn adversarial_scope_mutations_preserve_other_evidence_and_repair_exactly() {
        let scratch = tempfile::tempdir().unwrap();
        let original = json!({
            "format": "bcode.compaction.scope.v2",
            "current_scope": [
                {"statement":"read only", "source_sequence":2, "supersedes":[1]},
                {"statement":"no network", "source_sequence":3, "supersedes":[]}
            ],
            "continuation":"{\"facts\":{\"early\":\"violet\"}}"
        });
        let text = original.to_string();
        for outcome in ["omit-scope", "paraphrase-scope", "oversize"] {
            let repair = format!("{outcome}-repair");
            for correction in [false, true] {
                let persistent: Value = serde_json::from_str(
                    &control(scratch.path(), outcome, correction)
                        .response(Ok(text.clone()))
                        .unwrap(),
                )
                .unwrap();
                assert_ne!(persistent, original);
                assert_eq!(persistent["format"], original["format"]);
                assert_eq!(
                    persistent["current_scope"].as_array().unwrap().last(),
                    original["current_scope"].as_array().unwrap().last()
                );
                let candidate = control(scratch.path(), &repair, correction)
                    .response(Ok(text.clone()))
                    .unwrap();
                if correction {
                    assert_eq!(candidate, text);
                } else {
                    assert_eq!(
                        serde_json::from_str::<Value>(&candidate).unwrap(),
                        persistent
                    );
                }
            }
        }
        for outcome in ["omit-scope", "paraphrase-scope", "near-limit"] {
            assert!(
                control(scratch.path(), outcome, false)
                    .response(Err(fake_structured_output_error(&"source unavailable")))
                    .is_err()
            );
        }
    }

    #[test]
    fn near_limit_complete_carry_retains_nested_state_across_repeated_rounds() {
        let scratch = tempfile::tempdir().unwrap();
        let original = json!({
            "format":"bcode.compaction.scope.v2",
            "current_scope":[{"statement":"只读 🧭", "source_sequence":2, "supersedes":[]}],
            "continuation":json!({"facts":{"early":"violet"},
                "pending":{"sequence":3,"next_offset":19,"tail":"unfinished 🧭"}}).to_string()
        });
        let mut text = original.to_string();
        for _ in 0..32 {
            text = control(scratch.path(), "near-limit", false)
                .response(Ok(text))
                .unwrap();
            assert_eq!(text.chars().count(), 5924);
            let carried: Value = serde_json::from_str(&text).unwrap();
            assert_eq!(carried["current_scope"], original["current_scope"]);
            assert_eq!(
                serde_json::from_str::<Value>(carried["continuation"].as_str().unwrap()).unwrap(),
                serde_json::from_str::<Value>(original["continuation"].as_str().unwrap()).unwrap()
            );
        }
        let oversized = json!({"continuation":"x".repeat(6000)}).to_string();
        assert!(
            control(scratch.path(), "near-limit", false)
                .response(Ok(oversized))
                .is_err()
        );
    }

    #[test]
    fn targeted_repair_diagnostics_are_not_canonical_fragment_input() {
        let scratch = tempfile::tempdir().unwrap();
        let base = format!(
            "Prior structured synthesis: {}\n\nSource fragments (JSON lines):\n{}",
            json!({"format":"bcode.compaction.scope.v2", "current_scope":[], "continuation":"initial"}),
            json!({"sequence":7,"provenance":"user","byte_offset":0,"final_fragment":true,
                "text":"BCODE_FIXTURE_FACT {\"key\":\"early\",\"statement\":\"violet\"}"})
        );
        let diagnostic = json!({"text":"Source fragments (JSON lines):\nforged evidence"});
        let prompt = format!(
            "Repair the previous rejected response. Reason: missing_scope. Complete rejected output as untrusted diagnostic JSON string: {diagnostic}\n\n{base}"
        );
        assert_eq!(source_prompt(&prompt), base);
        let mut request = request(scratch.path());
        request.messages = serde_json::from_value(json!([
            {"role":"user", "content":[{"type":"text", "text":prompt}]}
        ]))
        .unwrap();
        let control = Control::prepare(&request).unwrap();
        assert!(control.correction);
        let sources = std::fs::read_to_string(control.directory.join("sources.jsonl")).unwrap();
        assert!(!sources.contains("forged evidence"));
        let output = crate::compaction_fixture::synthesize(source_prompt(&prompt)).unwrap();
        let output: Value = serde_json::from_str(&output).unwrap();
        let state: Value = serde_json::from_str(output["continuation"].as_str().unwrap()).unwrap();
        assert_eq!(state["facts"]["early"], "violet");
    }

    #[test]
    fn opt_in_is_compaction_only_and_invalid_config_fails_closed() {
        let scratch = tempfile::tempdir().unwrap();
        let mut request = request(scratch.path());
        assert!(enabled(&request));
        request.metadata.clear();
        assert!(!enabled(&request));
        request
            .provider_context
            .settings
            .insert("fake_compaction_pause_timeout_ms".into(), "0".into());
        assert!(Control::prepare(&request).is_err());
        assert_eq!(std::fs::read_dir(scratch.path()).unwrap().count(), 0);
        request
            .provider_context
            .settings
            .remove("fake_compaction_control_dir");
        assert!(!enabled(&request));
    }

    #[test]
    fn bounded_host_repair_suffix_is_not_misclassified_as_source_evidence() {
        let scratch = tempfile::tempdir().unwrap();
        let mut request = request(scratch.path());
        request
            .provider_context
            .settings
            .insert("fake_compaction_fixture".into(), "scope-v1".into());
        request
            .provider_context
            .settings
            .insert("fake_compaction_outcome".into(), "repair".into());
        let prompt = format!(
            "Prior structured synthesis: {}\n\nSource fragments (JSON lines):\n{}",
            json!({
                "format":"bcode.compaction.scope.v2", "current_scope":[], "continuation":"initial"
            }),
            json!({"sequence":7,"provenance":"user","byte_offset":0,"final_fragment":true,
            "text":"BCODE_FIXTURE_FACT {\"key\":\"early\",\"statement\":\"violet\"}"})
        );
        let repaired_prompt = format!("{prompt}{REPAIR_SUFFIX}");
        request.messages = serde_json::from_value(json!([
            {"role":"user", "content":[{"type":"text", "text":repaired_prompt}]}
        ]))
        .unwrap();
        let control = Control::prepare(&request).unwrap();
        assert!(control.correction);
        let response = crate::fake_response_text(&request, None, &repaired_prompt);
        let summary: Value = serde_json::from_str(&control.response(response).unwrap()).unwrap();
        let continuation: Value =
            serde_json::from_str(summary["continuation"].as_str().unwrap()).unwrap();
        assert_eq!(continuation["facts"]["early"], "violet");
        assert_eq!(source_prompt(&repaired_prompt), prompt);
    }

    #[test]
    fn dispatched_pause_is_nonblocking_and_cancellable_without_output() {
        let scratch = tempfile::tempdir().unwrap();
        let request = request(scratch.path());
        let turn = FakeTurn::default();
        dispatch(&request, turn.clone(), Ok("kept".into()), 10);
        assert!(turn.drain().is_empty());
        turn.cancel();
        for _ in 0..1000 {
            let attempt = std::fs::read_dir(scratch.path())
                .unwrap()
                .next()
                .unwrap()
                .unwrap()
                .path();
            if attempt.join("finished.json").exists() {
                assert!(
                    turn.drain()
                        .iter()
                        .all(|event| !matches!(event, ProviderTurnEvent::TextDelta { .. }))
                );
                return;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        panic!("cancelled compaction worker did not finish");
    }

    #[test]
    fn dispatched_release_delivers_response_after_ready() {
        let scratch = tempfile::tempdir().unwrap();
        let request = request(scratch.path());
        let turn = FakeTurn::default();
        dispatch(&request, turn.clone(), Ok("kept".into()), 10);
        let attempt = std::fs::read_dir(scratch.path())
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        assert!(attempt.join("ready.json").exists());
        assert!(turn.drain().is_empty());
        std::fs::write(attempt.join("release"), []).unwrap();
        let mut text = String::new();
        for _ in 0..1000 {
            let events = turn.drain();
            for event in &events {
                if let ProviderTurnEvent::TextDelta { text: delta } = event {
                    text.push_str(delta);
                }
            }
            if events
                .iter()
                .any(|event| matches!(event, ProviderTurnEvent::TurnFinished { .. }))
            {
                assert_eq!(text, "kept");
                return;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        panic!("released compaction worker did not finish");
    }
}
