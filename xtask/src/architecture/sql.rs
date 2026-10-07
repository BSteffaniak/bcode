//! SQL boundaries over resolved metadata. See docs/architecture-validation.md.

use super::Graph;

fn driver(name: &str) -> bool {
    matches!(
        name,
        "rusqlite"
            | "libsqlite3-sys"
            | "sqlx"
            | "sqlx-core"
            | "sqlx-sqlite"
            | "sqlx-postgres"
            | "sqlx-mysql"
            | "postgres"
            | "tokio-postgres"
            | "diesel"
            | "mysql"
            | "mysql_async"
            | "sqlite"
    )
}

fn raw_feature(feature: &str) -> bool {
    feature == "raw-sql" || feature.ends_with("/raw-sql") || feature.ends_with("-raw-sql")
}

impl Graph {
    pub(super) fn check_sql(&self, _strict: bool, label: &str, failures: &mut Vec<String>) {
        for (id, node) in &self.nodes {
            let package = &self.packages[id];
            let owner = package.name.as_str();
            let bcode_package = self.members.contains(id)
                || owner.starts_with("bcode_")
                || owner.starts_with("brouter_")
                || owner == "bcode";
            for feature in &node.features {
                if raw_feature(feature) {
                    failures.push(format!("{label}: {owner} [{id}] activates {feature}; remove raw-SQL enablement (docs/architecture-validation.md)"));
                }
            }
            if !bcode_package {
                continue;
            }
            for feature in &node.features {
                for forwarded in package.features.get(feature).into_iter().flatten() {
                    if raw_feature(forwarded) {
                        failures.push(format!("{label}: {owner} feature {feature} -> {forwarded}; unapproved raw-SQL forwarding"));
                    }
                }
            }
            for dependency in &package.dependencies {
                if dependency.name.starts_with("switchy") && dependency.uses_default_features {
                    failures.push(format!("{label}: {owner} -> {} enables upstream defaults; use default-features = false to avoid additive raw-sql", dependency.name));
                }
                for feature in &dependency.features {
                    if raw_feature(feature) {
                        failures.push(format!("{label}: {owner} -> {} requests {feature}; specific user approval is required, not a missing capability", dependency.name));
                    }
                }
            }
            // Unlike portable-contract checks, include every resolved dependency kind here.
            for dependency in &node.deps {
                let name = &self.names[&dependency.pkg];
                if !driver(name) {
                    continue;
                }
                for kind in &dependency.dep_kinds {
                    failures.push(format!("{label}: {owner} -> {name} ({}, {}); direct SQL driver edge is forbidden by the selected SQL policy; use Switchy typed APIs", kind.kind.as_deref().unwrap_or("normal"), package.manifest_path));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn graph(owner: &str, kind: Option<&str>, feature: &str) -> Graph {
        serde_json::from_value::<super::super::Metadata>(serde_json::json!({
            "packages": [{"id":"owner","name":owner}, {"id":"driver","name":"rusqlite"}, {"id":"db","name":"switchy_database"}],
            "workspace_members": [],
            "resolve":{"nodes":[
                {"id":"owner","deps":[{"pkg":"driver","dep_kinds":[{"kind":kind}]}]},
                {"id":"driver","deps":[]},
                {"id":"db","features":[feature],"deps":[]}
            ]}
        })).unwrap().into()
    }

    #[test]
    fn forbidden_driver_edges_include_dev_and_build() {
        for kind in [None, Some("dev"), Some("build")] {
            let mut failures = Vec::new();
            graph("bcode_new_consumer", kind, "schema").check_sql(false, "test", &mut failures);
            assert_eq!(failures.len(), 1);
            assert!(failures[0].contains("bcode_new_consumer -> rusqlite"));
        }
    }

    #[test]
    fn switchy_backend_internals_are_allowed() {
        let mut failures = Vec::new();
        graph("switchy_database", None, "schema").check_sql(true, "test", &mut failures);
        assert!(failures.is_empty());
    }

    #[test]
    fn migrated_workflow_rejects_driver_edges_in_every_dependency_kind() {
        for kind in [None, Some("dev"), Some("build")] {
            let mut failures = Vec::new();
            graph("bcode_workflow_store", kind, "schema").check_sql(false, "test", &mut failures);
            assert_eq!(failures.len(), 1);
        }
        let mut failures = Vec::new();
        graph("bcode_workflow_store", None, "raw-sql").check_sql(true, "test", &mut failures);
        assert_eq!(failures.len(), 2);
    }

    #[test]
    fn migrated_observability_rejects_driver_edges_in_every_dependency_kind() {
        for kind in [None, Some("dev"), Some("build")] {
            let mut failures = Vec::new();
            graph("bcode_database_observability", kind, "schema").check_sql(
                false,
                "test",
                &mut failures,
            );
            assert_eq!(failures.len(), 1);
            assert!(failures[0].contains("bcode_database_observability -> rusqlite"));
        }
    }

    #[test]
    fn migrated_consumers_cannot_request_or_forward_raw_features() {
        let mut graph = graph("bcode_database_observability", Some("dev"), "raw-sql");
        graph.nodes.get_mut("owner").unwrap().deps.clear();
        graph.nodes.get_mut("owner").unwrap().features = vec!["raw-sql".into()];
        graph
            .packages
            .get_mut("owner")
            .unwrap()
            .features
            .insert("raw-sql".into(), vec!["switchy_database/raw-sql".into()]);
        let mut failures = Vec::new();
        graph.check_sql(false, "test", &mut failures);
        assert_eq!(failures.len(), 3);
        failures.clear();
        graph.check_sql(true, "test", &mut failures);
        assert_eq!(failures.len(), 3);
    }

    #[test]
    fn raw_feature_forwarders_and_default_enablement_are_rejected() {
        let mut graph = graph("bcode_new_consumer", None, "schema");
        graph.nodes.get_mut("owner").unwrap().deps.clear();
        graph
            .nodes
            .get_mut("owner")
            .unwrap()
            .features
            .push("database-raw-sql".into());
        graph.packages.get_mut("owner").unwrap().dependencies.push(
            super::super::DeclaredDependency {
                name: "switchy".into(),
                features: vec!["database-raw-sql".into()],
                uses_default_features: true,
            },
        );
        graph.packages.get_mut("owner").unwrap().features.insert(
            "database-raw-sql".into(),
            vec!["switchy/database-raw-sql".into()],
        );
        let mut failures = Vec::new();
        graph.check_sql(false, "test", &mut failures);
        assert_eq!(failures.len(), 4);
    }
}
