//! Real provider/tool scenario inventory and canonical effect assertions.
use super::{BcodeClient, SessionEventKind, SessionId, TestResult, history, json, require};
use serde_json::Value;
use sha2::Digest as _;
use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::Path;

pub fn configure(config: &mut bcode_config::BcodeConfig) {
    let mut scripts = BTreeMap::new();
    for (marker, approved) in [
        ("[browser-batch-approve]", true),
        ("[browser-batch-deny]", false),
    ] {
        let mut steps = Vec::new();
        for index in 0..2 {
            let id = format!("browser-batch-{approved}-{index}");
            let arguments = json!({"path":format!("batch-{approved}-{index}.txt"), "contents":format!("batch {approved} {index}\n")});
            steps.push(json!({"event":{"type":"tool_call_started", "call_id":id, "name":"filesystem.write"}}));
            steps.push(json!({"event":{"type":"tool_call_finished", "call":{"id":id, "name":"filesystem.write", "arguments":arguments}}}));
        }
        steps.push(json!({"event":{"type":"turn_finished", "stop_reason":"tool_call"}}));
        scripts.insert(marker, json!({"steps":steps}));
    }
    scripts.insert("[browser-cancel]", json!({"steps":[
        {"event":{"type":"text_delta", "text":"Cancellation checkpoint 日本語 🦀 e\u{301}"}},
        {"gate":"browser-cancel-release", "event":{"type":"text_delta", "text":"FORBIDDEN LATE PROVIDER OUTPUT"}},
        {"event":{"type":"turn_finished", "stop_reason":"end_turn"}}
    ]}));
    scripts.insert("[browser-rich-markdown]", json!({"steps":[
        {"event":{"type":"text_delta", "text":"# Browser rich output\n\n日本語 🦀 e\u{301}\n\n```rust\nlet browser = true;\n```\n\n[Acceptance link](https://example.invalid/browser-acceptance)\n\n<details><summary>Expand detail</summary>\nHidden detail body\n</details>\n"}},
        {"event":{"type":"turn_finished", "stop_reason":"end_turn"}}
    ]}));
    config.model.settings.insert(
        "fake_event_scripts_by_prompt_marker".into(),
        serde_json::to_string(&scripts).expect("scripts"),
    );
}

pub async fn create(client: &BcodeClient, root: &Path) -> TestResult<Value> {
    // A real tool reads test-owned bytes, stores an artifact through its normal bridge,
    // and emits its own image semantic result; no test-created session events.
    const PNG: &[u8] = &[
        137, 80, 78, 71, 13, 10, 26, 10, 0, 0, 0, 13, 73, 72, 68, 82, 0, 0, 0, 16, 0, 0, 0, 16, 8,
        6, 0, 0, 0, 31, 243, 255, 97, 0, 0, 0, 25, 73, 68, 65, 84, 120, 156, 99, 248, 32, 18, 240,
        159, 18, 204, 48, 106, 192, 168, 1, 163, 6, 12, 23, 3, 0, 174, 88, 83, 31, 87, 41, 171, 1,
        0, 0, 0, 0, 73, 69, 78, 68, 174, 66, 96, 130,
    ];
    std::fs::write(root.join("browser-image.png"), PNG)?;
    // An ancillary PNG text chunk makes a real finalized tool artifact cross
    // the 128-KiB range boundary without requiring a giant decoded bitmap.
    let mut large = PNG[..33].to_vec();
    let mut ancillary = b"acceptance\0".to_vec();
    ancillary.extend(std::iter::repeat_n(b'x', 200_000));
    large.extend_from_slice(&u32::try_from(ancillary.len())?.to_be_bytes());
    large.extend_from_slice(b"tEXt");
    large.extend_from_slice(&ancillary);
    let mut crc = u32::MAX;
    for byte in b"tEXt".iter().chain(&ancillary) {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            crc = (crc >> 1) ^ (0xedb8_8320_u32 & 0_u32.wrapping_sub(crc & 1));
        }
    }
    large.extend_from_slice(&(!crc).to_be_bytes());
    large.extend_from_slice(&PNG[33..]);
    std::fs::write(root.join("browser-large.png"), large)?;
    // Real shell producer retains the normalized stream using its existing
    // artifact port. Browser receives only semantic results and bounded ranges.
    {
        use std::io::Write as _;
        let mut file =
            std::io::BufWriter::new(std::fs::File::create(root.join("browser-overbudget.txt"))?);
        let mut line = [b'x'; 1024];
        line[1023] = b'\n';
        for _ in 0..65_537 {
            file.write_all(&line)?;
        }
        file.flush()?;
    }
    std::fs::write(
        root.join("browser-rich.md"),
        "# Rich browser acceptance\n\n日本語 🦀 e\u{301}\n\n```rust\nlet browser = true;\n```\n\n[Acceptance link](https://example.invalid/browser-acceptance)\n",
    )?;
    std::fs::write(
        root.join("declarative-sentinel.toml"),
        b"# test-owned declarative sentinel\n[model]\nmodel_id = 'fake-echo'\n",
    )?;
    let mut scenarios = Vec::new();
    for kind in [
        "permission-approve",
        "permission-deny",
        "batch-approve",
        "batch-deny",
        "question-custom",
        "question-cancel",
        "image",
        "artifact",
        "rich",
        "cancel",
        "config",
        "question-option",
    ] {
        let session = client
            .create_session_in_working_directory(
                Some(format!("browser-{kind}")),
                root.to_path_buf(),
            )
            .await?;
        if kind == "artifact" {
            client
                .set_session_model(
                    session.id,
                    Some("bcode.fake-provider".into()),
                    "fake-tool-artifact-references".into(),
                )
                .await?;
        }
        let prompt = match kind {
            "permission-approve" => "tool-call filesystem.write {\"path\":\"approved.txt\",\"contents\":\"approved exactly once\\n\"}".to_owned(),
            "permission-deny" => "tool-call filesystem.write {\"path\":\"denied.txt\",\"contents\":\"must never exist\"}".to_owned(),
            "batch-approve" => "[browser-batch-approve]".into(),
            "batch-deny" => "[browser-batch-deny]".into(),
            "artifact" => "tool-call filesystem.read {\"path\":\"browser-large.png\"}\ntool-call shell.run {\"command\":\"cat browser-overbudget.txt\",\"timeout_ms\":300000,\"columns\":120,\"rows\":30}".into(),
            "image" => "tool-call filesystem.read {\"path\":\"browser-image.png\"}".into(),
            "rich" => "[browser-rich-markdown]".into(),
            "cancel" => "[browser-cancel]".into(),
            "config" => "configured browser submission 日本語 🦀 e\u{301}".into(),
            _ => format!("tool-call question {}", json!({"questions":[{
                "header":"Browser acceptance", "question":"Choose a genuine plugin answer 日本語 🦀 e\u{301}", "required":true,
                "control":"radio", "selection_mode":"single", "custom":true, "custom_mode":"exclusive",
                "options":[{"label":"First option", "value":"first", "description":"Canonical first"},
                    {"label":"Second option", "value":"second", "description":"Canonical second"}]
            }]})),
        };
        scenarios.push(json!({"kind":kind,"session":session.id,"prompt":prompt}));
    }
    Ok(Value::Array(scenarios))
}

// Keep the cross-scenario canonical checks together so every browser claim is
// visibly paired with its durable permission, question, artifact, or tool evidence.
#[allow(clippy::too_many_lines)]
pub async fn canonical(
    client: &BcodeClient,
    root: &Path,
    scenarios: &Value,
    report: &Value,
) -> TestResult<()> {
    let scenarios = scenarios.as_array().ok_or("scenario inventory")?;
    let results = report.as_array().ok_or("scenario report")?;
    require(
        results.len() == scenarios.len(),
        "not all production scenarios completed",
    )?;
    for scenario in scenarios {
        let kind = scenario["kind"].as_str().ok_or("kind")?;
        let session: SessionId = serde_json::from_value(scenario["session"].clone())?;
        let result = results
            .iter()
            .find(|result| result["kind"] == kind)
            .ok_or("missing scenario result")?;
        require(result["passed"] == true, "scenario did not pass")?;
        if kind == "artifact" {
            require(
                result["overbudget_download_error"] == "artifact_download_exceeds_budget",
                "real oversized artifact was not rejected",
            )?;
            require(
                result["overbudget_total_bytes"]
                    .as_u64()
                    .is_some_and(|bytes| bytes > 64 * 1024 * 1024),
                "oversized reference not proven",
            )?;
            let expected = sha2::Sha256::digest(std::fs::read(root.join("browser-large.png"))?)
                .iter()
                .fold(String::new(), |mut output, byte| {
                    write!(&mut output, "{byte:02x}").expect("format digest");
                    output
                });
            require(
                result["download_sha256"] == expected,
                "complete multi-range artifact download differs from test-owned source",
            )?;
        }
        let events = history(client, session).await?;
        require(
            events
                .iter()
                .filter(|event| matches!(event.kind, SessionEventKind::SubmissionAccepted { .. }))
                .count()
                == 1,
            "scenario submission duplicated",
        )?;
        require(
            events
                .iter()
                .filter(|event| matches!(event.kind, SessionEventKind::SubmissionActivated { .. }))
                .count()
                == 1,
            "scenario activation duplicated",
        )?;
        let requested: Vec<_> = events
            .iter()
            .filter_map(|event| match &event.kind {
                SessionEventKind::PermissionRequested {
                    permission_id,
                    batch,
                    ..
                } => Some((permission_id, batch)),
                _ => None,
            })
            .collect();
        let resolved: Vec<_> = events
            .iter()
            .filter_map(|event| match &event.kind {
                SessionEventKind::PermissionResolved {
                    permission_id,
                    approved,
                } => Some((permission_id, *approved)),
                _ => None,
            })
            .collect();
        if kind.starts_with("permission-") || kind.starts_with("batch-") {
            let count = if kind.starts_with("batch-") { 2 } else { 1 };
            let approved = kind.ends_with("approve");
            require(
                requested.len() == count && resolved.len() == count,
                "permission admission/decision count differs",
            )?;
            for (id, _) in &requested {
                require(
                    resolved
                        .iter()
                        .filter(|(resolved_id, decision)| {
                            resolved_id == id && *decision == approved
                        })
                        .count()
                        == 1,
                    "permission resolved incorrectly or more than once",
                )?;
            }
            if count == 2 {
                require(
                    requested.iter().all(|(_, batch)| {
                        batch.as_ref().is_some_and(|batch| batch.call_count == 2)
                    }),
                    "genuine two-call batch correlation missing",
                )?;
            }
        }
        require(
            events
                .iter()
                .any(|event| matches!(event.kind, SessionEventKind::ModelTurnFinished { .. })),
            "scenario has no canonical terminal turn",
        )?;
        if kind == "cancel" {
            require(
                events.iter().any(|event| {
                    matches!(
                        event.kind,
                        SessionEventKind::ModelTurnCancelRequested { .. }
                    )
                }),
                "cancel never reached canonical runtime",
            )?;
            require(
                events.iter().any(|event| {
                    matches!(
                        &event.kind,
                        SessionEventKind::ModelTurnFinished {
                            outcome: bcode_session_models::ModelTurnOutcome::Cancelled,
                            ..
                        }
                    )
                }),
                "canonical cancellation was not terminal Cancelled",
            )?;
            require(
                !serde_json::to_string(&events)?.contains("FORBIDDEN LATE PROVIDER OUTPUT"),
                "late provider output reopened terminal turn",
            )?;
        }
        let tool_results: Vec<_> = events
            .iter()
            .filter_map(|event| match &event.kind {
                SessionEventKind::ToolInvocationResultRecorded { record } => Some(record),
                _ => None,
            })
            .collect();
        if kind != "config" && kind != "cancel" && kind != "rich" {
            let expected = if kind.starts_with("batch-") || kind == "artifact" {
                2
            } else {
                1
            };
            require(
                tool_results.len() == expected,
                "real plugin result count differs",
            )?;
        }
        if kind == "artifact" {
            let artifact = tool_results
                .iter()
                .find_map(|record| match &record.result {
                    Some(bcode_session_models::ToolInvocationResult::Artifact { artifact })
                        if artifact.producer_plugin_id == "bcode.shell"
                            && artifact
                                .refs
                                .iter()
                                .any(|reference| reference.key == "clean_output") =>
                    {
                        Some(artifact)
                    }
                    _ => None,
                })
                .ok_or("real oversized shell result missing from canonical events")?;
            let reference = artifact
                .refs
                .iter()
                .find(|reference| reference.key == "clean_output")
                .ok_or("oversized reference missing")?;
            require(
                reference
                    .byte_len
                    .is_some_and(|bytes| bytes > 64 * 1024 * 1024),
                "canonical producer did not retain >64 MiB",
            )?;
            let range = client
                .session_artifact_range(
                    session,
                    artifact.artifact_id.clone(),
                    "clean_output".into(),
                    0,
                    128 * 1024,
                )
                .await?;
            require(
                range.finalized
                    && range.complete != Some(false)
                    && range.total_bytes > 64 * 1024 * 1024
                    && range.bytes.len() == 128 * 1024
                    && report
                        .as_array()
                        .ok_or("scenario reports")?
                        .iter()
                        .any(|result| {
                            result["kind"] == "artifact"
                                && result["overbudget_total_bytes"] == range.total_bytes
                        }),
                "canonical oversized artifact differs from browser rejection evidence",
            )?;
        }
        if kind.starts_with("question-") {
            let exchanges: Vec<_> = events
                .iter()
                .filter_map(|event| match &event.kind {
                    SessionEventKind::ToolExchangeResolved { event } => Some(event),
                    _ => None,
                })
                .collect();
            require(
                exchanges.len() == 1,
                "question exchange resolution duplicated or missing",
            )?;
            let bcode_session_models::ToolExchangeResolution::Responded { payload } =
                &exchanges[0].resolution
            else {
                return Err("question did not finish through producer response".into());
            };
            match kind {
                "question-option" => require(
                    payload["status"] == "answered"
                        && payload["questions"][0]["selected"] == json!(["second"]),
                    "canonical option answer differs",
                )?,
                "question-custom" => require(
                    payload["status"] == "answered"
                        && payload["questions"][0]["custom"] == "browser custom 日本語 🦀 e\u{301}",
                    "canonical custom answer differs",
                )?,
                _ => require(
                    payload["status"] == "dismissed",
                    "canonical question dismissal differs",
                )?,
            }
        }
    }
    require(
        std::fs::read(root.join("declarative-sentinel.toml"))?
            == b"# test-owned declarative sentinel\n[model]\nmodel_id = 'fake-echo'\n",
        "interactive browser configuration modified declarative input",
    )?;
    require(
        std::fs::read_to_string(root.join("approved.txt"))? == "approved exactly once\n",
        "approved tool effect differs",
    )?;
    require(
        !root.join("denied.txt").exists(),
        "denied tool performed side effect",
    )?;
    for index in 0..2 {
        require(
            std::fs::read_to_string(root.join(format!("batch-true-{index}.txt")))?
                == format!("batch true {index}\n"),
            "approved batch effect differs",
        )?;
        require(
            !root.join(format!("batch-false-{index}.txt")).exists(),
            "denied batch performed side effect",
        )?;
    }
    Ok(())
}
