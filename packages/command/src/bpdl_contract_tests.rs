#[cfg(test)]
mod bpdl_tests {
    #[test]
    fn command_registry_bpdl_contract_compiles() {
        let schema = bmux_plugin_schema::compile(include_str!("../bpdl/command.bpdl"))
            .expect("command registry BPDL schema should compile");
        assert_eq!(schema.plugin.plugin_id, "bcode.cmd");
        assert_eq!(schema.interfaces[0].name, "command-registry-query");
        let rust = bmux_plugin_schema::codegen_rust::emit(&schema);
        assert!(rust.contains("pub mod command_registry_query"));
        assert!(rust.contains("pub mod command_registry_command"));
        assert!(rust.contains("pub struct ListCommandsEndpoint"));
        assert!(rust.contains("pub struct RegisterEndpoint"));
        assert!(rust.contains("pub struct InvokeEndpoint"));
    }
}
