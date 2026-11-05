//! Canonical actor-owned managed-tool claim API. No authorization is inferred from claim data.
use crate::actor::managed_tool::{Action, Reply};
use crate::{SessionError, SessionManager};
use bcode_session_models::{
    ExternalHarnessOwner, ManagedToolClaimOutcome, ManagedToolClaimRecord, ManagedToolIdentity,
    ToolInvocationResultRecord,
};
fn invalid() -> SessionError {
    SessionError::EventSerialization("invalid managed tool claim response".into())
}
impl SessionManager {
    async fn managed_tool_operation(
        &self,
        owner: &ExternalHarnessOwner,
        action: Action,
    ) -> Result<Reply, SessionError> {
        let handle = self.session_handle(owner.session_id).await?;
        handle
            .managed_tool_action(owner.clone(), action, self.next_activity_timestamp_ms())
            .await
    }
    /// Atomically retain exact identity under the current durable managed owner.
    /// # Errors
    /// Rejects stale owners, conflicting identities, ordinary ID reuse and unavailable canonical storage.
    pub async fn claim_managed_tool(
        &self,
        identity: ManagedToolIdentity,
    ) -> Result<ManagedToolClaimOutcome, SessionError> {
        let owner = identity.owner.clone();
        match self
            .managed_tool_operation(&owner, Action::Claim(identity))
            .await?
        {
            Reply::Claim(claim) => Ok(claim),
            _ => Err(invalid()),
        }
    }
    /// Inspect bounded canonical claim evidence without native execution or replay.
    /// # Errors
    /// Rejects stale owners, corrupt/missing evidence and unavailable storage.
    pub async fn inspect_managed_tool(
        &self,
        owner: &ExternalHarnessOwner,
        id: String,
    ) -> Result<Option<ManagedToolClaimRecord>, SessionError> {
        match self
            .managed_tool_operation(owner, Action::Inspect(id))
            .await?
        {
            Reply::Record(claim) => Ok(claim),
            _ => Err(invalid()),
        }
    }
    /// Consume a claim once, immediately before ordinary tool transport dispatch.
    /// # Errors
    /// Rejects stale, conflicting, already-started or terminal claims. Never automatically retries.
    pub async fn start_managed_tool(
        &self,
        identity: ManagedToolIdentity,
        claim_sequence: u64,
    ) -> Result<(), SessionError> {
        let owner = identity.owner.clone();
        match self
            .managed_tool_operation(
                &owner,
                Action::Start {
                    identity,
                    claim_sequence,
                },
            )
            .await?
        {
            Reply::Committed => Ok(()),
            _ => Err(invalid()),
        }
    }
    /// Append ordinary canonical result evidence for one exact started claim.
    /// # Errors
    /// Rejects altered identity/results, missing start, stale owners and unavailable storage.
    pub async fn complete_managed_tool(
        &self,
        identity: ManagedToolIdentity,
        claim_sequence: u64,
        result: ToolInvocationResultRecord,
    ) -> Result<(), SessionError> {
        let owner = identity.owner.clone();
        match self
            .managed_tool_operation(
                &owner,
                Action::Complete {
                    identity,
                    claim_sequence,
                    result: Box::new(result),
                },
            )
            .await?
        {
            Reply::Committed => Ok(()),
            _ => Err(invalid()),
        }
    }
}
