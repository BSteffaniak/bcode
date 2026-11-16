//! Feature gates are checked as real consumers, without this package's dev-dependencies.

use std::{path::Path, process::Output};

#[test]
fn testing_surface_requires_opt_in_feature() {
    check_opt_in(
        "testing",
        "tests/ui/default_fail/testing_feature_is_opt_in.rs",
    );
}

#[test]
fn evaluation_surface_requires_opt_in_feature() {
    check_opt_in(
        "evaluation",
        "tests/ui/default_fail/evaluation_feature_is_opt_in.rs",
    );
}

fn check_opt_in(feature: &str, source: &str) {
    let package = Path::new(env!("CARGO_MANIFEST_DIR"));
    let workspace = package.parent().unwrap().parent().unwrap();
    let fixture = tempfile::tempdir().expect("isolated feature consumer");
    let root = fixture.path();
    std::fs::create_dir(root.join("src")).unwrap();
    std::fs::copy(package.join(source), root.join("src/main.rs")).unwrap();
    // Preserve the selected repository dependency revisions and local patches,
    // but never copy dev-dependencies that can re-enable bcode/testing through
    // another workspace consumer. An empty workspace prevents parent discovery.
    std::fs::copy(workspace.join("Cargo.lock"), root.join("Cargo.lock")).unwrap();
    let patches = workspace_patches(workspace);
    write_consumer_manifest(root, package, None, &patches);
    let disabled = check_consumer(root, workspace);
    assert!(
        !disabled.status.success(),
        "{feature} leaked into a default consumer"
    );
    let errors = compiler_errors(&disabled);
    assert!(
        !errors.is_empty()
            && errors.iter().all(|error| {
                error["code"]["code"] == "E0432"
                    && error["message"]
                        .as_str()
                        .is_some_and(|message| message.contains(&format!("bcode::{feature}")))
            }),
        "expected only the gated import failure, got:\n{}\n{}",
        String::from_utf8_lossy(&disabled.stdout),
        String::from_utf8_lossy(&disabled.stderr),
    );
    // The same API must compile when explicitly enabled. This also prevents an
    // unrelated dependency or fixture error from masquerading as a gate check.
    write_consumer_manifest(root, package, Some(feature), &patches);
    let enabled = check_consumer(root, workspace);
    assert!(
        enabled.status.success(),
        "explicit {feature} consumer failed:\n{}\n{}",
        String::from_utf8_lossy(&enabled.stdout),
        String::from_utf8_lossy(&enabled.stderr),
    );
}

fn workspace_patches(workspace: &Path) -> String {
    let manifest = std::fs::read_to_string(workspace.join("Cargo.toml")).unwrap();
    let mut copying = false;
    let mut patches = String::new();
    for line in manifest.lines() {
        if line.trim_start().starts_with('[') {
            copying = line.trim_start().starts_with("[patch.");
        }
        if copying {
            patches.push_str(line);
            patches.push('\n');
        }
    }
    patches
}

fn write_consumer_manifest(root: &Path, package: &Path, feature: Option<&str>, patches: &str) {
    let features =
        feature.map_or_else(String::new, |feature| format!("features = [{feature:?}]\n"));
    // JSON string quoting is also valid TOML basic-string quoting for these paths.
    let path = serde_json::to_string(package.to_str().unwrap()).unwrap();
    let manifest = format!(
        "[package]\nname = \"bcode-feature-gate-consumer\"\nversion = \"0.0.0\"\nedition = \"2024\"\n\
         [workspace]\n\
         [dependencies.bcode]\npath = {path}\ndefault-features = false\n{features}\n{patches}",
    );
    std::fs::write(root.join("Cargo.toml"), manifest).unwrap();
}

fn check_consumer(root: &Path, workspace: &Path) -> Output {
    std::process::Command::new(std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into()))
        .args([
            "check",
            "--offline",
            "--message-format=json",
            "--manifest-path",
        ])
        .arg(root.join("Cargo.toml"))
        .arg("--target-dir")
        .arg(workspace.join("target/feature-gate-consumers"))
        .current_dir(workspace)
        .output()
        .expect("run isolated Cargo feature check")
}

fn compiler_errors(output: &Output) -> Vec<serde_json::Value> {
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .filter(|message| message["reason"] == "compiler-message")
        .map(|message| message["message"].clone())
        .filter(|message| message["level"] == "error")
        .collect()
}
