//! Explicit-only resume overrides; omitted arguments never select today's defaults.
use clap::Args;

/// Override only the supplied inference settings for unfinished generation work.
#[derive(Clone, Debug, Default, Args)]
pub struct ResumeModelOptions {
    /// Replace the saved configuration-file reference.
    #[arg(long, value_parser = super::parse_config_path)]
    pub model_config: Option<std::path::PathBuf>,
    /// Override the saved model profile.
    #[arg(long)]
    pub profile: Option<String>,
    /// Override the saved model.
    #[arg(long)]
    pub model: Option<String>,
    /// Override the saved provider.
    #[arg(long)]
    pub provider: Option<String>,
    /// Override the saved credential-free endpoint.
    #[arg(long)]
    pub endpoint: Option<String>,
    /// Override the saved authentication profile reference.
    #[arg(long)]
    pub auth_profile: Option<String>,
    /// Override the saved configuration context.
    #[arg(long)]
    pub context: Option<String>,
    /// Override the saved output-token limit.
    #[arg(long, value_parser = clap::value_parser!(u32).range(1..))]
    pub max_output_tokens: Option<u32>,
    /// Override the saved request deadline in seconds.
    #[arg(long, value_parser = super::positive_u64)]
    pub model_timeout_seconds: Option<u64>,
}
impl ResumeModelOptions {
    pub(crate) fn apply(&self, saved: &mut crate::bcode_adapter::BcodeOptions) {
        if let Some(path) = &self.model_config {
            saved.config_path.clone_from(path);
        }
        for (target, value) in [
            (&mut saved.profile, &self.profile),
            (&mut saved.model, &self.model),
            (&mut saved.provider, &self.provider),
            (&mut saved.endpoint, &self.endpoint),
            (&mut saved.auth_profile, &self.auth_profile),
            (&mut saved.context, &self.context),
        ] {
            if value.is_some() {
                target.clone_from(value);
            }
        }
        if let Some(tokens) = self.max_output_tokens {
            saved.max_output_tokens = tokens;
        }
        if let Some(seconds) = self.model_timeout_seconds {
            saved.timeout = std::time::Duration::from_secs(seconds);
        }
    }
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    #[test]
    fn resume_omissions_preserve_every_saved_setting_and_explicit_defaults_override() {
        let parsed = crate::cli::Cli::try_parse_from(["recap", "resume", "saved"]).unwrap();
        let Some(crate::cli::Command::Resume(value)) = parsed.command else {
            panic!("resume")
        };
        let mut saved =
            crate::application::model_options(&crate::cli::ModelOptions::defaults_for_resume());
        saved.config_path = "/original/provider.toml".into();
        saved.profile = Some("original".into());
        saved.max_output_tokens = 10000;
        saved.timeout = std::time::Duration::from_secs(300);
        let original = saved.clone();
        value.model.apply(&mut saved);
        assert_eq!(original, saved);
        let parsed = crate::cli::Cli::try_parse_from([
            "recap",
            "resume",
            "saved",
            "--max-output-tokens",
            "8192",
            "--model-timeout-seconds",
            "180",
            "--model",
            "new-model",
        ])
        .unwrap();
        let Some(crate::cli::Command::Resume(value)) = parsed.command else {
            panic!("resume")
        };
        value.model.apply(&mut saved);
        assert_eq!(saved.model.as_deref(), Some("new-model"));
        assert_eq!(saved.max_output_tokens, 8192);
        assert_eq!(saved.timeout.as_secs(), 180);
        assert_eq!(saved.profile, original.profile);
        assert_eq!(saved.config_path, original.config_path);
    }
}
