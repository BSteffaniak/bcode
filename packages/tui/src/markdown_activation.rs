//! Safe Markdown destination activation boundary.

use bcode_markdown_render::MarkdownDestination;

/// Result of attempting to activate a classified Markdown destination.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MarkdownActivation {
    /// A safe web or local destination was handed to the platform opener.
    External,
    /// A document fragment should be handled by internal transcript navigation.
    Fragment,
    /// The destination is intentionally non-actionable.
    Inert,
}

/// Failure reported by the platform opener.
#[derive(Debug)]
pub struct MarkdownActivationError(String);

impl std::fmt::Display for MarkdownActivationError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for MarkdownActivationError {}

/// Activate a destination only after it has passed Markdown classification.
///
/// Web URLs and trusted local paths reach the platform opener. Fragments are
/// returned to the caller for internal navigation. Inert and unresolved
/// destinations never produce external side effects.
///
/// # Errors
///
/// Returns an error when the operating-system opener rejects an otherwise safe
/// web or local destination.
pub fn activate_markdown_destination(
    destination: &MarkdownDestination,
) -> Result<MarkdownActivation, MarkdownActivationError> {
    activate_markdown_destination_with(destination, |target| {
        open::that_detached(target).map_err(|error| MarkdownActivationError(error.to_string()))
    })
}

fn activate_markdown_destination_with(
    destination: &MarkdownDestination,
    open_external: impl FnOnce(&std::ffi::OsStr) -> Result<(), MarkdownActivationError>,
) -> Result<MarkdownActivation, MarkdownActivationError> {
    match destination {
        MarkdownDestination::Web(url) => {
            open_external(std::ffi::OsStr::new(url.as_str()))?;
            Ok(MarkdownActivation::External)
        }
        MarkdownDestination::LocalPath(path) => {
            open_external(path.as_os_str())?;
            Ok(MarkdownActivation::External)
        }
        MarkdownDestination::Fragment(_) => Ok(MarkdownActivation::Fragment),
        MarkdownDestination::Inert { .. } | MarkdownDestination::UnresolvedRelative(_) => {
            Ok(MarkdownActivation::Inert)
        }
    }
}

/// Copy a destination only when its classification permits external use.
///
/// # Errors
///
/// Returns a clipboard initialization or write error for safe web/local
/// destinations. Inert, unresolved, and fragment destinations return `Ok(false)`.
pub fn copy_markdown_destination(
    destination: &MarkdownDestination,
) -> Result<bool, arboard::Error> {
    let Some(text) = markdown_destination_text(destination) else {
        return Ok(false);
    };
    let mut clipboard = arboard::Clipboard::new()?;
    clipboard.set_text(text)?;
    Ok(true)
}

/// Copy arbitrary explicit text to the system clipboard.
///
/// # Errors
///
/// Returns a clipboard initialization or write error.
pub fn copy_text(text: &str) -> Result<(), arboard::Error> {
    let mut clipboard = arboard::Clipboard::new()?;
    clipboard.set_text(text)
}

/// Return text suitable for explicit copy from a safe destination.
#[must_use]
pub fn markdown_destination_text(destination: &MarkdownDestination) -> Option<String> {
    match destination {
        MarkdownDestination::Web(url) => Some(url.as_str().to_owned()),
        MarkdownDestination::LocalPath(path) => Some(path.to_string_lossy().into_owned()),
        MarkdownDestination::Fragment(_)
        | MarkdownDestination::Inert { .. }
        | MarkdownDestination::UnresolvedRelative(_) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::{
        MarkdownActivation, activate_markdown_destination, activate_markdown_destination_with,
        copy_markdown_destination, markdown_destination_text,
    };
    use bcode_markdown_render::{
        MarkdownDestination, MarkdownDestinationRejection, resolve_markdown_destination,
    };
    use std::cell::RefCell;

    #[test]
    fn classified_web_and_local_destinations_reach_external_adapter() {
        let opened = RefCell::new(Vec::new());
        let activate = |destination: &MarkdownDestination| {
            activate_markdown_destination_with(destination, |target| {
                opened
                    .borrow_mut()
                    .push(target.to_string_lossy().into_owned());
                Ok(())
            })
            .unwrap()
        };

        assert_eq!(
            activate(&resolve_markdown_destination(
                "https://example.com/docs",
                None,
            )),
            MarkdownActivation::External
        );
        assert_eq!(
            markdown_destination_text(&resolve_markdown_destination(
                "https://example.com/docs",
                None,
            ))
            .as_deref(),
            Some("https://example.com/docs")
        );
        assert_eq!(
            activate(&MarkdownDestination::LocalPath(std::path::PathBuf::from(
                "/trusted/docs.md"
            ))),
            MarkdownActivation::External
        );
        assert_eq!(
            opened.into_inner(),
            ["https://example.com/docs", "/trusted/docs.md"]
        );
    }

    #[test]
    fn safe_destination_text_supports_copy_policy_without_clipboard_side_effects() {
        assert_eq!(
            markdown_destination_text(&MarkdownDestination::Web(
                "https://example.com/docs".parse().expect("valid URL")
            ))
            .as_deref(),
            Some("https://example.com/docs")
        );
        assert_eq!(
            markdown_destination_text(&MarkdownDestination::LocalPath("/trusted/docs.md".into()))
                .as_deref(),
            Some("/trusted/docs.md")
        );
        for destination in [
            MarkdownDestination::Fragment("section".to_owned()),
            MarkdownDestination::UnresolvedRelative("docs.md".to_owned()),
            MarkdownDestination::Inert {
                reason: MarkdownDestinationRejection::UnsupportedScheme,
            },
        ] {
            assert_eq!(markdown_destination_text(&destination), None);
        }
    }

    #[test]
    fn fragments_are_reserved_for_internal_navigation() {
        assert_eq!(
            activate_markdown_destination(&MarkdownDestination::Fragment("section".to_owned()))
                .unwrap(),
            MarkdownActivation::Fragment
        );
    }

    #[test]
    fn inert_and_unresolved_destinations_never_reach_platform_opener() {
        for destination in [
            MarkdownDestination::Inert {
                reason: MarkdownDestinationRejection::UnsupportedScheme,
            },
            MarkdownDestination::UnresolvedRelative("relative.md".to_owned()),
        ] {
            let opener_called = RefCell::new(false);
            assert_eq!(
                activate_markdown_destination_with(&destination, |_| {
                    *opener_called.borrow_mut() = true;
                    Ok(())
                })
                .unwrap(),
                MarkdownActivation::Inert
            );
            assert!(!opener_called.into_inner());
            assert!(!copy_markdown_destination(&destination).unwrap());
        }
    }
}
