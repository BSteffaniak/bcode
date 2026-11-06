//! Explicit, nonsecret execution defaults; catalog resolution remains model-catalog owned.

use bcode_model::execution::{ExecutionModelSelection, ExecutionRoute};
use serde::{Deserialize, Deserializer, Serialize};

/// Optional execution override, independent of ordinary provider and authentication defaults.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ExecutionConfig {
    /// Exact user-selected route/model/account. Absence preserves ordinary provider selection.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_selection: Option<ExecutionPreference>,
}

/// A remembered explicit choice, not discovery, readiness, admission, or native acceptance.
///
/// Route is mandatory so persisting a preference cannot silently choose a backend. An absent
/// account reference remains unconstrained; the central catalog must reject ambiguous matches.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ExecutionPreference {
    /// Exact normalized catalog model identity, never an inferred illustrative model name.
    pub model_id: String,
    /// Explicit execution backend.
    pub route: ExecutionRoute,
    /// Optional nonsecret account reference; never a credential or an auth-pool override.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub account_reference: Option<String>,
}

impl ExecutionPreference {
    /// Validate reference syntax without discovering models or selecting a fallback.
    ///
    /// # Errors
    /// Rejects empty, overlong, whitespace-padded, or control-bearing references.
    pub fn validate(&self) -> Result<(), &'static str> {
        fn reference(value: &str) -> bool {
            !value.is_empty()
                && value.len() <= 4096
                && value.trim() == value
                && !value.chars().any(char::is_control)
        }
        let route_valid = match &self.route {
            ExecutionRoute::ModelProvider { provider_plugin_id } => reference(provider_plugin_id),
            ExecutionRoute::ManagedHarness {
                instance_id,
                plugin_id,
            } => reference(instance_id) && reference(plugin_id),
        };
        if !reference(&self.model_id)
            || !route_valid
            || self
                .account_reference
                .as_deref()
                .is_some_and(|value| !reference(value))
        {
            return Err("invalid execution preference reference");
        }
        Ok(())
    }

    /// Pass the exact preference to centralized catalog resolution, without doing resolution.
    #[must_use]
    pub fn selection(&self) -> ExecutionModelSelection {
        ExecutionModelSelection {
            model_id: self.model_id.clone(),
            route: Some(self.route.clone()),
            account_reference: self.account_reference.clone(),
        }
    }
}

impl<'de> Deserialize<'de> for ExecutionPreference {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Fields {
            model_id: String,
            route: ExecutionRoute,
            #[serde(default)]
            account_reference: Option<String>,
        }
        let fields = Fields::deserialize(deserializer)?;
        let preference = Self {
            model_id: fields.model_id,
            route: fields.route,
            account_reference: fields.account_reference,
        };
        preference.validate().map_err(serde::de::Error::custom)?;
        Ok(preference)
    }
}

impl crate::ConfigDocSchema for ExecutionConfig {
    fn section_name() -> &'static str {
        "execution"
    }

    fn section_description() -> &'static str {
        "Explicit execution default; omission retains ordinary provider selection."
    }

    fn default_values() -> std::collections::BTreeMap<String, String> {
        std::collections::BTreeMap::new()
    }

    fn field_docs() -> Vec<crate::FieldDoc> {
        vec![crate::config_field(
            "default_selection",
            "table",
            "Exact model_id, route (kind and backend identity), and optional nonsecret account_reference. Catalog resolution and managed admission are still required; no auth binding is changed.",
        )]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_config_keeps_provider_default_and_has_no_execution_override() {
        let legacy: crate::BcodeConfig =
            toml::from_str("[model]\nmodel_id = 'existing'\n").unwrap();
        assert!(legacy.execution.default_selection.is_none());
        assert_eq!(legacy.model.model_id.as_deref(), Some("existing"));
    }

    #[test]
    fn exact_route_and_account_round_trip_without_resolution() {
        let text = "[execution.default_selection]\nmodel_id = 'discovered-model'\naccount_reference = 'native-account'\n[execution.default_selection.route]\nkind = 'managed_harness'\ninstance_id = 'primary'\nplugin_id = 'example.native'\n";
        let config: crate::BcodeConfig = toml::from_str(text).unwrap();
        let preference = config.execution.default_selection.as_ref().unwrap();
        let selection = preference.selection();
        assert_eq!(selection.model_id, "discovered-model");
        assert_eq!(
            selection.account_reference.as_deref(),
            Some("native-account")
        );
        assert_eq!(selection.route, Some(preference.route.clone()));
        let encoded = toml::to_string(&config.execution).unwrap();
        assert_eq!(
            toml::from_str::<ExecutionConfig>(&encoded).unwrap(),
            config.execution
        );
    }

    #[test]
    fn malformed_or_implicit_routes_fail_closed() {
        for text in [
            "model_id = 'model'",
            "model_id = ' '\nroute = { kind = 'model_provider', provider_plugin_id = 'provider' }",
            "model_id = 'model'\nroute = { kind = 'managed_harness', instance_id = '', plugin_id = 'native' }",
            "model_id = 'model'\nroute = { kind = 'unknown' }",
            "model_id = 'model'\nroute = { kind = 'model_provider', provider_plugin_id = 'provider' }\ncredential = 'not-allowed'",
        ] {
            assert!(toml::from_str::<ExecutionPreference>(text).is_err());
        }
    }
}
