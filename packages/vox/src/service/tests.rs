use super::*;

#[derive(Default)]
struct Fake {
    actions: Vec<Action>,
    fail: bool,
}

impl Handler for Fake {
    fn action(&mut self, action: Action) -> Result<Status> {
        self.actions.push(action);
        if self.fail {
            Err(Error::Backend("private transcript and credential".into()))
        } else {
            Ok(Status::Recording { operation: 7 })
        }
    }

    fn status(&self) -> Status {
        Status::Failed {
            message: "private transcript and credential".into(),
        }
    }
}

const INSTANCE: &str = "11111111111111111111111111111111";
const CLIENT_ID: &str = "22222222222222222222222222222222";

fn id(sequence: u64) -> OperationId {
    OperationId {
        instance: INSTANCE.into(),
        client: CLIENT_ID.into(),
        sequence,
    }
}

const fn request(command: Command) -> Request {
    Request {
        version: VERSION,
        command,
    }
}

fn action_request(sequence: u64, action: Action) -> Request {
    request(Command::Action {
        id: id(sequence),
        action,
    })
}

#[test]
fn duplicate_mutation_returns_original_result_without_another_action() {
    let mut server = Dispatcher::new(Fake::default(), INSTANCE.into());
    let first = server.dispatch(&action_request(1, Action::Toggle));
    let repeated = server.dispatch(&action_request(1, Action::Toggle));
    assert_eq!(first.result, repeated.result);
    assert_eq!(server.handler.actions, [Action::Toggle]);
}

#[test]
fn conflicting_duplicate_and_older_sequence_never_execute() {
    let mut server = Dispatcher::new(Fake::default(), INSTANCE.into());
    server.dispatch(&action_request(4, Action::Stop));
    assert_eq!(
        server.dispatch(&action_request(4, Action::Start)).result,
        Err(Failure::Conflict)
    );
    assert_eq!(
        server.dispatch(&action_request(3, Action::Toggle)).result,
        Err(Failure::Expired)
    );
    assert_eq!(server.handler.actions, [Action::Stop]);
    assert!(
        server
            .dispatch(&action_request(5, Action::Cancel))
            .result
            .is_ok()
    );
    assert_eq!(server.handler.actions, [Action::Stop, Action::Cancel]);
}

#[test]
fn errors_are_cached_and_never_disclose_runtime_error_text() {
    let mut server = Dispatcher::new(
        Fake {
            fail: true,
            ..Fake::default()
        },
        INSTANCE.into(),
    );
    let first = server.dispatch(&action_request(1, Action::Stop));
    server.handler.fail = false;
    let repeated = server.dispatch(&action_request(1, Action::Stop));
    assert_eq!(first.result, Err(Failure::Backend));
    assert_eq!(repeated.result, first.result);
    assert_eq!(server.handler.actions, [Action::Stop]);
    let encoded = serde_json::to_string(&first).unwrap();
    assert!(!encoded.contains("private transcript"));
    assert!(!encoded.contains("credential"));
}

#[test]
fn polling_is_redacted_and_does_not_consume_mutation_capacity() {
    let mut server = Dispatcher::new(Fake::default(), INSTANCE.into());
    for _ in 0..10 {
        let response = server.dispatch(&request(Command::Status));
        assert_eq!(
            response.result,
            Ok(Status::Failed {
                message: "Vox operation failed; check local configuration and capabilities".into(),
            })
        );
    }
    assert!(server.clients.is_empty());
    assert!(server.handler.actions.is_empty());
}

#[test]
fn foreign_service_and_bad_mutation_ids_fail_closed() {
    let mut server = Dispatcher::new(Fake::default(), INSTANCE.into());
    for invalid_id in [
        OperationId {
            instance: "33333333333333333333333333333333".into(),
            ..id(1)
        },
        OperationId {
            client: "not-a-client".into(),
            ..id(1)
        },
        id(0),
    ] {
        let response = server.dispatch(&request(Command::Action {
            id: invalid_id,
            action: Action::Start,
        }));
        assert_eq!(response.result, Err(Failure::Identity));
    }
    assert!(server.handler.actions.is_empty());
    assert!(server.clients.is_empty());
}

#[test]
fn unknown_version_cannot_mutate() {
    let mut server = Dispatcher::new(Fake::default(), INSTANCE.into());
    let mut unknown = action_request(1, Action::Toggle);
    unknown.version = VERSION + 1;
    assert_eq!(server.dispatch(&unknown).result, Err(Failure::Version));
    assert!(server.handler.actions.is_empty());
}

#[test]
fn capacity_does_not_evict_a_mutation_high_water_mark() {
    let mut server = Dispatcher::new(Fake::default(), INSTANCE.into());
    for client in 0..MAX_CLIENTS {
        let mutation = OperationId {
            client: format!("{client:032x}"),
            ..id(1)
        };
        assert!(
            server
                .dispatch(&request(Command::Action {
                    id: mutation,
                    action: Action::Cancel
                }))
                .result
                .is_ok()
        );
    }
    assert_eq!(
        server.dispatch(&action_request(1, Action::Start)).result,
        Err(Failure::Capacity)
    );
    assert_eq!(server.handler.actions.len(), MAX_CLIENTS);
    let retained = OperationId {
        client: format!("{:032x}", 0),
        ..id(1)
    };
    assert!(
        server
            .dispatch(&request(Command::Action {
                id: retained,
                action: Action::Cancel
            }))
            .result
            .is_ok()
    );
    assert_eq!(server.handler.actions.len(), MAX_CLIENTS);
}

fn saturated_server() -> Dispatcher<Fake> {
    let mut server = Dispatcher::new(Fake::default(), INSTANCE.into());
    for client in 0..MAX_CLIENTS {
        let mutation = OperationId {
            client: format!("{client:032x}"),
            ..id(1)
        };
        assert!(
            server
                .dispatch(&request(Command::Action {
                    id: mutation,
                    action: Action::Start,
                }))
                .result
                .is_ok()
        );
    }
    server
}

#[test]
fn saturated_cancel_drains_without_evicting_or_admitting_later_work() {
    let mut server = saturated_server();
    let cancel = action_request(1, Action::Cancel);
    assert!(server.dispatch(&cancel).result.is_ok());
    assert!(server.draining);
    assert_eq!(server.clients.len(), MAX_CLIENTS);
    assert!(!server.clients.contains_key(CLIENT_ID));
    assert_eq!(server.handler.actions.last(), Some(&Action::Cancel));

    // A retained identity cannot restart work between repeated overflow cancels.
    let retained = OperationId {
        client: format!("{:032x}", 0),
        ..id(2)
    };
    for action in [Action::Start, Action::Stop, Action::Toggle] {
        assert_eq!(
            server
                .dispatch(&request(Command::Action {
                    id: retained.clone(),
                    action,
                }))
                .result,
            Err(Failure::Capacity)
        );
    }
    assert_eq!(
        server.dispatch(&action_request(2, Action::Start)).result,
        Err(Failure::Capacity)
    );
    assert!(server.dispatch(&cancel).result.is_ok());
    assert_eq!(server.handler.actions.len(), MAX_CLIENTS + 2);
    // More one-shot CLI clients must not consume extra dedup storage or remove
    // the escape path. Reusing an untracked cancel ID for unsafe work also fails.
    for sequence in 3..35 {
        let escape = OperationId {
            client: format!("{sequence:016x}ffffffffffffffff"),
            ..id(sequence)
        };
        assert!(
            server
                .dispatch(&request(Command::Action {
                    id: escape.clone(),
                    action: Action::Cancel,
                }))
                .result
                .is_ok()
        );
        assert_eq!(
            server
                .dispatch(&request(Command::Action {
                    id: escape,
                    action: Action::Toggle,
                }))
                .result,
            Err(Failure::Capacity)
        );
    }
    assert_eq!(server.clients.len(), MAX_CLIENTS);
    assert!(server.dispatch(&request(Command::Status)).result.is_ok());
}

#[test]
fn saturated_retained_replay_conflict_and_expiry_still_apply() {
    let mut server = saturated_server();
    assert!(
        server
            .dispatch(&action_request(1, Action::Cancel))
            .result
            .is_ok()
    );
    let retained = OperationId {
        client: format!("{:032x}", 0),
        ..id(1)
    };
    let calls = server.handler.actions.len();
    assert!(
        server
            .dispatch(&request(Command::Action {
                id: retained.clone(),
                action: Action::Start,
            }))
            .result
            .is_ok()
    );
    assert_eq!(
        server
            .dispatch(&request(Command::Action {
                id: retained.clone(),
                action: Action::Cancel,
            }))
            .result,
        Err(Failure::Conflict)
    );
    assert_eq!(server.handler.actions.len(), calls);

    let newer = OperationId {
        sequence: 2,
        ..retained.clone()
    };
    assert!(
        server
            .dispatch(&request(Command::Action {
                id: newer.clone(),
                action: Action::Cancel,
            }))
            .result
            .is_ok()
    );
    let calls = server.handler.actions.len();
    assert!(
        server
            .dispatch(&request(Command::Action {
                id: newer,
                action: Action::Cancel,
            }))
            .result
            .is_ok()
    );
    assert_eq!(
        server
            .dispatch(&request(Command::Shutdown { id: retained }))
            .result,
        Err(Failure::Expired)
    );
    assert_eq!(server.handler.actions.len(), calls);
}

#[test]
fn saturated_shutdown_remains_accessible_and_settles_once() {
    let mut server = saturated_server();
    let shutdown = request(Command::Shutdown { id: id(1) });
    assert!(server.dispatch(&shutdown).result.is_ok());
    assert!(server.stopping);
    assert!(server.draining);
    let calls = server.handler.actions.len();
    assert!(server.dispatch(&shutdown).result.is_ok());
    assert_eq!(server.handler.actions.len(), calls);
    assert_eq!(server.clients.len(), MAX_CLIENTS);
}

#[test]
fn saturated_failed_escape_keeps_cancel_and_shutdown_accessible() {
    let mut server = saturated_server();
    server.handler.fail = true;
    assert_eq!(
        server.dispatch(&action_request(1, Action::Cancel)).result,
        Err(Failure::Backend)
    );
    assert!(server.draining);
    assert!(!server.stopping);
    assert_eq!(
        server
            .dispatch(&request(Command::Shutdown { id: id(2) }))
            .result,
        Err(Failure::Backend)
    );
    assert!(!server.stopping);
    server.handler.fail = false;
    assert!(
        server
            .dispatch(&action_request(1, Action::Cancel))
            .result
            .is_ok()
    );
    assert!(
        server
            .dispatch(&request(Command::Shutdown { id: id(2) }))
            .result
            .is_ok()
    );
    assert!(server.stopping);
    assert_eq!(server.clients.len(), MAX_CLIENTS);
}

#[test]
fn saturated_escape_still_requires_current_instance_and_valid_identity() {
    let mut server = saturated_server();
    let foreign = OperationId {
        instance: "33333333333333333333333333333333".into(),
        ..id(1)
    };
    assert_eq!(
        server
            .dispatch(&request(Command::Shutdown { id: foreign }))
            .result,
        Err(Failure::Identity)
    );
    assert_eq!(
        server.dispatch(&action_request(0, Action::Cancel)).result,
        Err(Failure::Identity)
    );
    assert!(!server.draining);
    assert_eq!(server.handler.actions.len(), MAX_CLIENTS);
}

#[test]
fn graceful_shutdown_requires_successful_cancel() {
    let mut server = Dispatcher::new(Fake::default(), INSTANCE.into());
    assert!(
        server
            .dispatch(&request(Command::Shutdown { id: id(1) }))
            .result
            .is_ok()
    );
    assert!(server.stopping);
    assert_eq!(server.handler.actions, [Action::Cancel]);
    assert!(
        server
            .dispatch(&request(Command::Shutdown { id: id(1) }))
            .result
            .is_ok()
    );
    assert_eq!(server.handler.actions, [Action::Cancel]);

    let mut failed = Dispatcher::new(
        Fake {
            fail: true,
            ..Fake::default()
        },
        INSTANCE.into(),
    );
    assert_eq!(
        failed
            .dispatch(&request(Command::Shutdown { id: id(1) }))
            .result,
        Err(Failure::Backend)
    );
    assert!(!failed.stopping);
}

#[test]
fn protocol_round_trip_and_unknown_fields_are_explicit() {
    let input = action_request(9, Action::Stop);
    let bytes = serde_json::to_vec(&input).unwrap();
    let decoded: Request = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(decoded.version, VERSION);
    assert_eq!(decoded.command, input.command);
    assert!(
        serde_json::from_str::<Request>(
            r#"{"version":1,"command":{"command":"status"},"transcript":"not allowed"}"#,
        )
        .is_err()
    );
    assert!(
        serde_json::from_str::<Request>(
            r#"{"version":1,"command":{"command":"type_text","text":"not allowed"}}"#,
        )
        .is_err()
    );
}
