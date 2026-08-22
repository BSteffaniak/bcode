#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Bounded, deterministic workflow package and standalone-source discovery.
//!
//! This package owns filesystem source discovery and confinement. It does not validate against the
//! live plugin catalog, persist authored state, publish revisions, or start runs.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Maximum package or standalone candidates inspected by one discovery request.
pub const MAX_DISCOVERY_RESULTS: usize = bcode_workflow::MAX_WORKFLOW_LAUNCH_CATALOG_PAGE_SIZE;

/// Errors returned before a bounded discovery result can be produced.
#[derive(Debug, Error)]
pub enum WorkflowDiscoveryError {
    #[error("workflow discovery I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("workflow discovery configuration error: {0}")]
    Config(#[from] bcode_config::ConfigError),
    #[error("workflow discovery source error: {0}")]
    Workflow(#[from] bcode_workflow::WorkflowError),
    #[error("workflow discovery JSON error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("workflow discovery TOML error: {0}")]
    Toml(#[from] toml::de::Error),
    #[error("workflow discovery failed: {0}")]
    Invalid(String),
}

/// One secret-safe diagnostic for a source that could not become a launch candidate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowDiscoveryDiagnostic {
    pub source_label: String,
    pub path: PathBuf,
    pub code: String,
    pub message: String,
}

/// One confined discovered source before live-catalog validation and publication lookup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiscoveredWorkflowSource {
    Package {
        package_id: String,
        source_label: String,
        precedence: u32,
        manifest_path: PathBuf,
        closure: bcode_workflow::WorkflowPackageClosure,
    },
    Standalone {
        source_label: String,
        precedence: u32,
        source_path: PathBuf,
        source_format: bcode_workflow::WorkflowSourceFormat,
        source: String,
    },
}

impl DiscoveredWorkflowSource {
    /// Return the deterministic filesystem identity used to page discovery results.
    #[must_use]
    pub fn source_key(&self) -> String {
        match self {
            Self::Package {
                package_id,
                manifest_path,
                ..
            } => format!("package:{package_id}:{}", manifest_path.display()),
            Self::Standalone { source_path, .. } => {
                format!("source:{}", source_path.display())
            }
        }
    }

    /// Return configured source precedence; lower values win.
    #[must_use]
    pub const fn precedence(&self) -> u32 {
        match self {
            Self::Package { precedence, .. } | Self::Standalone { precedence, .. } => *precedence,
        }
    }
}

/// Complete bounded, non-mutating discovery result.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct WorkflowDiscoveryResult {
    pub sources: Vec<DiscoveredWorkflowSource>,
    pub diagnostics: Vec<WorkflowDiscoveryDiagnostic>,
}

#[derive(Debug, Clone)]
struct DiscoveryRoot {
    path: PathBuf,
    label: String,
    precedence: u32,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct SourcePackageImport {
    import_id: String,
    package_id: String,
    export: String,
    #[serde(default)]
    manifest: Option<String>,
    #[serde(default)]
    target: Option<bcode_workflow::WorkflowCallTarget>,
    #[serde(default)]
    package_lock_digest_sha256: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct SourcePackageManifest {
    version: u32,
    package_id: String,
    exports: BTreeMap<String, String>,
    #[serde(default)]
    external_dependencies: BTreeMap<String, bcode_workflow::WorkflowCallTarget>,
    #[serde(default)]
    imports: Vec<SourcePackageImport>,
    members: Vec<SourcePackageMember>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct SourcePackageMember {
    member_id: String,
    source_name: String,
    #[serde(default)]
    dependencies: Vec<String>,
    #[serde(default)]
    external_dependencies: Vec<String>,
}

/// Discover bounded package and standalone workflow sources using the configured root policy.
///
/// Equal-precedence package identities are removed and surfaced as ambiguity diagnostics. Package
/// member files are suppressed from standalone results.
///
/// # Errors
///
/// Returns an error when the workspace cannot be canonicalized or request bounds are invalid.
#[allow(clippy::too_many_lines)]
pub fn discover_workflows(
    workspace: &Path,
    config: &bcode_config::WorkflowsConfig,
    limit: usize,
) -> Result<WorkflowDiscoveryResult, WorkflowDiscoveryError> {
    if limit == 0 || limit > MAX_DISCOVERY_RESULTS {
        return Err(WorkflowDiscoveryError::Invalid(format!(
            "workflow discovery limit must be within 1..={MAX_DISCOVERY_RESULTS}"
        )));
    }
    let workspace = fs::canonicalize(workspace)?;
    let roots = discovery_roots(&workspace, config);
    let mut result = WorkflowDiscoveryResult::default();
    let mut packages = BTreeMap::<String, DiscoveredWorkflowSource>::new();
    let mut ambiguous_packages = BTreeSet::new();
    let mut package_members = BTreeSet::new();

    for root in &roots {
        for manifest_path in matching_files(&root.path, is_package_manifest, limit)? {
            match read_package_closure(&manifest_path, &root.path) {
                Ok((closure, members)) => {
                    package_members.extend(members);
                    let package_id = closure.entry_package_id.clone();
                    let candidate = DiscoveredWorkflowSource::Package {
                        package_id: package_id.clone(),
                        source_label: root.label.clone(),
                        precedence: root.precedence,
                        manifest_path: manifest_path.clone(),
                        closure,
                    };
                    match packages.get(&package_id) {
                        Some(existing) if existing.precedence() == root.precedence => {
                            ambiguous_packages.insert(package_id.clone());
                            result.diagnostics.push(WorkflowDiscoveryDiagnostic {
                                source_label: root.label.clone(),
                                path: manifest_path,
                                code: "ambiguous_package_identity".to_string(),
                                message: format!(
                                    "package '{package_id}' appears more than once at precedence {}",
                                    root.precedence
                                ),
                            });
                        }
                        Some(existing) if existing.precedence() < root.precedence => {}
                        _ => {
                            packages.insert(package_id, candidate);
                        }
                    }
                }
                Err(error) => result.diagnostics.push(WorkflowDiscoveryDiagnostic {
                    source_label: root.label.clone(),
                    path: manifest_path,
                    code: "invalid_package".to_string(),
                    message: error.to_string(),
                }),
            }
        }
    }
    for package_id in ambiguous_packages {
        packages.remove(&package_id);
    }
    result.sources.extend(packages.into_values());

    for root in &roots {
        for source_path in matching_files(&root.path, is_standalone_source, limit)? {
            let canonical = match fs::canonicalize(&source_path) {
                Ok(path) => path,
                Err(error) => {
                    result.diagnostics.push(WorkflowDiscoveryDiagnostic {
                        source_label: root.label.clone(),
                        path: source_path,
                        code: "unreadable_source".to_string(),
                        message: error.to_string(),
                    });
                    continue;
                }
            };
            if package_members.contains(&canonical) {
                continue;
            }
            match read_bounded_source(&canonical) {
                Ok(source) => {
                    let Some(name) = canonical.file_name().and_then(std::ffi::OsStr::to_str) else {
                        continue;
                    };
                    match bcode_workflow::WorkflowSourceFormat::from_file_name(name) {
                        Ok(source_format) => {
                            result.sources.push(DiscoveredWorkflowSource::Standalone {
                                source_label: root.label.clone(),
                                precedence: root.precedence,
                                source_path: canonical,
                                source_format,
                                source,
                            });
                        }
                        Err(error) => result.diagnostics.push(WorkflowDiscoveryDiagnostic {
                            source_label: root.label.clone(),
                            path: canonical,
                            code: "unsupported_source_format".to_string(),
                            message: error.to_string(),
                        }),
                    }
                }
                Err(error) => result.diagnostics.push(WorkflowDiscoveryDiagnostic {
                    source_label: root.label.clone(),
                    path: canonical,
                    code: "invalid_source".to_string(),
                    message: error.to_string(),
                }),
            }
        }
    }
    result.sources.sort_by(|left, right| {
        (left.precedence(), left.source_key()).cmp(&(right.precedence(), right.source_key()))
    });
    result.sources.truncate(limit);
    result.diagnostics.sort_by(|left, right| {
        (&left.source_label, &left.path, &left.code).cmp(&(
            &right.source_label,
            &right.path,
            &right.code,
        ))
    });
    result.diagnostics.truncate(limit);
    Ok(result)
}

/// Read one explicit package manifest or standalone workflow source outside automatic roots.
///
/// The exact file is canonicalized and treated as its own authorized root boundary. Package member
/// and import paths remain confined beneath the manifest parent.
///
/// # Errors
///
/// Returns an error when the path is missing, unsupported, unconfined, or exceeds source bounds.
pub fn inspect_explicit_source(
    path: &Path,
) -> Result<DiscoveredWorkflowSource, WorkflowDiscoveryError> {
    let path = fs::canonicalize(path)?;
    if !path.is_file() {
        return Err(WorkflowDiscoveryError::Invalid(
            "explicit workflow source is not a file".to_string(),
        ));
    }
    let name = path
        .file_name()
        .and_then(std::ffi::OsStr::to_str)
        .ok_or_else(|| WorkflowDiscoveryError::Invalid("source name is not UTF-8".to_string()))?;
    if is_package_manifest(name) {
        let root = path.parent().ok_or_else(|| {
            WorkflowDiscoveryError::Invalid("package manifest has no parent".to_string())
        })?;
        let (closure, _) = read_package_closure(&path, root)?;
        return Ok(DiscoveredWorkflowSource::Package {
            package_id: closure.entry_package_id.clone(),
            source_label: "explicit".to_string(),
            precedence: 0,
            manifest_path: path,
            closure,
        });
    }
    if !is_standalone_source(name) {
        return Err(WorkflowDiscoveryError::Invalid(
            "explicit workflow source has an unsupported file name".to_string(),
        ));
    }
    let source_format = bcode_workflow::WorkflowSourceFormat::from_file_name(name)?;
    Ok(DiscoveredWorkflowSource::Standalone {
        source_label: "explicit".to_string(),
        precedence: 0,
        source_path: path.clone(),
        source_format,
        source: read_bounded_source(&path)?,
    })
}

fn discovery_roots(workspace: &Path, config: &bcode_config::WorkflowsConfig) -> Vec<DiscoveryRoot> {
    let mut roots = Vec::new();
    if config.include_repo_workflows {
        roots.extend([
            DiscoveryRoot {
                path: workspace.join(".bcode/workflows"),
                label: "repository:.bcode/workflows".to_string(),
                precedence: 10,
            },
            DiscoveryRoot {
                path: workspace.join("workflows"),
                label: "repository:workflows".to_string(),
                precedence: 20,
            },
        ]);
    }
    roots.extend(
        config
            .paths
            .iter()
            .enumerate()
            .map(|(index, path)| DiscoveryRoot {
                path: path.clone(),
                label: "configured".to_string(),
                precedence: 30_u32.saturating_add(u32::try_from(index).unwrap_or(u32::MAX - 30)),
            }),
    );
    if config.include_user_workflows {
        roots.extend([
            DiscoveryRoot {
                path: bcode_config::default_config_dir().join("workflows"),
                label: "user-config:workflows".to_string(),
                precedence: 100,
            },
            DiscoveryRoot {
                path: bcode_config::default_state_dir().join("workflows"),
                label: "user-state:workflows".to_string(),
                precedence: 110,
            },
        ]);
    }
    roots
}

fn matching_files(
    root: &Path,
    predicate: fn(&str) -> bool,
    limit: usize,
) -> Result<Vec<PathBuf>, std::io::Error> {
    if !root.is_dir() {
        return Ok(Vec::new());
    }
    let mut paths = fs::read_dir(root)?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.is_file()
                && path
                    .file_name()
                    .and_then(std::ffi::OsStr::to_str)
                    .is_some_and(predicate)
        })
        .collect::<Vec<_>>();
    paths.sort();
    paths.truncate(limit);
    Ok(paths)
}

fn is_package_manifest(name: &str) -> bool {
    [
        ".workflow-package.json",
        ".workflow-package.yaml",
        ".workflow-package.yml",
        ".workflow-package.toml",
    ]
    .iter()
    .any(|suffix| name.ends_with(suffix))
}

fn is_standalone_source(name: &str) -> bool {
    !is_package_manifest(name)
        && [
            ".workflow.json",
            ".workflow.yaml",
            ".workflow.yml",
            ".workflow.toml",
        ]
        .iter()
        .any(|suffix| name.ends_with(suffix))
}

fn read_package_closure(
    entry: &Path,
    authorized_root: &Path,
) -> Result<(bcode_workflow::WorkflowPackageClosure, BTreeSet<PathBuf>), WorkflowDiscoveryError> {
    let entry = fs::canonicalize(entry)?;
    let authorized_root = fs::canonicalize(authorized_root)?;
    if !entry.starts_with(&authorized_root) {
        return Err(WorkflowDiscoveryError::Invalid(
            "workflow package manifest escapes its authorized root".to_string(),
        ));
    }
    let mut pending = vec![(entry, 1_usize)];
    let mut visited = BTreeSet::new();
    let mut packages = Vec::new();
    let mut members = BTreeSet::new();
    while let Some((manifest_path, depth)) = pending.pop() {
        if depth > bcode_workflow::MAX_WORKFLOW_PACKAGE_DEPTH {
            return Err(WorkflowDiscoveryError::Invalid(
                "workflow package import depth exceeds the package bound".to_string(),
            ));
        }
        if !visited.insert(manifest_path.clone()) {
            continue;
        }
        if packages.len() >= bcode_workflow::MAX_WORKFLOW_PACKAGE_CLOSURE_PACKAGES {
            return Err(WorkflowDiscoveryError::Invalid(
                "workflow package closure exceeds the package-count bound".to_string(),
            ));
        }
        let (manifest, manifest_members, imports) =
            read_package_manifest(&manifest_path, &authorized_root)?;
        members.extend(manifest_members);
        pending.extend(imports.into_iter().rev().map(|path| (path, depth + 1)));
        packages.push(bcode_workflow::WorkflowPackageClosureSource {
            package_id: manifest.package_id.clone(),
            source_name: Some(manifest_path.display().to_string()),
            manifest,
        });
    }
    let entry_package_id = packages
        .first()
        .ok_or_else(|| WorkflowDiscoveryError::Invalid("empty package closure".to_string()))?
        .package_id
        .clone();
    Ok((
        bcode_workflow::WorkflowPackageClosure {
            version: bcode_workflow::WORKFLOW_PACKAGE_CLOSURE_VERSION,
            entry_package_id,
            packages,
        },
        members,
    ))
}

fn read_package_manifest(
    path: &Path,
    authorized_root: &Path,
) -> Result<
    (
        bcode_workflow::WorkflowPackageManifest,
        BTreeSet<PathBuf>,
        Vec<PathBuf>,
    ),
    WorkflowDiscoveryError,
> {
    let manifest_path = fs::canonicalize(path)?;
    let package_root = manifest_path.parent().ok_or_else(|| {
        WorkflowDiscoveryError::Invalid("workflow package manifest has no parent".to_string())
    })?;
    if !manifest_path.starts_with(authorized_root) {
        return Err(WorkflowDiscoveryError::Invalid(
            "workflow package manifest escapes its authorized root".to_string(),
        ));
    }
    let manifest_source = read_bounded_package_source(&manifest_path)?;
    let name = manifest_path
        .file_name()
        .and_then(std::ffi::OsStr::to_str)
        .ok_or_else(|| WorkflowDiscoveryError::Invalid("manifest name is not UTF-8".to_string()))?;
    let decoded: SourcePackageManifest =
        match bcode_workflow::WorkflowSourceFormat::from_file_name(name)? {
            bcode_workflow::WorkflowSourceFormat::Json => serde_json::from_str(&manifest_source)?,
            bcode_workflow::WorkflowSourceFormat::Yaml => yaml_serde::from_str(&manifest_source)
                .map_err(|error| {
                    WorkflowDiscoveryError::Invalid(format!("invalid YAML manifest: {error}"))
                })?,
            bcode_workflow::WorkflowSourceFormat::Toml => toml::from_str(&manifest_source)?,
        };
    let mut member_paths = BTreeSet::new();
    let mut total_bytes = manifest_source.len();
    let mut manifest = bcode_workflow::WorkflowPackageManifest {
        version: decoded.version,
        package_id: decoded.package_id,
        exports: decoded.exports,
        external_dependencies: decoded.external_dependencies,
        imports: decoded
            .imports
            .iter()
            .map(|import| bcode_workflow::WorkflowPackageImport {
                import_id: import.import_id.clone(),
                package_id: import.package_id.clone(),
                export: import.export.clone(),
                manifest: import.manifest.clone(),
                target: import.target.clone(),
                package_lock_digest_sha256: import.package_lock_digest_sha256.clone(),
            })
            .collect(),
        members: decoded
            .members
            .into_iter()
            .map(|member| bcode_workflow::WorkflowPackageMember {
                member_id: member.member_id,
                source_name: member.source_name,
                format: bcode_workflow::WorkflowSourceFormat::Json,
                source: String::new(),
                dependencies: member.dependencies,
                external_dependencies: member.external_dependencies,
            })
            .collect(),
    };
    for member in &mut manifest.members {
        let relative = confined_relative_path(&member.source_name)?;
        let path = fs::canonicalize(package_root.join(relative))?;
        if !path.starts_with(package_root) || !path.is_file() {
            return Err(WorkflowDiscoveryError::Invalid(format!(
                "workflow package member '{}' escapes its package root or is not a file",
                member.source_name
            )));
        }
        let source = fs::read_to_string(&path)?;
        total_bytes = total_bytes.checked_add(source.len()).ok_or_else(|| {
            WorkflowDiscoveryError::Invalid("workflow package byte count overflow".to_string())
        })?;
        if total_bytes > bcode_workflow::MAX_WORKFLOW_PACKAGE_SOURCE_BYTES {
            return Err(WorkflowDiscoveryError::Invalid(
                "workflow package sources exceed the package byte bound".to_string(),
            ));
        }
        member.format = bcode_workflow::WorkflowSourceFormat::from_file_name(&member.source_name)?;
        member.source = source;
        member_paths.insert(path);
    }
    manifest.validate()?;
    let mut imports = Vec::new();
    for import in &decoded.imports {
        if let Some(relative) = &import.manifest {
            let path = fs::canonicalize(package_root.join(confined_relative_path(relative)?))?;
            if !path.starts_with(authorized_root) || !path.is_file() {
                return Err(WorkflowDiscoveryError::Invalid(format!(
                    "workflow package import '{relative}' escapes its authorized root or is not a file"
                )));
            }
            imports.push(path);
        }
    }
    imports.sort();
    Ok((manifest, member_paths, imports))
}

fn confined_relative_path(value: &str) -> Result<&Path, WorkflowDiscoveryError> {
    let path = Path::new(value);
    if path.is_absolute()
        || path
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(WorkflowDiscoveryError::Invalid(format!(
            "workflow path '{value}' is not confined"
        )));
    }
    Ok(path)
}

fn read_bounded_package_source(path: &Path) -> Result<String, WorkflowDiscoveryError> {
    let source = fs::read_to_string(path)?;
    if source.len() > bcode_workflow::MAX_WORKFLOW_PACKAGE_SOURCE_BYTES {
        return Err(WorkflowDiscoveryError::Invalid(
            "workflow package manifest exceeds the package byte bound".to_string(),
        ));
    }
    Ok(source)
}

fn read_bounded_source(path: &Path) -> Result<String, WorkflowDiscoveryError> {
    let source = fs::read_to_string(path)?;
    if source.len() > bcode_workflow::MAX_WORKFLOW_AUTHORING_DOCUMENT_BYTES {
        return Err(WorkflowDiscoveryError::Invalid(
            "workflow source exceeds the authoring byte bound".to_string(),
        ));
    }
    Ok(source)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn source(workflow_id: &str, title: &str) -> String {
        format!(
            r"workflow_source_version: 3
workflow_id: {workflow_id}
title: {title}
steps:
  - id: done
    name: Done
    output:
      type_name: example/output
      schema: {{}}
"
        )
    }

    #[test]
    fn discovers_standalone_sources_and_suppresses_package_members() {
        let temp = tempfile::tempdir().expect("temp");
        let root = temp.path().join(".bcode/workflows");
        fs::create_dir_all(&root).expect("root");
        fs::write(
            root.join("standalone.workflow.yaml"),
            source("standalone", "Standalone"),
        )
        .expect("standalone");
        fs::write(
            root.join("member.workflow.yaml"),
            source("member", "Member"),
        )
        .expect("member");
        fs::write(
            root.join("example.workflow-package.yaml"),
            r"version: 3
package_id: example/package
exports: { main: member }
members:
  - member_id: member
    source_name: member.workflow.yaml
",
        )
        .expect("manifest");

        let result = discover_workflows(
            temp.path(),
            &bcode_config::WorkflowsConfig {
                include_repo_workflows: true,
                include_user_workflows: false,
                paths: Vec::new(),
            },
            20,
        )
        .expect("discovery");

        assert_eq!(result.sources.len(), 2);
        assert!(result.sources.iter().any(|source| matches!(
            source,
            DiscoveredWorkflowSource::Package { package_id, .. }
                if package_id == "example/package"
        )));
        assert!(result.sources.iter().any(|source| matches!(
            source,
            DiscoveredWorkflowSource::Standalone { source_path, .. }
                if source_path.ends_with("standalone.workflow.yaml")
        )));
    }

    #[test]
    fn equal_precedence_duplicate_packages_fail_closed_as_ambiguous() {
        let temp = tempfile::tempdir().expect("temp");
        let configured = temp.path().join("configured");
        fs::create_dir_all(&configured).expect("configured");
        for suffix in ["a", "b"] {
            fs::write(
                configured.join(format!("{suffix}.workflow.yaml")),
                source(suffix, suffix),
            )
            .expect("source");
            fs::write(
                configured.join(format!("{suffix}.workflow-package.yaml")),
                format!(
                    "version: 3\npackage_id: duplicate/package\nexports: {{ main: {suffix} }}\nmembers:\n  - member_id: {suffix}\n    source_name: {suffix}.workflow.yaml\n"
                ),
            )
            .expect("manifest");
        }
        let result = discover_workflows(
            temp.path(),
            &bcode_config::WorkflowsConfig {
                include_repo_workflows: false,
                include_user_workflows: false,
                paths: vec![configured],
            },
            20,
        )
        .expect("discovery");
        assert!(result.sources.is_empty());
        assert!(
            result
                .diagnostics
                .iter()
                .any(|diagnostic| diagnostic.code == "ambiguous_package_identity")
        );
    }

    #[test]
    fn rejects_parent_traversal_in_package_members() {
        let temp = tempfile::tempdir().expect("temp");
        let root = temp.path().join("workflows");
        fs::create_dir_all(&root).expect("root");
        fs::write(
            root.join("bad.workflow-package.yaml"),
            "version: 3\npackage_id: bad/package\nexports: { main: bad }\nmembers:\n  - member_id: bad\n    source_name: ../bad.workflow.yaml\n",
        )
        .expect("manifest");
        let result = discover_workflows(
            temp.path(),
            &bcode_config::WorkflowsConfig {
                include_repo_workflows: true,
                include_user_workflows: false,
                paths: Vec::new(),
            },
            20,
        )
        .expect("discovery");
        assert!(result.sources.is_empty());
        assert_eq!(result.diagnostics[0].code, "invalid_package");
    }

    #[test]
    fn explicit_source_inspection_is_confined_to_the_exact_file() {
        let temp = tempfile::tempdir().expect("temp");
        let path = temp.path().join("outside.workflow.yaml");
        fs::write(&path, source("outside", "Outside")).expect("source");
        let discovered = inspect_explicit_source(&path).expect("explicit source");
        assert!(matches!(
            discovered,
            DiscoveredWorkflowSource::Standalone {
                source_label,
                source_path,
                source_format: bcode_workflow::WorkflowSourceFormat::Yaml,
                ..
            } if source_label == "explicit" && source_path == fs::canonicalize(path).expect("path")
        ));
    }
}
