//! Invocation-local leaf cursor; pending lifecycle work does not reopen an exhausted scan.
#[derive(Default)]
pub(super) struct Scan {
    pub(super) after: Option<String>,
    pub(super) exhausted: bool,
    progressed: bool,
}

impl Scan {
    pub(super) const fn observe_progress(&mut self, progressed: bool) {
        self.progressed |= progressed;
    }

    pub(super) fn finish_page(&mut self, next: Option<String>, progressed: bool) {
        self.observe_progress(progressed);
        self.exhausted = next.is_none();
        self.after = next;
    }

    pub(super) fn restart_if_progressed(&mut self) {
        if self.exhausted && self.progressed {
            self.after = None;
            self.exhausted = false;
            self.progressed = false;
        }
    }
}

#[derive(Default)]
pub(super) struct FanOutScan {
    after: Option<String>,
    version: Option<super::super::representation::graph::parallel_progress::ScanVersion>,
    reduction_pending: bool,
    finished: bool,
}

impl FanOutScan {
    pub(super) async fn advance(
        &mut self,
        path: &std::path::Path,
        db: &dyn switchy_database::Database,
        authority: &bcode_workflow_store::WorkflowExecutionAuthority,
        run_id: &str,
    ) -> Result<(bool, bool), super::super::ServerError> {
        // Keep the page-local graph/reduction state out of every driver and test
        // caller's future while preserving the same borrow and cancellation scope.
        Box::pin(self.advance_inner(path, db, authority, run_id)).await
    }

    async fn advance_inner(
        &mut self,
        path: &std::path::Path,
        db: &dyn switchy_database::Database,
        authority: &bcode_workflow_store::WorkflowExecutionAuthority,
        run_id: &str,
    ) -> Result<(bool, bool), super::super::ServerError> {
        use super::super::{backpressure, representation};
        let version = backpressure::retry(|| {
            representation::graph::parallel_progress::scan_version(db, run_id)
        })
        .await
        .map_err(backpressure::normalize)?;
        if self.version != Some(version) {
            self.after = None;
            self.version = Some(version);
            self.reduction_pending = false;
            self.finished = false;
        }
        if self.finished {
            return Ok((false, false));
        }
        let nodes = backpressure::retry(|| {
            representation::graph_pages::nodes(
                db,
                run_id,
                version.revision,
                self.after.as_deref(),
                32,
            )
        })
        .await
        .map_err(backpressure::normalize)?;
        let next = nodes.last().map(|node| node.id.clone());
        let mut progressed = false;
        for node in nodes {
            if node.kind != bcode_workflow::NodeKind::FanOut {
                continue;
            }
            let activation =
                backpressure::retry(|| representation::graph::activation_id(db, run_id, &node.id))
                    .await
                    .map_err(backpressure::normalize)?;
            let Some(activation) = activation else {
                continue;
            };
            if !backpressure::retry(|| {
                representation::graph::is_ready_activation(db, run_id, &node.id, &activation)
            })
            .await
            .map_err(backpressure::normalize)?
            {
                continue;
            }
            let context = crate::xray_workflow::activation_context(
                bcode_xray::CaptureContext::current(),
                run_id,
                &activation,
                None,
            );
            let boundary = crate::xray_workflow::Boundary::new(context.clone(), "fanout.advance");
            let result = crate::xray_workflow::scope(
                context,
                super::super::fanout::advance(
                    path,
                    db,
                    authority,
                    run_id,
                    version.revision,
                    &node.id,
                    &activation,
                ),
            )
            .await;
            boundary.finish_result(&result);
            let (changed, pending) = result?;
            progressed |= changed;
            self.reduction_pending |= pending;
        }
        self.after = next;
        let pending = self.after.is_some() || self.reduction_pending;
        if self.after.is_none() {
            self.finished = !self.reduction_pending;
            self.reduction_pending = false;
        }
        Ok((progressed, pending))
    }
}

#[cfg(test)]
mod tests {
    use super::Scan;

    #[tokio::test]
    async fn fanout_reduction_pending_is_not_leaf_progress_or_external_wait() {
        use crate::workflow_representation::{owner, representation, tests};
        let (_root, _sessions, state, definition) = Box::pin(tests::typed_provider_fixture()).await;
        let db = crate::workflow_representation::open(&crate::workflow_store_path(&state), true)
            .expect("typed database");
        let authority = owner::local_authority(&state, "new")
            .await
            .unwrap()
            .unwrap();
        let mut run = representation::hierarchy::parent_context(&db, &authority.authority, "new")
            .await
            .unwrap()
            .admission;
        let definition = reduction_definition(definition.nodes.values().next().unwrap().clone());
        let identity =
            bcode_workflow::WorkflowDefinitionIdentity::for_definition("reduction", &definition)
                .unwrap();
        run.run_id = "reduction".into();
        run.definition_id = identity.definition_id;
        run.definition_version = identity.definition_version;
        run.input = Some(serde_json::json!(vec![1_u32; 65]));
        representation::admit(&db, &run, &definition).await.unwrap();
        let activation = representation::graph::activation_id(&db, "reduction", "fanout")
            .await
            .unwrap()
            .unwrap();
        for _ in 0..3 {
            let (progressed, _) = crate::workflow_representation::fanout::advance(
                &crate::workflow_store_path(&state),
                &db,
                &authority.authority,
                "reduction",
                1,
                "fanout",
                &activation,
            )
            .await
            .unwrap();
            assert!(progressed);
        }
        let mut leaves = Scan::default();
        leaves.finish_page(None, false);
        let mut fanouts = super::FanOutScan::default();
        let (progressed, pending) = fanouts
            .advance(
                &crate::workflow_store_path(&state),
                &db,
                &authority.authority,
                "reduction",
            )
            .await
            .unwrap();
        assert!(!progressed);
        assert!(pending);
        leaves.observe_progress(progressed);
        leaves.restart_if_progressed();
        assert!(leaves.exhausted);
        // The final reduction page records unresolved members, then only external waiting remains.
        for _ in 0..3 {
            let (progressed, pending) = fanouts
                .advance(
                    &crate::workflow_store_path(&state),
                    &db,
                    &authority.authority,
                    "reduction",
                )
                .await
                .unwrap();
            assert!(!progressed);
            if !pending {
                return;
            }
        }
        panic!("reduction must stop at external member waiting");
    }

    fn reduction_definition(
        leaf: bcode_workflow::NodeDefinition,
    ) -> bcode_workflow::WorkflowDefinition {
        let mut node = leaf.clone();
        node.id = "fanout".into();
        node.kind = bcode_workflow::NodeKind::FanOut;
        node.input = bcode_workflow::ValueSchema {
            type_name: "fanout.input".into(),
            schema: serde_json::json!({"type":"array","items":leaf.input.schema}),
        };
        node.output = bcode_workflow::ValueSchema {
            type_name: format!("workflow.fan-out-result/v1<{}>", leaf.output.type_name),
            schema: serde_json::json!({"type":"object","additionalProperties":false,"required":["version","members"],"properties":{"version":{"type":"integer","const":1},"members":{"type":"array","items":{"type":"object","additionalProperties":false,"required":["index","value"],"properties":{"index":{"type":"integer","minimum":0},"value":leaf.output.schema}}}}}),
        };
        node.configuration = serde_json::to_value(bcode_workflow::WorkflowFanOutConfiguration {
            version: bcode_workflow::WORKFLOW_FAN_OUT_CONFIGURATION_VERSION,
            member_node: Box::new(leaf),
            max_members: 65,
            max_concurrency: 65,
            failure_policy: bcode_workflow::ParallelFailurePolicy::WaitAll,
        })
        .unwrap();
        bcode_workflow::WorkflowDefinition {
            schema_version: bcode_workflow::WORKFLOW_DEFINITION_SCHEMA_VERSION,
            name: "reduction".into(),
            input: node.input.clone(),
            output: node.output.clone(),
            nodes: std::collections::BTreeMap::from([("fanout".into(), node)]),
            entries: vec!["fanout".into()],
            exits: vec!["fanout".into()],
            edges: vec![],
        }
    }

    #[test]
    fn pending_lifecycle_drain_does_not_reopen_exhausted_leaf_scan() {
        let mut scan = Scan::default();
        scan.finish_page(Some("leaf".into()), false);
        scan.finish_page(None, false);
        for _ in 0..100 {
            scan.observe_progress(false);
            scan.restart_if_progressed();
            assert!(scan.exhausted);
        }
    }

    #[test]
    fn actual_progress_reopens_scan_after_last_page() {
        let mut scan = Scan::default();
        scan.finish_page(Some("leaf".into()), true);
        scan.restart_if_progressed();
        assert_eq!(scan.after.as_deref(), Some("leaf"));
        scan.finish_page(None, false);
        scan.restart_if_progressed();
        assert!(!scan.exhausted);
        assert!(scan.after.is_none());
        scan.finish_page(None, false);
        scan.restart_if_progressed();
        assert!(scan.exhausted);
        scan.observe_progress(true);
        scan.restart_if_progressed();
        assert!(!scan.exhausted);
        assert!(scan.after.is_none());
    }
}
