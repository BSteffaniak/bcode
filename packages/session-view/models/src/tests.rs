use super::*;

#[test]
fn tool_presentation_slot_ids_are_stable_and_supplementals_are_independent() {
    assert_eq!(
        TranscriptViewItemId::tool_presentation_slot(
            "call-1",
            bcode_session_models::ToolContributionPlacement::Request,
            None,
        ),
        TranscriptViewItemId::new("tool-slot:call-1:request")
    );
    assert_ne!(
        TranscriptViewItemId::tool_presentation_slot(
            "call-1",
            bcode_session_models::ToolContributionPlacement::Supplemental,
            Some("one"),
        ),
        TranscriptViewItemId::tool_presentation_slot(
            "call-1",
            bcode_session_models::ToolContributionPlacement::Supplemental,
            Some("two"),
        )
    );
}

#[test]
fn structured_reasoning_activity_round_trips_through_renderer_wire_model() {
    let sentinel = "encrypted-sentinel-do-not-expose";
    let item = TranscriptViewItem {
        id: TranscriptViewItemId::reasoning("turn-1", "reasoning-1"),
        revision: 2,
        sequence: Some(9),
        timestamp_ms: Some(10),
        streaming: false,
        kind: TranscriptViewItemKind::ReasoningActivity {
            activity: ReasoningActivityView {
                turn_id: "turn-1".to_owned(),
                activity_id: "reasoning-1".to_owned(),
                order: 3,
                status: bcode_session_models::ReasoningActivityStatus::Interrupted,
                parts: vec![bcode_session_models::ReasoningPart {
                    part_id: "summary-0".to_owned(),
                    kind: bcode_session_models::ReasoningContentKind::Summary,
                    role: bcode_session_models::ReasoningContentRole::Milestone,
                    order: 0,
                    text: "milestone".to_owned(),
                }],
                opaque: true,
            },
        },
    };

    let encoded = serde_json::to_string(&item).expect("serialize reasoning item");
    let decoded: TranscriptViewItem =
        serde_json::from_str(&encoded).expect("deserialize reasoning item");
    assert_eq!(decoded, item);
    assert!(!encoded.contains("encrypted_content"));
    assert!(!encoded.contains("provider_state"));
    assert!(!encoded.contains(sentinel));
}

#[test]
fn reasoning_presentation_policy_has_stable_wire_values() {
    for (policy, wire) in [
        (ReasoningPresentationPolicy::All, r#""all""#),
        (ReasoningPresentationPolicy::Summary, r#""summary""#),
        (ReasoningPresentationPolicy::Raw, r#""raw""#),
        (ReasoningPresentationPolicy::Hidden, r#""hidden""#),
    ] {
        let encoded = serde_json::to_string(&policy).expect("serialize reasoning policy");
        assert_eq!(encoded, wire);
        assert_eq!(
            serde_json::from_str::<ReasoningPresentationPolicy>(wire)
                .expect("deserialize reasoning policy"),
            policy
        );
    }
}

#[test]
fn empty_snapshot_shows_reasoning_by_default() {
    let snapshot = SessionViewSnapshot::empty();
    assert!(snapshot.thinking.visible);
    assert_eq!(
        snapshot.connection_status,
        SessionConnectionViewStatus::Disconnected
    );
}

#[test]
fn legacy_snapshot_defaults_connection_catalog_and_notice_state() {
    let mut value = serde_json::to_value(SessionViewSnapshot::empty()).expect("serialize snapshot");
    let object = value.as_object_mut().expect("snapshot object");
    object.remove("connection_status");
    object.remove("catalog_status");
    object.remove("notice");

    let snapshot: SessionViewSnapshot =
        serde_json::from_value(value).expect("deserialize legacy snapshot");

    assert_eq!(
        snapshot.connection_status,
        SessionConnectionViewStatus::Disconnected
    );
    assert_eq!(
        snapshot.catalog_status,
        SessionCatalogViewStatus::NotStarted
    );
    assert!(snapshot.notice.is_none());
}

#[test]
fn runtime_work_status_label_preserves_semantic_activity() {
    let running = |id: &str, kind, label: &str, message: Option<&str>| RuntimeWorkView {
        work_id: WorkId::new(id),
        kind,
        label: label.to_owned(),
        status: RuntimeWorkStatus::Running,
        cancellable: true,
        message: message.map(ToOwned::to_owned),
        completed_units: None,
        total_units: None,
        updated_at_ms: None,
    };
    let one = running("work-1", RuntimeWorkKind::Tool, "shell", Some("halfway"));
    assert_eq!(
        runtime_work_status_label(std::slice::from_ref(&one)).as_deref(),
        Some("running tool: shell — halfway")
    );
    let two = running("work-2", RuntimeWorkKind::Tool, "web", None);
    assert_eq!(
        runtime_work_status_label(&[one, two]).as_deref(),
        Some("running 2 tools")
    );
    let queued = RuntimeWorkView {
        work_id: WorkId::new("a-work"),
        kind: RuntimeWorkKind::ModelTurn,
        label: "queued turn".to_owned(),
        status: RuntimeWorkStatus::Queued,
        cancellable: false,
        message: None,
        completed_units: None,
        total_units: None,
        updated_at_ms: None,
    };
    let plugin = running("z-work", RuntimeWorkKind::PluginInvocation, "plugin", None);
    assert_eq!(
        runtime_work_status_label(&[plugin, queued]).as_deref(),
        Some("queued: queued turn")
    );

    let cancelling = RuntimeWorkView {
        status: RuntimeWorkStatus::Cancelling,
        ..running("work-3", RuntimeWorkKind::PluginInvocation, "plugin", None)
    };
    assert_eq!(
        runtime_work_status_label(&[cancelling]).as_deref(),
        Some("cancelling: plugin")
    );
}

#[test]
fn runtime_work_view_deserializes_legacy_shape() {
    let work: RuntimeWorkView = serde_json::from_value(serde_json::json!({
        "work_id": "legacy-work",
        "status": "running",
        "message": "legacy",
        "completed_units": null,
        "total_units": null,
        "updated_at_ms": null
    }))
    .expect("legacy runtime work view");

    assert_eq!(work.kind, RuntimeWorkKind::Tool);
    assert_eq!(work.label, "");
    assert!(!work.cancellable);
}

#[test]
fn permission_view_deserializes_legacy_shape() {
    let permission: PermissionView = serde_json::from_value(serde_json::json!({
        "permission_id": "permission-1",
        "tool_call_id": "call-1",
        "title": "Permission requested",
        "detail": null,
        "resolved": false,
        "approved": null,
        "can_remember": false
    }))
    .expect("legacy permission view");

    assert_eq!(permission.session_id, None);
    assert_eq!(permission.tool_name, "");
    assert_eq!(permission.batch, None);
    assert_eq!(permission.policy_source, None);
}

#[test]
fn legacy_tool_request_draft_view_defaults_to_request_placement() {
    let draft: ToolRequestDraftView = serde_json::from_value(serde_json::json!({
        "turn_id": "turn-1",
        "tool_call_id": "call-1",
        "tool_name": "third_party.tool",
        "producer_plugin_id": null,
        "schema": "third-party.draft",
        "schema_version": 1,
        "generation": 1,
        "revision": 1,
        "argument_bytes": 2,
        "preview_start_offset": 0,
        "preview": "{}",
        "truncated": false
    }))
    .expect("legacy request draft view should decode");

    assert_eq!(
        draft.placement,
        bcode_session_models::ToolContributionPlacement::Request
    );
}

#[test]
fn transcript_patch_replaces_tool_slot_across_draft_and_final_schemas() {
    let id = TranscriptViewItemId::tool_presentation_slot(
        "call-1",
        bcode_session_models::ToolContributionPlacement::Result,
        None,
    );
    let draft = TranscriptViewItem {
        id: id.clone(),
        revision: 1,
        sequence: None,
        timestamp_ms: None,
        streaming: true,
        kind: TranscriptViewItemKind::ToolRequestDraft {
            draft: ToolRequestDraftView {
                turn_id: "turn-1".to_owned(),
                tool_call_id: "call-1".to_owned(),
                tool_name: "filesystem.write".to_owned(),
                producer_plugin_id: Some("bcode.filesystem".to_owned()),
                schema: "bcode.filesystem.request-draft.write".to_owned(),
                schema_version: 1,
                placement: bcode_session_models::ToolContributionPlacement::Result,
                generation: 1,
                revision: 1,
                argument_bytes: 1,
                preview_start_offset: 0,
                preview: "{".to_owned(),
                truncated: false,
            },
        },
    };
    let final_item = TranscriptViewItem {
        id,
        revision: 2,
        sequence: None,
        timestamp_ms: Some(1),
        streaming: false,
        kind: TranscriptViewItemKind::SystemMessage {
            message: ChatMessageView::plain("authoritative final artifact"),
        },
    };
    let base = TranscriptViewDocument {
        revision: 1,
        items: vec![draft],
        source_start_sequence: None,
        source_end_sequence: None,
        has_older_history: false,
        has_newer_history: false,
    };
    let next = TranscriptViewDocument {
        revision: 2,
        items: vec![final_item.clone()],
        source_start_sequence: None,
        source_end_sequence: None,
        has_older_history: false,
        has_newer_history: false,
    };

    let patch = SessionViewPatch::transcript_between(1, 2, None, &base, &next);
    assert_eq!(
        patch.transcript,
        vec![TranscriptViewPatchOp::Replace { item: final_item }]
    );
}

#[test]
fn transcript_patch_appends_and_replaces_prefix_compatible_items() {
    let mut base = transcript_document(3, [transcript_item("one", 1, "old")]);
    let next = transcript_document(
        4,
        [
            transcript_item_with_revision("one", 1, 2, "new"),
            transcript_item("two", 2, "append"),
        ],
    );

    let patch = SessionViewPatch::transcript_between(3, 4, None, &base, &next);
    assert_eq!(
        patch.transcript,
        vec![
            TranscriptViewPatchOp::Replace {
                item: transcript_item_with_revision("one", 1, 2, "new")
            },
            TranscriptViewPatchOp::Append {
                item: transcript_item("two", 2, "append")
            },
        ]
    );

    base.apply_patch(&patch).expect("patch applies");
    assert_eq!(base, next);
}

#[test]
fn transcript_patch_removes_middle_item_without_reset() {
    let mut base = transcript_document(
        3,
        [
            transcript_item("one", 1, "one"),
            transcript_item("two", 2, "remove"),
            transcript_item("three", 3, "three"),
        ],
    );
    let next = transcript_document(
        4,
        [
            transcript_item("one", 1, "one"),
            transcript_item("three", 3, "three"),
        ],
    );

    let patch = SessionViewPatch::transcript_between(3, 4, None, &base, &next);
    assert_eq!(
        patch.transcript,
        vec![TranscriptViewPatchOp::Remove {
            id: TranscriptViewItemId::new("two")
        }]
    );
    base.apply_patch(&patch).expect("middle removal applies");
    assert_eq!(base, next);
}

#[test]
fn transcript_patch_rejects_non_monotonic_item_replacement() {
    let mut base = transcript_document(3, [transcript_item("one", 2, "old")]);
    let next = transcript_document(4, [transcript_item("one", 2, "new")]);
    let patch = SessionViewPatch::transcript_between(3, 4, None, &base, &next);

    assert_eq!(
        base.apply_patch(&patch),
        Err(TranscriptViewPatchError::NonMonotonicItemRevision {
            id: TranscriptViewItemId::new("one"),
            current: 2,
            replacement: 2,
        })
    );
}

#[test]
fn transcript_patch_removes_tail_items() {
    let mut base = transcript_document(
        3,
        [
            transcript_item("one", 1, "one"),
            transcript_item("two", 2, "remove"),
        ],
    );
    let next = transcript_document(4, [transcript_item("one", 1, "one")]);

    let patch = SessionViewPatch::transcript_between(3, 4, None, &base, &next);
    assert_eq!(
        patch.transcript,
        vec![TranscriptViewPatchOp::Remove {
            id: TranscriptViewItemId::new("two")
        }]
    );

    base.apply_patch(&patch).expect("patch applies");
    assert_eq!(base, next);
}

#[test]
fn transcript_patch_resets_when_window_metadata_changes() {
    let base = transcript_document(3, [transcript_item("one", 1, "old")]);
    let mut next = transcript_document(4, [transcript_item("two", 2, "new")]);
    next.has_older_history = true;

    let patch = SessionViewPatch::transcript_between(3, 4, None, &base, &next);
    assert_eq!(
        patch.transcript,
        vec![TranscriptViewPatchOp::Reset {
            document: next.clone()
        }]
    );

    let mut applied = base;
    applied.apply_patch(&patch).expect("reset patch applies");
    assert_eq!(applied, next);
}

#[test]
fn transcript_patch_rejects_wrong_base_revision() {
    let mut base = transcript_document(3, [transcript_item("one", 1, "old")]);
    let next = transcript_document(5, [transcript_item("one", 1, "new")]);
    let patch = SessionViewPatch::transcript_between(4, 5, None, &base, &next);

    assert_eq!(
        base.apply_patch(&patch),
        Err(TranscriptViewPatchError::RevisionMismatch {
            expected: 4,
            actual: 3,
        })
    );
}

#[test]
fn transcript_patch_rejects_reset_revision_mismatch() {
    let mut base = transcript_document(3, [transcript_item("one", 1, "old")]);
    let patch = SessionViewPatch {
        transcript: vec![TranscriptViewPatchOp::Reset {
            document: transcript_document(99, [transcript_item("one", 1, "new")]),
        }],
        ..SessionViewPatch::empty(3, 4)
    };

    assert_eq!(
        base.apply_patch(&patch),
        Err(TranscriptViewPatchError::ResetRevisionMismatch {
            expected: 4,
            actual: 99,
        })
    );
}

#[test]
fn snapshot_patch_rejects_reset_revision_mismatch() {
    let mut base = SessionViewSnapshot::empty();
    base.revision = 3;
    let mut reset = base.clone();
    reset.revision = 99;
    let patch = SessionViewPatch {
        reset: Some(Box::new(reset)),
        ..SessionViewPatch::empty(3, 4)
    };

    assert_eq!(
        base.apply_patch(&patch),
        Err(TranscriptViewPatchError::ResetRevisionMismatch {
            expected: 4,
            actual: 99,
        })
    );
}

#[test]
fn session_view_patch_deserializes_without_reset_field() {
    let patch: SessionViewPatch = serde_json::from_value(serde_json::json!({
        "schema_version": SessionViewPatch::SCHEMA_VERSION,
        "base_revision": 1,
        "revision": 2,
        "session_id": null,
        "transcript": [],
        "contributions": {},
        "active_exchanges": {},
        "active_invocations": {},
        "tools": {},
        "permissions": [],
        "runtime_work": [],
        "active_skills": null,
        "plugin_status": {},
        "composer": null,
        "thinking": null,
        "runtime": null,
        "interactions": []
    }))
    .expect("legacy patch without reset");

    assert!(patch.reset.is_none());
}

#[test]
fn transcript_patch_rejects_missing_and_duplicate_items() {
    let mut base = transcript_document(1, [transcript_item("one", 1, "one")]);
    let duplicate = SessionViewPatch {
        transcript: vec![TranscriptViewPatchOp::Append {
            item: transcript_item("one", 1, "again"),
        }],
        ..SessionViewPatch::empty(1, 2)
    };
    assert_eq!(
        base.apply_patch(&duplicate),
        Err(TranscriptViewPatchError::DuplicateItem {
            id: TranscriptViewItemId::new("one")
        })
    );

    let missing = SessionViewPatch {
        transcript: vec![TranscriptViewPatchOp::Remove {
            id: TranscriptViewItemId::new("missing"),
        }],
        ..SessionViewPatch::empty(1, 2)
    };
    assert_eq!(
        base.apply_patch(&missing),
        Err(TranscriptViewPatchError::MissingItem {
            id: TranscriptViewItemId::new("missing")
        })
    );
}

#[test]
fn snapshot_patch_applies_transcript_only_incrementally() {
    let mut base = SessionViewSnapshot::empty();
    base.revision = 1;
    base.transcript = transcript_document(1, [transcript_item("one", 1, "old")]);

    let mut next = base.clone();
    next.revision = 2;
    next.transcript = transcript_document(
        2,
        [
            transcript_item_with_revision("one", 1, 2, "new"),
            transcript_item("two", 2, "append"),
        ],
    );

    let patch = SessionViewPatch::between_snapshots(&base, &next);
    assert!(patch.reset.is_none());
    assert_eq!(patch.transcript.len(), 2);

    base.apply_patch(&patch).expect("snapshot patch applies");
    assert_eq!(base, next);
}

#[test]
fn snapshot_patch_keeps_slot_replacement_incremental_with_contribution_update() {
    let contribution = |sequence, label: &str| bcode_session_models::ToolContributionEvent {
        invocation_id: "call-1".to_owned(),
        contribution_id: "request".to_owned(),
        sequence,
        producer_id: "test.plugin".to_owned(),
        schema: "test.request".to_owned(),
        schema_version: 1,
        operation: bcode_session_models::ToolContributionOperation::Upsert,
        persistence: bcode_session_models::ToolContributionPersistence::Durable,
        artifact: None,
        payload: serde_json::json!({"label": label}),
    };
    let mut base = SessionViewSnapshot::empty();
    base.revision = 1;
    let unchanged = transcript_item("unchanged", 2, "other");
    base.transcript = transcript_document(
        1,
        [
            transcript_item("tool-slot:call-1:request", 1, "compact"),
            unchanged.clone(),
        ],
    );
    base.contributions
        .insert("call-1:request".to_owned(), contribution(1, "compact"));

    let mut next = base.clone();
    next.revision = 2;
    next.transcript = transcript_document(
        2,
        [
            transcript_item_with_revision("tool-slot:call-1:request", 1, 2, "rich"),
            unchanged,
        ],
    );
    next.contributions
        .insert("call-1:request".to_owned(), contribution(2, "rich"));

    let patch = SessionViewPatch::between_snapshots(&base, &next);
    assert!(patch.reset.is_none());
    assert!(matches!(
        patch.transcript.as_slice(),
        [TranscriptViewPatchOp::Replace { item }]
            if item.id == TranscriptViewItemId::new("tool-slot:call-1:request")
    ));
    assert_eq!(patch.contributions.len(), 1);
    assert_eq!(
        patch.transcript.len(),
        1,
        "unchanged sibling emits no patch operation"
    );
    base.apply_patch(&patch)
        .expect("incremental slot patch applies");
    assert_eq!(base, next);
}

#[test]
fn snapshot_patch_removes_transient_contribution_incrementally() {
    let contribution = bcode_session_models::ToolContributionEvent {
        invocation_id: "call-1".to_owned(),
        contribution_id: "progress".to_owned(),
        sequence: 1,
        producer_id: "test.plugin".to_owned(),
        schema: "test.progress".to_owned(),
        schema_version: 1,
        operation: bcode_session_models::ToolContributionOperation::Upsert,
        persistence: bcode_session_models::ToolContributionPersistence::Transient,
        artifact: None,
        payload: serde_json::json!({"frame": 1}),
    };
    let key = "call-1:progress".to_owned();
    let mut item = transcript_item("tool-slot:call-1:progress", 1, "progress");
    item.sequence = None;
    let mut base = SessionViewSnapshot::empty();
    base.revision = 1;
    base.transcript = transcript_document(1, [item]);
    base.contributions.insert(key.clone(), contribution);

    let mut next = base.clone();
    next.revision = 2;
    next.transcript = transcript_document(2, []);
    next.contributions.remove(&key);

    let patch = SessionViewPatch::between_snapshots(&base, &next);
    assert!(patch.reset.is_none());
    assert!(patch.contributions.is_empty());
    assert_eq!(patch.removed_contributions, [key]);
    assert!(matches!(
        patch.transcript.as_slice(),
        [TranscriptViewPatchOp::Remove { id }]
            if id == &TranscriptViewItemId::new("tool-slot:call-1:progress")
    ));

    base.apply_patch(&patch)
        .expect("incremental transient removal applies");
    assert_eq!(base, next);
}

#[test]
fn snapshot_patch_resets_when_non_transcript_state_changes() {
    let mut base = SessionViewSnapshot::empty();
    base.revision = 1;
    base.transcript = transcript_document(1, [transcript_item("one", 1, "old")]);

    let mut next = base.clone();
    next.revision = 2;
    next.title = Some("renamed".to_owned());
    next.transcript = transcript_document(2, [transcript_item("one", 1, "new")]);

    let patch = SessionViewPatch::between_snapshots(&base, &next);
    assert_eq!(patch.reset.as_deref(), Some(&next));
    assert!(patch.transcript.is_empty());

    base.apply_patch(&patch).expect("reset patch applies");
    assert_eq!(base, next);
}

#[test]
fn patch_size_measurements_cover_incremental_and_reset_workloads() {
    let mut base = SessionViewSnapshot::empty();
    base.revision = 100;
    base.transcript = transcript_document(
        100,
        std::array::from_fn::<_, 100, _>(|index| {
            transcript_item(
                &format!("item-{index}"),
                u64::try_from(index).expect("index") + 1,
                &"existing transcript content ".repeat(8),
            )
        }),
    );

    let mut appended = base.clone();
    appended.revision = 101;
    appended.transcript.revision = 101;
    appended.transcript.items.push(transcript_item(
        "item-100",
        101,
        &"new transcript content ".repeat(8),
    ));
    appended.transcript.refresh_source_bounds();
    let append_patch = SessionViewPatch::between_snapshots(&base, &appended);
    assert!(append_patch.reset.is_none());
    assert_serialized_patch_smaller("append", &append_patch, &appended);

    let mut replaced = appended.clone();
    replaced.revision = 102;
    replaced.transcript.revision = 102;
    replaced.transcript.items[100] =
        transcript_item("item-100", 101, &"streaming transcript content ".repeat(16));
    replaced.transcript.items[100].revision = 102;
    let replace_patch = SessionViewPatch::between_snapshots(&appended, &replaced);
    assert!(replace_patch.reset.is_none());
    assert_serialized_patch_smaller("replace", &replace_patch, &replaced);

    let mut reset = replaced.clone();
    reset.revision = 103;
    reset.transcript.revision = 103;
    reset.title = Some("renamed session".to_owned());
    let reset_patch = SessionViewPatch::between_snapshots(&replaced, &reset);
    assert!(reset_patch.reset.is_some());
    let reset_patch_bytes = serde_json::to_vec(&reset_patch)
        .expect("reset patch serializes")
        .len();
    let reset_snapshot_bytes = serde_json::to_vec(&reset)
        .expect("reset snapshot serializes")
        .len();
    assert!(
        reset_patch_bytes >= reset_snapshot_bytes,
        "reset patch ({reset_patch_bytes}) should not be treated as a transport optimization over its snapshot ({reset_snapshot_bytes})"
    );
}

fn assert_serialized_patch_smaller(
    workload: &str,
    patch: &SessionViewPatch,
    snapshot: &SessionViewSnapshot,
) {
    let patch_bytes = serde_json::to_vec(patch).expect("patch serializes").len();
    let snapshot_bytes = serde_json::to_vec(snapshot)
        .expect("snapshot serializes")
        .len();
    assert!(
        patch_bytes.saturating_mul(4) < snapshot_bytes,
        "{workload} patch ({patch_bytes}) should be at least 4x smaller than snapshot ({snapshot_bytes})"
    );
}

fn transcript_document<const N: usize>(
    revision: ViewRevision,
    items: [TranscriptViewItem; N],
) -> TranscriptViewDocument {
    let mut document = TranscriptViewDocument {
        revision,
        items: items.into(),
        source_start_sequence: None,
        source_end_sequence: None,
        has_older_history: false,
        has_newer_history: false,
    };
    document.refresh_source_bounds();
    document
}

fn transcript_item(id: &str, sequence: u64, text: &str) -> TranscriptViewItem {
    transcript_item_with_revision(id, sequence, sequence, text)
}

fn transcript_item_with_revision(
    id: &str,
    sequence: u64,
    revision: ViewRevision,
    text: &str,
) -> TranscriptViewItem {
    TranscriptViewItem {
        id: TranscriptViewItemId::new(id),
        sequence: Some(sequence),
        timestamp_ms: Some(sequence.saturating_mul(10)),
        revision,
        streaming: false,
        kind: TranscriptViewItemKind::SystemMessage {
            message: ChatMessageView::plain(text.to_owned()),
        },
    }
}
