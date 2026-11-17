use super::*;
use crate::definition::{PluginCallContext, PluginCreateContext};
use crate::tui::*;
use bmux_plugin_sdk::definition::{LoadedInstanceId, RuntimeScopeId};
use bmux_tui::{
    image::{
        ImageContribution, ImageFit, ImageKey, ImageLifecycle, ImagePayload, ImagePixelFormat,
        ImagePlacement,
    },
    selection::{SelectionContentId, SelectionFragment, SelectionScope},
};

#[test]
fn response_envelope_roundtrips_nested_outcomes_and_sequence_payloads() {
    let event = PluginTuiEventResult::consumed();
    let edit = PluginTuiFocusedTextFieldEditResult::rejected(
        bcode_plugin_models::composer::ComposerValidationError::Stale,
    );
    let responses = [
        Response::Event(event.clone()),
        Response::Edit(edit),
        Response::EditStatus(None),
        Response::EditStatus(Some(PluginTuiFocusedTextFieldEditResult {
            event: PluginTuiEventResult::consumed(),
            edit_result: FocusedTextFieldEditResult::Unknown,
        })),
        Response::Action(PluginTuiAction::None),
        Response::Text(vec!["界".to_owned()]),
        Response::Applied,
        Response::Stale { revision: 7 },
    ];
    for response in responses {
        let bytes = serde_json::to_vec(&response).unwrap();
        let decoded: Response = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(
            serde_json::to_value(&decoded).unwrap(),
            serde_json::to_value(&response).unwrap()
        );
    }
    let encoded = serde_json::to_value(Response::Event(event.clone())).unwrap();
    assert_eq!(encoded["response"], "event");
    assert_eq!(encoded["payload"], serde_json::to_value(event).unwrap());
    assert_eq!(
        serde_json::to_value(Response::Stale { revision: 7 }).unwrap(),
        serde_json::json!({"response":"stale","payload":{"revision":7}})
    );
    assert!(serde_json::from_str::<Response>(r#"{"outcome":"applied"}"#).is_err());
    assert!(serde_json::from_str::<Response>(r#"{"response":"future"}"#).is_err());
}

#[test]
fn author_codec_opens_async_factory_without_caller_tokio_runtime() {
    struct AsyncFactory;
    impl PluginTuiSurfaceFactory for AsyncFactory {
        fn surface_kind(&self) -> &'static str {
            "async-probe"
        }
        fn open(&self, request: PluginTuiSurfaceOpenRequest) -> PluginTuiSurfaceFuture {
            Box::pin(async move {
                tokio::time::sleep(std::time::Duration::from_millis(1)).await;
                Factory.open(request).await
            })
        }
    }
    fn async_registry() -> PluginTuiRegistry {
        let mut registry = PluginTuiRegistry::default();
        registry.register_factory(Box::new(AsyncFactory));
        registry
    }
    struct WakeThread(std::thread::Thread);
    impl std::task::Wake for WakeThread {
        fn wake(self: Arc<Self>) {
            self.0.unpark();
        }
    }
    let adapter = extension(
        async_registry,
        PluginCreateContext {
            runtime_scope: RuntimeScopeId::from_uuid(uuid::Uuid::new_v4()),
            instance: LoadedInstanceId::from_uuid(uuid::Uuid::new_v4()),
            role: crate::definition::ComponentRole::Frontend,
        },
    );
    let request = Request::Open {
        kind: "async-probe".to_owned(),
        request: PluginTuiSurfaceOpenRequest {
            instance_id: "without-host-runtime".to_owned(),
            repo_path: None,
            session_id: None,
            target: None,
            options: serde_json::Value::Null,
        },
    };
    let mut future = std::pin::pin!(adapter.invoke(call(), serde_json::to_vec(&request).unwrap()));
    let waker = std::task::Waker::from(Arc::new(WakeThread(std::thread::current())));
    let mut cx = std::task::Context::from_waker(&waker);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let bytes = loop {
        if let std::task::Poll::Ready(result) = std::future::Future::poll(future.as_mut(), &mut cx)
        {
            break result.unwrap();
        }
        assert!(
            std::time::Instant::now() < deadline,
            "author timer must progress"
        );
        std::thread::park_timeout(std::time::Duration::from_millis(10));
    };
    assert!(matches!(
        serde_json::from_slice::<Response>(&bytes).unwrap(),
        Response::Opened { .. }
    ));
    adapter.close().unwrap();
}

struct Factory;
impl PluginTuiSurfaceFactory for Factory {
    fn surface_kind(&self) -> &'static str {
        "probe"
    }
    fn open(&self, _: PluginTuiSurfaceOpenRequest) -> PluginTuiSurfaceFuture {
        Box::pin(async {
            Ok(Box::new(Probe {
                text: "界".to_owned(),
                updates: None,
                effect: PluginTuiAction::None,
            }) as BoxedPluginTuiSurface)
        })
    }
}
struct Probe {
    text: String,
    updates: Option<PluginTuiSurfaceUpdateReceiver>,
    effect: PluginTuiAction,
}
impl PluginTuiSurface for Probe {
    fn id(&self) -> &'static str {
        "probe"
    }
    fn title(&self) -> &'static str {
        "Probe"
    }
    fn preferred_height(&mut self, _: u16) -> u16 {
        1
    }
    fn render(&mut self, area: Rect, cx: &mut PaintCx<'_, '_>) {
        cx.write_line(
            bmux_tui::paint::LocalRect::new(0, 0, area.width, area.height),
            &bmux_tui::text::Line::raw(self.text.clone()),
        );
        cx.set_focus_scope(Some(bmux_tui::hit::HitId::new("modal")));
        cx.push_hit(bmux_tui::hit::HitRegion::new("edit", Rect::new(0, 0, 2, 1)).focusable(true));
        cx.push_selection_scope(SelectionScope::new("source", area));
        cx.push_selection_fragment(SelectionFragment::new(
            "source",
            "source",
            Rect::new(0, 0, 2, 1),
            0,
            0..self.text.len(),
        ));
        cx.push_image(ImageContribution::Present(ImagePlacement {
            key: ImageKey::new("image"),
            payload: ImagePayload::Pixels {
                bytes: vec![255, 0, 0, 255].into(),
                width: 1,
                height: 1,
                format: ImagePixelFormat::Rgba8,
            },
            destination: Rect::new(2, 0, 1, 1),
            clip: Rect::new(2, 0, 1, 1),
            allocation: None,
            fit: ImageFit::Contain,
            lifecycle: ImageLifecycle::Persistent,
            z: 0,
        }));
    }
    fn retained_selection_text(&self) -> BTreeMap<SelectionContentId, String> {
        BTreeMap::from([("source".into(), self.text.clone())])
    }
    fn retained_modal_scopes(&self) -> Vec<bmux_tui::focus::FocusScopeId> {
        vec!["modal".into()]
    }
    fn attach_updates(&mut self, updates: PluginTuiSurfaceUpdateReceiver) {
        self.updates = Some(updates);
    }
    fn poll(&mut self, _: &dyn PluginTuiHost) -> PluginTuiAction {
        if let Some(PluginTuiSurfaceUpdate::WorkflowCatalogError { message }) = self
            .updates
            .as_mut()
            .and_then(|updates| updates.try_recv().ok())
        {
            self.text = message;
            return PluginTuiAction::Redraw;
        }
        PluginTuiAction::None
    }
    fn handle_event(&mut self, event: &Event, _: &dyn PluginTuiHost) -> PluginTuiAction {
        match event {
            Event::Paste(text) => self.text = text.clone(),
            Event::User(text) => {
                self.effect = PluginTuiAction::RunCommand {
                    command: text.clone(),
                }
            }
            _ => {}
        }
        PluginTuiAction::Redraw
    }
    fn drain_effects<'a>(
        &'a mut self,
        _: &'a dyn PluginTuiHost,
    ) -> Pin<Box<dyn Future<Output = PluginTuiAction> + Send + 'a>> {
        let action = std::mem::replace(&mut self.effect, PluginTuiAction::None);
        Box::pin(async move { action })
    }
}
fn registry() -> PluginTuiRegistry {
    let mut registry = PluginTuiRegistry::default();
    registry.register_factory(Box::new(Factory));
    registry
}
fn call() -> PluginCallContext {
    PluginCallContext {
        invocation_id: bmux_plugin_sdk::PluginInvocationId::new(),
        cancellation: bmux_plugin_sdk::ServiceCancellation::new(),
        host_services: None,
        callback_execution: None,
    }
}
#[tokio::test]
async fn deferred_edit_status_delivers_one_terminal_result_without_replaying_mutation() {
    struct Deferred {
        polls: usize,
    }
    impl PluginTuiSurface for Deferred {
        fn id(&self) -> &'static str {
            "deferred"
        }
        fn title(&self) -> &'static str {
            "Deferred"
        }
        fn render(&mut self, _: Rect, _: &mut PaintCx<'_, '_>) {}
        fn handle_event(&mut self, _: &Event, _: &dyn PluginTuiHost) -> PluginTuiAction {
            PluginTuiAction::None
        }
        fn take_focused_text_field_edit_result(
            &mut self,
        ) -> Option<PluginTuiFocusedTextFieldEditResult> {
            self.polls += 1;
            (self.polls == 2).then(|| PluginTuiFocusedTextFieldEditResult {
                event: PluginTuiEventResult::consumed(),
                edit_result: FocusedTextFieldEditResult::Applied,
            })
        }
    }
    let resource = uuid::Uuid::new_v4();
    let (redraw_sender, redraw) = mpsc::channel(1);
    let (updates, _) = mpsc::channel(1);
    let mut state = State {
        registry: None,
        surfaces: BTreeMap::from([(
            resource,
            Surface {
                implementation: Box::new(Deferred { polls: 0 }),
                host: TokioPluginTuiHost::current(redraw_sender),
                redraw,
                updates,
                scene: PresentationState::new(Rect::new(0, 0, 1, 1)),
                revision: 1,
                dirty: false,
                viewport: None,
                bindings: None,
                edit_pending: true,
            },
        )]),
    };
    assert!(matches!(
        AuthorFrontend::execute_surface(&mut state, Request::EditStatus { resource })
            .await
            .unwrap(),
        Response::EditStatus(None)
    ));
    assert!(state.surfaces[&resource].edit_pending);
    assert!(matches!(
        AuthorFrontend::execute_surface(&mut state, Request::EditStatus { resource })
            .await
            .unwrap(),
        Response::EditStatus(Some(PluginTuiFocusedTextFieldEditResult {
            edit_result: FocusedTextFieldEditResult::Applied,
            ..
        }))
    ));
    assert!(!state.surfaces[&resource].edit_pending);
    assert!(
        AuthorFrontend::execute_surface(&mut state, Request::EditStatus { resource })
            .await
            .is_err()
    );
}

/// Records each delivered event; a field edit reaching it would be a fence violation.
struct Counting(Arc<std::sync::atomic::AtomicUsize>);
impl PluginTuiSurface for Counting {
    fn id(&self) -> &'static str {
        "counting"
    }
    fn title(&self) -> &'static str {
        "Counting"
    }
    fn render(&mut self, _: Rect, _: &mut PaintCx<'_, '_>) {}
    fn handle_event(&mut self, _: &Event, _: &dyn PluginTuiHost) -> PluginTuiAction {
        self.0.fetch_add(1, Ordering::AcqRel);
        PluginTuiAction::Redraw
    }
    fn apply_focused_text_field_edit_result(
        &mut self,
        _: &FocusedTextFieldEdit,
        _: &dyn PluginTuiHost,
    ) -> PluginTuiFocusedTextFieldEditResult {
        panic!("a stale field edit must never reach the surface");
    }
}

/// One counting surface at `revision`, executed directly through the author codec.
fn counting_state(
    applied: Arc<std::sync::atomic::AtomicUsize>,
    revision: u64,
) -> (State, uuid::Uuid) {
    let resource = uuid::Uuid::new_v4();
    let (redraw_sender, redraw) = mpsc::channel(1);
    let (updates, _) = mpsc::channel(1);
    let state = State {
        registry: None,
        surfaces: BTreeMap::from([(
            resource,
            Surface {
                implementation: Box::new(Counting(applied)),
                host: TokioPluginTuiHost::current(redraw_sender),
                redraw,
                updates,
                scene: PresentationState::new(Rect::new(0, 0, 1, 1)),
                revision,
                dirty: false,
                viewport: None,
                bindings: None,
                edit_pending: false,
            },
        )]),
    };
    (state, resource)
}

/// A focused field edit fenced to a snapshot taken at a superseded revision.
fn superseded_field_edit() -> FocusedTextFieldEdit {
    let snapshot = FocusedTextFieldSnapshot {
        identity: FocusedTextFieldIdentity {
            owner_id: "counting".to_owned(),
            field_id: "counting.field".to_owned(),
            lifetime: 1,
        },
        generation: 1,
        document: bcode_plugin_models::composer::ComposerDocument {
            document_id: "document".to_owned(),
            revision: 1,
            text: String::new(),
            cursor: 0,
            selection: None,
        },
        eligible: true,
        content_rect: Rect::new(0, 0, 1, 1),
        anchor: Rect::new(0, 0, 1, 1),
    };
    FocusedTextFieldEdit {
        edit: bcode_plugin_models::composer::ComposerEdit {
            document_id: "document".to_owned(),
            revision: 1,
            range: bcode_plugin_models::composer::ComposerTextRange { start: 0, end: 0 },
            expected_text: String::new(),
            replacement: "x".to_owned(),
            cursor: 1,
        },
        expected: snapshot,
    }
}

#[tokio::test]
async fn superseded_input_reports_stale_while_fenced_edits_still_reject() {
    let applied = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let (mut state, resource) = counting_state(Arc::clone(&applied), 5);
    let input = |expected_revision| Request::Input {
        resource,
        expected_revision,
        event: Event::Tick,
        bindings: Bindings::default(),
        focused: false,
    };
    assert!(matches!(
        AuthorFrontend::execute_surface(&mut state, input(4))
            .await
            .unwrap(),
        Response::Stale { revision: 5 }
    ));
    assert_eq!(applied.load(Ordering::Acquire), 0);
    assert!(matches!(
        AuthorFrontend::execute_surface(&mut state, input(5)).await.unwrap(),
        Response::Event(result) if result.action == PluginTuiAction::Redraw
    ));
    assert_eq!(applied.load(Ordering::Acquire), 1);
    // A revision the surface never prepared is a protocol violation, not staleness.
    assert!(
        AuthorFrontend::execute_surface(&mut state, input(6))
            .await
            .is_err()
    );
    assert_eq!(applied.load(Ordering::Acquire), 1);
    let edit = superseded_field_edit();
    let rejected = AuthorFrontend::execute_surface(
        &mut state,
        Request::Edit {
            resource,
            expected_revision: 4,
            edit: Box::new(edit),
        },
    )
    .await
    .unwrap();
    assert!(matches!(
        rejected,
        Response::Edit(result) if result.edit_result == FocusedTextFieldEditResult::Rejected(
            bcode_plugin_models::composer::ComposerValidationError::Stale
        )
    ));
}

async fn invoke(adapter: &Arc<dyn ProductComponentExtension>, request: Request) -> Response {
    let payload = serde_json::to_vec(&request).unwrap();
    serde_json::from_slice(&adapter.invoke(call(), payload).await.unwrap()).unwrap()
}
#[tokio::test]
async fn lazy_descendant_must_be_joined_before_response_publication_seals() {
    let context = settlement::ResponseContext::new(settlement::ResponseId {
        session_id: uuid::Uuid::new_v4(),
        sequence: 1,
    });
    let tasks = Arc::new(AuthorTasks {
        context: Some(Arc::clone(&context)),
        ..AuthorTasks::new()
    });
    let (entered, mut observed) = tokio::sync::mpsc::channel(1);
    let (release, wait) = tokio::sync::oneshot::channel();
    let nested = Arc::clone(&tasks);
    let runtime = tokio::runtime::Handle::current();
    let child_runtime = runtime.clone();
    tasks.spawn(
        &runtime,
        Box::pin(async move {
            let _ = wait.await;
            nested.spawn(
                &child_runtime,
                Box::pin(async move {
                    entered.send(()).await.unwrap();
                }),
            );
        }),
    );
    assert!(!tasks.seal_joined_publication().unwrap());
    assert!(!context.publication_finished());
    release.send(()).unwrap();
    observed.recv().await.unwrap();
    for _ in 0..32 {
        if tasks.seal_joined_publication().unwrap() {
            break;
        }
        tokio::task::yield_now().await;
    }
    assert!(tasks.seal_joined_publication().unwrap());
    assert!(context.publication_finished());
    tasks.settle().unwrap();
}

#[tokio::test]
async fn dropped_author_task_is_not_joined_settlement() {
    let context = settlement::ResponseContext::new(settlement::ResponseId {
        session_id: uuid::Uuid::new_v4(),
        sequence: 1,
    });
    let tasks = AuthorTasks {
        context: Some(Arc::clone(&context)),
        ..AuthorTasks::new()
    };
    tasks.spawn(
        &tokio::runtime::Handle::current(),
        Box::pin(std::future::pending()),
    );
    tasks.handles.lock().unwrap()[0].abort();
    tokio::task::yield_now().await;
    assert!(tasks.seal_joined_publication().is_err());
    assert!(!context.publication_finished());
    assert!(tasks.settle().is_err());
}

#[tokio::test]
async fn admitted_state_has_no_mutex_guard_and_nested_calls_fail_closed() {
    let author = AuthorFrontend {
        context: PluginCreateContext {
            runtime_scope: RuntimeScopeId::from_uuid(uuid::Uuid::new_v4()),
            instance: LoadedInstanceId::from_uuid(uuid::Uuid::new_v4()),
            role: crate::definition::ComponentRole::Frontend,
        },
        factory: registry,
        state: std::sync::Mutex::new(Some(State {
            registry: None,
            surfaces: BTreeMap::new(),
        })),
        closed: AtomicBool::new(false),
        executor: std::sync::OnceLock::new(),
        tasks: Arc::new(AuthorTasks::new()),
        retained_session: std::sync::Mutex::new(None),
        callbacks: std::sync::Mutex::new(None),
        recovery_session: std::sync::Mutex::new(None),
        settlement: std::sync::Mutex::new(None),
        settlement_busy: AtomicBool::new(false),
    };
    let lease = author.checkout().unwrap();
    assert!(author.execute(Request::Describe).await.is_err());
    drop(lease);
    assert!(matches!(
        author.execute(Request::Describe).await.unwrap(),
        Response::Descriptor(_)
    ));
    let lease = author.checkout().unwrap();
    assert!(author.close().is_err(), "active work retains cleanup state");
    drop(lease);
    author.close().unwrap();
    assert!(author.execute(Request::Describe).await.is_err());
}

#[tokio::test]
async fn author_updates_use_attached_bounded_stream_without_replaying_overflow() {
    let adapter = extension(
        registry,
        PluginCreateContext {
            runtime_scope: RuntimeScopeId::from_uuid(uuid::Uuid::new_v4()),
            instance: LoadedInstanceId::from_uuid(uuid::Uuid::new_v4()),
            role: crate::definition::ComponentRole::Frontend,
        },
    );
    let Response::Opened { resource, .. } = invoke(
        &adapter,
        Request::Open {
            kind: "probe".to_owned(),
            request: PluginTuiSurfaceOpenRequest {
                instance_id: "bounded-updates".to_owned(),
                repo_path: None,
                session_id: None,
                target: None,
                options: serde_json::Value::Null,
            },
        },
    )
    .await
    else {
        panic!("open")
    };
    for index in 0..32 {
        assert!(matches!(
            invoke(
                &adapter,
                Request::Update {
                    resource,
                    update: PluginTuiSurfaceUpdate::WorkflowCatalogError {
                        message: index.to_string()
                    },
                }
            )
            .await,
            Response::Applied
        ));
    }
    assert!(
        adapter
            .invoke(
                call(),
                serde_json::to_vec(&Request::Update {
                    resource,
                    update: PluginTuiSurfaceUpdate::WorkflowCatalogError {
                        message: "overflow-must-not-replace-first".to_owned()
                    },
                })
                .unwrap()
            )
            .await
            .is_err()
    );
    let Response::Projection(projection) = invoke(
        &adapter,
        Request::Prepare {
            resource,
            viewport: Box::new(Viewport {
                width: 4,
                height: 1,
                logical_row_offset: 0,
                theme: None,
            }),
            bindings: Bindings::default(),
        },
    )
    .await
    else {
        panic!("updated projection")
    };
    assert_eq!(
        projection.scene.text.values().next().map(String::as_str),
        Some("0")
    );
    adapter.close().unwrap();
}

async fn open_rich_scene_probe() -> (Arc<dyn ProductComponentExtension>, uuid::Uuid) {
    let context = PluginCreateContext {
        runtime_scope: RuntimeScopeId::from_uuid(uuid::Uuid::new_v4()),
        instance: LoadedInstanceId::from_uuid(uuid::Uuid::new_v4()),
        role: crate::definition::ComponentRole::Frontend,
    };
    let adapter = extension(registry, context);
    let Response::Opened { resource, .. } = invoke(
        &adapter,
        Request::Open {
            kind: "probe".to_owned(),
            request: PluginTuiSurfaceOpenRequest {
                instance_id: "instance".to_owned(),
                repo_path: None,
                session_id: None,
                target: None,
                options: serde_json::Value::Null,
            },
        },
    )
    .await
    else {
        panic!("open")
    };
    (adapter, resource)
}

async fn verify_rich_scene_and_input_revision_fences(
    adapter: &Arc<dyn ProductComponentExtension>,
    resource: uuid::Uuid,
) {
    let viewport = Viewport {
        width: 4,
        height: 1,
        logical_row_offset: 0,
        theme: None,
    };
    let Response::Projection(first) = invoke(
        adapter,
        Request::Prepare {
            resource,
            viewport: Box::new(viewport.clone()),
            bindings: Bindings::default(),
        },
    )
    .await
    else {
        panic!("projection")
    };
    assert_eq!(first.scene.version, 2);
    assert_eq!(first.scene.images.len(), 1);
    assert_eq!(first.scene.modal_scopes.len(), 1);
    assert_eq!(
        first.scene.text.values().next().map(String::as_str),
        Some("界")
    );
    // Superseded input is reported as not applied, never silently consumed.
    assert!(matches!(invoke(adapter, Request::Input {
        resource,
        expected_revision: 0,
        event: Event::Paste("e\u{301}".to_owned()),
        bindings: Bindings::default(),
        focused: false,
    }).await, Response::Stale { revision } if revision == first.revision));
    invoke(
        adapter,
        Request::Input {
            resource,
            expected_revision: first.revision,
            event: Event::Paste("ab".to_owned()),
            bindings: Bindings::default(),
            focused: false,
        },
    )
    .await;
    let Response::Projection(second) = invoke(
        adapter,
        Request::Prepare {
            resource,
            viewport: Box::new(viewport),
            bindings: Bindings::default(),
        },
    )
    .await
    else {
        panic!("projection")
    };
    assert!(second.revision > first.revision);
    assert_eq!(
        second.scene.text.values().next().map(String::as_str),
        Some("ab")
    );
}

#[tokio::test]
async fn author_wire_factory_preserves_rich_scene_and_revision_fences() {
    let (adapter, resource) = open_rich_scene_probe().await;
    verify_rich_scene_and_input_revision_fences(&adapter, resource).await;
    invoke(
        &adapter,
        Request::Update {
            resource,
            update: PluginTuiSurfaceUpdate::WorkflowCatalogError {
                message: "更新".to_owned(),
            },
        },
    )
    .await;
    let Response::Projection(third) = invoke(
        &adapter,
        Request::Prepare {
            resource,
            viewport: Box::new(Viewport {
                width: 4,
                height: 1,
                logical_row_offset: 0,
                theme: None,
            }),
            bindings: Bindings::default(),
        },
    )
    .await
    else {
        panic!("updated projection")
    };
    assert_eq!(
        third.scene.text.values().next().map(String::as_str),
        Some("更新")
    );
    let command = "plugin-owned-command".to_owned();
    invoke(
        &adapter,
        Request::Input {
            resource,
            expected_revision: third.revision,
            event: Event::User(command.clone()),
            bindings: Bindings::default(),
            focused: false,
        },
    )
    .await;
    let Response::Action(effect) = invoke(&adapter, Request::Effects { resource }).await else {
        panic!("effect")
    };
    assert_eq!(effect, PluginTuiAction::RunCommand { command });
    assert!(matches!(
        invoke(&adapter, Request::Effects { resource }).await,
        Response::Action(PluginTuiAction::None)
    ));
    let other = extension(
        registry,
        PluginCreateContext {
            runtime_scope: RuntimeScopeId::from_uuid(uuid::Uuid::new_v4()),
            instance: LoadedInstanceId::from_uuid(uuid::Uuid::new_v4()),
            role: crate::definition::ComponentRole::Frontend,
        },
    );
    assert!(
        other
            .invoke(
                call(),
                serde_json::to_vec(&Request::Close { resource }).unwrap()
            )
            .await
            .is_err()
    );
    adapter.close().unwrap();
    assert!(
        adapter
            .invoke(call(), serde_json::to_vec(&Request::Describe).unwrap())
            .await
            .is_err()
    );
}
