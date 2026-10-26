//! Private, versioned local control IPC. No transcript or configuration crosses this boundary.
//!
//! Linux uses an owner-only `$XDG_RUNTIME_DIR/vox` directory. Existing endpoints are
//! never removed on startup (including stale endpoints). There is no daemon launch,
//! root-service mode, implicit retry, or transport fallback.
use crate::config::Config;
use crate::contracts::{Action, Status};
use crate::{Error, Result};
use serde::{Deserialize, Serialize};

#[cfg(target_os = "linux")]
mod linux;

#[cfg(any(target_os = "linux", test))]
const VERSION: u32 = 1;
#[cfg(target_os = "linux")]
const MAX_FRAME: usize = 16 * 1024;
#[cfg(any(target_os = "linux", test))]
const MAX_CLIENTS: usize = 4096;

/// A mutation identity bound to one running service instance.
///
/// Obtain with [`operation_id`], retain before sending, and reuse only with the
/// same action after an ambiguous transport failure. Never generate a new ID to
/// retry a possibly delivered mutation. IDs from a restarted service are rejected.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct OperationId {
    instance: String,
    client: String,
    sequence: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "command", rename_all = "snake_case", deny_unknown_fields)]
enum Command {
    Status,
    Action { id: OperationId, action: Action },
    Shutdown { id: OperationId },
}

#[cfg(any(target_os = "linux", test))]
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    version: u32,
    command: Command,
}

#[cfg(any(target_os = "linux", test))]
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Response {
    version: u32,
    instance: String,
    result: std::result::Result<Status, Failure>,
}

#[cfg(any(target_os = "linux", test))]
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum Failure {
    Version,
    Identity,
    Conflict,
    Expired,
    Capacity,
    Busy,
    Cancelled,
    Backend,
}

#[cfg(target_os = "linux")]
impl Failure {
    fn into_error(self) -> Error {
        match self {
            Self::Busy => Error::Busy,
            Self::Cancelled => Error::Cancelled,
            Self::Version => Error::Unsupported("Vox IPC version mismatch".into()),
            Self::Identity => {
                Error::Invalid("mutation identity does not match this service".into())
            }
            Self::Conflict => Error::Invalid("mutation ID was reused with another command".into()),
            Self::Expired => {
                Error::Invalid("mutation ID is older than the retained result; not retried".into())
            }
            Self::Capacity => {
                Error::Backend("IPC mutation-client capacity reached; no action executed".into())
            }
            Self::Backend => {
                Error::Backend("Vox runtime operation failed; consult local configuration".into())
            }
        }
    }
}

#[cfg(any(target_os = "linux", test))]
fn safe_result(result: Result<Status>) -> std::result::Result<Status, Failure> {
    result.map(redact_status).map_err(|error| match error {
        Error::Busy => Failure::Busy,
        Error::Cancelled => Failure::Cancelled,
        _ => Failure::Backend,
    })
}

// Runtime diagnostics are not trusted to be suitable for a public IPC boundary.
#[cfg(any(target_os = "linux", test))]
fn redact_status(status: Status) -> Status {
    match status {
        Status::Failed { .. } => Status::Failed {
            message: "Vox operation failed; check local configuration and capabilities".into(),
        },
        status => status,
    }
}

#[cfg(any(target_os = "linux", test))]
trait Handler {
    fn action(&mut self, action: Action) -> Result<Status>;
    fn status(&self) -> Status;
    fn shutdown(&mut self) -> Result<Status> {
        self.action(Action::Cancel)
    }
}

#[cfg(target_os = "linux")]
struct RuntimeHandler(crate::runtime::Handle);

#[cfg(target_os = "linux")]
impl Handler for RuntimeHandler {
    fn action(&mut self, action: Action) -> Result<Status> {
        self.0.action(action)
    }

    fn status(&self) -> Status {
        self.0.status()
    }

    fn shutdown(&mut self) -> Result<Status> {
        let status = self.0.action(Action::Cancel)?;
        self.0.shutdown();
        Ok(status)
    }
}

#[cfg(any(target_os = "linux", test))]
struct Entry {
    sequence: u64,
    command: Command,
    result: std::result::Result<Status, Failure>,
}

#[cfg(any(target_os = "linux", test))]
struct Dispatcher<H> {
    handler: H,
    instance: String,
    clients: std::collections::BTreeMap<String, Entry>,
    stopping: bool,
    draining: bool,
}

#[cfg(any(target_os = "linux", test))]
impl<H: Handler> Dispatcher<H> {
    const fn new(handler: H, instance: String) -> Self {
        Self {
            handler,
            instance,
            clients: std::collections::BTreeMap::new(),
            stopping: false,
            draining: false,
        }
    }

    fn dispatch(&mut self, request: &Request) -> Response {
        let result = if request.version == VERSION {
            match &request.command {
                Command::Status => Ok(redact_status(self.handler.status())),
                Command::Action { id, .. } | Command::Shutdown { id } => {
                    self.mutate(id, &request.command)
                }
            }
        } else {
            Err(Failure::Version)
        };
        Response {
            version: VERSION,
            instance: self.instance.clone(),
            result,
        }
    }

    fn mutate(
        &mut self,
        id: &OperationId,
        command: &Command,
    ) -> std::result::Result<Status, Failure> {
        if id.instance != self.instance || !valid_token(&id.client) || id.sequence == 0 {
            return Err(Failure::Identity);
        }
        let safe_escape = matches!(
            command,
            Command::Action {
                action: Action::Cancel,
                ..
            } | Command::Shutdown { .. }
        );
        let tracked = self.clients.contains_key(&id.client);
        if let Some(entry) = self.clients.get(&id.client) {
            if id.sequence < entry.sequence {
                return Err(Failure::Expired);
            }
            if id.sequence == entry.sequence {
                return if entry.command == *command {
                    entry.result.clone()
                } else {
                    Err(Failure::Conflict)
                };
            }
        }
        let overflow = !tracked && self.clients.len() >= MAX_CLIENTS;
        if overflow || self.draining || self.stopping {
            if !safe_escape {
                return Err(Failure::Capacity);
            }
            // Emergency cancellation must remain accessible without evicting any
            // high-water mark. Permanently prohibit new work before executing an
            // untracked escape: even a replay cannot cancel a later operation.
            self.draining = true;
            if self.stopping {
                return Ok(redact_status(self.handler.status()));
            }
        }
        let result = match command {
            Command::Action { action, .. } => safe_result(self.handler.action(*action)),
            Command::Shutdown { .. } => {
                let result = safe_result(self.handler.shutdown());
                if result.is_ok() {
                    self.stopping = true;
                }
                result
            }
            Command::Status => unreachable!("only mutations enter deduplication"),
        };
        // Overflow escapes are intentionally idempotent, not deduplicated. No
        // new operation can be admitted after draining begins, even for retained
        // clients. Their existing replay/conflict checks still run above.
        if overflow {
            return result;
        }
        // The serialized server records even failed operations before replying.
        self.clients.insert(
            id.client.clone(),
            Entry {
                sequence: id.sequence,
                command: command.clone(),
                result: result.clone(),
            },
        );
        result
    }
}

#[cfg(any(target_os = "linux", test))]
fn valid_token(token: &str) -> bool {
    token.len() == 32 && token.bytes().all(|byte| byte.is_ascii_hexdigit())
}

/// Serve the standalone runtime in this process until an explicit shutdown.
///
/// # Errors
/// Fails on insecure/missing runtime directories, root execution, an existing
/// socket, unsupported platforms, runtime startup failure, or listener failure.
/// It never removes a pre-existing socket or starts another process.
pub fn serve(config: Config) -> Result<()> {
    #[cfg(target_os = "linux")]
    {
        linux::serve(config)
    }
    #[cfg(not(target_os = "linux"))]
    {
        drop(config);
        Err(unsupported())
    }
}

/// Send one action, once. Transport failure may mean it already executed.
///
/// # Errors
/// Returns transport, version, identity, local random-source, sequence-exhaustion,
/// unsupported-platform, or redacted runtime errors. For callers requiring retry
/// recovery, retain an [`OperationId`] and use [`action_with_id`].
pub fn action(action: Action) -> Result<Status> {
    action_with_id(operation_id()?, action)
}

/// Allocate a mutation ID for the current service without changing runtime state.
///
/// # Errors
/// Returns connection, ownership, timeout, framing, version, unsupported-platform,
/// local random-source, client-lock, or sequence-exhaustion errors.
pub fn operation_id() -> Result<OperationId> {
    #[cfg(target_os = "linux")]
    {
        linux::operation_id()
    }
    #[cfg(not(target_os = "linux"))]
    {
        Err(unsupported())
    }
}

/// Submit or recover the latest mutation for this client and service instance.
///
/// The latest result is cached; older sequences are rejected without execution.
/// Capacity is bounded and never evicts identities. At saturation, a fresh Cancel
/// or Shutdown remains available as an idempotent emergency escape and permanently
/// drains the service: new Start/Stop/Toggle commands require a service restart.
/// These overflow escapes are not result-cached; retries may repeat cancellation,
/// but cannot affect later work because draining prohibits admitting any new work.
/// Retained identities still enforce replay/conflict checks before escape handling.
/// Restarted services reject old instance IDs. This is not durable exactly-once
/// delivery across service crashes.
///
/// # Errors
/// Returns transport, ownership, version, unsupported-platform, or redacted runtime
/// errors, conflicting/old or foreign-instance IDs, or identity-capacity errors.
pub fn action_with_id(id: OperationId, action: Action) -> Result<Status> {
    exchange(Command::Action { id, action })
}

/// Obtain a redacted state snapshot; callers may poll for status changes.
///
/// # Errors
/// Returns connection, ownership, timeout, framing, version, or unsupported-platform
/// errors.
pub fn status() -> Result<Status> {
    exchange(Command::Status)
}

/// Cancel current work and stop accepting connections. No process is spawned.
///
/// Requests `runtime::Handle::shutdown` after cancellation. Runtime shutdown does
/// not join a non-cooperative backend; such work may outlive this call.
/// The endpoint is removed only if it still refers to this server's socket.
///
/// At identity saturation this remains available through the draining escape
/// described in [`action_with_id`]; overflow shutdown is not result-cached.
///
/// # Errors
/// Returns transport, ownership, version, unsupported-platform, mutation-identity,
/// capacity, or cancellation errors; failed cancellation leaves the server running.
/// An ambiguous reply must not be retried with a fresh ID.
pub fn shutdown(id: OperationId) -> Result<Status> {
    exchange(Command::Shutdown { id })
}

fn exchange(command: Command) -> Result<Status> {
    #[cfg(target_os = "linux")]
    {
        linux::exchange(command)?
            .result
            .map_err(Failure::into_error)
    }
    #[cfg(not(target_os = "linux"))]
    {
        drop(command);
        Err(unsupported())
    }
}

#[cfg(not(target_os = "linux"))]
fn unsupported() -> Error {
    Error::Unsupported("Vox local IPC is currently supported only on Linux".into())
}

#[cfg(test)]
mod tests;
