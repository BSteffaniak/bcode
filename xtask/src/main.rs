#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Bcode release automation tasks.

use std::env;
use std::ffi::OsStr;
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

const PACKAGE_NAME: &str = "bcode";
const BINARY_NAME: &str = "bcode";
const DIST_DIR: &str = "target/dist";
const DEFAULT_DEV_CODESIGN_IDENTITY: &str = "Bcode Dev";

#[derive(Debug)]
struct XtaskError(String);

type Result<T> = std::result::Result<T, XtaskError>;

impl fmt::Display for XtaskError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for XtaskError {}

impl From<io::Error> for XtaskError {
    fn from(error: io::Error) -> Self {
        Self(error.to_string())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CommandName {
    Release,
    VerifyRelease,
    DevSign,
    DevRelease,
    Help,
}

#[derive(Debug)]
struct Options {
    command: CommandName,
    target: String,
    version: String,
    out_dir: PathBuf,
    dev_binary: Option<PathBuf>,
    dev_identity: String,
    skip_notarize: bool,
}

impl Options {
    fn parse() -> Result<Self> {
        let mut args = env::args().skip(1);
        let command = match args.next().as_deref() {
            Some("release") => CommandName::Release,
            Some("verify-release") => CommandName::VerifyRelease,
            Some("dev-sign") => CommandName::DevSign,
            Some("dev-release") => CommandName::DevRelease,
            Some("help" | "--help" | "-h") | None => CommandName::Help,
            Some(command) => {
                return Err(format_error(format!("unknown xtask command `{command}`")));
            }
        };

        let mut target = env::var("TARGET").unwrap_or_else(|_| host_target());
        let mut version = env::var("VERSION").unwrap_or_else(|_| workspace_version());
        let mut out_dir = PathBuf::from(DIST_DIR);
        let mut dev_binary = None;
        let mut dev_identity = env::var("BCODE_DEV_CODESIGN_IDENTITY")
            .unwrap_or_else(|_| DEFAULT_DEV_CODESIGN_IDENTITY.to_owned());
        let mut skip_notarize = env_flag("BCODE_SKIP_NOTARIZE");

        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--target" => target = require_value(&mut args, "--target")?,
                "--version" => version = require_value(&mut args, "--version")?,
                "--out-dir" => out_dir = PathBuf::from(require_value(&mut args, "--out-dir")?),
                "--binary" => {
                    dev_binary = Some(PathBuf::from(require_value(&mut args, "--binary")?));
                }
                "--identity" => dev_identity = require_value(&mut args, "--identity")?,
                "--skip-notarize" => skip_notarize = true,
                "--help" | "-h" => return Ok(Self::help()),
                unknown => return Err(format_error(format!("unknown option `{unknown}`"))),
            }
        }

        Ok(Self {
            command,
            target,
            version,
            out_dir,
            dev_binary,
            dev_identity,
            skip_notarize,
        })
    }

    fn help() -> Self {
        Self {
            command: CommandName::Help,
            target: host_target(),
            version: workspace_version(),
            out_dir: PathBuf::from(DIST_DIR),
            dev_binary: None,
            dev_identity: DEFAULT_DEV_CODESIGN_IDENTITY.to_owned(),
            skip_notarize: false,
        }
    }
}

fn main() {
    if let Err(error) = run() {
        eprintln!("error: {error}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let options = Options::parse()?;
    match options.command {
        CommandName::Release => release(&options),
        CommandName::VerifyRelease => verify_release(&options),
        CommandName::DevSign => dev_sign(&options),
        CommandName::DevRelease => dev_release(&options),
        CommandName::Help => {
            print_help();
            Ok(())
        }
    }
}

fn build_bcode_release(target: &str) -> Result<()> {
    run_command(
        Command::new("cargo")
            .arg("build")
            .arg("--release")
            .arg("--package")
            .arg(PACKAGE_NAME)
            .arg("--target")
            .arg(target),
    )
}

fn release(options: &Options) -> Result<()> {
    let target_kind = TargetKind::parse(&options.target)?;
    build_bcode_release(&options.target)?;

    let binary = built_binary(&options.target);
    if target_kind == TargetKind::Macos {
        sign_macos_release_binary(&binary)?;
        verify_macos_signature(&binary)?;
    } else if target_kind == TargetKind::Linux {
        strip_binary(&binary);
    }

    let staging_dir = staging_dir(options);
    recreate_dir(&staging_dir)?;
    let staged_binary = staging_dir.join(binary_file_name(target_kind));
    fs::copy(&binary, &staged_binary).map_err(|error| {
        format_error(format!(
            "failed to copy {} to {}: {error}",
            binary.display(),
            staged_binary.display()
        ))
    })?;

    let archive = archive_path(options, target_kind);
    if archive.exists() {
        fs::remove_file(&archive)?;
    }
    create_archive(&archive, &staging_dir)?;

    if target_kind == TargetKind::Macos && !options.skip_notarize {
        notarize_macos_archive(&archive)?;
    }

    write_checksum(&archive)?;
    println!("release artifact: {}", archive.display());
    Ok(())
}

fn verify_release(options: &Options) -> Result<()> {
    let target_kind = TargetKind::parse(&options.target)?;
    let archive = archive_path(options, target_kind);
    ensure_file(&archive)?;
    ensure_file(&checksum_path(&archive))?;
    verify_checksum(&archive)?;

    if target_kind == TargetKind::Macos {
        let binary = built_binary(&options.target);
        ensure_file(&binary)?;
        verify_macos_signature(&binary)?;
    }

    println!("verified release artifact: {}", archive.display());
    Ok(())
}

fn dev_release(options: &Options) -> Result<()> {
    let target_kind = TargetKind::parse(&options.target)?;
    build_bcode_release(&options.target)?;
    let binary = built_binary(&options.target);
    ensure_file(&binary)?;

    match target_kind {
        TargetKind::Macos => {
            sign_macos_binary(&binary, &options.dev_identity, false)?;
            verify_macos_signature(&binary)?;
            println!(
                "dev release ready: {} signed with identity `{}`",
                binary.display(),
                options.dev_identity
            );
        }
        TargetKind::Linux => {
            strip_binary(&binary);
            println!("dev release ready: {}", binary.display());
        }
    }

    Ok(())
}

fn dev_sign(options: &Options) -> Result<()> {
    let target_kind = TargetKind::parse(&options.target)?;
    if target_kind != TargetKind::Macos {
        return Err(format_error(
            "dev-sign is currently only supported on macOS",
        ));
    }

    let binary = options
        .dev_binary
        .clone()
        .unwrap_or_else(|| built_binary(&options.target));
    ensure_file(&binary)?;
    sign_macos_binary(&binary, &options.dev_identity, false)?;
    verify_macos_signature(&binary)?;
    println!(
        "dev-signed {} with identity `{}`",
        binary.display(),
        options.dev_identity
    );
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TargetKind {
    Macos,
    Linux,
}

impl TargetKind {
    fn parse(target: &str) -> Result<Self> {
        if target.contains("apple-darwin") {
            Ok(Self::Macos)
        } else if target.contains("linux") {
            Ok(Self::Linux)
        } else {
            Err(format_error(format!(
                "unsupported release target `{target}`; v1 supports macOS and Linux"
            )))
        }
    }
}

fn sign_macos_release_binary(binary: &Path) -> Result<()> {
    let identity = env::var("APPLE_CODESIGN_IDENTITY").map_err(|_| {
        format_error("APPLE_CODESIGN_IDENTITY is required for macOS release signing")
    })?;
    sign_macos_binary(binary, &identity, true)
}

fn sign_macos_binary(binary: &Path, identity: &str, hardened_runtime: bool) -> Result<()> {
    let mut command = Command::new("codesign");
    command.arg("--force");
    if hardened_runtime {
        command.arg("--options").arg("runtime");
        command.arg("--timestamp");
    }
    command.arg("--sign").arg(identity).arg(binary);
    run_command(&mut command)
}

fn verify_macos_signature(binary: &Path) -> Result<()> {
    run_command(
        Command::new("codesign")
            .arg("--verify")
            .arg("--strict")
            .arg("--verbose=2")
            .arg(binary),
    )?;
    run_command(
        Command::new("codesign")
            .arg("-dv")
            .arg("--verbose=4")
            .arg(binary),
    )
}

fn notarize_macos_archive(archive: &Path) -> Result<()> {
    let Ok(apple_id) = env::var("APPLE_ID") else {
        println!("APPLE_ID not set; skipping notarization");
        return Ok(());
    };
    let password = env::var("APPLE_APP_SPECIFIC_PASSWORD").map_err(|_| {
        format_error("APPLE_APP_SPECIFIC_PASSWORD is required when APPLE_ID is set")
    })?;
    let team_id = env::var("APPLE_TEAM_ID")
        .map_err(|_| format_error("APPLE_TEAM_ID is required when APPLE_ID is set"))?;

    run_command(
        Command::new("xcrun")
            .arg("notarytool")
            .arg("submit")
            .arg(archive)
            .arg("--apple-id")
            .arg(apple_id)
            .arg("--password")
            .arg(password)
            .arg("--team-id")
            .arg(team_id)
            .arg("--wait"),
    )
}

fn strip_binary(binary: &Path) {
    match Command::new("strip").arg(binary).status() {
        Ok(status) if status.success() => println!("stripped {}", binary.display()),
        Ok(_) | Err(_) => println!("strip unavailable or failed; continuing without stripping"),
    }
}

fn create_archive(archive: &Path, staging_dir: &Path) -> Result<()> {
    let parent = archive
        .parent()
        .ok_or_else(|| format_error("archive path has no parent directory"))?;
    fs::create_dir_all(parent)?;
    let file_name = archive
        .file_name()
        .ok_or_else(|| format_error("archive path has no file name"))?;

    if archive
        .extension()
        .is_some_and(|extension| extension == "zip")
    {
        run_command(
            Command::new("ditto")
                .arg("-c")
                .arg("-k")
                .arg("--sequesterRsrc")
                .arg("--keepParent")
                .arg(staging_dir)
                .arg(file_name)
                .current_dir(parent),
        )
    } else {
        run_command(
            Command::new("tar")
                .arg("-czf")
                .arg(file_name)
                .arg("-C")
                .arg(staging_dir)
                .arg(".")
                .current_dir(parent),
        )
    }
}

fn write_checksum(archive: &Path) -> Result<()> {
    let digest = command_output(Command::new("shasum").arg("-a").arg("256").arg(archive))?;
    let checksum = checksum_path(archive);
    fs::write(&checksum, digest).map_err(|error| {
        format_error(format!(
            "failed to write checksum {}: {error}",
            checksum.display()
        ))
    })
}

fn verify_checksum(archive: &Path) -> Result<()> {
    let checksum = checksum_path(archive);
    run_command(
        Command::new("shasum")
            .arg("-a")
            .arg("256")
            .arg("-c")
            .arg(&checksum),
    )
}

fn built_binary(target: &str) -> PathBuf {
    PathBuf::from("target")
        .join(target)
        .join("release")
        .join(BINARY_NAME)
}

fn staging_dir(options: &Options) -> PathBuf {
    options.out_dir.join("staging").join(artifact_stem(options))
}

fn archive_path(options: &Options, target_kind: TargetKind) -> PathBuf {
    let extension = match target_kind {
        TargetKind::Macos => "zip",
        TargetKind::Linux => "tar.gz",
    };
    options
        .out_dir
        .join(format!("{}.{extension}", artifact_stem(options)))
}

fn artifact_stem(options: &Options) -> String {
    format!("{BINARY_NAME}-{}-{}", options.version, options.target)
}

fn checksum_path(archive: &Path) -> PathBuf {
    PathBuf::from(format!("{}.sha256", archive.display()))
}

const fn binary_file_name(_target_kind: TargetKind) -> &'static str {
    BINARY_NAME
}

fn recreate_dir(path: &Path) -> Result<()> {
    if path.exists() {
        fs::remove_dir_all(path)?;
    }
    fs::create_dir_all(path)?;
    Ok(())
}

fn ensure_file(path: &Path) -> Result<()> {
    if path.is_file() {
        Ok(())
    } else {
        Err(format_error(format!("expected file {}", path.display())))
    }
}

fn run_command(command: &mut Command) -> Result<()> {
    println!("running: {}", display_command(command));
    let status = command.status().map_err(|error| {
        format_error(format!(
            "failed to run {}: {error}",
            display_command(command)
        ))
    })?;
    if status.success() {
        Ok(())
    } else {
        Err(format_error(format!(
            "command failed with {status}: {}",
            display_command(command)
        )))
    }
}

fn command_output(command: &mut Command) -> Result<String> {
    println!("running: {}", display_command(command));
    let output = command
        .stdout(Stdio::piped())
        .output()
        .map_err(|error| format_error(format!("failed to run command: {error}")))?;
    if output.status.success() {
        String::from_utf8(output.stdout)
            .map_err(|error| format_error(format!("command output was not UTF-8: {error}")))
    } else {
        Err(format_error(format!(
            "command failed with {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        )))
    }
}

fn display_command(command: &Command) -> String {
    let mut parts = Vec::new();
    parts.push(command.get_program().to_string_lossy().into_owned());
    parts.extend(command.get_args().map(shell_quote));
    parts.join(" ")
}

fn shell_quote(value: &OsStr) -> String {
    let text = value.to_string_lossy();
    if text.contains(' ') {
        format!("'{text}'")
    } else {
        text.into_owned()
    }
}

fn require_value(args: &mut impl Iterator<Item = String>, name: &str) -> Result<String> {
    args.next()
        .ok_or_else(|| format_error(format!("{name} requires a value")))
}

fn env_flag(name: &str) -> bool {
    env::var(name).is_ok_and(|value| matches!(value.as_str(), "1" | "true" | "yes" | "on"))
}

fn host_target() -> String {
    let os = env::consts::OS;
    let arch = env::consts::ARCH;
    match (arch, os) {
        ("aarch64", "macos") => "aarch64-apple-darwin".to_owned(),
        ("x86_64", "macos") => "x86_64-apple-darwin".to_owned(),
        ("aarch64", "linux") => "aarch64-unknown-linux-gnu".to_owned(),
        ("x86_64", "linux") => "x86_64-unknown-linux-gnu".to_owned(),
        _ => format!("{arch}-{os}"),
    }
}

fn workspace_version() -> String {
    env!("CARGO_PKG_VERSION").to_owned()
}

fn format_error(message: impl Into<String>) -> XtaskError {
    XtaskError(message.into())
}

fn print_help() {
    println!(
        "Bcode release tasks\n\n\
         Usage:\n\
           cargo xtask release --target <triple> --version <version>\n\
           cargo xtask verify-release --target <triple> --version <version>\n\
           cargo xtask dev-release [--target <triple>] [--identity <name>]\n\
           cargo xtask dev-sign --target <triple> [--binary <path>] [--identity <name>]\n\n\
         Supported release targets:\n\
           * aarch64-apple-darwin\n\
           * x86_64-apple-darwin\n\
           * aarch64-unknown-linux-gnu\n\
           * x86_64-unknown-linux-gnu\n\n\
         macOS release env:\n\
           * APPLE_CODESIGN_IDENTITY\n\
           * APPLE_ID, APPLE_APP_SPECIFIC_PASSWORD, APPLE_TEAM_ID for notarization\n\n\
         macOS dev signing:\n\
           * defaults to `Bcode Dev`\n\
           * override with --identity or BCODE_DEV_CODESIGN_IDENTITY"
    );
}
