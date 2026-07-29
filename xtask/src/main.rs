#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Bcode release automation tasks.

use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::ffi::OsStr;
use std::fmt;
use std::fmt::Write as FmtWrite;
use std::fs::{self, File};
use std::io::{self, Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use sha2::{Digest, Sha256};

const PACKAGE_NAME: &str = "bcode";
const BINARY_NAME: &str = "bcode";
const MERMAID_WORKER_BINARY_NAME: &str = "bcode-mermaid-worker";
const DIST_DIR: &str = "target/dist";
const DEFAULT_DEV_CODESIGN_IDENTITY: &str = "Bcode Dev";
const DEV_CODESIGN_KEYCHAIN_RELATIVE_DIR: &str = "Library/Application Support/bcode/dev-signing";
const DEV_CODESIGN_KEYCHAIN_NAME: &str = "bcode-dev-signing.keychain-db";
const DEV_CODESIGN_PASSWORD_FILE: &str = "password";
const DEV_CODESIGN_P12_PASSWORD: &str = "bcode-dev-signing";
const WINDOWS_TIMESTAMP_URL: &str = "http://timestamp.digicert.com";

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

impl From<zip::result::ZipError> for XtaskError {
    fn from(error: zip::result::ZipError) -> Self {
        Self(error.to_string())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CommandName {
    Build,
    Release,
    VerifyRelease,
    DevSign,
    DevRelease,
    UpdateTesseractCatalog,
    DiscoverTesseractUpstream,
    UpdateTesseractPolicy,
    PackageTesseractRuntimes,
    SmokeTestTesseract,
    Help,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GeneratedWriteMode {
    Write,
    Check,
    DryRun,
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
    generated_write_mode: GeneratedWriteMode,
    prune_tesseract_policy: bool,
    features: Option<Vec<String>>,
}

impl Options {
    fn parse() -> Result<Self> {
        let mut args = env::args().skip(1);
        let command = match args.next().as_deref() {
            Some("build") => CommandName::Build,
            Some("release") => CommandName::Release,
            Some("verify-release") => CommandName::VerifyRelease,
            Some("dev-sign") => CommandName::DevSign,
            Some("dev-release") => CommandName::DevRelease,
            Some("update-tesseract-catalog") => CommandName::UpdateTesseractCatalog,
            Some("discover-tesseract-upstream") => CommandName::DiscoverTesseractUpstream,
            Some("update-tesseract-policy") => CommandName::UpdateTesseractPolicy,
            Some("package-tesseract-runtimes") => CommandName::PackageTesseractRuntimes,
            Some("smoke-test-tesseract" | "tesseract-smoke") => CommandName::SmokeTestTesseract,
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
        let mut generated_write_mode = GeneratedWriteMode::Write;
        let mut prune_tesseract_policy = false;
        let mut features = None;

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
                "--features" => {
                    let value = require_value(&mut args, "--features")?;
                    features
                        .get_or_insert_with(Vec::new)
                        .extend(parse_features(&value)?);
                }
                "--skip-notarize" => skip_notarize = true,
                "--check" => generated_write_mode = GeneratedWriteMode::Check,
                "--dry-run" => generated_write_mode = GeneratedWriteMode::DryRun,
                "--prune" => prune_tesseract_policy = true,
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
            generated_write_mode,
            prune_tesseract_policy,
            features,
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
            generated_write_mode: GeneratedWriteMode::Write,
            prune_tesseract_policy: false,
            features: None,
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
        CommandName::Build => build(&options),
        CommandName::Release => release(&options),
        CommandName::VerifyRelease => verify_release(&options),
        CommandName::DevSign => dev_sign(&options),
        CommandName::DevRelease => dev_release(&options),
        CommandName::UpdateTesseractCatalog => update_tesseract_catalog(&options),
        CommandName::DiscoverTesseractUpstream => discover_tesseract_upstream(),
        CommandName::UpdateTesseractPolicy => update_tesseract_policy(&options),
        CommandName::PackageTesseractRuntimes => package_tesseract_runtimes(&options),
        CommandName::SmokeTestTesseract => smoke_test_tesseract(&options),
        CommandName::Help => {
            print_help();
            Ok(())
        }
    }
}

#[derive(Debug, Clone)]
struct TesseractSyncPolicy {
    versions: Vec<String>,
    default: String,
    latest: String,
    leptonica_default: String,
    tessdata_flavor: String,
    tessdata_repo: String,
    tessdata_commit: String,
    tessdata_languages: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct VersionTag {
    major: u64,
    minor: u64,
    patch: u64,
}

impl VersionTag {
    fn parse(tag: &str) -> Option<Self> {
        let tag = tag.strip_prefix('v').unwrap_or(tag);
        let mut parts = tag.split('.');
        let major = parts.next()?.parse().ok()?;
        let minor = parts.next()?.parse().ok()?;
        let patch = parts.next()?.parse().ok()?;
        parts.next().is_none().then_some(Self {
            major,
            minor,
            patch,
        })
    }
}

impl fmt::Display for VersionTag {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

#[derive(Debug)]
struct UpstreamTesseractState {
    policy: TesseractSyncPolicy,
    tesseract_versions: Vec<VersionTag>,
    leptonica_versions: Vec<VersionTag>,
    tessdata_commit: String,
}

#[derive(Debug)]
struct TesseractPolicyUpdate {
    policy: TesseractSyncPolicy,
    tesseract_added: Vec<String>,
    leptonica_changed: bool,
    tessdata_changed: bool,
}

#[derive(Debug)]
struct ResolvedTesseractVersion {
    version: String,
    url: String,
    sha256: String,
    leptonica: String,
}

#[derive(Debug)]
struct ResolvedArtifact {
    url: String,
    sha256: String,
}

fn discover_tesseract_upstream() -> Result<()> {
    let state = discover_upstream_tesseract_state()?;
    let update = recommended_tesseract_policy_update(&state);
    let latest_tesseract = state
        .tesseract_versions
        .last()
        .ok_or_else(|| format_error("no upstream Tesseract versions discovered"))?
        .to_string();
    let latest_leptonica = state
        .leptonica_versions
        .last()
        .ok_or_else(|| format_error("no upstream Leptonica versions discovered"))?
        .to_string();

    println!(
        "current policy Tesseract versions: {}",
        state.policy.versions.join(", ")
    );
    println!("latest upstream Tesseract: {latest_tesseract}");
    println!(
        "current policy Leptonica: {}",
        state.policy.leptonica_default
    );
    println!("latest upstream Leptonica: {latest_leptonica}");
    println!("current tessdata commit: {}", state.policy.tessdata_commit);
    println!("latest tessdata commit: {}", state.tessdata_commit);
    if update.tesseract_added.is_empty() && !update.leptonica_changed && !update.tessdata_changed {
        println!("policy is already up to date with discovered upstream state");
    } else {
        println!("recommended policy update:");
        if !update.tesseract_added.is_empty() {
            println!("  add Tesseract: {}", update.tesseract_added.join(", "));
        }
        if update.leptonica_changed {
            println!("  set Leptonica: {}", update.policy.leptonica_default);
        }
        if update.tessdata_changed {
            println!("  pin tessdata commit: {}", update.policy.tessdata_commit);
        }
        println!("run: cargo xtask update-tesseract-policy");
    }
    Ok(())
}

fn update_tesseract_policy(options: &Options) -> Result<()> {
    let root = workspace_root();
    let policy_path = root.join("packages/tesseract-sys/bundled/sync-policy.toml");
    let mut update = recommended_tesseract_policy_update(&discover_upstream_tesseract_state()?);
    if options.prune_tesseract_policy {
        let latest = update.policy.latest.clone();
        update.policy.versions.retain(|version| version == &latest);
        update.tesseract_added.clear();
    }
    let rendered = render_tesseract_sync_policy(&update.policy);
    write_generated_file(&policy_path, &rendered, options)?;
    println!(
        "updated Tesseract policy: default={}, latest={}, versions={}, leptonica={}, tessdata={}",
        update.policy.default,
        update.policy.latest,
        update.policy.versions.join(","),
        update.policy.leptonica_default,
        update.policy.tessdata_commit
    );
    update_tesseract_catalog(options)
}

fn discover_upstream_tesseract_state() -> Result<UpstreamTesseractState> {
    let policy_path = workspace_root().join("packages/tesseract-sys/bundled/sync-policy.toml");
    let policy = read_tesseract_sync_policy(&policy_path)?;
    Ok(UpstreamTesseractState {
        tesseract_versions: github_semver_tags("tesseract-ocr", "tesseract")?
            .into_iter()
            .filter(|version| version.major == 5)
            .collect(),
        leptonica_versions: github_semver_tags("DanBloomberg", "leptonica")?,
        tessdata_commit: github_branch_commit(
            "tesseract-ocr",
            &policy.tessdata_repo,
            &policy.tessdata_commit,
        )?,
        policy,
    })
}

fn recommended_tesseract_policy_update(state: &UpstreamTesseractState) -> TesseractPolicyUpdate {
    let mut policy = state.policy.clone();
    let old_versions = policy.versions.clone();
    let current_latest = old_versions
        .iter()
        .filter_map(|version| VersionTag::parse(version))
        .max();
    for version in &state.tesseract_versions {
        if current_latest.is_some_and(|current| version <= &current) {
            continue;
        }
        let version = version.to_string();
        if !policy.versions.contains(&version) {
            policy.versions.push(version);
        }
    }
    policy
        .versions
        .sort_by_key(|version| VersionTag::parse(version));
    policy.versions.dedup();
    if let Some(latest) = state.tesseract_versions.last() {
        policy.latest = latest.to_string();
        policy.default.clone_from(&policy.latest);
    }
    if let Some(latest) = state.leptonica_versions.last() {
        policy.leptonica_default = latest.to_string();
    }
    policy.tessdata_commit.clone_from(&state.tessdata_commit);
    let tesseract_added = policy
        .versions
        .iter()
        .filter(|version| !old_versions.contains(version))
        .cloned()
        .collect();
    TesseractPolicyUpdate {
        leptonica_changed: policy.leptonica_default != state.policy.leptonica_default,
        tessdata_changed: policy.tessdata_commit != state.policy.tessdata_commit,
        policy,
        tesseract_added,
    }
}

fn github_semver_tags(owner: &str, repo: &str) -> Result<Vec<VersionTag>> {
    let text = fetch_url(&format!(
        "https://api.github.com/repos/{owner}/{repo}/git/matching-refs/tags/"
    ))?;
    let mut versions = text
        .split("\"ref\":")
        .skip(1)
        .filter_map(|chunk| chunk.split('"').nth(1))
        .filter_map(|reference| reference.strip_prefix("refs/tags/"))
        .filter_map(VersionTag::parse)
        .collect::<Vec<_>>();
    versions.sort();
    versions.dedup();
    Ok(versions)
}

fn github_branch_commit(owner: &str, repo: &str, branch_or_commit: &str) -> Result<String> {
    if is_git_sha(branch_or_commit) {
        return Ok(branch_or_commit.to_owned());
    }
    let text = fetch_url(&format!(
        "https://api.github.com/repos/{owner}/{repo}/commits/{branch_or_commit}"
    ))?;
    text.split("\"sha\":")
        .nth(1)
        .and_then(|chunk| chunk.split('"').nth(1))
        .map(str::to_owned)
        .ok_or_else(|| format_error("failed to parse GitHub commit sha"))
}

fn fetch_url(url: &str) -> Result<String> {
    let client = reqwest::blocking::Client::builder()
        .user_agent("bcode-xtask/0.0.1")
        .build()
        .map_err(|error| format_error(format!("failed to build HTTP client: {error}")))?;
    client
        .get(url)
        .send()
        .and_then(reqwest::blocking::Response::error_for_status)
        .and_then(reqwest::blocking::Response::text)
        .map_err(|error| format_error(format!("failed to fetch {url}: {error}")))
}

fn is_git_sha(value: &str) -> bool {
    matches!(value.len(), 40 | 64) && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn render_tesseract_sync_policy(policy: &TesseractSyncPolicy) -> String {
    format!(
        "[tesseract]\ndefault  = \"{}\"\nlatest   = \"{}\"\nversions = [{}]\n\n[leptonica]\ndefault_version = \"{}\"\n\n[tessdata]\ncommit    = \"{}\"\nflavor    = \"{}\"\nlanguages = [{}]\nrepo      = \"{}\"\n",
        policy.default,
        policy.latest,
        policy
            .versions
            .iter()
            .map(|version| format!("\"{version}\""))
            .collect::<Vec<_>>()
            .join(", "),
        policy.leptonica_default,
        policy.tessdata_commit,
        policy.tessdata_flavor,
        policy
            .tessdata_languages
            .iter()
            .map(|language| format!("\"{language}\""))
            .collect::<Vec<_>>()
            .join(", "),
        policy.tessdata_repo
    )
}

fn update_tesseract_catalog(options: &Options) -> Result<()> {
    let root = workspace_root();
    let policy_path = root.join("packages/tesseract-sys/bundled/sync-policy.toml");
    let catalog_path = root.join("packages/tesseract-sys/bundled/catalog.generated.toml");
    let policy = read_tesseract_sync_policy(&policy_path)?;
    validate_tesseract_sync_policy(&policy)?;

    let resolved_tesseract = policy
        .versions
        .iter()
        .map(|version| {
            let url = tesseract_source_url(version);
            let sha256 = sha256_url(&url)?;
            Ok(ResolvedTesseractVersion {
                version: version.clone(),
                url,
                sha256,
                leptonica: policy.leptonica_default.clone(),
            })
        })
        .collect::<Result<Vec<_>>>()?;

    let leptonica_url = leptonica_source_url(&policy.leptonica_default);
    let leptonica = ResolvedArtifact {
        sha256: sha256_url(&leptonica_url)?,
        url: leptonica_url,
    };
    let tessdata = policy
        .tessdata_languages
        .iter()
        .map(|language| {
            let url = tessdata_url(&policy.tessdata_repo, &policy.tessdata_commit, language);
            Ok((
                language.clone(),
                ResolvedArtifact {
                    sha256: sha256_url(&url)?,
                    url,
                },
            ))
        })
        .collect::<Result<BTreeMap<_, _>>>()?;

    let catalog = render_tesseract_catalog(&policy, &resolved_tesseract, &leptonica, &tessdata);
    write_generated_file(&catalog_path, &catalog, options)?;
    sync_tesseract_feature_blocks(&root, &policy, options)?;

    println!(
        "synced bundled Tesseract catalog: {} version(s), {} tessdata language(s)",
        policy.versions.len(),
        policy.tessdata_languages.len()
    );
    Ok(())
}

fn read_tesseract_sync_policy(path: &Path) -> Result<TesseractSyncPolicy> {
    let policy_text = fs::read_to_string(path)
        .map_err(|error| format_error(format!("failed to read {}: {error}", path.display())))?;
    let policy = policy_text
        .parse::<toml_edit::DocumentMut>()
        .map_err(|error| format_error(format!("failed to parse policy TOML: {error}")))?;
    Ok(TesseractSyncPolicy {
        versions: string_array(&policy, &["tesseract", "versions"])
            .ok_or_else(|| format_error("policy must contain tesseract.versions"))?,
        default: string_value(&policy, &["tesseract", "default"])
            .ok_or_else(|| format_error("policy must contain tesseract.default"))?,
        latest: string_value(&policy, &["tesseract", "latest"])
            .ok_or_else(|| format_error("policy must contain tesseract.latest"))?,
        leptonica_default: string_value(&policy, &["leptonica", "default_version"])
            .ok_or_else(|| format_error("policy must contain leptonica.default_version"))?,
        tessdata_flavor: string_value(&policy, &["tessdata", "flavor"])
            .ok_or_else(|| format_error("policy must contain tessdata.flavor"))?,
        tessdata_repo: string_value(&policy, &["tessdata", "repo"])
            .ok_or_else(|| format_error("policy must contain tessdata.repo"))?,
        tessdata_commit: string_value(&policy, &["tessdata", "commit"])
            .ok_or_else(|| format_error("policy must contain tessdata.commit"))?,
        tessdata_languages: string_array(&policy, &["tessdata", "languages"])
            .ok_or_else(|| format_error("policy must contain tessdata.languages"))?,
    })
}

fn validate_tesseract_sync_policy(policy: &TesseractSyncPolicy) -> Result<()> {
    if policy.versions.is_empty() {
        return Err(format_error("policy tesseract.versions cannot be empty"));
    }
    for alias in [&policy.default, &policy.latest] {
        if !policy.versions.contains(alias) {
            return Err(format_error(format!(
                "alias version {alias} is not listed in tesseract.versions"
            )));
        }
    }
    if policy.tessdata_languages.is_empty() {
        return Err(format_error("policy tessdata.languages cannot be empty"));
    }
    Ok(())
}

fn string_value(document: &toml_edit::DocumentMut, path: &[&str]) -> Option<String> {
    let mut item = document.as_item();
    for segment in path {
        item = item.get(segment)?;
    }
    item.as_str().map(ToOwned::to_owned)
}

fn string_array(document: &toml_edit::DocumentMut, path: &[&str]) -> Option<Vec<String>> {
    let mut item = document.as_item();
    for segment in path {
        item = item.get(segment)?;
    }
    item.as_array().map(|array| {
        array
            .iter()
            .filter_map(|value| value.as_str().map(ToOwned::to_owned))
            .collect()
    })
}

fn tesseract_source_url(version: &str) -> String {
    format!("https://github.com/tesseract-ocr/tesseract/archive/refs/tags/{version}.zip")
}

fn leptonica_source_url(version: &str) -> String {
    format!("https://github.com/DanBloomberg/leptonica/archive/refs/tags/{version}.zip")
}

fn tessdata_url(repo: &str, commit: &str, language: &str) -> String {
    format!("https://github.com/tesseract-ocr/{repo}/raw/{commit}/{language}.traineddata")
}

fn sha256_url(url: &str) -> Result<String> {
    let client = reqwest::blocking::Client::builder()
        .user_agent("bcode-xtask/0.0.1")
        .connect_timeout(Duration::from_secs(30))
        .timeout(Duration::from_mins(5))
        .build()
        .map_err(|error| format_error(format!("failed to build HTTP client: {error}")))?;
    let mut last_error = None;
    for attempt in 1..=5 {
        match client
            .get(url)
            .send()
            .and_then(reqwest::blocking::Response::error_for_status)
            .and_then(reqwest::blocking::Response::bytes)
        {
            Ok(bytes) => {
                let digest = Sha256::digest(&bytes);
                return Ok(format!("{digest:x}"));
            }
            Err(error) => {
                last_error = Some(error);
                if attempt < 5 {
                    std::thread::sleep(Duration::from_secs(attempt * 2));
                }
            }
        }
    }
    Err(format_error(format!(
        "failed to hash {url}: {}",
        last_error.map_or_else(|| "unknown error".to_string(), |error| error.to_string())
    )))
}

fn render_tesseract_catalog(
    policy: &TesseractSyncPolicy,
    tesseract: &[ResolvedTesseractVersion],
    leptonica: &ResolvedArtifact,
    tessdata: &BTreeMap<String, ResolvedArtifact>,
) -> String {
    let mut output = String::from(
        "# @generated by cargo xtask update-tesseract-catalog\n# Do not edit manually.\n\n",
    );
    output.push_str("[aliases]\n");
    writeln!(output, "default = \"{}\"", policy.default).expect("write to string cannot fail");
    writeln!(output, "latest  = \"{}\"\n", policy.latest).expect("write to string cannot fail");
    writeln!(output, "[leptonica.\"{}\"]", policy.leptonica_default)
        .expect("write to string cannot fail");
    writeln!(output, "sha256 = \"{}\"", leptonica.sha256).expect("write to string cannot fail");
    writeln!(output, "url    = \"{}\"\n", leptonica.url).expect("write to string cannot fail");
    for entry in tesseract {
        writeln!(output, "[tesseract.\"{}\"]", entry.version).expect("write to string cannot fail");
        writeln!(output, "leptonica = \"{}\"", entry.leptonica)
            .expect("write to string cannot fail");
        writeln!(output, "sha256    = \"{}\"", entry.sha256).expect("write to string cannot fail");
        writeln!(output, "url       = \"{}\"\n", entry.url).expect("write to string cannot fail");
    }
    writeln!(output, "[tessdata.{}]", policy.tessdata_flavor).expect("write to string cannot fail");
    writeln!(output, "commit = \"{}\"\n", policy.tessdata_commit)
        .expect("write to string cannot fail");
    for (language, artifact) in tessdata {
        writeln!(
            output,
            "[tessdata.{}.languages.{language}]",
            policy.tessdata_flavor
        )
        .expect("write to string cannot fail");
        writeln!(output, "sha256 = \"{}\"", artifact.sha256).expect("write to string cannot fail");
        writeln!(output, "url    = \"{}\"\n", artifact.url).expect("write to string cannot fail");
    }
    output
}

fn write_generated_file(path: &Path, contents: &str, options: &Options) -> Result<()> {
    let current = fs::read_to_string(path).ok();
    if current.as_deref() == Some(contents) {
        return Ok(());
    }
    match options.generated_write_mode {
        GeneratedWriteMode::Check => {
            return Err(format_error(format!("{} is stale", path.display())));
        }
        GeneratedWriteMode::DryRun => {
            println!("would update {}", path.display());
            return Ok(());
        }
        GeneratedWriteMode::Write => {}
    }
    fs::write(path, contents)
        .map_err(|error| format_error(format!("failed to write {}: {error}", path.display())))
}

fn sync_tesseract_feature_blocks(
    root: &Path,
    policy: &TesseractSyncPolicy,
    options: &Options,
) -> Result<()> {
    sync_tesseract_sys_features(
        &root.join("packages/tesseract-sys/Cargo.toml"),
        policy,
        options,
    )?;
    sync_tesseract_ocr_features(
        &root.join("packages/tesseract-ocr/Cargo.toml"),
        policy,
        options,
    )?;
    sync_ocr_plugin_features(&root.join("plugins/ocr-plugin/Cargo.toml"), policy, options)?;
    sync_bcode_features(&root.join("packages/bcode/Cargo.toml"), policy, options)
}

fn feature_name(prefix: &str, version: &str) -> String {
    format!("{prefix}-v{}", version.replace('.', "-"))
}

fn array_item(value: &str) -> toml_edit::Value {
    toml_edit::Value::from(value)
}

fn set_feature(features: &mut toml_edit::Table, name: &str, deps: &[String]) {
    let mut array = toml_edit::Array::new();
    for dep in deps {
        array.push(array_item(dep));
    }
    features[name] = toml_edit::value(array);
}

fn load_cargo_toml(path: &Path) -> Result<toml_edit::DocumentMut> {
    let text = fs::read_to_string(path)
        .map_err(|error| format_error(format!("failed to read {}: {error}", path.display())))?;
    text.parse::<toml_edit::DocumentMut>()
        .map_err(|error| format_error(format!("failed to parse {}: {error}", path.display())))
}

fn features_table(document: &mut toml_edit::DocumentMut) -> Result<&mut toml_edit::Table> {
    document
        .get_mut("features")
        .and_then(toml_edit::Item::as_table_mut)
        .ok_or_else(|| format_error("Cargo.toml must contain a [features] table"))
}

fn write_cargo_toml(
    path: &Path,
    document: &toml_edit::DocumentMut,
    options: &Options,
) -> Result<()> {
    write_generated_file(path, &document.to_string(), options)
}

fn sync_tesseract_sys_features(
    path: &Path,
    policy: &TesseractSyncPolicy,
    options: &Options,
) -> Result<()> {
    let mut document = load_cargo_toml(path)?;
    let features = features_table(&mut document)?;
    set_feature(
        features,
        "bundled-tesseract",
        &["bundled-tesseract-default".to_owned()],
    );
    set_feature(
        features,
        "bundled-tesseract-default",
        &[feature_name("bundled-tesseract", &policy.default)],
    );
    set_feature(
        features,
        "bundled-tesseract-latest",
        &[feature_name("bundled-tesseract", &policy.latest)],
    );
    set_feature(
        features,
        "bundled-tesseract-all",
        &policy
            .versions
            .iter()
            .map(|version| feature_name("bundled-tesseract", version))
            .collect::<Vec<_>>(),
    );
    set_feature(
        features,
        "bundled-tesseract-build",
        &[
            "dep:reqwest".to_owned(),
            "dep:sha2".to_owned(),
            "dep:zip".to_owned(),
        ],
    );
    for version in &policy.versions {
        set_feature(
            features,
            &feature_name("bundled-tesseract", version),
            &["bundled-tesseract-build".to_owned()],
        );
    }
    write_cargo_toml(path, &document, options)
}

fn sync_tesseract_ocr_features(
    path: &Path,
    policy: &TesseractSyncPolicy,
    options: &Options,
) -> Result<()> {
    let mut document = load_cargo_toml(path)?;
    let features = features_table(&mut document)?;
    set_feature(
        features,
        "bundled-tesseract",
        &["bundled-tesseract-default".to_owned()],
    );
    set_feature(
        features,
        "bundled-tesseract-default",
        &[feature_name("bundled-tesseract", &policy.default)],
    );
    set_feature(
        features,
        "bundled-tesseract-latest",
        &[feature_name("bundled-tesseract", &policy.latest)],
    );
    set_feature(
        features,
        "bundled-tesseract-all",
        &policy
            .versions
            .iter()
            .map(|version| feature_name("bundled-tesseract", version))
            .collect::<Vec<_>>(),
    );
    for version in &policy.versions {
        let feature = feature_name("bundled-tesseract", version);
        set_feature(
            features,
            &feature,
            &[format!("bcode_tesseract_sys/{feature}")],
        );
    }
    write_cargo_toml(path, &document, options)
}

fn sync_ocr_plugin_features(
    path: &Path,
    policy: &TesseractSyncPolicy,
    options: &Options,
) -> Result<()> {
    let mut document = load_cargo_toml(path)?;
    let features = features_table(&mut document)?;
    set_feature(
        features,
        "bundled-tesseract",
        &["bundled-tesseract-default".to_owned()],
    );
    set_feature(
        features,
        "bundled-tesseract-default",
        &[feature_name("bundled-tesseract", &policy.default)],
    );
    set_feature(
        features,
        "bundled-tesseract-latest",
        &[feature_name("bundled-tesseract", &policy.latest)],
    );
    set_feature(
        features,
        "bundled-tesseract-all",
        &policy
            .versions
            .iter()
            .map(|version| feature_name("bundled-tesseract", version))
            .collect::<Vec<_>>(),
    );
    for version in &policy.versions {
        let feature = feature_name("bundled-tesseract", version);
        set_feature(
            features,
            &feature,
            &[
                "_bundled-tesseract-runtime".to_owned(),
                format!("bcode_tesseract_ocr/{feature}"),
            ],
        );
    }
    write_cargo_toml(path, &document, options)
}

fn sync_bcode_features(path: &Path, policy: &TesseractSyncPolicy, options: &Options) -> Result<()> {
    let mut document = load_cargo_toml(path)?;
    let features = features_table(&mut document)?;
    set_feature(
        features,
        "bundled-ocr-tesseract",
        &["bundled-ocr-tesseract-default".to_owned()],
    );
    set_feature(
        features,
        "bundled-ocr-tesseract-default",
        &[feature_name("bundled-ocr-tesseract", &policy.default)],
    );
    set_feature(
        features,
        "bundled-ocr-tesseract-latest",
        &[feature_name("bundled-ocr-tesseract", &policy.latest)],
    );
    set_feature(
        features,
        "bundled-ocr-tesseract-all",
        &policy
            .versions
            .iter()
            .map(|version| feature_name("bundled-ocr-tesseract", version))
            .collect::<Vec<_>>(),
    );
    for version in &policy.versions {
        let app_feature = feature_name("bundled-ocr-tesseract", version);
        let plugin_feature = feature_name("bundled-tesseract", version);
        set_feature(
            features,
            &app_feature,
            &[
                "static-bundled-ocr-plugin".to_owned(),
                format!("bcode_ocr_plugin/{plugin_feature}"),
            ],
        );
    }
    write_cargo_toml(path, &document, options)
}

fn package_tesseract_runtimes(options: &Options) -> Result<()> {
    let target_kind = TargetKind::parse(&options.target)?;
    let source = latest_bundled_runtime_root(&options.target)?;
    let binary = options
        .dev_binary
        .clone()
        .unwrap_or_else(|| built_binary(&options.target, target_kind));
    let binary_dir = binary.parent().ok_or_else(|| {
        format_error(format!(
            "failed to determine binary directory for {}",
            binary.display()
        ))
    })?;
    let destination = if options.out_dir == Path::new(DIST_DIR) {
        binary_dir.join("bcode-runtimes").join("tesseract")
    } else {
        options.out_dir.clone()
    };
    recreate_dir(&destination)?;
    copy_dir_recursive(&source, &destination)?;
    write_runtime_manifest(&destination)?;
    println!(
        "packaged bundled Tesseract runtimes: {} -> {}",
        source.display(),
        destination.display()
    );
    Ok(())
}

fn smoke_test_tesseract(options: &Options) -> Result<()> {
    let target_kind = TargetKind::parse(&options.target)?;
    let binary = options
        .dev_binary
        .clone()
        .unwrap_or_else(|| built_binary(&options.target, target_kind));
    let runtime_root = if options.out_dir == Path::new(DIST_DIR) {
        binary
            .parent()
            .ok_or_else(|| format_error("failed to determine binary directory"))?
            .join("bcode-runtimes")
            .join("tesseract")
    } else {
        options.out_dir.clone()
    };
    ensure_file(&binary)?;
    ensure_dir(&runtime_root)?;
    run_command(
        Command::new(&binary)
            .arg("--version")
            .env("BCODE_TESSERACT_RUNTIME_ROOT", &runtime_root),
    )?;
    run_command(
        Command::new("cargo")
            .arg("run")
            .arg("--package")
            .arg("bcode_tesseract_ocr")
            .arg("--bin")
            .arg("tesseract-smoke")
            .arg("--no-default-features")
            .arg("--features")
            .arg("bundled-tesseract-default")
            .env("BCODE_TESSERACT_RUNTIME_ROOT", &runtime_root),
    )?;
    println!(
        "smoke-tested bcode binary with bundled Tesseract runtime root {}",
        runtime_root.display()
    );
    Ok(())
}

fn latest_bundled_runtime_root(target: &str) -> Result<PathBuf> {
    let build_dir = workspace_root()
        .join("target")
        .join(target)
        .join("debug")
        .join("build");
    let release_build_dir = workspace_root()
        .join("target")
        .join(target)
        .join("release")
        .join("build");
    let fallback_build_dir = workspace_root().join("target").join("debug").join("build");
    [release_build_dir, build_dir, fallback_build_dir]
        .into_iter()
        .filter(|dir| dir.is_dir())
        .flat_map(|dir| bundled_runtime_roots(&dir).unwrap_or_default())
        .max_by_key(|path| {
            path.metadata()
                .and_then(|metadata| metadata.modified())
                .ok()
        })
        .ok_or_else(|| format_error("failed to find built bundled Tesseract runtimes"))
}

fn bundled_runtime_roots(build_dir: &Path) -> Result<Vec<PathBuf>> {
    let mut roots = Vec::new();
    for entry in fs::read_dir(build_dir)? {
        let path = entry?.path().join("out").join("bundled-runtimes");
        if path.is_dir() {
            roots.push(path);
        }
    }
    Ok(roots)
}

fn copy_dir_recursive(source: &Path, destination: &Path) -> Result<()> {
    ensure_dir(source)?;
    fs::create_dir_all(destination)?;
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let source_path = entry.path();
        let destination_path = destination.join(entry.file_name());
        if source_path.is_dir() {
            copy_dir_recursive(&source_path, &destination_path)?;
        } else {
            fs::copy(&source_path, &destination_path).map_err(|error| {
                format_error(format!(
                    "failed to copy {} to {}: {error}",
                    source_path.display(),
                    destination_path.display()
                ))
            })?;
        }
    }
    Ok(())
}

fn write_runtime_manifest(runtime_root: &Path) -> Result<()> {
    let catalog = load_cargo_toml(
        &workspace_root().join("packages/tesseract-sys/bundled/catalog.generated.toml"),
    )?;
    let default = string_value(&catalog, &["aliases", "default"])
        .ok_or_else(|| format_error("catalog is missing aliases.default"))?;
    let latest = string_value(&catalog, &["aliases", "latest"])
        .ok_or_else(|| format_error("catalog is missing aliases.latest"))?;
    let mut versions = fs::read_dir(runtime_root)?
        .filter_map(std::result::Result::ok)
        .filter(|entry| entry.path().is_dir())
        .filter_map(|entry| entry.file_name().into_string().ok())
        .collect::<Vec<_>>();
    versions.sort();
    if versions.is_empty() {
        return Err(format_error(format!(
            "bundled Tesseract runtime root {} contains no version directories",
            runtime_root.display()
        )));
    }
    let mut languages = Vec::new();
    if let Some(first_version) = versions.first() {
        let tessdata = runtime_root.join(first_version).join("tessdata");
        if tessdata.is_dir() {
            languages = fs::read_dir(tessdata)?
                .filter_map(std::result::Result::ok)
                .filter_map(|entry| entry.file_name().into_string().ok())
                .filter_map(|name| name.strip_suffix(".traineddata").map(str::to_string))
                .collect();
            languages.sort();
        }
    }
    let manifest = format!(
        "{{\n  \"default\": \"{}\",\n  \"latest\": \"{}\",\n  \"versions\": [{}],\n  \"languages\": [{}]\n}}\n",
        json_escape(&default),
        json_escape(&latest),
        json_array(&versions),
        json_array(&languages)
    );
    fs::write(runtime_root.join("manifest.json"), manifest)?;
    Ok(())
}

fn json_array(values: &[String]) -> String {
    values
        .iter()
        .map(|value| format!("\"{}\"", json_escape(value)))
        .collect::<Vec<_>>()
        .join(", ")
}

fn json_escape(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("xtask lives under workspace root")
        .to_path_buf()
}

fn parse_features(value: &str) -> Result<Vec<String>> {
    let features = value
        .split(',')
        .map(str::trim)
        .filter(|feature| !feature.is_empty())
        .map(str::to_owned)
        .collect::<Vec<_>>();
    if features.is_empty() {
        Err(format_error("--features requires at least one feature"))
    } else {
        Ok(features)
    }
}

fn selected_features(options: &Options, default_distribution: bool) -> Vec<String> {
    let mut features = options.features.clone().unwrap_or_else(|| {
        vec![if default_distribution {
            "distribution".to_owned()
        } else {
            "app".to_owned()
        }]
    });
    features.push("app".to_owned());
    features
        .into_iter()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

fn features_enable_mermaid_worker(features: &[String]) -> bool {
    features
        .iter()
        .any(|feature| matches!(feature.as_str(), "distribution" | "mermaid-renderer"))
}

fn features_enable_bundled_tesseract(features: &[String]) -> bool {
    features.iter().any(|feature| {
        feature == "distribution"
            || feature == "bundled-ocr-tesseract"
            || feature.starts_with("bundled-ocr-tesseract-")
    })
}

fn build_bcode_release(target: &str, features: &[String]) -> Result<()> {
    let mut command = Command::new("cargo");
    command
        .arg("build")
        .arg("--release")
        .arg("--package")
        .arg(PACKAGE_NAME)
        .arg("--bin")
        .arg(BINARY_NAME);
    if features_enable_mermaid_worker(features) {
        command.arg("--bin").arg(MERMAID_WORKER_BINARY_NAME);
    }
    command
        .arg("--no-default-features")
        .arg("--features")
        .arg(features.join(","))
        .arg("--target")
        .arg(target);
    run_command(&mut command)
}

fn build(options: &Options) -> Result<()> {
    let features = selected_features(options, false);
    build_bcode_release(&options.target, &features)
}

fn release(options: &Options) -> Result<()> {
    let features = selected_features(options, true);
    let include_mermaid_worker = features_enable_mermaid_worker(&features);
    let include_tesseract = features_enable_bundled_tesseract(&features);
    let target_kind = TargetKind::parse(&options.target)?;
    build_bcode_release(&options.target, &features)?;

    let binary = built_binary(&options.target, target_kind);
    ensure_file(&binary)?;
    let mermaid_worker =
        include_mermaid_worker.then(|| built_mermaid_worker(&options.target, target_kind));
    if let Some(worker) = &mermaid_worker {
        ensure_file(worker)?;
    }
    if target_kind == TargetKind::Macos {
        sign_macos_release_binary(&binary)?;
        verify_macos_signature(&binary)?;
        if let Some(worker) = &mermaid_worker {
            sign_macos_release_binary(worker)?;
            verify_macos_signature(worker)?;
        }
    } else if target_kind == TargetKind::Linux {
        strip_binary(&binary);
        if let Some(worker) = &mermaid_worker {
            strip_binary(worker);
        }
    } else if target_kind == TargetKind::Windows {
        sign_windows_release_binary_if_configured(&binary)?;
        if let Some(worker) = &mermaid_worker {
            sign_windows_release_binary_if_configured(worker)?;
        }
    }

    let staging_dir = staging_dir(options);
    recreate_dir(&staging_dir)?;
    let staged_binary = staging_dir.join(binary_file_name(target_kind));
    copy_release_binary(&binary, &staged_binary)?;
    if let Some(worker) = &mermaid_worker {
        let staged_worker = staging_dir.join(mermaid_worker_file_name(target_kind));
        copy_release_binary(worker, &staged_worker)?;
    }
    if include_tesseract {
        let runtime_source = latest_bundled_runtime_root(&options.target)?;
        let runtime_destination = staging_dir.join("bcode-runtimes").join("tesseract");
        recreate_dir(&runtime_destination)?;
        copy_dir_recursive(&runtime_source, &runtime_destination)?;
        write_runtime_manifest(&runtime_destination)?;
    }

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
    let features = selected_features(options, true);
    let include_mermaid_worker = features_enable_mermaid_worker(&features);
    let include_tesseract = features_enable_bundled_tesseract(&features);
    let target_kind = TargetKind::parse(&options.target)?;
    let archive = archive_path(options, target_kind);
    ensure_file(&archive)?;
    ensure_file(&checksum_path(&archive))?;
    verify_checksum(&archive)?;
    verify_archive_contents(
        &archive,
        target_kind,
        include_mermaid_worker,
        include_tesseract,
    )?;

    if target_kind == TargetKind::Macos {
        let binary = built_binary(&options.target, target_kind);
        ensure_file(&binary)?;
        verify_macos_signature(&binary)?;
        if include_mermaid_worker {
            let mermaid_worker = built_mermaid_worker(&options.target, target_kind);
            ensure_file(&mermaid_worker)?;
            verify_macos_signature(&mermaid_worker)?;
        }
    } else if target_kind == TargetKind::Windows && windows_signing_requested() {
        verify_windows_signature(&built_binary(&options.target, target_kind))?;
        if include_mermaid_worker {
            verify_windows_signature(&built_mermaid_worker(&options.target, target_kind))?;
        }
    }
    if options.target == host_target() {
        smoke_test_release_archive(
            &archive,
            target_kind,
            include_mermaid_worker,
            include_tesseract,
        )?;
    } else {
        println!(
            "skipping extracted artifact execution for non-host target {} on {}",
            options.target,
            host_target()
        );
    }

    println!("verified release artifact: {}", archive.display());
    Ok(())
}

fn dev_release(options: &Options) -> Result<()> {
    let features = selected_features(options, true);
    let target_kind = TargetKind::parse(&options.target)?;
    build_bcode_release(&options.target, &features)?;
    let binary = built_binary(&options.target, target_kind);
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
        TargetKind::Windows => {
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
        .unwrap_or_else(|| built_binary(&options.target, target_kind));
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
    Windows,
}

impl TargetKind {
    fn parse(target: &str) -> Result<Self> {
        match target {
            "aarch64-apple-darwin" | "x86_64-apple-darwin" => Ok(Self::Macos),
            "aarch64-unknown-linux-gnu" | "x86_64-unknown-linux-gnu" => Ok(Self::Linux),
            "x86_64-pc-windows-msvc" => Ok(Self::Windows),
            "x86_64-windows" => Err(format_error(
                "unsupported release target `x86_64-windows`; use `x86_64-pc-windows-msvc`",
            )),
            _ => Err(format_error(format!(
                "unsupported release target `{target}`; supported targets are listed by `cargo xtask help`"
            ))),
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

fn windows_signing_requested() -> bool {
    windows_signing_requested_from(
        env::var_os("WINDOWS_CODESIGN_CERTIFICATE_PFX_PATH").is_some_and(|value| !value.is_empty()),
    )
}

const fn windows_signing_requested_from(certificate_path: bool) -> bool {
    certificate_path
}

fn sign_windows_release_binary_if_configured(binary: &Path) -> Result<()> {
    let Some(certificate) = env::var_os("WINDOWS_CODESIGN_CERTIFICATE_PFX_PATH") else {
        println!(
            "WINDOWS_CODESIGN_CERTIFICATE_PFX_PATH not set; leaving {} unsigned",
            binary.display()
        );
        return Ok(());
    };
    let password = env::var("WINDOWS_CODESIGN_CERTIFICATE_PASSWORD")
        .ok()
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            format_error(
                "WINDOWS_CODESIGN_CERTIFICATE_PASSWORD is required when Windows signing is configured",
            )
        })?;
    run_sensitive_command(
        Command::new("signtool")
            .arg("sign")
            .arg("/fd")
            .arg("SHA256")
            .arg("/td")
            .arg("SHA256")
            .arg("/tr")
            .arg(WINDOWS_TIMESTAMP_URL)
            .arg("/f")
            .arg(certificate)
            .arg("/p")
            .arg(password)
            .arg(binary),
        "signtool sign /fd SHA256 /td SHA256 /tr <timestamp-url> /f <certificate> /p <redacted> <binary>",
    )?;
    verify_windows_signature(binary)
}

fn verify_windows_signature(binary: &Path) -> Result<()> {
    ensure_file(binary)?;
    run_command(
        Command::new("signtool")
            .arg("verify")
            .arg("/pa")
            .arg("/all")
            .arg("/v")
            .arg(binary),
    )
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

    if archive
        .extension()
        .is_some_and(|extension| extension == "zip")
    {
        create_zip_archive(archive, staging_dir)
    } else {
        let file_name = archive
            .file_name()
            .ok_or_else(|| format_error("archive path has no file name"))?;
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

fn create_zip_archive(archive: &Path, staging_dir: &Path) -> Result<()> {
    let mut paths = archive_source_files(staging_dir)?;
    paths.sort();
    let file = File::create(archive)?;
    let mut writer = zip::ZipWriter::new(file);
    let options = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated);
    for path in paths {
        let relative = path.strip_prefix(staging_dir).map_err(|error| {
            format_error(format!(
                "failed to make {} relative to {}: {error}",
                path.display(),
                staging_dir.display()
            ))
        })?;
        let name = zip_entry_name(relative)?;
        writer.start_file(name, options)?;
        let mut source = File::open(&path)?;
        io::copy(&mut source, &mut writer)?;
    }
    writer.finish()?;
    Ok(())
}

fn archive_source_files(root: &Path) -> Result<Vec<PathBuf>> {
    let canonical_root = root.canonicalize().map_err(|error| {
        format_error(format!(
            "failed to canonicalize release staging root {}: {error}",
            root.display()
        ))
    })?;
    let mut files = Vec::new();
    collect_archive_source_files(root, &canonical_root, &mut files)?;
    Ok(files)
}

fn collect_archive_source_files(
    directory: &Path,
    canonical_root: &Path,
    files: &mut Vec<PathBuf>,
) -> Result<()> {
    for entry in fs::read_dir(directory)? {
        let path = entry?.path();
        let metadata = fs::symlink_metadata(&path)?;
        if metadata.file_type().is_symlink() {
            return Err(format_error(format!(
                "release staging contains symbolic link {}",
                path.display()
            )));
        }
        let canonical = path.canonicalize().map_err(|error| {
            format_error(format!(
                "failed to canonicalize release staging entry {}: {error}",
                path.display()
            ))
        })?;
        if !canonical.starts_with(canonical_root) {
            return Err(format_error(format!(
                "release staging entry escapes its authorized root: {}",
                path.display()
            )));
        }
        if metadata.is_dir() {
            collect_archive_source_files(&path, canonical_root, files)?;
        } else if metadata.is_file() {
            files.push(path);
        } else {
            return Err(format_error(format!(
                "release staging contains unsupported entry {}",
                path.display()
            )));
        }
    }
    Ok(())
}

fn zip_entry_name(path: &Path) -> Result<String> {
    let components = path
        .components()
        .map(|component| match component {
            std::path::Component::Normal(value) => {
                value.to_str().map(str::to_owned).ok_or_else(|| {
                    format_error(format!("release path is not UTF-8: {}", path.display()))
                })
            }
            _ => Err(format_error(format!(
                "release path is not a safe relative path: {}",
                path.display()
            ))),
        })
        .collect::<Result<Vec<_>>>()?;
    if components.is_empty() {
        return Err(format_error("release archive entry path is empty"));
    }
    Ok(components.join("/"))
}

fn archive_sha256(archive: &Path) -> Result<String> {
    let mut file = File::open(archive)?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0_u8; 64 * 1024];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

fn write_checksum(archive: &Path) -> Result<()> {
    let file_name = archive
        .file_name()
        .and_then(OsStr::to_str)
        .ok_or_else(|| format_error("archive path has no UTF-8 file name"))?;
    let checksum = checksum_path(archive);
    fs::write(
        checksum,
        format!("{}  {file_name}\n", archive_sha256(archive)?),
    )?;
    Ok(())
}

fn verify_checksum(archive: &Path) -> Result<()> {
    let checksum_path = checksum_path(archive);
    let checksum_text = fs::read_to_string(&checksum_path)?;
    let expected = checksum_text
        .split_whitespace()
        .next()
        .ok_or_else(|| format_error(format!("empty checksum file {}", checksum_path.display())))?;
    let actual = archive_sha256(archive)?;
    if expected.eq_ignore_ascii_case(&actual) {
        Ok(())
    } else {
        Err(format_error(format!(
            "checksum mismatch for {}: expected {expected}, got {actual}",
            archive.display()
        )))
    }
}

fn verify_archive_contents(
    archive: &Path,
    target_kind: TargetKind,
    include_mermaid_worker: bool,
    include_tesseract: bool,
) -> Result<()> {
    if target_kind == TargetKind::Linux {
        return verify_tar_archive_contents(
            archive,
            target_kind,
            include_mermaid_worker,
            include_tesseract,
        );
    }
    let mut zip = zip::ZipArchive::new(File::open(archive)?)?;
    let mut entries = std::collections::BTreeSet::new();
    for index in 0..zip.len() {
        let entry = zip.by_index(index)?;
        if !entry.is_dir() {
            entries.insert(entry.name().to_owned());
        }
    }
    verify_required_archive_entries(
        &entries,
        archive,
        target_kind,
        include_mermaid_worker,
        include_tesseract,
    )
}

fn verify_tar_archive_contents(
    archive: &Path,
    target_kind: TargetKind,
    include_mermaid_worker: bool,
    include_tesseract: bool,
) -> Result<()> {
    let listing = command_output(Command::new("tar").arg("-tzf").arg(archive))?;
    let entries = listing
        .lines()
        .map(|entry| entry.trim_start_matches("./").to_owned())
        .collect::<std::collections::BTreeSet<_>>();
    verify_required_archive_entries(
        &entries,
        archive,
        target_kind,
        include_mermaid_worker,
        include_tesseract,
    )
}

fn smoke_test_release_archive(
    archive: &Path,
    target_kind: TargetKind,
    include_mermaid_worker: bool,
    include_tesseract: bool,
) -> Result<()> {
    let extraction = archive.with_extension(format!(
        "{}.verify",
        archive
            .extension()
            .and_then(OsStr::to_str)
            .unwrap_or("archive")
    ));
    recreate_dir(&extraction)?;
    let result = smoke_test_release_archive_in(
        archive,
        target_kind,
        &extraction,
        include_mermaid_worker,
        include_tesseract,
    );
    let cleanup = fs::remove_dir_all(&extraction);
    match (result, cleanup) {
        (Err(error), _) => Err(error),
        (Ok(()), Err(error)) => Err(error.into()),
        (Ok(()), Ok(())) => Ok(()),
    }
}

fn smoke_test_release_archive_in(
    archive: &Path,
    target_kind: TargetKind,
    extraction: &Path,
    include_mermaid_worker: bool,
    include_tesseract: bool,
) -> Result<()> {
    if target_kind == TargetKind::Linux {
        run_command(
            Command::new("tar")
                .arg("-xzf")
                .arg(archive)
                .arg("-C")
                .arg(extraction),
        )?;
    } else {
        let mut zip = zip::ZipArchive::new(File::open(archive)?)?;
        extract_zip_confined(&mut zip, extraction)?;
    }
    let binary = extraction.join(binary_file_name(target_kind));
    ensure_file(&binary)?;
    run_command(Command::new(&binary).arg("--version"))?;
    if include_mermaid_worker {
        smoke_test_mermaid_worker(&extraction.join(mermaid_worker_file_name(target_kind)))?;
    }
    if include_tesseract {
        smoke_test_extracted_tesseract(extraction)?;
    }
    if target_kind == TargetKind::Windows {
        smoke_test_windows_daemon(&binary, extraction)?;
        if windows_signing_requested() {
            verify_windows_signature(&binary)?;
            if include_mermaid_worker {
                verify_windows_signature(&extraction.join(mermaid_worker_file_name(target_kind)))?;
            }
        }
    }
    Ok(())
}

fn smoke_test_extracted_tesseract(extraction: &Path) -> Result<()> {
    let runtime_root = extraction.join("bcode-runtimes").join("tesseract");
    ensure_dir(&runtime_root)?;
    run_command(
        Command::new("cargo")
            .arg("run")
            .arg("--package")
            .arg("bcode_tesseract_ocr")
            .arg("--bin")
            .arg("tesseract-smoke")
            .arg("--no-default-features")
            .arg("--features")
            .arg("bundled-tesseract-default")
            .env("BCODE_TESSERACT_RUNTIME_ROOT", runtime_root),
    )
}

fn smoke_test_windows_shell_contract() -> Result<()> {
    let output = Command::new("cmd")
        .args(["/C", "echo bcode-windows-shell-smoke"])
        .output()?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    if !output.status.success() || !stdout.contains("bcode-windows-shell-smoke") {
        return Err(format_error(format!(
            "Windows shell smoke test failed with {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        )));
    }
    smoke_test_windows_child_termination()
}

fn smoke_test_windows_child_termination() -> Result<()> {
    let mut child = Command::new("cmd")
        .args(["/C", "ping -n 30 127.0.0.1 >NUL"])
        .spawn()?;
    std::thread::sleep(Duration::from_millis(100));
    child.kill()?;
    let status = child.wait()?;
    if status.success() {
        Err(format_error(
            "Windows shell cancellation smoke process unexpectedly succeeded",
        ))
    } else {
        Ok(())
    }
}

fn smoke_test_windows_daemon(binary: &Path, extraction: &Path) -> Result<()> {
    let smoke_root = extraction.join("windows smoke 状态");
    fs::create_dir_all(&smoke_root)?;
    let config = smoke_root.join("config.toml");
    fs::write(&config, "")?;
    smoke_test_windows_shell_contract()?;
    let isolated_environment = |command: &mut Command| {
        command
            .env("BCODE_CONFIG", &config)
            .env("APPDATA", smoke_root.join("appdata"))
            .env("LOCALAPPDATA", smoke_root.join("local-appdata"))
            .env("BCODE_DAEMON_LOG", smoke_root.join("daemon.log"));
    };
    let mut start = Command::new(binary);
    start.args(["server", "start"]);
    isolated_environment(&mut start);
    run_command(&mut start)?;

    let result = smoke_test_running_windows_daemon(binary, &isolated_environment);
    let mut stop = Command::new(binary);
    stop.args(["server", "stop"]);
    isolated_environment(&mut stop);
    let stop_result = run_command(&mut stop);
    match (result, stop_result) {
        (Err(error), _) => {
            let mut force_stop = Command::new(binary);
            force_stop.args(["server", "stop", "--force"]);
            isolated_environment(&mut force_stop);
            let _ = force_stop.status();
            Err(error)
        }
        (Ok(()), Err(error)) => Err(error),
        (Ok(()), Ok(())) => Ok(()),
    }
}

fn smoke_test_running_windows_daemon(
    binary: &Path,
    isolated_environment: &impl Fn(&mut Command),
) -> Result<()> {
    let mut status = Command::new(binary);
    status.args(["server", "status", "--verbose"]);
    isolated_environment(&mut status);
    run_command(&mut status)
}

fn extract_zip_confined<R: io::Read + io::Seek>(
    archive: &mut zip::ZipArchive<R>,
    destination: &Path,
) -> Result<()> {
    for index in 0..archive.len() {
        let mut entry = archive.by_index(index)?;
        let enclosed = entry
            .enclosed_name()
            .ok_or_else(|| format_error(format!("unsafe ZIP entry path `{}`", entry.name())))?;
        let output = destination.join(enclosed);
        if entry.is_dir() {
            fs::create_dir_all(&output)?;
            continue;
        }
        if let Some(parent) = output.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut file = File::create(output)?;
        io::copy(&mut entry, &mut file)?;
    }
    Ok(())
}

fn smoke_test_mermaid_worker(worker: &Path) -> Result<()> {
    ensure_file(worker)?;
    let source = b"graph TD; A-->B";
    let mut request = Vec::new();
    request.extend_from_slice(b"BCMW");
    request.extend_from_slice(&1_u16.to_be_bytes());
    request.extend_from_slice(&800_u32.to_be_bytes());
    request.extend_from_slice(&600_u32.to_be_bytes());
    request.extend_from_slice(&(4_u32 * 1024 * 1024).to_be_bytes());
    request.extend_from_slice(
        &u32::try_from(source.len())
            .map_err(|_| format_error("Mermaid smoke request is too large"))?
            .to_be_bytes(),
    );
    request.extend_from_slice(source);
    let mut child = Command::new(worker)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()?;
    child
        .stdin
        .take()
        .ok_or_else(|| format_error("Mermaid worker stdin was unavailable"))?
        .write_all(&request)?;
    let output = child.wait_with_output()?;
    if !output.status.success()
        || output.stdout.len() < 11
        || &output.stdout[..4] != b"BCMR"
        || output.stdout[6] != 1
    {
        return Err(format_error(format!(
            "packaged Mermaid worker smoke test failed with {} and {} response bytes",
            output.status,
            output.stdout.len()
        )));
    }
    Ok(())
}

fn verify_required_archive_entries(
    entries: &BTreeSet<String>,
    archive: &Path,
    target_kind: TargetKind,
    include_mermaid_worker: bool,
    include_tesseract: bool,
) -> Result<()> {
    let mut required = vec![binary_file_name(target_kind)];
    if include_mermaid_worker {
        required.push(mermaid_worker_file_name(target_kind));
    }
    if include_tesseract {
        required.push("bcode-runtimes/tesseract/manifest.json");
    }
    for name in required {
        if !entries.contains(name) {
            return Err(format_error(format!(
                "release archive {} is missing required entry `{name}`",
                archive.display()
            )));
        }
    }
    if !include_tesseract {
        return Ok(());
    }
    let runtime_prefix = "bcode-runtimes/tesseract/";
    if !entries.iter().any(|name| {
        name.starts_with(runtime_prefix)
            && (name.ends_with("/lib/tesseract.dll")
                || name.ends_with("/lib/libtesseract.dylib")
                || name.ends_with("/lib/libtesseract.so"))
    }) {
        return Err(format_error(format!(
            "release archive {} contains no canonical bundled Tesseract runtime library",
            archive.display()
        )));
    }
    if target_kind == TargetKind::Windows
        && !entries
            .iter()
            .any(|name| name.starts_with(runtime_prefix) && name.ends_with("/lib/leptonica.dll"))
    {
        return Err(format_error(format!(
            "release archive {} contains no canonical bundled Leptonica runtime library",
            archive.display()
        )));
    }
    if !entries
        .iter()
        .any(|name| name.starts_with(runtime_prefix) && name.ends_with("/tessdata/eng.traineddata"))
    {
        return Err(format_error(format!(
            "release archive {} contains no bundled English Tesseract language data",
            archive.display()
        )));
    }
    Ok(())
}

fn built_binary(target: &str, target_kind: TargetKind) -> PathBuf {
    built_release_binary(target, binary_file_name(target_kind))
}

fn built_mermaid_worker(target: &str, target_kind: TargetKind) -> PathBuf {
    built_release_binary(target, mermaid_worker_file_name(target_kind))
}

fn built_release_binary(target: &str, file_name: &str) -> PathBuf {
    PathBuf::from("target")
        .join(target)
        .join("release")
        .join(file_name)
}

fn copy_release_binary(source: &Path, destination: &Path) -> Result<()> {
    fs::copy(source, destination).map_err(|error| {
        format_error(format!(
            "failed to copy {} to {}: {error}",
            source.display(),
            destination.display()
        ))
    })?;
    Ok(())
}

fn staging_dir(options: &Options) -> PathBuf {
    options.out_dir.join("staging").join(artifact_stem(options))
}

fn archive_path(options: &Options, target_kind: TargetKind) -> PathBuf {
    options.out_dir.join(format!(
        "{}.{}",
        artifact_stem(options),
        archive_extension(target_kind)
    ))
}

const fn archive_extension(target_kind: TargetKind) -> &'static str {
    match target_kind {
        TargetKind::Macos | TargetKind::Windows => "zip",
        TargetKind::Linux => "tar.gz",
    }
}

fn artifact_stem(options: &Options) -> String {
    format!("{BINARY_NAME}-{}-{}", options.version, options.target)
}

fn checksum_path(archive: &Path) -> PathBuf {
    PathBuf::from(format!("{}.sha256", archive.display()))
}

const fn binary_file_name(target_kind: TargetKind) -> &'static str {
    match target_kind {
        TargetKind::Macos | TargetKind::Linux => BINARY_NAME,
        TargetKind::Windows => "bcode.exe",
    }
}

const fn mermaid_worker_file_name(target_kind: TargetKind) -> &'static str {
    match target_kind {
        TargetKind::Macos | TargetKind::Linux => MERMAID_WORKER_BINARY_NAME,
        TargetKind::Windows => "bcode-mermaid-worker.exe",
    }
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

fn ensure_dir(path: &Path) -> Result<()> {
    if path.is_dir() {
        Ok(())
    } else {
        Err(format_error(format!(
            "expected directory {}",
            path.display()
        )))
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
    host_target_for(env::consts::ARCH, env::consts::OS)
}

fn host_target_for(arch: &str, os: &str) -> String {
    match (arch, os) {
        ("aarch64", "macos") => "aarch64-apple-darwin".to_owned(),
        ("x86_64", "macos") => "x86_64-apple-darwin".to_owned(),
        ("aarch64", "linux") => "aarch64-unknown-linux-gnu".to_owned(),
        ("x86_64", "linux") => "x86_64-unknown-linux-gnu".to_owned(),
        ("x86_64", "windows") => "x86_64-pc-windows-msvc".to_owned(),
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
           cargo xtask build [--target <triple>] [--features <feature,...>]\n\
           cargo xtask release --target <triple> --version <version> [--features <feature,...>]\n\
           cargo xtask verify-release --target <triple> --version <version> [--features <feature,...>]\n\
           cargo xtask dev-release [--target <triple>] [--features <feature,...>] [--identity <name>]\n\
           cargo xtask dev-sign --target <triple> [--binary <path>] [--identity <name>]\n\
           cargo xtask update-tesseract-catalog\n\
           cargo xtask discover-tesseract-upstream\n\
           cargo xtask update-tesseract-policy [--prune]\n\n\
         Supported release targets:\n\
           * aarch64-apple-darwin\n\
           * x86_64-apple-darwin\n\
           * aarch64-unknown-linux-gnu\n\
           * x86_64-unknown-linux-gnu\n\
           * x86_64-pc-windows-msvc\n\n\
         macOS release env:\n\
           * APPLE_CODESIGN_IDENTITY\n\
           * APPLE_ID, APPLE_APP_SPECIFIC_PASSWORD, APPLE_TEAM_ID for notarization\n\n\
         macOS dev signing:\n\
           * defaults to `Bcode Dev`\n\
           * override with --identity or BCODE_DEV_CODESIGN_IDENTITY"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_release_features_replace_distribution_and_imply_app() {
        let mut options = Options::help();
        options.features = Some(vec![
            "web-renderer".to_owned(),
            "static-bundled-shell-plugin".to_owned(),
        ]);
        assert_eq!(
            selected_features(&options, true),
            vec![
                "app".to_owned(),
                "static-bundled-shell-plugin".to_owned(),
                "web-renderer".to_owned(),
            ]
        );
    }

    #[test]
    fn release_and_build_feature_defaults_are_distinct() {
        let options = Options::help();
        assert_eq!(selected_features(&options, false), vec!["app".to_owned()]);
        assert_eq!(
            selected_features(&options, true),
            vec!["app".to_owned(), "distribution".to_owned()]
        );
    }

    #[test]
    fn optional_release_assets_follow_selected_features() {
        assert!(!features_enable_mermaid_worker(&["app".to_owned()]));
        assert!(features_enable_mermaid_worker(&[
            "mermaid-renderer".to_owned()
        ]));
        assert!(!features_enable_bundled_tesseract(&[
            "static-bundled-plugins".to_owned()
        ]));
        assert!(features_enable_bundled_tesseract(&[
            "bundled-ocr-tesseract-v5-5-2".to_owned()
        ]));
    }

    #[test]
    fn release_targets_are_exact_and_canonical() {
        assert_eq!(
            TargetKind::parse("x86_64-pc-windows-msvc").expect("Windows target"),
            TargetKind::Windows
        );
        assert_eq!(
            TargetKind::parse("aarch64-apple-darwin").expect("macOS target"),
            TargetKind::Macos
        );
        assert_eq!(
            TargetKind::parse("x86_64-unknown-linux-gnu").expect("Linux target"),
            TargetKind::Linux
        );
        assert!(TargetKind::parse("aarch64-pc-windows-msvc").is_err());
        assert!(TargetKind::parse("custom-linux-target").is_err());
    }

    #[test]
    fn windows_host_uses_the_canonical_msvc_target() {
        assert_eq!(
            host_target_for("x86_64", "windows"),
            "x86_64-pc-windows-msvc"
        );
    }

    #[test]
    fn windows_shorthand_suggests_the_canonical_target() {
        let error = TargetKind::parse("x86_64-windows").expect_err("shorthand must fail");
        assert!(error.0.contains("x86_64-pc-windows-msvc"));
    }

    #[test]
    fn platform_release_file_names_and_extensions_are_explicit() {
        assert_eq!(binary_file_name(TargetKind::Windows), "bcode.exe");
        assert_eq!(
            mermaid_worker_file_name(TargetKind::Windows),
            "bcode-mermaid-worker.exe"
        );
        assert_eq!(archive_extension(TargetKind::Windows), "zip");
        assert_eq!(archive_extension(TargetKind::Macos), "zip");
        assert_eq!(archive_extension(TargetKind::Linux), "tar.gz");
    }

    #[test]
    fn windows_signing_requires_a_certificate_path() {
        assert!(!windows_signing_requested_from(false));
        assert!(windows_signing_requested_from(true));
    }

    #[test]
    fn zip_entry_names_are_safe_and_portable() {
        assert_eq!(
            zip_entry_name(Path::new("bcode-runtimes/tesseract/manifest.json")).expect("safe path"),
            "bcode-runtimes/tesseract/manifest.json"
        );
        assert!(zip_entry_name(Path::new("../bcode.exe")).is_err());
        assert!(zip_entry_name(Path::new("/bcode.exe")).is_err());
    }

    #[test]
    fn zip_archive_contains_staged_files_and_rejects_missing_required_entries() {
        let temp = tempfile::tempdir().expect("temp dir");
        let staging = temp.path().join("staging");
        let runtime = staging.join("bcode-runtimes/tesseract/5.5.2");
        fs::create_dir_all(runtime.join("lib")).expect("runtime lib dir");
        fs::create_dir_all(runtime.join("tessdata")).expect("tessdata dir");
        fs::write(staging.join("bcode.exe"), b"bcode").expect("bcode");
        fs::write(staging.join("bcode-mermaid-worker.exe"), b"mermaid worker").expect("worker");
        fs::write(
            staging.join("bcode-runtimes/tesseract/manifest.json"),
            b"{}",
        )
        .expect("manifest");
        fs::write(runtime.join("lib/tesseract.dll"), b"runtime").expect("runtime DLL");
        fs::write(runtime.join("lib/leptonica.dll"), b"runtime").expect("Leptonica DLL");
        fs::write(runtime.join("tessdata/eng.traineddata"), b"language").expect("language data");
        let archive = temp.path().join("release.zip");
        create_zip_archive(&archive, &staging).expect("create archive");
        verify_archive_contents(&archive, TargetKind::Windows, true, true)
            .expect("complete archive");

        fs::remove_file(staging.join("bcode-mermaid-worker.exe")).expect("remove worker");
        let incomplete = temp.path().join("incomplete.zip");
        create_zip_archive(&incomplete, &staging).expect("create incomplete archive");
        assert!(verify_archive_contents(&incomplete, TargetKind::Windows, true, true).is_err());
    }

    #[test]
    #[cfg(unix)]
    fn archive_creation_rejects_staging_symlinks() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().expect("temp dir");
        let staging = temp.path().join("staging");
        fs::create_dir_all(&staging).expect("staging");
        let outside = temp.path().join("outside.exe");
        fs::write(&outside, b"outside").expect("outside file");
        symlink(&outside, staging.join("bcode.exe")).expect("symlink");
        let archive = temp.path().join("release.zip");
        assert!(create_zip_archive(&archive, &staging).is_err());
    }

    #[test]
    fn windows_archive_requires_canonical_leptonica_dll() {
        let entries = std::collections::BTreeSet::from([
            "bcode.exe".to_owned(),
            "bcode-mermaid-worker.exe".to_owned(),
            "bcode-runtimes/tesseract/manifest.json".to_owned(),
            "bcode-runtimes/tesseract/5.5.2/lib/tesseract.dll".to_owned(),
            "bcode-runtimes/tesseract/5.5.2/tessdata/eng.traineddata".to_owned(),
        ]);
        assert!(
            verify_required_archive_entries(
                &entries,
                Path::new("release.zip"),
                TargetKind::Windows,
                true,
                true,
            )
            .is_err()
        );
    }

    #[test]
    fn zip_extraction_rejects_parent_traversal() {
        let temp = tempfile::tempdir().expect("temp dir");
        let archive_path = temp.path().join("unsafe.zip");
        let mut writer = zip::ZipWriter::new(File::create(&archive_path).expect("archive"));
        writer
            .start_file("../escape.exe", zip::write::SimpleFileOptions::default())
            .expect("unsafe entry");
        writer.write_all(b"escape").expect("entry bytes");
        writer.finish().expect("finish archive");
        let mut archive = zip::ZipArchive::new(File::open(&archive_path).expect("open archive"))
            .expect("ZIP archive");
        let destination = temp.path().join("extract");
        fs::create_dir_all(&destination).expect("destination");
        assert!(extract_zip_confined(&mut archive, &destination).is_err());
        assert!(!temp.path().join("escape.exe").exists());
    }

    #[test]
    fn failed_release_smoke_removes_extraction_directory() {
        let temp = tempfile::tempdir().expect("temp dir");
        let archive = temp.path().join("invalid.zip");
        let staging = temp.path().join("staging");
        fs::create_dir_all(&staging).expect("staging");
        fs::write(staging.join("not-bcode"), b"invalid").expect("invalid artifact");
        create_zip_archive(&archive, &staging).expect("archive");
        let extraction = archive.with_extension("zip.verify");
        assert!(smoke_test_release_archive(&archive, TargetKind::Windows, true, true).is_err());
        assert!(!extraction.exists());
    }

    #[test]
    fn checksum_round_trip_detects_archive_changes() {
        let temp = tempfile::tempdir().expect("temp dir");
        let archive = temp.path().join("release.zip");
        fs::write(&archive, b"original").expect("archive");
        write_checksum(&archive).expect("write checksum");
        verify_checksum(&archive).expect("valid checksum");
        fs::write(&archive, b"changed").expect("change archive");
        assert!(verify_checksum(&archive).is_err());
    }
}
