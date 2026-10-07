use super::*;
use crate::{ToolInvocationResult, tool_write};

fn receipt(output: &str) -> serde_json::Value {
    let (_, json) = output.split_once('\n').expect("receipt after summary");
    serde_json::from_str(json).expect("receipt JSON")
}

#[test]
fn unicode_write_receipt_matches_bytes_and_preserves_change_artifact() {
    let root = crate::tests::temp_dir("write-receipt-unicode");
    let contents = "漢字🙂e\u{301}\n";
    let path = root.join("answer.txt");
    let response = tool_write(
        serde_json::json!({"path": path, "contents": contents}),
        None,
        "write-unicode",
    );
    assert!(!response.is_error);
    let written = std::fs::read(&path).expect("written bytes");
    assert_eq!(written, contents.as_bytes());
    let value = receipt(&response.output);
    assert_eq!(value["schema"], "bcode.filesystem.write_receipt");
    assert_eq!(value["version"], 1);
    assert_eq!(value["byte_len"], written.len());
    assert_eq!(value["sha256"], hex::encode(Sha256::digest(&written)));
    assert_eq!(value["phase"], "written_at_operation");
    let Some(ToolInvocationResult::Artifact { artifact }) = response.result else {
        panic!("expected unchanged FileChange artifact");
    };
    assert_eq!(artifact.schema, "bcode.filesystem.change");
    assert_eq!(artifact.metadata["new_text"], contents);
    assert_eq!(
        artifact.metadata["summary"],
        format!("wrote {} bytes", written.len())
    );
    std::fs::remove_dir_all(root).expect("cleanup");
}

#[test]
fn failed_write_has_no_success_receipt() {
    let root = crate::tests::temp_dir("write-receipt-failure");
    let response = tool_write(
        serde_json::json!({"path": root, "contents": "must not report success"}),
        None,
        "write-failure",
    );
    assert!(response.is_error);
    assert!(!response.output.contains("bcode.filesystem.write_receipt"));
    assert!(response.result.is_none());
    std::fs::remove_dir_all(root).expect("cleanup");
}

#[test]
fn large_write_receipt_is_bounded_without_content_preview() {
    let root = crate::tests::temp_dir("write-receipt-large");
    let contents = "sensitive-content🙂".repeat(100_000);
    let response = tool_write(
        serde_json::json!({"path": root.join("large.txt"), "contents": contents}),
        None,
        "write-large",
    );
    assert!(!response.is_error);
    assert!(response.output.len() < 512);
    assert!(!response.output.contains("sensitive-content"));
    let value = receipt(&response.output);
    assert_eq!(value["byte_len"], contents.len());
    assert_eq!(
        value["sha256"],
        hex::encode(Sha256::digest(contents.as_bytes()))
    );
    assert!(value.get("preview").is_none());
    std::fs::remove_dir_all(root).expect("cleanup");
}

#[test]
fn later_mutation_does_not_turn_receipt_into_freshness_or_consumption_proof() {
    let root = crate::tests::temp_dir("write-receipt-mutation");
    let path = root.join("answer.txt");
    let response = tool_write(
        serde_json::json!({"path": path, "contents": "original"}),
        None,
        "write-original",
    );
    assert!(!response.is_error);
    std::fs::write(&path, "changed later").expect("later mutation");
    let value = receipt(&response.output);
    assert_eq!(value["sha256"], hex::encode(Sha256::digest(b"original")));
    assert_ne!(
        value["sha256"],
        hex::encode(Sha256::digest(std::fs::read(&path).expect("read")))
    );
    assert_eq!(value["phase"], "written_at_operation");
    assert_eq!(value["later_freshness"], "not_assessed");
    assert_eq!(value["downstream_consumption"], "not_assessed");
    std::fs::remove_dir_all(root).expect("cleanup");
}
