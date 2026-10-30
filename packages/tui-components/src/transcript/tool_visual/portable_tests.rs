use super::*;
use bmux_tui::prelude::Line;

struct Adapter(&'static str, bool);
impl PortableVisualAdapter for Adapter {
    fn render(
        &self,
        _: &CanonicalPluginVisual,
        _: &PortableVisualContext<'_>,
    ) -> Option<PortableVisualOutput> {
        Some(PortableVisualOutput {
            rows: if self.1 {
                TranscriptLayoutRows::BlankSpan(8193)
            } else {
                vec![Line::from(self.0)].into()
            },
            ..PortableVisualOutput::default()
        })
    }
}

fn registration(
    plugin: &str,
    priority: i32,
    default: bool,
    oversized: bool,
) -> PortableVisualRegistration {
    PortableVisualRegistration {
        descriptor: PortableVisualDescriptor {
            plugin_id: plugin.into(),
            adapter_id: "card".into(),
            schemas: BTreeSet::from([("example.content".into(), 1)]),
            priority,
            producer_default: default,
        },
        adapter: Box::new(Adapter("producer rows", oversized)),
    }
}
fn policy() -> PortableVisualPolicy {
    PortableVisualPolicy {
        enabled_plugins: BTreeSet::from(["a".into(), "b".into()]),
        ..PortableVisualPolicy::default()
    }
}
fn visual() -> CanonicalPluginVisual {
    CanonicalPluginVisual {
        invocation_id: Some("call".into()),
        revision: 7,
        producer_plugin_id: Some("b".into()),
        schema: "example.content".into(),
        schema_version: 1,
        title: None,
        subtitle: None,
        payload: serde_json::json!({"text":"界🙂e\u{301}"}),
        streaming: false,
    }
}
fn context() -> PortableVisualContext<'static> {
    PortableVisualContext {
        width: 20,
        theme: None,
    }
}

#[test]
fn native_order_priority_then_producer_then_identity_with_explicit_preference() {
    let registry = PortableVisualRegistry::new(
        1,
        policy(),
        vec![
            registration("a", 10, true, false),
            registration("b", 10, true, false),
        ],
    )
    .unwrap();
    assert_eq!(
        registry
            .prepare(&visual(), &context())
            .unwrap()
            .adapter_reference(),
        "b/card"
    );
    let mut preferred = policy();
    preferred.preferred.push("a/card".into());
    let registry = PortableVisualRegistry::new(
        2,
        preferred,
        vec![
            registration("a", 1, false, false),
            registration("b", 100, true, false),
        ],
    )
    .unwrap();
    assert_eq!(
        registry
            .prepare(&visual(), &context())
            .unwrap()
            .adapter_reference(),
        "a/card"
    );
}

#[test]
fn disabled_unenabled_and_wrong_versions_never_execute() {
    let mut selected = policy();
    selected.disabled.insert("b/card".into());
    selected.enabled_plugins.remove("a");
    let registry = PortableVisualRegistry::new(
        1,
        selected,
        vec![
            registration("a", 0, false, false),
            registration("b", 0, true, false),
        ],
    )
    .unwrap();
    assert!(registry.prepare(&visual(), &context()).is_none());
    let registry =
        PortableVisualRegistry::new(1, policy(), vec![registration("b", 0, true, false)]).unwrap();
    let mut input = visual();
    input.schema_version = 2;
    assert!(registry.prepare(&input, &context()).is_none());
}

#[test]
fn malformed_output_advances_and_duplicate_registration_rejects() {
    let registry = PortableVisualRegistry::new(
        1,
        policy(),
        vec![
            registration("b", 100, true, true),
            registration("a", 0, false, false),
        ],
    )
    .unwrap();
    assert_eq!(
        registry
            .prepare(&visual(), &context())
            .unwrap()
            .adapter_reference(),
        "a/card"
    );
    assert!(
        PortableVisualRegistry::new(
            1,
            policy(),
            vec![
                registration("b", 1, true, false),
                registration("b", 2, true, false)
            ]
        )
        .is_err()
    );
}

#[test]
fn every_preparation_identity_dimension_is_checked() {
    let registry =
        PortableVisualRegistry::new(1, policy(), vec![registration("b", 0, true, false)]).unwrap();
    for changed in 0..5 {
        let prepared = registry.prepare(&visual(), &context()).unwrap();
        let mut current = visual();
        let mut width = 20;
        match changed {
            0 => current.revision += 1,
            1 => current.invocation_id = Some("other".into()),
            2 => current.streaming = true,
            3 => current.payload = serde_json::json!({"text":"changed"}),
            _ => width = 19,
        }
        assert!(registry.resolve(prepared, &current, width).is_none());
    }
    let prepared = registry.prepare(&visual(), &context()).unwrap();
    let replacement =
        PortableVisualRegistry::new(2, policy(), vec![registration("b", 0, true, false)]).unwrap();
    assert!(replacement.resolve(prepared, &visual(), 20).is_none());
    let prepared = registry.prepare(&visual(), &context()).unwrap();
    assert_eq!(
        registry
            .resolve(prepared, &visual(), 20)
            .unwrap()
            .rows
            .len(),
        1
    );
}

#[test]
fn artifact_delivery_is_selected_bounded_and_invalidates_prepared_output() {
    struct ArtifactAdapter;
    impl PortableVisualAdapter for ArtifactAdapter {
        fn artifact_chunk(&self, chunk: &PortableArtifactChunk<'_>) -> bool {
            chunk.finalized
        }
        fn render(
            &self,
            _: &CanonicalPluginVisual,
            _: &PortableVisualContext<'_>,
        ) -> Option<PortableVisualOutput> {
            Some(PortableVisualOutput {
                rows: vec![Line::from("artifact")].into(),
                ..PortableVisualOutput::default()
            })
        }
    }
    let mut registration = registration("b", 0, true, false);
    registration.adapter = Box::new(ArtifactAdapter);
    let registry = PortableVisualRegistry::new(1, policy(), vec![registration]).unwrap();
    let prepared = registry.prepare(&visual(), &context()).unwrap();
    let mut chunk = PortableArtifactChunk {
        invocation_id: "call",
        artifact_id: "artifact",
        reference_key: "source",
        producer_plugin_id: "b",
        schema: "example.content",
        schema_version: 1,
        content_type: Some("text/plain"),
        offset: 0,
        total_bytes: 1,
        revision: 1,
        finalized: true,
        bytes: b"x",
    };
    assert!(!registry.deliver_artifact("a/card", &chunk));
    assert_eq!(registry.artifact_revision(), 0);
    chunk.schema_version = 2;
    assert!(!registry.deliver_artifact("b/card", &chunk));
    chunk.schema_version = 1;
    assert!(registry.deliver_artifact("b/card", &chunk));
    assert_eq!(registry.artifact_revision(), 1);
    assert!(registry.resolve(prepared, &visual(), 20).is_none());
    chunk.offset = 1;
    assert!(!registry.deliver_artifact("b/card", &chunk));
}

#[test]
fn zero_width_and_oversized_semantics_do_not_render() {
    let registry =
        PortableVisualRegistry::new(1, policy(), vec![registration("b", 0, true, false)]).unwrap();
    assert!(
        registry
            .prepare(
                &visual(),
                &PortableVisualContext {
                    width: 0,
                    theme: None
                }
            )
            .is_none()
    );
    let mut input = visual();
    input.payload = serde_json::json!({"text":"x".repeat(256 * 1024 + 1)});
    assert!(registry.prepare(&input, &context()).is_none());
}
