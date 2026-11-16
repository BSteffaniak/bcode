//! Generate the path-discovery transport from its contract-only owner.

fn main() {
    let schema_path = "../filesystem-plugin/models/bpdl/path-search.bpdl";
    println!("cargo:rerun-if-changed={schema_path}");
    let source = std::fs::read_to_string(schema_path).expect("read path-search contract");
    let schema = bmux_plugin_schema::compile(&source).expect("compile path-search contract");
    let output = std::path::PathBuf::from(std::env::var_os("OUT_DIR").expect("OUT_DIR"));
    std::fs::write(
        output.join("path_search.rs"),
        bmux_plugin_schema::codegen_rust::emit(&schema),
    )
    .expect("write generated path-search transport");
}
