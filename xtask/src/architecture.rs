//! Dependency boundaries over Cargo's resolved package graph, never repository source text.

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
}

#[derive(Deserialize)]
struct Resolve {
    nodes: Vec<Node>,
}

#[derive(Deserialize)]
struct Node {
    id: String,
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
}

impl From<Metadata> for Graph {
    fn from(metadata: Metadata) -> Self {
        Self {
            names: metadata
                .packages
                .into_iter()
                .map(|p| (p.id, p.name))
                .collect(),
            edges: metadata
                .resolve
                .nodes
                .into_iter()
                .map(|node| {
                    let deps = node
                        .deps
                        .into_iter()
                        .filter(|dep| {
                            dep.dep_kinds
                                .iter()
                                .any(|kind| kind.kind.as_deref() != Some("dev"))
                        })
                        .map(|dep| dep.pkg)
                        .collect();
                    (node.id, deps)
                })
                .collect(),
            members: metadata.workspace_members.into_iter().collect(),
        }
    }
}

impl Graph {
    fn path(&self, root: &str, forbidden: impl Fn(&str) -> bool) -> Option<Vec<String>> {
        let mut queue = VecDeque::from([(root.to_owned(), vec![self.names[root].clone()])]);
        let mut seen = BTreeSet::new();
        while let Some((id, path)) = queue.pop_front() {
            if !seen.insert(id.clone()) {
                continue;
            }
            if id != root && forbidden(&self.names[&id]) {
                return Some(path);
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
            "bcode_tui" | "bcode_tui_components" | "ratatui" | "crossterm" | "termion"
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

fn forbidden_dependency(owner: &str, dependency: &str) -> bool {
    if let Some(parent) = owner.strip_suffix("_models") {
        return dependency == parent || implementation(dependency);
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

/// Check resolved production/build dependencies for the selected target.
///
/// # Errors
///
/// Returns an error when Cargo resolution fails or a checked boundary is violated.
pub fn check(target: &str) -> Result<()> {
    let graph = load(metadata_command(target).arg("--locked"))?;
    let mut failures = Vec::new();
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
    // Separate consumer workspaces prevent unrelated workspace members from unifying features.
    for (package, directory, features, required, denied) in [
        (
            "bcode",
            "packages/bcode",
            "app",
            &[][..],
            &["bcode_tesseract_sys", "bcode_hyperchad", "tantivy"][..],
        ),
        (
            "bcode",
            "packages/bcode",
            "distribution",
            &[
                "bcode_tesseract_sys",
                "bcode_hyperchad",
                "bcode_bundled_plugins",
            ][..],
            &[][..],
        ),
        (
            "bcode_cli",
            "packages/cli",
            "",
            &[][..],
            &["bcode_hyperchad"][..],
        ),
        (
            "bcode_cli",
            "packages/cli",
            "web-renderer",
            &[
                "bcode_hyperchad",
                "hyperchad_renderer_html_actix",
                "hyperchad_renderer_vanilla_js",
            ][..],
            &[][..],
        ),
        (
            "bcode_bundled_plugins",
            "packages/bundled-plugins",
            "",
            &[][..],
            &["tantivy", "zstd"][..],
        ),
    ] {
        check_profile(
            target,
            package,
            directory,
            features,
            required,
            denied,
            &mut failures,
        )?;
    }
    if !failures.is_empty() {
        return Err(format_error(failures.join("\n")));
    }
    println!("Dependency boundaries passed for {target}");
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
    std::fs::write(temp.path().join("Cargo.toml"), manifest)?;
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
}
