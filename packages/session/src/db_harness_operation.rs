//! Canonical operation projection. All locators point back to a typed canonical event.
use super::{
    Database, SessionDb, SessionDbError, SessionDbResult, SessionEvent, SessionEventKind,
    SessionId, SortDirection, last_sequence, required_non_negative_u64, seq_to_value,
    strict_event_from_row,
};
use bcode_harness_models::{
    HarnessActivity, HarnessAttemptIdentity, HarnessAttention, HarnessCompleteness,
    HarnessConnectivity, HarnessContinuation, HarnessExecutionEvent, HarnessExecutionStatus,
    HarnessInteraction, HarnessObservationKind, HarnessObservationPage, HarnessWorkState,
    MAX_HARNESS_EXECUTION_ITEMS, MAX_HARNESS_OBSERVATIONS, MAX_HARNESS_WIRE_BYTES,
};
use bcode_session_models::external_harness::ExternalHarnessOperationRecord as R;
use bcode_session_models::external_harness::{
    ExternalHarnessOperation, ExternalHarnessOperationRecord, ExternalHarnessOwner,
    ExternalHarnessRequest, ExternalHarnessRequestKind, ExternalHarnessResponse,
    ExternalHarnessResponseKind, ExternalHarnessState,
};
use bcode_session_models::external_harness::{
    HarnessInventoryCommitment, HarnessInventoryCursor, HarnessInventoryEntry,
    HarnessInventoryKind, HarnessInventoryPage, HarnessInventoryScope, HarnessInventorySummary,
};
use sha2::{Digest as _, Sha256};
use std::fmt::Write as _;
use switchy::database::query::FilterableQuery;

// Content-addressed binary Patricia nodes. Branch positions strictly increase, so path
// work is bounded by the 256-bit identity digest, independently of inventory lifetime.
const INVENTORY_NODE_BYTES: u64 = 32 * 1024;
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct InventoryLink {
    hash: String,
    summary: HarnessInventorySummary,
}
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(tag = "node", deny_unknown_fields)]
enum InventoryNode {
    Leaf {
        key: String,
        sequence: u64,
        summary: HarnessInventorySummary,
    },
    Branch {
        bit: u16,
        representative: String,
        left: InventoryLink,
        right: InventoryLink,
    },
    // Private derived representation. Old proof readers reject this closed enum
    // variant; the public canonical commitment remains an opaque root and totals.
    NotificationGateV1 {
        gate: NotificationGate,
    },
}
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct NotificationGate {
    owner: ExternalHarnessOwner,
    origin: HarnessAttemptIdentity,
    source: Option<InventoryLink>,
    reverse: Option<InventoryLink>,
    eligible: Option<InventoryLink>,
    coverage: Option<u64>,
    complete: bool,
}
type InventoryNodes = std::collections::BTreeMap<String, InventoryNode>;

fn inventory_digest(key: &str) -> [u8; 32] {
    Sha256::digest(key.as_bytes()).into()
}
fn inventory_bit(digest: &[u8; 32], bit: u16) -> bool {
    digest[usize::from(bit / 8)] & (0x80 >> (bit % 8)) != 0
}
fn inventory_summary_add(
    a: &HarnessInventorySummary,
    b: &HarnessInventorySummary,
) -> SessionDbResult<HarnessInventorySummary> {
    Ok(HarnessInventorySummary {
        identities: a.identities.checked_add(b.identities).ok_or_else(invalid)?,
        unfinished: a.unfinished.checked_add(b.unfinished).ok_or_else(invalid)?,
        obligations: a
            .obligations
            .checked_add(b.obligations)
            .ok_or_else(invalid)?,
        failed: a.failed.checked_add(b.failed).ok_or_else(invalid)?,
        interrupted: a
            .interrupted
            .checked_add(b.interrupted)
            .ok_or_else(invalid)?,
    })
}
fn inventory_node_summary(node: &InventoryNode) -> SessionDbResult<HarnessInventorySummary> {
    match node {
        InventoryNode::Leaf {
            sequence,
            key,
            summary,
        } => {
            if *sequence > i64::MAX.cast_unsigned()
                || key.len() > bcode_harness_models::MAX_HARNESS_ID_BYTES * 8 + 512
                || summary.identities != 1
                || summary.unfinished > 1
                || summary.obligations > 1
                || summary.failed > 1
                || summary.interrupted > 1
            {
                return Err(invalid());
            }
            Ok(summary.clone())
        }
        InventoryNode::Branch {
            bit, left, right, ..
        } => {
            if *bit >= 256 || left.summary.identities == 0 || right.summary.identities == 0 {
                return Err(invalid());
            }
            inventory_summary_add(&left.summary, &right.summary)
        }
        InventoryNode::NotificationGateV1 { gate } => {
            gate.owner.validate().map_err(SessionDbError::InvalidData)?;
            gate.origin.validate().map_err(|_| invalid())?;
            let mut summary = gate
                .source
                .as_ref()
                .map_or_else(HarnessInventorySummary::default, |link| {
                    link.summary.clone()
                });
            let eligible = gate
                .eligible
                .as_ref()
                .map_or(0, |link| link.summary.identities);
            let reverse = gate
                .reverse
                .as_ref()
                .map_or(0, |link| link.summary.identities);
            if eligible > summary.obligations
                || eligible > reverse
                || (gate.complete && gate.coverage.is_none())
            {
                return Err(evidence_maintenance_required());
            }
            if gate.complete {
                summary.obligations = summary
                    .obligations
                    .checked_sub(eligible)
                    .ok_or_else(evidence_maintenance_required)?;
            }
            Ok(summary)
        }
    }
}
fn inventory_payload_hash(bytes: &[u8]) -> String {
    let mut digest = Sha256::new();
    digest.update(b"bcode.harness.inventory.patricia.v1\0");
    digest.update(bytes);
    digest
        .finalize()
        .iter()
        .fold(String::with_capacity(64), |mut hex, byte| {
            write!(hex, "{byte:02x}").expect("writing to a String cannot fail");
            hex
        })
}
fn inventory_link(node: &InventoryNode) -> SessionDbResult<InventoryLink> {
    let bytes = serde_json::to_vec(node)?;
    if bytes.len() as u64 > INVENTORY_NODE_BYTES {
        return Err(invalid());
    }
    // Distinct domain separator prevents interpreting unrelated content as a proof node.
    Ok(InventoryLink {
        hash: inventory_payload_hash(&bytes),
        summary: inventory_node_summary(node)?,
    })
}
fn inventory_store_node(
    nodes: &mut InventoryNodes,
    node: InventoryNode,
) -> SessionDbResult<InventoryLink> {
    let link = inventory_link(&node)?;
    nodes.insert(link.hash.clone(), node);
    Ok(link)
}
async fn inventory_load_node(
    db: &dyn Database,
    link: &InventoryLink,
    nodes: &InventoryNodes,
) -> SessionDbResult<InventoryNode> {
    if link.hash.len() != 64 || !link.hash.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(invalid());
    }
    let node = if let Some(node) = nodes.get(&link.hash) {
        node.clone()
    } else {
        let name = format!("harness_inventory_node_v1:{}", link.hash);
        let lengths = db
            .select("snapshots")
            .project_as(
                switchy::database::query::byte_length(switchy::database::query::identifier(
                    "payload",
                )),
                "bytes",
            )
            .where_eq("snapshot_name", name.clone())
            .execute_first(db)
            .await?
            .ok_or_else(evidence_maintenance_required)?;
        if required_non_negative_u64(&lengths, "bytes")? > INVENTORY_NODE_BYTES {
            return Err(evidence_maintenance_required());
        }
        let row = db
            .select("snapshots")
            .columns(&["payload", "schema_version"])
            .where_eq("snapshot_name", name)
            .execute_first(db)
            .await?
            .ok_or_else(evidence_maintenance_required)?;
        if required_non_negative_u64(&row, "schema_version")? != 2 {
            return Err(evidence_maintenance_required());
        }
        let payload = super::required_string(&row, "payload")?;
        // Recheck after fetch as well as probing before selection. Authenticate raw bytes
        // before allocating decoded strings/branches; only canonical encoding is stored.
        if payload.len() as u64 > INVENTORY_NODE_BYTES
            || inventory_payload_hash(payload.as_bytes()) != link.hash
        {
            return Err(evidence_maintenance_required());
        }
        serde_json::from_str::<InventoryNode>(&payload)
            .map_err(|_| evidence_maintenance_required())?
    };
    let actual = inventory_link(&node)?;
    if actual.hash != link.hash || actual.summary != link.summary {
        return Err(evidence_maintenance_required());
    }
    Ok(node)
}

async fn inventory_update(
    db: &dyn Database,
    root: Option<InventoryLink>,
    key: &str,
    value: Option<(u64, HarnessInventorySummary)>,
    nodes: &mut InventoryNodes,
) -> SessionDbResult<Option<InventoryLink>> {
    let digest = inventory_digest(key);
    let Some(mut current) = root else {
        return value
            .map(|(sequence, summary)| {
                inventory_store_node(
                    nodes,
                    InventoryNode::Leaf {
                        key: key.into(),
                        sequence,
                        summary,
                    },
                )
            })
            .transpose();
    };
    let mut path = Vec::new();
    let mut previous_bit = None;
    let leaf_key = loop {
        let node = inventory_load_node(db, &current, nodes).await?;
        match node {
            InventoryNode::NotificationGateV1 { .. } => return Err(evidence_maintenance_required()),
            InventoryNode::Leaf { key, .. } => break key,
            InventoryNode::Branch {
                bit,
                representative,
                left,
                right,
            } => {
                if previous_bit.is_some_and(|prior| bit <= prior) {
                    return Err(invalid());
                }
                previous_bit = Some(bit);
                let go_right = inventory_bit(&digest, bit);
                let next = if go_right {
                    right.clone()
                } else {
                    left.clone()
                };
                path.push((bit, representative, left, right, go_right, current));
                current = next;
            }
        }
    };
    let replacement = if leaf_key == key {
        value
            .map(|(sequence, summary)| {
                inventory_store_node(
                    nodes,
                    InventoryNode::Leaf {
                        key: key.into(),
                        sequence,
                        summary,
                    },
                )
            })
            .transpose()?
    } else if let Some((sequence, summary)) = value {
        let old_digest = inventory_digest(&leaf_key);
        let bit = (0..256_u16)
            .find(|bit| inventory_bit(&digest, *bit) != inventory_bit(&old_digest, *bit))
            .ok_or_else(invalid)?;
        while path.last().is_some_and(|(prior, ..)| *prior > bit) {
            current = path.pop().ok_or_else(invalid)?.5;
        }
        let new = inventory_store_node(
            nodes,
            InventoryNode::Leaf {
                key: key.into(),
                sequence,
                summary,
            },
        )?;
        let (left, right) = if inventory_bit(&digest, bit) {
            (current, new)
        } else {
            (new, current)
        };
        Some(inventory_store_node(
            nodes,
            InventoryNode::Branch {
                bit,
                representative: key.into(),
                left,
                right,
            },
        )?)
    } else {
        return Ok(path
            .first()
            .map_or_else(|| Some(current.clone()), |entry| Some(entry.5.clone())));
    };
    inventory_rebuild_path(nodes, path, replacement)
}

type InventoryPath = Vec<(
    u16,
    String,
    InventoryLink,
    InventoryLink,
    bool,
    InventoryLink,
)>;

fn inventory_rebuild_path(
    nodes: &mut InventoryNodes,
    mut path: InventoryPath,
    mut replacement: Option<InventoryLink>,
) -> SessionDbResult<Option<InventoryLink>> {
    while let Some((bit, representative, left, right, go_right, _)) = path.pop() {
        let sibling = if go_right { left } else { right };
        replacement = Some(if let Some(child) = replacement {
            let (left, right) = if go_right {
                (sibling, child)
            } else {
                (child, sibling)
            };
            inventory_store_node(
                nodes,
                InventoryNode::Branch {
                    bit,
                    representative,
                    left,
                    right,
                },
            )?
        } else {
            sibling
        });
    }
    Ok(replacement)
}

async fn inventory_window(
    db: &dyn Database,
    root: Option<InventoryLink>,
    offset: u64,
    limit: usize,
) -> SessionDbResult<Vec<(String, u64)>> {
    let root = inventory_read_gate(db, root).await?.0;
    let mut stack = root
        .into_iter()
        .map(|link| (link, None, 0_u64))
        .collect::<Vec<_>>();
    let mut result = Vec::new();
    while let Some((link, parent_bit, start)) = stack.pop() {
        if start
            .checked_add(link.summary.identities)
            .ok_or_else(invalid)?
            <= offset
        {
            continue;
        }
        match inventory_load_node(db, &link, &InventoryNodes::new()).await? {
            InventoryNode::NotificationGateV1 { .. } => return Err(evidence_maintenance_required()),
            InventoryNode::Leaf { key, sequence, .. } => {
                result.push((key, sequence));
                if result.len() == limit {
                    break;
                }
            }
            InventoryNode::Branch {
                bit, left, right, ..
            } => {
                if parent_bit.is_some_and(|parent| bit <= parent) {
                    return Err(invalid());
                }
                let right_start = start
                    .checked_add(left.summary.identities)
                    .ok_or_else(invalid)?;
                stack.push((right, Some(bit), right_start));
                stack.push((left, Some(bit), start));
            }
        }
    }
    Ok(result)
}

fn inventory_root_link(
    commitment: &HarnessInventoryCommitment,
) -> SessionDbResult<Option<InventoryLink>> {
    match &commitment.root {
        Some(hash) => Ok(Some(InventoryLink {
            hash: hash.clone(),
            summary: commitment.summary.clone(),
        })),
        None if commitment.summary == HarnessInventorySummary::default() => Ok(None),
        _ => Err(evidence_maintenance_required()),
    }
}
async fn inventory_find_raw(
    db: &dyn Database,
    mut root: Option<InventoryLink>,
    key: &str,
    nodes: &InventoryNodes,
) -> SessionDbResult<Option<(u64, HarnessInventorySummary)>> {
    let digest = inventory_digest(key);
    let mut previous_bit = None;
    while let Some(link) = root {
        match inventory_load_node(db, &link, nodes).await? {
            InventoryNode::NotificationGateV1 { .. } => return Err(evidence_maintenance_required()),
            InventoryNode::Leaf {
                key: retained,
                sequence,
                summary,
            } => {
                return Ok((retained == key).then_some((sequence, summary)));
            }
            InventoryNode::Branch {
                bit, left, right, ..
            } => {
                if previous_bit.is_some_and(|prior| bit <= prior) {
                    return Err(invalid());
                }
                previous_bit = Some(bit);
                root = Some(if inventory_bit(&digest, bit) {
                    right
                } else {
                    left
                });
            }
        }
    }
    Ok(None)
}

fn notification_namespace(owner: &ExternalHarnessOwner, origin: &HarnessAttemptIdentity) -> String {
    execution_namespace(
        owner,
        &format!(
            "inventory:attempt:{}:notification_reverse_v1",
            submission_key(&origin.submission_id, &origin.attempt_id)
        ),
    )
}
fn notification_work_namespace(
    owner: &ExternalHarnessOwner,
    origin: &HarnessAttemptIdentity,
) -> String {
    execution_namespace(
        owner,
        &format!(
            "inventory:attempt:{}:work",
            submission_key(&origin.submission_id, &origin.attempt_id)
        ),
    )
}

// A witness is always checked against canonical keys and its immutable version locator.
// Mutable latest rows are deliberately not used by historical gated proofs.
async fn notification_event(
    db: &dyn Database,
    owner: &ExternalHarnessOwner,
    key: &str,
    sequence: u64,
) -> SessionDbResult<SessionEvent> {
    let version = op_key(
        "inventory_member_v1",
        &submission_key(key, &sequence.to_string()),
    );
    verify_inventory_members(db, &[(version, sequence)]).await?;
    let event = inventory_canonical_event(db, owner.session_id, sequence).await?;
    let op = operation(&event)?;
    if op.owner != *owner || !keys(op).iter().any(|(ns, id)| op_key(ns, id) == key) {
        return Err(evidence_maintenance_required());
    }
    Ok(event)
}

async fn inventory_read_gate(
    db: &dyn Database,
    root: Option<InventoryLink>,
) -> SessionDbResult<(Option<InventoryLink>, Option<NotificationGate>)> {
    let Some(link) = root else {
        return Ok((None, None));
    };
    let nodes = InventoryNodes::new();
    let InventoryNode::NotificationGateV1 { gate } = inventory_load_node(db, &link, &nodes).await?
    else {
        return Ok((Some(link), None));
    };
    // Authenticate the accessed roots even when their effective obligations are zero.
    for child in [&gate.source, &gate.reverse, &gate.eligible]
        .into_iter()
        .flatten()
    {
        match inventory_load_node(db, child, &nodes).await? {
            InventoryNode::NotificationGateV1 { .. } => return Err(evidence_maintenance_required()),
            InventoryNode::Leaf { key, sequence, .. } => {
                notification_event(db, &gate.owner, &key, sequence).await?;
            }
            InventoryNode::Branch { .. } => {}
        }
    }
    if let Some(sequence) = gate.coverage {
        let id = submission_key(&gate.origin.submission_id, &gate.origin.attempt_id);
        let namespace =
            execution_namespace(&gate.owner, &format!("inventory:attempt:{id}:coverage"));
        let event = notification_event(db, &gate.owner, &op_key(&namespace, &id), sequence).await?;
        let R::ObservationPage { page, .. } = &operation(&event)?.record else {
            return Err(evidence_maintenance_required());
        };
        let observation = page
            .observations
            .iter()
            .rev()
            .find(|observation| {
                matches!(
            &observation.kind, HarnessObservationKind::Execution {
                event: HarnessExecutionEvent::Coverage { attempt, .. }
            } if attempt == &gate.origin)
            })
            .ok_or_else(evidence_maintenance_required)?;
        if notification_coverage_complete(page, observation) != gate.complete {
            return Err(evidence_maintenance_required());
        }
    } else if gate.complete {
        return Err(evidence_maintenance_required());
    }
    Ok((gate.source.clone(), Some(gate)))
}
fn notification_coverage_complete(
    page: &HarnessObservationPage,
    observation: &bcode_harness_models::HarnessObservation,
) -> bool {
    page.completeness == HarnessCompleteness::Complete
        && observation.completeness == HarnessCompleteness::Complete
        && matches!(
            &observation.kind,
            HarnessObservationKind::Execution {
                event: HarnessExecutionEvent::Coverage {
                    background: bcode_harness_models::HarnessTrackingCoverage::Complete,
                    continuations: bcode_harness_models::HarnessTrackingCoverage::Complete,
                    ..
                }
            }
        )
}

async fn notification_verify_source(
    db: &dyn Database,
    gate: &NotificationGate,
    key: &str,
    sequence: u64,
) -> SessionDbResult<()> {
    let namespace = notification_work_namespace(&gate.owner, &gate.origin);
    let source = key
        .strip_prefix(&op_key(&namespace, ""))
        .ok_or_else(evidence_maintenance_required)?;
    let reverse_key = op_key(&notification_namespace(&gate.owner, &gate.origin), source);
    let (reverse_sequence, _) = inventory_find_raw(
        db,
        gate.reverse.clone(),
        &reverse_key,
        &InventoryNodes::new(),
    )
    .await?
    .ok_or_else(evidence_maintenance_required)?;
    let event = notification_event(db, &gate.owner, &reverse_key, reverse_sequence).await?;
    let R::ObservationPage { page, .. } = &operation(&event)?.record else {
        return Err(evidence_maintenance_required());
    };
    if page.completeness != HarnessCompleteness::Complete || !page.observations.iter().any(|observation| {
        observation.completeness == HarnessCompleteness::Complete && matches!(
            &observation.kind, HarnessObservationKind::Execution {
                event: HarnessExecutionEvent::Continuation { origin: Some(origin), source_work_id: Some(id), state, .. }
            } if origin == &gate.origin && id == source && state.is_terminal())
    }) {
        return Err(evidence_maintenance_required());
    }
    let source_event = notification_event(db, &gate.owner, key, sequence).await?;
    let observation = execution_observation(&source_event, "work", source)?;
    let R::ObservationPage { page, .. } = &operation(&source_event)?.record else {
        return Err(evidence_maintenance_required());
    };
    if page.completeness != HarnessCompleteness::Complete
        || observation.completeness != HarnessCompleteness::Complete
        || !matches!(&observation.kind, HarnessObservationKind::Execution {
            event: HarnessExecutionEvent::BackgroundWork { origin: Some(origin), state,
                notification: bcode_harness_models::HarnessNotificationState::Observed, .. }
        } if origin == &gate.origin && state.is_terminal())
    {
        return Err(evidence_maintenance_required());
    }
    Ok(())
}

async fn inventory_find(
    db: &dyn Database,
    root: Option<InventoryLink>,
    key: &str,
    nodes: &InventoryNodes,
) -> SessionDbResult<Option<(u64, HarnessInventorySummary)>> {
    // Newly prepared plain trees are not yet persisted; only committed wrappers
    // enter this read path. Projector updates use inventory_find_raw directly.
    if let Some(link) = &root
        && matches!(
            inventory_load_node(db, link, nodes).await?,
            InventoryNode::NotificationGateV1 { .. }
        )
    {
        let (source, gate) = inventory_read_gate(db, root).await?;
        let gate = gate.ok_or_else(evidence_maintenance_required)?;
        let mut found = inventory_find_raw(db, source, key, nodes).await?;
        if let Some((sequence, mut summary)) = found.take() {
            if let Some((eligible_sequence, _)) =
                inventory_find_raw(db, gate.eligible.clone(), key, nodes).await?
            {
                if eligible_sequence != sequence {
                    return Err(evidence_maintenance_required());
                }
                notification_verify_source(db, &gate, key, sequence).await?;
                if gate.complete {
                    summary.obligations = 0;
                }
            }
            found = Some((sequence, summary));
        }
        return Ok(found);
    }
    inventory_find_raw(db, root, key, nodes).await
}

async fn notification_validate_commitment(
    db: &dyn Database,
    owner: &ExternalHarnessOwner,
    namespace: &str,
    commitment: &HarnessInventoryCommitment,
) -> SessionDbResult<()> {
    let (_, gate) = inventory_read_gate(db, inventory_root_link(commitment)?).await?;
    if namespace.contains(":inventory:attempt:") && namespace.ends_with(":work") {
        let gate = gate.ok_or_else(evidence_maintenance_required)?;
        if gate.owner != *owner || notification_work_namespace(owner, &gate.origin) != namespace {
            return Err(evidence_maintenance_required());
        }
    } else if gate.is_some() {
        return Err(evidence_maintenance_required());
    }
    Ok(())
}

async fn inventory_directory(
    db: &dyn Database,
    owner: &ExternalHarnessOwner,
    before: Option<u64>,
) -> SessionDbResult<Option<HarnessInventoryCommitment>> {
    // Anchor discovery in the canonical event-type index, not a deletable latest-marker
    // projection. The directory commits which namespaces exist, including empty scopes.
    let mut query = db
        .select("events")
        .project_as(
            switchy::database::query::max(switchy::database::query::identifier("event_seq")),
            "event_seq",
        )
        .where_eq("event_type", "external_harness_operation");
    if let Some(before) = before {
        query = query.where_lt("event_seq", seq_to_value(before));
    }
    let row = query.execute_first(db).await?.ok_or_else(invalid)?;
    if matches!(
        row.get("event_seq"),
        Some(switchy::database::DatabaseValue::Null)
    ) {
        return Ok(None);
    }
    let sequence = required_non_negative_u64(&row, "event_seq")?;
    let event = inventory_canonical_event(db, owner.session_id, sequence).await?;
    let op = operation(&event)?;
    if op.owner != *owner {
        // Only a released immediately preceding generation can seed an empty new
        // directory. This computes commitments, not authority: transactional recovery
        // validation still compares the exact release and current configuration.
        if matches!(&op.record, R::TransportReleased { .. })
            && op.owner.session_id == owner.session_id
            && op.owner.generation.checked_add(1) == Some(owner.generation)
            && op.owner.daemon_id != owner.daemon_id
        {
            return Ok(None);
        }
        return Err(invalid());
    }
    let namespace = execution_namespace(owner, "inventory_directory_v1");
    let root = op
        .inventory_commitments
        .iter()
        .find(|root| root.snapshot_key == namespace)
        .ok_or_else(evidence_maintenance_required)?;
    if root.revision != sequence {
        return Err(invalid());
    }
    Ok(Some(root.clone()))
}
async fn inventory_current_commitment(
    db: &dyn Database,
    owner: &ExternalHarnessOwner,
    namespace: &str,
    before: Option<u64>,
) -> SessionDbResult<Option<HarnessInventoryCommitment>> {
    let Some(directory) = inventory_directory(db, owner, before).await? else {
        return Ok(None);
    };
    let Some((sequence, _)) = inventory_find(
        db,
        inventory_root_link(&directory)?,
        namespace,
        &InventoryNodes::new(),
    )
    .await?
    else {
        return Ok(None);
    };
    let marker = inventory_canonical_event(db, owner.session_id, sequence).await?;
    let op = operation(&marker)?;
    if op.owner != *owner {
        return Err(invalid());
    }
    let commitment = op
        .inventory_commitments
        .iter()
        .find(|root| root.snapshot_key == namespace)
        .ok_or_else(evidence_maintenance_required)?;
    if commitment.revision != marker.sequence {
        return Err(invalid());
    }
    let length = db
        .select("snapshots")
        .project_as(
            switchy::database::query::byte_length(switchy::database::query::identifier("payload")),
            "bytes",
        )
        .where_eq("snapshot_name", namespace)
        .execute_first(db)
        .await?
        .ok_or_else(evidence_maintenance_required)?;
    if required_non_negative_u64(&length, "bytes")? > 16 * 1024 {
        return Err(evidence_maintenance_required());
    }
    let summary = commitment.clone();
    let row = db
        .select("snapshots")
        .columns(&["payload", "schema_version", "last_event_seq"])
        .where_eq("snapshot_name", namespace)
        .execute_first(db)
        .await?
        .ok_or_else(evidence_maintenance_required)?;
    let payload = super::required_string(&row, "payload")?;
    if payload.len() > 16 * 1024
        || required_non_negative_u64(&row, "schema_version")? != 2
        || required_non_negative_u64(&row, "last_event_seq")? != summary.revision
        || serde_json::from_str::<HarnessInventoryCommitment>(&payload)? != summary
    {
        return Err(evidence_maintenance_required());
    }
    Ok(Some(summary))
}

fn inventory_leaf_summary(
    op: &ExternalHarnessOperation,
    key: &str,
) -> SessionDbResult<HarnessInventorySummary> {
    let mut result = HarnessInventorySummary {
        identities: 1,
        ..Default::default()
    };
    if let R::ObservationPage { page, .. } = &op.record {
        let native_namespace = execution_namespace(&op.owner, "inventory:native_reference_v1");
        if page.observations.iter().any(|observation| {
            native_observation_references(observation)
                .iter()
                .any(|reference| op_key(&native_namespace, &native_reference_id(reference)) == key)
        }) {
            // Reference directory entries are locators, not lifecycle summary assertions.
            return Ok(result);
        }
        for observation in page.observations.iter().rev() {
            match &observation.kind {
                HarnessObservationKind::Execution { event } => {
                    let (kind, id) = execution_identity(event);
                    let namespace = execution_namespace(
                        &op.owner,
                        &format!("inventory:{}:{kind}", execution_scope(event)),
                    );
                    if op_key(&namespace, &id) != key {
                        continue;
                    }
                    if let Some(state) = execution_state(event) {
                        result.unfinished = u64::from(!state.is_terminal());
                        result.failed = u64::from(state == HarnessWorkState::Failed);
                        result.interrupted = u64::from(state == HarnessWorkState::Interrupted);
                    }
                    if observation.completeness != HarnessCompleteness::Complete
                        && matches!(
                            event,
                            HarnessExecutionEvent::BackgroundWork { .. }
                                | HarnessExecutionEvent::Continuation { .. }
                        )
                    {
                        result.unfinished = 1;
                    }
                    match event {
                        HarnessExecutionEvent::BackgroundWork { notification, .. } => {
                            result.obligations = u64::from(!matches!(
                                notification,
                                bcode_harness_models::HarnessNotificationState::NotExpected
                            ));
                        }
                        HarnessExecutionEvent::SubmissionTerminal { disposition, .. } => {
                            result.failed = u64::from(
                                *disposition
                                    == bcode_harness_models::HarnessTerminalDisposition::Failed,
                            );
                            result.interrupted = u64::from(*disposition == bcode_harness_models::HarnessTerminalDisposition::Interrupted);
                        }
                        _ => {}
                    }
                    return Ok(result);
                }
                HarnessObservationKind::Interaction { interaction }
                    if key
                        == op_key(
                            &execution_namespace(&op.owner, "inventory:unknown:interaction"),
                            &interaction.request_id,
                        ) =>
                {
                    result.obligations = 1;
                    return Ok(result);
                }
                _ => {}
            }
        }
    }
    Err(invalid())
}

pub(super) async fn prepare_current_inventory_operation(
    db: &dyn Database,
    op: &mut ExternalHarnessOperation,
    revision: u64,
) -> SessionDbResult<()> {
    op.validate().map_err(SessionDbError::InvalidData)?;
    let computed = prepare_inventory_commitments(db, op, revision).await?.0;
    if !op.inventory_commitments.is_empty() && op.inventory_commitments != computed {
        return Err(invalid());
    }
    op.inventory_commitments = computed;
    op.validate().map_err(SessionDbError::InvalidData)
}

// Only exact-attempt work roots acquire this representation. All roster-sized
// work is confined to explicit migration replay; routine updates touch page IDs.
async fn notification_prepare_gate(
    db: &dyn Database,
    op: &ExternalHarnessOperation,
    revision: u64,
    namespace: &str,
    root: Option<InventoryLink>,
    nodes: &InventoryNodes,
) -> SessionDbResult<(Option<InventoryLink>, Option<NotificationGate>)> {
    if let Some(link) = &root
        && let InventoryNode::NotificationGateV1 { gate } =
            inventory_load_node(db, link, nodes).await?
    {
        if gate.owner != op.owner
            || notification_work_namespace(&op.owner, &gate.origin) != namespace
        {
            return Err(evidence_maintenance_required());
        }
        let (source, _) = inventory_read_gate(db, root).await?;
        return Ok((source, Some(gate)));
    }
    let origin = match &op.record {
        R::ObservationPage { page, .. } => page.observations.iter().find_map(|observation| {
            let HarnessObservationKind::Execution { event } = &observation.kind else {
                return None;
            };
            execution_origin(event)
                .filter(|origin| notification_work_namespace(&op.owner, origin) == namespace)
                .cloned()
        }),
        R::DeliveryClaimed { request } => match &request.kind {
            ExternalHarnessRequestKind::Submit { submission, .. } => Some(HarnessAttemptIdentity {
                submission_id: submission.submission_id.clone(),
                attempt_id: submission.attempt_id.clone(),
            })
            .filter(|origin| notification_work_namespace(&op.owner, origin) == namespace),
            _ => None,
        },
        _ => None,
    };
    let Some(origin) = origin else {
        return Ok((root, None));
    };
    // Existing plain exact-work roots cannot be lazily upgraded on append.
    if inventory_current_commitment(db, &op.owner, namespace, Some(revision))
        .await?
        .is_some()
    {
        return Err(evidence_maintenance_required());
    }
    Ok((
        root.clone(),
        Some(NotificationGate {
            owner: op.owner.clone(),
            origin,
            source: root,
            reverse: None,
            eligible: None,
            coverage: None,
            complete: false,
        }),
    ))
}

async fn notification_update_eligibility(
    db: &dyn Database,
    op: &ExternalHarnessOperation,
    revision: u64,
    gate: &mut NotificationGate,
    id: &str,
    nodes: &mut InventoryNodes,
) -> SessionDbResult<()> {
    let work_namespace = notification_work_namespace(&op.owner, &gate.origin);
    let reverse_namespace = notification_namespace(&op.owner, &gate.origin);
    let key = op_key(&work_namespace, id);
    let source = inventory_find_raw(db, gate.source.clone(), &key, nodes).await?;
    let reverse_key = op_key(&reverse_namespace, id);
    let witness = inventory_find_raw(db, gate.reverse.clone(), &reverse_key, nodes).await?;
    let mut eligible = None;
    if let (Some((sequence, summary)), Some((witness_sequence, _))) = (source, witness) {
        if witness_sequence != revision {
            notification_event(db, &op.owner, &reverse_key, witness_sequence).await?;
        }
        let canonical;
        let source_op = if sequence == revision {
            op
        } else {
            canonical = notification_event(db, &op.owner, &key, sequence).await?;
            operation(&canonical)?
        };
        let R::ObservationPage {
            page: source_page, ..
        } = &source_op.record
        else {
            return Err(evidence_maintenance_required());
        };
        let observation = source_page
            .observations
            .iter()
            .rev()
            .find(|observation| {
                matches!(
            &observation.kind, HarnessObservationKind::Execution {
                event: HarnessExecutionEvent::BackgroundWork { work_id, .. }
            } if work_id == id)
            })
            .ok_or_else(evidence_maintenance_required)?;
        if summary.obligations == 1
            && source_page.completeness == HarnessCompleteness::Complete
            && observation.completeness == HarnessCompleteness::Complete
            && matches!(&observation.kind, HarnessObservationKind::Execution {
                event: HarnessExecutionEvent::BackgroundWork { origin: Some(origin), state,
                    notification: bcode_harness_models::HarnessNotificationState::Observed, .. }
            } if origin == &gate.origin && state.is_terminal())
        {
            eligible = Some((
                sequence,
                HarnessInventorySummary {
                    identities: 1,
                    ..Default::default()
                },
            ));
        }
    }
    gate.eligible = inventory_update(db, gate.eligible.take(), &key, eligible, nodes).await?;
    Ok(())
}

async fn notification_update_gate(
    db: &dyn Database,
    op: &ExternalHarnessOperation,
    revision: u64,
    mut gate: NotificationGate,
    source_root: Option<InventoryLink>,
    nodes: &mut InventoryNodes,
) -> SessionDbResult<Option<InventoryLink>> {
    gate.source = source_root;
    if let R::ObservationPage { page, .. } = &op.record {
        let mut affected = std::collections::BTreeSet::new();
        let reverse_namespace = notification_namespace(&op.owner, &gate.origin);
        for observation in &page.observations {
            let HarnessObservationKind::Execution { event } = &observation.kind else {
                continue;
            };
            if execution_origin(event) != Some(&gate.origin) {
                continue;
            }
            match event {
                HarnessExecutionEvent::Coverage { .. } => {
                    gate.coverage = Some(revision);
                    gate.complete = notification_coverage_complete(page, observation);
                }
                HarnessExecutionEvent::BackgroundWork { work_id, .. } => {
                    affected.insert(work_id.clone());
                }
                HarnessExecutionEvent::Continuation {
                    source_work_id: Some(id),
                    state,
                    ..
                } if state.is_terminal()
                    && page.completeness == HarnessCompleteness::Complete
                    && observation.completeness == HarnessCompleteness::Complete =>
                {
                    // One complete immutable terminal witness is sufficient. Keep it
                    // across later incomplete notifications and deduplicate by source.
                    let key = op_key(&reverse_namespace, id);
                    gate.reverse = inventory_update(
                        db,
                        gate.reverse,
                        &key,
                        Some((
                            revision,
                            HarnessInventorySummary {
                                identities: 1,
                                ..Default::default()
                            },
                        )),
                        nodes,
                    )
                    .await?;
                    affected.insert(id.clone());
                }
                _ => {}
            }
        }
        for id in affected {
            notification_update_eligibility(db, op, revision, &mut gate, &id, nodes).await?;
        }
    }
    inventory_store_node(nodes, InventoryNode::NotificationGateV1 { gate }).map(Some)
}

async fn inventory_prepare_namespace(
    db: &dyn Database,
    op: &ExternalHarnessOperation,
    revision: u64,
    namespace: &str,
    has_directory: bool,
    nodes: &InventoryNodes,
) -> SessionDbResult<(Option<HarnessInventoryCommitment>, Option<InventoryLink>)> {
    let prefix = op_key(namespace, "");
    let prior = inventory_current_commitment(db, &op.owner, namespace, Some(revision)).await?;
    // Absence is allowed only for first construction of an exact scope. A missing
    // unknown root after configuration is damage, never an implicit empty inventory.
    if prior.is_none()
        && namespace.contains(":inventory:unknown:")
        && has_directory
        && !matches!(op.record, R::Configured { .. } | R::RecoveryPrepared { .. })
    {
        return Err(evidence_maintenance_required());
    }
    if prior.is_none() {
        let end = format!("{};", prefix.strip_suffix(':').ok_or_else(invalid)?);
        let stray = db
            .select("turn_evidence")
            .columns(&["turn_id"])
            .where_eq("evidence_kind", KIND)
            .where_gte("turn_id", prefix.clone())
            .where_lt("turn_id", end)
            .limit(1)
            .execute_first(db)
            .await?;
        if stray.is_some() {
            return Err(evidence_maintenance_required());
        }
    }
    let root = prior
        .as_ref()
        .map(inventory_root_link)
        .transpose()?
        .flatten();
    if let Some(link) = &root {
        inventory_load_node(db, link, nodes).await?;
    }
    Ok((prior, root))
}

fn inventory_initial_namespaces(
    op: &ExternalHarnessOperation,
    has_directory: bool,
) -> std::collections::BTreeSet<String> {
    let mut namespaces = inventory_namespaces(op);
    if !has_directory {
        for kind in [
            "terminal",
            "coverage",
            "work",
            "continuation",
            "interaction",
        ] {
            namespaces.insert(execution_namespace(
                &op.owner,
                &format!("inventory:unknown:{kind}"),
            ));
        }
        namespaces.insert(execution_namespace(
            &op.owner,
            "inventory:native_reference_v1",
        ));
    }
    namespaces
}

async fn prepare_inventory_commitments(
    db: &dyn Database,
    op: &ExternalHarnessOperation,
    revision: u64,
) -> SessionDbResult<(Vec<HarnessInventoryCommitment>, InventoryNodes)> {
    if revision > i64::MAX.cast_unsigned() {
        return Err(invalid());
    }
    let prior_directory = inventory_directory(db, &op.owner, Some(revision)).await?;
    let mut commitments = Vec::new();
    let mut nodes = InventoryNodes::new();
    let additions = keys(op);
    let removals = replaced_execution_keys(op);
    let namespaces = inventory_initial_namespaces(op, prior_directory.is_some());
    for namespace in namespaces {
        let prefix = op_key(&namespace, "");
        let (_prior, root) = inventory_prepare_namespace(
            db,
            op,
            revision,
            &namespace,
            prior_directory.is_some(),
            &nodes,
        )
        .await?;
        let (mut root, gate) =
            notification_prepare_gate(db, op, revision, &namespace, root, &nodes).await?;
        for key in removals.iter().filter(|key| key.starts_with(&prefix)) {
            if let Some((sequence, _)) = inventory_find(db, root.clone(), key, &nodes).await? {
                verify_inventory_members(db, &[(key.clone(), sequence)]).await?;
            }
            root = inventory_update(db, root, key, None, &mut nodes).await?;
        }
        // Last observation wins for repeated same-page transitions, after sequential
        // lifecycle validation; one leaf per stable identity, never one per poll.
        let unique: std::collections::BTreeSet<_> = additions
            .iter()
            .filter(|(ns, _)| ns == &namespace)
            .map(|(_, id)| op_key(&namespace, id))
            .collect();
        for key in unique {
            let summary = inventory_leaf_summary(op, &key)?;
            root = inventory_update(db, root, &key, Some((revision, summary)), &mut nodes).await?;
        }
        if let Some(gate) = gate {
            root = notification_update_gate(db, op, revision, gate, root, &mut nodes).await?;
        }
        commitments.push(HarnessInventoryCommitment {
            snapshot_key: namespace,
            revision,
            root: root.as_ref().map(|link| link.hash.clone()),
            summary: root.map_or_else(HarnessInventorySummary::default, |link| link.summary),
        });
    }
    let mut directory = prior_directory
        .as_ref()
        .map(inventory_root_link)
        .transpose()?
        .flatten();
    for commitment in &commitments {
        directory = inventory_update(
            db,
            directory,
            &commitment.snapshot_key,
            Some((
                revision,
                HarnessInventorySummary {
                    identities: 1,
                    ..Default::default()
                },
            )),
            &mut nodes,
        )
        .await?;
    }
    commitments.push(HarnessInventoryCommitment {
        snapshot_key: execution_namespace(&op.owner, "inventory_directory_v1"),
        revision,
        root: directory.as_ref().map(|root| root.hash.clone()),
        summary: directory.map_or_else(HarnessInventorySummary::default, |root| root.summary),
    });
    Ok((commitments, nodes))
}

impl SessionDb {
    /// Read canonical summary truth with bounded proof work, without enumerating descendants.
    /// Missing accessed proof nodes require repair; unvisited descendants are not certified.
    /// # Errors
    /// Rejects foreign owners, missing roots, damaged accessed proofs and concurrent changes.
    pub async fn harness_inventory_summary(
        &self,
        owner: &ExternalHarnessOwner,
        scope: &HarnessInventoryScope,
        kind: HarnessInventoryKind,
    ) -> SessionDbResult<bcode_session_models::external_harness::HarnessInventorySummaryRead> {
        owner.validate().map_err(SessionDbError::InvalidData)?;
        if owner.session_id != self.session_id {
            return Err(invalid());
        }
        let scope_key = match scope {
            HarnessInventoryScope::Attempt(attempt) => {
                attempt
                    .validate()
                    .map_err(|error| SessionDbError::InvalidData(error.to_string()))?;
                format!(
                    "attempt:{}",
                    submission_key(&attempt.submission_id, &attempt.attempt_id)
                )
            }
            HarnessInventoryScope::Uncorrelated => "unknown".into(),
        };
        let kind_key = match kind {
            HarnessInventoryKind::Terminal => "terminal",
            HarnessInventoryKind::Coverage => "coverage",
            HarnessInventoryKind::Work => "work",
            HarnessInventoryKind::Continuation => "continuation",
            HarnessInventoryKind::Interaction => "interaction",
        };
        let db = &**self.db;
        super::external_backend::external_index_ready(db).await?;
        let observed_through_sequence = last_sequence(db).await?.ok_or_else(invalid)?;
        let configured = latest(db, owner.session_id, "config", "current")
            .await?
            .ok_or_else(invalid)?;
        if operation(&configured)?.owner != *owner {
            return Err(invalid());
        }
        let namespace = execution_namespace(owner, &format!("inventory:{scope_key}:{kind_key}"));
        let commitment = inventory_current_commitment(db, owner, &namespace, None)
            .await?
            .ok_or_else(evidence_maintenance_required)?;
        notification_validate_commitment(db, owner, &namespace, &commitment).await?;
        if last_sequence(db).await? != Some(observed_through_sequence) {
            return Err(invalid());
        }
        Ok(
            bcode_session_models::external_harness::HarnessInventorySummaryRead {
                owner: owner.clone(),
                commitment,
                observed_through_sequence,
            },
        )
    }

    /// Read at most 32 canonical lifecycle identities from one authenticated inventory root.
    /// Cursors retain the old root across concurrent updates and process restarts. Coverage
    /// and foreground disposition remain separate; exhausting a page proves neither success.
    /// # Errors
    /// Rejects foreign/stale owners, forged cursors, missing nodes/members and damaged roots.
    pub async fn harness_inventory_page(
        &self,
        owner: &ExternalHarnessOwner,
        scope: &HarnessInventoryScope,
        kind: HarnessInventoryKind,
        cursor: Option<&HarnessInventoryCursor>,
    ) -> SessionDbResult<HarnessInventoryPage> {
        owner.validate().map_err(SessionDbError::InvalidData)?;
        if owner.session_id != self.session_id {
            return Err(invalid());
        }
        let (scope_key, kind_key) = inventory_scope_kind(scope, kind)?;
        let namespace = execution_namespace(owner, &format!("inventory:{scope_key}:{kind_key}"));
        let db = &**self.db;
        super::external_backend::external_index_ready(db).await?;
        let observed_through_sequence = last_sequence(db).await?.ok_or_else(invalid)?;
        let configured = latest(db, owner.session_id, "config", "current")
            .await?
            .ok_or_else(invalid)?;
        if operation(&configured)?.owner != *owner {
            return Err(invalid());
        }
        let (commitment, offset, pinned) = if let Some(cursor) = cursor {
            if cursor.owner != *owner || cursor.commitment.snapshot_key != namespace {
                return Err(invalid());
            }
            let event =
                inventory_canonical_event(db, owner.session_id, cursor.commitment.revision).await?;
            if operation(&event)?.owner != *owner
                || !operation(&event)?
                    .inventory_commitments
                    .contains(&cursor.commitment)
            {
                return Err(invalid());
            }
            (cursor.commitment.clone(), cursor.offset, true)
        } else {
            (
                inventory_current_commitment(db, owner, &namespace, None)
                    .await?
                    .ok_or_else(evidence_maintenance_required)?,
                0,
                false,
            )
        };
        notification_validate_commitment(db, owner, &namespace, &commitment).await?;
        if offset > commitment.summary.identities {
            return Err(invalid());
        }
        let rows = inventory_window(
            db,
            inventory_root_link(&commitment)?,
            offset,
            MAX_HARNESS_EXECUTION_ITEMS,
        )
        .await?;
        let expected =
            (commitment.summary.identities - offset).min(MAX_HARNESS_EXECUTION_ITEMS as u64);
        if rows.len() as u64 != expected {
            return Err(evidence_maintenance_required());
        }
        if !pinned {
            verify_inventory_members(db, &rows).await?;
        }
        for (key, sequence) in &rows {
            let (retained, _) = inventory_find(
                db,
                inventory_root_link(&commitment)?,
                key,
                &InventoryNodes::new(),
            )
            .await?
            .ok_or_else(evidence_maintenance_required)?;
            if retained != *sequence {
                return Err(evidence_maintenance_required());
            }
        }
        let entries = inventory_page_entries(db, owner, &namespace, kind_key, &rows).await?;
        let next_offset = offset.checked_add(rows.len() as u64).ok_or_else(invalid)?;
        let inventory_exhausted = next_offset == commitment.summary.identities;
        let next_cursor = (!inventory_exhausted).then(|| HarnessInventoryCursor {
            owner: owner.clone(),
            commitment: commitment.clone(),
            offset: next_offset,
        });
        if last_sequence(db).await? != Some(observed_through_sequence) {
            return Err(invalid());
        }
        Ok(HarnessInventoryPage {
            owner: owner.clone(),
            commitment,
            observed_through_sequence,
            next_cursor,
            inventory_exhausted,
            entries,
        })
    }

    /// Build read-only retention context from the exact canonical previous observe page.
    /// Requests come only from that committed page, not a caller-supplied native correlation.
    /// Missing metadata returns None (unknown), never an invented acknowledgment.
    /// # Errors
    /// Rejects foreign/stale targets, missing canonical pages, mismatched cursors,
    /// damaged pages and concurrent changes.
    pub async fn harness_retention_context(
        &self,
        owner: &ExternalHarnessOwner,
        target: &bcode_harness_models::HarnessSessionTarget,
        after: Option<&str>,
    ) -> SessionDbResult<Option<bcode_harness_models::HarnessRetentionContext>> {
        owner.validate().map_err(SessionDbError::InvalidData)?;
        if owner.session_id != self.session_id {
            return Err(invalid());
        }
        target
            .validate_owner(&owner.session_id.to_string(), owner.generation)
            .map_err(|_| invalid())?;
        let db = &**self.db;
        super::external_backend::external_index_ready(db).await?;
        let before = last_sequence(db).await?;
        let current = state(db, owner).await?.ok_or_else(invalid)?;
        if current.target.as_ref() != Some(target) || current.observation_after.as_deref() != after
        {
            return Err(invalid());
        }
        let Some(previous) = latest(db, owner.session_id, "page", "current").await? else {
            if after.is_some() || last_sequence(db).await? != before {
                return Err(evidence_maintenance_required());
            }
            return Ok(None);
        };
        let op = operation(&previous)?;
        if op.owner != *owner {
            return Err(invalid());
        }
        let R::ObservationPage { instance, page, .. } = &op.record else {
            return Err(invalid());
        };
        if *instance != current.instance || page.next_after.as_deref() != after {
            return Err(invalid());
        }
        // A later target replacement cannot adopt a previous transport's retained page.
        let binding = latest(db, owner.session_id, "binding", "current")
            .await?
            .ok_or_else(invalid)?;
        if binding.sequence > previous.sequence {
            return Err(invalid());
        }
        page.validate_retention().map_err(|_| invalid())?;
        let Some(retained) = &page.retention else {
            if last_sequence(db).await? != before {
                return Err(invalid());
            }
            return Ok(None);
        };
        let acknowledgment = self.harness_canonical_page_ack(owner, retained).await?;
        if acknowledgment.canonical_revision != previous.sequence {
            return Err(invalid());
        }
        let hydration = self
            .harness_native_hydration(owner, &page.hydration_requests)
            .await?;
        let context = bcode_harness_models::HarnessRetentionContext {
            connection_id: retained.connection_id.clone(),
            acknowledgment: Some(acknowledgment),
            hydration,
        };
        context
            .validate_for(&owner.session_id.to_string(), owner.generation)
            .map_err(|_| invalid())?;
        if last_sequence(db).await? != before {
            return Err(invalid());
        }
        Ok(Some(context))
    }

    /// Hydrate bounded genuine native references from canonical causality and exact current
    /// lifecycle locators. Never scans inventory pages or treats absence as no-effects proof.
    /// # Errors
    /// Rejects foreign owners, malformed requests, excess batch size and concurrent changes.
    pub async fn harness_native_hydration(
        &self,
        owner: &ExternalHarnessOwner,
        requests: &[bcode_harness_models::HarnessHydrationRequest],
    ) -> SessionDbResult<Vec<bcode_harness_models::HarnessHydrationReply>> {
        use bcode_harness_models::{
            HarnessHydrationReply, HarnessHydrationResolution as Resolution,
            HarnessNativeReferenceKind as K,
        };
        if owner.session_id != self.session_id
            || requests.len() > bcode_harness_models::MAX_HARNESS_ITEMS
        {
            return Err(invalid());
        }
        owner.validate().map_err(SessionDbError::InvalidData)?;
        let mut seen = std::collections::BTreeSet::new();
        for request in requests {
            request.validate().map_err(|_| invalid())?;
            if !seen.insert(&request.request_id) {
                return Err(invalid());
            }
        }
        let db = &**self.db;
        super::external_backend::external_index_ready(db).await?;
        let before = last_sequence(db).await?.ok_or_else(invalid)?;
        let configured = latest(db, owner.session_id, "config", "current")
            .await?
            .ok_or_else(invalid)?;
        if operation(&configured)?.owner != *owner {
            return Err(invalid());
        }
        let mut replies = Vec::new();
        let mut total_observations = 0;
        for request in requests {
            let resolved = native_hydration_observations(db, owner, &request.reference).await;
            let resolution = match resolved {
                Ok(Some(observations))
                    if total_observations + observations.len()
                        <= bcode_harness_models::MAX_HARNESS_ITEMS =>
                {
                    total_observations += observations.len();
                    Resolution::Found { observations }
                }
                Ok(None) if request.reference.kind != K::Item => Resolution::Absent,
                _ => Resolution::Unavailable,
            };
            let mut reply = HarnessHydrationReply {
                request_id: request.request_id.clone(),
                reference: request.reference.clone(),
                canonical_revision: before,
                resolution,
            };
            if reply
                .validate_for(&owner.session_id.to_string(), owner.generation)
                .is_err()
            {
                reply.resolution = Resolution::Unavailable;
            }
            replies.push(reply);
        }
        if last_sequence(db).await? != Some(before) {
            return Err(invalid());
        }
        Ok(replies)
    }

    /// Read one exact retained-page identity for serialized retry comparison.
    /// This does not acknowledge the page or authorize a new append.
    /// # Errors
    /// Rejects malformed identities, foreign sessions and unavailable canonical indexes.
    pub(crate) async fn harness_retained_page_event(
        &self,
        owner: &ExternalHarnessOwner,
        retained: &bcode_harness_models::HarnessRetentionPage,
    ) -> SessionDbResult<Option<SessionEvent>> {
        owner.validate().map_err(SessionDbError::InvalidData)?;
        retained.validate().map_err(|_| invalid())?;
        if owner.session_id != self.session_id {
            return Err(invalid());
        }
        let db = &**self.db;
        super::external_backend::external_index_ready(db).await?;
        latest(
            db,
            owner.session_id,
            &execution_namespace(owner, "retention_page_v1"),
            &submission_key(&retained.connection_id, &retained.page_id),
        )
        .await
    }

    /// Recover an exact canonical post-commit acknowledgment for the retained adapter page.
    /// This transfers retention responsibility only, never execution or settlement authority.
    /// # Errors
    /// Rejects foreign owner/connection/page, missing recording and digest mismatches.
    pub async fn harness_canonical_page_ack(
        &self,
        owner: &ExternalHarnessOwner,
        retained: &bcode_harness_models::HarnessRetentionPage,
    ) -> SessionDbResult<bcode_harness_models::HarnessCanonicalAck> {
        retained.validate().map_err(|_| invalid())?;
        owner.validate().map_err(SessionDbError::InvalidData)?;
        if owner.session_id != self.session_id {
            return Err(invalid());
        }
        let db = &**self.db;
        super::external_backend::external_index_ready(db).await?;
        let configured = latest(db, owner.session_id, "config", "current")
            .await?
            .ok_or_else(invalid)?;
        if operation(&configured)?.owner != *owner {
            return Err(invalid());
        }
        let event = latest(
            db,
            owner.session_id,
            &execution_namespace(owner, "retention_page_v1"),
            &submission_key(&retained.connection_id, &retained.page_id),
        )
        .await?
        .ok_or_else(invalid)?;
        let op = operation(&event)?;
        if op.owner != *owner {
            return Err(invalid());
        }
        let R::ObservationPage { page, .. } = &op.record else {
            return Err(invalid());
        };
        if page.retention.as_ref() != Some(retained) {
            return Err(invalid());
        }
        let digest = bcode_harness_models::harness_observations_digest(&page.observations)
            .map_err(|_| invalid())?;
        if digest != retained.observations_digest {
            return Err(invalid());
        }
        Ok(bcode_harness_models::HarnessCanonicalAck {
            page_id: retained.page_id.clone(),
            canonical_revision: event.sequence,
            observations_digest: digest,
        })
    }

    /// Resolve one stable identity at an exact canonical commitment without paging an inventory.
    /// This is retained evidence, not an adapter acknowledgement or execution authorization.
    /// # Errors
    /// Rejects foreign scope/kind/owner, malformed identities and missing or corrupt proofs.
    pub async fn harness_inventory_item(
        &self,
        owner: &ExternalHarnessOwner,
        scope: &HarnessInventoryScope,
        kind: HarnessInventoryKind,
        identity: &str,
        commitment: &HarnessInventoryCommitment,
    ) -> SessionDbResult<bcode_session_models::HarnessInventoryItemRead> {
        owner.validate().map_err(SessionDbError::InvalidData)?;
        if owner.session_id != self.session_id
            || identity.is_empty()
            || identity.len() > bcode_harness_models::MAX_HARNESS_ID_BYTES * 2 + 32
            || identity.chars().any(char::is_control)
        {
            return Err(invalid());
        }
        let (scope_key, kind_key) = inventory_scope_kind(scope, kind)?;
        let namespace = execution_namespace(owner, &format!("inventory:{scope_key}:{kind_key}"));
        if commitment.snapshot_key != namespace || commitment.revision > i64::MAX.cast_unsigned() {
            return Err(invalid());
        }
        let db = &**self.db;
        super::external_backend::external_index_ready(db).await?;
        let observed_through_sequence = last_sequence(db).await?.ok_or_else(invalid)?;
        let configured = latest(db, owner.session_id, "config", "current")
            .await?
            .ok_or_else(invalid)?;
        if operation(&configured)?.owner != *owner {
            return Err(invalid());
        }
        let canonical =
            inventory_canonical_event(db, owner.session_id, commitment.revision).await?;
        if operation(&canonical)?.owner != *owner
            || !operation(&canonical)?
                .inventory_commitments
                .contains(commitment)
        {
            return Err(invalid());
        }
        notification_validate_commitment(db, owner, &namespace, commitment).await?;
        let key = op_key(&namespace, identity);
        let found = inventory_find(
            db,
            inventory_root_link(commitment)?,
            &key,
            &InventoryNodes::new(),
        )
        .await?;
        let entry = if let Some((sequence, _)) = found {
            let version_key = op_key(
                "inventory_member_v1",
                &submission_key(&key, &sequence.to_string()),
            );
            let retained = db
                .select("turn_evidence")
                .columns(&["event_seq"])
                .where_eq("turn_id", version_key)
                .where_eq("evidence_kind", KIND)
                .where_eq("event_seq", seq_to_value(sequence))
                .execute_first(db)
                .await?;
            if retained.is_none() {
                return Err(evidence_maintenance_required());
            }
            let event = inventory_canonical_event(db, owner.session_id, sequence).await?;
            let op = operation(&event)?;
            if op.owner != *owner || !keys(op).iter().any(|(ns, id)| op_key(ns, id) == key) {
                return Err(invalid());
            }
            let R::ObservationPage { page, .. } = &op.record else {
                return Err(invalid());
            };
            let observation = page
                .observations
                .iter()
                .rev()
                .find(|observation| match &observation.kind {
                    HarnessObservationKind::Execution { event } => {
                        execution_identity(event) == (kind_key, identity.to_owned())
                    }
                    HarnessObservationKind::Interaction { interaction } => {
                        kind_key == "interaction" && interaction.request_id == identity
                    }
                    _ => false,
                })
                .ok_or_else(invalid)?
                .clone();
            Some(HarnessInventoryEntry {
                key,
                event_sequence: sequence,
                observation,
            })
        } else {
            None
        };
        if last_sequence(db).await? != Some(observed_through_sequence) {
            return Err(invalid());
        }
        Ok(bcode_session_models::HarnessInventoryItemRead {
            owner: owner.clone(),
            commitment: commitment.clone(),
            observed_through_sequence,
            entry,
        })
    }

    /// Compute host-only inventory commitments before append. Transactional append validates
    /// the exact result again; this method grants neither writer ownership nor append authority.
    /// # Errors
    /// Rejects damaged proofs, inconsistent current roots and malformed lifecycle references.
    pub async fn prepare_harness_inventory_operation(
        &self,
        op: &mut ExternalHarnessOperation,
        revision: u64,
    ) -> SessionDbResult<()> {
        verify_operation_writer(&**self.db, op).await?;
        // Construction fills empty commitments; retained commitments must compare exactly.
        prepare_current_inventory_operation(&**self.db, op, revision).await?;
        Ok(())
    }
}

async fn inventory_page_entries(
    db: &dyn Database,
    owner: &ExternalHarnessOwner,
    namespace: &str,
    kind_key: &str,
    rows: &[(String, u64)],
) -> SessionDbResult<Vec<HarnessInventoryEntry>> {
    let mut entries = Vec::new();
    for (key, sequence) in rows {
        let version_key = op_key(
            "inventory_member_v1",
            &submission_key(key, &sequence.to_string()),
        );
        let retained = db
            .select("turn_evidence")
            .columns(&["event_seq"])
            .where_eq("turn_id", version_key)
            .where_eq("evidence_kind", KIND)
            .where_eq("event_seq", seq_to_value(*sequence))
            .execute_first(db)
            .await?;
        if retained.is_none() {
            return Err(evidence_maintenance_required());
        }
        let event = inventory_canonical_event(db, owner.session_id, *sequence).await?;
        let op = operation(&event)?;
        if op.owner != *owner || !keys(op).iter().any(|(ns, id)| op_key(ns, id) == *key) {
            return Err(invalid());
        }
        let R::ObservationPage { page, .. } = &op.record else {
            return Err(invalid());
        };
        let observation = page
            .observations
            .iter()
            .rev()
            .find(|observation| match &observation.kind {
                HarnessObservationKind::Execution { event } => {
                    let (entry_kind, id) = execution_identity(event);
                    entry_kind == kind_key && *key == op_key(namespace, &id)
                }
                HarnessObservationKind::Interaction { interaction } => {
                    kind_key == "interaction" && *key == op_key(namespace, &interaction.request_id)
                }
                _ => false,
            })
            .ok_or_else(invalid)?
            .clone();
        entries.push(HarnessInventoryEntry {
            key: key.clone(),
            event_sequence: *sequence,
            observation,
        });
    }
    Ok(entries)
}

fn inventory_scope_kind(
    scope: &HarnessInventoryScope,
    kind: HarnessInventoryKind,
) -> SessionDbResult<(String, &'static str)> {
    let scope_key = match scope {
        HarnessInventoryScope::Attempt(attempt) => {
            attempt
                .validate()
                .map_err(|error| SessionDbError::InvalidData(error.to_string()))?;
            format!(
                "attempt:{}",
                submission_key(&attempt.submission_id, &attempt.attempt_id)
            )
        }
        HarnessInventoryScope::Uncorrelated => "unknown".into(),
    };
    let kind_key = match kind {
        HarnessInventoryKind::Terminal => "terminal",
        HarnessInventoryKind::Coverage => "coverage",
        HarnessInventoryKind::Work => "work",
        HarnessInventoryKind::Continuation => "continuation",
        HarnessInventoryKind::Interaction => "interaction",
    };
    Ok((scope_key, kind_key))
}

type HydrationObservations =
    std::collections::BTreeMap<(u64, usize), bcode_harness_models::HarnessObservation>;

async fn native_hydration_current(
    db: &dyn Database,
    owner: &ExternalHarnessOwner,
    reference: &bcode_harness_models::HarnessNativeReference,
    source: &SessionEvent,
    observation: &bcode_harness_models::HarnessObservation,
    identities: &mut std::collections::BTreeSet<(String, String)>,
    ordered: &mut HydrationObservations,
) -> SessionDbResult<(SessionEvent, bcode_harness_models::HarnessObservation)> {
    let mut retained = source.clone();
    let mut selected = observation.clone();
    if let HarnessObservationKind::Execution { event } = &observation.kind {
        let (kind, identity) = execution_identity(event);
        identities.insert((kind.to_owned(), identity.clone()));
        retained = latest(
            db,
            owner.session_id,
            &execution_namespace(owner, kind),
            &identity,
        )
        .await?
        .ok_or_else(evidence_maintenance_required)?;
        if operation(&retained)?.owner != *owner {
            return Err(invalid());
        }
        let current_observation = execution_observation(&retained, kind, &identity)?;
        if let HarnessObservationKind::Execution { event } = &current_observation.kind {
            let inventory_namespace = execution_namespace(
                owner,
                &format!("inventory:{}:{kind}", execution_scope(event)),
            );
            let current_root = inventory_current_commitment(db, owner, &inventory_namespace, None)
                .await?
                .ok_or_else(evidence_maintenance_required)?;
            let current_key = op_key(&inventory_namespace, &identity);
            let current_member = inventory_find(
                db,
                inventory_root_link(&current_root)?,
                &current_key,
                &InventoryNodes::new(),
            )
            .await?
            .ok_or_else(evidence_maintenance_required)?;
            if current_member.0 != retained.sequence {
                return Err(evidence_maintenance_required());
            }
            verify_inventory_members(db, &[(current_key, retained.sequence)]).await?;
        }
        selected = current_observation;
        if !reference.matches_observation(&selected) {
            return Err(invalid());
        }
        if let HarnessObservationKind::Execution { event } = &selected.kind
            && let Some(origin) = execution_origin(event)
            && let Some(accepted) = latest(
                db,
                owner.session_id,
                "submission_acceptance",
                &submission_key(&origin.submission_id, &origin.attempt_id),
            )
            .await?
        {
            let (position, acceptance) = evidence_position(&accepted, |value| {
                matches!(&value.kind,
                            HarnessObservationKind::SubmissionAccepted { submission_id, attempt_id, .. }
                            if submission_id == &origin.submission_id && attempt_id == &origin.attempt_id)
            })?;
            if operation(&accepted)?.owner != *owner {
                return Err(invalid());
            }
            ordered.insert((accepted.sequence, position), acceptance);
        }
    }
    Ok((retained, selected))
}

async fn native_hydration_observations(
    db: &dyn Database,
    owner: &ExternalHarnessOwner,
    reference: &bcode_harness_models::HarnessNativeReference,
) -> SessionDbResult<Option<Vec<bcode_harness_models::HarnessObservation>>> {
    use bcode_harness_models::HarnessNativeReferenceKind as K;
    if reference.kind == K::Item {
        return Err(evidence_maintenance_required());
    }
    let namespace = execution_namespace(owner, "inventory:native_reference_v1");
    let commitment = inventory_current_commitment(db, owner, &namespace, None)
        .await?
        .ok_or_else(evidence_maintenance_required)?;
    let mut locator_reference = reference.clone();
    locator_reference.native_turn_id = None;
    let key = op_key(&namespace, &native_reference_id(&locator_reference));
    let Some((sequence, _)) = inventory_find(
        db,
        inventory_root_link(&commitment)?,
        &key,
        &InventoryNodes::new(),
    )
    .await?
    else {
        return Ok(None);
    };
    verify_inventory_members(db, &[(key, sequence)]).await?;
    let source = inventory_canonical_event(db, owner.session_id, sequence).await?;
    if operation(&source)?.owner != *owner {
        return Err(invalid());
    }
    let R::ObservationPage { page, .. } = &operation(&source)?.record else {
        return Err(invalid());
    };
    let mut ordered = std::collections::BTreeMap::new();
    let mut identities = std::collections::BTreeSet::new();
    for observation in &page.observations {
        if !reference.matches_observation(observation) {
            continue;
        }
        // Keep the exact canonical causality observation as well as the current state.
        let (position, _) = evidence_position(&source, |value| value == observation)?;
        ordered.insert((source.sequence, position), observation.clone());
        let (retained, selected) = native_hydration_current(
            db,
            owner,
            reference,
            &source,
            observation,
            &mut identities,
            &mut ordered,
        )
        .await?;
        let (position, _) = evidence_position(&retained, |value| value == &selected)?;
        ordered.insert((retained.sequence, position), selected);
    }
    if identities.len() > 1 {
        return Err(evidence_maintenance_required());
    }
    if reference.kind == K::Turn
        && let Some(accepted) =
            latest(db, owner.session_id, "accepted_turn", &reference.native_id).await?
    {
        if operation(&accepted)?.owner != *owner {
            return Err(invalid());
        }
        let (position, acceptance) = evidence_position(&accepted, |value| {
            matches!(&value.kind,
                HarnessObservationKind::SubmissionAccepted { native_turn_id, .. } if native_turn_id == &reference.native_id)
        })?;
        ordered.insert((accepted.sequence, position), acceptance.clone());
        if let HarnessObservationKind::SubmissionAccepted {
            submission_id,
            attempt_id,
            ..
        } = &acceptance.kind
        {
            for kind in ["terminal", "coverage"] {
                let identity = submission_key(submission_id, attempt_id);
                if let Some(current) = latest(
                    db,
                    owner.session_id,
                    &execution_namespace(owner, kind),
                    &identity,
                )
                .await?
                {
                    if operation(&current)?.owner != *owner {
                        return Err(invalid());
                    }
                    let selected = execution_observation(&current, kind, &identity)?;
                    let (position, _) = evidence_position(&current, |value| value == &selected)?;
                    ordered.insert((current.sequence, position), selected);
                }
            }
        }
    }
    if ordered.is_empty() || ordered.len() > bcode_harness_models::MAX_HARNESS_ITEMS {
        return Err(invalid());
    }
    Ok(Some(ordered.into_values().collect()))
}

async fn inventory_canonical_event(
    db: &dyn Database,
    session: SessionId,
    sequence: u64,
) -> SessionDbResult<SessionEvent> {
    let length = db
        .select("events")
        .project_as(
            switchy::database::query::byte_length(switchy::database::query::identifier("payload")),
            "bytes",
        )
        .where_eq("event_seq", seq_to_value(sequence))
        .execute_first(db)
        .await?
        .ok_or_else(evidence_maintenance_required)?;
    if required_non_negative_u64(&length, "bytes")? > (MAX_HARNESS_WIRE_BYTES * 6 + 4096) as u64 {
        return Err(invalid());
    }
    let row = db
        .select("events")
        .columns(&["event_seq", "payload"])
        .where_eq("event_seq", seq_to_value(sequence))
        .execute_first(db)
        .await?
        .ok_or_else(evidence_maintenance_required)?;
    let event = strict_event_from_row(&row, session)?;
    verify_operation_writer(db, operation(&event)?).await?;
    Ok(event)
}

const KIND: &str = "harness_operation_v1";
fn op_key(namespace: &str, id: &str) -> String {
    bcode_session_models::external_backend::external_backend_key(namespace, id)
}
fn invalid() -> SessionDbError {
    SessionDbError::InvalidData("canonical harness operation conflict or stale owner".into())
}

fn evidence_maintenance_required() -> SessionDbError {
    SessionDbError::InvalidData(
        "managed harness evidence index coverage is missing or stale; explicit session maintenance must backfill canonical harness evidence before evidence reads or page appends".into(),
    )
}

fn operation(event: &SessionEvent) -> SessionDbResult<&ExternalHarnessOperation> {
    let SessionEventKind::ExternalHarnessOperation { operation } = &event.kind else {
        return Err(invalid());
    };
    operation
        .validate_event_schema(event.schema_version)
        .map_err(SessionDbError::InvalidData)?;
    if operation.owner.session_id != event.session_id {
        return Err(invalid());
    }
    Ok(operation)
}
async fn verify_operation_writer(
    db: &dyn Database,
    op: &ExternalHarnessOperation,
) -> SessionDbResult<()> {
    if op.is_selected_open()
        && super::storage_writer_epoch_for(db).await?
            != u64::from(bcode_session_models::external_harness::SELECTED_OPEN_STORAGE_WRITER_EPOCH)
    {
        return Err(invalid());
    }
    Ok(())
}

async fn latest(
    db: &dyn Database,
    session: SessionId,
    namespace: &str,
    id: &str,
) -> SessionDbResult<Option<SessionEvent>> {
    // Switchy's sorted selection wraps the sort expression in parentheses. The locked
    // Turso planner does not match that expression to ordinary index ordering. MAX keeps
    // the extremum argument a plain column and uses the existing equality-prefix index.
    let rows = db
        .select("turn_evidence")
        .project_as(
            switchy::database::query::max(switchy::database::query::identifier("event_seq")),
            "event_seq",
        )
        .where_eq("turn_id", op_key(namespace, id))
        .where_eq("evidence_kind", KIND)
        .execute(db)
        .await?;
    let row = rows.first().ok_or_else(invalid)?;
    if matches!(
        row.get("event_seq"),
        Some(switchy::database::DatabaseValue::Null)
    ) {
        return Ok(None);
    }
    let sequence = required_non_negative_u64(row, "event_seq")?;
    let lengths = db
        .select("events")
        .project_as(
            switchy::database::query::byte_length(switchy::database::query::identifier("payload")),
            "bytes",
        )
        .where_eq("event_seq", seq_to_value(sequence))
        .limit(1)
        .execute(db)
        .await?;
    if required_non_negative_u64(lengths.first().ok_or_else(invalid)?, "bytes")?
        > (bcode_harness_models::MAX_HARNESS_WIRE_BYTES * 6 + 4096) as u64
    {
        return Err(invalid());
    }
    let rows = db
        .select("events")
        .columns(&["event_seq", "payload"])
        .where_eq("event_seq", seq_to_value(sequence))
        .limit(1)
        .execute(db)
        .await?;
    let event = strict_event_from_row(rows.first().ok_or_else(invalid)?, session)?;
    let op = operation(&event)?;
    verify_operation_writer(db, op).await?;
    if !keys(op)
        .iter()
        .any(|(ns, identity)| ns == namespace && identity == id)
    {
        return Err(invalid());
    }
    Ok(Some(event))
}
fn mutation_key(request: &ExternalHarnessRequest) -> String {
    use ExternalHarnessRequestKind as R;
    match &request.kind {
        R::Open(_) | R::OpenSelected(_) => "open".into(),
        R::Submit { submission, .. } => format!("submit:{}", submission.submission_id),
        R::Respond { response, .. } => format!("respond:{}", response.request_id),
        R::Interrupt(v) => format!("interrupt:{}", v.mutation_id),
        R::Close(v) => format!("close:{}", v.mutation_id),
        R::Reconcile(_) => format!("reconcile:{}", request.request_id),
    }
}
fn submission_key(submission_id: &str, attempt_id: &str) -> String {
    format!("{}:{submission_id}:{attempt_id}", submission_id.len())
}
fn claim_keys(request: &ExternalHarnessRequest) -> Vec<(String, String)> {
    let mut keys = vec![
        ("claim".into(), request.request_id.clone()),
        ("mutation".into(), mutation_key(request)),
    ];
    if let ExternalHarnessRequestKind::Submit {
        target: bcode_harness_models::HarnessSessionTarget::Prepared(prepared),
        ..
    } = &request.kind
    {
        keys.push(("prepared_submit".into(), prepared.open_id.clone()));
    }
    if let ExternalHarnessRequestKind::Respond { response, .. } = &request.kind {
        keys.push(("answered".into(), response.request_id.clone()));
    }
    if let ExternalHarnessRequestKind::Submit { submission, .. } = &request.kind {
        keys.push((
            "submission_claim".into(),
            submission_key(&submission.submission_id, &submission.attempt_id),
        ));
    }
    keys
}
fn completion_keys(
    request: &ExternalHarnessRequest,
    response: &ExternalHarnessResponse,
) -> Vec<(String, String)> {
    let mut keys = vec![("completion".into(), request.request_id.clone())];
    match &response.kind {
        ExternalHarnessResponseKind::Selected { .. }
        | ExternalHarnessResponseKind::Prepared(_)
        | ExternalHarnessResponseKind::Opened(_)
        | ExternalHarnessResponseKind::Reconciled(
            bcode_harness_models::HarnessReconciliation::Known { .. }
            | bcode_harness_models::HarnessReconciliation::KnownPrepared { .. },
        ) => keys.push(("binding".into(), "current".into())),
        _ => {}
    }
    let receipt = match &response.kind {
        ExternalHarnessResponseKind::Receipt(receipt) => Some(receipt),
        ExternalHarnessResponseKind::Reconciled(
            bcode_harness_models::HarnessReconciliation::Known { receipt, .. }
            | bcode_harness_models::HarnessReconciliation::KnownPrepared { receipt, .. },
        ) => receipt.as_ref(),
        _ => None,
    };
    if let Some(receipt) = receipt {
        keys.push((
            "receipt".into(),
            format!(
                "{}:{}:{}",
                receipt.submission_id.len(),
                receipt.submission_id,
                receipt.attempt_id
            ),
        ));
    }
    if let ExternalHarnessRequestKind::Respond { response, .. } = &request.kind {
        keys.push(("answered".into(), response.request_id.clone()));
    }
    keys
}

fn execution_identity(event: &HarnessExecutionEvent) -> (&'static str, String) {
    match event {
        HarnessExecutionEvent::SubmissionTerminal { attempt, .. } => (
            "terminal",
            submission_key(&attempt.submission_id, &attempt.attempt_id),
        ),
        HarnessExecutionEvent::Coverage { attempt, .. } => (
            "coverage",
            submission_key(&attempt.submission_id, &attempt.attempt_id),
        ),
        HarnessExecutionEvent::BackgroundWork { work_id, .. } => ("work", work_id.clone()),
        HarnessExecutionEvent::Continuation {
            continuation_id, ..
        } => ("continuation", continuation_id.clone()),
    }
}

fn execution_namespace(owner: &ExternalHarnessOwner, kind: &str) -> String {
    format!("execution_v3:{}:{kind}", owner.generation)
}

const fn execution_origin(event: &HarnessExecutionEvent) -> Option<&HarnessAttemptIdentity> {
    match event {
        HarnessExecutionEvent::SubmissionTerminal { attempt, .. }
        | HarnessExecutionEvent::Coverage { attempt, .. } => Some(attempt),
        HarnessExecutionEvent::BackgroundWork { origin, .. }
        | HarnessExecutionEvent::Continuation { origin, .. } => origin.as_ref(),
    }
}

fn execution_scope(event: &HarnessExecutionEvent) -> String {
    execution_origin(event).map_or_else(
        || "unknown".to_owned(),
        |attempt| {
            format!(
                "attempt:{}",
                submission_key(&attempt.submission_id, &attempt.attempt_id)
            )
        },
    )
}

const fn execution_state(event: &HarnessExecutionEvent) -> Option<HarnessWorkState> {
    match event {
        HarnessExecutionEvent::BackgroundWork { state, .. }
        | HarnessExecutionEvent::Continuation { state, .. } => Some(*state),
        _ => None,
    }
}

fn execution_observation(
    event: &SessionEvent,
    kind: &str,
    id: &str,
) -> SessionDbResult<bcode_harness_models::HarnessObservation> {
    let R::ObservationPage { page, .. } = &operation(event)?.record else {
        return Err(invalid());
    };
    page.observations
        .iter()
        .rev()
        .find(|observation| {
            matches!(&observation.kind, HarnessObservationKind::Execution { event }
            if execution_identity(event) == (kind, id.to_owned()))
        })
        .cloned()
        .ok_or_else(invalid)
}

fn execution_retired(event: &HarnessExecutionEvent) -> bool {
    match event {
        HarnessExecutionEvent::BackgroundWork {
            state,
            notification,
            ..
        } => {
            state.is_terminal()
                && *notification == bcode_harness_models::HarnessNotificationState::NotExpected
        }
        HarnessExecutionEvent::Continuation { state, .. } => state.is_terminal(),
        _ => false,
    }
}

// Current memberships are replaceable; canonical identity/history locators remain append-only.
// This prevents update history from becoming inventory scan work while preserving old attempts.
fn replaced_execution_keys(op: &ExternalHarnessOperation) -> Vec<String> {
    let mut replaced = Vec::new();
    if let R::DeliveryCompleted {
        request,
        response: result,
    } = &op.record
        && matches!(
            result.kind,
            ExternalHarnessResponseKind::Acknowledged { .. }
        )
        && let ExternalHarnessRequestKind::Respond { response, .. } = &request.kind
    {
        replaced.push(op_key(
            &execution_namespace(&op.owner, "inventory:unknown:interaction"),
            &response.request_id,
        ));
    }
    let R::ObservationPage { page, .. } = &op.record else {
        return replaced;
    };
    for observation in &page.observations {
        for reference in native_observation_references(observation) {
            replaced.push(op_key(
                &execution_namespace(&op.owner, "inventory:native_reference_v1"),
                &native_reference_id(&reference),
            ));
        }
    }
    replaced.extend(
        page.observations
            .iter()
            .filter_map(|observation| {
                let HarnessObservationKind::Execution { event } = &observation.kind else {
                    return None;
                };
                let (kind, id) = execution_identity(event);
                let mut result = vec![op_key(
                    &execution_namespace(&op.owner, &format!("inventory:unknown:{kind}")),
                    &id,
                )];
                let scope = execution_scope(event);
                result.push(op_key(
                    &execution_namespace(&op.owner, &format!("inventory:{scope}:{kind}")),
                    &id,
                ));
                Some(result)
            })
            .flatten(),
    );
    replaced
}

fn inventory_namespaces(op: &ExternalHarnessOperation) -> std::collections::BTreeSet<String> {
    let mut result = std::collections::BTreeSet::new();
    if matches!(op.record, R::Configured { .. } | R::RecoveryPrepared { .. }) {
        // Establish unknown-scope empty proofs once, not on every unrelated mutation.
        for kind in [
            "terminal",
            "coverage",
            "work",
            "continuation",
            "interaction",
        ] {
            result.insert(execution_namespace(
                &op.owner,
                &format!("inventory:unknown:{kind}"),
            ));
        }
    }
    if let R::ObservationPage { page, .. } = &op.record {
        for observation in &page.observations {
            match &observation.kind {
                HarnessObservationKind::Execution { event } => {
                    let (kind, _) = execution_identity(event);
                    if matches!(
                        event,
                        HarnessExecutionEvent::Coverage { .. }
                            | HarnessExecutionEvent::Continuation {
                                source_work_id: Some(_),
                                ..
                            }
                    ) {
                        result.insert(execution_namespace(
                            &op.owner,
                            &format!("inventory:{}:work", execution_scope(event)),
                        ));
                    }
                    for scope in ["unknown".to_owned(), execution_scope(event)] {
                        result.insert(execution_namespace(
                            &op.owner,
                            &format!("inventory:{scope}:{kind}"),
                        ));
                    }
                }
                HarnessObservationKind::Interaction { .. } => {
                    result.insert(execution_namespace(
                        &op.owner,
                        "inventory:unknown:interaction",
                    ));
                }
                _ => {}
            }
        }
    }
    if let R::ObservationPage { page, .. } = &op.record {
        for observation in &page.observations {
            if !native_observation_references(observation).is_empty() {
                result.insert(execution_namespace(
                    &op.owner,
                    "inventory:native_reference_v1",
                ));
            }
        }
    }
    if let R::DeliveryClaimed { request } = &op.record
        && let ExternalHarnessRequestKind::Submit { submission, .. } = &request.kind
    {
        let scope = format!(
            "attempt:{}",
            submission_key(&submission.submission_id, &submission.attempt_id)
        );
        for kind in ["terminal", "coverage", "work", "continuation"] {
            result.insert(execution_namespace(
                &op.owner,
                &format!("inventory:{scope}:{kind}"),
            ));
        }
    }
    if let R::DeliveryCompleted { request, .. } = &op.record
        && matches!(request.kind, ExternalHarnessRequestKind::Respond { .. })
    {
        result.insert(execution_namespace(
            &op.owner,
            "inventory:unknown:interaction",
        ));
    }
    result
}

fn execution_locator_keys(
    op: &ExternalHarnessOperation,
    observation: &bcode_harness_models::HarnessObservation,
    keys: &mut Vec<(String, String)>,
) {
    if let HarnessObservationKind::Execution { event } = &observation.kind {
        let (kind, id) = execution_identity(event);
        keys.push((execution_namespace(&op.owner, kind), id));
        if let HarnessExecutionEvent::Continuation {
            origin: Some(origin),
            source_work_id: Some(source),
            state,
            ..
        } = event
            && state.is_terminal()
            && observation.completeness == HarnessCompleteness::Complete
            && matches!(&op.record, R::ObservationPage { page, .. }
                if page.completeness == HarnessCompleteness::Complete)
        {
            keys.push((notification_namespace(&op.owner, origin), source.clone()));
        }
        if let HarnessExecutionEvent::BackgroundWork {
            native_task_id: Some(native),
            ..
        } = event
        {
            keys.push((
                execution_namespace(&op.owner, "native_task"),
                native.clone(),
            ));
        }
    }
}

fn current_execution_keys(
    op: &ExternalHarnessOperation,
    page: &HarnessObservationPage,
    keys: &mut Vec<(String, String)>,
) {
    let mut final_execution = std::collections::BTreeMap::new();
    for observation in &page.observations {
        if let HarnessObservationKind::Execution { event } = &observation.kind {
            final_execution.insert(execution_identity(event), event);
        }
    }
    for ((kind, id), event) in final_execution {
        let scope = execution_scope(event);
        // Keep terminal identities and immutable failures in the authenticated inventory.
        // Outstanding-responsibility counters, not deletion, discharge settled work.
        {
            keys.push((
                execution_namespace(&op.owner, &format!("inventory:{scope}:{kind}")),
                id,
            ));
        }
    }
}

fn native_reference_id(reference: &bcode_harness_models::HarnessNativeReference) -> String {
    use bcode_harness_models::HarnessNativeReferenceKind as K;
    let kind = match reference.kind {
        K::Turn => "turn",
        K::Item => "item",
        K::Process => "process",
        K::Agent => "agent",
    };
    submission_key(
        kind,
        &submission_key(
            &reference.native_id,
            reference.native_turn_id.as_deref().unwrap_or(""),
        ),
    )
}
fn native_observation_references(
    observation: &bcode_harness_models::HarnessObservation,
) -> Vec<bcode_harness_models::HarnessNativeReference> {
    use bcode_harness_models::{HarnessNativeReference as Ref, HarnessNativeReferenceKind as K};
    let mut references = std::collections::BTreeSet::new();
    let turn = match &observation.kind {
        HarnessObservationKind::SubmissionAccepted { native_turn_id, .. }
        | HarnessObservationKind::NativeTurnTerminal { native_turn_id, .. } => Some(native_turn_id),
        HarnessObservationKind::Execution {
            event:
                HarnessExecutionEvent::SubmissionTerminal { native_turn_id, .. }
                | HarnessExecutionEvent::Continuation { native_turn_id, .. },
        } => native_turn_id.as_ref(),
        _ => None,
    };
    if let Some(turn) = turn {
        references.insert(Ref {
            kind: K::Turn,
            native_id: turn.clone(),
            native_turn_id: None,
        });
    }
    if let HarnessObservationKind::Execution {
        event:
            HarnessExecutionEvent::BackgroundWork {
                native_task_id,
                work_kind,
                ..
            },
    } = &observation.kind
        && let Some(native) = native_task_id
    {
        let kind = match work_kind {
            bcode_harness_models::HarnessWorkKind::Command => Some(K::Process),
            bcode_harness_models::HarnessWorkKind::Subagent => Some(K::Agent),
            _ => None,
        };
        if let Some(kind) = kind {
            references.insert(Ref {
                kind,
                native_id: native.clone(),
                native_turn_id: None,
            });
        }
    }
    // Item/turn associations are not typed on canonical BackgroundWork. Do not
    // guess which unordered correlation string is the turn versus native item.
    references
        .into_iter()
        .filter(|reference| reference.matches_observation(observation))
        .collect()
}

fn observation_page_keys(
    op: &ExternalHarnessOperation,
    page: &bcode_harness_models::HarnessObservationPage,
    after: Option<&String>,
) -> Vec<(String, String)> {
    let mut keys = vec![
        ("history".into(), "all".into()),
        ("terminal_coverage_v2".into(), "current".into()),
        ("execution_coverage_v3".into(), "current".into()),
        ("page".into(), "current".into()),
        (
            "page_input".into(),
            format!(
                "{}:{}",
                op.owner.generation,
                serde_json::to_string(&after).unwrap_or_default()
            ),
        ),
    ];
    if page.completeness == HarnessCompleteness::Gap
        || page.status.completeness == HarnessCompleteness::Gap
        || page.observations.iter().any(observation_gap)
    {
        keys.push(("evidence_gap".into(), op.owner.generation.to_string()));
    }
    for observation in &page.observations {
        execution_locator_keys(op, observation, &mut keys);
        if let HarnessObservationKind::Interaction { interaction } = &observation.kind {
            keys.push((
                execution_namespace(&op.owner, "inventory:unknown:interaction"),
                interaction.request_id.clone(),
            ));
        }
        if matches!(observation.kind, HarnessObservationKind::Result { .. }) {
            for turn in &observation.native_correlations {
                keys.push(("turn_result".into(), turn.clone()));
            }
        }
        if matches!(
            observation.kind,
            HarnessObservationKind::BindingConfirmed { .. }
        ) {
            keys.push(("binding".into(), "current".into()));
        }
        if let HarnessObservationKind::NativeTurnTerminal { native_turn_id, .. } = &observation.kind
        {
            keys.push(("native_terminal".into(), native_turn_id.clone()));
        }
        if let HarnessObservationKind::Settlement { native_turn_id } = &observation.kind {
            keys.push(("settled".into(), native_turn_id.clone()));
        }
        if let HarnessObservationKind::SubmissionAccepted {
            submission_id,
            attempt_id,
            native_turn_id,
        } = &observation.kind
        {
            keys.push(("accepted_turn".into(), native_turn_id.clone()));
            keys.push((
                "submission_acceptance".into(),
                submission_key(submission_id, attempt_id),
            ));
        }
        if let Some(key) = submission_frame_key(op.owner.generation, &observation.kind) {
            keys.push(("submission_native_frame_v2".into(), key));
        }
        keys.push(("observation".into(), observation.observation_id.clone()));
        if let HarnessObservationKind::Interaction { interaction } = &observation.kind {
            keys.push(("interaction".into(), interaction.request_id.clone()));
        }
    }
    if let Some(retention) = &page.retention {
        keys.push((
            execution_namespace(&op.owner, "retention_page_v1"),
            submission_key(&retention.connection_id, &retention.page_id),
        ));
    }
    for observation in &page.observations {
        for reference in native_observation_references(observation) {
            keys.push((
                execution_namespace(&op.owner, "inventory:native_reference_v1"),
                native_reference_id(&reference),
            ));
        }
    }
    current_execution_keys(op, page, &mut keys);
    keys
}

fn keys(op: &ExternalHarnessOperation) -> Vec<(String, String)> {
    let mut keys = match &op.record {
        R::Configured { .. } => vec![("config".into(), "current".into())],
        R::RecoveryPrepared { recovery } => vec![
            ("config".into(), "current".into()),
            (
                "recovery".into(),
                recovery.resume_request.request_id.clone(),
            ),
        ],
        R::TransportReleased { .. } => vec![(
            "transport_release".into(),
            format!("generation:{}", op.owner.generation),
        )],
        R::DeliveryClaimed { request } => claim_keys(request),
        R::DeliveryCompleted { request, response } => completion_keys(request, response),
        R::ObservationPage { page, after, .. } => observation_page_keys(op, page, after.as_ref()),
    };
    for namespace in inventory_namespaces(op) {
        keys.push(("execution_inventory_proof".into(), namespace));
    }
    keys.sort();
    keys.dedup();
    // Immutable membership-version locators survive current-row replacement. Pinned reads
    // verify these independently of mutable current membership and proof-node storage.
    let versions: Vec<_> = keys
        .iter()
        .filter(|(namespace, _)| namespace.contains(":inventory:"))
        .map(|(namespace, id)| {
            (
                "inventory_member_v1".to_owned(),
                submission_key(
                    &op_key(namespace, id),
                    &op.inventory_commitments
                        .iter()
                        .find(|root| {
                            root.snapshot_key == *namespace
                                || namespace
                                    .strip_suffix(":notification_reverse_v1")
                                    .is_some_and(|prefix| {
                                        root.snapshot_key == format!("{prefix}:work")
                                    })
                        })
                        .map_or(0, |root| root.revision)
                        .to_string(),
                ),
            )
        })
        .collect();
    keys.extend(versions);
    keys
}
pub(super) async fn project(db: &dyn Database, event: &SessionEvent) -> SessionDbResult<bool> {
    if !matches!(
        event.kind,
        SessionEventKind::ExternalHarnessOperation { .. }
    ) {
        return Ok(false);
    }
    let op = operation(event)?;
    verify_operation_writer(db, op).await?;
    let (expected, nodes) = prepare_inventory_commitments(db, op, event.sequence).await?;
    if op.inventory_commitments != expected {
        return Err(invalid());
    }
    for (hash, node) in nodes {
        let name = format!("harness_inventory_node_v1:{hash}");
        let present = db
            .select("snapshots")
            .columns(&["snapshot_name"])
            .where_eq("snapshot_name", name.clone())
            .execute_first(db)
            .await?;
        if present.is_none() {
            db.insert("snapshots")
                .value("snapshot_name", name)
                .value("last_event_seq", seq_to_value(event.sequence))
                .value("schema_version", 2_i64)
                .value("payload", serde_json::to_string(&node)?)
                .execute(db)
                .await?;
        } else {
            inventory_load_node(db, &inventory_link(&node)?, &InventoryNodes::new()).await?;
        }
    }
    for commitment in &op.inventory_commitments {
        let name = &commitment.snapshot_key;
        db.delete("snapshots")
            .where_eq("snapshot_name", name.clone())
            .execute(db)
            .await?;
        db.insert("snapshots")
            .value("snapshot_name", name.clone())
            .value("last_event_seq", seq_to_value(event.sequence))
            .value("schema_version", 2_i64)
            .value("payload", serde_json::to_string(commitment)?)
            .execute(db)
            .await?;
    }
    let replaced: std::collections::BTreeSet<_> = replaced_execution_keys(op).into_iter().collect();
    for key in replaced {
        db.delete("turn_evidence")
            .where_eq("evidence_kind", KIND)
            .where_eq("turn_id", key)
            .execute(db)
            .await?;
    }
    for (namespace, id) in keys(op) {
        // Sequential transitions in one native page can emit the same membership key.
        db.insert("turn_evidence")
            .value("turn_id", op_key(&namespace, &id))
            .value("evidence_kind", KIND)
            .value("segment_order", seq_to_value(event.sequence))
            .value("event_seq", seq_to_value(event.sequence))
            .execute(db)
            .await?;
    }
    Ok(true)
}
/// Compare the complete harness locator projection with current canonical events.
///
/// This is an explicit migration validation, not a normal read or repair path.
/// Expected keys use the same projector as ingestion, including v2 coverage and
/// attribution keys; historical observations never gain new terminal evidence.
///
/// # Errors
/// Returns an error for invalid canonical events, failed database reads, or any
/// missing, duplicate, corrupt, or unexpected harness projection locator.
pub(super) async fn validate_migration_keys(db: &dyn Database) -> SessionDbResult<()> {
    let events = super::strict_events(db).await?;
    super::validate_contiguous_canonical_events(&events)?;
    let mut expected: std::collections::BTreeSet<(String, u64, u64)> =
        std::collections::BTreeSet::new();
    for event in &events {
        if matches!(
            event.kind,
            SessionEventKind::ExternalHarnessOperation { .. }
        ) {
            let op = operation(event)?;
            let replaced: std::collections::BTreeSet<_> =
                replaced_execution_keys(op).into_iter().collect();
            expected.retain(|(key, _, _)| !replaced.contains(key));
            for (namespace, id) in keys(op) {
                expected.insert((op_key(&namespace, &id), event.sequence, event.sequence));
            }
        }
    }
    let rows = db
        .select("turn_evidence")
        .columns(&["turn_id", "segment_order", "event_seq"])
        .where_eq("evidence_kind", KIND)
        .execute(db)
        .await?;
    let mut actual = std::collections::BTreeSet::new();
    for row in &rows {
        let locator = (
            super::required_string(row, "turn_id")?,
            required_non_negative_u64(row, "segment_order")?,
            required_non_negative_u64(row, "event_seq")?,
        );
        if !actual.insert(locator) {
            return Err(SessionDbError::MigrationHistoryIncompatible {
                reason: "duplicate canonical harness projection locator".to_owned(),
            });
        }
    }
    if actual != expected {
        return Err(SessionDbError::MigrationHistoryIncompatible {
            reason: "canonical harness projection locators are missing, corrupt, or unexpected"
                .to_owned(),
        });
    }
    validate_migration_inventory_proofs(db, &events).await
}

async fn validate_migration_inventory_proofs(
    db: &dyn Database,
    events: &[SessionEvent],
) -> SessionDbResult<()> {
    let mut inventories = std::collections::BTreeMap::new();
    for event in events {
        if matches!(
            event.kind,
            SessionEventKind::ExternalHarnessOperation { .. }
        ) {
            let op = operation(event)?;
            for commitment in &op.inventory_commitments {
                if commitment.revision != event.sequence {
                    return Err(invalid());
                }
                inventories.insert(
                    commitment.snapshot_key.clone(),
                    (op.owner.clone(), commitment.clone()),
                );
            }
        }
    }
    for (namespace, (owner, canonical)) in inventories {
        let directory = namespace == execution_namespace(&owner, "inventory_directory_v1");
        let snapshot = db
            .select("snapshots")
            .columns(&["last_event_seq", "schema_version", "payload"])
            .where_eq("snapshot_name", namespace.clone())
            .execute_first(db)
            .await?
            .ok_or_else(evidence_maintenance_required)?;
        if required_non_negative_u64(&snapshot, "last_event_seq")? != canonical.revision
            || required_non_negative_u64(&snapshot, "schema_version")? != 2
        {
            return Err(invalid());
        }
        let commitment: HarnessInventoryCommitment =
            serde_json::from_str(&super::required_string(&snapshot, "payload")?)?;
        if commitment != canonical {
            return Err(invalid());
        }
        notification_validate_commitment(db, &owner, &namespace, &commitment).await?;
        let (_, gate) = inventory_read_gate(db, inventory_root_link(&commitment)?).await?;
        if let Some(gate) = gate {
            // Explicit maintenance validates every private descendant. Routine
            // summaries deliberately certify only accessed roots/proof paths.
            for (tree, reverse) in [(gate.reverse.clone(), true), (gate.eligible.clone(), false)] {
                let count = tree.as_ref().map_or(0, |link| link.summary.identities);
                let mut offset = 0;
                while offset < count {
                    let rows =
                        inventory_window(db, tree.clone(), offset, MAX_HARNESS_EXECUTION_ITEMS)
                            .await?;
                    if rows.is_empty() {
                        return Err(evidence_maintenance_required());
                    }
                    for (key, sequence) in &rows {
                        notification_event(db, &owner, key, *sequence).await?;
                        if !reverse {
                            notification_verify_source(db, &gate, key, *sequence).await?;
                            let source = inventory_find_raw(
                                db,
                                gate.source.clone(),
                                key,
                                &InventoryNodes::new(),
                            )
                            .await?
                            .ok_or_else(evidence_maintenance_required)?;
                            if source.0 != *sequence || source.1.obligations != 1 {
                                return Err(evidence_maintenance_required());
                            }
                        }
                    }
                    offset += rows.len() as u64;
                }
            }
        }
        let mut offset = 0;
        while offset < commitment.summary.identities {
            let rows = inventory_window(
                db,
                inventory_root_link(&commitment)?,
                offset,
                MAX_HARNESS_EXECUTION_ITEMS,
            )
            .await?;
            let expected =
                (commitment.summary.identities - offset).min(MAX_HARNESS_EXECUTION_ITEMS as u64);
            if rows.len() as u64 != expected {
                return Err(evidence_maintenance_required());
            }
            if !directory {
                verify_inventory_members(db, &rows).await?;
            }
            offset += rows.len() as u64;
        }
    }
    Ok(())
}

pub async fn verify_managed_tool_owner(
    db: &dyn Database,
    owner: &ExternalHarnessOwner,
) -> SessionDbResult<()> {
    state(db, owner).await?.ok_or_else(invalid).map(|_| ())
}

async fn canonical_harness_tail(
    db: &dyn Database,
    session: SessionId,
    before: Option<u64>,
) -> SessionDbResult<Option<SessionEvent>> {
    let mut query = db
        .select("events")
        .project_as(
            switchy::database::query::max(switchy::database::query::identifier("event_seq")),
            "event_seq",
        )
        .where_eq("event_type", "external_harness_operation");
    if let Some(before) = before {
        query = query.where_lt("event_seq", seq_to_value(before));
    }
    let row = query.execute_first(db).await?.ok_or_else(invalid)?;
    if matches!(
        row.get("event_seq"),
        Some(switchy::database::DatabaseValue::Null)
    ) {
        return Ok(None);
    }
    let sequence = required_non_negative_u64(&row, "event_seq")?;
    Ok(Some(
        inventory_canonical_event(db, session, sequence).await?,
    ))
}

async fn state(
    db: &dyn Database,
    owner: &ExternalHarnessOwner,
) -> SessionDbResult<Option<ExternalHarnessState>> {
    Box::pin(state_in_prefix(db, owner, None)).await
}

async fn state_in_prefix(
    db: &dyn Database,
    owner: &ExternalHarnessOwner,
    before: Option<u64>,
) -> SessionDbResult<Option<ExternalHarnessState>> {
    let Some(event) = latest(db, owner.session_id, "config", "current").await? else {
        if canonical_harness_tail(db, owner.session_id, before)
            .await?
            .is_some()
        {
            return Err(invalid());
        }
        return Ok(None);
    };
    let config_sequence = event.sequence;
    let config = operation(&event)?;
    let tail = canonical_harness_tail(db, owner.session_id, before)
        .await?
        .ok_or_else(invalid)?;
    if operation(&tail)?.owner != config.owner {
        // A stale/deleted config/current row cannot roll back a canonical transfer.
        return Err(invalid());
    }
    if &config.owner != owner {
        return Err(invalid());
    }
    let (instance, capabilities, open_request) = match &config.record {
        R::Configured {
            instance,
            capabilities,
            open_request,
        } => (instance.clone(), capabilities.clone(), open_request.clone()),
        R::RecoveryPrepared { recovery } => (
            recovery.release.instance.clone(),
            recovery.capabilities.clone(),
            Some(recovery.resume_request.clone()),
        ),
        _ => return Err(invalid()),
    };
    let mut result = ExternalHarnessState {
        owner: owner.clone(),
        instance,
        capabilities,
        binding: None,
        target: None,
        observation_after: None,
        prepared_open: open_request,
    };
    if let R::RecoveryPrepared { recovery } = &config.record {
        let ExternalHarnessRequestKind::Open(open) = &recovery.resume_request.kind else {
            return Err(invalid());
        };
        result.binding.clone_from(&open.resume);
    }
    if let Some(event) = latest(db, owner.session_id, "binding", "current").await? {
        let op = operation(&event)?;
        if &op.owner == owner {
            apply_binding_record(&mut result, &op.record)?;
        } else {
            let R::RecoveryPrepared { recovery } = &config.record else {
                return Err(invalid());
            };
            if op.owner != recovery.previous_owner || event.sequence >= config_sequence {
                return Err(invalid());
            }
            // The old binding is history only. Continue checking page ownership below,
            // rather than returning early and overlooking a corrupt current-page locator.
        }
    }
    if let Some(event) = latest(db, owner.session_id, "page", "current").await? {
        let page_operation = operation(&event)?;
        if &page_operation.owner != owner {
            let R::RecoveryPrepared { recovery } = &config.record else {
                return Err(invalid());
            };
            if page_operation.owner != recovery.previous_owner || event.sequence >= config_sequence
            {
                return Err(invalid());
            }
            return Ok(Some(result));
        }
        if let ExternalHarnessOperationRecord::ObservationPage { page, .. } = &page_operation.record
        {
            result.observation_after.clone_from(&page.next_after);
        }
    }
    Ok(Some(result))
}
fn apply_binding_record(result: &mut ExternalHarnessState, record: &R) -> SessionDbResult<()> {
    match record {
        R::DeliveryCompleted { response, .. } => {
            result.binding = match &response.kind {
                ExternalHarnessResponseKind::Selected {
                    target: bcode_harness_models::HarnessSessionTarget::Confirmed(binding),
                    ..
                }
                | ExternalHarnessResponseKind::Opened(binding)
                | ExternalHarnessResponseKind::Reconciled(
                    bcode_harness_models::HarnessReconciliation::Known { binding, .. },
                ) => Some(binding.clone()),
                ExternalHarnessResponseKind::Selected {
                    target: bcode_harness_models::HarnessSessionTarget::Prepared(_),
                    ..
                }
                | ExternalHarnessResponseKind::Prepared(_)
                | ExternalHarnessResponseKind::Reconciled(
                    bcode_harness_models::HarnessReconciliation::KnownPrepared { .. },
                ) => None,
                _ => return Err(invalid()),
            };
            result.target = match &response.kind {
                ExternalHarnessResponseKind::Selected { target, .. } => Some(target.clone()),
                ExternalHarnessResponseKind::Prepared(prepared)
                | ExternalHarnessResponseKind::Reconciled(
                    bcode_harness_models::HarnessReconciliation::KnownPrepared { prepared, .. },
                ) => Some(bcode_harness_models::HarnessSessionTarget::Prepared(
                    prepared.clone(),
                )),
                _ => result
                    .binding
                    .clone()
                    .map(bcode_harness_models::HarnessSessionTarget::Confirmed),
            };
        }
        R::ObservationPage { page, .. } => {
            let binding = page
                .observations
                .iter()
                .find_map(|observation| match &observation.kind {
                    HarnessObservationKind::BindingConfirmed { binding, .. } => {
                        Some(binding.clone())
                    }
                    _ => None,
                })
                .ok_or_else(invalid)?;
            result.binding = Some(binding.clone());
            result.target = Some(bcode_harness_models::HarnessSessionTarget::Confirmed(
                binding,
            ));
        }
        R::Configured { .. }
        | R::TransportReleased { .. }
        | R::RecoveryPrepared { .. }
        | R::DeliveryClaimed { .. } => return Err(invalid()),
    }
    Ok(())
}

pub(super) async fn validate(db: &dyn Database, event: &SessionEvent) -> SessionDbResult<()> {
    if !matches!(
        event.kind,
        SessionEventKind::ExternalHarnessOperation { .. }
    ) {
        return Ok(());
    }
    super::external_backend::external_index_ready(db).await?;
    Box::pin(validate_operation(db, event)).await
}

async fn validate_operation(db: &dyn Database, event: &SessionEvent) -> SessionDbResult<()> {
    let op = operation(event)?;
    verify_operation_writer(db, op).await?;
    let (expected, _) = prepare_inventory_commitments(db, op, event.sequence).await?;
    if op.inventory_commitments != expected {
        return Err(invalid());
    }
    if matches!(
        op.record,
        R::TransportReleased { .. } | R::RecoveryPrepared { .. }
    ) {
        return Err(invalid());
    }
    if let Some(tail) =
        canonical_harness_tail(db, op.owner.session_id, Some(event.sequence)).await?
    {
        let previous = operation(&tail)?;
        if previous.owner == op.owner && matches!(previous.record, R::TransportReleased { .. }) {
            return Err(invalid());
        }
    }
    let current = state_in_prefix(db, &op.owner, Some(event.sequence)).await?;
    if latest(
        db,
        op.owner.session_id,
        "transport_release",
        &format!("generation:{}", op.owner.generation),
    )
    .await?
    .is_some()
    {
        return Err(invalid());
    }
    if let R::Configured {
        instance,
        capabilities,
        open_request,
    } = &op.record
    {
        if op.owner.generation != 1
            || current.as_ref().is_some_and(|v| {
                &v.instance != instance
                    || &v.capabilities != capabilities
                    || v.prepared_open.as_ref() != open_request.as_ref()
            })
        {
            return Err(invalid());
        }
        return Ok(());
    }
    let current = current.ok_or_else(invalid)?;
    match &op.record {
        R::DeliveryClaimed { request } => {
            validate_claim(db, op, &current, request).await?;
        }
        R::DeliveryCompleted { request, response } => {
            validate_completion(db, op, &current, request, response).await?;
        }
        R::ObservationPage {
            after,
            page,
            instance,
        } => {
            validate_page(db, op, &current, after.as_deref(), page, instance).await?;
        }
        R::Configured { .. } | R::TransportReleased { .. } | R::RecoveryPrepared { .. } => {}
    }
    Ok(())
}
async fn interaction(
    db: &dyn Database,
    session: SessionId,
    id: &str,
) -> SessionDbResult<Option<HarnessInteraction>> {
    let Some(event) = latest(db, session, "interaction", id).await? else {
        return Ok(None);
    };
    let observed = operation(&event)?;
    let Some(configured) = latest(db, session, "config", "current").await? else {
        return Err(invalid());
    };
    if observed.owner != operation(&configured)?.owner {
        return Ok(None);
    }
    let ExternalHarnessOperationRecord::ObservationPage { page, .. } = &observed.record else {
        return Err(invalid());
    };
    Ok(page.observations.iter().find_map(|v| match &v.kind {
        HarnessObservationKind::Interaction { interaction } if interaction.request_id == id => {
            Some(interaction.clone())
        }
        _ => None,
    }))
}
fn submission_native_frame(kind: &HarnessObservationKind) -> Option<(&str, &str, &str)> {
    match kind {
        HarnessObservationKind::SubmissionConsumed {
            submission_id,
            attempt_id,
            native_frame_id,
        }
        | HarnessObservationKind::SubmissionResultObserved {
            submission_id,
            attempt_id,
            native_frame_id,
            ..
        } => Some((submission_id, attempt_id, native_frame_id)),
        _ => None,
    }
}

fn submission_frame_key(generation: u64, kind: &HarnessObservationKind) -> Option<String> {
    let (submission, attempt, frame) = submission_native_frame(kind)?;
    let stage = match kind {
        HarnessObservationKind::SubmissionConsumed { .. } => "consumed",
        HarnessObservationKind::SubmissionResultObserved { .. } => "result_observed",
        _ => return None,
    };
    Some(submission_key(
        &generation.to_string(),
        &submission_key(
            stage,
            &submission_key(frame, &submission_key(submission, attempt)),
        ),
    ))
}

fn observation_gap(observation: &bcode_harness_models::HarnessObservation) -> bool {
    observation.completeness == HarnessCompleteness::Gap
        || matches!(&observation.kind, HarnessObservationKind::Status { status }
            if status.completeness == HarnessCompleteness::Gap)
}

fn evidence_position(
    event: &SessionEvent,
    predicate: impl Fn(&bcode_harness_models::HarnessObservation) -> bool,
) -> SessionDbResult<(usize, bcode_harness_models::HarnessObservation)> {
    let R::ObservationPage { page, .. } = &operation(event)?.record else {
        return Err(invalid());
    };
    let mut matches = page
        .observations
        .iter()
        .enumerate()
        .filter(|(_, v)| predicate(v));
    let (offset, observation) = matches.next().ok_or_else(invalid)?;
    if matches.next().is_some() {
        return Err(invalid());
    }
    Ok((offset, observation.clone()))
}

async fn validate_evidence_coverage(
    db: &dyn Database,
    session: SessionId,
    owner: &ExternalHarnessOwner,
    accepted: &SessionEvent,
) -> SessionDbResult<()> {
    let coverage = latest(db, session, "terminal_coverage_v2", "current")
        .await?
        .ok_or_else(evidence_maintenance_required)?;
    let tail = latest(db, session, "page", "current")
        .await?
        .ok_or_else(invalid)?;
    if coverage.sequence != tail.sequence || operation(&coverage)?.owner != *owner {
        return Err(evidence_maintenance_required());
    }
    let covered_acceptance = db
        .select("turn_evidence")
        .columns(&["event_seq"])
        .where_eq("turn_id", op_key("terminal_coverage_v2", "current"))
        .where_eq("evidence_kind", KIND)
        .where_eq("event_seq", seq_to_value(accepted.sequence))
        .limit(1)
        .execute(db)
        .await?;
    if covered_acceptance.is_empty() {
        return Err(evidence_maintenance_required());
    }
    Ok(())
}

async fn terminal_observations(
    db: &dyn Database,
    owner: &ExternalHarnessOwner,
    native_turn_id: &str,
    accepted_position: (u64, usize),
) -> SessionDbResult<Vec<bcode_harness_models::HarnessObservation>> {
    let mut terminal = Vec::new();
    for namespace in ["turn_result", "native_terminal", "settled"] {
        if let Some(event) = latest(db, owner.session_id, namespace, native_turn_id).await? {
            if operation(&event)?.owner != *owner {
                return Err(invalid());
            }
            let (position, observation) = evidence_position(&event, |v| match &v.kind {
                HarnessObservationKind::Result { .. } => {
                    namespace == "turn_result"
                        && v.native_correlations.iter().any(|id| id == native_turn_id)
                }
                HarnessObservationKind::NativeTurnTerminal {
                    native_turn_id: id, ..
                } => namespace == "native_terminal" && id == native_turn_id,
                HarnessObservationKind::Settlement { native_turn_id: id } => {
                    namespace == "settled" && id == native_turn_id
                }
                _ => false,
            })?;
            if (event.sequence, position) <= accepted_position {
                return Err(invalid());
            }
            terminal.push(((event.sequence, position), observation));
        }
    }
    terminal.sort_by_key(|(position, _)| *position);
    Ok(terminal
        .into_iter()
        .map(|(_, observation)| observation)
        .collect())
}

async fn validate_accepted_submission_claim(
    db: &dyn Database,
    owner: &ExternalHarnessOwner,
    accepted: &SessionEvent,
    submission_id: &str,
    attempt_id: &str,
) -> SessionDbResult<()> {
    let claim = latest(
        db,
        owner.session_id,
        "submission_claim",
        &submission_key(submission_id, attempt_id),
    )
    .await?
    .ok_or_else(invalid)?;
    let claim_op = operation(&claim)?;
    let R::DeliveryClaimed { request } = &claim_op.record else {
        return Err(invalid());
    };
    if claim_op.owner != *owner
        || claim.sequence >= accepted.sequence
        || !matches!(&request.kind, ExternalHarnessRequestKind::Submit { submission, .. }
            if submission.submission_id == submission_id
                && submission.attempt_id == attempt_id
                && submission.generation == owner.generation)
    {
        return Err(invalid());
    }
    Ok(())
}

/// Fetch the latest version of each retained lifecycle identity, not the first N pages.
async fn inventory_relevant_window(
    db: &dyn Database,
    root: Option<InventoryLink>,
) -> SessionDbResult<Vec<(String, u64)>> {
    let root = inventory_read_gate(db, root).await?.0;
    let mut stack = root
        .into_iter()
        .map(|root| (root, None))
        .collect::<Vec<_>>();
    let mut result = Vec::new();
    while let Some((link, parent_bit)) = stack.pop() {
        if link.summary.unfinished == 0
            && link.summary.obligations == 0
            && link.summary.failed == 0
            && link.summary.interrupted == 0
        {
            continue;
        }
        match inventory_load_node(db, &link, &InventoryNodes::new()).await? {
            InventoryNode::NotificationGateV1 { .. } => return Err(evidence_maintenance_required()),
            InventoryNode::Leaf { key, sequence, .. } => {
                result.push((key, sequence));
                if result.len() > MAX_HARNESS_EXECUTION_ITEMS {
                    return Err(evidence_maintenance_required());
                }
            }
            InventoryNode::Branch {
                bit, left, right, ..
            } => {
                if parent_bit.is_some_and(|prior| bit <= prior) {
                    return Err(invalid());
                }
                stack.push((right, Some(bit)));
                stack.push((left, Some(bit)));
            }
        }
    }
    Ok(result)
}

async fn verify_inventory(
    db: &dyn Database,
    owner: &ExternalHarnessOwner,
    namespace: &str,
) -> SessionDbResult<Vec<(String, u64)>> {
    let Some(commitment) = inventory_current_commitment(db, owner, namespace, None).await? else {
        if latest(db, owner.session_id, "page", "current")
            .await?
            .is_some()
            && namespace.contains(":inventory:unknown:")
        {
            return Err(evidence_maintenance_required());
        }
        return Ok(Vec::new());
    };
    notification_validate_commitment(db, owner, namespace, &commitment).await?;
    if namespace.contains(":inventory:unknown:") {
        let rows = inventory_relevant_window(db, inventory_root_link(&commitment)?).await?;
        verify_inventory_members(db, &rows).await?;
        return Ok(rows);
    }
    if commitment.summary.identities > MAX_HARNESS_EXECUTION_ITEMS as u64 {
        return Err(SessionDbError::InvalidData(
            "inventory requires revision-pinned pagination".into(),
        ));
    }
    let rows = inventory_window(
        db,
        inventory_root_link(&commitment)?,
        0,
        MAX_HARNESS_EXECUTION_ITEMS,
    )
    .await?;
    if rows.len() as u64 != commitment.summary.identities {
        return Err(evidence_maintenance_required());
    }
    verify_inventory_members(db, &rows).await?;
    Ok(rows)
}
async fn verify_inventory_members(
    db: &dyn Database,
    rows: &[(String, u64)],
) -> SessionDbResult<()> {
    for (key, sequence) in rows {
        let retained = db
            .select("turn_evidence")
            .columns(&["event_seq"])
            .where_eq("turn_id", key.clone())
            .where_eq("evidence_kind", KIND)
            .where_eq("event_seq", seq_to_value(*sequence))
            .execute_first(db)
            .await?;
        if retained.is_none() {
            return Err(evidence_maintenance_required());
        }
    }
    Ok(())
}

async fn execution_inventory(
    db: &dyn Database,
    owner: &ExternalHarnessOwner,
    scope: &str,
    kind: &str,
) -> SessionDbResult<Vec<SessionEvent>> {
    let namespace = execution_namespace(owner, &format!("inventory:{scope}:{kind}"));
    let prefix = op_key(&namespace, "");
    let rows = verify_inventory(db, owner, &namespace).await?;
    let mut result = Vec::new();
    for (key, sequence) in rows {
        let id = key.strip_prefix(&prefix).ok_or_else(invalid)?;
        let event = latest(db, owner.session_id, &execution_namespace(owner, kind), id)
            .await?
            .ok_or_else(evidence_maintenance_required)?;
        if operation(&event)?.owner != *owner || event.sequence != sequence {
            return Err(invalid());
        }
        let observation = execution_observation(&event, kind, id)?;
        let HarnessObservationKind::Execution { event: execution } = &observation.kind else {
            return Err(invalid());
        };
        let actual_scope = execution_origin(execution).map_or_else(
            || "unknown".to_owned(),
            |attempt| {
                format!(
                    "attempt:{}",
                    submission_key(&attempt.submission_id, &attempt.attempt_id)
                )
            },
        );
        if actual_scope != scope {
            return Err(invalid());
        }
        result.push(event);
    }
    Ok(result)
}

async fn set_execution_attention(
    db: &dyn Database,
    owner: &ExternalHarnessOwner,
    attempt: &HarnessAttemptIdentity,
    tail: &SessionEvent,
    page: &mut HarnessObservationPage,
) -> SessionDbResult<()> {
    let attention_namespace = execution_namespace(owner, "inventory:unknown:interaction");
    for kind in ["terminal", "coverage"] {
        let id = submission_key(&attempt.submission_id, &attempt.attempt_id);
        if latest(db, owner.session_id, &execution_namespace(owner, kind), &id)
            .await?
            .is_some()
        {
            let namespace = execution_namespace(owner, &format!("inventory:attempt:{id}:{kind}"));
            if latest(
                db,
                owner.session_id,
                "execution_inventory_proof",
                &namespace,
            )
            .await?
            .is_none()
            {
                return Err(evidence_maintenance_required());
            }
        }
    }
    let pending = verify_inventory(db, owner, &attention_namespace).await?;
    let attention_marker = latest(
        db,
        owner.session_id,
        "execution_inventory_proof",
        &attention_namespace,
    )
    .await?;
    page.status.attention = if attention_marker.is_none() {
        HarnessAttention::Unknown
    } else if pending.is_empty() {
        let R::ObservationPage { page: current, .. } = &operation(tail)?.record else {
            return Err(invalid());
        };
        match current.status.attention {
            HarnessAttention::Unknown => HarnessAttention::Unknown,
            HarnessAttention::Authentication => HarnessAttention::Authentication,
            _ => HarnessAttention::None,
        }
    } else {
        // Preserve exact canonical attention when there is an unanswered interaction;
        // never infer None from idle activity or an empty native poll.
        let R::ObservationPage { page: current, .. } = &operation(tail)?.record else {
            return Err(invalid());
        };
        if current.status.attention == HarnessAttention::None {
            HarnessAttention::Unknown
        } else {
            current.status.attention
        }
    };
    Ok(())
}

async fn order_execution_evidence(
    db: &dyn Database,
    owner: &ExternalHarnessOwner,
    key: &str,
    page: &mut HarnessObservationPage,
) -> SessionDbResult<()> {
    let mut ordered = std::collections::BTreeMap::new();
    for observation in &page.observations {
        let source = latest(
            db,
            owner.session_id,
            "observation",
            &observation.observation_id,
        )
        .await?
        .ok_or_else(evidence_maintenance_required)?;
        let (offset, canonical) =
            evidence_position(&source, |v| v.observation_id == observation.observation_id)?;
        if canonical != *observation {
            return Err(invalid());
        }
        ordered.insert((source.sequence, offset), canonical);
    }
    page.observations = ordered.into_values().collect();
    if let Some(claim) = latest(db, owner.session_id, "submission_claim", key).await?
        && let Some(gap) = latest(
            db,
            owner.session_id,
            "evidence_gap",
            &owner.generation.to_string(),
        )
        .await?
        && gap.sequence >= claim.sequence
    {
        if operation(&gap)?.owner != *owner {
            return Err(invalid());
        }
        page.completeness = HarnessCompleteness::Gap;
    }
    Ok(())
}

fn execution_evidence_completeness<'a>(
    observations: impl Iterator<Item = &'a bcode_harness_models::HarnessObservation>,
    current: HarnessCompleteness,
) -> HarnessCompleteness {
    let mut has_terminal = false;
    let mut complete = true;
    for observation in observations {
        has_terminal |= matches!(
            &observation.kind,
            HarnessObservationKind::Execution {
                event: HarnessExecutionEvent::SubmissionTerminal { .. }
            }
        );
        complete &= observation.completeness == HarnessCompleteness::Complete;
    }
    if !has_terminal || current == HarnessCompleteness::Gap {
        return current;
    }
    if complete {
        HarnessCompleteness::Complete
    } else {
        HarnessCompleteness::Partial
    }
}

async fn append_execution_evidence(
    db: &dyn Database,
    owner: &ExternalHarnessOwner,
    attempt: &HarnessAttemptIdentity,
    page: &mut HarnessObservationPage,
) -> SessionDbResult<()> {
    let Some(marker) = latest(db, owner.session_id, "execution_coverage_v3", "current").await?
    else {
        // Pages from an older projector cannot assert that v3 evidence is absent.
        if latest(db, owner.session_id, "page", "current")
            .await?
            .is_some()
        {
            return Err(evidence_maintenance_required());
        }
        return Ok(());
    };
    let tail = latest(db, owner.session_id, "page", "current")
        .await?
        .ok_or_else(invalid)?;
    if marker.sequence != tail.sequence || operation(&marker)?.owner != *owner {
        return Err(evidence_maintenance_required());
    }
    set_execution_attention(db, owner, attempt, &tail, page).await?;
    let key = submission_key(&attempt.submission_id, &attempt.attempt_id);
    let scope = format!("attempt:{key}");
    let mut events = std::collections::BTreeMap::new();
    for kind in ["terminal", "coverage", "work", "continuation"] {
        let unknown_scope = "unknown".to_owned();
        for scope in [&scope, &unknown_scope] {
            for event in execution_inventory(db, owner, scope, kind).await? {
                events.insert(event.sequence, event);
            }
        }
    }
    let mut retained = std::collections::BTreeMap::new();
    if events.len() > MAX_HARNESS_EXECUTION_ITEMS {
        return Err(evidence_maintenance_required());
    }
    for event in events.values() {
        let R::ObservationPage { page: source, .. } = &operation(event)?.record else {
            return Err(invalid());
        };
        for (offset, observation) in source.observations.iter().enumerate() {
            let HarnessObservationKind::Execution { event: execution } = &observation.kind else {
                continue;
            };
            if execution_origin(execution).is_some_and(|origin| origin != attempt) {
                continue;
            }
            let (kind, id) = execution_identity(execution);
            let latest = latest(db, owner.session_id, &execution_namespace(owner, kind), &id)
                .await?
                .ok_or_else(evidence_maintenance_required)?;
            if latest.sequence == event.sequence {
                if latest.sequence == event.sequence
                    && execution_scope(execution) == "unknown"
                    && execution_retired(execution)
                {
                    continue;
                }
                retained.insert(
                    (kind.to_owned(), id),
                    (event.sequence, offset, observation.clone()),
                );
            }
        }
    }
    if retained.is_empty() {
        return Ok(());
    }
    validate_execution_attempt(db, owner, attempt).await?;
    if retained.len() > MAX_HARNESS_EXECUTION_ITEMS
        || retained.len() + page.observations.len() > MAX_HARNESS_OBSERVATIONS
    {
        return Err(evidence_maintenance_required());
    }
    page.completeness = execution_evidence_completeness(
        retained.values().map(|(_, _, observation)| observation),
        page.completeness,
    );
    page.observations.extend(
        retained
            .into_values()
            .map(|(_, _, observation)| observation),
    );
    order_execution_evidence(db, owner, &key, page).await?;
    Ok(())
}

async fn append_legacy_submission_evidence(
    db: &dyn Database,
    owner: &ExternalHarnessOwner,
    submission_id: &str,
    attempt_id: &str,
    page: &mut HarnessObservationPage,
) -> SessionDbResult<()> {
    if let Some(accepted) = latest(
        db,
        owner.session_id,
        "submission_acceptance",
        &submission_key(submission_id, attempt_id),
    )
    .await?
    {
        if operation(&accepted)?.owner != *owner {
            return Err(invalid());
        }
        let (offset, acceptance) = evidence_position(&accepted, |v| {
            matches!(&v.kind, HarnessObservationKind::SubmissionAccepted {
                    submission_id: id, attempt_id: attempt, ..
                } if id == submission_id && attempt == attempt_id)
        })?;
        let HarnessObservationKind::SubmissionAccepted { native_turn_id, .. } = &acceptance.kind
        else {
            return Err(invalid());
        };
        // Coverage is canonical-event checked at both ends. Older projections cannot
        // silently advertise absence of newly indexed result or loss evidence.
        validate_evidence_coverage(db, owner.session_id, owner, &accepted).await?;
        validate_accepted_submission_claim(db, owner, &accepted, submission_id, attempt_id).await?;
        let terminal =
            terminal_observations(db, owner, native_turn_id, (accepted.sequence, offset)).await?;
        page.next_after = Some(format!("h1:{}:{}", accepted.sequence, offset + 1));
        page.completeness = if acceptance.completeness == HarnessCompleteness::Complete
            && terminal.len() == 3
            && terminal
                .iter()
                .all(|v| v.completeness == HarnessCompleteness::Complete)
        {
            HarnessCompleteness::Complete
        } else {
            HarnessCompleteness::Partial
        };
        page.observations.push(acceptance);
        page.observations.extend(terminal);
        if let Some(gap) = latest(
            db,
            owner.session_id,
            "evidence_gap",
            &owner.generation.to_string(),
        )
        .await?
        {
            if operation(&gap)?.owner != *owner {
                return Err(invalid());
            }
            if gap.sequence >= accepted.sequence {
                page.completeness = HarnessCompleteness::Gap;
            }
        }
    }
    Ok(())
}

impl SessionDb {
    /// Explicitly rebuild managed-harness evidence using the current typed event projector.
    ///
    /// Caller must hold exclusive session maintenance and write guards. This is a maintenance
    /// capability, never a normal read/open/append fallback. Canonical events and unrelated
    /// evidence are unchanged; all harness locators (including coverage) are replaced atomically
    /// at their original event sequences. The migration domain owns version classification,
    /// backup/recovery and writer/schema-fence advancement after successful target validation.
    ///
    /// # Errors
    /// Returns an error for invalid canonical history or typed harness events, database failure,
    /// or transaction failure. A failed rebuild does not publish partial coverage.
    pub async fn reindex_external_harness_evidence(
        &self,
        _maintenance: &crate::lease::SessionMaintenanceGuard,
        _write: &crate::lease::SessionWriteGuard,
    ) -> SessionDbResult<usize> {
        let tx = self.db.begin_transaction().await?;
        super::configure_turso_connection(&*tx).await?;
        let events = super::strict_events(&*tx).await?;
        super::validate_contiguous_canonical_events(&events)?;
        // Validate typed canonical payloads before replacing any derived rows. The existing
        // projector owns every namespace and encoding; maintenance must not duplicate them.
        for event in &events {
            if event.session_id != self.session_id {
                return Err(invalid());
            }
            if matches!(
                event.kind,
                SessionEventKind::ExternalHarnessOperation { .. }
            ) {
                operation(event)?;
            }
        }
        let mut inventories = std::collections::BTreeSet::new();
        for event in &events {
            if matches!(
                event.kind,
                SessionEventKind::ExternalHarnessOperation { .. }
            ) {
                inventories.extend(inventory_namespaces(operation(event)?));
            }
        }
        for namespace in inventories {
            tx.delete("snapshots")
                .where_eq("snapshot_name", namespace)
                .execute(&*tx)
                .await?;
        }
        // Explicit maintenance only: discard derived immutable nodes before deterministic
        // reconstruction. Ordinary reads/appends never delete or repair damaged proof nodes.
        tx.delete("snapshots")
            .where_gte("snapshot_name", "harness_inventory_node_v1:")
            .where_lt("snapshot_name", "harness_inventory_node_v1;")
            .execute(&*tx)
            .await?;
        tx.delete("turn_evidence")
            .where_eq("evidence_kind", KIND)
            .execute(&*tx)
            .await?;
        let mut projected = 0;
        for event in &events {
            if matches!(
                event.kind,
                SessionEventKind::ExternalHarnessOperation { .. }
            ) {
                validate_operation(&*tx, event).await?;
                project(&*tx, event).await?;
                projected += 1;
            }
        }
        tx.commit().await?;
        Ok(projected)
    }

    /// Bounded exact submission evidence from canonical indexed events, never a history scan.
    ///
    /// The tuple carries the current owner and legacy acceptance/result/native-terminal/
    /// settlement plus the latest bounded v3 lifecycle entries for the exact attempt and
    /// uncorrelated session-wide work. All observations retain canonical sequence/page-offset
    /// order. Completeness describes evidence, not successful outcome or workflow completion.
    /// `next_after` remains the acceptance cursor, not an execution inventory paging cursor.
    /// Gap fences loss since the retained claim. None means no configured harness. Missing
    /// projector coverage or overflowing inventory requires explicit maintenance/degraded handling.
    /// # Errors
    /// Rejects invalid identities, foreign/conflicting evidence, stale indexes and mixed snapshots.
    pub async fn external_harness_submission_evidence(
        &self,
        submission_id: &str,
        attempt_id: &str,
    ) -> SessionDbResult<Option<(ExternalHarnessOwner, HarnessObservationPage)>> {
        if [submission_id, attempt_id].iter().any(|id| {
            id.is_empty()
                || id.len() > bcode_harness_models::MAX_HARNESS_ID_BYTES
                || id.chars().any(char::is_control)
        }) {
            return Err(invalid());
        }
        let db = &**self.db;
        super::external_backend::external_index_ready(db).await?;
        let before = last_sequence(db).await?;
        let Some(config) = latest(db, self.session_id, "config", "current").await? else {
            super::external_backend::external_index_ready(db).await?;
            if last_sequence(db).await? != before {
                return Err(invalid());
            }
            return Ok(None);
        };
        let owner = operation(&config)?.owner.clone();
        state(db, &owner).await?.ok_or_else(invalid)?;
        let mut page = HarnessObservationPage {
            retention: None,
            hydration_requests: Vec::new(),
            inventory_page: None,
            observations: Vec::new(),
            next_after: None,
            completeness: HarnessCompleteness::Partial,
            status: HarnessExecutionStatus {
                connectivity: HarnessConnectivity::Unknown,
                activity: HarnessActivity::Unknown,
                attention: HarnessAttention::Unknown,
                continuation: HarnessContinuation::Unknown,
                completeness: HarnessCompleteness::Partial,
            },
        };
        append_legacy_submission_evidence(db, &owner, submission_id, attempt_id, &mut page).await?;
        append_execution_evidence(
            db,
            &owner,
            &HarnessAttemptIdentity {
                submission_id: submission_id.to_owned(),
                attempt_id: attempt_id.to_owned(),
            },
            &mut page,
        )
        .await?;
        page.status.completeness = page.completeness;
        if serde_json::to_vec(&page)?.len() > MAX_HARNESS_WIRE_BYTES {
            return Err(invalid());
        }
        super::external_backend::external_index_ready(db).await?;
        if last_sequence(db).await? != before {
            return Err(invalid());
        }
        Ok(Some((owner, page)))
    }

    /// Exact canonical submission-to-native-turn acceptance, independent of native request IDs.
    /// # Errors
    /// Rejects malformed identities, stale indexes, inconsistent evidence and concurrent changes.
    pub async fn external_harness_submission_acceptance(
        &self,
        submission_id: &str,
        attempt_id: &str,
    ) -> SessionDbResult<Option<bcode_harness_models::HarnessObservation>> {
        if [submission_id, attempt_id].iter().any(|id| {
            id.is_empty()
                || id.len() > bcode_harness_models::MAX_HARNESS_ID_BYTES
                || id.chars().any(char::is_control)
        }) {
            return Err(invalid());
        }
        super::external_backend::external_index_ready(&**self.db).await?;
        let before = last_sequence(&**self.db).await?;
        let result = if let Some(event) = latest(
            &**self.db,
            self.session_id,
            "submission_acceptance",
            &submission_key(submission_id, attempt_id),
        )
        .await?
        {
            let op = operation(&event)?;
            let R::ObservationPage { page, .. } = &op.record else {
                return Err(invalid());
            };
            let mut matches=page.observations.iter().filter(|v|matches!(&v.kind,HarnessObservationKind::SubmissionAccepted { submission_id: id,attempt_id: attempt,.. } if id==submission_id&&attempt==attempt_id));
            let observation = matches.next().ok_or_else(invalid)?;
            if matches.next().is_some() {
                return Err(invalid());
            }
            let claim = latest(
                &**self.db,
                self.session_id,
                "submission_claim",
                &submission_key(submission_id, attempt_id),
            )
            .await?
            .ok_or_else(invalid)?;
            if operation(&claim)?.owner != op.owner || claim.sequence >= event.sequence {
                return Err(invalid());
            }
            Some(observation.clone())
        } else {
            None
        };
        super::external_backend::external_index_ready(&**self.db).await?;
        if last_sequence(&**self.db).await? != before {
            return Err(invalid());
        }
        Ok(result)
    }
    /// Read exact canonical selected-open locators and result, without delivery authority.
    /// # Errors
    /// Rejects conflicting requests, stale locators, wrong owners and concurrent appends.
    pub async fn external_harness_selected_open_receipt(
        &self,
        request: &ExternalHarnessRequest,
    ) -> SessionDbResult<Option<bcode_session_models::ManagedSessionSpawnOpenReceipt>> {
        if !request.is_selected_open() {
            return Err(invalid());
        }
        let before = last_sequence(&**self.db).await?;
        let Some(bcode_session_models::ExternalHarnessClaim::Recorded(response)) =
            self.external_harness_retained_request(request).await?
        else {
            return Ok(None);
        };
        let config = latest(&**self.db, self.session_id, "config", "current")
            .await?
            .ok_or_else(invalid)?;
        let claim = latest(&**self.db, self.session_id, "claim", &request.request_id)
            .await?
            .ok_or_else(invalid)?;
        let completed = latest(
            &**self.db,
            self.session_id,
            "completion",
            &request.request_id,
        )
        .await?
        .ok_or_else(invalid)?;
        let configured = operation(&config)?;
        if !matches!(&configured.record, R::Configured { open_request: Some(open), .. } if open == request)
            || operation(&claim)?.owner != configured.owner
            || operation(&completed)?.owner != configured.owner
            || config.sequence >= claim.sequence
            || claim.sequence >= completed.sequence
            || last_sequence(&**self.db).await? != before
        {
            return Err(invalid());
        }
        Ok(Some(bcode_session_models::ManagedSessionSpawnOpenReceipt {
            owner: configured.owner.clone(),
            request: request.clone(),
            response: *response,
            configured_event_sequence: config.sequence,
            claimed_event_sequence: claim.sequence,
            completed_event_sequence: completed.sequence,
        }))
    }

    /// Compare a complete retained request without adopting its historical execution owner.
    /// # Errors
    /// Rejects conflicting content, stale locators, malformed evidence and concurrent appends.
    pub async fn external_harness_retained_request(
        &self,
        request: &ExternalHarnessRequest,
    ) -> SessionDbResult<Option<bcode_session_models::ExternalHarnessClaim>> {
        use bcode_session_models::ExternalHarnessClaim;
        request.validate().map_err(|_| invalid())?;
        if request.target().0 != self.session_id.to_string() {
            return Err(invalid());
        }
        super::external_backend::external_index_ready(&**self.db).await?;
        let before = last_sequence(&**self.db).await?;
        let claim = latest(&**self.db, self.session_id, "claim", &request.request_id).await?;
        let result = if let Some(claim) = claim {
            let retained = operation(&claim)?;
            if !matches!(&retained.record, R::DeliveryClaimed { request: original } if original == request)
            {
                return Err(invalid());
            }
            if let Some(completed) = latest(
                &**self.db,
                self.session_id,
                "completion",
                &request.request_id,
            )
            .await?
            {
                let completion = operation(&completed)?;
                let R::DeliveryCompleted {
                    request: original,
                    response,
                } = &completion.record
                else {
                    return Err(invalid());
                };
                if original != request
                    || completion.owner != retained.owner
                    || completed.sequence <= claim.sequence
                {
                    return Err(invalid());
                }
                Some(ExternalHarnessClaim::Recorded(Box::new(response.clone())))
            } else {
                Some(ExternalHarnessClaim::Uncertain)
            }
        } else {
            // A new request ID cannot bypass an existing mutation's uncertainty.
            if latest(
                &**self.db,
                self.session_id,
                "mutation",
                &mutation_key(request),
            )
            .await?
            .is_some()
                || latest(
                    &**self.db,
                    self.session_id,
                    "completion",
                    &request.request_id,
                )
                .await?
                .is_some()
            {
                return Err(invalid());
            }
            None
        };
        super::external_backend::external_index_ready(&**self.db).await?;
        if last_sequence(&**self.db).await? != before {
            return Err(invalid());
        }
        Ok(result)
    }
    /// Bounded canonical snapshot without owner admission, plugin access or recovery.
    /// # Errors
    /// Rejects stale/missing locators, incompatible storage and concurrent snapshot changes.
    pub async fn external_harness_snapshot(&self) -> SessionDbResult<Option<ExternalHarnessState>> {
        super::external_backend::external_index_ready(&**self.db).await?;
        let before = last_sequence(&**self.db).await?;
        let result =
            if let Some(event) = latest(&**self.db, self.session_id, "config", "current").await? {
                let owner = operation(&event)?.owner.clone();
                state(&**self.db, &owner).await?
            } else {
                None
            };
        // Queries are exact-key and bounded. A concurrent append invalidates this read;
        // never return a mixed snapshot or retry/replay an unbounded history.
        super::external_backend::external_index_ready(&**self.db).await?;
        if last_sequence(&**self.db).await? != before {
            return Err(invalid());
        }
        Ok(result)
    }
    /// Read the exact current-generation transport release from canonical indexed evidence.
    /// This is observation only, never proof that its daemon lifetime is released.
    /// # Errors
    /// Rejects stale owners, missing release locators at a released canonical tail,
    /// incompatible indexes and concurrent changes; never scans or repairs history.
    pub(crate) async fn harness_transport_release(
        &self,
        owner: &ExternalHarnessOwner,
    ) -> SessionDbResult<
        Option<(
            u64,
            bcode_session_models::external_harness::ExternalHarnessTransportRelease,
        )>,
    > {
        super::external_backend::external_index_ready(&**self.db).await?;
        let before = last_sequence(&**self.db).await?;
        state(&**self.db, owner).await?.ok_or_else(invalid)?;
        let tail = canonical_harness_tail(&**self.db, owner.session_id, None)
            .await?
            .ok_or_else(invalid)?;
        let tail_op = operation(&tail)?;
        let result = if let R::TransportReleased { release } = &tail_op.record {
            if tail_op.owner != *owner {
                return Err(invalid());
            }
            let event = latest(
                &**self.db,
                owner.session_id,
                "transport_release",
                &format!("generation:{}", owner.generation),
            )
            .await?
            .ok_or_else(invalid)?;
            if event != tail {
                return Err(invalid());
            }
            Some((tail.sequence, release.clone()))
        } else {
            None
        };
        super::external_backend::external_index_ready(&**self.db).await?;
        if last_sequence(&**self.db).await? != before {
            return Err(invalid());
        }
        Ok(result)
    }

    pub(crate) async fn harness_binding_event(&self) -> SessionDbResult<Option<SessionEvent>> {
        super::external_backend::external_index_ready(&**self.db).await?;
        let Some(event) = latest(&**self.db, self.session_id, "binding", "current").await? else {
            return Ok(None);
        };
        let configured = latest(&**self.db, self.session_id, "config", "current")
            .await?
            .ok_or_else(invalid)?;
        let config = operation(&configured)?;
        let binding_owner = &operation(&event)?.owner;
        if binding_owner != &config.owner {
            if matches!(&config.record, R::RecoveryPrepared { recovery }
                if binding_owner == &recovery.previous_owner && event.sequence < configured.sequence)
            {
                return Ok(None);
            }
            return Err(invalid());
        }
        if matches!(&operation(&event)?.record, ExternalHarnessOperationRecord::DeliveryCompleted { response,.. }
            if matches!(&response.kind, ExternalHarnessResponseKind::Prepared(_) | ExternalHarnessResponseKind::Reconciled(bcode_harness_models::HarnessReconciliation::KnownPrepared { .. })))
        {
            return Ok(None);
        }
        Ok(Some(event))
    }
    pub(crate) async fn harness_history(
        &self,
        after: Option<String>,
        limit: u16,
    ) -> SessionDbResult<bcode_harness_models::HarnessObservationPage> {
        if limit == 0 || usize::from(limit) > MAX_HARNESS_OBSERVATIONS {
            return Err(invalid());
        }
        super::external_backend::external_index_ready(&**self.db).await?;
        let (sequence, offset) = history_position(after)?;
        let rows = self
            .db
            .select("turn_evidence")
            .columns(&["event_seq"])
            .where_eq("turn_id", op_key("history", "all"))
            .where_eq("evidence_kind", KIND)
            .where_gte("event_seq", seq_to_value(sequence))
            .sort("event_seq", SortDirection::Asc)
            .limit(2)
            .execute(&**self.db)
            .await?;
        let unknown = HarnessExecutionStatus {
            connectivity: HarnessConnectivity::Unknown,
            activity: HarnessActivity::Unknown,
            attention: HarnessAttention::Unknown,
            continuation: HarnessContinuation::Unknown,
            completeness: HarnessCompleteness::Unknown,
        };
        let Some(row) = rows.first() else {
            return Ok(HarnessObservationPage {
                retention: None,
                hydration_requests: Vec::new(),
                inventory_page: None,
                observations: vec![],
                next_after: Some(format!("h1:{sequence}:{offset}")),
                completeness: HarnessCompleteness::Complete,
                status: unknown,
            });
        };
        let seq = required_non_negative_u64(row, "event_seq")?;
        let lengths = self
            .db
            .select("events")
            .project_as(
                switchy::database::query::byte_length(switchy::database::query::identifier(
                    "payload",
                )),
                "bytes",
            )
            .where_eq("event_seq", seq_to_value(seq))
            .limit(1)
            .execute(&**self.db)
            .await?;
        if required_non_negative_u64(lengths.first().ok_or_else(invalid)?, "bytes")?
            > (MAX_HARNESS_WIRE_BYTES * 6 + 4096) as u64
        {
            return Err(invalid());
        }
        let events = self
            .db
            .select("events")
            .columns(&["event_seq", "payload"])
            .where_eq("event_seq", seq_to_value(seq))
            .limit(1)
            .execute(&**self.db)
            .await?;
        let event = strict_event_from_row(events.first().ok_or_else(invalid)?, self.session_id)?;
        let ExternalHarnessOperationRecord::ObservationPage { page, .. } =
            &operation(&event)?.record
        else {
            return Err(invalid());
        };
        let start = if seq == sequence { offset } else { 0 };
        if start > page.observations.len() {
            return Err(invalid());
        }
        let end = (start + usize::from(limit)).min(page.observations.len());
        let observations = page.observations[start..end].to_vec();
        let next_after = Some(if end < page.observations.len() {
            format!("h1:{seq}:{end}")
        } else {
            format!("h1:{}:0", seq.checked_add(1).ok_or_else(invalid)?)
        });
        Ok(HarnessObservationPage {
            retention: None,
            hydration_requests: Vec::new(),
            inventory_page: None,
            observations,
            next_after,
            completeness: if end < page.observations.len() || rows.len() > 1 {
                HarnessCompleteness::Partial
            } else {
                page.completeness
            },
            status: page.status.clone(),
        })
    }
    pub(crate) async fn harness_state(
        &self,
        owner: &ExternalHarnessOwner,
    ) -> SessionDbResult<Option<ExternalHarnessState>> {
        super::external_backend::external_index_ready(&**self.db).await?;
        state(&**self.db, owner).await
    }
    pub(crate) async fn harness_latest(
        &self,
        session: SessionId,
        namespace: &str,
        id: &str,
    ) -> SessionDbResult<Option<SessionEvent>> {
        super::external_backend::external_index_ready(&**self.db).await?;
        latest(&**self.db, session, namespace, id).await
    }
    pub(crate) async fn harness_interaction(
        &self,
        session: SessionId,
        id: &str,
    ) -> SessionDbResult<Option<HarnessInteraction>> {
        super::external_backend::external_index_ready(&**self.db).await?;
        interaction(&**self.db, session, id).await
    }
}

async fn validate_claim(
    db: &dyn Database,
    op: &ExternalHarnessOperation,
    current: &ExternalHarnessState,
    request: &ExternalHarnessRequest,
) -> SessionDbResult<()> {
    if current.instance != request.instance
        || (request.open().is_some()
            && current.prepared_open.as_ref().is_some_and(|v| v != request))
        || (request.is_selected_open() && current.prepared_open.as_ref() != Some(request))
        || latest(db, op.owner.session_id, "claim", &request.request_id)
            .await?
            .is_some()
        || latest(db, op.owner.session_id, "mutation", &mutation_key(request))
            .await?
            .is_some()
    {
        return Err(invalid());
    }
    match &request.kind {
        ExternalHarnessRequestKind::Open(open) => {
            if current.target.is_some() || open.resume != current.binding {
                return Err(invalid());
            }
        }
        ExternalHarnessRequestKind::Submit { target, .. }
        | ExternalHarnessRequestKind::Respond { target, .. } => {
            if current.target.as_ref() != Some(target) {
                return Err(invalid());
            }
        }
        ExternalHarnessRequestKind::Interrupt(control)
        | ExternalHarnessRequestKind::Close(control) => {
            if current.target.as_ref() != Some(&control.target) {
                return Err(invalid());
            }
        }
        ExternalHarnessRequestKind::OpenSelected(_) => {
            if current.binding.is_some()
                || current.target.is_some()
                || request.open().is_none_or(|open| open.resume.is_some())
            {
                return Err(invalid());
            }
        }
        ExternalHarnessRequestKind::Reconcile(_) => {}
    }
    if let ExternalHarnessRequestKind::Submit {
        target: bcode_harness_models::HarnessSessionTarget::Prepared(prepared),
        submission,
    } = &request.kind
        && (!matches!(
            submission.placement,
            bcode_harness_models::HarnessPlacement::NextTurn
        ) || latest(
            db,
            op.owner.session_id,
            "prepared_submit",
            &prepared.open_id,
        )
        .await?
        .is_some())
    {
        return Err(invalid());
    }
    if let ExternalHarnessRequestKind::Respond { response, .. } = &request.kind {
        if latest(db, op.owner.session_id, "answered", &response.request_id)
            .await?
            .is_some()
        {
            return Err(invalid());
        }
        let interaction = interaction(db, op.owner.session_id, &response.request_id)
            .await?
            .ok_or_else(invalid)?;
        response.validate_for(&interaction).map_err(|_| invalid())?;
    }
    Ok(())
}

async fn validate_completion(
    db: &dyn Database,
    op: &ExternalHarnessOperation,
    current: &ExternalHarnessState,
    request: &ExternalHarnessRequest,
    response: &ExternalHarnessResponse,
) -> SessionDbResult<()> {
    if let Some(target) = &current.target {
        match &response.kind {
            ExternalHarnessResponseKind::Selected {
                target: bcode_harness_models::HarnessSessionTarget::Confirmed(binding),
                ..
            }
            | ExternalHarnessResponseKind::Opened(binding)
            | ExternalHarnessResponseKind::Reconciled(
                bcode_harness_models::HarnessReconciliation::Known { binding, .. },
            ) => {
                let open_id = match target {
                    bcode_harness_models::HarnessSessionTarget::Prepared(v) => v.open_id.as_str(),
                    bcode_harness_models::HarnessSessionTarget::Confirmed(_) => "confirmed",
                };
                target
                    .validate_confirmation(open_id, binding)
                    .map_err(|_| invalid())?;
            }
            ExternalHarnessResponseKind::Selected {
                target: bcode_harness_models::HarnessSessionTarget::Prepared(prepared),
                ..
            }
            | ExternalHarnessResponseKind::Prepared(prepared)
            | ExternalHarnessResponseKind::Reconciled(
                bcode_harness_models::HarnessReconciliation::KnownPrepared { prepared, .. },
            ) if target
                != &bcode_harness_models::HarnessSessionTarget::Prepared(prepared.clone()) =>
            {
                return Err(invalid());
            }
            _ => {}
        }
    }
    let event = latest(db, op.owner.session_id, "claim", &request.request_id)
        .await?
        .ok_or_else(invalid)?;
    if operation(&event)?.owner != op.owner
        || !matches!(&operation(&event)?.record,R::DeliveryClaimed { request: original } if original==request)
    {
        return Err(invalid());
    }
    if latest(db, op.owner.session_id, "completion", &request.request_id)
        .await?
        .is_some()
    {
        return Err(invalid());
    }
    Ok(())
}

async fn validate_page(
    db: &dyn Database,
    op: &ExternalHarnessOperation,
    current: &ExternalHarnessState,
    after: Option<&str>,
    page: &bcode_harness_models::HarnessObservationPage,
    instance: &bcode_harness_models::HarnessInstance,
) -> SessionDbResult<()> {
    page.validate_retention().map_err(|_| invalid())?;
    if let Some(retained) = &page.retention
        && latest(
            db,
            op.owner.session_id,
            &execution_namespace(&op.owner, "retention_page_v1"),
            &submission_key(&retained.connection_id, &retained.page_id),
        )
        .await?
        .is_some()
    {
        // Exact retries settle in the actor before append. Reusing a delivered page
        // identity for a second canonical operation is never a new observation.
        return Err(invalid());
    }
    if *instance != current.instance {
        return Err(invalid());
    }
    let previous = latest(db, op.owner.session_id, "page", "current").await?;
    let previous = match previous {
        Some(event) if operation(&event)?.owner == op.owner => Some(event),
        Some(event) => {
            let configured = latest(db, op.owner.session_id, "config", "current")
                .await?
                .ok_or_else(invalid)?;
            let config = operation(&configured)?;
            let R::RecoveryPrepared { recovery } = &config.record else {
                return Err(invalid());
            };
            if config.owner != op.owner
                || operation(&event)?.owner != recovery.previous_owner
                || event.sequence >= configured.sequence
            {
                return Err(invalid());
            }
            None
        }
        None => None,
    };
    if let Some(previous) = &previous {
        let coverage = latest(db, op.owner.session_id, "terminal_coverage_v2", "current")
            .await?
            .ok_or_else(evidence_maintenance_required)?;
        if coverage.sequence != previous.sequence {
            return Err(evidence_maintenance_required());
        }
        let execution = latest(db, op.owner.session_id, "execution_coverage_v3", "current")
            .await?
            .ok_or_else(evidence_maintenance_required)?;
        if execution.sequence != previous.sequence || operation(&execution)?.owner != op.owner {
            return Err(evidence_maintenance_required());
        }
    }
    let expected = previous
        .as_ref()
        .map(operation)
        .transpose()?
        .and_then(|v| match &v.record {
            R::ObservationPage { page, .. } => page.next_after.clone(),
            _ => None,
        });
    if after != expected.as_deref()
        || (!page.observations.is_empty() && page.next_after.as_deref() == after)
    {
        return Err(invalid());
    }
    if current.target.is_none() {
        return Err(invalid());
    }
    validate_page_observations(db, op, current, page).await?;
    Ok(())
}

fn history_position(after: Option<String>) -> SessionDbResult<(u64, usize)> {
    Ok(if let Some(cursor) = after {
        if cursor.len() > 128 {
            return Err(invalid());
        }
        let mut parts = cursor.split(':');
        if parts.next() != Some("h1") {
            return Err(invalid());
        }
        let sequence = parts
            .next()
            .ok_or_else(invalid)?
            .parse::<u64>()
            .map_err(|_| invalid())?;
        let offset = parts
            .next()
            .ok_or_else(invalid)?
            .parse::<usize>()
            .map_err(|_| invalid())?;
        if parts.next().is_some() || sequence > i64::MAX as u64 || offset > MAX_HARNESS_OBSERVATIONS
        {
            return Err(invalid());
        }
        (sequence, offset)
    } else {
        (0, 0)
    })
}

async fn validate_native_terminal(
    db: &dyn Database,
    op: &ExternalHarnessOperation,
    observation: &bcode_harness_models::HarnessObservation,
    accepted_turns_in_page: &std::collections::BTreeSet<String>,
    native_terminals_in_page: &mut std::collections::BTreeSet<String>,
) -> SessionDbResult<()> {
    if let HarnessObservationKind::NativeTurnTerminal { native_turn_id, .. } = &observation.kind {
        if observation.completeness != HarnessCompleteness::Complete
            || !native_terminals_in_page.insert(native_turn_id.clone())
            || latest(db, op.owner.session_id, "native_terminal", native_turn_id)
                .await?
                .is_some()
        {
            return Err(invalid());
        }
        if !accepted_turns_in_page.contains(native_turn_id) {
            let accepted = latest(db, op.owner.session_id, "accepted_turn", native_turn_id)
                .await?
                .ok_or_else(invalid)?;
            if operation(&accepted)?.owner != op.owner {
                return Err(invalid());
            }
            evidence_position(&accepted, |v| {
                matches!(&v.kind,
                HarnessObservationKind::SubmissionAccepted { native_turn_id: id, .. }
                    if id == native_turn_id)
            })?;
        }
    }
    Ok(())
}

async fn validate_result_correlations(
    db: &dyn Database,
    op: &ExternalHarnessOperation,
    observation: &bcode_harness_models::HarnessObservation,
) -> SessionDbResult<()> {
    for correlation in &observation.native_correlations {
        if matches!(observation.kind, HarnessObservationKind::Result { .. }) {
            let previous = latest(db, op.owner.session_id, "turn_result", correlation).await?;
            if let Some(previous) = previous {
                let (_, retained) = evidence_position(&previous, |v| {
                    matches!(v.kind, HarnessObservationKind::Result { .. })
                        && v.native_correlations.contains(correlation)
                })?;
                if operation(&previous)?.owner != op.owner
                    || (retained.completeness == HarnessCompleteness::Complete
                        && retained.kind != observation.kind)
                {
                    return Err(invalid());
                }
            }
        }
        if latest(db, op.owner.session_id, "settled", correlation)
            .await?
            .is_some()
        {
            return Err(invalid());
        }
    }
    Ok(())
}

async fn validate_interaction_and_binding(
    db: &dyn Database,
    op: &ExternalHarnessOperation,
    current: &ExternalHarnessState,
    observation: &bcode_harness_models::HarnessObservation,
    confirmed: &mut Option<bcode_harness_models::HarnessSessionBinding>,
) -> SessionDbResult<()> {
    if let HarnessObservationKind::Interaction { interaction } = &observation.kind
        && (latest(
            db,
            op.owner.session_id,
            "interaction",
            &interaction.request_id,
        )
        .await?
        .is_some()
            || latest(db, op.owner.session_id, "answered", &interaction.request_id)
                .await?
                .is_some())
    {
        return Err(invalid());
    }
    if let HarnessObservationKind::BindingConfirmed { open_id, binding } = &observation.kind {
        current
            .target
            .as_ref()
            .ok_or_else(invalid)?
            .validate_confirmation(open_id, binding)
            .map_err(|_| invalid())?;
        if confirmed.as_ref().is_some_and(|v| v != binding) {
            return Err(invalid());
        }
        *confirmed = Some(binding.clone());
    }
    Ok(())
}

async fn validate_execution_attempt(
    db: &dyn Database,
    owner: &ExternalHarnessOwner,
    attempt: &HarnessAttemptIdentity,
) -> SessionDbResult<()> {
    let claim = latest(
        db,
        owner.session_id,
        "submission_claim",
        &submission_key(&attempt.submission_id, &attempt.attempt_id),
    )
    .await?
    .ok_or_else(invalid)?;
    if operation(&claim)?.owner != *owner {
        return Err(invalid());
    }
    let R::DeliveryClaimed { request } = &operation(&claim)?.record else {
        return Err(invalid());
    };
    let ExternalHarnessRequestKind::Submit { submission, .. } = &request.kind else {
        return Err(invalid());
    };
    if submission.submission_id != attempt.submission_id
        || submission.attempt_id != attempt.attempt_id
        || submission.generation != owner.generation
    {
        return Err(invalid());
    }
    Ok(())
}

fn validate_execution_transition(
    previous: &HarnessExecutionEvent,
    next: &HarnessExecutionEvent,
) -> SessionDbResult<()> {
    use HarnessExecutionEvent as E;
    if execution_identity(previous) != execution_identity(next)
        || execution_origin(previous).is_some_and(|origin| Some(origin) != execution_origin(next))
    {
        return Err(invalid());
    }
    if let Some(state) = execution_state(previous)
        && matches!(
            state,
            HarnessWorkState::Completed | HarnessWorkState::Failed | HarnessWorkState::Interrupted
        )
        && execution_state(next) != Some(state)
    {
        return Err(invalid());
    }
    if execution_state(previous) == Some(HarnessWorkState::Running)
        && execution_state(next) == Some(HarnessWorkState::Pending)
    {
        return Err(invalid());
    }
    let compatible = match (previous, next) {
        (
            E::BackgroundWork {
                native_task_id: previous_native,
                parent_work_id: previous_parent,
                work_kind: previous_kind,
                notification: previous_notification,
                ..
            },
            E::BackgroundWork {
                native_task_id: next_native,
                parent_work_id: next_parent,
                work_kind: next_kind,
                notification: next_notification,
                ..
            },
        ) => {
            (previous_native.is_none() || previous_native == next_native)
                && previous_parent == next_parent
                && (previous_kind == next_kind
                    || *previous_kind == bcode_harness_models::HarnessWorkKind::Other)
                && (!matches!(
                    previous_notification,
                    bcode_harness_models::HarnessNotificationState::Observed
                        | bcode_harness_models::HarnessNotificationState::NotExpected
                ) || previous_notification == next_notification)
        }
        (
            E::Continuation {
                native_turn_id: previous_native,
                source_work_id: previous_parent,
                output: old,
                ..
            },
            E::Continuation {
                native_turn_id: next_native,
                source_work_id: next_parent,
                output: new,
                ..
            },
        ) => {
            (previous_native.is_none() || previous_native == next_native)
                && previous_parent == next_parent
                && (old.is_none() || old == new)
        }
        (E::Coverage { .. }, E::Coverage { .. }) => true,
        _ => false,
    };
    if !compatible {
        return Err(invalid());
    }
    Ok(())
}

async fn validate_execution_native_identity(
    db: &dyn Database,
    op: &ExternalHarnessOperation,
    event: &HarnessExecutionEvent,
    seen: &std::collections::BTreeMap<(String, String), HarnessExecutionEvent>,
) -> SessionDbResult<()> {
    let (kind, id) = execution_identity(event);
    let native_identity = match event {
        HarnessExecutionEvent::BackgroundWork {
            native_task_id: Some(native),
            ..
        } => Some(("native_task", native)),
        _ => None,
    };
    if let Some((namespace, native)) = native_identity {
        if seen.values().any(|previous| match (previous, event) {
            (
                HarnessExecutionEvent::BackgroundWork {
                    native_task_id: a, ..
                },
                HarnessExecutionEvent::BackgroundWork {
                    native_task_id: b, ..
                },
            )
            | (
                HarnessExecutionEvent::SubmissionTerminal {
                    native_frame_id: a, ..
                },
                HarnessExecutionEvent::SubmissionTerminal {
                    native_frame_id: b, ..
                },
            ) => a == b && execution_identity(previous) != (kind, id.clone()),
            _ => false,
        }) {
            return Err(invalid());
        }
        if let Some(previous) = latest(
            db,
            op.owner.session_id,
            &execution_namespace(&op.owner, namespace),
            native,
        )
        .await?
        {
            if operation(&previous)?.owner != op.owner {
                return Err(invalid());
            }
            let R::ObservationPage { page, .. } = &operation(&previous)?.record else {
                return Err(invalid());
            };
            if page.observations.iter().any(|v| matches!(&v.kind, HarnessObservationKind::Execution { event: prior }
                if match prior {
                    HarnessExecutionEvent::BackgroundWork { native_task_id: Some(value), .. }
                    | HarnessExecutionEvent::SubmissionTerminal { native_frame_id: Some(value), .. } => value == native && execution_identity(prior) != (kind, id.clone()),
                    _ => false,
                })) { return Err(invalid()); }
        }
    }
    Ok(())
}

async fn validate_execution_parent(
    db: &dyn Database,
    op: &ExternalHarnessOperation,
    event: &HarnessExecutionEvent,
    seen: &std::collections::BTreeMap<(String, String), HarnessExecutionEvent>,
) -> SessionDbResult<()> {
    let parent = match event {
        HarnessExecutionEvent::BackgroundWork { parent_work_id, .. } => parent_work_id,
        HarnessExecutionEvent::Continuation { source_work_id, .. } => source_work_id,
        _ => &None,
    };
    if let Some(parent) = parent {
        let prior = if let Some(prior) = seen.get(&("work".into(), parent.clone())) {
            prior.clone()
        } else {
            let previous = latest(
                db,
                op.owner.session_id,
                &execution_namespace(&op.owner, "work"),
                parent,
            )
            .await?
            .ok_or_else(invalid)?;
            if operation(&previous)?.owner != op.owner {
                return Err(invalid());
            }
            let previous = execution_observation(&previous, "work", parent)?;
            let HarnessObservationKind::Execution { event } = previous.kind else {
                return Err(invalid());
            };
            event
        };
        if execution_origin(&prior) != execution_origin(event) {
            return Err(invalid());
        }
    }
    Ok(())
}

async fn validate_execution_acceptance(
    db: &dyn Database,
    op: &ExternalHarnessOperation,
    event: &HarnessExecutionEvent,
) -> SessionDbResult<()> {
    if let HarnessExecutionEvent::SubmissionTerminal {
        attempt,
        native_turn_id: Some(turn),
        ..
    } = event
        && let Some(accepted) = latest(db, op.owner.session_id, "accepted_turn", turn).await?
    {
        if operation(&accepted)?.owner != op.owner {
            return Err(invalid());
        }
        evidence_position(&accepted, |v| {
            matches!(&v.kind,
            HarnessObservationKind::SubmissionAccepted { submission_id, attempt_id, native_turn_id }
            if submission_id == &attempt.submission_id && attempt_id == &attempt.attempt_id && native_turn_id == turn)
        })?;
    }
    if let HarnessExecutionEvent::SubmissionTerminal {
        attempt,
        native_turn_id: Some(turn),
        ..
    } = event
        && let Some(accepted) = latest(
            db,
            op.owner.session_id,
            "submission_acceptance",
            &submission_key(&attempt.submission_id, &attempt.attempt_id),
        )
        .await?
    {
        if operation(&accepted)?.owner != op.owner {
            return Err(invalid());
        }
        evidence_position(&accepted, |v| {
            matches!(&v.kind,
            HarnessObservationKind::SubmissionAccepted { native_turn_id, .. } if native_turn_id == turn)
        })?;
        if let Some(native) = latest(db, op.owner.session_id, "native_terminal", turn).await? {
            if operation(&native)?.owner != op.owner {
                return Err(invalid());
            }
            let HarnessExecutionEvent::SubmissionTerminal { disposition, .. } = event else {
                return Err(invalid());
            };
            evidence_position(&native, |v| {
                matches!(&v.kind,
                HarnessObservationKind::NativeTurnTerminal { native_turn_id, disposition: previous }
                if native_turn_id == turn && previous == disposition)
            })?;
        }
    }
    Ok(())
}

async fn validate_execution_observation(
    db: &dyn Database,
    op: &ExternalHarnessOperation,
    observation: &bcode_harness_models::HarnessObservation,
    seen: &mut std::collections::BTreeMap<(String, String), HarnessExecutionEvent>,
    _inventory_cache: &mut std::collections::BTreeMap<
        String,
        std::collections::BTreeSet<(String, String)>,
    >,
) -> SessionDbResult<()> {
    let HarnessObservationKind::Execution { event } = &observation.kind else {
        return Ok(());
    };
    event.validate().map_err(|_| invalid())?;
    if matches!(event, HarnessExecutionEvent::SubmissionTerminal { .. })
        && observation.completeness != HarnessCompleteness::Complete
    {
        return Err(invalid());
    }
    if let Some(attempt) = execution_origin(event) {
        validate_execution_attempt(db, &op.owner, attempt).await?;
    }
    let (kind, id) = execution_identity(event);
    validate_execution_native_identity(db, op, event, seen).await?;
    let identity = (kind.to_owned(), id.clone());
    if let Some(previous) = seen.get(&identity) {
        validate_execution_transition(previous, event)?;
    } else if let Some(previous) = latest(
        db,
        op.owner.session_id,
        &execution_namespace(&op.owner, kind),
        &id,
    )
    .await?
    {
        if operation(&previous)?.owner != op.owner {
            return Err(invalid());
        }
        let previous = execution_observation(&previous, kind, &id)?;
        let HarnessObservationKind::Execution { event: previous } = previous.kind else {
            return Err(invalid());
        };
        validate_execution_transition(&previous, event)?;
    }
    validate_execution_parent(db, op, event, seen).await?;
    validate_execution_acceptance(db, op, event).await?;
    // Current-membership integrity is proved along affected authenticated paths during
    // commitment construction. Never enumerate all historical identities to admit a page.
    seen.insert(identity, event.clone());
    Ok(())
}

fn validate_page_execution_acceptance(
    observation: &bcode_harness_models::HarnessObservation,
    page: &HarnessObservationPage,
) -> SessionDbResult<()> {
    if let HarnessObservationKind::Execution {
            event:
                HarnessExecutionEvent::SubmissionTerminal {
                    attempt,
                    native_turn_id: Some(turn),
                    ..
                },
        } = &observation.kind
            && page.observations.iter().any(|v| matches!(&v.kind,
                HarnessObservationKind::SubmissionAccepted { submission_id, attempt_id, native_turn_id }
                if submission_id == &attempt.submission_id && attempt_id == &attempt.attempt_id && native_turn_id != turn))
            { return Err(invalid()); }
    Ok(())
}

async fn validate_page_frame(
    db: &dyn Database,
    op: &ExternalHarnessOperation,
    observation: &bcode_harness_models::HarnessObservation,
    frames_in_page: &mut std::collections::BTreeSet<String>,
) -> SessionDbResult<()> {
    if let Some(key) = submission_frame_key(op.owner.generation, &observation.kind) {
        // Merged native frames may attribute multiple retained attempts. Only the exact
        // generation/stage/frame/submission/attempt tuple is immutable and unique.
        // Replays are handled only by exact retained page input.
        if !frames_in_page.insert(key.clone())
            || latest(db, op.owner.session_id, "submission_native_frame_v2", &key)
                .await?
                .is_some()
        {
            return Err(invalid());
        }
        validate_submission_attribution(db, op, observation).await?;
    }
    Ok(())
}

async fn validate_page_observations(
    db: &dyn Database,
    op: &ExternalHarnessOperation,
    current: &ExternalHarnessState,
    page: &bcode_harness_models::HarnessObservationPage,
) -> SessionDbResult<()> {
    let mut accepted_in_page = std::collections::BTreeSet::new();
    let mut confirmed = None;
    let mut settled_in_page = std::collections::BTreeSet::new();
    let mut results_in_page = std::collections::BTreeSet::new();
    let mut frames_in_page = std::collections::BTreeSet::new();
    let mut accepted_turns_in_page = std::collections::BTreeSet::new();
    let mut native_terminals_in_page = std::collections::BTreeSet::new();
    let mut execution_in_page = std::collections::BTreeMap::new();
    // Lifetime is one page validation inside the append transaction, never a session cache.
    let mut inventory_cache = std::collections::BTreeMap::new();
    for observation in &page.observations {
        validate_page_execution_acceptance(observation, page)?;
        validate_execution_observation(
            db,
            op,
            observation,
            &mut execution_in_page,
            &mut inventory_cache,
        )
        .await?;
        if let HarnessObservationKind::SubmissionAccepted { native_turn_id, .. } = &observation.kind
            && (!accepted_turns_in_page.insert(native_turn_id.clone())
                || latest(db, op.owner.session_id, "accepted_turn", native_turn_id)
                    .await?
                    .is_some())
        {
            return Err(invalid());
        }
        validate_native_terminal(
            db,
            op,
            observation,
            &accepted_turns_in_page,
            &mut native_terminals_in_page,
        )
        .await?;
        validate_page_frame(db, op, observation, &mut frames_in_page).await?;
        if matches!(observation.kind, HarnessObservationKind::Result { .. }) {
            for correlation in &observation.native_correlations {
                if !results_in_page.insert(correlation) {
                    return Err(invalid());
                }
            }
        }
        if let HarnessObservationKind::SubmissionAccepted {
            submission_id,
            attempt_id,
            ..
        } = &observation.kind
        {
            let key = submission_key(submission_id, attempt_id);
            if !accepted_in_page.insert(key) {
                return Err(invalid());
            }
            validate_submission_attribution(db, op, observation).await?;
        }
        if observation
            .native_correlations
            .iter()
            .any(|id| settled_in_page.contains(id))
        {
            return Err(invalid());
        }
        if let HarnessObservationKind::Settlement { native_turn_id } = &observation.kind
            && (!settled_in_page.insert(native_turn_id.clone())
                || latest(db, op.owner.session_id, "settled", native_turn_id)
                    .await?
                    .is_some())
        {
            return Err(invalid());
        }
        if matches!(observation.kind, HarnessObservationKind::Settlement { .. })
            && observation.completeness != bcode_harness_models::HarnessCompleteness::Complete
        {
            return Err(invalid());
        }
        validate_result_correlations(db, op, observation).await?;
        validate_interaction_and_binding(db, op, current, observation, &mut confirmed).await?;
        if let Some(previous) = latest(
            db,
            op.owner.session_id,
            "observation",
            &observation.observation_id,
        )
        .await?
        {
            // An identical poll retry is handled before append by its retained input
            // cursor. Repeating an ID on a different page must not duplicate history.
            let _ = previous;
            return Err(invalid());
        }
    }
    Ok(())
}

async fn validate_submission_attribution(
    db: &dyn Database,
    op: &ExternalHarnessOperation,
    observation: &bcode_harness_models::HarnessObservation,
) -> SessionDbResult<()> {
    let (submission_id, attempt_id): (&str, &str) = match &observation.kind {
        HarnessObservationKind::SubmissionAccepted {
            submission_id,
            attempt_id,
            ..
        } => (submission_id, attempt_id),
        kind => {
            let (submission_id, attempt_id, _) =
                submission_native_frame(kind).ok_or_else(invalid)?;
            (submission_id, attempt_id)
        }
    };
    let key = submission_key(submission_id, attempt_id);
    if let HarnessObservationKind::SubmissionAccepted { native_turn_id, .. } = &observation.kind
        && let Some(terminal) = latest(
            db,
            op.owner.session_id,
            &execution_namespace(&op.owner, "terminal"),
            &key,
        )
        .await?
    {
        if operation(&terminal)?.owner != op.owner {
            return Err(invalid());
        }
        let prior = execution_observation(&terminal, "terminal", &key)?;
        if let HarnessObservationKind::Execution {
            event:
                HarnessExecutionEvent::SubmissionTerminal {
                    native_turn_id: Some(turn),
                    ..
                },
        } = prior.kind
            && &turn != native_turn_id
        {
            return Err(invalid());
        }
    }
    let claim = latest(db, op.owner.session_id, "submission_claim", &key)
        .await?
        .ok_or_else(invalid)?;
    let retained = operation(&claim)?;
    if retained.owner != op.owner {
        return Err(invalid());
    }
    let R::DeliveryClaimed { request } = &retained.record else {
        return Err(invalid());
    };
    let ExternalHarnessRequestKind::Submit { submission, .. } = &request.kind else {
        return Err(invalid());
    };
    if submission.submission_id != submission_id
        || submission.attempt_id != attempt_id
        || submission.generation != observation.generation
    {
        return Err(invalid());
    }
    if matches!(
        observation.kind,
        HarnessObservationKind::SubmissionAccepted { .. }
    ) && latest(db, op.owner.session_id, "submission_acceptance", &key)
        .await?
        .is_some()
    {
        return Err(invalid());
    }
    Ok(())
}

#[cfg(test)]
mod migration_key_tests {
    use super::*;
    use bcode_session_migration_target::{MigrationReceipt, MigrationTarget as _};
    use std::collections::BTreeMap;

    #[tokio::test]
    async fn unpublished_recovery_rejects_before_host_verification_or_session_creation() {
        let root = tempfile::tempdir().unwrap();
        let sessions = crate::SessionManager::persistent(root.path()).unwrap();
        let owner = ExternalHarnessOwner {
            session_id: SessionId::new(),
            generation: 1,
            artifact_id: "fingerprint".into(),
            daemon_id: "daemon".into(),
        };
        let release = bcode_session_models::external_harness::ExternalHarnessTransportRelease {
            close_request_id: "close".into(),
            instance: recovery_test_instance(),
            reap_id: "reap".into(),
            binding: bcode_harness_models::HarnessSessionBinding {
                schema_version: 1,
                session_id: owner.session_id.to_string(),
                instance_id: "fixture".into(),
                native_session_id: "native".into(),
                native_version: "exact".into(),
                generation: 1,
            },
            lifetime: bcode_session_models::external_harness::ExternalHarnessLifetime {
                daemon_instance_id: owner.daemon_id.clone(),
                artifact_id: "exact-artifact".into(),
                state_location_id: "location".into(),
            },
        };
        let gated = ExternalHarnessOperation {
            version: 1,
            owner: owner.clone(),
            inventory_commitments: Vec::new(),
            record: R::TransportReleased {
                release: release.clone(),
            },
        };
        for schema in [55, 56, 57, 58] {
            assert!(
                gated.validate_storage(schema, 25).is_err(),
                "later schemas must not activate recovery"
            );
        }
        let verified = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let called = std::sync::Arc::clone(&verified);
        assert!(
            sessions
                .release_external_harness_transport(&owner, release, move |_| {
                    called.store(true, std::sync::atomic::Ordering::SeqCst);
                    Ok(())
                })
                .await
                .is_err()
        );
        assert!(!verified.load(std::sync::atomic::Ordering::SeqCst));
        assert!(sessions.session_summary(owner.session_id).await.is_err());
        drop(sessions);
    }

    #[test]
    fn recovery_shape_rejects_recreation_identity_changes_and_generation_overflow() {
        let owner = ExternalHarnessOwner {
            session_id: SessionId::new(),
            generation: 1,
            artifact_id: "fingerprint".into(),
            daemon_id: "old-daemon".into(),
        };
        let instance = recovery_test_instance();
        let release = bcode_session_models::external_harness::ExternalHarnessTransportRelease {
            close_request_id: "closed".into(),
            instance: instance.clone(),
            reap_id: "reaped".into(),
            binding: bcode_harness_models::HarnessSessionBinding {
                schema_version: 1,
                session_id: owner.session_id.to_string(),
                instance_id: instance.instance_id.clone(),
                native_session_id: "native-exact".into(),
                native_version: "version-exact".into(),
                generation: 1,
            },
            lifetime: bcode_session_models::external_harness::ExternalHarnessLifetime {
                daemon_instance_id: owner.daemon_id.clone(),
                artifact_id: "real-artifact".into(),
                state_location_id: "location".into(),
            },
        };
        let next = ExternalHarnessOwner {
            generation: 2,
            daemon_id: "new-daemon".into(),
            ..owner.clone()
        };
        let mut binding = release.binding.clone();
        binding.generation = 2;
        let mut recovery = bcode_session_models::external_harness::ExternalHarnessRecovery {
            previous_owner: owner,
            release_event_sequence: 7,
            release,
            capabilities: recovery_test_capabilities(),
            resume_request: ExternalHarnessRequest::from_request(
                &bcode_harness_models::HarnessRequest {
                    schema_version: 1,
                    request_id: "recover".into(),
                    instance,
                    kind: bcode_harness_models::HarnessRequestKind::Open(
                        bcode_harness_models::HarnessOpenRequest {
                            open_id: "recover".into(),
                            session_id: next.session_id.to_string(),
                            generation: 2,
                            working_directory: "/tmp".into(),
                            resume: Some(binding),
                            parent_assistance: None,
                        },
                    ),
                },
            )
            .unwrap(),
        };
        recovery.validate_for(&next).unwrap();
        let ExternalHarnessRequestKind::Open(open) = &mut recovery.resume_request.kind else {
            unreachable!()
        };
        let expected = open.resume.take().unwrap();
        assert!(recovery.validate_for(&next).is_err());
        let ExternalHarnessRequestKind::Open(open) = &mut recovery.resume_request.kind else {
            unreachable!()
        };
        let mut changed = expected.clone();
        changed.native_session_id = "foreign-native".into();
        open.resume = Some(changed);
        assert!(recovery.validate_for(&next).is_err());
        let ExternalHarnessRequestKind::Open(open) = &mut recovery.resume_request.kind else {
            unreachable!()
        };
        open.resume = Some(expected);
        recovery.previous_owner.generation = u64::MAX;
        recovery.release.binding.generation = u64::MAX;
        assert!(recovery.validate_for(&next).is_err());
        recovery.previous_owner.generation = 1;
        recovery.release.binding.generation = 1;
        recovery.release.lifetime.daemon_instance_id = "unrelated-daemon".into();
        assert!(recovery.validate_for(&next).is_err());
    }

    fn recovery_test_instance() -> bcode_harness_models::HarnessInstance {
        serde_json::from_value(serde_json::json!({"instance_id":"fixture","plugin_id":"fixture",
            "executable":"/not-invoked","config_namespace":"fixture", "auth":{"provider_id":"fixture",
            "profile":"native","credential_namespace":"fixture","custody":"native_managed"}})).unwrap()
    }
    fn recovery_test_capabilities() -> bcode_harness_models::HarnessCapabilities {
        let capability = bcode_harness_models::HarnessCapability {
            support: bcode_harness_models::HarnessSupportLevel::Constrained,
            constraints: vec!["exact provenance".into()],
        };
        bcode_harness_models::HarnessCapabilities {
            resume: capability.clone(),
            steering: capability.clone(),
            approval: capability.clone(),
            questions: capability.clone(),
            assistance: capability.clone(),
            interrupt: capability.clone(),
            authoritative_settlement: capability.clone(),
            background_continuation: capability.clone(),
            authentication: capability,
        }
    }
    fn selected_test_event(owner: &ExternalHarnessOwner, sequence: u64, record: R) -> SessionEvent {
        SessionEvent {
            schema_version: 57,
            sequence,
            timestamp_ms: sequence,
            session_id: owner.session_id,
            provenance: None,
            kind: SessionEventKind::ExternalHarnessOperation {
                operation: Box::new(ExternalHarnessOperation {
                    version: 1,
                    owner: owner.clone(),
                    record,
                    inventory_commitments: Vec::new(),
                }),
            },
        }
    }
    fn selected_request(
        owner: &ExternalHarnessOwner,
        model: &str,
    ) -> bcode_harness_models::HarnessModelsRequest {
        bcode_harness_models::HarnessModelsRequest {
            schema_version: 1,
            request_id: "selected-open".into(),
            instance: recovery_test_instance(),
            kind: bcode_harness_models::HarnessModelsRequestKind::OpenSelected {
                open: Box::new(bcode_harness_models::HarnessOpenRequest {
                    open_id: "selected-open".into(),
                    session_id: owner.session_id.to_string(),
                    generation: owner.generation,
                    working_directory: "/tmp".into(),
                    resume: None,
                    parent_assistance: None,
                }),
                selection: bcode_harness_models::HarnessModelSelection {
                    model_id: model.into(),
                },
            },
        }
    }

    fn selected_result(
        request: &bcode_harness_models::HarnessModelsRequest,
    ) -> bcode_harness_models::HarnessModelsResponse {
        let bcode_harness_models::HarnessModelsRequestKind::OpenSelected { open, selection } =
            &request.kind
        else {
            unreachable!()
        };
        bcode_harness_models::HarnessModelsResponse {
            schema_version: 1,
            request_id: request.request_id.clone(),
            instance_id: request.instance.instance_id.clone(),
            kind: bcode_harness_models::HarnessModelsResponseKind::Selected {
                target: bcode_harness_models::HarnessSessionTarget::Prepared(
                    bcode_harness_models::HarnessPreparedSession {
                        open_id: open.open_id.clone(),
                        session_id: open.session_id.clone(),
                        instance_id: request.instance.instance_id.clone(),
                        generation: open.generation,
                        native_version: "fixture-version".into(),
                        requested_native_session_id: None,
                    },
                ),
                evidence: bcode_harness_models::HarnessModelEvidence {
                    requested_model_id: selection.model_id.clone(),
                    effective_model_id: None,
                },
            },
        }
    }

    #[test]
    fn selected_family_preserves_exact_envelope_and_separates_result_evidence() {
        let owner = ExternalHarnessOwner {
            session_id: SessionId::new(),
            generation: 1,
            artifact_id: "artifact".into(),
            daemon_id: "daemon".into(),
        };
        let request = selected_request(&owner, "discovered-model");
        let retained = ExternalHarnessRequest::from_models_request(&request).unwrap();
        assert!(
            retained.request().is_err(),
            "selected delivery must never become legacy Open"
        );
        let serialized = serde_json::to_string(&retained).unwrap();
        let decoded: ExternalHarnessRequest = serde_json::from_str(&serialized).unwrap();
        assert_eq!(decoded, retained);
        decoded.validate().unwrap();
        let response =
            ExternalHarnessResponse::from_models_response(&request, &selected_result(&request))
                .unwrap();
        response.validate_for(&retained).unwrap();
        assert!(response.response().is_err());
        assert!(
            matches!(response.kind, ExternalHarnessResponseKind::Selected { target: bcode_harness_models::HarnessSessionTarget::Prepared(_), ref evidence } if evidence.effective_model_id.is_none())
        );
        let changed =
            ExternalHarnessRequest::from_models_request(&selected_request(&owner, "other-model"))
                .unwrap();
        assert!(response.validate_for(&changed).is_err());
        let mut corrupt = retained.clone();
        if let ExternalHarnessRequestKind::OpenSelected(selected) = &mut corrupt.kind {
            selected.version = 2;
        }
        assert!(corrupt.validate().is_err());
        let mut corrupt = retained.clone();
        corrupt.request_id = "wrong-request".into();
        assert!(corrupt.validate().is_err());
        let mut corrupt = retained.clone();
        corrupt.instance.auth.profile = "other-account".into();
        assert!(corrupt.validate().is_err());
        let configured = ExternalHarnessOperation {
            version: 1,
            owner,
            inventory_commitments: Vec::new(),
            record: R::Configured {
                instance: request.instance,
                capabilities: recovery_test_capabilities(),
                open_request: Some(retained),
            },
        };
        assert!(configured.validate_storage(55, 23).is_err());
        assert!(configured.validate_storage(56, 24).is_err());
        configured.validate_storage(57, 25).unwrap();
        assert!(configured.validate_storage(57, 24).is_err());
    }

    async fn assert_selected_claim_identity_fences(
        db: &SessionDb,
        owner: &ExternalHarnessOwner,
        request: &bcode_harness_models::HarnessModelsRequest,
        retained: &ExternalHarnessRequest,
        current: &ExternalHarnessState,
    ) {
        let legacy = ExternalHarnessRequest::from_request(&bcode_harness_models::HarnessRequest {
            schema_version: 1,
            request_id: "legacy-open".into(),
            instance: request.instance.clone(),
            kind: bcode_harness_models::HarnessRequestKind::Open(
                bcode_harness_models::HarnessOpenRequest {
                    open_id: "legacy-open".into(),
                    ..retained.open().unwrap().clone()
                },
            ),
        })
        .unwrap();
        assert_eq!(mutation_key(retained), mutation_key(&legacy));
        legacy.validate().unwrap();
        assert!(
            matches!(
                legacy.request().unwrap().kind,
                bcode_harness_models::HarnessRequestKind::Open(_)
            ),
            "legacy Open remains legacy"
        );
        let mut different_id = bcode_harness_models::HarnessModelsRequest {
            schema_version: request.schema_version,
            request_id: request.request_id.clone(),
            instance: request.instance.clone(),
            kind: request.kind.clone(),
        };
        different_id.request_id = "other-open".into();
        if let bcode_harness_models::HarnessModelsRequestKind::OpenSelected { open, .. } =
            &mut different_id.kind
        {
            open.open_id = "other-open".into();
        }
        let different_id = ExternalHarnessRequest::from_models_request(&different_id).unwrap();
        let different_op = ExternalHarnessOperation {
            version: 1,
            owner: owner.clone(),
            record: R::DeliveryClaimed {
                request: different_id.clone(),
            },
            inventory_commitments: Vec::new(),
        };
        assert!(
            validate_claim(db.database(), &different_op, current, &different_id)
                .await
                .is_err()
        );
        let op = ExternalHarnessOperation {
            version: 1,
            owner: owner.clone(),
            record: R::DeliveryClaimed {
                request: legacy.clone(),
            },
            inventory_commitments: Vec::new(),
        };
        assert!(
            validate_claim(db.database(), &op, current, &legacy)
                .await
                .is_err()
        );
    }

    async fn assert_selected_database_completion(
        db: &SessionDb,
        owner: &ExternalHarnessOwner,
        request: &bcode_harness_models::HarnessModelsRequest,
        retained: &ExternalHarnessRequest,
    ) {
        let response =
            ExternalHarnessResponse::from_models_response(request, &selected_result(request))
                .unwrap();
        append_selected_test_event(
            db,
            selected_test_event(
                owner,
                3,
                R::DeliveryCompleted {
                    request: retained.clone(),
                    response: response.clone(),
                },
            ),
        )
        .await;
        assert!(matches!(
            db.external_harness_retained_request(retained)
                .await
                .unwrap(),
            Some(bcode_session_models::ExternalHarnessClaim::Recorded(_))
        ));
        let receipt = db
            .external_harness_selected_open_receipt(retained)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(&receipt.owner, owner);
        assert_eq!(&receipt.request, retained);
        assert_eq!(
            (
                receipt.configured_event_sequence,
                receipt.claimed_event_sequence,
                receipt.completed_event_sequence
            ),
            (1, 2, 3)
        );
        let prepared = state(db.database(), owner).await.unwrap().unwrap();
        assert!(prepared.binding.is_none());
        assert!(matches!(
            prepared.target,
            Some(bcode_harness_models::HarnessSessionTarget::Prepared(_))
        ));
        let mut corrupt = response;
        if let ExternalHarnessResponseKind::Selected { evidence, .. } = &mut corrupt.kind {
            evidence.requested_model_id = "wrong-model".into();
        }
        assert!(corrupt.validate_for(retained).is_err());
        super::super::set_storage_writer_contract(db.database(), 24)
            .await
            .unwrap();
        assert!(
            state(db.database(), owner).await.is_err(),
            "schema57 cannot substitute for a writer25 fence"
        );
        super::super::set_storage_writer_contract(db.database(), 25)
            .await
            .unwrap();
        db.database()
            .delete("turn_evidence")
            .where_eq("evidence_kind", KIND)
            .where_eq("turn_id", op_key("config", "current"))
            .execute(db.database())
            .await
            .unwrap();
        assert!(
            state(db.database(), owner).await.is_err(),
            "missing configuration must not reconstruct authority"
        );
    }

    async fn append_selected_session_created(
        db: &SessionDb,
        owner: &ExternalHarnessOwner,
        root: &std::path::Path,
    ) {
        db.append_event(&SessionEvent {
            schema_version: bcode_session_models::CURRENT_SESSION_EVENT_SCHEMA_VERSION,
            sequence: 0,
            timestamp_ms: 0,
            session_id: owner.session_id,
            provenance: None,
            kind: serde_json::from_value(serde_json::json!({"session_created": {
                "name": "selected claim fixture", "working_directory": root
            }}))
            .unwrap(),
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn selected_database_claims_are_exact_exclusive_and_uncertain_without_replay() {
        // Canonical decoding remains current-format only; publication belongs to the coordinator.
        if bcode_session_models::CURRENT_SESSION_EVENT_SCHEMA_VERSION != 57 {
            return;
        }
        let root = tempfile::tempdir().unwrap();
        let owner = ExternalHarnessOwner {
            session_id: SessionId::new(),
            generation: 1,
            artifact_id: "artifact".into(),
            daemon_id: "daemon".into(),
        };
        let db = SessionDb::open_turso_in_root(owner.session_id, root.path())
            .await
            .unwrap();
        // Candidate-fence test only; production CURRENT constants remain coordinator-owned.
        super::super::set_storage_writer_contract(db.database(), 25)
            .await
            .unwrap();
        append_selected_session_created(&db, &owner, root.path()).await;
        let request = selected_request(&owner, "discovered-model");
        let retained = ExternalHarnessRequest::from_models_request(&request).unwrap();
        append_selected_test_event(
            &db,
            selected_test_event(
                &owner,
                1,
                R::Configured {
                    instance: request.instance.clone(),
                    capabilities: recovery_test_capabilities(),
                    open_request: Some(retained.clone()),
                },
            ),
        )
        .await;
        let current = state(db.database(), &owner).await.unwrap().unwrap();
        assert!(
            current.binding.is_none() && current.target.is_none(),
            "configuration is not native evidence"
        );
        append_selected_test_event(
            &db,
            selected_test_event(
                &owner,
                2,
                R::DeliveryClaimed {
                    request: retained.clone(),
                },
            ),
        )
        .await;
        assert!(matches!(
            db.external_harness_retained_request(&retained)
                .await
                .unwrap(),
            Some(bcode_session_models::ExternalHarnessClaim::Uncertain)
        ));
        let changed =
            ExternalHarnessRequest::from_models_request(&selected_request(&owner, "other-model"))
                .unwrap();
        assert!(
            db.external_harness_retained_request(&changed)
                .await
                .is_err()
        );
        assert_selected_claim_identity_fences(&db, &owner, &request, &retained, &current).await;
        let other = ExternalHarnessRequest::from_models_request(&selected_request(
            &ExternalHarnessOwner {
                generation: 2,
                ..owner.clone()
            },
            "discovered-model",
        ))
        .unwrap();
        let stale = ExternalHarnessOperation {
            version: 1,
            owner: owner.clone(),
            record: R::DeliveryClaimed { request: other },
            inventory_commitments: Vec::new(),
        };
        assert!(stale.validate().is_err());
        assert!(
            state(
                db.database(),
                &ExternalHarnessOwner {
                    daemon_id: "stale-daemon".into(),
                    ..owner.clone()
                }
            )
            .await
            .is_err()
        );
        assert_selected_database_completion(&db, &owner, &request, &retained).await;
    }

    fn selected_actor_fixture(
        root: &std::path::Path,
    ) -> (crate::SessionManager, ExternalHarnessOwner) {
        let sessions = crate::SessionManager::persistent_with_metrics_and_lease_owner(
            root,
            bcode_metrics::MetricsRegistry::default(),
            crate::SessionLeaseOwnerContext {
                build_fingerprint: Some("artifact".into()),
                daemon_instance_id: Some("daemon".into()),
                ..crate::SessionLeaseOwnerContext::default()
            },
        )
        .unwrap();
        let owner = ExternalHarnessOwner {
            session_id: SessionId::new(),
            generation: 1,
            artifact_id: "artifact".into(),
            daemon_id: "daemon".into(),
        };
        (sessions, owner)
    }

    async fn assert_selected_actor_changed_request_rejected(
        sessions: &crate::SessionManager,
        owner: &ExternalHarnessOwner,
        request: &bcode_harness_models::HarnessModelsRequest,
    ) {
        let mut changed = bcode_harness_models::HarnessModelsRequest {
            schema_version: request.schema_version,
            request_id: request.request_id.clone(),
            instance: request.instance.clone(),
            kind: request.kind.clone(),
        };
        if let bcode_harness_models::HarnessModelsRequestKind::OpenSelected { selection, .. } =
            &mut changed.kind
        {
            selection.model_id = "other-model".into();
        }
        assert!(
            sessions
                .prepare_external_harness_selected_open(
                    owner,
                    &changed,
                    recovery_test_capabilities(),
                    None
                )
                .await
                .is_err()
        );
        assert!(
            sessions
                .claim_external_harness_selected_open(owner, &changed)
                .await
                .is_err()
        );
        assert!(
            sessions
                .claim_external_harness_selected_open(
                    &ExternalHarnessOwner {
                        daemon_id: "stale-daemon".into(),
                        ..owner.clone()
                    },
                    request
                )
                .await
                .is_err()
        );
        assert!(matches!(
            sessions
                .read_external_harness_retained_selected_request(owner.session_id, request)
                .await
                .unwrap(),
            Some(bcode_session_models::ExternalHarnessClaim::Uncertain)
        ));
        assert!(
            sessions
                .read_external_harness_retained_selected_request(owner.session_id, &changed)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn selected_actor_retry_is_uncertain_and_rejects_changed_model_and_owner() {
        if bcode_session_models::CURRENT_SESSION_EVENT_SCHEMA_VERSION != 57
            || bcode_session_models::CURRENT_SESSION_STORAGE_WRITER_EPOCH != 25
        {
            return;
        }
        let root = tempfile::tempdir().unwrap();
        let (sessions, owner) = selected_actor_fixture(root.path());
        let mut request = selected_request(&owner, "discovered-model");
        if let bcode_harness_models::HarnessModelsRequestKind::OpenSelected { open, .. } =
            &mut request.kind
        {
            open.working_directory = root.path().to_string_lossy().into_owned();
        }
        sessions
            .prepare_external_harness_selected_open(
                &owner,
                &request,
                recovery_test_capabilities(),
                None,
            )
            .await
            .unwrap();
        assert!(matches!(
            sessions
                .claim_external_harness_selected_open(&owner, &request)
                .await
                .unwrap(),
            bcode_session_models::ExternalHarnessClaim::New
        ));
        assert!(matches!(
            sessions
                .claim_external_harness_selected_open(&owner, &request)
                .await
                .unwrap(),
            bcode_session_models::ExternalHarnessClaim::Uncertain
        ));
        assert_selected_actor_changed_request_rejected(&sessions, &owner, &request).await;
        sessions
            .complete_external_harness_selected_open(&owner, &request, &selected_result(&request))
            .await
            .unwrap();
        assert!(matches!(
            sessions
                .claim_external_harness_selected_open(&owner, &request)
                .await
                .unwrap(),
            bcode_session_models::ExternalHarnessClaim::Recorded(_)
        ));
        let Some(bcode_session_models::ExternalHarnessClaim::Recorded(response)) = sessions
            .read_external_harness_retained_selected_request(owner.session_id, &request)
            .await
            .unwrap()
        else {
            panic!("selected result was not retained")
        };
        let retained = ExternalHarnessRequest::from_models_request(&request).unwrap();
        assert!(response.models_response(&retained).is_ok());
        assert!(response.response().is_err());
        let receipt = sessions
            .read_external_harness_selected_open_receipt(owner.session_id, &request)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(receipt.request, retained);
        assert!(receipt.configured_event_sequence < receipt.claimed_event_sequence);
        assert!(receipt.claimed_event_sequence < receipt.completed_event_sequence);
    }

    #[tokio::test]
    async fn selected_top_level_publication_keeps_workspace_in_first_catalog_actor() {
        let root = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let (sessions, owner) = selected_actor_fixture(root.path());
        let directory = std::fs::canonicalize(workspace.path()).unwrap();
        let mut request = selected_request(&owner, "discovered-model");
        let bcode_harness_models::HarnessModelsRequestKind::OpenSelected { open, .. } =
            &mut request.kind
        else {
            unreachable!()
        };
        open.working_directory = directory.to_string_lossy().into_owned();
        sessions
            .prepare_external_harness_selected_open(
                &owner,
                &request,
                recovery_test_capabilities(),
                None,
            )
            .await
            .unwrap();
        // This first summary actor must not seed itself from a root-only degraded catalog row.
        let summary = sessions.session_summary(owner.session_id).await.unwrap();
        assert_eq!(summary.working_directory, directory);
        assert_eq!(summary.parent_id, None);
        sessions
            .prepare_external_harness_selected_open(
                &owner,
                &request,
                recovery_test_capabilities(),
                None,
            )
            .await
            .unwrap();
        let history = sessions.session_history(owner.session_id).await.unwrap();
        assert_eq!(history.iter().filter(|event| matches!(event.kind, SessionEventKind::ExternalHarnessOperation { ref operation } if matches!(operation.record, R::Configured { .. }))).count(), 1);
        assert!(matches!(
            sessions
                .claim_external_harness_selected_open(&owner, &request)
                .await
                .unwrap(),
            bcode_session_models::ExternalHarnessClaim::New
        ));
    }

    fn selected_spawn_request(
        native: &bcode_harness_models::HarnessModelsRequest,
        owner: &ExternalHarnessOwner,
        parent_id: SessionId,
        directory: std::path::PathBuf,
    ) -> bcode_session_models::SessionSpawnRequest {
        let retained = ExternalHarnessRequest::from_models_request(native).unwrap();
        let ExternalHarnessRequestKind::OpenSelected(selected) = &retained.kind else {
            unreachable!()
        };
        bcode_session_models::SessionSpawnRequest {
            version: 2,
            creation: bcode_session_models::SessionChildCreateRequest {
                child_id: owner.session_id,
                parent_id,
                name: None,
                working_directory: directory,
            },
            worktree_directory: None,
            input: "immutable initial input".into(),
            auth_profile: None,
            admission: bcode_session_models::TurnAdmissionMetadata {
                idempotency_key: Some("spawn-input".into()),
                origin: Some(bcode_session_models::TurnOrigin {
                    producer: "test.spawn".into(),
                    correlation_id: None,
                    display_label: None,
                }),
                ..bcode_session_models::TurnAdmissionMetadata::default()
            },
            managed: Some(bcode_session_models::ManagedSessionSpawnIntent {
                selected_open: selected.clone(),
                source: bcode_session_models::ManagedSessionSpawnSource::Explicit,
            }),
        }
    }

    #[tokio::test]
    async fn selected_spawn_retains_composed_intent_and_canonical_open_receipt() {
        let root = tempfile::tempdir().unwrap();
        let (sessions, owner) = selected_actor_fixture(root.path());
        let directory = std::fs::canonicalize(root.path()).unwrap();
        let parent = sessions
            .create_session(None, directory.clone())
            .await
            .unwrap();
        let mut native = selected_request(&owner, "discovered-model");
        if let bcode_harness_models::HarnessModelsRequestKind::OpenSelected { open, .. } =
            &mut native.kind
        {
            open.working_directory = directory.to_string_lossy().into_owned();
        }
        let request = selected_spawn_request(&native, &owner, parent.id, directory);
        let session = sessions
            .prepare_external_harness_selected_spawn(&owner, &request, recovery_test_capabilities())
            .await
            .unwrap();
        sessions
            .prepare_external_harness_selected_spawn(&owner, &request, recovery_test_capabilities())
            .await
            .unwrap();
        let mut changed = request.clone();
        changed.input.push_str(" conflicting input");
        assert!(
            sessions
                .prepare_external_harness_selected_spawn(
                    &owner,
                    &changed,
                    recovery_test_capabilities()
                )
                .await
                .is_err()
        );
        let history = sessions.session_history(owner.session_id).await.unwrap();
        assert!(
            matches!(&history[1].kind, SessionEventKind::ChildSessionCreated { spawning: Some(exact), .. } if exact.as_ref() == &request)
        );
        assert!(matches!(
            sessions
                .claim_external_harness_selected_open(&owner, &native)
                .await
                .unwrap(),
            bcode_session_models::ExternalHarnessClaim::New
        ));
        sessions
            .complete_external_harness_selected_open(&owner, &native, &selected_result(&native))
            .await
            .unwrap();
        let receipt = sessions
            .read_external_harness_selected_open_receipt(owner.session_id, &native)
            .await
            .unwrap()
            .unwrap();
        let result = bcode_session_models::ManagedSessionSpawnResult {
            version: 2,
            open: Some(receipt),
            outcome: bcode_session_models::SessionSpawnResult {
                session: Some(session),
                admission: None,
                failure: Some(bcode_session_models::SessionSpawnFailure {
                    stage: bcode_session_models::SessionSpawnStage::Admission,
                    code: "not-dispatched".into(),
                    message: "test stops before admission".into(),
                }),
            },
        };
        result.validate_for(&request).unwrap();
        let mut wrong_open = request.clone();
        if let bcode_harness_models::HarnessModelsRequestKind::OpenSelected { selection, .. } =
            &mut wrong_open
                .managed
                .as_mut()
                .unwrap()
                .selected_open
                .request
                .kind
        {
            selection.model_id = "other-model".into();
        }
        assert!(result.validate_for(&wrong_open).is_err());
    }

    async fn append_selected_test_event(db: &SessionDb, mut event: SessionEvent) {
        let SessionEventKind::ExternalHarnessOperation { operation: op } = &mut event.kind else {
            unreachable!()
        };
        op.validate().unwrap();
        prepare_current_inventory_operation(db.database(), op, event.sequence)
            .await
            .unwrap();
        // Exercise transactional validation + canonical insertion + projector with candidate
        // schema explicitly, without changing production writer/schema constants.
        let tx = db.database().begin_transaction().await.unwrap();
        validate_operation(&*tx, &event).await.unwrap();
        super::super::append_event_projections(&*tx, &event, None)
            .await
            .unwrap();
        tx.commit().await.unwrap();
    }

    #[tokio::test]
    async fn inventory_tree_updates_share_immutable_nodes_and_preserve_counts() {
        let directory = tempfile::tempdir().expect("temporary root");
        let session = SessionId::new();
        let db = SessionDb::open_turso_in_root(session, directory.path())
            .await
            .expect("current DB");
        let mut nodes = InventoryNodes::new();
        let mut root = None;
        for index in 0..1050 {
            root = inventory_update(
                db.database(),
                root,
                &format!("identity-{index}"),
                Some((
                    index,
                    HarnessInventorySummary {
                        identities: 1,
                        unfinished: 1,
                        ..Default::default()
                    },
                )),
                &mut nodes,
            )
            .await
            .expect("insert identity");
        }
        let pinned = root.clone();
        assert_eq!(pinned.as_ref().expect("root").summary.identities, 1050);
        root = inventory_update(
            db.database(),
            root,
            "identity-500",
            Some((
                2000,
                HarnessInventorySummary {
                    identities: 1,
                    failed: 1,
                    ..Default::default()
                },
            )),
            &mut nodes,
        )
        .await
        .expect("terminal transition");
        assert_eq!(root.as_ref().expect("root").summary.unfinished, 1049);
        assert_eq!(root.as_ref().expect("root").summary.failed, 1);
        assert_eq!(
            inventory_find(db.database(), pinned.clone(), "identity-500", &nodes)
                .await
                .expect("pinned lookup")
                .expect("identity")
                .0,
            500
        );
        assert_eq!(
            inventory_find(db.database(), root.clone(), "identity-500", &nodes)
                .await
                .expect("current lookup")
                .expect("identity")
                .0,
            2000
        );
        root = inventory_update(db.database(), root, "absent", None, &mut nodes)
            .await
            .expect("absent deletion");
        assert_eq!(root.as_ref().expect("root").summary.identities, 1050);
        root = inventory_update(db.database(), root, "identity-500", None, &mut nodes)
            .await
            .expect("delete identity");
        assert_eq!(root.as_ref().expect("root").summary.identities, 1049);
        assert_eq!(
            pinned.as_ref().expect("pinned root").summary.identities,
            1050
        );
    }

    fn current_fixture_events() -> Vec<SessionEvent> {
        // Current-format input only: historical decoding is tested by session-migration.
        let session = "00000000-0000-0000-0000-000000000046";
        (0..=8)
            .map(|sequence| {
                let kind = if sequence == 0 {
                    serde_json::json!({"session_created": {
                        "name": "current target fixture", "working_directory": "/tmp"
                    }})
                } else {
                        let observations = match sequence {
                            5 => vec![serde_json::json!({"kind": "submission_accepted",
                                "submission_id": "submission", "attempt_id": "attempt",
                                "native_turn_id": "turn"})],
                            6 => vec![serde_json::json!({"kind": "result", "text": "result"})],
                            7 => vec![serde_json::json!({"kind": "settlement", "native_turn_id": "turn"})],
                            _ => Vec::new(),
                        };
                        let observations: Vec<_> = observations.into_iter().map(|kind| {
                            serde_json::json!({
                                "observation_id": format!("observation-{sequence}"),
                                "session_id": session, "generation": 1,
                                "native_correlations": ["turn"], "completeness": "complete",
                                "kind": kind
                            })
                        }).collect();
                        serde_json::json!({"external_harness_operation": {"operation": {
                            "version": 1,
                            "owner": {"session_id": session, "generation": 1,
                                "artifact_id": "fixture-artifact", "daemon_id": "fixture-daemon"},
                            "record": {"kind": "observation_page",
                                "instance": {"instance_id": "fixture", "plugin_id": "fixture",
                                    "executable": "/not-invoked", "config_namespace": "fixture",
                                    "auth": {"provider_id": "fixture", "profile": "native",
                                        "credential_namespace": "fixture", "custody": "native_managed"}},
                                "after": null,
                                "page": {"observations": observations,
                                    "next_after": format!("page-{sequence}"),
                                    "completeness": "complete",
                                    "status": {"connectivity": "connected", "activity": "running",
                                        "attention": "none", "continuation": "available",
                                        "completeness": "complete"}}
                            }
                        }}})
                };
                crate::persisted::decode_session_event(&serde_json::json!({
                    "schema_version": bcode_session_models::CURRENT_SESSION_EVENT_SCHEMA_VERSION,
                    "sequence": sequence, "timestamp_ms": sequence + 1,
                    "session_id": session, "provenance": null, "kind": kind
                }).to_string()).expect("current target fixture")
            })
            .collect()
    }

    fn target_fixture(with_attribution: bool) -> Vec<SessionEvent> {
        let mut events = current_fixture_events();
        if with_attribution {
            let SessionEventKind::ExternalHarnessOperation { operation } = &mut events[6].kind
            else {
                panic!("fixture result operation");
            };
            let R::ObservationPage { page, .. } = &mut operation.record else {
                panic!("fixture result page");
            };
            let template = page.observations[0].clone();
            for (id, kind) in [
                (
                    "consumed",
                    HarnessObservationKind::SubmissionConsumed {
                        submission_id: "submission".into(),
                        attempt_id: "attempt".into(),
                        native_frame_id: "consumed-frame".into(),
                    },
                ),
                (
                    "observed",
                    HarnessObservationKind::SubmissionResultObserved {
                        submission_id: "submission".into(),
                        attempt_id: "attempt".into(),
                        native_frame_id: "result-frame".into(),
                        is_error: false,
                        queued_user_sends: Some(0),
                    },
                ),
                (
                    "terminal",
                    HarnessObservationKind::NativeTurnTerminal {
                        native_turn_id: "turn".into(),
                        disposition: bcode_harness_models::HarnessTerminalDisposition::Completed,
                    },
                ),
            ] {
                let mut observation = template.clone();
                observation.observation_id = id.into();
                observation.kind = kind;
                page.observations.push(observation);
            }
        }
        events
    }

    async fn seeded_target(root: &std::path::Path, with_attribution: bool) -> SessionDb {
        let events = target_fixture(with_attribution);
        let db = SessionDb::open_turso_in_root(events[0].session_id, root)
            .await
            .expect("open target DB");
        let metrics = bcode_metrics::MetricsRegistry::in_memory();
        let mut target = super::super::SessionMigrationTarget::new(db.database(), &metrics);
        target
            .materialize_current_schema()
            .await
            .expect("materialize target");
        for original in &events {
            let mut event = original.clone();
            target
                .prepare_current_event(&mut event)
                .await
                .expect("host commitment");
            super::super::insert_event(db.database(), &event, None)
                .await
                .expect("canonical migrated row");
            target
                .ingest_projectors(&event)
                .await
                .expect("real target ingestion");
        }
        target
            .finalize_projectors(Some(8))
            .await
            .expect("finalize projectors");
        let facts = target
            .validate_strict_current()
            .await
            .expect("valid exact keys");
        super::super::validate_strict_target_facts(&facts).expect("complete current projections");
        super::super::set_storage_writer_contract(db.database(), 18)
            .await
            .expect("source writer epoch");
        db
    }

    #[tokio::test]
    async fn current_target_preparation_rejects_nonempty_forged_roots_without_overwriting() {
        let root = tempfile::tempdir().expect("temporary root");
        let events = target_fixture(false);
        let db = SessionDb::open_turso_in_root(events[0].session_id, root.path())
            .await
            .expect("open target");
        let metrics = bcode_metrics::MetricsRegistry::in_memory();
        let mut target = super::super::SessionMigrationTarget::new(db.database(), &metrics);
        target.materialize_current_schema().await.expect("schema");
        for original in events {
            let mut event = original;
            target
                .prepare_current_event(&mut event)
                .await
                .expect("prepare against prefix");
            let mut repeated = event.clone();
            target
                .prepare_current_event(&mut repeated)
                .await
                .expect("matching retained roots");
            assert_eq!(repeated, event);
            if let SessionEventKind::ExternalHarnessOperation { operation } = &mut repeated.kind {
                operation.inventory_commitments[0].summary.identities += 1;
                let forged = repeated.clone();
                assert!(target.prepare_current_event(&mut repeated).await.is_err());
                assert_eq!(
                    repeated, forged,
                    "never silently repair retained commitments"
                );
            }
            super::super::insert_event(db.database(), &event, None)
                .await
                .expect("prepared canonical");
            target
                .ingest_projectors(&event)
                .await
                .expect("ordered ingestion");
        }
    }

    #[tokio::test]
    async fn strict_migration_validation_rejects_missing_and_corrupt_inventory_snapshots() {
        for corrupt in [false, true] {
            let root = tempfile::tempdir().expect("temporary root");
            let db = seeded_target(root.path(), false).await;
            let events = super::super::strict_events(db.database())
                .await
                .expect("canonical");
            let commitment = events
                .iter()
                .rev()
                .find_map(|event| {
                    let SessionEventKind::ExternalHarnessOperation { operation } = &event.kind
                    else {
                        return None;
                    };
                    operation
                        .inventory_commitments
                        .iter()
                        .find(|root| root.snapshot_key.ends_with(":inventory_directory_v1"))
                        .cloned()
                })
                .expect("canonical directory");
            validate_migration_keys(db.database())
                .await
                .expect("valid snapshot graph");
            if corrupt {
                let mut forged = commitment.clone();
                forged.summary.identities += 1;
                db.database()
                    .update("snapshots")
                    .value("payload", serde_json::to_string(&forged).expect("json"))
                    .where_eq("snapshot_name", commitment.snapshot_key)
                    .execute(db.database())
                    .await
                    .expect("corrupt directory");
            } else {
                db.database()
                    .delete("snapshots")
                    .where_eq("snapshot_name", commitment.snapshot_key)
                    .execute(db.database())
                    .await
                    .expect("remove directory");
            }
            assert!(validate_migration_keys(db.database()).await.is_err());
        }
    }

    #[tokio::test]
    async fn latest_extremum_preserves_absence_and_canonical_locator_validation() {
        let root = tempfile::tempdir().expect("temporary root");
        let db = seeded_target(root.path(), false).await;
        let session = current_fixture_events()[0].session_id;
        assert!(
            latest(db.database(), session, "absent", "absent")
                .await
                .expect("absent")
                .is_none()
        );
        let configured = latest(db.database(), session, "page", "current")
            .await
            .expect("configured")
            .expect("retained operation");
        assert_eq!(configured.sequence, 8);
        // A newer locator for the exact key must still be validated against its canonical
        // payload; the extremum query must not hide corruption by returning an older row.
        db.database()
            .insert("turn_evidence")
            .value("turn_id", op_key("forged", "identity"))
            .value("evidence_kind", KIND)
            .value("segment_order", seq_to_value(8))
            .value("event_seq", seq_to_value(8))
            .execute(db.database())
            .await
            .expect("forged locator");
        assert!(
            latest(db.database(), session, "forged", "identity")
                .await
                .is_err()
        );
    }

    fn receipt() -> MigrationReceipt {
        MigrationReceipt {
            operation_id: "exact-key-fixture".into(),
            session_id: target_fixture(false)[0].session_id,
            source_writer_epoch: 18,
            target_writer_epoch: bcode_session_models::CURRENT_SESSION_STORAGE_WRITER_EPOCH,
            migration_step_ids: vec!["session-writer-epoch-18-to-19".into()],
            source_event_count: 9,
            source_event_tail: Some(8),
            source_payload_digest_sha256: "fixture-source".into(),
            target_event_count: 9,
            target_event_tail: Some(8),
            target_payload_digest_sha256: "fixture-target".into(),
            converted_events: BTreeMap::new(),
            retired_known_events: BTreeMap::new(),
            completed_at_ms: 1,
        }
    }

    async fn assert_finalization_rejected(db: &SessionDb) {
        let epoch = db.storage_writer_epoch().await.expect("writer epoch");
        assert_eq!(epoch, 18);
        let metrics = bcode_metrics::MetricsRegistry::in_memory();
        let mut target = super::super::SessionMigrationTarget::new(db.database(), &metrics);
        assert!(target.validate_strict_current().await.is_err());
        assert!(target.persist_migration_receipt(&receipt()).await.is_err());
        assert!(target.finalize_writer_contract().await.is_err());
        assert_eq!(
            db.storage_writer_epoch().await.expect("writer epoch"),
            epoch
        );
        assert!(
            db.database()
                .select("session_migration_receipts")
                .columns(&["operation_id"])
                .where_eq("operation_id", "exact-key-fixture")
                .execute_first(db.database())
                .await
                .expect("receipt query")
                .is_none()
        );
    }

    #[tokio::test]
    async fn migration_target_rejects_missing_exact_harness_keys() {
        let events = target_fixture(true);
        let mut locators = vec![
            op_key("terminal_coverage_v2", "current"),
            op_key("native_terminal", "turn"),
        ];
        locators.extend(
            keys(operation(&events[6]).expect("canonical operation"))
                .into_iter()
                .filter(|(namespace, _)| namespace == "submission_native_frame_v2")
                .map(|(namespace, id)| op_key(&namespace, &id)),
        );
        assert_eq!(
            locators.len(),
            4,
            "coverage, terminal, consumed and result attribution"
        );
        for key in locators {
            let root = tempfile::tempdir().expect("temp root");
            let db = seeded_target(root.path(), true).await;
            db.database()
                .delete("turn_evidence")
                .where_eq("evidence_kind", KIND)
                .where_eq("turn_id", key)
                .execute(db.database())
                .await
                .expect("remove locator");
            assert_finalization_rejected(&db).await;
        }
    }

    #[tokio::test]
    async fn migration_target_rejects_corrupt_exact_harness_keys() {
        for column in ["event_seq", "segment_order", "turn_id"] {
            let root = tempfile::tempdir().expect("temp root");
            let db = seeded_target(root.path(), true).await;
            let query = db
                .database()
                .update("turn_evidence")
                .where_eq("evidence_kind", KIND)
                .where_eq("turn_id", op_key("terminal_coverage_v2", "current"))
                .where_eq("event_seq", seq_to_value(6));
            if column == "turn_id" {
                query
                    .value(column, "corrupt-key")
                    .execute(db.database())
                    .await
                    .expect("corrupt key");
            } else {
                query
                    .value(column, seq_to_value(0))
                    .execute(db.database())
                    .await
                    .expect("corrupt locator");
            }
            assert_finalization_rejected(&db).await;
        }
    }

    #[tokio::test]
    async fn migration_snapshot_reset_is_scoped_transactional_and_preserves_canonical_rows() {
        let root = tempfile::tempdir().expect("temp root");
        let db = seeded_target(root.path(), true).await;
        let before = super::super::strict_events(db.database())
            .await
            .expect("canonical source");
        db.database()
            .insert("snapshots")
            .value("snapshot_name", "unrelated-maintenance-snapshot")
            .value("last_event_seq", 0_i64)
            .value("schema_version", 1_i64)
            .value("payload", "preserve")
            .execute(db.database())
            .await
            .expect("unrelated snapshot");
        let original = db
            .database()
            .select("snapshots")
            .columns(&["snapshot_name"])
            .where_gte("snapshot_name", "harness_inventory_node_v1:")
            .where_lt("snapshot_name", "harness_inventory_node_v1;")
            .execute(db.database())
            .await
            .expect("original proof nodes");
        assert_ne!(original, Vec::new());
        let tx = db
            .database()
            .begin_transaction()
            .await
            .expect("exclusive migration transaction");
        let metrics = bcode_metrics::MetricsRegistry::in_memory();
        let mut target = super::super::SessionMigrationTarget::new(&*tx, &metrics);
        target
            .materialize_current_schema()
            .await
            .expect("reset derived migration state");
        for (start, end) in [
            ("execution_v3:", "execution_v3;"),
            ("harness_inventory_node_v1:", "harness_inventory_node_v1;"),
        ] {
            assert_eq!(
                tx.select("snapshots")
                    .columns(&["snapshot_name"])
                    .where_gte("snapshot_name", start)
                    .where_lt("snapshot_name", end)
                    .execute(&*tx)
                    .await
                    .unwrap(),
                Vec::new()
            );
        }
        assert!(
            tx.select("snapshots")
                .columns(&["snapshot_name"])
                .where_eq("snapshot_name", "unrelated-maintenance-snapshot")
                .execute_first(&*tx)
                .await
                .unwrap()
                .is_some()
        );
        assert_eq!(super::super::strict_events(&*tx).await.unwrap(), before);
        assert!(
            target.validate_strict_current().await.is_err(),
            "reset alone cannot publish"
        );
        drop(target);
        tx.rollback().await.expect("rollback interrupted reset");
        assert_eq!(
            super::super::strict_events(db.database()).await.unwrap(),
            before
        );
        validate_migration_keys(db.database())
            .await
            .expect("original proofs restored");
        assert_eq!(
            db.database()
                .select("snapshots")
                .columns(&["snapshot_name"])
                .where_gte("snapshot_name", "harness_inventory_node_v1:")
                .where_lt("snapshot_name", "harness_inventory_node_v1;")
                .execute(db.database())
                .await
                .unwrap()
                .len(),
            original.len()
        );
    }

    #[tokio::test]
    async fn migration_target_key_failure_rolls_back_without_publication() {
        let root = tempfile::tempdir().expect("temp root");
        let db = seeded_target(root.path(), true).await;
        let tx = db
            .database()
            .begin_transaction()
            .await
            .expect("migration transaction");
        tx.delete("turn_evidence")
            .where_eq("evidence_kind", KIND)
            .where_eq("turn_id", op_key("terminal_coverage_v2", "current"))
            .execute(&*tx)
            .await
            .expect("damage target in transaction");
        let metrics = bcode_metrics::MetricsRegistry::in_memory();
        let mut target = super::super::SessionMigrationTarget::new(&*tx, &metrics);
        assert!(target.validate_strict_current().await.is_err());
        assert!(target.persist_migration_receipt(&receipt()).await.is_err());
        assert!(target.finalize_writer_contract().await.is_err());
        tx.rollback().await.expect("rollback failed migration");
        validate_migration_keys(db.database())
            .await
            .expect("original keys restored");
        assert_eq!(db.storage_writer_epoch().await.expect("writer epoch"), 18);
        assert!(
            db.database()
                .select("session_migration_receipts")
                .columns(&["operation_id"])
                .where_eq("operation_id", "exact-key-fixture")
                .execute_first(db.database())
                .await
                .expect("receipt query")
                .is_none()
        );
    }

    #[tokio::test]
    async fn migration_target_preserves_absence_of_terminal_evidence() {
        let root = tempfile::tempdir().expect("temp root");
        let db = seeded_target(root.path(), false).await;
        for namespace in ["native_terminal", "submission_native_frame_v2"] {
            let rows = db
                .database()
                .select("turn_evidence")
                .columns(&["turn_id"])
                .where_eq("evidence_kind", KIND)
                .execute(db.database())
                .await
                .expect("locator rows");
            let prefix = op_key(namespace, "");
            assert!(rows.iter().all(|row| {
                !super::super::required_string(row, "turn_id")
                    .expect("key")
                    .starts_with(&prefix)
            }));
        }
        let metrics = bcode_metrics::MetricsRegistry::in_memory();
        let mut target = super::super::SessionMigrationTarget::new(db.database(), &metrics);
        target
            .persist_migration_receipt(&receipt())
            .await
            .expect("valid legacy receipt");
        target
            .finalize_writer_contract()
            .await
            .expect("valid legacy writer");
        assert_eq!(
            db.storage_writer_epoch().await.expect("writer epoch"),
            u64::from(bcode_session_models::CURRENT_SESSION_STORAGE_WRITER_EPOCH)
        );
        for (namespace, id, sequence) in [
            (
                "submission_acceptance",
                submission_key("submission", "attempt"),
                5,
            ),
            ("accepted_turn", "turn".into(), 5),
            ("turn_result", "turn".into(), 6),
            ("settled", "turn".into(), 7),
            ("terminal_coverage_v2", "current".into(), 8),
        ] {
            let row = db
                .database()
                .select("turn_evidence")
                .columns(&["event_seq"])
                .where_eq("evidence_kind", KIND)
                .where_eq("turn_id", op_key(namespace, &id))
                .where_eq("event_seq", seq_to_value(sequence))
                .execute_first(db.database())
                .await
                .expect("legacy key query");
            assert!(row.is_some(), "missing legacy {namespace} evidence");
        }
        db.database()
            .insert("turn_evidence")
            .value("turn_id", op_key("native_terminal", "turn"))
            .value("evidence_kind", KIND)
            .value("event_seq", seq_to_value(6))
            .value("segment_order", seq_to_value(6))
            .execute(db.database())
            .await
            .expect("invent terminal locator");
        assert!(target.validate_strict_current().await.is_err());
        assert!(target.finalize_writer_contract().await.is_err());
    }
}
