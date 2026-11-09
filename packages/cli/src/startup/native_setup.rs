//! Native setup is shared with the in-session execution picker.

pub(super) async fn run(
    shell: &mut bcode_tui::TerminalShell,
    no_discovery: bool,
) -> Result<bool, super::CliError> {
    let refresh = bcode_tui::run_native_setup_in_shell(shell, no_discovery).await?;
    if refresh && !shell.stop_requested() {
        let host = super::super::load_cli_plugin_host()?;
        Box::pin(super::route_backend_selection(&host, shell, no_discovery)).await?;
    }
    Ok(true)
}
