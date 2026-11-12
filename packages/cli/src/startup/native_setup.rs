//! Native setup is shared with the in-session execution picker.

pub(super) async fn run(
    shell: &mut bcode_tui::TerminalShell,
    no_discovery: bool,
) -> Result<bool, super::CliError> {
    let refresh = match bcode_tui::run_native_setup_in_shell(shell, no_discovery).await? {
        bcode_tui::NativeSetupOutcome::Unchanged => false,
        bcode_tui::NativeSetupOutcome::Refresh => true,
        bcode_tui::NativeSetupOutcome::ExternalApproved(candidate) => {
            // One confirmation: save the recommended model and start chatting.
            super::external_setup::save_recommended_external_model(&candidate)?;
            if !shell.stop_requested() {
                Box::pin(super::super::run_new_session_tui_in_shell(
                    shell,
                    None,
                    bcode_tui::TuiLaunchOptions::default(),
                ))
                .await?;
            }
            false
        }
    };
    if refresh && !shell.stop_requested() {
        let host = super::super::load_cli_plugin_host()?;
        Box::pin(super::route_backend_selection(&host, shell, no_discovery)).await?;
    }
    Ok(true)
}
