//! Bounded native initialize metadata, exact pre-prompt model selection, and mid-session
//! model/permission-mode control between foreground turns.
use crate::{ClaudeClient, ControlRequest, Error, PermissionMode};
use serde_json::Value;
use std::collections::BTreeSet;

/// Native-advertised metadata is not an account-access guarantee.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NativeModel {
    pub value: String,
    pub display_name: String,
    pub description: Option<String>,
}

/// Correlated `set_model` success; effective identity is retained only when reported.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NativeModelSelection {
    pub requested: String,
    pub effective: Option<String>,
}

/// Exact native protocol capability; unknown versions retain strict identity matching.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ModelSelectionContract {
    ExactIdentity,
    AdvertisedAliasResolution,
}
impl ModelSelectionContract {
    pub(crate) fn for_version(version: &str) -> Self {
        // This native stream-json contract reports the resolved next-turn model in
        // the correlated set_model success payload. It is not inference evidence.
        if crate::is_tested_version(version) {
            Self::AdvertisedAliasResolution
        } else {
            Self::ExactIdentity
        }
    }
}

const MAX_MODELS: usize = 128;
const MAX_ID: usize = 256;
const MAX_TEXT: usize = 64 * 1024;

fn valid_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_ID
        && value.trim() == value
        && !value.chars().any(char::is_control)
}

/// Decode a complete initialize model snapshot without assuming account entitlement.
/// # Errors
/// Missing/unknown schema, duplicate keys or excess metadata fail closed (never truncate).
pub fn available_models(initialize: &Value) -> Result<Vec<NativeModel>, Error> {
    let models = initialize
        .get("models")
        .and_then(Value::as_array)
        .ok_or(Error::Protocol)?;
    if models.len() > MAX_MODELS {
        return Err(Error::Capacity);
    }
    let mut unique = BTreeSet::new();
    let mut result = Vec::with_capacity(models.len());
    for model in models {
        let value = model
            .get("value")
            .and_then(Value::as_str)
            .ok_or(Error::Protocol)?;
        let name = model
            .get("displayName")
            .and_then(Value::as_str)
            .ok_or(Error::Protocol)?;
        let description = match model.get("description") {
            None | Some(Value::Null) => None,
            Some(Value::String(value)) if value.len() <= MAX_TEXT => Some(value.clone()),
            _ => return Err(Error::Protocol),
        };
        if !valid_id(value)
            || name.trim().is_empty()
            || name.len() > MAX_TEXT
            || !unique.insert(value)
        {
            return Err(Error::Protocol);
        }
        result.push(NativeModel {
            value: value.into(),
            display_name: name.into(),
            description,
        });
    }
    Ok(result)
}

#[cfg(test)]
fn selection_response(requested: &str, response: &Value) -> Result<NativeModelSelection, Error> {
    selection_response_with_contract(
        requested,
        response,
        ModelSelectionContract::ExactIdentity,
        false,
    )
}

fn selection_response_with_contract(
    requested: &str,
    response: &Value,
    contract: ModelSelectionContract,
    advertised: bool,
) -> Result<NativeModelSelection, Error> {
    if !response.is_object() {
        return Err(Error::Protocol);
    }
    // Native route aliases are simple identifiers; concrete Claude IDs are not aliases.
    // The native success contract, not a guessed alias->version table, owns resolution.
    let native_alias = advertised
        && valid_id(requested)
        && requested.bytes().all(|byte| byte.is_ascii_alphabetic());
    let effective = match response.get("model") {
        None => None,
        Some(Value::String(model)) if model == requested => Some(model.clone()),
        Some(Value::String(model))
            if contract == ModelSelectionContract::AdvertisedAliasResolution
                && native_alias
                && concrete_model_id(model) =>
        {
            Some(model.clone())
        }
        _ => return Err(Error::Protocol),
    };
    Ok(NativeModelSelection {
        requested: requested.into(),
        effective,
    })
}

fn concrete_model_id(model: &str) -> bool {
    valid_id(model)
        && model.starts_with("claude-")
        && model
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
        && model.split('-').all(|part| !part.is_empty())
        && model.bytes().any(|byte| byte.is_ascii_digit())
}

impl ClaudeClient {
    /// Select an exact advertised key on this initialized transport before any user prompt.
    /// The caller must retain exclusive custody until this operation completes. Native
    /// advertised route aliases use only the verified native acknowledgment contract;
    /// no catalog alias resolution, unknown-key fallback, retry or inference is performed.
    /// # Errors
    /// Missing discovery, unknown model, native rejection or uncertain transport outcome.
    /// A failure terminates this transport so it cannot later submit with a native default.
    pub async fn select_model_before_prompt(
        &self,
        initialize: &Value,
        model: &str,
    ) -> Result<NativeModelSelection, Error> {
        let result = async {
            if !self.is_before_first_prompt() {
                return Err(Error::InvalidConfiguration(
                    "model selection requires pre-prompt custody",
                ));
            }
            let models = available_models(initialize)?;
            self.set_advertised_model(&models, model).await
        }
        .await;
        if result.is_err() {
            self.terminate();
        }
        result
    }

    /// Switch the native model for subsequent turns on a live session.
    ///
    /// Admitted only before the first prompt or while the next foreground turn is
    /// admissible (no active turn, unanswered callback or queued send), and only for a key
    /// in the init-advertised `models` snapshot. The acknowledgment is the correlated native
    /// control response; it is not proof that a later turn used the model. A rejected or
    /// ambiguous switch is reported, never retried, and does not terminate the session.
    /// # Errors
    /// `InvalidConfiguration` while a turn is active or for an unadvertised key; native
    /// rejection or a mismatched effective model as `Protocol`; uncertain transport outcome.
    pub async fn select_model(
        &self,
        models: &[NativeModel],
        model: &str,
    ) -> Result<NativeModelSelection, Error> {
        if !self.is_control_ready() {
            return Err(Error::InvalidConfiguration(
                "transport is not control-ready",
            ));
        }
        if !self.is_before_first_prompt() && !self.can_submit_next_turn() {
            return Err(Error::InvalidConfiguration(
                "model switching requires no active foreground turn",
            ));
        }
        self.set_advertised_model(models, model).await
    }

    async fn set_advertised_model(
        &self,
        models: &[NativeModel],
        model: &str,
    ) -> Result<NativeModelSelection, Error> {
        if !valid_id(model) || !models.iter().any(|entry| entry.value == model) {
            return Err(Error::InvalidConfiguration(
                "model is not native-advertised",
            ));
        }
        let response = self
            .request(ControlRequest::SetModel {
                model: model.into(),
            })
            .await?
            .wait()
            .await?;
        selection_response_with_contract(model, &response, self.model_selection_contract(), true)
    }

    /// Change the native permission mode for subsequent native tool use.
    ///
    /// Returns only after the correlated native control response. Host approval policy is
    /// unchanged: native callbacks still surface as Bcode interactions.
    /// # Errors
    /// Uninitialized transport, native rejection (`Protocol`) or uncertain delivery.
    pub async fn set_permission_mode(&self, mode: PermissionMode) -> Result<(), Error> {
        if !self.is_control_ready() {
            return Err(Error::InvalidConfiguration(
                "transport is not control-ready",
            ));
        }
        self.request(ControlRequest::SetPermissionMode { mode })
            .await?
            .wait()
            .await
            .map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn discovery_is_exact_bounded_metadata_not_a_builtin_catalog() {
        assert!(available_models(&json!({})).is_err());
        assert_eq!(
            available_models(&json!({"models": []})).unwrap(),
            Vec::<NativeModel>::new()
        );
        let entry =
            json!({"value":"native-key","displayName":"Native","description":"Metadata only"});
        assert_eq!(
            available_models(&json!({"models":[&entry]})).unwrap()[0].value,
            "native-key"
        );
        assert!(available_models(&json!({"models":[&entry,&entry]})).is_err());
        assert!(available_models(&json!({"models":vec![entry;MAX_MODELS + 1]})).is_err());
    }

    #[test]
    fn verified_native_contract_resolves_advertised_alias_without_mapping_table() {
        fn resolve(
            requested: &str,
            response: &Value,
            advertised: bool,
        ) -> Result<NativeModelSelection, Error> {
            selection_response_with_contract(
                requested,
                response,
                ModelSelectionContract::AdvertisedAliasResolution,
                advertised,
            )
        }
        let selected = resolve("haiku", &json!({"model":"claude-haiku-5-5"}), true).unwrap();
        assert_eq!(selected.requested, "haiku");
        assert_eq!(selected.effective.as_deref(), Some("claude-haiku-5-5"));
        // No hardcoded alias/version lookup: a new native resolution remains native-owned.
        assert_eq!(
            resolve("haiku", &json!({"model":"claude-haiku-9-1-20300101"}), true)
                .unwrap()
                .effective
                .as_deref(),
            Some("claude-haiku-9-1-20300101")
        );
        assert!(resolve("haiku", &json!({"model":"claude-haiku-5-5"}), false).is_err());
        assert!(
            selection_response_with_contract(
                "haiku",
                &json!({"model":"claude-haiku-5-5"}),
                ModelSelectionContract::ExactIdentity,
                true
            )
            .is_err()
        );
        assert!(
            resolve(
                "claude-haiku-5-5",
                &json!({"model":"claude-sonnet-4-6"}),
                true
            )
            .is_err()
        );
        assert!(resolve("haiku", &json!({"model":"unrecognized-model"}), true).is_err());
        assert!(resolve("haiku", &json!({"model":"claude-"}), true).is_err());
        assert!(resolve("haiku", &json!({"model":"claude-secret url"}), true).is_err());
        assert_eq!(resolve("haiku", &json!({}), true).unwrap().effective, None);
        assert_eq!(
            ModelSelectionContract::for_version("unknown"),
            ModelSelectionContract::ExactIdentity
        );
        // Untested drift keeps strict identity; only tested versions resolve aliases.
        assert_eq!(
            ModelSelectionContract::for_version("9.9.9 (Claude Code)"),
            ModelSelectionContract::ExactIdentity
        );
        for version in crate::TESTED_VERSIONS {
            assert_eq!(
                ModelSelectionContract::for_version(version),
                ModelSelectionContract::AdvertisedAliasResolution
            );
        }
    }

    #[test]
    fn acknowledgment_does_not_invent_effective_identity() {
        assert_eq!(
            selection_response("exact", &json!({})).unwrap().effective,
            None
        );
        assert_eq!(
            selection_response("exact", &json!({"model":"exact"}))
                .unwrap()
                .effective
                .as_deref(),
            Some("exact")
        );
        assert_eq!(
            selection_response("alias", &json!({})).unwrap().effective,
            None
        );
        // A concrete name without native alias-mapping proof is substitution, not evidence.
        assert!(selection_response("alias", &json!({"model":"alias-20261105"})).is_err());
        assert!(selection_response("exact", &json!({"model":"other"})).is_err());
        assert!(selection_response("exact", &Value::Null).is_err());
    }
}
