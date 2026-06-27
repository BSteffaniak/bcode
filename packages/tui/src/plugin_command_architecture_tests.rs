//! Architecture guardrails for plugin-owned command palette behavior.

#[test]
fn palette_flow_does_not_dispatch_migrated_plugin_commands_by_id() {
    let source = include_str!("palette_flow.rs");
    for forbidden in [
        "command.work-tree",
        "model.status",
        "model.serverStatus",
        "runtime.status",
        "model.select",
        "skills.list",
        "skills.active",
        "diff.toggle",
        "bcode.worktree",
        "bcode.model",
        "bcode.skills",
        "bcode.code_review",
        "host_route",
    ] {
        assert!(
            !source.contains(forbidden),
            "palette_flow.rs must not dispatch plugin command behavior by hardcoded id: {forbidden}"
        );
    }
}
