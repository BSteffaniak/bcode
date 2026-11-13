//! Dependency boundaries over Cargo's resolved package graph, never repository source text.

mod sql;

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::process::Command;

use serde::Deserialize;

use crate::{Result, format_error, workspace_root};

#[derive(Deserialize)]
struct Metadata {
    packages: Vec<Package>,
    workspace_members: Vec<String>,
    resolve: Resolve,
}

#[derive(Deserialize)]
struct Package {
    id: String,
    name: String,
    #[serde(default)]
    manifest_path: String,
    #[serde(default)]
    dependencies: Vec<DeclaredDependency>,
    #[serde(default)]
    features: BTreeMap<String, Vec<String>>,
}

#[derive(Deserialize)]
struct DeclaredDependency {
    name: String,
    #[serde(default)]
    features: Vec<String>,
    uses_default_features: bool,
}

#[derive(Deserialize)]
struct Resolve {
    nodes: Vec<Node>,
}

#[derive(Deserialize)]
struct Node {
    id: String,
    #[serde(default)]
    features: Vec<String>,
    deps: Vec<Dependency>,
}

#[derive(Deserialize)]
struct Dependency {
    pkg: String,
    dep_kinds: Vec<DependencyKind>,
}

#[derive(Deserialize)]
struct DependencyKind {
    kind: Option<String>,
}

struct Graph {
    names: BTreeMap<String, String>,
    edges: BTreeMap<String, Vec<String>>,
    members: BTreeSet<String>,
    packages: BTreeMap<String, Package>,
    nodes: BTreeMap<String, Node>,
}

impl From<Metadata> for Graph {
    fn from(metadata: Metadata) -> Self {
        Self {
            names: metadata
                .packages
                .iter()
                .map(|p| (p.id.clone(), p.name.clone()))
                .collect(),
            edges: metadata
                .resolve
                .nodes
                .iter()
                .map(|node| {
                    let deps = node
                        .deps
                        .iter()
                        .filter(|dep| {
                            dep.dep_kinds
                                .iter()
                                .any(|kind| kind.kind.as_deref() != Some("dev"))
                        })
                        .map(|dep| dep.pkg.clone())
                        .collect();
                    (node.id.clone(), deps)
                })
                .collect(),
            members: metadata.workspace_members.into_iter().collect(),
            packages: metadata
                .packages
                .into_iter()
                .map(|p| (p.id.clone(), p))
                .collect(),
            nodes: metadata
                .resolve
                .nodes
                .into_iter()
                .map(|n| (n.id.clone(), n))
                .collect(),
        }
    }
}

impl Graph {
    fn path(&self, root: &str, forbidden: impl Fn(&str) -> bool) -> Option<Vec<String>> {
        self.path_through(root, forbidden, |_| true)
    }

    /// Like [`Self::path`], but does not search below packages for which `transit` is false.
    fn path_through(
        &self,
        root: &str,
        forbidden: impl Fn(&str) -> bool,
        transit: impl Fn(&str) -> bool,
    ) -> Option<Vec<String>> {
        let mut queue = VecDeque::from([(root.to_owned(), vec![self.names[root].clone()])]);
        let mut seen = BTreeSet::new();
        while let Some((id, path)) = queue.pop_front() {
            if !seen.insert(id.clone()) {
                continue;
            }
            if id != root && forbidden(&self.names[&id]) {
                return Some(path);
            }
            if id != root && !transit(&self.names[&id]) {
                continue;
            }
            for dep in self.edges.get(&id).into_iter().flatten() {
                let mut next = path.clone();
                next.push(self.names[dep].clone());
                queue.push_back((dep.clone(), next));
            }
        }
        None
    }

    fn root(&self, name: &str) -> Result<&str> {
        let mut matches = self
            .names
            .iter()
            .filter(|(_, value)| value.as_str() == name);
        let (id, _) = matches
            .next()
            .ok_or_else(|| format_error(format!("missing package {name}")))?;
        if matches.next().is_some() {
            return Err(format_error(format!("ambiguous package {name}")));
        }
        Ok(id)
    }
}

fn terminal(name: &str) -> bool {
    name.starts_with("bmux_tui")
        || matches!(
            name,
            "bcode_tui"
                | "bcode_tui_components"
                | "bmux_terminal"
                | "ratatui"
                | "crossterm"
                | "termion"
        )
}

fn implementation(name: &str) -> bool {
    terminal(name)
        || matches!(
            name,
            "bcode_server"
                | "bcode_session"
                | "bcode_plugin"
                | "bcode_agent_runtime"
                | "reqwest"
                | "axum"
                | "rusqlite"
                | "switchy_database"
                | "switchy_database_connection"
        )
}

fn load(command: &mut Command) -> Result<Graph> {
    let output = command.output()?;
    if !output.status.success() {
        return Err(format_error(format!(
            "Cargo metadata failed: {}",
            String::from_utf8_lossy(&output.stderr)
        )));
    }
    let metadata: Metadata = serde_json::from_slice(&output.stdout)
        .map_err(|error| format_error(format!("invalid Cargo metadata: {error}")))?;
    Ok(metadata.into())
}

fn metadata_command(target: &str) -> Command {
    let mut command = Command::new("cargo");
    command.current_dir(workspace_root()).args([
        "metadata",
        "--format-version",
        "1",
        "--filter-platform",
        target,
    ]);
    command
}

/// Crates of a particular forge or stacking tool. The review's own data layer and interfaces
/// stay provider-neutral: providers are reached through `bcode_code_review_provider`, and only
/// the app's composition root (`bcode_code_review_ui`) and plugins name them.
fn review_provider(name: &str) -> bool {
    matches!(
        name,
        "bcode_code_review_github" | "bcode_code_review_graphite" | "bcode_forge"
    )
}

fn forbidden_dependency(owner: &str, dependency: &str) -> bool {
    if matches!(
        owner,
        "bcode_code_review" | "bcode_code_review_models" | "bcode_code_review_provider"
    ) && review_provider(dependency)
    {
        return true;
    }
    if let Some(parent) = owner.strip_suffix("_models") {
        return dependency == parent || implementation(dependency);
    }
    // Repository access backs tools and non-terminal hosts; presentation stays in the UI crate.
    if owner == "bcode_git" {
        return terminal(dependency);
    }
    // Raw config layering serves lightweight binaries; typed config and hosts stay out.
    if owner == "bcode_config_layers" {
        return implementation(dependency)
            || dependency.starts_with("bcode_")
            || dependency.starts_with("hyperchad");
    }
    owner == "brouter_router"
        && matches!(
            dependency,
            "bcode_server"
                | "bcode_agent_runtime"
                | "bcode_tui"
                | "brouter_server"
                | "brouter_provider"
                | "brouter_catalog"
                | "brouter_telemetry"
                | "brouter_config"
                | "brouter_introspection"
        )
}

/// The terminal transcript engine embedded by the `bcode_tui` host.
const TRANSCRIPT_ENGINE: &str = "bcode_tui_transcript";

/// Host, daemon-client and persistence crates the transcript engine must not reach: it holds
/// ephemeral presentation state only and the host owns I/O.
fn transcript_engine_forbidden(dependency: &str) -> bool {
    matches!(
        dependency,
        "bcode_tui" | "bcode_client" | "bcode_ipc" | "bcode_session" | "bcode_server"
    )
}

/// Packages whose own dependencies the transcript engine boundary does not inspect.
///
/// `bcode_session_view` is the shared semantic transcript reducer; its client-backed actions use
/// `bcode_client`, and its renderer neutrality is checked by the portable presentation profile.
fn transcript_engine_transit(dependency: &str) -> bool {
    dependency != "bcode_session_view"
}

fn check_transcript_engine(graph: &Graph, failures: &mut Vec<String>) -> Result<()> {
    let root = graph.root(TRANSCRIPT_ENGINE)?;
    if let Some(path) =
        graph.path_through(root, transcript_engine_forbidden, transcript_engine_transit)
    {
        failures.push(format!("transcript engine boundary: {}", path.join(" -> ")));
    }
    Ok(())
}

/// Check resolved production/build dependencies for the selected target.
///
/// # Errors
///
/// Returns an error when Cargo resolution fails or a checked boundary is violated.
pub fn check(target: &str) -> Result<()> {
    // The BMUX lock check reads the manifest and lockfile directly, so it runs first: a local
    // override can make `--locked` resolution fail, and its hint must not be lost behind that.
    let mut failures = crate::bmux_lock::workspace_failures()?;
    let graph = match load(metadata_command(target).arg("--locked")) {
        Ok(graph) => graph,
        Err(error) if failures.is_empty() => return Err(error),
        Err(error) => {
            failures.push(error.to_string());
            return Err(format_error(failures.join("\n")));
        }
    };
    check_sql_profiles(&graph, target, &mut failures)?;
    check_transcript_engine(&graph, &mut failures)?;
    for root in &graph.members {
        let name = &graph.names[root];
        let forbidden = |dep: &str| forbidden_dependency(name, dep);
        if let Some(path) = graph.path(root, forbidden) {
            failures.push(format!("contract boundary: {}", path.join(" -> ")));
        }
        if let Some(path) = graph.path(root, |dep| dep == "sha2-asm") {
            failures.push(format!("portable SHA-2: {}", path.join(" -> ")));
        }
    }
    for (package, directory) in [
        ("bcode_session_view", "packages/session-view"),
        ("bcode_hyperchad", "packages/hyperchad"),
        ("bcode_hyperchad_ui", "packages/hyperchad/ui"),
    ] {
        let portable = profile_graph(target, package, directory, "")?;
        if let Some(path) = portable.path(portable.root(package)?, terminal) {
            failures.push(format!("portable presentation: {}", path.join(" -> ")));
        }
    }
    check_feature_profiles(target, &mut failures)?;
    if !failures.is_empty() {
        return Err(format_error(failures.join("\n")));
    }
    println!("Dependency boundaries passed for {target}");
    Ok(())
}

/// `(package, directory, features, required, denied)` resolved in isolated consumer workspaces.
type FeatureProfile = (
    &'static str,
    &'static str,
    &'static str,
    &'static [&'static str],
    &'static [&'static str],
);

const FEATURE_PROFILES: &[FeatureProfile] = &[
    (
        "bcode_recap_cli",
        "packages/recap/cli",
        "",
        &[],
        &[
            "bmux_tui",
            "bmux_tui_components",
            "bmux_tui_runtime",
            "bmux_keyboard",
            "bmux_text_edit",
            "bmux_image",
            "bmux_terminal",
            "crossterm",
        ],
    ),
    (
        "bcode_recap_cli",
        "packages/recap/cli",
        "tui",
        &[
            "bmux_tui",
            "bmux_tui_components",
            "bmux_tui_runtime",
            "bmux_image",
        ],
        &[],
    ),
    (
        "bcode",
        "packages/bcode",
        "app",
        &[],
        &[
            "bcode_tesseract_sys",
            "bcode_hyperchad",
            "tantivy",
            "bcode_git_ui",
        ],
    ),
    (
        "bcode",
        "packages/bcode",
        "distribution",
        &[
            "bcode_tesseract_sys",
            "bcode_hyperchad",
            "bcode_bundled_plugins",
            "bcode_git_ui",
        ],
        &[],
    ),
    (
        // The standalone git client stays independent of agent, daemon and plugin hosting,
        // yet composes configuration through the same layering policy as bcode.
        "bcode_git_ui_cli",
        "packages/git-ui/cli",
        "",
        &["bcode_git_ui", "bmux_tui_runtime", "bcode_config_layers"],
        &[
            "bcode_plugin_sdk",
            "bcode_plugin",
            "bcode_config",
            "bcode_tui",
            "bcode_client",
            "bcode_server",
            "bcode_session",
            "bcode_agent_runtime",
            "bcode_model",
            "reqwest",
        ],
    ),
    (
        // The standalone review app runs its own terminal loop over the review data layer and
        // the daemon client, without the chat interface or in-process plugin hosting.
        "bcode_code_review_ui_cli",
        "packages/code-review-ui/cli",
        "",
        &[
            "bcode_code_review_ui",
            "bcode_code_review",
            "bmux_tui_runtime",
            "bcode_config_layers",
        ],
        &[
            "bcode_tui",
            "bcode_plugin",
            "bcode_code_review_plugin",
            "bcode_bundled_plugins",
            "bcode_server",
            "bcode_session",
            "bcode_agent_runtime",
        ],
    ),
    (
        // The review data layer and its document stay renderer-neutral and reach plugin
        // services only through the transport its caller provides.
        "bcode_code_review",
        "packages/code-review",
        "",
        &["bcode_code_review_models"],
        &[
            "bmux_tui",
            "bmux_tui_components",
            "bmux_tui_runtime",
            "bmux_terminal",
            "crossterm",
            "bcode_client",
            "bcode_plugin",
            "bcode_tui",
        ],
    ),
    ("bcode_cli", "packages/cli", "", &[], &["bcode_hyperchad"]),
    (
        "bcode_cli",
        "packages/cli",
        "web-renderer",
        &[
            "bcode_hyperchad",
            "hyperchad_renderer_html_actix",
            "hyperchad_renderer_vanilla_js",
        ],
        &[],
    ),
    (
        // The transcript engine and browser surface compile for wasm32: without the
        // `client` feature, session-view leaves daemon transport and plugin hosting out.
        "bcode_tui_transcript",
        "packages/tui-transcript",
        "",
        &["bcode_session_view"],
        &["bcode_client", "bcode_plugin_sdk", "bcode_provider_auth"],
    ),
    (
        "bcode_tui_web",
        "packages/tui-web",
        "wasm",
        &["bcode_tui_transcript", "bmux_web_renderer"],
        &["bcode_client", "bcode_plugin_sdk", "bcode_provider_auth"],
    ),
    (
        "bcode_bundled_plugins",
        "packages/bundled-plugins",
        "",
        &[],
        &["tantivy", "zstd"],
    ),
];

fn check_feature_profiles(target: &str, failures: &mut Vec<String>) -> Result<()> {
    // Separate consumer workspaces prevent unrelated workspace members from unifying features.
    for (package, directory, features, required, denied) in FEATURE_PROFILES {
        check_profile(
            target, package, directory, features, required, denied, failures,
        )?;
    }
    Ok(())
}

fn check_sql_profiles(graph: &Graph, target: &str, failures: &mut Vec<String>) -> Result<()> {
    let strict = std::env::var_os("BCODE_ARCHITECTURE_SQL_CLOSURE").is_some();
    graph.check_sql(strict, "workspace (including dev/build)", failures);
    for id in &graph.members {
        let package = &graph.packages[id];
        if std::path::Path::new(&package.manifest_path)
            .starts_with(workspace_root().join("plugins"))
        {
            let directory = std::path::Path::new(&package.manifest_path)
                .parent()
                .ok_or_else(|| format_error("plugin manifest has no parent"))?;
            let plugin = profile_graph(
                target,
                &package.name,
                &directory.to_string_lossy(),
                "default",
            )?;
            plugin.check_sql(
                strict,
                &format!("independent plugin {}", package.name),
                failures,
            );
        }
    }
    Ok(())
}

fn check_profile(
    target: &str,
    package: &str,
    directory: &str,
    features: &str,
    required: &[&str],
    denied: &[&str],
    failures: &mut Vec<String>,
) -> Result<()> {
    let graph = profile_graph(target, package, directory, features)?;
    let root = graph.root(package)?;
    graph.check_sql(
        std::env::var_os("BCODE_ARCHITECTURE_SQL_CLOSURE").is_some(),
        &format!("{package} [{features}]"),
        failures,
    );
    for dep in denied {
        if let Some(path) = graph.path(root, |name| name == *dep) {
            failures.push(format!("{package} [{features}]: {}", path.join(" -> ")));
        }
    }
    for dep in required {
        if graph.path(root, |name| name == *dep).is_none() {
            failures.push(format!("{package} [{features}] must activate {dep}"));
        }
    }
    Ok(())
}

// Binary-only subjects cannot be a dependency of the usual consumer probe. Resolve their
// real dependency/feature tables in a fresh workspace with a synthetic local target instead.
fn configure_binary_probe(
    probe: &mut toml_edit::DocumentMut,
    workspace: &toml_edit::DocumentMut,
    directory: &std::path::Path,
) -> Result<()> {
    let subject = std::fs::read_to_string(directory.join("Cargo.toml"))?
        .parse::<toml_edit::DocumentMut>()
        .map_err(|error| format_error(format!("invalid binary subject manifest: {error}")))?;
    let selected = probe["dependencies"]["subject"]["features"].clone();
    probe["package"] = subject["package"].clone();
    probe["package"]["autobins"] = toml_edit::value(false);
    probe["dependencies"] = subject["dependencies"].clone();
    probe["features"] = subject["features"].clone();
    probe["features"]["default"] = selected;
    probe["workspace"] = workspace["workspace"].clone();
    probe["workspace"]
        .as_table_mut()
        .expect("workspace table")
        .remove("members");
    probe["workspace"]
        .as_table_mut()
        .expect("workspace table")
        .remove("default-members");
    if let Some(dependencies) = probe["workspace"]["dependencies"].as_table_mut() {
        for (_, dependency) in dependencies.iter_mut() {
            if let Some(path) = dependency.get_mut("path")
                && let Some(relative) = path.as_str()
            {
                *path =
                    toml_edit::value(workspace_root().join(relative).to_string_lossy().as_ref());
            }
        }
    }
    probe["workspace"]["package"]["readme"] = toml_edit::value(
        workspace_root()
            .join("README.md")
            .to_string_lossy()
            .as_ref(),
    );
    Ok(())
}

fn profile_graph(target: &str, package: &str, directory: &str, features: &str) -> Result<Graph> {
    let temp = tempfile::tempdir()?;
    let path = workspace_root().join(directory);
    let manifest = format!(
        "[package]\nname = \"bcode-architecture-probe\"\nversion = \"0.0.0\"\nedition = \"2024\"\n[workspace]\n[lib]\npath = \"lib.rs\"\n[dependencies.subject]\npackage = {package:?}\npath = {:?}\ndefault-features = false\nfeatures = {:?}\n",
        path.to_string_lossy(),
        features
            .split(',')
            .filter(|f| !f.is_empty())
            .collect::<Vec<_>>()
    );
    // Cargo does not inherit patches from a path dependency's workspace root.
    // Preserve coordinator-owned local integration identities in every isolated probe.
    let workspace_manifest = std::fs::read_to_string(workspace_root().join("Cargo.toml"))?
        .parse::<toml_edit::DocumentMut>()
        .map_err(|error| format_error(format!("invalid workspace manifest: {error}")))?;
    let mut probe_manifest = manifest
        .parse::<toml_edit::DocumentMut>()
        .map_err(|error| format_error(format!("invalid probe manifest: {error}")))?;
    if matches!(
        package,
        "bcode_recap_cli" | "bcode_git_ui_cli" | "bcode_code_review_ui_cli"
    ) {
        configure_binary_probe(&mut probe_manifest, &workspace_manifest, &path)?;
    }
    if let Some(patches) = workspace_manifest.get("patch") {
        let mut patches = patches.clone();
        if let Some(sources) = patches.as_table_mut() {
            for (_, source) in sources.iter_mut() {
                if let Some(dependencies) = source.as_table_mut() {
                    for (_, dependency) in dependencies.iter_mut() {
                        if let Some(path) = dependency.get("path").and_then(toml_edit::Item::as_str)
                        {
                            let absolute = workspace_root().join(path);
                            dependency["path"] =
                                toml_edit::value(absolute.to_string_lossy().as_ref());
                        }
                    }
                }
            }
        }
        probe_manifest["patch"] = patches;
    }
    std::fs::write(temp.path().join("Cargo.toml"), probe_manifest.to_string())?;
    std::fs::write(temp.path().join("lib.rs"), "")?;
    std::fs::copy(
        workspace_root().join("Cargo.lock"),
        temp.path().join("Cargo.lock"),
    )?;
    load(
        metadata_command(target)
            .args(["--offline", "--manifest-path"])
            .arg(temp.path().join("Cargo.toml")),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolved_ids_follow_renames_build_edges_and_cycles_but_not_dev_edges() {
        let metadata = serde_json::json!({
            "packages": [{"id":"a","name":"contract"},{"id":"b","name":"bridge"},{"id":"c","name":"bcode_server"},{"id":"d","name":"dev_only"}],
            "workspace_members":["a"],
            "resolve":{"nodes":[
                {"id":"a","deps":[{"name":"renamed","pkg":"b","dep_kinds":[{"kind":null}]},{"pkg":"d","dep_kinds":[{"kind":"dev"}]}]},
                {"id":"b","deps":[{"pkg":"a","dep_kinds":[{"kind":null}]},{"pkg":"c","dep_kinds":[{"kind":"build"}]}]},
                {"id":"c","deps":[]},{"id":"d","deps":[]}
            ]}
        });
        let graph = Graph::from(serde_json::from_value::<Metadata>(metadata).unwrap());
        assert_eq!(
            graph.path("a", implementation),
            Some(vec![
                "contract".into(),
                "bridge".into(),
                "bcode_server".into()
            ])
        );
        assert!(graph.path("a", |name| name == "dev_only").is_none());
        assert!(graph.path("a", |name| name == "absent").is_none());
        assert!(graph.root("absent").is_err());
    }

    #[test]
    fn the_review_core_names_no_provider_and_the_app_may() {
        for owner in [
            "bcode_code_review",
            "bcode_code_review_models",
            "bcode_code_review_provider",
        ] {
            for provider in [
                "bcode_code_review_github",
                "bcode_code_review_graphite",
                "bcode_forge",
            ] {
                assert!(
                    forbidden_dependency(owner, provider),
                    "{owner} -> {provider}"
                );
            }
        }
        assert!(!forbidden_dependency(
            "bcode_code_review_ui",
            "bcode_code_review_github"
        ));
        assert!(!forbidden_dependency(
            "bcode_code_review",
            "bcode_code_review_provider"
        ));
    }

    #[test]
    fn transcript_engine_rejects_host_and_client_crates_but_allows_session_view() {
        let metadata = serde_json::json!({
            "packages": [
                {"id":"t","name":"bcode_tui_transcript"},
                {"id":"v","name":"bcode_session_view"},
                {"id":"c","name":"bcode_client"},
                {"id":"i","name":"bcode_ipc"},
                {"id":"b","name":"bridge"},
                {"id":"h","name":"bcode_tui"}
            ],
            "workspace_members":["t"],
            "resolve":{"nodes":[
                {"id":"t","deps":[{"pkg":"v","dep_kinds":[{"kind":null}]}]},
                {"id":"v","deps":[{"pkg":"c","dep_kinds":[{"kind":null}]}]},
                {"id":"c","deps":[{"pkg":"i","dep_kinds":[{"kind":null}]}]},
                {"id":"i","deps":[]},{"id":"b","deps":[]},{"id":"h","deps":[]}
            ]}
        });
        let allowed = Graph::from(serde_json::from_value::<Metadata>(metadata.clone()).unwrap());
        let mut failures = Vec::new();
        check_transcript_engine(&allowed, &mut failures).unwrap();
        assert!(failures.is_empty(), "{failures:?}");

        let mut through_bridge = metadata.clone();
        through_bridge["resolve"]["nodes"][0]["deps"] = serde_json::json!([
            {"pkg":"v","dep_kinds":[{"kind":null}]},
            {"pkg":"b","dep_kinds":[{"kind":null}]}
        ]);
        through_bridge["resolve"]["nodes"][4]["deps"] =
            serde_json::json!([{"pkg":"h","dep_kinds":[{"kind":null}]}]);
        let graph = Graph::from(serde_json::from_value::<Metadata>(through_bridge).unwrap());
        let mut failures = Vec::new();
        check_transcript_engine(&graph, &mut failures).unwrap();
        assert_eq!(
            failures,
            ["transcript engine boundary: bcode_tui_transcript -> bridge -> bcode_tui"]
        );

        let mut direct_client = metadata;
        direct_client["resolve"]["nodes"][0]["deps"] =
            serde_json::json!([{"pkg":"c","dep_kinds":[{"kind":"build"}]}]);
        let graph = Graph::from(serde_json::from_value::<Metadata>(direct_client).unwrap());
        let mut failures = Vec::new();
        check_transcript_engine(&graph, &mut failures).unwrap();
        assert_eq!(
            failures,
            ["transcript engine boundary: bcode_tui_transcript -> bcode_client"]
        );
    }

    #[test]
    fn config_layers_reject_bcode_hosts_and_terminals_but_allow_composition() {
        for dependency in [
            "bcode_config",
            "bcode_plugin_sdk",
            "bcode_tui",
            "bmux_tui",
            "reqwest",
        ] {
            assert!(forbidden_dependency("bcode_config_layers", dependency));
        }
        for dependency in ["bmux_config_composition", "toml", "serde", "thiserror"] {
            assert!(!forbidden_dependency("bcode_config_layers", dependency));
        }
    }
}
