use std::{env, fs, path::PathBuf};
mod registry_generation;

fn main() {
    println!("cargo:rerun-if-changed=Cargo.toml");
    println!("cargo:rerun-if-changed=registry_generation.rs");
    let manifest_path =
        PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").expect("manifest directory"))
            .join("Cargo.toml");
    // Value::from_str parses one TOML value in toml 1.x, not a document.
    // Cargo.toml is a document: deserialize its tables with toml::from_str.
    let manifest: toml::Value =
        toml::from_str(&fs::read_to_string(manifest_path).expect("read bundled manifest"))
            .expect("parse bundled manifest document");
    let mut generated = String::from(
        "/// Inert shared definitions selected by the bundled feature set.\n\
         /// Factories are not called until the host creates a scoped component.\n\
         #[must_use]\n\
         pub fn bundled_plugin_definitions() -> Vec<bmux_plugin_sdk::PluginDefinition> {\n\
         let mut definitions = Vec::new();\n\
         append_bundled_plugin_definitions(&mut definitions);\n\
         definitions\n}\n\
         fn append_bundled_plugin_definitions(definitions: &mut Vec<bmux_plugin_sdk::PluginDefinition>) {\n\
         definitions.reserve(0);\n",
    );
    for (feature, dependency) in registry_generation::implementation_features(&manifest) {
        // Do not replicate IDs, declarations, lifecycle or transport behavior.
        // Each implementation owns its one shared definition.
        generated.push_str(&format!(
            "#[cfg(feature = {feature:?})]\ndefinitions.push({dependency}::plugin_definition());\n"
        ));
    }
    generated.push_str("}\n");
    let output = PathBuf::from(env::var_os("OUT_DIR").expect("Cargo output directory"));
    fs::write(output.join("shared_definitions.rs"), generated).expect("write definition registry");
}
