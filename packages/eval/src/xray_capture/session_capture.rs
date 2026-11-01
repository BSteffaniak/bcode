//! Executor-level evidence grouping. Each actual segment is consumed by `Capture`,
//! whose ordinary application path delegates observation lifecycle to the client.
use super::{Capture, EvalXrayEvidence, Path, PathBuf};

/// Groups one executor's actual segment receipts without conflating restart gaps
/// with task outcomes. This owns no reattach policy; the executor supplies hooks.
pub struct SessionCapture {
    active: Option<Capture>,
    directory: PathBuf,
    session: String,
    segments: Vec<EvalXrayEvidence>,
    restart_gap: bool,
}

impl SessionCapture {
    pub(crate) fn start(
        session: &str,
        directory: &Path,
        client: &bcode_client::BcodeClient,
    ) -> Option<Self> {
        Some(Self {
            active: Capture::start(session, directory, client),
            directory: directory.into(),
            session: session.into(),
            segments: Vec::new(),
            restart_gap: false,
        })
        .filter(|capture| capture.active.is_some())
    }

    pub(crate) fn start_workflow(
        session: &str,
        directory: &Path,
        binary: &Path,
        environment: impl Iterator<Item = (String, String)>,
    ) -> Option<Self> {
        Some(Self {
            active: Capture::start_workflow(session, directory, binary, environment),
            directory: directory.into(),
            session: session.into(),
            segments: Vec::new(),
            restart_gap: false,
        })
        .filter(|capture| capture.active.is_some())
    }

    /// Close and drain observer before the executor stops its isolated daemon.
    pub(crate) fn before_restart(&mut self) {
        self.restart_gap = true;
        if let Some(capture) = self.active.take() {
            self.segments.push(
                capture
                    .finish_with_restart_gap()
                    .unwrap_or_else(|| EvalXrayEvidence::new(self.session.clone())),
            );
        }
    }

    /// Called after matching-daemon availability, before follow-up submission.
    /// Capture's client consumer uses E's authorized controller for this fresh segment.
    pub(crate) fn after_restart(&mut self, client: &bcode_client::BcodeClient) {
        self.restart_gap = true;
        let directory = self
            .directory
            .join(format!("xray-segment-{:08}", self.segments.len()));
        self.active = std::fs::create_dir(&directory)
            .ok()
            .and_then(|()| Capture::start(&self.session, &directory, client));
        if self.active.is_none() {
            let mut failure = EvalXrayEvidence::new(self.session.clone());
            failure.status = "restart_segment_unavailable";
            self.segments.push(failure);
        }
    }

    pub(crate) fn finish(mut self) -> Option<EvalXrayEvidence> {
        if let Some(capture) = self.active.take() {
            self.segments.push(
                capture
                    .finish()
                    .unwrap_or_else(|| EvalXrayEvidence::new(self.session.clone())),
            );
        }
        self.publish()
    }

    fn publish(&self) -> Option<EvalXrayEvidence> {
        if self.segments.is_empty() {
            return None;
        }
        let mut summary = EvalXrayEvidence::new(self.session.clone());
        summary.ready_before_submission = self
            .segments
            .iter()
            .all(|segment| segment.ready_before_submission);
        summary.delivery_complete = self
            .segments
            .iter()
            .all(|segment| segment.delivery_complete);
        summary.payloads_complete = self
            .segments
            .iter()
            .all(|segment| segment.payloads_complete);
        summary.observation_released = self
            .segments
            .iter()
            .all(|segment| segment.observation_released);
        summary.continuity_complete = !self.restart_gap
            && self
                .segments
                .iter()
                .all(|segment| segment.continuity_complete);
        summary.envelopes = self.segments.iter().map(|segment| segment.envelopes).sum();
        summary.evidence_complete = summary.ready_before_submission
            && summary.delivery_complete
            && summary.payloads_complete
            && summary.continuity_complete;
        summary.status = if self.restart_gap {
            "daemon_restart_gap"
        } else if summary.evidence_complete {
            "evidence_complete"
        } else {
            "evidence_incomplete"
        };
        // A multi-observation aggregate has no single authoritative engine cutoff.
        summary.finalization = if self.segments.len() == 1 {
            self.segments[0].finalization.clone()
        } else {
            None
        };
        let receipt = serde_json::json!({
            "schema_version": 1, "session_id": self.session,
            "restart_gap": self.restart_gap, "segments": self.segments,
            "evidence_complete": summary.evidence_complete,
            "workflow_children_complete": false,
        });
        let written = serde_json::to_vec_pretty(&receipt).is_ok_and(|bytes| {
            std::fs::write(self.directory.join("xray-segments.json"), bytes).is_ok()
        });
        if !written {
            summary.evidence_complete = false;
            summary.delivery_complete = false;
            summary.status = "segment_receipt_write_failed";
        }
        if let Ok(bytes) = serde_json::to_vec_pretty(&summary)
            && std::fs::write(self.directory.join("xray-evidence.json"), bytes).is_err()
        {
            summary.evidence_complete = false;
            summary.delivery_complete = false;
            summary.status = "evidence_receipt_write_failed";
        }
        Some(summary)
    }
}

impl Drop for SessionCapture {
    fn drop(&mut self) {
        if let Some(capture) = self.active.take() {
            self.segments.push(
                capture
                    .finish()
                    .unwrap_or_else(|| EvalXrayEvidence::new(self.session.clone())),
            );
            // Unwinding/error return is not an explicit complete run close.
            self.restart_gap = true;
            let _ = self.publish();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn actual_segment_receipts_are_all_retained_across_restart() {
        let first = super::super::tests::capture_fixture(false, false);
        let second = super::super::tests::capture_fixture(false, false);
        let directory = tempfile::tempdir().unwrap();
        let capture = SessionCapture {
            active: None,
            directory: directory.path().into(),
            session: "session".into(),
            segments: vec![first, second],
            restart_gap: true,
        };
        let receipt = capture.finish().unwrap();
        assert!(!receipt.evidence_complete);
        assert!(!receipt.continuity_complete);
        assert!(receipt.delivery_complete);
        assert_eq!(receipt.status, "daemon_restart_gap");
        assert!(receipt.finalization.is_none());
        let json: serde_json::Value = serde_json::from_slice(
            &std::fs::read(directory.path().join("xray-segments.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(json["segments"].as_array().unwrap().len(), 2);
        assert_eq!(json["restart_gap"], true);
    }

    #[test]
    fn one_failed_segment_cannot_be_hidden_by_a_successful_later_segment() {
        let first = EvalXrayEvidence::new("session".into());
        let second = super::super::tests::capture_fixture(false, false);
        let directory = tempfile::tempdir().unwrap();
        let capture = SessionCapture {
            active: None,
            directory: directory.path().into(),
            session: "session".into(),
            segments: vec![first, second],
            restart_gap: false,
        };
        let receipt = capture.finish().unwrap();
        assert!(!receipt.evidence_complete);
        assert!(!receipt.delivery_complete);
        assert!(!receipt.ready_before_submission);
    }
}
