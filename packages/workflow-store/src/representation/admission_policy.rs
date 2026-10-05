//! Explicit admission/snapshot work policy, separate from individual payload bounds.
/// Configurable limits for operations that intentionally validate a complete candidate snapshot.
/// `None` permits growth without a representation-owned total-graph byte ceiling.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct GraphAdmissionPolicy {
    /// Maximum serialized graph bytes for one admission/publication, if configured.
    pub max_snapshot_bytes: Option<usize>,
}
impl GraphAdmissionPolicy {
    pub(super) fn check(self, bytes: usize) -> Result<(), super::RepresentationError> {
        if self.max_snapshot_bytes.is_some_and(|limit| bytes > limit) {
            return Err(super::RepresentationError::InvalidAdmission);
        }
        Ok(())
    }
}
