use sha2::{Digest as _, Sha256};
use std::process::Command;

const FALLBACK_INPUT: &str = concat!(env!("CARGO_PKG_NAME"), "-", env!("CARGO_PKG_VERSION"));

fn main() {
    println!("cargo:rerun-if-env-changed=BCODE_BUILD_FINGERPRINT");
    if let Ok(value) = std::env::var("BCODE_BUILD_FINGERPRINT")
        && is_valid_fingerprint(&value)
    {
        println!("cargo:rustc-env=BCODE_BUILD_FINGERPRINT={value}");
        return;
    }

    println!("cargo:rerun-if-changed=../../Cargo.lock");
    println!("cargo:rerun-if-changed=../../Cargo.toml");
    println!("cargo:rerun-if-changed=../../packages");
    println!("cargo:rerun-if-changed=../../plugins");
    println!("cargo:rerun-if-changed=../../.git/HEAD");
    println!("cargo:rerun-if-changed=../../.git/index");

    let input = git_fingerprint_input().unwrap_or_else(|| FALLBACK_INPUT.to_string());
    println!(
        "cargo:rustc-env=BCODE_BUILD_FINGERPRINT={}",
        short_sha256(&input)
    );
}

fn git_fingerprint_input() -> Option<String> {
    let head = git_output(["rev-parse", "HEAD"])?;
    let status = git_output(["status", "--short"]).unwrap_or_default();
    let diff = git_output(["diff", "--binary", "HEAD"]).unwrap_or_default();
    let staged_diff = git_output(["diff", "--binary", "--cached"]).unwrap_or_default();
    Some(format!(
        "git-head={head}\ngit-status={status}\ngit-diff={diff}\ngit-staged-diff={staged_diff}"
    ))
}

fn git_output<const N: usize>(args: [&str; N]) -> Option<String> {
    let output = Command::new("git").args(args).output().ok()?;
    output.status.success().then(|| {
        String::from_utf8_lossy(&output.stdout)
            .trim_end()
            .to_string()
    })
}

fn short_sha256(input: &str) -> String {
    let digest = Sha256::digest(input.as_bytes());
    let hex = format!("{digest:x}");
    hex[..16].to_string()
}

fn is_valid_fingerprint(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
}
