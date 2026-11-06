//! Deliberate X-ray asset builds and an isolated, offline demo launch.

use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::{Result, format_error, run_command, sha256_bytes, workspace_root};

fn npm() -> Command {
    Command::new(if cfg!(windows) { "npm.cmd" } else { "npm" })
}

/// Build the locked web application explicitly, never from a Cargo build script.
pub fn build_web_dist(root: &Path) -> Result<PathBuf> {
    let web = root.join("packages/xray/web");
    let lock = fs::read(web.join("package-lock.json")).map_err(|error| {
        format_error(format!(
            "X-ray needs packages/xray/web/package-lock.json: {error}"
        ))
    })?;
    let mut inputs = fs::read(web.join("package.json"))?;
    inputs.push(0);
    inputs.extend(lock);
    let fingerprint = sha256_bytes(&inputs);
    let stamp = web.join("node_modules/.bcode-locked-inputs");
    let installed = fs::read_to_string(&stamp).is_ok_and(|value| value == fingerprint)
        && web.join("node_modules/typescript/package.json").is_file()
        && web.join("node_modules/vite/package.json").is_file();
    if !installed {
        run_command(npm().current_dir(&web).args([
            "ci",
            "--ignore-scripts",
            "--no-audit",
            "--no-fund",
        ]))?;
        fs::write(stamp, fingerprint)?;
    }
    run_command(npm().current_dir(&web).args(["run", "build"]))?;
    let dist = web.join("dist");
    if !dist.join("index.html").is_file() {
        return Err(format_error(
            "X-ray web build did not produce dist/index.html; refusing an asset-free application",
        ));
    }
    Ok(dist.canonicalize()?)
}

fn launch_args(args: Vec<OsString>) -> Result<Vec<OsString>> {
    let mut forwarded = Vec::new();
    let mut args = args.into_iter().peekable();
    if args.peek().is_some_and(|arg| arg == "--") {
        args.next();
    }
    while let Some(arg) = args.next() {
        match arg.to_str() {
            Some("--demo" | "--no-open" | "--require-pairing" | "--help" | "-h") => {
                forwarded.push(arg);
            }
            Some("--lifetime-seconds") => {
                let value = args
                    .next()
                    .ok_or_else(|| format_error("--lifetime-seconds needs a value"))?;
                if !value
                    .to_str()
                    .and_then(|value| value.parse::<u64>().ok())
                    .is_some_and(|value| (1..=86400).contains(&value))
                {
                    return Err(format_error("--lifetime-seconds must be 1..=86400"));
                }
                forwarded.extend([arg, value]);
            }
            _ => {
                return Err(format_error(
                    "xray-dev accepts --demo, --no-open, --require-pairing, --lifetime-seconds N, --help only; state/config/endpoint overrides are intentionally not forwarded",
                ));
            }
        }
    }
    if !forwarded.iter().any(|arg| arg == "--demo") {
        forwarded.insert(0, "--demo".into());
    }
    Ok(forwarded)
}

/// Build an isolated application artifact and invoke it by explicit absolute path.
/// This offline demo never selects a provider or submits a model turn.
pub fn run(args: Vec<OsString>) -> Result<()> {
    let args = launch_args(args)?;
    let root = workspace_root();
    let dist = build_web_dist(&root)?;
    let sandbox = tempfile::Builder::new()
        .prefix("bcode-xray-dev-")
        .tempdir()?;
    let target = sandbox.path().join("target");
    let host = crate::host_target();
    let state = sandbox.path().join("state");
    let config = sandbox.path().join("config.toml");
    fs::create_dir(&state)?;
    fs::write(
        &config,
        "# Isolated offline X-ray demo; no provider configuration.\n",
    )?;
    let artifact_id = crate::generated_artifact_id();
    run_command(
        Command::new("cargo")
            .current_dir(&root)
            .args([
                "build",
                "--locked",
                "--package",
                "bcode",
                "--bin",
                "bcode",
                "--no-default-features",
                "--features",
                "app",
            ])
            .arg("--target-dir")
            .arg(&target)
            .arg("--target")
            .arg(&host)
            .env_remove("CARGO_BUILD_TARGET")
            .env("BCODE_ARTIFACT_ID", &artifact_id)
            .env("BCODE_DISTRIBUTION_BUILD", "0")
            .env("BCODE_XRAY_WEB_DIST", &dist),
    )?;
    let binary =
        target
            .join(&host)
            .join("debug")
            .join(if cfg!(windows) { "bcode.exe" } else { "bcode" });
    let binary = binary.canonicalize()?;
    let probe = isolated_command(&binary, sandbox.path(), &config, &state)
        .arg("artifact-id")
        .output()?;
    if !probe.status.success() || String::from_utf8_lossy(&probe.stdout).trim() != artifact_id {
        return Err(format_error(
            "new X-ray executable artifact identity did not match this build; refusing launch",
        ));
    }
    let digest = crate::archive_sha256(&binary)?;
    let record = format!(
        "artifact_id={artifact_id}\nsha256={digest}\nexecutable={}\n",
        binary.display()
    );
    fs::write(sandbox.path().join("artifact.txt"), &record)?;
    // Preserve the state and artifact for diagnostics; never delete a live daemon's root.
    let sandbox = sandbox.keep();
    println!("X-ray isolated workspace: {}\n{record}", sandbox.display());
    let mut launch = isolated_command(&binary, &sandbox, &config, &state);
    launch
        .arg("--state-root")
        .arg(&state)
        .arg("xray")
        .args(args);
    run_command(&mut launch)
}

fn isolated_command(binary: &Path, sandbox: &Path, config: &Path, state: &Path) -> Command {
    let mut launch = Command::new(binary);
    for (key, _) in std::env::vars_os() {
        // Drop request/session/daemon routing overrides, but retain inherited auth selection.
        let name = key.to_string_lossy();
        if name.starts_with("BCODE_") && !name.starts_with("BCODE_AUTH_") {
            launch.env_remove(key);
        }
    }
    launch
        .current_dir(sandbox)
        .env("BCODE_CONFIG", config)
        .env("BCODE_STATE_DIR", state)
        .env("XDG_CONFIG_HOME", sandbox.join("config"));
    launch
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn demo_opens_by_default_and_forwards_no_open() {
        assert_eq!(launch_args(vec![]).unwrap(), vec![OsString::from("--demo")]);
        assert_eq!(
            launch_args(vec!["--".into(), "--no-open".into()]).unwrap(),
            vec![OsString::from("--demo"), OsString::from("--no-open")]
        );
    }

    #[test]
    fn rejects_scope_overrides_and_invalid_lifetimes() {
        for args in [
            vec!["--state-root", "/tmp"],
            vec!["--config=x"],
            vec!["--lifetime-seconds", "0"],
            vec!["--lifetime-seconds", "86401"],
            vec!["--lifetime-seconds"],
        ] {
            assert!(launch_args(args.into_iter().map(OsString::from).collect()).is_err());
        }
        assert!(
            launch_args(vec![
                "--demo".into(),
                "--lifetime-seconds".into(),
                "1".into()
            ])
            .is_ok()
        );
    }
}
