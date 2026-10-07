use super::*;
use std::cell::Cell;

#[test]
fn paged_diagnostics_verify_identity_progress_and_digest() {
    let root = std::env::temp_dir().join(format!("bcode-diag-export-{}", std::process::id()));
    let bytes = vec![b'x'; 40_000];
    let digest = hex::encode(Sha256::digest(&bytes));
    let calls = Cell::new(0);
    let invoke = |args: &[String], _: &str, _: Duration| {
        if args[1] == "diagnostic-artifacts" {
            return Ok(
                serde_json::json!({"version":1,"run_id":"run","dispatch_identity":"dispatch","omitted_artifacts":0,"unavailable":[],"artifacts":[{
                    "artifact_id":"payload","sha256":digest,"byte_len":bytes.len(),"completeness":"retained"
                }]}),
            );
        }
        calls.set(calls.get() + 1);
        let offset: usize = args[9].parse().unwrap();
        let end = (offset + 32768).min(bytes.len());
        Ok(
            serde_json::json!({"version":1,"run_id":"run","dispatch_identity":"dispatch","artifact_id":"payload",
            "sha256":digest,"byte_len":bytes.len(),"completeness":"retained","offset":offset,
            "bytes":bytes[offset..end],"eof":end==bytes.len()}),
        )
    };
    capture("run", "dispatch", &root, &invoke).unwrap();
    assert_eq!(calls.get(), 2);
    assert_eq!(
        std::fs::read(root.join("artifact-000.json")).unwrap(),
        bytes
    );
    let bad = |args: &[String], label: &str, timeout: Duration| {
        let mut value = invoke(args, label, timeout)?;
        if args[1] == "read-diagnostic-artifact" {
            value["bytes"] = serde_json::json!([]);
        }
        Ok(value)
    };
    assert!(capture("run", "dispatch", &root, &bad).is_err());
    let bad_identity = |args: &[String], label: &str, timeout: Duration| {
        let mut value = invoke(args, label, timeout)?;
        value["run_id"] = serde_json::json!("foreign");
        Ok(value)
    };
    assert!(capture("run", "dispatch", &root, &bad_identity).is_err());
    for omitted in [serde_json::Value::Null, serde_json::json!(1)] {
        let incomplete = |args: &[String], label: &str, timeout: Duration| {
            let mut value = invoke(args, label, timeout)?;
            if args[1] == "diagnostic-artifacts" {
                value["omitted_artifacts"] = omitted.clone();
            }
            Ok(value)
        };
        assert!(capture("run", "dispatch", &root, &incomplete).is_err());
        assert_eq!(
            std::fs::read(root.join("artifact-000.json")).unwrap(),
            bytes
        );
    }
}
