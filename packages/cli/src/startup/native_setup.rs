//! Native setup is shared with the in-session execution picker.

pub(super) async fn run(
    shell: &mut bcode_tui::TerminalShell,
    no_discovery: bool,
) -> Result<bool, super::CliError> {
    let refresh = match bcode_tui::run_native_setup_in_shell(shell, no_discovery).await? {
        bcode_tui::NativeSetupOutcome::Unchanged => false,
        bcode_tui::NativeSetupOutcome::Refresh => true,
        bcode_tui::NativeSetupOutcome::ExternalApproved(candidate) => {
            Box::pin(super::external_setup::select_external_model_in_shell(
                shell, &candidate,
            ))
            .await?
        }
    };
    if refresh && !shell.stop_requested() {
        let host = super::super::load_cli_plugin_host()?;
        Box::pin(super::route_backend_selection(&host, shell, no_discovery)).await?;
    }
    Ok(true)
}
