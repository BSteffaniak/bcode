//! Typed application handlers. Transport identity and authorization are supplied by
//! the application, never deserialized from client request data.
use super::{
    ExecutionWatchError, ExecutionWatchGrant, ExecutionWatchRegistry, ExecutionWatchStatus,
    Ordering, XrayHub, authorized_watch,
};
use bcode_xray_models::execution_watch as dto;

impl ExecutionWatchRegistry {
    /// Admit an already-authorized exact run/recursive/deep scope for an authenticated
    /// principal. Caller MUST authorize this request's scope before calling, validate
    /// canonical run existence/location, and call before scheduling its first work.
    /// Session access or a parent-session grant is NOT sufficient authorization.
    ///
    /// # Errors
    /// Rejects unsupported/malformed requests, unavailable state and configured capacity.
    #[cfg(test)]
    pub(crate) fn open_authorized(
        &self,
        principal: &str,
        request: dto::OpenExecutionWatchRequest,
    ) -> Result<dto::ExecutionWatchReady, dto::ExecutionWatchError> {
        self.open_authorized_until(principal, request, None)
    }

    pub(crate) fn open_authorized_until(
        &self,
        principal: &str,
        request: dto::OpenExecutionWatchRequest,
        deadline: Option<std::time::Instant>,
    ) -> Result<dto::ExecutionWatchReady, dto::ExecutionWatchError> {
        let mut grant = ExecutionWatchGrant::authorized(
            principal.to_owned(),
            request.scope.root_run_id.clone(),
            request.scope.recursive,
            request.scope.deep_capture,
        )
        .map_err(error)?;
        grant.deadline = deadline;
        if deadline.is_some_and(|deadline| std::time::Instant::now() >= deadline) {
            return Err(dto::ExecutionWatchError::Forbidden);
        }
        let watch_id = self.open(request.schema_version, &grant).map_err(error)?;
        Ok(dto::ExecutionWatchReady {
            schema_version: 1,
            watch_id,
            scope: request.scope,
            root_session_id: None,
            cursor: 0,
            prior_execution_gap: true,
            max_page_items: u32::try_from(self.policy.max_page_events).unwrap_or(u32::MAX),
            max_page_bytes: u64::try_from(self.policy.max_page_bytes).unwrap_or(u64::MAX),
        })
    }

    /// Reserve capture for a server-generated fresh run before canonical publication.
    /// Only the actual admission boundary may call this, after policy authorization.
    /// The caller preserves a stable run request identity; duplicate reservations fail
    /// closed rather than granting a second gap-free admission for the same run.
    #[cfg(test)]
    pub(crate) fn reserve_admission(
        &self,
        principal: &str,
        request: dto::OpenExecutionWatchRequest,
    ) -> Result<dto::ExecutionWatchReady, dto::ExecutionWatchError> {
        self.reserve_admission_until(principal, request, None)
    }

    pub(crate) fn reserve_admission_until(
        &self,
        principal: &str,
        request: dto::OpenExecutionWatchRequest,
        deadline: Option<std::time::Instant>,
    ) -> Result<dto::ExecutionWatchReady, dto::ExecutionWatchError> {
        let mut ready = self.open_authorized_until(principal, request, deadline)?;
        let mut watches = self
            .watches
            .lock()
            .map_err(|_| dto::ExecutionWatchError::Unavailable)?;
        let duplicate = watches.iter().any(|(id, watch)| {
            id != &ready.watch_id && watch.grant.root_run_id == ready.scope.root_run_id
        });
        if duplicate {
            watches.remove(&ready.watch_id);
            return Err(dto::ExecutionWatchError::InvalidRequest);
        }
        let watch = watches
            .get_mut(&ready.watch_id)
            .ok_or(dto::ExecutionWatchError::Unavailable)?;
        watch.prior_execution_gap = false;
        drop(watches);
        ready.prior_execution_gap = false;
        Ok(ready)
    }

    /// Read under the authenticated opening principal. Recheck current application
    /// authorization before every operation and revoke on withdrawal. No replay occurs.
    ///
    /// # Errors
    /// Rejects foreign principals, revoked/unknown watches, stale cursors and bounds.
    pub(crate) fn read_authorized(
        &self,
        principal: &str,
        request: dto::ReadExecutionWatchRequest,
    ) -> Result<dto::ExecutionWatchPage, dto::ExecutionWatchError> {
        let dto::ReadExecutionWatchRequest {
            schema_version,
            watch_id,
            cursor,
            limit,
        } = request;
        version(schema_version)?;
        let grant = self.principal_grant(principal, &watch_id)?;
        let page = self
            .page(
                &grant,
                &watch_id,
                cursor,
                usize::try_from(limit).map_err(|_| dto::ExecutionWatchError::InvalidRequest)?,
            )
            .map_err(error)?;
        drop(watch_id);
        Ok(dto::ExecutionWatchPage {
            schema_version: 1,
            watch_id: page.watch_id,
            next_cursor: page.next_cursor,
            status: status(page.status),
            prior_execution_gap: page.prior_execution_gap,
            contention_gaps: page.contention_gaps,
            omitted_links: page.omitted_links,
            omitted_events: page.omitted_events,
            events: page
                .events
                .into_iter()
                .map(|event| dto::ExecutionWatchEvent {
                    run_id: event.run_id,
                    session_id: event.session_id.to_string(),
                    envelope: event.envelope,
                })
                .collect(),
        })
    }

    /// Freeze future attachments and engine capture, retaining observations and payloads
    /// for drain/export. Cutoffs are stable on retry; pages do not imply disk flush,
    /// transport delivery, canonical settlement or task success. Work is never cancelled.
    ///
    /// # Errors
    /// Rejects unsupported versions, foreign/revoked watches and invalid page bounds.
    pub(crate) fn finalize_authorized(
        &self,
        principal: &str,
        request: dto::FinalizeExecutionWatchRequest,
    ) -> Result<dto::ExecutionWatchFinalEvidence, dto::ExecutionWatchError> {
        version(request.schema_version)?;
        let limit =
            usize::try_from(request.limit).map_err(|_| dto::ExecutionWatchError::InvalidRequest)?;
        let offset = usize::try_from(request.offset)
            .map_err(|_| dto::ExecutionWatchError::InvalidRequest)?;
        if limit == 0 || limit > self.policy.max_page_events {
            return Err(dto::ExecutionWatchError::InvalidRequest);
        }
        let grant = self.principal_grant(principal, &request.watch_id)?;
        let mut watches = self
            .watches
            .lock()
            .map_err(|_| dto::ExecutionWatchError::Unavailable)?;
        let watch = authorized_watch(&mut watches, &grant, &request.watch_id).map_err(error)?;
        if watch.cutoffs.is_none() {
            if offset != 0 {
                return Err(dto::ExecutionWatchError::InvalidRequest);
            }
            if watch.status == ExecutionWatchStatus::Ready {
                watch.status = ExecutionWatchStatus::Finalized;
            }
            // Work is bounded by configurable concurrent subscriptions, not lifetime
            // descendants. Capture producers never call this consumer-side operation.
            watch.cutoffs = Some(
                watch
                    .sessions
                    .iter_mut()
                    .map(|((session, run), entry)| dto::ExecutionWatchCutoff {
                        run_id: run.clone(),
                        session_id: session.to_string(),
                        finalization: entry.observation.finalize().ok(),
                    })
                    .collect(),
            );
        }
        let cutoffs = watch
            .cutoffs
            .as_ref()
            .ok_or(dto::ExecutionWatchError::Unavailable)?;
        if offset > cutoffs.len() {
            return Err(dto::ExecutionWatchError::InvalidRequest);
        }
        let end = offset.saturating_add(limit).min(cutoffs.len());
        let evidence = dto::ExecutionWatchFinalEvidence {
            schema_version: 1,
            watch_id: request.watch_id,
            status: status(watch.status),
            prior_execution_gap: watch.prior_execution_gap,
            contention_gaps: self
                .contention
                .load(Ordering::Relaxed)
                .saturating_sub(watch.contention_start),
            omitted_links: watch.omitted_links,
            omitted_events: watch.omitted_events,
            next_offset: (end < cutoffs.len()).then(|| u64::try_from(end).unwrap_or(u64::MAX)),
            cutoffs: cutoffs[offset..end].to_vec(),
        };
        drop(watches);
        Ok(evidence)
    }

    /// Export retained bytes only under the authenticated opening principal's separate
    /// deep grant. Applications must reauthorize sensitive export before this call.
    ///
    /// # Errors
    /// Rejects unsupported versions, foreign/shallow scopes, identities and invalid ranges.
    pub(crate) fn export_authorized(
        &self,
        hub: &XrayHub,
        principal: &str,
        request: dto::ExportExecutionWatchPayloadRequest,
    ) -> Result<bcode_xray_models::XrayPayloadRange, dto::ExecutionWatchError> {
        let dto::ExportExecutionWatchPayloadRequest {
            schema_version,
            watch_id,
            run_id,
            payload,
        } = request;
        version(schema_version)?;
        let grant = self.principal_grant(principal, &watch_id)?;
        let result = self
            .read_payload(hub, &grant, &watch_id, &run_id, &payload)
            .map_err(error);
        drop(payload);
        result
    }

    /// Delete this principal's observation/captures, never its execution. The application
    /// also uses this operation on authorization withdrawal and authenticated disconnect.
    ///
    /// # Errors
    /// Rejects unsupported versions, foreign/unknown watches and unavailable state.
    pub(crate) fn revoke_authorized(
        &self,
        principal: &str,
        request: dto::RevokeExecutionWatchRequest,
    ) -> Result<dto::ExecutionWatchRevoked, dto::ExecutionWatchError> {
        version(request.schema_version)?;
        let grant = self.principal_grant(principal, &request.watch_id)?;
        self.revoke(&grant).map_err(error)?;
        Ok(dto::ExecutionWatchRevoked {
            schema_version: 1,
            watch_id: request.watch_id,
        })
    }

    /// Revoke all captures for an authenticated disconnected/disabled principal.
    /// This operation has no execution side effects and exports no watch identities.
    ///
    /// # Errors
    /// Returns unavailable if registry state cannot be locked.
    pub fn revoke_principal(&self, principal: &str) -> Result<(), dto::ExecutionWatchError> {
        let mut watches = self
            .watches
            .lock()
            .map_err(|_| dto::ExecutionWatchError::Unavailable)?;
        watches.retain(|_, watch| {
            if watch.grant.principal == principal {
                watch.grant.revoke();
                false
            } else {
                true
            }
        });
        drop(watches);
        Ok(())
    }

    /// Snapshot matching live grants for a bounded pre-dispatch policy recheck.
    /// The configured concurrent-watch bound limits this allocation. No registry lock
    /// is retained across application authorization or asynchronous caller checks.
    pub(crate) fn dispatch_grants(
        &self,
        run_id: &str,
        ancestors: &[String],
    ) -> Result<Vec<ExecutionWatchGrant>, dto::ExecutionWatchError> {
        let Ok(watches) = self.watches.try_lock() else {
            self.contention.fetch_add(1, Ordering::Relaxed);
            return Err(dto::ExecutionWatchError::Unavailable);
        };
        Ok(watches
            .values()
            .filter(|watch| {
                watch.status == ExecutionWatchStatus::Ready
                    && !watch.grant.revoked.load(Ordering::Acquire)
                    && (watch.grant.root_run_id == run_id
                        || (watch.grant.descendants
                            && ancestors.contains(&watch.grant.root_run_id)))
            })
            .map(|watch| watch.grant.clone())
            .collect())
    }

    /// Return only the scope of this authenticated principal's watch for reauthorization.
    /// Watch lookup is not itself permission to inspect execution or export diagnostics.
    ///
    /// # Errors
    /// Rejects foreign/revoked/unknown watches and unavailable registry state.
    pub(crate) fn scope_for_principal(
        &self,
        principal: &str,
        watch_id: &str,
    ) -> Result<dto::ExecutionWatchScope, dto::ExecutionWatchError> {
        let grant = self.principal_grant(principal, watch_id)?;
        Ok(grant.scope())
    }

    fn principal_grant(
        &self,
        principal: &str,
        watch_id: &str,
    ) -> Result<ExecutionWatchGrant, dto::ExecutionWatchError> {
        let watches = self
            .watches
            .lock()
            .map_err(|_| dto::ExecutionWatchError::Unavailable)?;
        let watch = watches
            .get(watch_id)
            .ok_or(dto::ExecutionWatchError::Forbidden)?;
        if watch.grant.principal != principal || watch.grant.revoked.load(Ordering::Acquire) {
            return Err(dto::ExecutionWatchError::Forbidden);
        }
        let grant = watch.grant.clone();
        drop(watches);
        Ok(grant)
    }
}

const fn version(value: u16) -> Result<(), dto::ExecutionWatchError> {
    if value == dto::EXECUTION_WATCH_SCHEMA_VERSION {
        Ok(())
    } else {
        Err(dto::ExecutionWatchError::InvalidRequest)
    }
}

const fn error(value: ExecutionWatchError) -> dto::ExecutionWatchError {
    match value {
        ExecutionWatchError::InvalidRequest => dto::ExecutionWatchError::InvalidRequest,
        ExecutionWatchError::Forbidden => dto::ExecutionWatchError::Forbidden,
        ExecutionWatchError::Capacity => dto::ExecutionWatchError::Capacity,
        ExecutionWatchError::Unavailable => dto::ExecutionWatchError::Unavailable,
    }
}

const fn status(value: ExecutionWatchStatus) -> dto::ExecutionWatchStatus {
    match value {
        ExecutionWatchStatus::Ready => dto::ExecutionWatchStatus::Ready,
        ExecutionWatchStatus::Finalized => dto::ExecutionWatchStatus::Finalized,
        ExecutionWatchStatus::Revoked => dto::ExecutionWatchStatus::Revoked,
        ExecutionWatchStatus::Overloaded => dto::ExecutionWatchStatus::Overloaded,
        ExecutionWatchStatus::OwnershipChanged => dto::ExecutionWatchStatus::OwnershipChanged,
    }
}
