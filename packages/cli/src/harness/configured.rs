//! Configured-instance CLI adapter. Discovery is plugin-owned; all effects use managed Start.
use crate::CliError;
use bcode_client::ManagedSessionClient;
use bcode_harness_models::{
    HARNESS_SCHEMA_VERSION, HarnessInstance, HarnessOpenRequest, HarnessRequest, HarnessRequestKind,
};
use bcode_ipc::harness::{
    MANAGED_SESSION_INTERFACE_ID, MANAGED_SESSION_VERSION, ManagedSessionCall,
    ManagedSessionOperation, ManagedSessionRequest,
};
use bcode_session_models::SessionId;
use clap::Args;
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, path::PathBuf};

const CONFIGURATION_INTERFACE: &str = "bcode.harness.configuration/v1";
const MAX_RESPONSE_BYTES: usize = 1024 * 1024;

/// Explicit configured selection and retained open identity; no native launch or auth arguments.
#[derive(Debug, Args)]
pub struct OpenOptions {
    /// Exact configured instance reference; never falls back to the default.
    #[arg(
        long,
        required_unless_present = "default_instance",
        conflicts_with = "default_instance"
    )]
    instance: Option<String>,
    /// Explicitly select the configured default, failing if none is configured.
    #[arg(long, conflicts_with = "instance")]
    default_instance: bool,
    /// Existing working directory; defaults to this process's current directory.
    #[arg(long)]
    cwd: Option<PathBuf>,
    /// Ordinary parent association only; grants no assistance or control authority.
    #[arg(long)]
    parent: Option<SessionId>,
    /// Retained Bcode session identity; generated when omitted. Reuse after lost acknowledgement.
    #[arg(long, requires = "open_id")]
    session_id: Option<SessionId>,
    /// Retained open mutation identity; generated when omitted. Reuse only with identical intent.
    #[arg(long, requires = "session_id")]
    open_id: Option<String>,
}

// Wire DTOs belong to the independently versioned service, not a concrete plugin dependency.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum Selection {
    Default {},
    Instance { instance_id: String },
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PreparedInstance {
    selection: Selection,
    instance: HarnessInstance,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Catalog {
    default_instance: Option<String>,
    instances: BTreeMap<String, HarnessInstance>,
}

fn invalid(message: &str) -> CliError {
    CliError::InvalidArguments(message.into())
}

impl OpenOptions {
    fn selection(&self) -> Result<Selection, CliError> {
        match (&self.instance, self.default_instance) {
            (Some(instance_id), false) if !instance_id.trim().is_empty() => {
                Ok(Selection::Instance {
                    instance_id: instance_id.clone(),
                })
            }
            (None, true) => Ok(Selection::Default {}),
            _ => Err(invalid(
                "choose exactly one of --instance or --default-instance",
            )),
        }
    }

    fn build_call(&self, instance: HarnessInstance) -> Result<ManagedSessionCall, CliError> {
        let session_id = self.session_id.unwrap_or_default();
        if self.parent == Some(session_id) {
            return Err(invalid("managed session cannot be its own parent"));
        }
        let cwd = match &self.cwd {
            Some(path) => path.clone(),
            None => std::env::current_dir()
                .map_err(|_| invalid("current working directory is unavailable"))?,
        };
        let cwd = std::fs::canonicalize(cwd)
            .map_err(|_| invalid("managed working directory is unavailable"))?;
        if !cwd.is_dir() {
            return Err(invalid(
                "managed working directory must be an existing directory",
            ));
        }
        let working_directory = cwd
            .to_str()
            .ok_or_else(|| invalid("managed working directory must be valid UTF-8"))?
            .to_owned();
        let open_id = self
            .open_id
            .clone()
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        let call = ManagedSessionCall {
            session_id: Some(session_id),
            harness: HarnessRequest {
                schema_version: HARNESS_SCHEMA_VERSION,
                request_id: open_id.clone(),
                instance,
                kind: HarnessRequestKind::Open(HarnessOpenRequest {
                    open_id,
                    session_id: session_id.to_string(),
                    generation: 1,
                    working_directory,
                    resume: None,
                    parent_assistance: None,
                }),
            },
            parent_session_id: self.parent.map(|id| id.to_string()),
            assistance_destination_session_id: None,
        };
        // Validate through the same typed envelope used by Start --request.
        envelope(&call)?
            .decode_call()
            .map_err(|_| invalid("invalid managed open request"))?;
        Ok(call)
    }
}

fn envelope(call: &ManagedSessionCall) -> Result<ManagedSessionRequest, CliError> {
    Ok(ManagedSessionRequest {
        schema_version: MANAGED_SESSION_VERSION,
        operation: ManagedSessionOperation::Start,
        request_json: serde_json::to_string(call)?,
    })
}

async fn configuration<T: serde::de::DeserializeOwned>(
    client: &ManagedSessionClient,
    operation: &str,
    payload: serde_json::Value,
) -> Result<T, CliError> {
    let response = client
        .call_plugin_service(
            CONFIGURATION_INTERFACE.into(),
            operation.into(),
            serde_json::to_vec(&payload)?,
        )
        .await?;
    if response.error.is_some() || response.payload.len() > MAX_RESPONSE_BYTES {
        return Err(invalid(
            "configured managed instances unavailable; inspect enabled harness configuration",
        ));
    }
    serde_json::from_slice(&response.payload)
        .map_err(|_| invalid("invalid managed configuration response"))
}

pub(super) async fn list() -> Result<(), CliError> {
    let catalog: Catalog = configuration(
        &ManagedSessionClient::default_endpoint(),
        "catalog",
        serde_json::json!({}),
    )
    .await?;
    for (reference, instance) in &catalog.instances {
        if reference != &instance.instance_id || instance.validate().is_err() {
            return Err(invalid("invalid managed instance catalog"));
        }
    }
    if catalog
        .default_instance
        .as_ref()
        .is_some_and(|id| !catalog.instances.contains_key(id))
    {
        return Err(invalid("invalid managed instance default"));
    }
    println!("{}", serde_json::to_string_pretty(&catalog)?);
    Ok(())
}

fn verify_prepared(selection: &Selection, prepared: &PreparedInstance) -> Result<(), CliError> {
    if &prepared.selection != selection || prepared.instance.validate().is_err() {
        return Err(invalid("managed instance preparation mismatch"));
    }
    if let Selection::Instance { instance_id } = selection
        && instance_id != &prepared.instance.instance_id
    {
        return Err(invalid("managed instance reference mismatch"));
    }
    Ok(())
}

pub(super) async fn open(options: OpenOptions) -> Result<(), CliError> {
    let selection = options.selection()?;
    let client = ManagedSessionClient::default_endpoint();
    let prepared: PreparedInstance = configuration(
        &client,
        "prepare",
        serde_json::json!({"selection": selection}),
    )
    .await?;
    verify_prepared(&selection, &prepared)?;
    let call = options.build_call(prepared.instance)?;
    let request = envelope(&call)?;
    request
        .validate_envelope()
        .map_err(|_| invalid("invalid managed open envelope"))?;
    // Publish exact retry input before any native mutation. If acknowledgement is lost, retain
    // this object for Start --request/reconciliation; never generate another session implicitly.
    eprintln!("Retain this exact managed Start request before retrying an uncertain open:");
    eprintln!("{}", serde_json::to_string(&call)?);
    let response = client
        .call_plugin_service(
            MANAGED_SESSION_INTERFACE_ID.into(),
            ManagedSessionOperation::Start.as_str().into(),
            serde_json::to_vec(&request)?,
        )
        .await?;
    if response.error.is_some() || response.payload.len() > MAX_RESPONSE_BYTES {
        return Err(invalid(
            "managed open outcome unavailable; retain the emitted request and reconcile before retrying",
        ));
    }
    let evidence: serde_json::Value = serde_json::from_slice(&response.payload)
        .map_err(|_| invalid("invalid managed open response; retain request and reconcile"))?;
    // Preserve the entire response, including prepared/confirmed distinctions and failures.
    println!(
        "{}",
        serde_json::to_string_pretty(&serde_json::json!({"request": call, "response": evidence}))?
    );
    Ok(())
}

#[cfg(test)]
mod tests;
