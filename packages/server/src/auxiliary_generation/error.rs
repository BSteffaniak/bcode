//! Typed failures for auxiliary inference. Provider and service causes remain intact until presentation.

use bcode_model::ProviderError;

#[cfg(test)]
#[path = "error_tests.rs"]
mod tests;

fn sanitize(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::String(text) => {
            *text = bcode_model_provider_runtime::sanitize_provider_diagnostic(text);
        }
        serde_json::Value::Object(fields) => {
            for value in fields.values_mut() {
                sanitize(value);
            }
        }
        serde_json::Value::Array(values) => {
            for value in values {
                sanitize(value);
            }
        }
        _ => {}
    }
}

#[derive(Debug, thiserror::Error)]
pub(super) enum GenerationError {
    #[error("{0}")]
    Host(String),
    #[error("{}", super::super::provider_error_message(.0))]
    Provider(Box<ProviderError>),
    #[error("{}", bcode_model_provider_runtime::sanitize_provider_diagnostic(&.0.to_string()))]
    Service(#[from] bcode_plugin::PluginServiceCallError),
    #[error("{}", bcode_model_provider_runtime::sanitize_provider_diagnostic(&.0.to_string()))]
    Invocation(#[from] bcode_plugin::PluginLoadError),
    #[error("{primary}; additionally: {secondary}")]
    Secondary {
        primary: Box<Self>,
        secondary: Box<Self>,
    },
}

impl From<String> for GenerationError {
    fn from(message: String) -> Self {
        Self::Host(message)
    }
}

impl From<&str> for GenerationError {
    fn from(message: &str) -> Self {
        Self::Host(message.into())
    }
}

impl GenerationError {
    pub(super) fn diagnostic(&self) -> serde_json::Value {
        use serde_json::json;
        let mut value = match self {
            Self::Provider(error) => {
                let mut error = *error.clone();
                // Provider-native response text is not a public diagnostic contract.
                error.provider_message = None;
                json!({"kind": "provider", "error": error})
            }
            Self::Service(bcode_plugin::PluginServiceCallError::Service { code, message }) => {
                json!({"kind": "service", "code": code, "message": message})
            }
            Self::Secondary { primary, secondary } => {
                json!({"primary": primary.diagnostic(), "secondary": secondary.diagnostic()})
            }
            _ => json!({"kind": "host", "message": self.to_string()}),
        };
        sanitize(&mut value);
        value
    }

    pub(super) fn secondary(self, error: Self) -> Self {
        Self::Secondary {
            primary: Box::new(self),
            secondary: Box::new(error),
        }
    }
}
