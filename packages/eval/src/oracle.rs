//! Independent grading of delivered artifacts, never intermediate worker summaries.
use super::{EvalError, Path, fs, process, shell_command};
use bcode_eval_models::WorkflowEvalConfig;
use std::{io::Read, time::Duration};

pub fn grade(
    config: &WorkflowEvalConfig,
    workspace: &Path,
    artifact_dir: &Path,
    report: &str,
) -> Result<bool, EvalError> {
    let answer = if let Some(path) = &config.answer_file {
        if path.is_absolute()
            || path
                .components()
                .any(|component| matches!(component, std::path::Component::ParentDir))
        {
            return Err(EvalError::Validation(
                "answer path must be workspace-relative".into(),
            ));
        }
        let root = workspace.canonicalize()?;
        let file = workspace.join(path);
        if !file.exists() {
            return Ok(false);
        }
        let file = file.canonicalize()?;
        if !file.starts_with(root) {
            return Err(EvalError::Validation(
                "answer symlink escapes workspace".into(),
            ));
        }
        if fs::metadata(&file)?.len() > 1_048_576 {
            return Ok(false);
        }
        let mut answer = String::new();
        fs::File::open(file)?
            .take(1_048_577)
            .read_to_string(&mut answer)?;
        if answer.len() > 1_048_576 {
            return Ok(false);
        }
        answer
    } else {
        report.to_owned()
    };
    let text_pass = config
        .expected_contains
        .iter()
        .all(|expected| answer.contains(expected));
    let json_pass = config.expected_json.as_ref().is_none_or(|expected| {
        serde_json::from_str::<serde_json::Value>(&answer).is_ok_and(|value| value == *expected)
    });
    let command_pass = if let Some(command) = &config.oracle_command {
        let mut process = shell_command(command);
        process
            .current_dir(workspace)
            .env("BCODE_EVAL_WORKSPACE", workspace)
            .env("BCODE_EVAL_ARTIFACT_DIR", artifact_dir);
        let (code, expired, _, _) = process::bounded_command(
            process,
            Duration::from_mins(1),
            artifact_dir,
            "oracle-command",
        )?;
        code == Some(0) && !expired
    } else {
        true
    };
    Ok(text_pass && json_pass && command_pass)
}

#[cfg(test)]
mod tests {
    use super::grade;
    use bcode_eval_models::WorkflowEvalConfig;
    #[test]
    fn exact_json_rejects_missing_extra_or_wrong_delivered_fields() {
        let root = std::env::temp_dir().join(format!("bcode-oracle-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let config:WorkflowEvalConfig=serde_json::from_value(serde_json::json!({"binary":"/bin/true","strategy":"auto","stop_condition":"correct","max_iterations":1,"concurrency":1,"worker_attempts":1,"answer_file":"answer.json","expected_json":{"sum":4}})).unwrap();
        for (answer, expected) in [
            ("{\"sum\":4}", true),
            ("{\"sum\":5}", false),
            ("{\"sum\":4,\"extra\":true}", false),
            ("not json", false),
        ] {
            std::fs::write(root.join("answer.json"), answer).unwrap();
            assert_eq!(
                grade(&config, &root, &root, "ignored intermediate answer").unwrap(),
                expected
            );
        }
        std::fs::remove_file(root.join("answer.json")).unwrap();
        std::fs::remove_dir(root).unwrap();
    }
}
