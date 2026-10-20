//! Dynamic controller routing and host-owned ephemeral lifecycle.
//!
//! Only the plugin interprets requests and controller state. The owning application
//! must authorize each operation and consume terminal effects canonically.

use std::collections::BTreeMap;
use std::time::Duration;

use bcode_plugin_models::interaction_controller::{
    INTERACTION_CONTROLLER_INTERFACE, INTERACTION_CONTROLLER_VERSION,
    InteractionControllerDiscovery, InteractionControllerDiscoveryRequest,
    InteractionControllerInput, InteractionControllerRequest, InteractionControllerRoute,
    InteractionControllerSnapshot, InteractionControllerTransition, InteractionOutput,
    MAX_CONTROLLER_BYTES, MAX_CONTROLLER_ROUTES,
};
use serde::Serialize;
use serde_json::Value;

use crate::{PluginInvocationScope, PluginRuntimeHost};

const CONTROLLER_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_HOST_CONTROLLERS: usize = 128;

pub use bcode_plugin_models::interaction_controller::InteractionControllerError;

struct SizeBudget(usize);
impl std::io::Write for SizeBudget {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0 = self
            .0
            .checked_sub(bytes.len())
            .ok_or_else(|| std::io::Error::other("interaction controller size limit exceeded"))?;
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Validate encoded size without allocating a second serialized payload.
///
/// # Errors
/// Returns `LimitExceeded` for serialization failure or encoded size overflow.
pub fn validate_controller_size(value: &impl Serialize) -> Result<(), InteractionControllerError> {
    serde_json::to_writer(SizeBudget(MAX_CONTROLLER_BYTES), value)
        .map_err(|_| InteractionControllerError::LimitExceeded)
}

fn validate_route(route: &InteractionControllerRoute, producer: &str) -> bool {
    route.producer_id == producer
        && !producer.trim().is_empty()
        && !route.exchange_schema.trim().is_empty()
        && !route.interaction_kind.trim().is_empty()
        && route.min_schema_version > 0
        && route.min_schema_version <= route.max_schema_version
        && validate_controller_size(route).is_ok()
}

/// Discover only the exchange's exact loaded producer, never a global plugin sweep.
///
/// # Errors
/// Rejects unavailable producers, unknown versions, malformed/conflicting routes and bounds.
pub async fn discover_controller(
    plugins: &PluginRuntimeHost,
    producer: &str,
    schema: &str,
    schema_version: u32,
    scope: PluginInvocationScope,
    cancellation: &bcode_plugin_sdk::ServiceCancellation,
) -> Result<InteractionControllerRoute, InteractionControllerError> {
    if !plugins
        .registry()
        .service_registry()
        .providers_for(INTERACTION_CONTROLLER_INTERFACE)
        .is_some_and(|providers| providers.contains(producer))
    {
        return Err(InteractionControllerError::Unsupported);
    }
    let discovery: InteractionControllerDiscovery = plugins
        .invoke_service_json_scoped_cancellable(
            producer,
            INTERACTION_CONTROLLER_INTERFACE,
            "discover",
            &InteractionControllerDiscoveryRequest {
                version: INTERACTION_CONTROLLER_VERSION,
            },
            scope,
            CONTROLLER_TIMEOUT,
            cancellation,
        )
        .await
        .map_err(|_| InteractionControllerError::Unavailable)?;
    select_controller_route(&discovery, producer, schema, schema_version)
}

fn select_controller_route(
    discovery: &InteractionControllerDiscovery,
    producer: &str,
    schema: &str,
    version: u32,
) -> Result<InteractionControllerRoute, InteractionControllerError> {
    validate_controller_size(discovery)?;
    if discovery.version != INTERACTION_CONTROLLER_VERSION
        || discovery.routes.len() > MAX_CONTROLLER_ROUTES
        || discovery
            .routes
            .iter()
            .any(|route| !validate_route(route, producer))
    {
        return Err(InteractionControllerError::InvalidContract);
    }
    let mut matching = discovery
        .routes
        .iter()
        .filter(|route| route.supports(producer, schema, version));
    let route = matching
        .next()
        .ok_or(InteractionControllerError::Unsupported)?;
    if matching.next().is_some() {
        return Err(InteractionControllerError::InvalidContract);
    }
    Ok(route.clone())
}

struct ControllerEntry {
    owner: String,
    session_id: String,
    state: Value,
    snapshot: InteractionControllerSnapshot,
    last_input: Option<InteractionControllerInput>,
}

/// Authorized pending exchange facts supplied by the application owner.
#[derive(Clone, Copy)]
pub struct InteractionControllerOpen<'a> {
    /// Verified consumer identity.
    pub owner: &'a str,
    /// Canonical owning session identity.
    pub session_id: &'a str,
    /// Canonical pending exchange identity.
    pub exchange_id: &'a str,
    /// Exact plugin producer identity.
    pub producer: &'a str,
    /// Exact exchange schema.
    pub schema: &'a str,
    /// Exchange schema version.
    pub schema_version: u32,
    /// Opaque request payload.
    pub payload: &'a Value,
}

/// Owned invocation ticket; never confers canonical exchange authority.
pub struct ControllerOpenTicket {
    cancellation: bcode_plugin_sdk::ServiceCancellation,
    generation: u64,
    owner: String,
    session_id: String,
    exchange_id: String,
    producer: String,
    schema: String,
    schema_version: u32,
    payload: Value,
}

/// Initialization either returns existing semantics or reserves a lifecycle ticket.
pub enum ControllerOpenPreparation {
    /// Existing controller for the same owner and pinned exchange.
    Existing(InteractionControllerSnapshot),
    /// Plugin invocation must run outside the lifecycle lock.
    Invoke(ControllerOpenTicket),
}

/// Owned revision-fenced input ticket, valid only while its lifecycle remains live.
pub struct ControllerInputTicket {
    cancellation: bcode_plugin_sdk::ServiceCancellation,
    generation: u64,
    owner: String,
    session_id: String,
    input: InteractionControllerInput,
    request: InteractionControllerRequest,
}

/// Revision admission either returns an exact retry or proposes a plugin call.
pub enum ControllerInputPreparation {
    /// Exact duplicate already admitted.
    Existing(InteractionControllerSnapshot),
    /// Plugin invocation must run outside the lifecycle lock.
    Invoke(ControllerInputTicket),
}

/// Disposable lifecycle with generation fencing. Locks are held only for prepare,
/// commit, and cleanup, never across plugin execution or callbacks.
#[derive(Default)]
pub struct InteractionControllerHost {
    entries: BTreeMap<String, ControllerEntry>,
    openings: BTreeMap<String, (u64, String, String)>,
    generations: BTreeMap<String, u64>,
    next_generation: u64,
    cancellations: BTreeMap<String, bcode_plugin_sdk::ServiceCancellation>,
}

impl std::fmt::Debug for InteractionControllerHost {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("InteractionControllerHost")
            .field("controllers", &self.entries.len())
            .field("initializations", &self.openings.len())
            .finish_non_exhaustive()
    }
}

impl Drop for InteractionControllerHost {
    fn drop(&mut self) {
        for cancellation in self.cancellations.values() {
            cancellation.cancel();
        }
    }
}

impl InteractionControllerHost {
    /// Remove all state and reservations owned by a detached consumer.
    pub fn remove_owner(&mut self, owner: &str) {
        let ids = self
            .entries
            .iter()
            .filter(|(_, entry)| entry.owner == owner)
            .map(|(id, _)| id.clone())
            .chain(
                self.openings
                    .iter()
                    .filter(|(_, (_, candidate, _))| candidate == owner)
                    .map(|(id, _)| id.clone()),
            )
            .collect::<Vec<_>>();
        for id in ids {
            self.remove_exchange(&id);
        }
    }
    /// Remove all state and reservations for a detached/cancelled session.
    pub fn remove_session(&mut self, session_id: &str) {
        let ids = self
            .entries
            .iter()
            .filter(|(_, entry)| entry.session_id == session_id)
            .map(|(id, _)| id.clone())
            .chain(
                self.openings
                    .iter()
                    .filter(|(_, (_, _, candidate))| candidate == session_id)
                    .map(|(id, _)| id.clone()),
            )
            .collect::<Vec<_>>();
        for id in ids {
            self.remove_exchange(&id);
        }
    }
    /// Invalidate every outstanding ticket for one exchange.
    pub fn remove_exchange(&mut self, exchange_id: &str) {
        if let Some(cancellation) = self.cancellations.remove(exchange_id) {
            cancellation.cancel();
        }
        self.entries.remove(exchange_id);
        self.openings.remove(exchange_id);
        self.generations.remove(exchange_id);
    }
    /// Read bounded public semantics for their owner.
    ///
    /// # Errors
    /// Rejects missing state and unauthorized consumers.
    pub fn snapshot(
        &self,
        owner: &str,
        exchange_id: &str,
    ) -> Result<InteractionControllerSnapshot, InteractionControllerError> {
        let entry = self
            .entries
            .get(exchange_id)
            .ok_or(InteractionControllerError::NotPending)?;
        if entry.owner != owner {
            return Err(InteractionControllerError::Unauthorized);
        }
        Ok(entry.snapshot.clone())
    }
    /// Reserve initialization before running any plugin code.
    ///
    /// # Errors
    /// Rejects conflicting owners, concurrent initialization and resource bounds.
    pub fn prepare_open(
        &mut self,
        request: InteractionControllerOpen<'_>,
    ) -> Result<ControllerOpenPreparation, InteractionControllerError> {
        let InteractionControllerOpen {
            owner,
            session_id,
            exchange_id,
            producer,
            schema,
            schema_version,
            payload,
        } = request;
        if let Some(entry) = self.entries.get(exchange_id) {
            if entry.owner != owner
                || entry.session_id != session_id
                || !entry
                    .snapshot
                    .route
                    .supports(producer, schema, schema_version)
                || entry.snapshot.schema_version != schema_version
            {
                return Err(InteractionControllerError::Conflict);
            }
            return Ok(ControllerOpenPreparation::Existing(entry.snapshot.clone()));
        }
        if self.openings.contains_key(exchange_id) {
            return Err(InteractionControllerError::Conflict);
        }
        if self.entries.len() + self.openings.len() >= MAX_HOST_CONTROLLERS {
            return Err(InteractionControllerError::LimitExceeded);
        }
        validate_controller_size(&(owner, session_id, exchange_id, producer, schema, payload))?;
        self.next_generation = self
            .next_generation
            .checked_add(1)
            .ok_or(InteractionControllerError::LimitExceeded)?;
        let generation = self.next_generation;
        let cancellation = bcode_plugin_sdk::ServiceCancellation::default();
        self.cancellations
            .insert(exchange_id.to_owned(), cancellation.clone());
        self.openings.insert(
            exchange_id.to_owned(),
            (generation, owner.to_owned(), session_id.to_owned()),
        );
        self.generations.insert(exchange_id.to_owned(), generation);
        Ok(ControllerOpenPreparation::Invoke(ControllerOpenTicket {
            cancellation,
            generation,
            owner: owner.to_owned(),
            session_id: session_id.to_owned(),
            exchange_id: exchange_id.to_owned(),
            producer: producer.to_owned(),
            schema: schema.to_owned(),
            schema_version,
            payload: payload.clone(),
        }))
    }
    /// Commit an initialized controller only if cleanup has not invalidated its ticket.
    ///
    /// # Errors
    /// Rejects stale lifecycle tickets and malformed initialization effects.
    pub fn commit_open(
        &mut self,
        ticket: ControllerOpenTicket,
        result: Result<
            (InteractionControllerRoute, InteractionControllerTransition),
            InteractionControllerError,
        >,
    ) -> Result<InteractionControllerSnapshot, InteractionControllerError> {
        if self.generations.get(&ticket.exchange_id) != Some(&ticket.generation)
            || !self.openings.contains_key(&ticket.exchange_id)
        {
            return Err(InteractionControllerError::NotPending);
        }
        self.openings.remove(&ticket.exchange_id);
        let (route, transition) = match result {
            Ok(result) => result,
            Err(error) => {
                self.remove_exchange(&ticket.exchange_id);
                return Err(error);
            }
        };
        let validation = validate_transition(&route, &transition).and(
            if matches!(
                transition.output,
                InteractionOutput::None | InteractionOutput::Redraw
            ) {
                Ok(())
            } else {
                Err(InteractionControllerError::InvalidContract)
            },
        );
        if let Err(error) = validation {
            self.remove_exchange(&ticket.exchange_id);
            return Err(error);
        }
        let snapshot = InteractionControllerSnapshot {
            version: INTERACTION_CONTROLLER_VERSION,
            exchange_id: ticket.exchange_id.clone(),
            route,
            schema_version: ticket.schema_version,
            revision: 0,
            snapshot: transition.snapshot,
            output: transition.output,
        };
        if let Err(error) = validate_controller_size(&snapshot) {
            self.remove_exchange(&ticket.exchange_id);
            return Err(error);
        }
        self.entries.insert(
            ticket.exchange_id,
            ControllerEntry {
                owner: ticket.owner,
                session_id: ticket.session_id,
                state: transition.state,
                snapshot: snapshot.clone(),
                last_input: None,
            },
        );
        Ok(snapshot)
    }
    /// Prepare revision-checked input without invoking plugin code.
    ///
    /// # Errors
    /// Rejects future contracts, unauthorized consumers and stale/conflicting revisions.
    pub fn prepare_input(
        &self,
        owner: &str,
        input: &InteractionControllerInput,
    ) -> Result<ControllerInputPreparation, InteractionControllerError> {
        validate_controller_size(input)?;
        if input.version != INTERACTION_CONTROLLER_VERSION {
            return Err(InteractionControllerError::InvalidContract);
        }
        let entry = self
            .entries
            .get(&input.exchange_id)
            .ok_or(InteractionControllerError::NotPending)?;
        validate_input_admission(entry, owner, input)?;
        if entry.last_input.as_ref() == Some(input) {
            return Ok(ControllerInputPreparation::Existing(entry.snapshot.clone()));
        }
        let generation = *self
            .generations
            .get(&input.exchange_id)
            .ok_or(InteractionControllerError::NotPending)?;
        Ok(ControllerInputPreparation::Invoke(ControllerInputTicket {
            cancellation: self
                .cancellations
                .get(&input.exchange_id)
                .cloned()
                .ok_or(InteractionControllerError::NotPending)?,
            generation,
            owner: owner.to_owned(),
            session_id: entry.session_id.clone(),
            input: input.clone(),
            request: InteractionControllerRequest::Input {
                version: INTERACTION_CONTROLLER_VERSION,
                route: entry.snapshot.route.clone(),
                state: entry.state.clone(),
                input: input.input.clone(),
            },
        }))
    }
    /// Commit only while the original lifecycle and expected revision remain live.
    ///
    /// # Errors
    /// Rejects cleanup-fenced results, conflicting duplicates and malformed transitions.
    pub fn commit_input(
        &mut self,
        ticket: &ControllerInputTicket,
        transition: InteractionControllerTransition,
    ) -> Result<InteractionControllerSnapshot, InteractionControllerError> {
        if self.generations.get(&ticket.input.exchange_id) != Some(&ticket.generation) {
            return Err(InteractionControllerError::NotPending);
        }
        let entry = self
            .entries
            .get_mut(&ticket.input.exchange_id)
            .ok_or(InteractionControllerError::NotPending)?;
        validate_input_admission(entry, &ticket.owner, &ticket.input)?;
        if entry.last_input.as_ref() != Some(&ticket.input) {
            apply_transition(entry, &ticket.input, transition)?;
        }
        Ok(entry.snapshot.clone())
    }
}

impl ControllerOpenTicket {
    /// Invoke discovery and initialization outside every host lifecycle lock.
    ///
    /// # Errors
    /// Rejects unsupported discovery, malformed contracts, timeouts and unavailable plugins.
    pub async fn invoke(
        &self,
        plugins: &PluginRuntimeHost,
    ) -> Result<
        (InteractionControllerRoute, InteractionControllerTransition),
        InteractionControllerError,
    > {
        let scope = PluginInvocationScope::session(&self.session_id).with_client_id(&self.owner);
        let route = discover_controller(
            plugins,
            &self.producer,
            &self.schema,
            self.schema_version,
            scope.clone(),
            &self.cancellation,
        )
        .await?;
        let transition = invoke_transition(
            plugins,
            &self.producer,
            &InteractionControllerRequest::Open {
                version: INTERACTION_CONTROLLER_VERSION,
                route: route.clone(),
                schema_version: self.schema_version,
                payload: self.payload.clone(),
            },
            scope,
            &self.cancellation,
        )
        .await?;
        Ok((route, transition))
    }
}
impl ControllerInputTicket {
    /// Invoke a stateless plugin transition outside every host lifecycle lock.
    ///
    /// # Errors
    /// Rejects malformed contracts, timeouts and unavailable plugins.
    pub async fn invoke(
        &self,
        plugins: &PluginRuntimeHost,
    ) -> Result<InteractionControllerTransition, InteractionControllerError> {
        let InteractionControllerRequest::Input { route, .. } = &self.request else {
            return Err(InteractionControllerError::InvalidContract);
        };
        invoke_transition(
            plugins,
            &route.producer_id,
            &self.request,
            PluginInvocationScope::session(&self.session_id).with_client_id(&self.owner),
            &self.cancellation,
        )
        .await
    }
}

fn validate_input_admission(
    entry: &ControllerEntry,
    owner: &str,
    input: &InteractionControllerInput,
) -> Result<(), InteractionControllerError> {
    if entry.owner != owner {
        return Err(InteractionControllerError::Unauthorized);
    }
    if entry.last_input.as_ref() == Some(input) {
        return Ok(());
    }
    if entry.snapshot.revision != input.expected_revision
        || matches!(
            entry.snapshot.output,
            InteractionOutput::Submitted { .. } | InteractionOutput::Cancelled
        )
    {
        return Err(InteractionControllerError::Conflict);
    }
    Ok(())
}

fn apply_transition(
    entry: &mut ControllerEntry,
    input: &InteractionControllerInput,
    transition: InteractionControllerTransition,
) -> Result<(), InteractionControllerError> {
    validate_transition(&entry.snapshot.route, &transition)?;
    let revision = entry
        .snapshot
        .revision
        .checked_add(1)
        .ok_or(InteractionControllerError::LimitExceeded)?;
    let mut snapshot = entry.snapshot.clone();
    snapshot.revision = revision;
    snapshot.snapshot = transition.snapshot;
    snapshot.output = transition.output;
    validate_controller_size(&snapshot)?;
    entry.snapshot = snapshot;
    entry.state = transition.state;
    entry.last_input = Some(input.clone());
    Ok(())
}

fn validate_transition(
    route: &InteractionControllerRoute,
    transition: &InteractionControllerTransition,
) -> Result<(), InteractionControllerError> {
    validate_controller_size(transition)?;
    if transition.version != INTERACTION_CONTROLLER_VERSION
        || transition.interaction_kind != route.interaction_kind
    {
        return Err(InteractionControllerError::InvalidContract);
    }
    Ok(())
}

async fn invoke_transition(
    plugins: &PluginRuntimeHost,
    producer: &str,
    request: &InteractionControllerRequest,
    scope: PluginInvocationScope,
    cancellation: &bcode_plugin_sdk::ServiceCancellation,
) -> Result<InteractionControllerTransition, InteractionControllerError> {
    validate_controller_size(request)?;
    plugins
        .invoke_service_json_scoped_cancellable(
            producer,
            INTERACTION_CONTROLLER_INTERFACE,
            "transition",
            request,
            scope,
            CONTROLLER_TIMEOUT,
            cancellation,
        )
        .await
        .map_err(|_| InteractionControllerError::Unavailable)
}

#[cfg(test)]
mod tests {
    use super::*;
    use bcode_plugin_models::interaction_controller::InteractionInput;
    use serde_json::json;

    fn route() -> InteractionControllerRoute {
        InteractionControllerRoute {
            producer_id: "producer".into(),
            exchange_schema: "schema".into(),
            min_schema_version: 1,
            max_schema_version: 2,
            interaction_kind: "kind".into(),
        }
    }
    fn entry() -> ControllerEntry {
        ControllerEntry {
            owner: "owner".into(),
            session_id: "session".into(),
            state: json!({}),
            last_input: None,
            snapshot: InteractionControllerSnapshot {
                version: 1,
                exchange_id: "exchange".into(),
                route: route(),
                schema_version: 1,
                revision: 0,
                snapshot: json!({}),
                output: InteractionOutput::None,
            },
        }
    }
    fn input() -> InteractionControllerInput {
        InteractionControllerInput {
            version: 1,
            exchange_id: "exchange".into(),
            expected_revision: 0,
            input: InteractionInput::Submit,
        }
    }
    #[test]
    fn discovery_fails_closed_on_future_spoofed_conflicting_and_out_of_range_routes() {
        let mut discovery = InteractionControllerDiscovery {
            version: 1,
            routes: vec![route()],
        };
        assert!(select_controller_route(&discovery, "producer", "schema", 2).is_ok());
        assert_eq!(
            select_controller_route(&discovery, "producer", "schema", 3),
            Err(InteractionControllerError::Unsupported)
        );
        discovery.version = 2;
        assert_eq!(
            select_controller_route(&discovery, "producer", "schema", 1),
            Err(InteractionControllerError::InvalidContract)
        );
        discovery.version = 1;
        assert_eq!(
            select_controller_route(&discovery, "other", "schema", 1),
            Err(InteractionControllerError::InvalidContract)
        );
        discovery.routes.push(route());
        assert_eq!(
            select_controller_route(&discovery, "producer", "schema", 1),
            Err(InteractionControllerError::InvalidContract)
        );
    }
    #[test]
    fn terminal_effect_is_stable_and_conflicting_duplicate_does_not_reopen() {
        let mut entry = entry();
        let input = input();
        apply_transition(
            &mut entry,
            &input,
            InteractionControllerTransition {
                version: 1,
                interaction_kind: "kind".into(),
                state: json!({}),
                snapshot: json!({"done": true}),
                output: InteractionOutput::Cancelled,
            },
        )
        .unwrap();
        assert_eq!(validate_input_admission(&entry, "owner", &input), Ok(()));
        let mut conflict = input.clone();
        conflict.input = InteractionInput::Cancel;
        assert_eq!(
            validate_input_admission(&entry, "owner", &conflict),
            Err(InteractionControllerError::Conflict)
        );
        conflict.expected_revision = 1;
        assert_eq!(
            validate_input_admission(&entry, "owner", &conflict),
            Err(InteractionControllerError::Conflict)
        );
        assert_eq!(
            validate_input_admission(&entry, "other", &input),
            Err(InteractionControllerError::Unauthorized)
        );
        assert_eq!(entry.snapshot.output, InteractionOutput::Cancelled);
    }
    #[test]
    fn cleanup_drops_all_ephemeral_state_without_plugin_calls() {
        let mut host = InteractionControllerHost::default();
        host.entries.insert("exchange".into(), entry());
        host.remove_owner("other");
        assert!(host.snapshot("owner", "exchange").is_ok());
        host.remove_owner("owner");
        assert_eq!(
            host.snapshot("owner", "exchange"),
            Err(InteractionControllerError::NotPending)
        );
        host.entries.insert("exchange".into(), entry());
        host.remove_session("session");
        assert!(host.entries.is_empty());
        host.entries.insert("exchange".into(), entry());
        host.remove_exchange("exchange");
        assert!(host.entries.is_empty());
    }
    #[test]
    fn cancellation_fences_open_and_input_tickets_even_after_same_exchange_reopens() {
        let mut host = InteractionControllerHost::default();
        let payload = json!({});
        let facts = || InteractionControllerOpen {
            owner: "owner",
            session_id: "session",
            exchange_id: "exchange",
            producer: "producer",
            schema: "schema",
            schema_version: 1,
            payload: &payload,
        };
        let ControllerOpenPreparation::Invoke(stale_open) = host.prepare_open(facts()).unwrap()
        else {
            panic!("expected ticket");
        };
        host.remove_exchange("exchange");
        let ControllerOpenPreparation::Invoke(live_open) = host.prepare_open(facts()).unwrap()
        else {
            panic!("expected ticket");
        };
        let transition = || InteractionControllerTransition {
            version: 1,
            interaction_kind: "kind".into(),
            state: json!({}),
            snapshot: json!({}),
            output: InteractionOutput::None,
        };
        assert_eq!(
            host.commit_open(stale_open, Ok((route(), transition()))),
            Err(InteractionControllerError::NotPending)
        );
        host.commit_open(live_open, Ok((route(), transition())))
            .unwrap();
        let ControllerInputPreparation::Invoke(stale_input) =
            host.prepare_input("owner", &input()).unwrap()
        else {
            panic!("expected ticket");
        };
        let cancellation = stale_input.cancellation.clone();
        host.remove_owner("owner");
        assert!(cancellation.is_cancelled());
        assert_eq!(
            host.commit_input(&stale_input, transition()),
            Err(InteractionControllerError::NotPending)
        );
        assert!(host.entries.is_empty());
        assert!(host.openings.is_empty());
        assert!(host.generations.is_empty());
    }
    #[test]
    fn concurrent_revision_tickets_admit_identical_retry_but_reject_conflicting_input() {
        let mut host = InteractionControllerHost::default();
        host.entries.insert("exchange".into(), entry());
        host.generations.insert("exchange".into(), 1);
        host.cancellations.insert(
            "exchange".into(),
            bcode_plugin_sdk::ServiceCancellation::default(),
        );
        let ControllerInputPreparation::Invoke(first) =
            host.prepare_input("owner", &input()).unwrap()
        else {
            panic!("expected ticket");
        };
        let ControllerInputPreparation::Invoke(duplicate) =
            host.prepare_input("owner", &input()).unwrap()
        else {
            panic!("expected ticket");
        };
        let mut conflict = input();
        conflict.input = InteractionInput::Cancel;
        let ControllerInputPreparation::Invoke(conflicting) =
            host.prepare_input("owner", &conflict).unwrap()
        else {
            panic!("expected ticket");
        };
        let transition = || InteractionControllerTransition {
            version: 1,
            interaction_kind: "kind".into(),
            state: json!({}),
            snapshot: json!({}),
            output: InteractionOutput::Redraw,
        };
        let committed = host.commit_input(&first, transition()).unwrap();
        assert_eq!(host.commit_input(&duplicate, transition()), Ok(committed));
        assert_eq!(
            host.commit_input(&conflicting, transition()),
            Err(InteractionControllerError::Conflict)
        );
        assert_eq!(host.snapshot("owner", "exchange").unwrap().revision, 1);
    }
    #[test]
    fn host_drop_signals_in_flight_plugin_work_without_retaining_ephemeral_state() {
        let mut host = InteractionControllerHost::default();
        let payload = json!({});
        let ControllerOpenPreparation::Invoke(ticket) = host
            .prepare_open(InteractionControllerOpen {
                owner: "owner",
                session_id: "session",
                exchange_id: "exchange",
                producer: "producer",
                schema: "schema",
                schema_version: 1,
                payload: &payload,
            })
            .unwrap()
        else {
            panic!("expected ticket");
        };
        let cancellation = ticket.cancellation;
        assert!(!cancellation.is_cancelled());
        drop(host);
        assert!(cancellation.is_cancelled());
    }
    #[test]
    fn future_input_and_oversized_requests_fail_before_revision_or_state_changes() {
        let mut host = InteractionControllerHost::default();
        host.entries.insert("exchange".into(), entry());
        host.generations.insert("exchange".into(), 1);
        host.cancellations.insert(
            "exchange".into(),
            bcode_plugin_sdk::ServiceCancellation::default(),
        );
        let mut future = input();
        future.version = 2;
        assert!(matches!(
            host.prepare_input("owner", &future),
            Err(InteractionControllerError::InvalidContract)
        ));
        future.version = 1;
        future.input = InteractionInput::Change {
            control_id: bcode_plugin_models::interaction_controller::InteractionControlId::new(
                "control",
            ),
            value: bcode_plugin_models::interaction_controller::InteractionValue::String(
                "x".repeat(MAX_CONTROLLER_BYTES),
            ),
        };
        assert!(matches!(
            host.prepare_input("owner", &future),
            Err(InteractionControllerError::LimitExceeded)
        ));
        assert_eq!(host.snapshot("owner", "exchange").unwrap().revision, 0);
    }
    #[test]
    fn encoded_bounds_count_escape_expansion_and_transition_validation_is_atomic() {
        assert!(validate_controller_size(&"x".repeat(MAX_CONTROLLER_BYTES - 2)).is_ok());
        assert_eq!(
            validate_controller_size(&"x".repeat(MAX_CONTROLLER_BYTES - 1)),
            Err(InteractionControllerError::LimitExceeded)
        );
        assert_eq!(
            validate_controller_size(&"\n".repeat(MAX_CONTROLLER_BYTES / 2)),
            Err(InteractionControllerError::LimitExceeded)
        );
        let mut entry = entry();
        assert_eq!(
            apply_transition(
                &mut entry,
                &input(),
                InteractionControllerTransition {
                    version: 2,
                    interaction_kind: "kind".into(),
                    state: json!({}),
                    snapshot: json!({}),
                    output: InteractionOutput::Cancelled
                }
            ),
            Err(InteractionControllerError::InvalidContract)
        );
        assert_eq!(entry.snapshot.revision, 0);
        assert_eq!(entry.snapshot.output, InteractionOutput::None);
    }
}
