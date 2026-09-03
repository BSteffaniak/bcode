#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Bundled model-scoped prompt profile policy.

use std::collections::BTreeSet;

use bcode_config::{PromptProfileLayerConfig, PromptProfileTextMode};
use bcode_plugin_sdk::prelude::*;
use bcode_prompt_profile::{
    OP_RESOLVE_PROMPT_PROFILE, PROMPT_PROFILE_INTERFACE_ID, PromptProfileResponse,
    ResolvePromptProfileRequest, TextOverrideMode, ToolDescriptionOverride,
};

mod bundled_profiles;

const MAX_PROFILE_TEXT_CHARS: usize = 16_384;

/// Bundled prompt-profile plugin.
#[derive(Default)]
pub struct PromptProfilePlugin;

impl RustPlugin for PromptProfilePlugin {
    fn invoke_service(&mut self, context: NativeServiceContext) -> ServiceResponse {
        if context.request.interface_id != PROMPT_PROFILE_INTERFACE_ID {
            return ServiceResponse::error(
                "unsupported_interface",
                "unsupported prompt-profile service interface",
            );
        }
        if context.request.operation != OP_RESOLVE_PROMPT_PROFILE {
            return ServiceResponse::error(
                "unsupported_operation",
                "unsupported prompt-profile operation",
            );
        }
        let request = match context
            .request
            .payload_json::<ResolvePromptProfileRequest>()
        {
            Ok(request) => request,
            Err(error) => return ServiceResponse::error("invalid_request", error.to_string()),
        };
        ServiceResponse::json(&resolve_profile(&request)).unwrap_or_else(|error| {
            ServiceResponse::error("serialization_failed", error.to_string())
        })
    }
}

fn resolve_profile(request: &ResolvePromptProfileRequest) -> PromptProfileResponse {
    let mut config = request
        .effective_config_toml
        .as_deref()
        .and_then(|contents| bcode_config::decode_effective_config(contents).ok())
        .map(|config| config.prompt_profile)
        .unwrap_or_default();
    let mut response = PromptProfileResponse::default();
    bundled_profiles::install(&mut config, &mut response.diagnostics);

    let target = &request.target;
    let layers = [
        Some(("default".to_string(), &config.default)),
        config
            .provider
            .get(&target.provider_plugin_id)
            .map(|layer| (format!("provider:{}", target.provider_plugin_id), layer)),
        target.family.as_ref().and_then(|family| {
            config
                .family
                .get(family)
                .map(|layer| (format!("family:{family}"), layer))
        }),
        target.catalog_entry_id.as_ref().and_then(|entry| {
            config
                .catalog_entry
                .get(entry)
                .map(|layer| (format!("catalog_entry:{entry}"), layer))
        }),
        config
            .model
            .get(&target.effective_model_id)
            .map(|layer| (format!("model:{}", target.effective_model_id), layer)),
    ];
    let known_tools = request
        .tools
        .iter()
        .map(|tool| tool.name.as_str())
        .collect::<BTreeSet<_>>();
    for (name, layer) in layers.into_iter().flatten() {
        apply_layer(&mut response, layer, &known_tools);
        if layer.system_prompt.is_some() || !layer.tool_description.is_empty() {
            response.applied_layers.push(name);
        }
    }
    response
}

fn apply_layer(
    response: &mut PromptProfileResponse,
    layer: &PromptProfileLayerConfig,
    known_tools: &BTreeSet<&str>,
) {
    if let Some(text) = layer
        .system_prompt
        .as_deref()
        .and_then(bounded_profile_text)
    {
        match layer.system_prompt_mode {
            PromptProfileTextMode::Replace => {
                response.system_prompt_replacement = Some(text);
                response.system_prompt_prepends.clear();
                response.system_prompt_appends.clear();
            }
            PromptProfileTextMode::Append => response.system_prompt_appends.push(text),
            PromptProfileTextMode::Prepend => response.system_prompt_prepends.push(text),
        }
    }
    for (tool, override_config) in &layer.tool_description {
        if !known_tools.contains(tool.as_str()) {
            response
                .diagnostics
                .push(format!("unknown tool in prompt profile: {tool}"));
            continue;
        }
        let Some(text) = bounded_profile_text(&override_config.text) else {
            response
                .diagnostics
                .push(format!("empty tool description override: {tool}"));
            continue;
        };
        response
            .tool_description_overrides
            .entry(tool.clone())
            .or_default()
            .push(ToolDescriptionOverride {
                mode: mode(override_config.mode),
                text,
            });
    }
}

fn bounded_profile_text(text: &str) -> Option<String> {
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    Some(text.chars().take(MAX_PROFILE_TEXT_CHARS).collect())
}

const fn mode(mode: PromptProfileTextMode) -> TextOverrideMode {
    match mode {
        PromptProfileTextMode::Append => TextOverrideMode::Append,
        PromptProfileTextMode::Prepend => TextOverrideMode::Prepend,
        PromptProfileTextMode::Replace => TextOverrideMode::Replace,
    }
}

#[cfg(not(feature = "static-bundled"))]
export_plugin!(PromptProfilePlugin, include_str!("../bcode-plugin.toml"));

#[cfg(feature = "static-bundled")]
#[must_use]
pub fn static_plugin() -> bcode_plugin_sdk::StaticPluginVtable {
    bcode_plugin_sdk::static_plugin_vtable!(
        PromptProfilePlugin,
        include_str!("../bcode-plugin.toml")
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use bcode_prompt_profile::PromptModelTarget;

    fn request(entry: Option<&str>) -> ResolvePromptProfileRequest {
        request_with_config(entry, None)
    }

    fn request_with_config(
        entry: Option<&str>,
        config: Option<&bcode_config::BcodeConfig>,
    ) -> ResolvePromptProfileRequest {
        request_with_family(entry, Some("claude"), config)
    }

    fn request_with_family(
        entry: Option<&str>,
        family: Option<&str>,
        config: Option<&bcode_config::BcodeConfig>,
    ) -> ResolvePromptProfileRequest {
        ResolvePromptProfileRequest {
            session_id: bcode_session_models::SessionId::new(),
            agent_id: "build".to_string(),
            target: PromptModelTarget {
                provider_plugin_id: "bcode.bedrock".to_string(),
                catalog_provider_id: "bedrock".to_string(),
                catalog_entry_id: entry.map(str::to_string),
                requested_model_id: None,
                effective_model_id: "us.anthropic.claude-opus-5-v1:0".to_string(),
                family: family.map(str::to_string),
                api_surface: None,
            },
            tools: vec![bcode_tool::ToolDefinition {
                name: "shell.run".to_string(),
                description: "Run a command".to_string(),
                input_schema: serde_json::json!({}),
            }],
            effective_config_toml: config.map(|config| {
                Box::new(bcode_config::encode_effective_config(config).expect("encode config"))
            }),
        }
    }

    #[test]
    fn all_layers_apply_in_precedence_order_and_preserve_tool_operations() {
        let layer = |text: &str, mode| bcode_config::PromptProfileLayerConfig {
            system_prompt_mode: mode,
            system_prompt: Some(text.to_string()),
            tool_description: std::collections::BTreeMap::from([(
                "shell.run".to_string(),
                bcode_config::ToolDescriptionOverrideConfig {
                    mode,
                    text: text.to_string(),
                },
            )]),
        };
        let mut config = bcode_config::BcodeConfig::default();
        config.prompt_profile.default = layer("default", PromptProfileTextMode::Prepend);
        config.prompt_profile.provider.insert(
            "bcode.bedrock".to_string(),
            layer("provider", PromptProfileTextMode::Append),
        );
        config.prompt_profile.family.insert(
            "claude".to_string(),
            layer("family", PromptProfileTextMode::Replace),
        );
        config.prompt_profile.catalog_entry.insert(
            "anthropic.claude-opus-5".to_string(),
            layer("entry", PromptProfileTextMode::Append),
        );
        config.prompt_profile.model.insert(
            "us.anthropic.claude-opus-5-v1:0".to_string(),
            layer("model", PromptProfileTextMode::Prepend),
        );

        let response = resolve_profile(&request_with_config(
            Some("anthropic.claude-opus-5"),
            Some(&config),
        ));
        assert_eq!(
            response.system_prompt_replacement.as_deref(),
            Some("family")
        );
        assert_eq!(response.system_prompt_prepends, ["model"]);
        assert_eq!(response.system_prompt_appends, ["entry"]);
        assert_eq!(response.applied_layers.len(), 5);
        let operations = &response.tool_description_overrides["shell.run"];
        assert_eq!(operations.len(), 5);
        assert_eq!(operations[0].mode, TextOverrideMode::Prepend);
        assert_eq!(operations[2].mode, TextOverrideMode::Replace);
        assert_eq!(operations[4].mode, TextOverrideMode::Prepend);
    }

    #[test]
    fn unknown_tools_are_diagnostic_only() {
        let mut config = bcode_config::BcodeConfig::default();
        config.prompt_profile.default.tool_description.insert(
            "missing.tool".to_string(),
            bcode_config::ToolDescriptionOverrideConfig {
                mode: PromptProfileTextMode::Replace,
                text: "bad".to_string(),
            },
        );
        let response = resolve_profile(&request_with_family(None, None, Some(&config)));
        assert!(response.tool_description_overrides.is_empty());
        assert_eq!(response.diagnostics.len(), 1);
    }

    #[test]
    fn individual_bundled_profile_can_be_disabled() {
        let mut config = bcode_config::BcodeConfig::default();
        config
            .prompt_profile
            .bundled
            .disabled
            .insert("anthropic-claude-output-preservation".to_string());
        assert_eq!(
            resolve_profile(&request_with_config(
                Some("anthropic.claude-opus-5"),
                Some(&config),
            )),
            PromptProfileResponse::default()
        );
    }

    #[test]
    fn bundled_default_is_scoped_to_claude_family() {
        for entry in [
            "anthropic.claude-opus-5",
            "anthropic.claude-sonnet-4",
            "anthropic.claude-fable-5-1",
        ] {
            let response = resolve_profile(&request(Some(entry)));
            assert_eq!(response.applied_layers, ["family:claude"], "{entry}");
            assert!(response.system_prompt_appends.is_empty(), "{entry}");
            assert!(response.system_prompt_prepends.is_empty(), "{entry}");
            assert_eq!(response.system_prompt_replacement, None, "{entry}");
            let overrides = &response.tool_description_overrides["shell.run"];
            assert_eq!(overrides.len(), 1, "{entry}");
            assert_eq!(overrides[0].mode, TextOverrideMode::Append, "{entry}");
            assert!(overrides[0].text.contains("Do not pipe"), "{entry}");
        }
        assert_eq!(
            resolve_profile(&request_with_family(
                Some("amazon.nova"),
                Some("nova"),
                None
            )),
            PromptProfileResponse::default()
        );
        assert_eq!(
            resolve_profile(&request_with_family(None, None, None)),
            PromptProfileResponse::default()
        );
    }
}
