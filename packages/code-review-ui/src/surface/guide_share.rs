//! Sharing the guide and choosing how it is written: copying a section or the whole guide as
//! markdown, exporting it to a file through the markdown file publisher, and asking for a
//! walkthrough with a chosen template.
//!
//! * **Copy.** `y` in the guide panel copies the current section; "Guide: Copy as markdown"
//!   copies the whole version. Citations of GitHub pull request sources link to their
//!   permalinks; others name their path, lines, and short commit.
//! * **Export.** "Guide: Export to file" writes the same markdown with the built-in
//!   `markdown_file` publisher to `.bcode/guides/<guide>-v<N>.md` in the repository; the
//!   publisher keeps the path inside the repository.
//! * **Template.** "Guide: walk me through this review, with a template…" lists the built-in
//!   and configured templates; choosing one puts the request with that template in the
//!   assistant's input for the reviewer to add directions.

use super::ReviewSurface;
use crate::action::Action;
use crate::assistant::LinkRequest;
use crate::context::UiContext;
use crate::loader::Message;
use crate::palette::{OverlayEntry, OverlayKind};
use crate::services::{ReviewServices, SubmitBundle};
use bcode_code_review::guide::export::{
    ExportOptions, LinkStyle, forge_repositories, render_markdown,
};
use bcode_code_review::guide::template::{BUILT_IN, resolve_template};
use bcode_code_review_models::{PublishBundleRequest, ReviewBundle, ReviewTarget};
use bmux_tui_components::status_bar::StatusSeverity;
use std::path::PathBuf;

/// Action id prefix of a template chosen from the template menu (followed by its name).
pub const GUIDE_TEMPLATE_PREFIX: &str = "guide_template:";
/// The start of the message asking for a walkthrough with a template (followed by its name).
pub const WALKTHROUGH_TEMPLATE: &str = "Walk me through this review using the ";
/// Folder in the repository guide exports are written to.
pub const EXPORT_DIR: &str = ".bcode/guides";
/// Publisher guide exports are written with.
const EXPORT_PUBLISHER: &str = "markdown_file";

/// The message asking for a walkthrough with `template`, for the reviewer to add directions.
#[must_use]
pub fn walkthrough_with(template: &str) -> String {
    format!("{WALKTHROUGH_TEMPLATE}{template} template. Directions: ")
}

impl ReviewSurface {
    /// The guide's sharing and template actions. `None` for other actions.
    pub(super) fn guide_share_action(&mut self, action: &str, host: &UiContext) -> Option<Action> {
        if let Some(name) = action.strip_prefix(GUIDE_TEMPLATE_PREFIX) {
            self.walk_me_through_with(name);
            return Some(Action::Redraw);
        }
        match action {
            "guide_walkthrough_template" => self.open_template_menu(),
            "guide_copy_all" => self.guide_copy_markdown(false),
            "guide_export" => self.guide_export(host),
            _ => return None,
        }
        Some(Action::Redraw)
    }

    /// The guide on screen as markdown: the current section (`section`) or the whole version.
    /// `None` without a guide.
    pub(super) fn guide_markdown(&self, section: bool) -> Option<String> {
        let active = self.guide.state.active.as_ref()?;
        let sections = if section {
            vec![active.current()?.id.clone()]
        } else {
            Vec::new()
        };
        let repositories = self
            .workspace
            .as_ref()
            .map(|workspace| forge_repositories(&workspace.sources))
            .unwrap_or_default();
        Some(render_markdown(
            &active.version,
            &ExportOptions {
                title: &active.guide.title,
                repositories,
                links: LinkStyle::Permalink,
                include_unverified: true,
                sections,
            },
        ))
    }

    /// Copy the current section (`section`) or the whole guide as markdown.
    pub(super) fn guide_copy_markdown(&mut self, section: bool) {
        match self.guide_markdown(section) {
            Some(markdown) => {
                self.clipboard = Some((markdown, if section { "guide section" } else { "guide" }));
            }
            None => self.no_guide(),
        }
    }

    /// Write the guide as markdown to the repository through the markdown file publisher.
    fn guide_export(&mut self, host: &UiContext) {
        let Some(markdown) = self.guide_markdown(false) else {
            self.no_guide();
            return;
        };
        let Some(active) = self.guide.state.active.as_ref() else {
            return;
        };
        let file = format!(
            "{EXPORT_DIR}/{}-v{}.md",
            safe_file_name(&active.guide.guide_id),
            active.version.version
        );
        let (repo_root, review_id) = self.workspace.as_ref().map_or_else(
            || (self.repo_path.clone(), String::new()),
            |workspace| (workspace.repo_root.clone(), workspace.id.clone()),
        );
        let bundle = ReviewBundle {
            review_id,
            title: active.guide.title.clone(),
            repo_root,
            target: ReviewTarget::Repository,
            files: Vec::new(),
            surfaces: Vec::new(),
            threads: Vec::new(),
            generated_at_ms: 0,
            pull_request: None,
        };
        let services = self.services.clone();
        let inbox = self.inbox.clone();
        self.set_status(
            format!("exporting the guide to {file}…"),
            StatusSeverity::Info,
        );
        host.spawn(Box::pin(async move {
            let result = export(&services, bundle, markdown, file).await;
            inbox.push(Message::GuideExported(result));
        }));
    }

    /// A guide export finished: say where it went, or why it did not.
    pub(super) fn guide_exported(&mut self, result: Result<String, String>) {
        match result {
            Ok(path) => self.set_status(
                format!("exported the guide to {path}"),
                StatusSeverity::Info,
            ),
            Err(error) => self.set_status(
                format!("could not export the guide: {error}"),
                StatusSeverity::Warning,
            ),
        }
    }

    /// The templates a walkthrough can use, in a menu: the built-in ones, then the configured
    /// ones, each with the sections it requires.
    fn open_template_menu(&mut self) {
        let config = self.services.guide_config();
        let mut names: Vec<String> = BUILT_IN.map(str::to_owned).to_vec();
        names.extend(config.templates.keys().cloned());
        let entries = names
            .into_iter()
            .map(|name| {
                let label = resolve_template(&config, Some(&name), false).map_or_else(
                    |error| format!("{name} · {error}"),
                    |template| {
                        format!(
                            "{name} · requires {}",
                            template.required_sections.join(", ")
                        )
                    },
                );
                OverlayEntry {
                    action: Some(format!("{GUIDE_TEMPLATE_PREFIX}{name}")),
                    label,
                    keys: String::new(),
                }
            })
            .collect();
        self.overlay = Some(crate::palette::Overlay::new(
            OverlayKind::Menu,
            entries,
            &self.theme,
        ));
    }

    /// Put the walkthrough request with `template` in the assistant's input.
    fn walk_me_through_with(&mut self, template: &str) {
        self.link.requests.push(LinkRequest::Message {
            text: walkthrough_with(template),
            send: false,
        });
        self.set_status(
            format!("add your directions for the {template} walkthrough and press enter"),
            StatusSeverity::Info,
        );
    }
}

/// Submit `markdown` to the markdown file publisher at `file` (relative to the repository).
async fn export(
    services: &ReviewServices,
    bundle: ReviewBundle,
    markdown: String,
    file: String,
) -> Result<String, String> {
    let response = services
        .call(SubmitBundle(PublishBundleRequest {
            publisher_id: EXPORT_PUBLISHER.to_owned(),
            bundle,
            options: serde_json::json!({ "document": markdown, "output_path": file }),
        }))
        .await
        .map_err(|error| error.message)?;
    Ok(response
        .output
        .map_or(file, |output| PathBuf::from(output).display().to_string()))
}

/// `id` with only letters, digits, `-`, `_` and `.` kept (others become `-`).
fn safe_file_name(id: &str) -> String {
    id.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                c
            } else {
                '-'
            }
        })
        .collect()
}
