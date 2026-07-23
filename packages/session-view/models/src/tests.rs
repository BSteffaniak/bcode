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
fn empty_snapshot_shows_reasoning_by_default() {
    assert!(SessionViewSnapshot::empty().thinking.visible);
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
fn transcript_patch_appends_and_replaces_prefix_compatible_items() {
    let mut base = transcript_document(3, [transcript_item("one", 1, "old")]);
    let next = transcript_document(
        4,
        [
            transcript_item("one", 1, "new"),
            transcript_item("two", 2, "append"),
        ],
    );

    let patch = SessionViewPatch::transcript_between(3, 4, None, &base, &next);
    assert_eq!(
        patch.transcript,
        vec![
            TranscriptViewPatchOp::Replace {
                item: transcript_item("one", 1, "new")
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
            transcript_item("one", 1, "new"),
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
    TranscriptViewItem {
        id: TranscriptViewItemId::new(id),
        sequence: Some(sequence),
        timestamp_ms: Some(sequence.saturating_mul(10)),
        revision: sequence,
        streaming: false,
        kind: TranscriptViewItemKind::SystemMessage {
            message: ChatMessageView::plain(text.to_owned()),
        },
    }
}
