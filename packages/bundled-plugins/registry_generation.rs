/// Derive registrations from direct optional implementation dependencies, never
/// from aggregate feature names or a second manually maintained plugin list.
pub fn implementation_features(manifest: &toml::Value) -> Vec<(&str, &str)> {
    let features = manifest["features"]
        .as_table()
        .expect("feature declarations");
    let dependencies = manifest["dependencies"].as_table().expect("dependencies");
    let mut implementations = Vec::new();
    for (feature, selections) in features {
        if !feature.starts_with("static-bundled-") {
            continue;
        }
        let selected: Vec<_> = selections
            .as_array()
            .expect("feature selections")
            .iter()
            .filter_map(toml::Value::as_str)
            .filter_map(|selection| selection.strip_prefix("dep:"))
            .filter(|name| name.starts_with("bcode_") && name.ends_with("_plugin"))
            .collect();
        if selected.is_empty() {
            // Aggregates and aliases enable features, not implementations.
            continue;
        }
        assert_eq!(
            selected.len(),
            1,
            "{feature} must select one plugin implementation"
        );
        let implementation = selected[0];
        assert!(
            dependencies
                .get(implementation)
                .is_some_and(|dependency| dependency
                    .get("optional")
                    .and_then(toml::Value::as_bool)
                    == Some(true)),
            "{feature} must select a declared optional implementation dependency"
        );
        implementations.push((feature.as_str(), implementation));
    }
    implementations
}

#[cfg(test)]
mod tests {
    use super::implementation_features;

    #[test]
    fn aggregates_and_aliases_do_not_become_plugin_registrations() {
        let manifest = toml::from_str(
            r#"
[dependencies]
bcode_example_plugin = { optional = true, workspace = true }
[features]
static-bundled-example-plugin = ["dep:bcode_example_plugin", "bcode_example_plugin/static-bundled"]
static-bundled-plugins = ["static-bundled-example-plugin"]
static-bundled-alias-plugin = ["static-bundled-example-plugin"]
"#,
        )
        .unwrap();
        assert_eq!(
            implementation_features(&manifest),
            vec![("static-bundled-example-plugin", "bcode_example_plugin")]
        );
    }

    #[test]
    fn actual_manifest_generates_one_registration_per_declared_implementation() {
        let manifest = toml::from_str(include_str!("Cargo.toml")).unwrap();
        let implementations = implementation_features(&manifest);
        assert_ne!(implementations, [] as [(&str, &str); 0]);
        let unique: std::collections::BTreeSet<_> = implementations
            .iter()
            .map(|(_, dependency)| *dependency)
            .collect();
        assert_eq!(unique.len(), implementations.len());
        assert!(
            implementations
                .iter()
                .all(|(feature, _)| *feature != "static-bundled-plugins")
        );
    }
}
