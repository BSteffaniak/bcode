//! Host-side attribution and collection of private usage events.

use bcode_session_models::OriginalUsage;

/// Accept original reports only from the provider that owns this request attempt.
/// Both regular turns and compaction use this boundary rather than trusting event labels.
///
/// # Errors
///
/// Rejects foreign provider attribution or malformed/unbounded evidence.
pub fn receive_original_usage(
    provider: &str,
    target: &mut Option<OriginalUsage>,
    original: OriginalUsage,
) -> Result<(), String> {
    if original.provider_id != provider {
        return Err("original usage provider mismatch".into());
    }
    if let Some(current) = target.as_ref()
        && current.api_shape != original.api_shape
    {
        return Err("original usage API shape mismatch".into());
    }
    original.validate()?;
    super::append_usage_capture(target, original);
    Ok(())
}
