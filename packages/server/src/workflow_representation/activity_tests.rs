//! Behavioral tests for ephemeral own-session progress, independent of workflow topology.

use super::*;

fn registry(capacity: usize) -> SessionProgressRegistry {
    SessionProgressRegistry::new(NonZeroUsize::new(capacity).unwrap())
}

#[test]
fn snapshot_and_duplicate_delivery_do_not_refresh_host_progress_time() {
    let mut observations = registry(1);
    let id = SessionId::new();
    observations.observe_event(id, 1);
    let first = observations.snapshot(id);
    assert!(matches!(
        first.observation,
        SessionProgressObservation::Available {
            last_progress_at_ms: Some(_),
            ..
        }
    ));
    for _ in 0..100 {
        observations.observe_event(id, 1);
        assert_eq!(observations.snapshot(id), first);
    }
    observations.observe_tool_progress(id, "tool", 1);
    let tool = observations.snapshot(id);
    observations.observe_tool_progress(id, "tool", 1);
    observations.observe_tool_progress(id, "tool", 0);
    assert_eq!(observations.snapshot(id), tool);
}

#[test]
fn canonical_stage_sequences_ignore_duplicate_and_lower_delivery() {
    let mut observations = registry(1);
    let id = SessionId::new();
    observations.observe_event(id, 0);
    let first = observations.snapshot(id);
    for sequence in [0, 0] {
        observations.observe_event(id, sequence);
        assert_eq!(observations.snapshot(id), first);
    }
    observations.observe_event(id, 10);
    let next = observations.snapshot(id);
    assert_eq!(next.advanced_since(&first), Some(true));
    for sequence in [0, 9, 10] {
        observations.observe_event(id, sequence);
        assert_eq!(observations.snapshot(id), next);
    }
}

#[test]
fn tool_sequences_are_monotonic_per_invocation_and_session() {
    let mut observations = registry(2);
    let id = SessionId::new();
    observations.observe_tool_progress(id, "first", 5);
    let first = observations.snapshot(id);
    for sequence in [0, 4, 5, 5] {
        observations.observe_tool_progress(id, "first", sequence);
        assert_eq!(observations.snapshot(id), first);
    }
    observations.observe_tool_progress(id, "first", 6);
    let higher = observations.snapshot(id);
    assert_eq!(higher.advanced_since(&first), Some(true));
    observations.observe_tool_progress(id, "second", 0);
    let second = observations.snapshot(id);
    assert_eq!(second.advanced_since(&higher), Some(true));
    observations.observe_tool_progress(SessionId::new(), "first", 6);
    assert_eq!(observations.snapshot(id), second);
    observations.observe_event(id, 0);
    assert_eq!(
        observations.snapshot(id).advanced_since(&second),
        Some(true)
    );
}

#[test]
fn tool_fifo_eviction_fails_closed_and_plain_progress_cannot_restore_coverage() {
    let mut observations = SessionProgressRegistry::with_tool_capacity(
        NonZeroUsize::new(1).unwrap(),
        NonZeroUsize::new(2).unwrap(),
    );
    let id = SessionId::new();
    observations.observe_tool_progress(id, "first", 1);
    observations.observe_tool_progress(id, "second", 1);
    let baseline = observations.snapshot(id);
    let epoch = observations.sessions.get(&id).unwrap().epoch.clone();
    // Updating an invocation does not refresh FIFO retention.
    observations.observe_tool_progress(id, "first", 2);
    observations.observe_tool_progress(id, "third", 1);
    let progress = observations.sessions.get(&id).unwrap();
    assert_ne!(progress.epoch, epoch);
    assert!(!progress.tools.contains_key("first"));
    assert_eq!(progress.tools.len(), 2);
    assert_eq!(progress.tool_order.len(), 2);
    assert!(matches!(
        observations.snapshot(id).observation,
        SessionProgressObservation::Unavailable {
            reason: SessionProgressUnavailableReason::CoverageUnavailable
        }
    ));
    for sequence in 0..1000 {
        observations.observe_tool_progress(id, "first", sequence);
    }
    observations.observe(id);
    observations.observe_event(id, 1000);
    assert_eq!(observations.snapshot(id).advanced_since(&baseline), None);
    assert_eq!(observations.sessions.get(&id).unwrap().tools.len(), 2);
}

#[test]
fn invalid_tool_identities_do_not_allocate_unbounded_keys_or_report_progress() {
    let mut observations = registry(1);
    let id = SessionId::new();
    observations.observe_tool_progress(id, "", 0);
    observations.observe_tool_progress(id, &"x".repeat(257), 1);
    assert!(observations.sessions.get(&id).unwrap().tools.is_empty());
    assert!(matches!(
        observations.snapshot(id).observation,
        SessionProgressObservation::Unavailable { .. }
    ));
}

#[test]
fn polling_missing_session_does_not_create_an_idle_baseline() {
    let observations = registry(2);
    let id = SessionId::new();
    let first = observations.snapshot(id);
    for _ in 0..1000 {
        let next = observations.snapshot(id);
        assert_eq!(next, first);
        assert_eq!(next.advanced_since(&first), None);
    }
    assert!(observations.sessions.is_empty());
    assert!(observations.insertion_order.is_empty());
}

#[test]
fn streaming_progress_is_measurable_without_persisted_events_or_content() {
    let mut observations = registry(2);
    let id = SessionId::new();
    let unknown = observations.snapshot(id);
    observations.observe(id);
    let first = observations.snapshot(id);
    assert_eq!(first.advanced_since(&unknown), None);
    for _ in 0..1000 {
        assert_eq!(
            observations.snapshot(id).advanced_since(&first),
            Some(false)
        );
    }
    observations.observe(id);
    let second = observations.snapshot(id);
    assert_eq!(second.advanced_since(&first), Some(true));
    assert!(matches!(
        second.observation,
        SessionProgressObservation::Available { counter: 2, .. }
    ));
}

#[test]
fn progress_is_session_scoped_not_an_aggregate_of_unrelated_work() {
    let mut observations = registry(2);
    let first_id = SessionId::new();
    let second_id = SessionId::new();
    observations.observe(first_id);
    observations.observe(second_id);
    let first = observations.snapshot(first_id);
    let second = observations.snapshot(second_id);
    observations.observe(second_id);
    assert_eq!(
        observations.snapshot(first_id).advanced_since(&first),
        Some(false)
    );
    assert_eq!(
        observations.snapshot(second_id).advanced_since(&second),
        Some(true)
    );
    assert_eq!(
        observations.snapshot(second_id).advanced_since(&first),
        None
    );
}

#[test]
fn eviction_is_unknown_and_readmission_has_a_new_epoch() {
    let mut observations = registry(1);
    let first_id = SessionId::new();
    observations.observe(first_id);
    let first = observations.snapshot(first_id);
    observations.observe(SessionId::new());
    let evicted = observations.snapshot(first_id);
    assert!(matches!(
        evicted.observation,
        SessionProgressObservation::Unavailable {
            reason: SessionProgressUnavailableReason::NotObservedOrEvicted
        }
    ));
    assert_eq!(evicted.advanced_since(&first), None);
    observations.observe(first_id);
    assert_eq!(observations.snapshot(first_id).advanced_since(&first), None);
    assert_eq!(observations.sessions.len(), 1);
    assert_eq!(observations.insertion_order.len(), 1);
}

#[test]
fn reads_do_not_extend_retention_and_memory_tracks_only_the_budget() {
    let mut observations = registry(2);
    let first_id = SessionId::new();
    observations.observe(first_id);
    observations.observe(SessionId::new());
    for _ in 0..1000 {
        let _ = observations.snapshot(first_id);
    }
    for _ in 0..1000 {
        observations.observe(SessionId::new());
        assert_eq!(observations.sessions.len(), 2);
        assert_eq!(observations.insertion_order.len(), 2);
    }
    assert!(matches!(
        observations.snapshot(first_id).observation,
        SessionProgressObservation::Unavailable { .. }
    ));
}

#[test]
fn restart_and_counter_overflow_never_look_like_continuous_advancement() {
    let id = SessionId::new();
    let mut first_registry = registry(1);
    first_registry.observe(id);
    let first = first_registry.snapshot(id);
    let mut restarted = registry(1);
    assert_eq!(restarted.snapshot(id).advanced_since(&first), None);
    restarted.observe(id);
    assert_eq!(restarted.snapshot(id).advanced_since(&first), None);

    first_registry.sessions.get_mut(&id).unwrap().counter = u64::MAX;
    let before_overflow = first_registry.snapshot(id);
    first_registry.observe(id);
    let after_overflow = first_registry.snapshot(id);
    assert_eq!(after_overflow.advanced_since(&before_overflow), None);
    assert!(matches!(
        after_overflow.observation,
        SessionProgressObservation::Available { counter: 1, .. }
    ));
    first_registry.observe(id);
    assert_eq!(
        first_registry.snapshot(id).advanced_since(&after_overflow),
        Some(true)
    );
}
