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
const DEV_CODESIGN_KEYCHAIN_RELATIVE_DIR: &str = "Library/Application Support/bcode/dev-signing";
const DEV_CODESIGN_KEYCHAIN_NAME: &str = "bcode-dev-signing.keychain-db";
const DEV_CODESIGN_PASSWORD_FILE: &str = "password";
const DEV_CODESIGN_P12_PASSWORD: &str = "bcode-dev-signing";

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
    UpdateTesseractCatalog,
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
    allow_create_dev_identity: bool,
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
            Some("update-tesseract-catalog") => CommandName::UpdateTesseractCatalog,
            Some("help" | "--help" | "-h") | None => CommandName::Help,
            Some(command) => {
                return Err(format_error(format!("unknown xtask command `{command}`")));
            }
        };

        let mut target = env::var("TARGET").unwrap_or_else(|_| host_target());
        let mut version = env::var("VERSION").unwrap_or_else(|_| workspace_version());
        let mut out_dir = PathBuf::from(DIST_DIR);
        let mut dev_binary = None;
        let env_dev_identity = env::var("BCODE_DEV_CODESIGN_IDENTITY").ok();
        let mut allow_create_dev_identity = env_dev_identity.is_none();
        let mut dev_identity =
            env_dev_identity.unwrap_or_else(|| DEFAULT_DEV_CODESIGN_IDENTITY.to_owned());
        let mut skip_notarize = env_flag("BCODE_SKIP_NOTARIZE");

        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--target" => target = require_value(&mut args, "--target")?,
                "--version" => version = require_value(&mut args, "--version")?,
                "--out-dir" => out_dir = PathBuf::from(require_value(&mut args, "--out-dir")?),
                "--binary" => {
                    dev_binary = Some(PathBuf::from(require_value(&mut args, "--binary")?));
                }
                "--identity" => {
                    dev_identity = require_value(&mut args, "--identity")?;
                    allow_create_dev_identity = false;
                }
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
            allow_create_dev_identity,
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
            allow_create_dev_identity: true,
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
        CommandName::UpdateTesseractCatalog => update_tesseract_catalog(),
        CommandName::Help => {
            print_help();
            Ok(())
        }
    }
}

fn update_tesseract_catalog() -> Result<()> {
    let root = workspace_root();
    let catalog_path = root.join("packages/tesseract-sys/bundled/catalog.toml");
    let catalog_text = fs::read_to_string(&catalog_path).map_err(|error| {
        format_error(format!(
            "failed to read {}: {error}",
            catalog_path.display()
        ))
    })?;
    let catalog = catalog_text
        .parse::<toml_edit::DocumentMut>()
        .map_err(|error| format_error(format!("failed to parse catalog TOML: {error}")))?;
    let versions = catalog_tesseract_versions(&catalog)?;
    println!(
        "bundled Tesseract catalog has {} supported version(s): {}",
        versions.len(),
        versions.join(", ")
    );
    println!(
        "feature names are generated from catalog versions; add versions in {}",
        catalog_path.display()
    );
    Ok(())
}

fn catalog_tesseract_versions(catalog: &toml_edit::DocumentMut) -> Result<Vec<String>> {
    let table = catalog
        .get("tesseract")
        .and_then(toml_edit::Item::as_table)
        .ok_or_else(|| format_error("catalog must contain a [tesseract] table"))?;
    let mut versions = table
        .iter()
        .filter(|&(_version, item)| item.is_table())
        .map(|(version, _item)| version.to_string())
        .collect::<Vec<_>>();
    versions.sort();
    Ok(versions)
}

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("xtask lives under workspace root")
        .to_path_buf()
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
            let (signing_identity, keychain) = ensure_dev_codesign_identity(
                &options.dev_identity,
                options.allow_create_dev_identity,
            )?;
            sign_macos_dev_binary(&binary, &signing_identity, keychain.as_deref())?;
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
    let (signing_identity, keychain) =
        ensure_dev_codesign_identity(&options.dev_identity, options.allow_create_dev_identity)?;
    sign_macos_dev_binary(&binary, &signing_identity, keychain.as_deref())?;
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

fn ensure_dev_codesign_identity(
    identity: &str,
    allow_create: bool,
) -> Result<(String, Option<PathBuf>)> {
    if allow_create && identity == DEFAULT_DEV_CODESIGN_IDENTITY {
        return ensure_default_dev_codesign_identity(identity);
    }

    if let Some(identity_hash) = codesign_identity_hash(identity)? {
        return Ok((identity_hash, None));
    }

    Err(format_error(format!(
        "code-signing identity `{identity}` was not found; create it or choose another with --identity"
    )))
}

fn ensure_default_dev_codesign_identity(identity: &str) -> Result<(String, Option<PathBuf>)> {
    if let Some(signing_identity) = existing_default_dev_codesign_identity(identity)? {
        return Ok(signing_identity);
    }

    println!("creating local development code-signing identity `{identity}`");
    create_and_verify_default_dev_codesign_identity(identity)?
        .map_or_else(|| recreate_unusable_dev_codesign_identity(identity), Ok)
}

fn existing_default_dev_codesign_identity(
    identity: &str,
) -> Result<Option<(String, Option<PathBuf>)>> {
    let keychain = dev_codesign_keychain_dir()?.join(DEV_CODESIGN_KEYCHAIN_NAME);
    let password_path = dev_codesign_keychain_dir()?.join(DEV_CODESIGN_PASSWORD_FILE);
    if !keychain.exists() || !password_path.exists() {
        return Ok(None);
    }

    let password = fs::read_to_string(password_path)?.trim().to_owned();
    configure_dev_codesign_keychain(&keychain, &password)?;
    let Some(identity_hash) = codesign_identity_hash_in_keychain(identity, &keychain)? else {
        return Ok(None);
    };

    if dev_codesign_identity_can_sign(&identity_hash, &keychain)? {
        Ok(Some((identity_hash, Some(keychain))))
    } else {
        Ok(None)
    }
}

fn recreate_unusable_dev_codesign_identity(identity: &str) -> Result<(String, Option<PathBuf>)> {
    println!("recreating unusable local development code-signing identity `{identity}`");
    let keychain_dir = dev_codesign_keychain_dir()?;
    if keychain_dir.exists() {
        fs::remove_dir_all(&keychain_dir)?;
    }
    create_and_verify_default_dev_codesign_identity(identity)?.map_or_else(
        || import_default_dev_identity_into_user_keychain(identity),
        Ok,
    )
}

fn import_default_dev_identity_into_user_keychain(
    identity: &str,
) -> Result<(String, Option<PathBuf>)> {
    println!("importing local development code-signing identity `{identity}` into user keychain");
    let keychain = default_user_keychain()?;
    if let Some(identity_hash) = codesign_identity_hash_in_keychain(identity, &keychain)?
        && dev_codesign_identity_can_sign(&identity_hash, &keychain)?
    {
        return Ok((identity_hash, Some(keychain)));
    }

    let certificate = PathBuf::from("target/xtask/dev-codesign/bcode-dev.cert.pem");
    let p12 = PathBuf::from("target/xtask/dev-codesign/bcode-dev.p12");

    ensure_file(&certificate)?;
    ensure_file(&p12)?;

    run_command(
        Command::new("security")
            .arg("import")
            .arg(&p12)
            .arg("-P")
            .arg(DEV_CODESIGN_P12_PASSWORD)
            .arg("-A")
            .arg("-f")
            .arg("pkcs12")
            .arg("-k")
            .arg(&keychain)
            .arg("-T")
            .arg("/usr/bin/codesign")
            .arg("-T")
            .arg("/usr/bin/security"),
    )?;

    let Some(identity_hash) = codesign_identity_hash_in_keychain(identity, &keychain)? else {
        return Err(format_error(format!(
            "imported `{identity}`, but codesign still cannot find it in the user keychain"
        )));
    };

    if dev_codesign_identity_can_sign(&identity_hash, &keychain)? {
        Ok((identity_hash, Some(keychain)))
    } else {
        Err(format_error(format!(
            "imported `{identity}`, but codesign cannot use it to sign"
        )))
    }
}

fn default_user_keychain() -> Result<PathBuf> {
    let output = command_output(
        Command::new("security")
            .arg("default-keychain")
            .arg("-d")
            .arg("user"),
    )?;
    let keychain = output.trim().trim_matches('"');
    if keychain.is_empty() {
        Err(format_error(
            "security default-keychain returned an empty path",
        ))
    } else {
        Ok(PathBuf::from(keychain))
    }
}

fn create_and_verify_default_dev_codesign_identity(
    identity: &str,
) -> Result<Option<(String, Option<PathBuf>)>> {
    let keychain = create_default_dev_codesign_identity(identity)?;
    let Some(identity_hash) = codesign_identity_hash_in_keychain(identity, &keychain)? else {
        return Ok(None);
    };

    if dev_codesign_identity_can_sign(&identity_hash, &keychain)? {
        Ok(Some((identity_hash, Some(keychain))))
    } else {
        Ok(None)
    }
}

fn codesign_identity_hash(identity: &str) -> Result<Option<String>> {
    let output = command_output(
        Command::new("security")
            .arg("find-identity")
            .arg("-v")
            .arg("-p")
            .arg("codesigning"),
    )?;
    Ok(find_identity_hash(&output, identity))
}

fn codesign_identity_hash_in_keychain(identity: &str, keychain: &Path) -> Result<Option<String>> {
    let output = command_output(
        Command::new("security")
            .arg("find-identity")
            .arg("-v")
            .arg("-p")
            .arg("codesigning")
            .arg(keychain),
    )?;
    Ok(find_identity_hash(&output, identity))
}

fn find_identity_hash(output: &str, identity: &str) -> Option<String> {
    output.lines().find_map(|line| {
        if line.contains(identity) {
            line.split_whitespace().nth(1).map(str::to_owned)
        } else {
            None
        }
    })
}

fn create_default_dev_codesign_identity(identity: &str) -> Result<PathBuf> {
    let keychain_dir = dev_codesign_keychain_dir()?;
    fs::create_dir_all(&keychain_dir)?;
    let keychain = keychain_dir.join(DEV_CODESIGN_KEYCHAIN_NAME);
    let password_path = keychain_dir.join(DEV_CODESIGN_PASSWORD_FILE);
    let password = ensure_dev_codesign_password(&password_path)?;

    if !keychain.exists() {
        run_sensitive_command(
            Command::new("security")
                .arg("create-keychain")
                .arg("-p")
                .arg(&password)
                .arg(&keychain),
            "security create-keychain <bcode dev keychain>",
        )?;
    }

    configure_dev_codesign_keychain(&keychain, &password)?;

    let dir = PathBuf::from("target/xtask/dev-codesign");
    recreate_dir(&dir)?;
    let key = dir.join("bcode-dev.key.pem");
    let certificate = dir.join("bcode-dev.cert.pem");
    let p12 = dir.join("bcode-dev.p12");

    run_command(
        Command::new("openssl")
            .arg("req")
            .arg("-new")
            .arg("-newkey")
            .arg("rsa:2048")
            .arg("-x509")
            .arg("-days")
            .arg("3650")
            .arg("-nodes")
            .arg("-subj")
            .arg(format!("/CN={identity}/"))
            .arg("-addext")
            .arg("basicConstraints=critical,CA:FALSE")
            .arg("-addext")
            .arg("keyUsage=critical,digitalSignature")
            .arg("-addext")
            .arg("extendedKeyUsage=codeSigning")
            .arg("-keyout")
            .arg(&key)
            .arg("-out")
            .arg(&certificate),
    )?;

    run_command(
        Command::new("openssl")
            .arg("pkcs12")
            .arg("-export")
            .arg("-out")
            .arg(&p12)
            .arg("-inkey")
            .arg(&key)
            .arg("-in")
            .arg(&certificate)
            .arg("-passout")
            .arg(format!("pass:{DEV_CODESIGN_P12_PASSWORD}")),
    )?;

    run_command(
        Command::new("security")
            .arg("import")
            .arg(&p12)
            .arg("-P")
            .arg(DEV_CODESIGN_P12_PASSWORD)
            .arg("-f")
            .arg("pkcs12")
            .arg("-k")
            .arg(&keychain)
            .arg("-T")
            .arg("/usr/bin/codesign")
            .arg("-T")
            .arg("/usr/bin/security"),
    )?;

    trust_dev_codesign_certificate(&certificate, &keychain)?;

    run_sensitive_command(
        Command::new("security")
            .arg("set-key-partition-list")
            .arg("-S")
            .arg("apple-tool:,apple:,codesign:")
            .arg("-s")
            .arg("-k")
            .arg(&password)
            .arg(&keychain),
        "security set-key-partition-list <bcode dev keychain>",
    )?;

    Ok(keychain)
}

fn trust_dev_codesign_certificate(certificate: &Path, keychain: &Path) -> Result<()> {
    run_command(
        Command::new("security")
            .arg("add-trusted-cert")
            .arg("-r")
            .arg("trustRoot")
            .arg("-p")
            .arg("codeSign")
            .arg("-k")
            .arg(keychain)
            .arg(certificate),
    )
}

fn configure_dev_codesign_keychain(keychain: &Path, password: &str) -> Result<()> {
    run_sensitive_command(
        Command::new("security")
            .arg("unlock-keychain")
            .arg("-p")
            .arg(password)
            .arg(keychain),
        "security unlock-keychain <bcode dev keychain>",
    )?;

    run_command(
        Command::new("security")
            .arg("set-keychain-settings")
            .arg("-lut")
            .arg("21600")
            .arg(keychain),
    )?;
    add_keychain_to_user_search_list(keychain)
}

fn dev_codesign_identity_can_sign(identity_hash: &str, keychain: &Path) -> Result<bool> {
    let probe = PathBuf::from("target/xtask/dev-codesign/codesign-probe");
    if let Some(parent) = probe.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::copy("/usr/bin/true", &probe).map_err(|error| {
        format_error(format!(
            "failed to create codesign probe {}: {error}",
            probe.display()
        ))
    })?;

    let status = Command::new("codesign")
        .arg("--force")
        .arg("--keychain")
        .arg(keychain)
        .arg("--sign")
        .arg(identity_hash)
        .arg(&probe)
        .status()
        .map_err(|error| format_error(format!("failed to run codesign probe: {error}")))?;

    Ok(status.success())
}

fn add_keychain_to_user_search_list(keychain: &Path) -> Result<()> {
    let output = command_output(
        Command::new("security")
            .arg("list-keychains")
            .arg("-d")
            .arg("user"),
    )?;

    let keychain_text = keychain.to_string_lossy();
    let mut keychains = vec![keychain.to_path_buf()];
    keychains.extend(output.lines().filter_map(|line| {
        let existing = line.trim().trim_matches('"');
        if existing.is_empty() || existing == keychain_text {
            None
        } else {
            Some(PathBuf::from(existing))
        }
    }));

    let mut command = Command::new("security");
    command
        .arg("list-keychains")
        .arg("-d")
        .arg("user")
        .arg("-s");
    for listed_keychain in keychains {
        command.arg(listed_keychain);
    }
    run_command(&mut command)
}

fn dev_codesign_keychain_dir() -> Result<PathBuf> {
    let home = env::var("HOME").map_err(|_| format_error("HOME is required for dev signing"))?;
    Ok(PathBuf::from(home).join(DEV_CODESIGN_KEYCHAIN_RELATIVE_DIR))
}

fn ensure_dev_codesign_password(path: &Path) -> Result<String> {
    if path.exists() {
        return fs::read_to_string(path)
            .map(|password| password.trim().to_owned())
            .map_err(Into::into);
    }

    let password = command_output(Command::new("openssl").arg("rand").arg("-hex").arg("32"))?
        .trim()
        .to_owned();
    fs::write(path, format!("{password}\n"))?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    }

    Ok(password)
}

fn sign_macos_dev_binary(binary: &Path, identity: &str, keychain: Option<&Path>) -> Result<()> {
    let mut command = Command::new("codesign");
    command.arg("--force");
    if let Some(keychain) = keychain {
        command.arg("--keychain").arg(keychain);
    }
    command.arg("--sign").arg(identity).arg(binary);
    run_command(&mut command)
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

fn run_sensitive_command(command: &mut Command, display: &str) -> Result<()> {
    println!("running: {display}");
    let status = command
        .status()
        .map_err(|error| format_error(format!("failed to run {display}: {error}")))?;
    if status.success() {
        Ok(())
    } else {
        Err(format_error(format!(
            "command failed with {status}: {display}"
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
           cargo xtask dev-sign --target <triple> [--binary <path>] [--identity <name>]\n\
           cargo xtask update-tesseract-catalog\n\n\
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
