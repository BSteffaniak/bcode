//! Schema-versioned plugin visual and artifact adapters.

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::sync::LazyLock;

use super::theme::{color, radius, space, surface, typeface};
use bcode_session_view_models::{PluginVisualView, ToolArtifactView};
use hyperchad::template::{Containers, container};

pub(super) type VisualAdapter = fn(&PluginVisualView) -> Option<Containers>;
pub(super) type ArtifactAdapter = fn(&ToolArtifactView) -> Option<Containers>;

pub(super) static ARTIFACT_ADAPTERS: LazyLock<BTreeMap<(&'static str, u32), ArtifactAdapter>> =
    LazyLock::new(|| {
        BTreeMap::from([
            (
                ("bcode.document.extract_result", 1),
                render_document_extract_result as ArtifactAdapter,
            ),
            (
                ("bcode.document.status", 1),
                render_document_status as ArtifactAdapter,
            ),
            (
                ("bcode.filesystem.read", 1),
                render_filesystem_read_result as ArtifactAdapter,
            ),
            (
                ("bcode.filesystem.change", 1),
                render_filesystem_change_result as ArtifactAdapter,
            ),
            (
                ("bcode.filesystem.exists", 1),
                render_filesystem_exists_result as ArtifactAdapter,
            ),
            (
                ("bcode.filesystem.list", 1),
                render_filesystem_list_result as ArtifactAdapter,
            ),
            (
                ("bcode.filesystem.find", 1),
                render_filesystem_find_result as ArtifactAdapter,
            ),
            (
                ("bcode.filesystem.grep", 1),
                render_filesystem_grep_result as ArtifactAdapter,
            ),
            (
                ("bcode.filesystem.stat", 1),
                render_filesystem_stat_result as ArtifactAdapter,
            ),
            (
                ("bcode.filesystem.artifact.metadata", 1),
                render_filesystem_artifact_metadata as ArtifactAdapter,
            ),
            (
                ("bcode.filesystem.artifact.read", 1),
                render_filesystem_artifact_read as ArtifactAdapter,
            ),
            (
                ("bcode.filesystem.artifact.grep", 1),
                render_filesystem_artifact_grep as ArtifactAdapter,
            ),
            (
                ("bcode.git.clone_result", 1),
                render_git_clone_result as ArtifactAdapter,
            ),
            (
                ("bcode.ocr.extract_result", 1),
                render_ocr_extract_result as ArtifactAdapter,
            ),
            (
                ("bcode.ocr.status", 1),
                render_ocr_status as ArtifactAdapter,
            ),
            (
                ("bcode.shell.run", 1),
                render_shell_result as ArtifactAdapter,
            ),
            (
                ("bcode.question.outcome", 1),
                render_question_outcome as ArtifactAdapter,
            ),
            (
                ("bcode.web-search.search_results", 1),
                render_web_search_results as ArtifactAdapter,
            ),
            (
                ("bcode.web-search.fetch_result", 1),
                render_web_fetch_result as ArtifactAdapter,
            ),
            (
                ("bcode.web-search.status", 1),
                render_web_status as ArtifactAdapter,
            ),
            (
                ("bcode.web-search.inspect_result", 1),
                render_web_inspect_result as ArtifactAdapter,
            ),
            (
                ("bcode.worktree.list", 1),
                render_worktree_list_result as ArtifactAdapter,
            ),
            (
                ("bcode.worktree.create_result", 1),
                render_worktree_create_result as ArtifactAdapter,
            ),
            (
                ("bcode.worktree.remove_result", 1),
                render_worktree_remove_result as ArtifactAdapter,
            ),
        ])
    });

pub(super) static VISUAL_ADAPTERS: LazyLock<BTreeMap<(&'static str, u32), VisualAdapter>> =
    LazyLock::new(|| {
        BTreeMap::from([
            (
                ("bcode.tool.request.shell.run", 1),
                render_shell_request as VisualAdapter,
            ),
            (
                ("bcode.filesystem.request", 1),
                render_filesystem_request as VisualAdapter,
            ),
            (
                ("bcode.filesystem.change", 1),
                render_filesystem_change as VisualAdapter,
            ),
            (
                ("bcode.filesystem.read", 1),
                render_filesystem_request as VisualAdapter,
            ),
            (
                ("bcode.filesystem.image", 1),
                render_filesystem_request as VisualAdapter,
            ),
            (
                ("bcode.filesystem.exists", 1),
                render_filesystem_request as VisualAdapter,
            ),
            (
                ("bcode.filesystem.list", 1),
                render_filesystem_request as VisualAdapter,
            ),
            (
                ("bcode.filesystem.find", 1),
                render_filesystem_request as VisualAdapter,
            ),
            (
                ("bcode.filesystem.grep", 1),
                render_filesystem_request as VisualAdapter,
            ),
            (
                ("bcode.filesystem.stat", 1),
                render_filesystem_request as VisualAdapter,
            ),
            (
                ("bcode.filesystem.artifact.metadata", 1),
                render_filesystem_request as VisualAdapter,
            ),
            (
                ("bcode.filesystem.artifact.read", 1),
                render_filesystem_request as VisualAdapter,
            ),
            (
                ("bcode.filesystem.artifact.grep", 1),
                render_filesystem_request as VisualAdapter,
            ),
            (
                ("bcode.document.request", 1),
                render_extraction_request as VisualAdapter,
            ),
            (
                ("bcode.ocr.request", 1),
                render_extraction_request as VisualAdapter,
            ),
            (
                ("bcode.web-search.search_request", 1),
                render_web_search_request as VisualAdapter,
            ),
            (
                ("bcode.web-search.fetch_request", 1),
                render_web_fetch_request as VisualAdapter,
            ),
            (
                ("bcode.web-search.status_request", 1),
                render_web_utility_request as VisualAdapter,
            ),
            (
                ("bcode.web-search.inspect_request", 1),
                render_web_utility_request as VisualAdapter,
            ),
            (
                ("bcode.git.clone_request", 1),
                render_git_clone_request as VisualAdapter,
            ),
            (
                ("bcode.worktree.request", 1),
                render_worktree_request as VisualAdapter,
            ),
            (
                ("bcode.vim-edit.request.preview", 1),
                render_vim_edit_request as VisualAdapter,
            ),
            (
                ("bcode.vim-edit.request.apply", 1),
                render_vim_edit_request as VisualAdapter,
            ),
            (
                ("bcode.vim-edit.live", 1),
                render_vim_edit_live as VisualAdapter,
            ),
            (
                ("bcode.vim-edit.playback", 1),
                render_vim_edit_playback as VisualAdapter,
            ),
        ])
    });

const MAX_EXTRACTED_TEXT_CHARS: usize = 32_000;

fn extracted_text_panel(text: &str, source_truncated: Option<bool>) -> Containers {
    let (text, display_truncated) = text
        .char_indices()
        .nth(MAX_EXTRACTED_TEXT_CHARS)
        .map_or_else(
            || (text, false),
            |(byte_index, _)| (&text[..byte_index], true),
        );
    container! {
        div border-top=((1, surface::BORDER)) margin-top=((space::SM)) padding-top=((space::SM)) {
            div color=(color::TEXT) font-size=((typeface::LABEL)) white-space="preserve-wrap" { (text) }
            @if source_truncated == Some(true) {
                div color=(color::WARNING) font-size=((typeface::DETAIL)) margin-top=((space::SM)) { "Source extraction was truncated." }
            }
            @if display_truncated {
                div color=(color::WARNING) font-size=((typeface::DETAIL)) margin-top=((space::SM)) { "Extracted text truncated for display." }
            }
        }
    }
}

fn artifact_references(artifact: &ToolArtifactView) -> Containers {
    container! {
        @if !artifact.artifact.refs.is_empty() {
            details margin-top=((space::SM)) {
                summary color=(color::INFO) font-size=((typeface::DETAIL)) { "artifact references (" (artifact.artifact.refs.len().to_string()) ")" }
                @for reference in artifact.artifact.refs.iter().take(10) {
                    div border-top=((1, surface::BORDER)) padding-top=((space::S6)) margin-top=((space::S6)) {
                        div color=(color::STRONG) font-family="monospace" { (reference.key) }
                        @if let Some(content_type) = &reference.content_type { div color=(color::MUTED) font-size=((typeface::DETAIL)) { (content_type) } }
                        @if let Some(storage_uri) = &reference.storage_uri { div color=(color::MUTED) font-size=((typeface::DETAIL)) white-space="preserve-wrap" { (storage_uri) } }
                        @if let Some(byte_len) = reference.byte_len { div color=(color::MUTED) font-size=((typeface::DETAIL)) { (byte_len.to_string()) " bytes" } }
                    }
                }
                @if artifact.artifact.refs.len() > 10 {
                    div color=(color::MUTED) font-size=((typeface::DETAIL)) margin-top=((space::S6)) { "… more references" }
                }
            }
        }
    }
}

pub(super) fn render_document_extract_result(artifact: &ToolArtifactView) -> Option<Containers> {
    let metadata = &artifact.artifact.metadata;
    let source = metadata.get("source").and_then(serde_json::Value::as_str)?;
    let content_type = metadata
        .get("content_type")
        .and_then(serde_json::Value::as_str);
    let extractor = metadata
        .get("extractor")
        .and_then(serde_json::Value::as_str);
    let truncated = metadata
        .get("truncated")
        .and_then(serde_json::Value::as_bool);
    let document_path = metadata
        .get("document_path")
        .and_then(serde_json::Value::as_str);
    let text_path = metadata
        .get("text_path")
        .and_then(serde_json::Value::as_str);
    let text = metadata.get("text").and_then(serde_json::Value::as_str);
    Some(container! {
        div border=((1, surface::BORDER)) border-radius=((radius::CONTROL)) background=(surface::INSET) padding=((space::S10)) margin-top=((space::SM)) {
            div color=(color::INFO) margin-bottom=((space::S6)) { (artifact.artifact.title.as_deref().unwrap_or("Document extraction")) }
            div color=(color::STRONG) font-family="monospace" white-space="preserve-wrap" { (source) }
            @if let Some(content_type) = content_type { div color=(color::MUTED) font-size=((typeface::LABEL)) margin-top=((space::XS)) { "type: " (content_type) } }
            @if let Some(extractor) = extractor { div color=(color::MUTED) font-size=((typeface::LABEL)) margin-top=((space::XS)) { "extractor: " (extractor) } }
            @if let Some(document_path) = document_path { div color=(color::MUTED) font-size=((typeface::LABEL)) margin-top=((space::XS)) font-family="monospace" white-space="preserve-wrap" { "document: " (document_path) } }
            @if let Some(text_path) = text_path { div color=(color::MUTED) font-size=((typeface::LABEL)) margin-top=((space::XS)) font-family="monospace" white-space="preserve-wrap" { "text: " (text_path) } }
            @if truncated == Some(true) { div color=(color::WARNING) font-size=((typeface::LABEL)) margin-top=((space::XS)) { "Source extraction was truncated." } }
            @if let Some(text) = text { (extracted_text_panel(text, None)) } @else { div color=(color::MUTED) font-size=((typeface::LABEL)) margin-top=((space::SM)) { "No extracted text was returned." } }
            (artifact_references(artifact))        }
    })
}

pub(super) fn render_document_status(artifact: &ToolArtifactView) -> Option<Containers> {
    render_extract_capabilities(artifact, "Document extractors", "extractors")
}

const MAX_FILE_CONTENT_CHARS: usize = 32_000;

fn source_language(path: &str) -> Option<&'static str> {
    let extension = std::path::Path::new(path).extension()?.to_str()?;
    match extension.to_ascii_lowercase().as_str() {
        "rs" => Some("rust"),
        "js" | "mjs" | "cjs" => Some("javascript"),
        "ts" | "tsx" => Some("typescript"),
        "py" => Some("python"),
        "sh" | "bash" | "zsh" => Some("bash"),
        "json" => Some("json"),
        "toml" => Some("toml"),
        "yaml" | "yml" => Some("yaml"),
        "md" => Some("markdown"),
        "html" | "htm" => Some("html"),
        "css" => Some("css"),
        "sql" => Some("sql"),
        _ => None,
    }
}

fn markdown_code_fence(contents: &str) -> String {
    let max_run = contents
        .split(|character| character != '`')
        .map(str::len)
        .max()
        .unwrap_or_default();
    "`".repeat(max_run.max(2).saturating_add(1))
}

fn file_content_panel(path: &str, contents: &str) -> Containers {
    let (contents, display_truncated) = contents
        .char_indices()
        .nth(MAX_FILE_CONTENT_CHARS)
        .map_or_else(
            || (contents, false),
            |(byte_index, _)| (&contents[..byte_index], true),
        );
    let language = source_language(path);
    let rendered = language.map(|language| {
        let fence = markdown_code_fence(contents);
        hyperchad::markdown::markdown_to_container(&format!(
            "{fence}{language}\n{contents}\n{fence}"
        ))
    });
    container! {
        div border-top=((1, surface::BORDER)) margin-top=((space::SM)) padding-top=((space::SM)) {
            @if let Some(rendered) = rendered {
                (rendered)
            } @else {
                div color=(color::TEXT) font-size=((typeface::LABEL)) font-family="monospace" white-space="preserve-wrap" { (contents) }
            }
            @if display_truncated {
                div color=(color::WARNING) font-size=((typeface::DETAIL)) margin-top=((space::SM)) { "File contents truncated for display." }
            }
        }
    }
}

pub(super) fn render_filesystem_read_result(artifact: &ToolArtifactView) -> Option<Containers> {
    let metadata = &artifact.artifact.metadata;
    let path = metadata.get("path").and_then(serde_json::Value::as_str)?;
    let contents = metadata
        .get("contents")
        .and_then(serde_json::Value::as_str)?;
    let start_line = metadata
        .get("start_line")
        .and_then(serde_json::Value::as_u64);
    let end_line = metadata.get("end_line").and_then(serde_json::Value::as_u64);
    let total_lines = metadata
        .get("total_lines")
        .and_then(serde_json::Value::as_u64);
    let returned_bytes = metadata
        .get("returned_bytes")
        .and_then(serde_json::Value::as_u64);
    let total_bytes = metadata
        .get("total_bytes")
        .and_then(serde_json::Value::as_u64);
    let truncated = metadata
        .get("truncated")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    Some(container! {
        div border=((1, surface::BORDER)) border-radius=((radius::CONTROL)) background=(surface::INSET) padding=((space::S10)) margin-top=((space::SM)) {
            div color=(color::INFO) margin-bottom=((space::S6)) { (artifact.artifact.title.as_deref().unwrap_or("File contents")) }
            div color=(color::STRONG) font-family="monospace" white-space="preserve-wrap" { (path) }
            div color=(color::MUTED) font-size=((typeface::DETAIL)) margin-top=((space::XS)) {
                @if let (Some(start_line), Some(end_line), Some(total_lines)) = (start_line, end_line, total_lines) {
                    "lines " (start_line.to_string()) "–" (end_line.to_string()) " of " (total_lines.to_string())
                }
                @if let (Some(returned_bytes), Some(total_bytes)) = (returned_bytes, total_bytes) {
                    " · " (returned_bytes.to_string()) " of " (total_bytes.to_string()) " bytes"
                }
                @if let Some(language) = source_language(path) { " · " (language) }
            }
            (file_content_panel(path, contents))
            @if truncated {
                div color=(color::WARNING) font-size=((typeface::DETAIL)) margin-top=((space::SM)) {
                    "More file content is available."
                    @if let Some(end_line) = end_line { " Continue at offset " (end_line.saturating_add(1).to_string()) "." }
                }
            }
            (artifact_references(artifact))
        }
    })
}

fn supported_inline_image_content_type(content_type: &str) -> bool {
    matches!(
        content_type.split(';').next().map(str::trim),
        Some("image/png" | "image/jpeg" | "image/gif" | "image/webp")
    )
}

pub(super) fn render_filesystem_image_result(
    artifact: &ToolArtifactView,
    guarded_source: Option<&str>,
) -> Option<Containers> {
    let metadata = &artifact.artifact.metadata;
    let path = metadata.get("path").and_then(serde_json::Value::as_str)?;
    let mime_type = metadata
        .get("mime_type")
        .and_then(serde_json::Value::as_str);
    let width = metadata.get("width").and_then(serde_json::Value::as_u64);
    let height = metadata.get("height").and_then(serde_json::Value::as_u64);
    let byte_len = metadata.get("byte_len").and_then(serde_json::Value::as_u64);
    let preview_source = guarded_source;
    let guarded_reference = artifact.artifact.refs.iter().find(|reference| {
        !reference.key.is_empty()
            && reference
                .content_type
                .as_deref()
                .is_some_and(supported_inline_image_content_type)
    });
    Some(container! {
        div border=((1, surface::BORDER)) border-radius=((radius::CONTROL)) background=(surface::INSET) padding=((space::S10)) margin-top=((space::SM)) {
            div color=(color::INFO) margin-bottom=((space::S6)) { (artifact.artifact.title.as_deref().unwrap_or("Image file")) }
            @if let Some(preview_source) = preview_source {
                image src=(preview_source) alt=(format!("Preview of {}", std::path::Path::new(path).file_name().and_then(std::ffi::OsStr::to_str).unwrap_or("image"))) fit="contain" loading="lazy" width=100% max-height=560 border=((1, surface::BORDER)) border-radius=((radius::CONTROL));
            } @else if guarded_reference.is_some() {
                div color=(color::MUTED) background=(surface::PANEL) border=((1, surface::BORDER)) border-radius=((radius::CONTROL)) padding=((space::SM)) margin-bottom=((space::SM)) {
                    "Image preview is protected. Use the guarded artifact control when one is available."
                }
            } @else {
                div color=(color::MUTED) background=(surface::PANEL) border=((1, surface::BORDER)) border-radius=((radius::CONTROL)) padding=((space::SM)) margin-bottom=((space::SM)) {
                    "No safe inline image source is available."
                }
            }
            div color=(color::STRONG) font-family="monospace" white-space="preserve-wrap" { (path) }
            @if let Some(mime_type) = mime_type { div color=(color::MUTED) font-size=((typeface::LABEL)) margin-top=((space::XS)) { "type: " (mime_type) } }
            @if let (Some(width), Some(height)) = (width, height) { div color=(color::MUTED) font-size=((typeface::LABEL)) margin-top=((space::XS)) { "dimensions: " (width.to_string()) "x" (height.to_string()) } }
            @if let Some(byte_len) = byte_len { div color=(color::MUTED) font-size=((typeface::LABEL)) margin-top=((space::XS)) { "bytes: " (byte_len.to_string()) } }
            @if let Some(reference) = guarded_reference {
                div color=(color::MUTED) font-size=((typeface::DETAIL)) margin-top=((space::S6)) {
                    "guarded artifact: " (reference.key)
                    @if let Some(byte_len) = reference.byte_len { " · " (byte_len.to_string()) " bytes" }
                }
            }
        }
    })
}

const MAX_DIFF_SIDE_CHARS: usize = 16_000;

fn bounded_diff_side(text: &str) -> (&str, bool) {
    text.char_indices().nth(MAX_DIFF_SIDE_CHARS).map_or_else(
        || (text, false),
        |(byte_index, _)| (&text[..byte_index], true),
    )
}

fn line_range(start_line: Option<u64>, text: &str) -> Option<String> {
    let start = start_line?;
    let line_count = text.lines().count().max(1) as u64;
    Some(if line_count == 1 {
        format!("line {start}")
    } else {
        format!("lines {start}–{}", start.saturating_add(line_count - 1))
    })
}

pub(super) fn render_filesystem_change_result(artifact: &ToolArtifactView) -> Option<Containers> {
    let metadata = &artifact.artifact.metadata;
    let path = metadata.get("path").and_then(serde_json::Value::as_str)?;
    let summary = metadata.get("summary").and_then(serde_json::Value::as_str);
    let operation = metadata
        .get("tool_name")
        .and_then(serde_json::Value::as_str);
    let old_text = metadata.get("old_text").and_then(serde_json::Value::as_str);
    let new_text = metadata.get("new_text").and_then(serde_json::Value::as_str);
    let old_start_line = metadata
        .get("old_start_line")
        .or_else(|| metadata.get("start_line"))
        .and_then(serde_json::Value::as_u64);
    let new_start_line = metadata
        .get("new_start_line")
        .or_else(|| metadata.get("start_line"))
        .and_then(serde_json::Value::as_u64);
    let old = old_text.map(|text| {
        let range = line_range(old_start_line, text);
        let (text, truncated) = bounded_diff_side(text);
        (text, truncated, range)
    });
    let new = new_text.map(|text| {
        let range = line_range(new_start_line, text);
        let (text, truncated) = bounded_diff_side(text);
        (text, truncated, range)
    });
    Some(container! {
        div border=((1, surface::BORDER)) border-radius=((radius::CONTROL)) background=(surface::INSET) padding=((space::S10)) margin-top=((space::SM)) {
            div color=(color::INFO) margin-bottom=((space::S6)) { (artifact.artifact.title.as_deref().unwrap_or("File change")) }
            div color=(color::STRONG) font-family="monospace" white-space="preserve-wrap" { (path) }
            @if let Some(summary) = summary { div color=(color::MUTED) font-size=((typeface::LABEL)) margin-top=((space::XS)) white-space="preserve-wrap" { (summary) } }
            @if let Some(operation) = operation { div color=(color::MUTED) font-size=((typeface::DETAIL)) margin-top=((space::XS)) { "operation: " (operation) } }
            div direction=row gap=((space::SM)) margin-top=((space::SM)) {
                @if let Some((old_text, truncated, range)) = old {
                    div flex=1 background=(surface::ERROR_INSET) border=((1, color::ERROR_BORDER)) border-radius=((radius::CONTROL)) padding=((space::SM)) {
                        div color=(color::ERROR) font-size=((typeface::DETAIL)) margin-bottom=((space::XS)) { "removed" @if let Some(range) = range { " · " (range) } }
                        div color=(color::REMOVED_TEXT) font-family="monospace" white-space="preserve-wrap" { (old_text) }
                        @if truncated { div color=(color::WARNING) font-size=((typeface::DETAIL)) margin-top=((space::S6)) { "Removed text truncated for display." } }
                    }
                }
                @if let Some((new_text, truncated, range)) = new {
                    div flex=1 background=(surface::SUCCESS_INSET) border=((1, color::SUCCESS_BORDER)) border-radius=((radius::CONTROL)) padding=((space::SM)) {
                        div color=(color::SUCCESS) font-size=((typeface::DETAIL)) margin-bottom=((space::XS)) { "added" @if let Some(range) = range { " · " (range) } }
                        div color=(color::ADDED_TEXT) font-family="monospace" white-space="preserve-wrap" { (new_text) }
                        @if truncated { div color=(color::WARNING) font-size=((typeface::DETAIL)) margin-top=((space::S6)) { "Added text truncated for display." } }
                    }
                }
            }
        }
    })
}

pub(super) fn render_filesystem_exists_result(artifact: &ToolArtifactView) -> Option<Containers> {
    let metadata = &artifact.artifact.metadata;
    let exists = metadata
        .get("exists")
        .and_then(serde_json::Value::as_bool)?;
    let path = metadata.get("path").and_then(serde_json::Value::as_str);
    Some(container! {
        div border=((1, surface::BORDER)) border-radius=((radius::CONTROL)) background=(surface::INSET) padding=((space::S10)) margin-top=((space::SM)) {
            div color=(color::INFO) margin-bottom=((space::S6)) { (artifact.artifact.title.as_deref().unwrap_or("Path exists")) }
            div color=(color::STRONG) { (if exists { "Path exists" } else { "Path does not exist" }) }
            @if let Some(path) = path { div color=(color::MUTED) font-size=((typeface::LABEL)) font-family="monospace" margin-top=((space::XS)) { (path) } }
        }
    })
}

pub(super) fn render_filesystem_list_result(artifact: &ToolArtifactView) -> Option<Containers> {
    let entries = artifact
        .artifact
        .metadata
        .get("entries")
        .and_then(serde_json::Value::as_array)?;
    Some(container! {
        div border=((1, surface::BORDER)) border-radius=((radius::CONTROL)) background=(surface::INSET) padding=((space::S10)) margin-top=((space::SM)) {
            div color=(color::INFO) margin-bottom=((space::S6)) { (format!("{} ({})", artifact.artifact.title.as_deref().unwrap_or("Directory entries"), entries.len())) }
            @if entries.is_empty() { div color=(color::MUTED) font-size=((typeface::LABEL)) { "No directory entries." } }
            @for entry in entries.iter().take(25) {
                @if let Some(entry) = entry.as_object() {
                    div border-top=((1, surface::BORDER)) padding-top=((space::S6)) margin-top=((space::S6)) {
                        @if let Some(path) = entry.get("path").and_then(serde_json::Value::as_str) {
                            span color=(color::STRONG) font-family="monospace" white-space="preserve-wrap" { (path) }
                        }
                        @if let Some(kind) = entry.get("kind").and_then(serde_json::Value::as_str) {
                            span color=(color::MUTED) { " · " (kind) }
                        }
                    }
                }
            }
            @if entries.len() > 25 {
                div color=(color::MUTED) font-size=((typeface::LABEL)) margin-top=((space::SM)) { "… " ((entries.len() - 25).to_string()) " more entries" }
            }
            (filesystem_result_metadata(&artifact.artifact.metadata))
        }
    })
}

pub(super) fn render_filesystem_find_result(artifact: &ToolArtifactView) -> Option<Containers> {
    let paths = artifact
        .artifact
        .metadata
        .get("paths")
        .and_then(serde_json::Value::as_array)?;
    Some(container! {
        div border=((1, surface::BORDER)) border-radius=((radius::CONTROL)) background=(surface::INSET) padding=((space::S10)) margin-top=((space::SM)) {
            div color=(color::INFO) margin-bottom=((space::S6)) { (format!("{} ({})", artifact.artifact.title.as_deref().unwrap_or("Path matches"), paths.len())) }
            @if paths.is_empty() { div color=(color::MUTED) font-size=((typeface::LABEL)) { "No matching paths." } }
            @for path in paths.iter().filter_map(serde_json::Value::as_str).take(30) {
                div color=(color::STRONG) font-size=((typeface::LABEL)) font-family="monospace" white-space="preserve-wrap" border-top=((1, surface::BORDER)) padding-top=((space::XS)) margin-top=((space::XS)) { (path) }
            }
            @if paths.len() > 30 {
                div color=(color::MUTED) font-size=((typeface::LABEL)) margin-top=((space::SM)) { "… " ((paths.len() - 30).to_string()) " more paths" }
            }
            (filesystem_result_metadata(&artifact.artifact.metadata))
        }
    })
}

pub(super) fn render_filesystem_grep_result(artifact: &ToolArtifactView) -> Option<Containers> {
    render_grep_matches(artifact, "Search matches")
}

pub(super) fn render_filesystem_stat_result(artifact: &ToolArtifactView) -> Option<Containers> {
    let metadata = &artifact.artifact.metadata;
    let exists = metadata
        .get("exists")
        .and_then(serde_json::Value::as_bool)?;
    let kind = metadata.get("kind").and_then(serde_json::Value::as_str);
    let path = metadata.get("path").and_then(serde_json::Value::as_str);
    let len = metadata.get("len").and_then(serde_json::Value::as_u64);
    Some(container! {
        div border=((1, surface::BORDER)) border-radius=((radius::CONTROL)) background=(surface::INSET) padding=((space::S10)) margin-top=((space::SM)) {
            div color=(color::INFO) margin-bottom=((space::S6)) { (artifact.artifact.title.as_deref().unwrap_or("Path metadata")) }
            div color=(color::STRONG) { (if exists { "Path exists" } else { "Path does not exist" }) }
            @if let Some(path) = path { div color=(color::MUTED) font-size=((typeface::LABEL)) font-family="monospace" margin-top=((space::XS)) { (path) } }
            @if let Some(kind) = kind { div color=(color::MUTED) font-size=((typeface::LABEL)) margin-top=((space::XS)) { "kind: " (kind) } }
            @if let Some(len) = len { div color=(color::MUTED) font-size=((typeface::LABEL)) margin-top=((space::XS)) { "len: " (len.to_string()) } }
        }
    })
}

pub(super) fn render_filesystem_artifact_metadata(
    artifact: &ToolArtifactView,
) -> Option<Containers> {
    let metadata = &artifact.artifact.metadata;
    let path = metadata.get("path").and_then(serde_json::Value::as_str)?;
    let exists = metadata.get("exists").and_then(serde_json::Value::as_bool);
    let kind = metadata.get("kind").and_then(serde_json::Value::as_str);
    let byte_len = metadata.get("byte_len").and_then(serde_json::Value::as_u64);
    let content_type = metadata
        .get("content_type")
        .and_then(serde_json::Value::as_str);
    let complete = metadata
        .get("complete")
        .and_then(serde_json::Value::as_bool);
    let message = metadata.get("message").and_then(serde_json::Value::as_str);
    Some(container! {
        div border=((1, surface::BORDER)) border-radius=((radius::CONTROL)) background=(surface::INSET) padding=((space::S10)) margin-top=((space::SM)) {
            div color=(color::INFO) margin-bottom=((space::S6)) { (artifact.artifact.title.as_deref().unwrap_or("Artifact metadata")) }
            div color=(color::STRONG) font-family="monospace" white-space="preserve-wrap" { (path) }
            @if let Some(exists) = exists { div color=(color::MUTED) font-size=((typeface::LABEL)) margin-top=((space::XS)) { "exists: " (exists.to_string()) } }
            @if let Some(kind) = kind { div color=(color::MUTED) font-size=((typeface::LABEL)) margin-top=((space::XS)) { "kind: " (kind) } }
            @if let Some(byte_len) = byte_len { div color=(color::MUTED) font-size=((typeface::LABEL)) margin-top=((space::XS)) { "bytes: " (byte_len.to_string()) } }
            @if let Some(content_type) = content_type { div color=(color::MUTED) font-size=((typeface::LABEL)) margin-top=((space::XS)) { "type: " (content_type) } }
            @if let Some(complete) = complete { div color=(color::MUTED) font-size=((typeface::LABEL)) margin-top=((space::XS)) { "complete: " (complete.to_string()) } }
            @if let Some(message) = message { div color=(color::MUTED) font-size=((typeface::LABEL)) margin-top=((space::XS)) white-space="preserve-wrap" { (message) } }
        }
    })
}

pub(super) fn render_filesystem_artifact_read(artifact: &ToolArtifactView) -> Option<Containers> {
    let metadata = &artifact.artifact.metadata;
    let path = metadata.get("path").and_then(serde_json::Value::as_str)?;
    let contents = metadata.get("contents").and_then(serde_json::Value::as_str);
    let returned_bytes = metadata
        .get("returned_bytes")
        .and_then(serde_json::Value::as_u64);
    let total_bytes = metadata
        .get("total_bytes")
        .and_then(serde_json::Value::as_u64);
    let truncated = metadata
        .get("truncated")
        .and_then(serde_json::Value::as_bool);
    Some(container! {
        div border=((1, surface::BORDER)) border-radius=((radius::CONTROL)) background=(surface::INSET) padding=((space::S10)) margin-top=((space::SM)) {
            div color=(color::INFO) margin-bottom=((space::S6)) { (artifact.artifact.title.as_deref().unwrap_or("Artifact contents")) }
            div color=(color::STRONG) font-family="monospace" white-space="preserve-wrap" { (path) }
            @if let Some(returned_bytes) = returned_bytes { div color=(color::MUTED) font-size=((typeface::LABEL)) margin-top=((space::XS)) { "returned bytes: " (returned_bytes.to_string()) } }
            @if let Some(total_bytes) = total_bytes { div color=(color::MUTED) font-size=((typeface::LABEL)) margin-top=((space::XS)) { "total bytes: " (total_bytes.to_string()) } }
            @if let Some(truncated) = truncated { div color=(color::MUTED) font-size=((typeface::LABEL)) margin-top=((space::XS)) { "truncated: " (truncated.to_string()) } }
            @if let Some(contents) = contents { div color=(color::TEXT) font-size=((typeface::LABEL)) font-family="monospace" white-space="preserve-wrap" border-top=((1, surface::BORDER)) margin-top=((space::SM)) padding-top=((space::SM)) { (contents) } }
        }
    })
}

pub(super) fn render_filesystem_artifact_grep(artifact: &ToolArtifactView) -> Option<Containers> {
    render_grep_matches(artifact, "Artifact matches")
}

pub(super) fn render_grep_matches(
    artifact: &ToolArtifactView,
    fallback_title: &'static str,
) -> Option<Containers> {
    let metadata = &artifact.artifact.metadata;
    let matches = metadata
        .get("matches")
        .and_then(serde_json::Value::as_array)?;
    let path = metadata.get("path").and_then(serde_json::Value::as_str);
    Some(container! {
        div border=((1, surface::BORDER)) border-radius=((radius::CONTROL)) background=(surface::INSET) padding=((space::S10)) margin-top=((space::SM)) {
            div color=(color::INFO) margin-bottom=((space::S6)) { (format!("{} ({})", artifact.artifact.title.as_deref().unwrap_or(fallback_title), matches.len())) }
            @if matches.is_empty() { div color=(color::MUTED) font-size=((typeface::LABEL)) { "No text matches." } }
            @if let Some(path) = path { div color=(color::MUTED) font-size=((typeface::LABEL)) font-family="monospace" white-space="preserve-wrap" margin-bottom=((space::S6)) { (path) } }
            @for hit in matches.iter().take(30) {
                @if let Some(hit) = hit.as_object() {
                    @let path = hit.get("path").and_then(serde_json::Value::as_str).map(bounded_search_field);
                    @let line = hit.get("line").and_then(serde_json::Value::as_str).map(bounded_search_field);
                    div border-top=((1, surface::BORDER)) padding-top=((space::S6)) margin-top=((space::S6)) {
                        @if let Some((path, truncated)) = path { div color=(color::STRONG) font-size=((typeface::LABEL)) font-family="monospace" white-space="preserve-wrap" { (path) @if truncated { "…" } } }
                        @if let Some(line_number) = hit.get("line_number").and_then(serde_json::Value::as_u64) { span color=(color::MUTED) font-size=((typeface::LABEL)) { (line_number.to_string()) ": " } }
                        @if let Some((line, truncated)) = line { span color=(color::TEXT) font-size=((typeface::LABEL)) white-space="preserve-wrap" { (line) @if truncated { "…" } } }
                    }
                }
            }
            @if matches.len() > 30 {
                div color=(color::MUTED) font-size=((typeface::LABEL)) margin-top=((space::SM)) { "… " ((matches.len() - 30).to_string()) " more matches" }
            }
            (filesystem_result_metadata(metadata))
        }
    })
}

fn filesystem_result_metadata(metadata: &serde_json::Value) -> Containers {
    let backend = metadata.get("backend").and_then(serde_json::Value::as_str);
    let visited_entries = metadata
        .get("visited_entries")
        .and_then(serde_json::Value::as_u64);
    let partial = metadata.get("partial").and_then(serde_json::Value::as_bool);
    let timed_out = metadata
        .get("timed_out")
        .and_then(serde_json::Value::as_bool);
    let message = metadata.get("message").and_then(serde_json::Value::as_str);
    container! {
        @if backend.is_some() || visited_entries.is_some() || partial.is_some() || timed_out.is_some() || message.is_some() {
            div color=(color::MUTED) font-size=((typeface::LABEL)) margin-top=((space::SM)) {
                @if let Some(backend) = backend { div { "backend: " (backend) } }
                @if let Some(visited_entries) = visited_entries { div { "visited entries: " (visited_entries.to_string()) } }
                @if let Some(partial) = partial { div { "partial: " (partial.to_string()) } }
                @if let Some(timed_out) = timed_out { div { "timed out: " (timed_out.to_string()) } }
                @if let Some(message) = message { div white-space="preserve-wrap" { (message) } }
            }
        }
    }
}

pub(super) fn render_git_clone_result(artifact: &ToolArtifactView) -> Option<Containers> {
    let metadata = &artifact.artifact.metadata;
    let repo = metadata.get("repo").and_then(serde_json::Value::as_str)?;
    let owner = metadata.get("owner").and_then(serde_json::Value::as_str);
    let host = metadata.get("host").and_then(serde_json::Value::as_str);
    let clone_url = metadata
        .get("clone_url")
        .and_then(serde_json::Value::as_str);
    let path = metadata.get("path").and_then(serde_json::Value::as_str);
    let git_ref = metadata
        .get("git_ref")
        .or_else(|| metadata.get("ref"))
        .and_then(serde_json::Value::as_str);
    let already_exists = metadata
        .get("already_exists")
        .and_then(serde_json::Value::as_bool);
    let repo_label = owner.map_or_else(|| repo.to_owned(), |owner| format!("{owner}/{repo}"));
    Some(container! {
        div border=((1, surface::BORDER)) border-radius=((radius::CONTROL)) background=(surface::INSET) padding=((space::S10)) margin-top=((space::SM)) {
            div direction=row gap=((space::SM)) align-items=center margin-bottom=((space::S6)) {
                span color=(color::INFO) { (artifact.artifact.title.as_deref().unwrap_or("Repository clone")) }
                @if let Some(host) = host { span color=(color::MUTED) { (host) } }
            }
            div color=(color::STRONG) font-family="monospace" white-space="preserve-wrap" { (repo_label) }
            div color=(if already_exists == Some(true) { color::WARNING } else { color::SUCCESS }) font-size=((typeface::LABEL)) margin-top=((space::XS)) {
                (if already_exists == Some(true) { "repository already existed" } else { "repository cloned" })
            }
            @if let Some(path) = path { div color=(color::MUTED) font-size=((typeface::LABEL)) margin-top=((space::XS)) font-family="monospace" white-space="preserve-wrap" { "path: " (path) } }
            @if let Some(git_ref) = git_ref { div color=(color::MUTED) font-size=((typeface::LABEL)) margin-top=((space::XS)) { "ref: " (git_ref) } }
            @if let Some(clone_url) = clone_url { div color=(color::MUTED) font-size=((typeface::LABEL)) margin-top=((space::XS)) font-family="monospace" white-space="preserve-wrap" { "remote: " (clone_url) } }
            @if let Some(already_exists) = already_exists { div color=(color::MUTED) font-size=((typeface::LABEL)) margin-top=((space::XS)) { "already exists: " (already_exists.to_string()) } }
        }
    })
}

pub(super) fn render_ocr_extract_result(artifact: &ToolArtifactView) -> Option<Containers> {
    let metadata = &artifact.artifact.metadata;
    let text = metadata.get("text").and_then(serde_json::Value::as_str)?;
    let source = metadata
        .get("source")
        .and_then(serde_json::Value::as_object);
    let path = source
        .and_then(|source| source.get("path"))
        .and_then(serde_json::Value::as_str);
    let url = source
        .and_then(|source| source.get("url"))
        .and_then(serde_json::Value::as_str);
    let engine = metadata.get("engine").and_then(serde_json::Value::as_str);
    let language = metadata.get("language").and_then(serde_json::Value::as_str);
    let truncated = metadata
        .get("truncated")
        .and_then(serde_json::Value::as_bool);
    let text_bytes = metadata
        .get("text_bytes")
        .and_then(serde_json::Value::as_u64);
    Some(container! {
        div border=((1, surface::BORDER)) border-radius=((radius::CONTROL)) background=(surface::INSET) padding=((space::S10)) margin-top=((space::SM)) {
            div color=(color::INFO) margin-bottom=((space::S6)) { (artifact.artifact.title.as_deref().unwrap_or("OCR extraction")) }
            @if let Some(path) = path { div color=(color::STRONG) font-family="monospace" white-space="preserve-wrap" { (path) } }
            @if let Some(url) = url { div color=(color::STRONG) font-family="monospace" white-space="preserve-wrap" { (url) } }
            @if let Some(engine) = engine { div color=(color::MUTED) font-size=((typeface::LABEL)) margin-top=((space::XS)) { "engine: " (engine) } }
            @if let Some(language) = language { div color=(color::MUTED) font-size=((typeface::LABEL)) margin-top=((space::XS)) { "language: " (language) } }
            @if let Some(text_bytes) = text_bytes { div color=(color::MUTED) font-size=((typeface::LABEL)) margin-top=((space::XS)) { "text bytes: " (text_bytes.to_string()) } }
            (extracted_text_panel(text, truncated))
            (artifact_references(artifact))
        }
    })
}

pub(super) fn render_ocr_status(artifact: &ToolArtifactView) -> Option<Containers> {
    render_extract_capabilities(artifact, "OCR engines", "engines")
}

pub(super) fn render_extract_capabilities(
    artifact: &ToolArtifactView,
    title: &'static str,
    entries_key: &'static str,
) -> Option<Containers> {
    let extract = artifact
        .artifact
        .metadata
        .get("extract")
        .and_then(serde_json::Value::as_object)?;
    let available = extract
        .get("available")
        .and_then(serde_json::Value::as_bool);
    let entries = extract
        .get(entries_key)
        .and_then(serde_json::Value::as_array);
    Some(container! {
        div border=((1, surface::BORDER)) border-radius=((radius::CONTROL)) background=(surface::INSET) padding=((space::S10)) margin-top=((space::SM)) {
            div color=(color::INFO) margin-bottom=((space::S6)) { (artifact.artifact.title.as_deref().unwrap_or(title)) }
            @if let Some(available) = available { div color=(color::MUTED) font-size=((typeface::LABEL)) margin-bottom=((space::XS)) { "available: " (available.to_string()) } }
            @if let Some(entries) = entries {
                @for entry in entries {
                    @if let Some(entry) = entry.as_object() {
                        div border-top=((1, surface::BORDER)) padding-top=((space::S6)) margin-top=((space::S6)) {
                            @if let Some(name) = entry.get("name").and_then(serde_json::Value::as_str) { span color=(color::STRONG) { (name) } }
                            @if let Some(quality) = entry.get("quality").and_then(serde_json::Value::as_str) { span color=(color::MUTED) { " · " (quality) } }
                            @if let Some(available) = entry.get("available").and_then(serde_json::Value::as_bool) { span color=(color::MUTED) { " · available: " (available.to_string()) } }
                        }
                    }
                }
            }
        }
    })
}

const MAX_SEARCH_FIELD_CHARS: usize = 2_000;

fn bounded_search_field(text: &str) -> (Cow<'_, str>, bool) {
    let Some((byte_index, _)) = text.char_indices().nth(MAX_SEARCH_FIELD_CHARS) else {
        return (Cow::Borrowed(text), false);
    };
    (Cow::Owned(text[..byte_index].to_owned()), true)
}

const MAX_SHELL_OUTPUT_CHARS: usize = 32_000;

fn shell_output_panel(label: &str, output: &str, source_truncated: bool) -> Containers {
    let (output, display_truncated) = output
        .char_indices()
        .nth(MAX_SHELL_OUTPUT_CHARS)
        .map_or_else(
            || (output, false),
            |(byte_index, _)| (&output[..byte_index], true),
        );
    container! {
        details open=true margin-top=((space::SM)) {
            summary color=(color::MUTED) font-size=((typeface::DETAIL)) { (label) }
            div color=(color::TEXT) font-family="monospace" white-space="preserve-wrap" background=(surface::INSET) border=((1, surface::BORDER)) border-radius=((radius::CONTROL)) padding=((space::SM)) margin-top=((space::S6)) { (output) }
            @if source_truncated { div color=(color::WARNING) font-size=((typeface::DETAIL)) margin-top=((space::S6)) { "Shell output was truncated by the producer." } }
            @if display_truncated { div color=(color::WARNING) font-size=((typeface::DETAIL)) margin-top=((space::S6)) { "Shell output truncated for display." } }
        }
    }
}

pub(super) fn render_shell_result(artifact: &ToolArtifactView) -> Option<Containers> {
    let metadata = artifact.artifact.metadata.as_object()?;
    let mode = metadata.get("mode").and_then(serde_json::Value::as_str)?;
    let exit_code = metadata
        .get("exit_code")
        .and_then(serde_json::Value::as_i64);
    let timed_out = metadata
        .get("timed_out")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    let cancelled = metadata
        .get("cancelled")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    let duration_ms = metadata
        .get("duration_ms")
        .and_then(serde_json::Value::as_u64);
    let failed = timed_out || cancelled || exit_code.is_some_and(|code| code != 0);
    Some(container! {
        div border=((1, surface::BORDER)) border-radius=((radius::CONTROL)) background=(if failed { surface::ERROR_INSET } else { surface::INSET }) padding=((space::S10)) margin-top=((space::SM)) {
            div justify-content=space-between gap=((space::SM)) {
                div color=(color::INFO) { (artifact.artifact.title.as_deref().unwrap_or("Shell result")) }
                div color=(if failed { color::ERROR } else { color::SUCCESS }) font-size=((typeface::LABEL)) {
                    @if timed_out { "timed out" } @else if cancelled { "cancelled" } @else if let Some(exit_code) = exit_code { "exit " (exit_code.to_string()) } @else { "completed" }
                }
            }
            div color=(color::MUTED) font-size=((typeface::DETAIL)) margin-top=((space::XS)) {
                (mode)
                @if let Some(duration_ms) = duration_ms { " · " (duration_ms.to_string()) " ms" }
                @if let (Some(columns), Some(rows)) = (metadata.get("columns").and_then(serde_json::Value::as_u64), metadata.get("rows").and_then(serde_json::Value::as_u64)) { " · " (columns.to_string()) "x" (rows.to_string()) }
            }
            @if mode == "terminal" {
                @if let Some(output) = metadata.get("output_tail").and_then(serde_json::Value::as_str) {
                    (shell_output_panel("terminal output", output, metadata.get("output_truncated").and_then(serde_json::Value::as_bool).unwrap_or(false)))
                }
                @if let Some(output_bytes) = metadata.get("output_bytes").and_then(serde_json::Value::as_u64) { div color=(color::MUTED) font-size=((typeface::DETAIL)) margin-top=((space::S6)) { (output_bytes.to_string()) " output bytes" } }
            } @else if mode == "captured" {
                @if let Some(stdout) = metadata.get("stdout").and_then(serde_json::Value::as_str) { (shell_output_panel("stdout", stdout, metadata.get("stdout_truncated").and_then(serde_json::Value::as_bool).unwrap_or(false))) }
                @if let Some(stderr) = metadata.get("stderr").and_then(serde_json::Value::as_str) { (shell_output_panel("stderr", stderr, metadata.get("stderr_truncated").and_then(serde_json::Value::as_bool).unwrap_or(false))) }
            }
            (artifact_references(artifact))
        }
    })
}

pub(super) fn render_web_search_results(artifact: &ToolArtifactView) -> Option<Containers> {
    let metadata = &artifact.artifact.metadata;
    let query = metadata.get("query").and_then(serde_json::Value::as_str);
    let provider = metadata.get("provider").and_then(serde_json::Value::as_str);
    let partial = metadata.get("partial").and_then(serde_json::Value::as_bool);
    let message = metadata.get("message").and_then(serde_json::Value::as_str);
    let results = metadata.get("results")?.as_array()?;
    Some(container! {
        div border=((1, surface::BORDER)) border-radius=((radius::CONTROL)) background=(surface::INSET) padding=((space::S10)) margin-top=((space::SM)) {
            div direction=row gap=((space::SM)) align-items=center margin-bottom=((space::S6)) {
                span color=(color::INFO) { (artifact.artifact.title.as_deref().unwrap_or("Search results")) }
                @if let Some(provider) = provider {
                    span color=(color::MUTED) { (provider) }
                }
            }
            @if let Some(query) = query {
                div color=(color::STRONG) font-family="monospace" white-space="preserve-wrap" margin-bottom=((space::SM)) { (query) }
            }
            @if results.is_empty() {
                div color=(color::MUTED) font-size=((typeface::LABEL)) margin-top=((space::SM)) { "No search results." }
            }
            @for (index, result) in results.iter().take(10).enumerate() {
                @if let Some(result) = result.as_object() {
                    @let title = result.get("title").and_then(serde_json::Value::as_str).map(|value| bounded_search_field(value));
                    @let url = result.get("url").and_then(serde_json::Value::as_str).map(|value| bounded_search_field(value));
                    @let snippet = result.get("snippet").and_then(serde_json::Value::as_str).map(|value| bounded_search_field(value));
                    div border-top=((1, surface::BORDER)) padding-top=((space::SM)) margin-top=((space::SM)) {
                        div color=(color::INFO) font-size=((typeface::LABEL)) margin-bottom=((space::S2)) { (format!("{}.", index + 1)) }
                        @if let Some((title, truncated)) = title {
                            div color=(color::STRONG) white-space="preserve-wrap" { (title) @if truncated { "…" } }
                        }
                        @if let Some((url, truncated)) = url {
                            div color=(color::MUTED) font-size=((typeface::LABEL)) font-family="monospace" white-space="preserve-wrap" margin-top=((space::S2)) { (url) @if truncated { "…" } }
                        }
                        @if let Some((snippet, truncated)) = snippet {
                            div color=(color::TEXT) font-size=((typeface::LABEL)) white-space="preserve-wrap" margin-top=((space::XS)) { (snippet) @if truncated { "…" } }
                        }
                    }
                }
            }
            @if results.len() > 10 {
                div color=(color::MUTED) font-size=((typeface::LABEL)) margin-top=((space::SM)) { "… " ((results.len() - 10).to_string()) " more results" }
            }
            @if partial == Some(true) {
                div color=(color::WARNING) font-size=((typeface::LABEL)) margin-top=((space::SM)) { "partial results" }
            }
            @if let Some(message) = message {
                div color=(color::MUTED) font-size=((typeface::LABEL)) margin-top=((space::XS)) white-space="preserve-wrap" { (message) }
            }
        }
    })
}

const MAX_FETCH_PREVIEW_CHARS: usize = 32_000;

fn bounded_fetch_preview(text: &str) -> (Cow<'_, str>, bool) {
    let Some((byte_index, _)) = text.char_indices().nth(MAX_FETCH_PREVIEW_CHARS) else {
        return (Cow::Borrowed(text), false);
    };
    (Cow::Owned(text[..byte_index].to_owned()), true)
}

pub(super) fn safe_web_url(url: &str) -> Option<&str> {
    let remainder = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))?;
    let authority = remainder.split(['/', '?', '#']).next().unwrap_or_default();
    (!authority.is_empty()
        && !url.chars().any(|character| {
            character.is_control() || character.is_whitespace() || character == '\\'
        }))
    .then_some(url)
}

fn fetched_content(preview: &str, content_format: Option<&str>) -> Containers {
    let (preview, display_truncated) = bounded_fetch_preview(preview);
    let content = if content_format == Some("markdown") {
        vec![hyperchad::markdown::markdown_to_container(&preview)]
    } else {
        container! {
            div color=(color::TEXT) font-size=((typeface::LABEL)) white-space="preserve-wrap" { (preview) }
        }
    };
    container! {
        div border-top=((1, surface::BORDER)) margin-top=((space::SM)) padding-top=((space::SM)) {
            (content)
            @if display_truncated {
                div color=(color::WARNING) font-size=((typeface::DETAIL)) margin-top=((space::SM)) { "Fetched content truncated for display." }
            }
        }
    }
}

pub(super) fn render_web_fetch_result(artifact: &ToolArtifactView) -> Option<Containers> {
    let metadata = &artifact.artifact.metadata;
    let original_url = metadata.get("url").and_then(serde_json::Value::as_str);
    let final_url = metadata
        .get("final_url")
        .and_then(serde_json::Value::as_str);
    let url = final_url.or(original_url)?;
    let title = metadata.get("title").and_then(serde_json::Value::as_str);
    let status = metadata.get("status").and_then(serde_json::Value::as_u64);
    let content_type = metadata
        .get("content_type")
        .and_then(serde_json::Value::as_str);
    let content_format = metadata
        .get("content_format")
        .and_then(serde_json::Value::as_str);
    let rendered = metadata
        .get("rendered")
        .and_then(serde_json::Value::as_bool);
    let truncated = metadata
        .get("truncated")
        .and_then(serde_json::Value::as_bool);
    let preview = metadata
        .get("markdown")
        .or_else(|| metadata.get("text"))
        .and_then(serde_json::Value::as_str);
    Some(container! {
        div border=((1, surface::BORDER)) border-radius=((radius::CONTROL)) background=(surface::INSET) padding=((space::S10)) margin-top=((space::SM)) {
            div color=(color::INFO) margin-bottom=((space::S6)) { (artifact.artifact.title.as_deref().unwrap_or("Fetched page")) }
            @if let Some(title) = title {
                div color=(color::STRONG) white-space="preserve-wrap" margin-bottom=((space::XS)) { (title) }
            }
            @if let Some(safe_url) = safe_web_url(url) {
                anchor href=(safe_url) color=(color::INFO) font-size=((typeface::LABEL)) font-family="monospace" text-decoration="none" { (url) }
            } @else {
                div color=(color::MUTED) font-size=((typeface::LABEL)) font-family="monospace" white-space="preserve-wrap" { (url) }
            }
            @if let (Some(original_url), Some(final_url)) = (original_url, final_url) {
                @if original_url != final_url {
                    div color=(color::MUTED) font-size=((typeface::DETAIL)) margin-top=((space::S3)) white-space="preserve-wrap" { "source: " (original_url) }
                }
            }
            @if let Some(status) = status {
                div color=(color::MUTED) font-size=((typeface::LABEL)) margin-top=((space::XS)) { "status: " (status.to_string()) }
            }
            @if let Some(content_type) = content_type {
                div color=(color::MUTED) font-size=((typeface::LABEL)) margin-top=((space::XS)) { "type: " (content_type) }
            }
            @if let Some(content_format) = content_format {
                div color=(color::MUTED) font-size=((typeface::LABEL)) margin-top=((space::XS)) { "format: " (content_format) }
            }
            @if let Some(rendered) = rendered {
                div color=(color::MUTED) font-size=((typeface::LABEL)) margin-top=((space::XS)) { "rendered: " (rendered.to_string()) }
            }
            @if truncated == Some(true) {
                div color=(color::WARNING) font-size=((typeface::LABEL)) margin-top=((space::XS)) { "Source content was truncated." }
            }
            @if let Some(preview) = preview {
                (fetched_content(preview, content_format))
            } @else {
                div color=(color::MUTED) font-size=((typeface::LABEL)) margin-top=((space::SM)) { "No extracted content was returned." }
            }
        }
    })
}

pub(super) fn render_question_outcome(artifact: &ToolArtifactView) -> Option<Containers> {
    let metadata = &artifact.artifact.metadata;
    let status = metadata.get("status").and_then(serde_json::Value::as_str)?;
    let questions = metadata
        .get("questions")
        .and_then(serde_json::Value::as_array)?;
    Some(container! {
        div border=((1, surface::BORDER)) border-radius=((radius::CONTROL)) background=(surface::INSET) padding=((space::S10)) margin-top=((space::SM)) {
            div justify-content=space-between gap=((space::SM)) {
                div color=(color::INFO) { (artifact.artifact.title.as_deref().unwrap_or("Question outcome")) }
                div color=(if status == "answered" { color::SUCCESS } else { color::WARNING }) font-size=((typeface::LABEL)) { (status) }
            }
            @if questions.is_empty() { div color=(color::MUTED) font-size=((typeface::LABEL)) margin-top=((space::S6)) { "No questions were returned." } }
            @for question in questions.iter().take(20) {
                @if let Some(question) = question.as_object() {
                    div border-top=((1, surface::BORDER)) padding-top=((space::S6)) margin-top=((space::S6)) {
                        @if let Some(header) = question.get("header").and_then(serde_json::Value::as_str) { div color=(color::STRONG) font-weight=bold { (header) } }
                        @if let Some(prompt) = question.get("question").and_then(serde_json::Value::as_str) { div color=(color::TEXT) font-size=((typeface::LABEL)) margin-top=((space::S2)) white-space="preserve-wrap" { (prompt) } }
                        @if let Some(question_status) = question.get("status").and_then(serde_json::Value::as_str) { div color=(color::MUTED) font-size=((typeface::DETAIL)) margin-top=((space::S3)) { (question_status) @if question.get("required").and_then(serde_json::Value::as_bool) == Some(true) { " · required" } } }
                        @for answer in question.get("selected").and_then(serde_json::Value::as_array).into_iter().flatten().take(20) {
                            @if let Some(label) = answer.get("label").and_then(serde_json::Value::as_str) { div color=(color::SUCCESS) font-size=((typeface::LABEL)) margin-top=((space::S3)) { "✓ " (label) } }
                        }
                        @if let Some(custom) = question.get("custom").and_then(serde_json::Value::as_str) { div color=(color::SUCCESS) font-size=((typeface::LABEL)) margin-top=((space::S3)) white-space="preserve-wrap" { "answer: " (custom) } }
                    }
                }
            }
            @if questions.len() > 20 { div color=(color::WARNING) font-size=((typeface::DETAIL)) margin-top=((space::S6)) { "Question outcomes truncated for display." } }
        }
    })
}

pub(super) fn render_web_status(artifact: &ToolArtifactView) -> Option<Containers> {
    let metadata = &artifact.artifact.metadata;
    let search = metadata
        .get("search")
        .and_then(serde_json::Value::as_object);
    let fetch = metadata.get("fetch").and_then(serde_json::Value::as_object);
    if search.is_none() && fetch.is_none() {
        return None;
    }
    Some(container! {
        div border=((1, surface::BORDER)) border-radius=((radius::CONTROL)) background=(surface::INSET) padding=((space::S10)) margin-top=((space::SM)) {
            div color=(color::INFO) { "Web capabilities" }
            @if let Some(search) = search {
                div border-top=((1, surface::BORDER)) padding-top=((space::S6)) margin-top=((space::S6)) {
                    div color=(color::STRONG) { "Search" }
                    @if let Some(available) = search.get("available").and_then(serde_json::Value::as_bool) { div color=(color::MUTED) font-size=((typeface::LABEL)) { "available: " (available.to_string()) } }
                    @if let Some(provider) = search.get("provider").and_then(serde_json::Value::as_str) { div color=(color::MUTED) font-size=((typeface::LABEL)) { "provider: " (provider) } }
                    @if let Some(quality) = search.get("quality").and_then(serde_json::Value::as_str) { div color=(color::MUTED) font-size=((typeface::LABEL)) { "quality: " (quality) } }
                }
            }
            @if let Some(fetch) = fetch {
                div border-top=((1, surface::BORDER)) padding-top=((space::S6)) margin-top=((space::S6)) {
                    div color=(color::STRONG) { "Fetch" }
                    @if let Some(available) = fetch.get("available").and_then(serde_json::Value::as_bool) { div color=(color::MUTED) font-size=((typeface::LABEL)) { "available: " (available.to_string()) } }
                    @if let Some(rendered) = fetch.get("rendered_fetch").and_then(serde_json::Value::as_bool) { div color=(color::MUTED) font-size=((typeface::LABEL)) { "rendered fetch: " (rendered.to_string()) } }
                    @if let Some(max_bytes) = fetch.get("max_bytes").and_then(serde_json::Value::as_u64) { div color=(color::MUTED) font-size=((typeface::LABEL)) { "max bytes: " (max_bytes.to_string()) } }
                }
            }
        }
    })
}

pub(super) fn render_web_inspect_result(artifact: &ToolArtifactView) -> Option<Containers> {
    let metadata = &artifact.artifact.metadata;
    let url = metadata.get("url").and_then(serde_json::Value::as_str)?;
    let kind = metadata.get("kind").and_then(serde_json::Value::as_str);
    let tool = metadata
        .get("recommended_tool")
        .and_then(serde_json::Value::as_str);
    let action = metadata
        .get("recommended_action")
        .and_then(serde_json::Value::as_str);
    let notes = metadata.get("notes").and_then(serde_json::Value::as_array);
    Some(container! {
        div border=((1, surface::BORDER)) border-radius=((radius::CONTROL)) background=(surface::INSET) padding=((space::S10)) margin-top=((space::SM)) {
            div color=(color::INFO) { "URL inspection" }
            div color=(color::STRONG) font-family="monospace" margin-top=((space::XS)) white-space="preserve-wrap" { (url) }
            @if let Some(kind) = kind { div color=(color::MUTED) font-size=((typeface::LABEL)) margin-top=((space::S3)) { "kind: " (kind) } }
            @if let Some(tool) = tool { div color=(color::MUTED) font-size=((typeface::LABEL)) margin-top=((space::S3)) { "recommended tool: " (tool) } }
            @if let Some(action) = action { div color=(color::TEXT) font-size=((typeface::LABEL)) margin-top=((space::S3)) white-space="preserve-wrap" { (action) } }
            @for note in notes.into_iter().flatten().filter_map(serde_json::Value::as_str).take(20) { div color=(color::MUTED) font-size=((typeface::LABEL)) margin-top=((space::S3)) white-space="preserve-wrap" { "• " (note) } }
        }
    })
}

pub(super) fn render_worktree_list_result(artifact: &ToolArtifactView) -> Option<Containers> {
    let metadata = &artifact.artifact.metadata;
    let repo_root = metadata
        .get("repo_root")
        .and_then(serde_json::Value::as_str);
    let current_worktree = metadata
        .get("current_worktree")
        .and_then(serde_json::Value::as_str);
    let worktrees = metadata
        .get("worktrees")
        .or_else(|| metadata.get("entries"))
        .and_then(serde_json::Value::as_array)?;
    Some(container! {
        div border=((1, surface::BORDER)) border-radius=((radius::CONTROL)) background=(surface::INSET) padding=((space::S10)) margin-top=((space::SM)) {
            div color=(color::INFO) margin-bottom=((space::S6)) { (format!("{} ({})", artifact.artifact.title.as_deref().unwrap_or("Worktrees"), worktrees.len())) }
            @if let Some(repo_root) = repo_root { div color=(color::MUTED) font-size=((typeface::DETAIL)) font-family="monospace" { "repository: " (repo_root) } }
            @if let Some(current_worktree) = current_worktree { div color=(color::MUTED) font-size=((typeface::DETAIL)) font-family="monospace" margin-top=((space::S3)) { "current: " (current_worktree) } }
            @if worktrees.is_empty() {
                div color=(color::MUTED) font-size=((typeface::LABEL)) { "No worktrees found." }
            }
            @for worktree in worktrees.iter().take(20) {
                @if let Some(worktree) = worktree.as_object() {
                    div border-top=((1, surface::BORDER)) padding-top=((space::S6)) margin-top=((space::S6)) {
                        @if let Some(path) = worktree.get("path").and_then(serde_json::Value::as_str) {
                            div color=(color::STRONG) font-family="monospace" white-space="preserve-wrap" { (path) }
                        }
                        @if let Some(branch) = worktree.get("branch").and_then(serde_json::Value::as_str) {
                            div color=(color::MUTED) font-size=((typeface::LABEL)) margin-top=((space::S2)) { "branch: " (branch) }
                        }
                        @if let Some(commit) = worktree.get("commit").and_then(serde_json::Value::as_str) {
                            div color=(color::MUTED) font-size=((typeface::LABEL)) margin-top=((space::S2)) { "commit: " (commit) }
                        }
                        @if let Some(is_main) = worktree.get("is_main").and_then(serde_json::Value::as_bool) {
                            div color=(color::MUTED) font-size=((typeface::LABEL)) margin-top=((space::S2)) { "main: " (is_main.to_string()) }
                        }
                    }
                }
            }
            @if worktrees.len() > 20 {
                div color=(color::MUTED) font-size=((typeface::LABEL)) margin-top=((space::SM)) { "… " ((worktrees.len() - 20).to_string()) " more worktrees" }
            }
        }
    })
}

pub(super) fn render_worktree_create_result(artifact: &ToolArtifactView) -> Option<Containers> {
    let metadata = &artifact.artifact.metadata;
    let path = metadata.get("path").and_then(serde_json::Value::as_str)?;
    let repo_root = metadata
        .get("repo_root")
        .and_then(serde_json::Value::as_str);
    let branch = metadata.get("branch").and_then(serde_json::Value::as_str);
    let created_branch = metadata
        .get("created_branch")
        .and_then(serde_json::Value::as_bool);
    let setup_applied = metadata
        .get("setup_applied")
        .and_then(serde_json::Value::as_bool);
    let session = metadata
        .get("session")
        .and_then(serde_json::Value::as_object);
    Some(container! {
        div border=((1, surface::BORDER)) border-radius=((radius::CONTROL)) background=(surface::INSET) padding=((space::S10)) margin-top=((space::SM)) {
            div color=(color::INFO) margin-bottom=((space::S6)) { (artifact.artifact.title.as_deref().unwrap_or("Worktree created")) }
            div color=(color::STRONG) font-family="monospace" white-space="preserve-wrap" { (path) }
            div color=(color::SUCCESS) font-size=((typeface::LABEL)) margin-top=((space::XS)) { "worktree created" }
            @if let Some(repo_root) = repo_root { div color=(color::MUTED) font-size=((typeface::LABEL)) margin-top=((space::XS)) font-family="monospace" white-space="preserve-wrap" { "repo: " (repo_root) } }
            @if let Some(branch) = branch { div color=(color::MUTED) font-size=((typeface::LABEL)) margin-top=((space::XS)) { "branch: " (branch) } }
            @if let Some(created_branch) = created_branch { div color=(color::MUTED) font-size=((typeface::LABEL)) margin-top=((space::XS)) { "created branch: " (created_branch.to_string()) } }
            @if let Some(setup_applied) = setup_applied { div color=(color::MUTED) font-size=((typeface::LABEL)) margin-top=((space::XS)) { "setup applied: " (setup_applied.to_string()) } }
            @if let Some(session) = session {
                div color=(color::MUTED) font-size=((typeface::LABEL)) margin-top=((space::XS)) {
                    "session: "
                    (session.get("name").and_then(serde_json::Value::as_str).or_else(|| session.get("id").and_then(serde_json::Value::as_str)).unwrap_or("created"))
                }
            }
        }
    })
}

pub(super) fn render_worktree_remove_result(artifact: &ToolArtifactView) -> Option<Containers> {
    let path = artifact
        .artifact
        .metadata
        .get("path")
        .and_then(serde_json::Value::as_str)?;
    Some(container! {
        div border=((1, surface::BORDER)) border-radius=((radius::CONTROL)) background=(surface::INSET) padding=((space::S10)) margin-top=((space::SM)) {
            div color=(color::INFO) margin-bottom=((space::S6)) { (artifact.artifact.title.as_deref().unwrap_or("Worktree removed")) }
            div color=(color::STRONG) font-family="monospace" white-space="preserve-wrap" { (path) }
            div color=(color::SUCCESS) font-size=((typeface::LABEL)) margin-top=((space::XS)) { "worktree removed" }
        }
    })
}

pub(super) fn render_plugin_visual(title: &str, visual: &PluginVisualView) -> Containers {
    let rich = VISUAL_ADAPTERS
        .get(&(
            visual.descriptor.schema.as_str(),
            visual.descriptor.schema_version,
        ))
        .and_then(|adapter| adapter(visual));
    container! {
        @if let Some(rich) = rich {
            (rich)
        }
        (json_panel(title, &visual.generic_payload))
    }
}

pub(super) fn render_extraction_request(visual: &PluginVisualView) -> Option<Containers> {
    let payload = visual
        .descriptor
        .payload
        .get("arguments")
        .unwrap_or(&visual.descriptor.payload);
    let operation = payload
        .get("operation")
        .and_then(serde_json::Value::as_str)?;
    let source = payload
        .get("path")
        .or_else(|| payload.get("url"))
        .and_then(serde_json::Value::as_str);
    Some(container! {
        div border=((1, surface::BORDER)) border-radius=((radius::CONTROL)) background=(surface::INSET) padding=((space::S10)) margin-top=((space::SM)) {
            div color=(color::INFO) margin-bottom=((space::S6)) { (visual.descriptor.title.as_deref().unwrap_or(operation)) }
            @if let Some(source) = source { div color=(color::STRONG) font-family="monospace" white-space="preserve-wrap" { (source) } }
            @if source.is_none() { div color=(color::MUTED) font-size=((typeface::LABEL)) { "Check configured extraction capabilities." } }
            @for key in ["engine", "language"] {
                @if let Some(value) = payload.get(key).and_then(serde_json::Value::as_str) { div color=(color::MUTED) font-size=((typeface::LABEL)) margin-top=((space::XS)) { (key) ": " (value) } }
            }
            @for key in ["max_bytes", "timeout_ms"] {
                @if let Some(value) = payload.get(key).and_then(serde_json::Value::as_u64) { div color=(color::MUTED) font-size=((typeface::LABEL)) margin-top=((space::XS)) { (key.replace('_', " ")) ": " (value.to_string()) } }
            }
        }
    })
}

pub(super) fn render_web_utility_request(visual: &PluginVisualView) -> Option<Containers> {
    let payload = visual
        .descriptor
        .payload
        .get("arguments")
        .unwrap_or(&visual.descriptor.payload);
    let operation = payload
        .get("operation")
        .and_then(serde_json::Value::as_str)?;
    let url = payload.get("url").and_then(serde_json::Value::as_str);
    Some(container! {
        div border=((1, surface::BORDER)) border-radius=((radius::CONTROL)) background=(surface::INSET) padding=((space::S10)) margin-top=((space::SM)) {
            div color=(color::INFO) margin-bottom=((space::S6)) {
                (if operation.contains("inspect") { "Inspect URL" } else { "Web capabilities" })
            }
            @if let Some(url) = url { div color=(color::STRONG) font-family="monospace" white-space="preserve-wrap" { (url) } }
            @if url.is_none() { div color=(color::MUTED) font-size=((typeface::LABEL)) { "Check available search and fetch providers." } }
        }
    })
}

pub(super) fn render_filesystem_request(visual: &PluginVisualView) -> Option<Containers> {
    let payload = visual
        .descriptor
        .payload
        .get("arguments")
        .unwrap_or(&visual.descriptor.payload);
    let operation = payload
        .get("operation")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("filesystem");
    let path = payload.get("path").and_then(serde_json::Value::as_str)?;
    Some(container! {
        div border=((1, surface::BORDER)) border-radius=((radius::CONTROL)) background=(surface::INSET) padding=((space::S10)) margin-top=((space::SM)) {
            div direction=row gap=((space::SM)) align-items=center margin-bottom=((space::S6)) {
                span color=(color::INFO) { (operation) }
                span color=(color::MUTED) { "filesystem" }
            }
            div color=(color::STRONG) font-family="monospace" white-space="preserve-wrap" { (path) }
            @for key in ["pattern", "query", "url", "region", "glob", "content_type"] {
                @if let Some(value) = payload.get(key).and_then(serde_json::Value::as_str) {
                    div color=(color::MUTED) font-size=((typeface::LABEL)) margin-top=((space::XS)) { (key.replace('_', " ")) ": " (value) }
                }
            }
            @for key in ["offset", "limit", "max_entries", "max_results", "max_matches", "timeout_ms", "offset_bytes", "max_bytes"] {
                @if let Some(value) = payload.get(key).and_then(serde_json::Value::as_u64) {
                    div color=(color::MUTED) font-size=((typeface::LABEL)) margin-top=((space::XS)) { (key.replace('_', " ")) ": " (value.to_string()) }
                }
            }
            @for key in ["recursive", "ignore_case", "from_end"] {
                @if let Some(value) = payload.get(key).and_then(serde_json::Value::as_bool) {
                    div color=(color::MUTED) font-size=((typeface::LABEL)) margin-top=((space::XS)) { (key.replace('_', " ")) ": " (value.to_string()) }
                }
            }
        }
    })
}

pub(super) fn render_filesystem_change(visual: &PluginVisualView) -> Option<Containers> {
    let payload = visual
        .descriptor
        .payload
        .get("arguments")
        .unwrap_or(&visual.descriptor.payload);
    let path = payload.get("path").and_then(serde_json::Value::as_str)?;
    let old_text = payload.get("old_text").and_then(serde_json::Value::as_str);
    let new_text = payload.get("new_text").and_then(serde_json::Value::as_str);
    Some(container! {
        div border=((1, surface::BORDER)) border-radius=((radius::CONTROL)) background=(surface::INSET) padding=((space::S10)) margin-top=((space::SM)) {
            div color=(color::INFO) margin-bottom=((space::S6)) { (visual.descriptor.title.as_deref().unwrap_or("Filesystem change")) }
            div color=(color::STRONG) font-family="monospace" white-space="preserve-wrap" { (path) }
            @if let Some(old_text) = old_text {
                div color=(color::ERROR) font-family="monospace" white-space="preserve-wrap" border-top=((1, surface::BORDER)) margin-top=((space::SM)) padding-top=((space::SM)) { "- " (old_text) }
            }
            @if let Some(new_text) = new_text {
                div color=(color::SUCCESS) font-family="monospace" white-space="preserve-wrap" border-top=((1, surface::BORDER)) margin-top=((space::SM)) padding-top=((space::SM)) { "+ " (new_text) }
            }
        }
    })
}

const MAX_VIM_DIFF_CHARS: usize = 32_000;

fn vim_diff_panel(diff: &str, source_truncated: bool) -> Containers {
    let (diff, display_truncated) = diff.char_indices().nth(MAX_VIM_DIFF_CHARS).map_or_else(
        || (diff, false),
        |(byte_index, _)| (&diff[..byte_index], true),
    );
    container! {
        details open=true margin-top=((space::SM)) {
            summary color=(color::INFO) font-size=((typeface::DETAIL)) { "diff" }
            div color=(color::TEXT) font-family="monospace" white-space="preserve-wrap" background=(surface::INSET) border=((1, surface::BORDER)) border-radius=((radius::CONTROL)) padding=((space::SM)) margin-top=((space::S6)) { (diff) }
            @if source_truncated { div color=(color::WARNING) font-size=((typeface::DETAIL)) margin-top=((space::S6)) { "Diff was truncated by the producer." } }
            @if display_truncated { div color=(color::WARNING) font-size=((typeface::DETAIL)) margin-top=((space::S6)) { "Diff truncated for display." } }
        }
    }
}

pub(super) fn render_vim_edit_live(visual: &PluginVisualView) -> Option<Containers> {
    let payload = &visual.descriptor.payload;
    let phase = payload.get("phase").and_then(serde_json::Value::as_str)?;
    let path = payload.get("path").and_then(serde_json::Value::as_str);
    let message = payload.get("message").and_then(serde_json::Value::as_str);
    let error = payload.get("error").and_then(serde_json::Value::as_str);
    let changed = payload.get("changed").and_then(serde_json::Value::as_bool);
    let cursor = payload.get("cursor").and_then(serde_json::Value::as_object);
    Some(container! {
        div border=((1, surface::BORDER)) border-radius=((radius::CONTROL)) background=(surface::INSET) padding=((space::S10)) margin-top=((space::SM)) {
            div justify-content=space-between gap=((space::SM)) {
                div color=(color::INFO) { "Vim edit" }
                div color=(if error.is_some() { color::ERROR } else { color::SUCCESS }) font-size=((typeface::LABEL)) { (phase) }
            }
            @if let Some(path) = path { div color=(color::STRONG) font-family="monospace" margin-top=((space::XS)) { (path) } }
            @if let (Some(file_index), Some(file_total)) = (payload.get("file_index").and_then(serde_json::Value::as_u64), payload.get("file_total").and_then(serde_json::Value::as_u64)) { div color=(color::MUTED) font-size=((typeface::DETAIL)) margin-top=((space::XS)) { "file " (file_index.saturating_add(1).to_string()) " of " (file_total.to_string()) } }
            @if let (Some(step_index), Some(step_total)) = (payload.get("step_index").and_then(serde_json::Value::as_u64), payload.get("step_total").and_then(serde_json::Value::as_u64)) { div color=(color::MUTED) font-size=((typeface::DETAIL)) margin-top=((space::XS)) { "step " (step_index.saturating_add(1).to_string()) " of " (step_total.to_string()) } }
            @if let Some(cursor) = cursor { div color=(color::MUTED) font-size=((typeface::DETAIL)) margin-top=((space::XS)) { "cursor " (cursor.get("line").and_then(serde_json::Value::as_u64).unwrap_or_default().to_string()) ":" (cursor.get("column").and_then(serde_json::Value::as_u64).unwrap_or_default().to_string()) } }
            @if let Some(changed) = changed { div color=(color::MUTED) font-size=((typeface::DETAIL)) margin-top=((space::XS)) { (if changed { "file changed" } else { "no file changes" }) } }
            @if let Some(message) = message { div color=(color::TEXT) font-size=((typeface::LABEL)) margin-top=((space::S6)) { (message) } }
            @if let Some(error) = error { div color=(color::ERROR) font-size=((typeface::LABEL)) margin-top=((space::S6)) white-space="preserve-wrap" { (error) } }
        }
    })
}

pub(super) fn render_vim_edit_playback(visual: &PluginVisualView) -> Option<Containers> {
    let payload = &visual.descriptor.payload;
    let success = payload
        .get("success")
        .and_then(serde_json::Value::as_bool)?;
    let summary = payload.get("summary").and_then(serde_json::Value::as_str);
    let path = payload.get("path").and_then(serde_json::Value::as_str);
    let mode = payload.get("tool_mode").and_then(serde_json::Value::as_str);
    let changed = payload.get("changed").and_then(serde_json::Value::as_bool);
    let diff = payload.get("diff").and_then(serde_json::Value::as_str);
    let diff_truncated = payload
        .get("diff_truncated")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    let error = payload.get("error").and_then(serde_json::Value::as_str);
    Some(container! {
        div border=((1, surface::BORDER)) border-radius=((radius::CONTROL)) background=(if success { surface::INSET } else { surface::ERROR_INSET }) padding=((space::S10)) margin-top=((space::SM)) {
            div justify-content=space-between gap=((space::SM)) {
                div color=(color::INFO) { "Vim edit result" }
                div color=(if success { color::SUCCESS } else { color::ERROR }) font-size=((typeface::LABEL)) { (if success { "completed" } else { "failed" }) }
            }
            @if let Some(path) = path { div color=(color::STRONG) font-family="monospace" margin-top=((space::XS)) { (path) } }
            div color=(color::MUTED) font-size=((typeface::DETAIL)) margin-top=((space::XS)) {
                @if let Some(mode) = mode { (mode) }
                @if let Some(changed) = changed { " · " (if changed { "changed" } else { "unchanged" }) }
                @if let Some(frame_count) = payload.get("frame_count").and_then(serde_json::Value::as_u64) { " · " (frame_count.to_string()) " playback frames" }
            }
            @if let Some(summary) = summary { div color=(color::TEXT) font-size=((typeface::LABEL)) margin-top=((space::S6)) { (summary) } }
            @if let Some(diff) = diff { (vim_diff_panel(diff, diff_truncated)) }
            @if let Some(error) = error { div color=(color::ERROR) font-size=((typeface::LABEL)) margin-top=((space::S6)) white-space="preserve-wrap" { (error) } }
            @if payload.get("frames_truncated").and_then(serde_json::Value::as_bool) == Some(true) { div color=(color::WARNING) font-size=((typeface::DETAIL)) margin-top=((space::S6)) { "Playback frames were truncated." } }
        }
    })
}

pub(super) fn render_vim_edit_request(visual: &PluginVisualView) -> Option<Containers> {
    let arguments = visual
        .descriptor
        .payload
        .get("arguments")
        .unwrap_or(&visual.descriptor.payload);
    let single_path = arguments.get("path").and_then(serde_json::Value::as_str);
    let files = arguments.get("files").and_then(serde_json::Value::as_array);
    let steps = arguments.get("steps").and_then(serde_json::Value::as_array);
    let sandbox = arguments.get("sandbox").and_then(serde_json::Value::as_str);
    let timeout_ms = arguments
        .get("timeout_ms")
        .and_then(serde_json::Value::as_u64);
    if single_path.is_none() && files.is_none() {
        return None;
    }
    Some(container! {
        div border=((1, surface::BORDER)) border-radius=((radius::CONTROL)) background=(surface::INSET) padding=((space::S10)) margin-top=((space::SM)) {
            div color=(color::INFO) margin-bottom=((space::S6)) { (visual.descriptor.title.as_deref().unwrap_or("Vim edit")) }
            @if let Some(path) = single_path {
                div color=(color::STRONG) font-family="monospace" white-space="preserve-wrap" { (path) }
                @if let Some(steps) = steps {
                    div color=(color::MUTED) font-size=((typeface::LABEL)) margin-top=((space::XS)) { "steps: " (steps.len().to_string()) }
                }
            }
            @if let Some(files) = files {
                @for file in files.iter().take(10) {
                    @if let Some(file) = file.as_object() {
                        div border-top=((1, surface::BORDER)) padding-top=((space::S6)) margin-top=((space::S6)) {
                            @if let Some(path) = file.get("path").and_then(serde_json::Value::as_str) {
                                span color=(color::STRONG) font-family="monospace" white-space="preserve-wrap" { (path) }
                            }
                            @if let Some(steps) = file.get("steps").and_then(serde_json::Value::as_array) {
                                span color=(color::MUTED) { " · steps: " (steps.len().to_string()) }
                            }
                        }
                    }
                }
                @if files.len() > 10 {
                    div color=(color::MUTED) font-size=((typeface::LABEL)) margin-top=((space::SM)) { "… " ((files.len() - 10).to_string()) " more files" }
                }
            }
            @if let Some(sandbox) = sandbox { div color=(color::MUTED) font-size=((typeface::LABEL)) margin-top=((space::XS)) { "sandbox: " (sandbox) } }
            @if let Some(timeout_ms) = timeout_ms { div color=(color::MUTED) font-size=((typeface::LABEL)) margin-top=((space::XS)) { "timeout: " (timeout_ms.to_string()) " ms" } }
        }
    })
}

pub(super) fn render_git_clone_request(visual: &PluginVisualView) -> Option<Containers> {
    let arguments = visual
        .descriptor
        .payload
        .get("arguments")
        .unwrap_or(&visual.descriptor.payload);
    let url = arguments.get("url")?.as_str()?;
    let reference = arguments
        .get("ref")
        .or_else(|| arguments.get("branch"))
        .and_then(serde_json::Value::as_str);
    let destination = arguments
        .get("destination")
        .and_then(serde_json::Value::as_str);
    Some(container! {
        div border=((1, surface::BORDER)) border-radius=((radius::CONTROL)) background=(surface::INSET) padding=((space::S10)) margin-top=((space::SM)) {
            div color=(color::INFO) margin-bottom=((space::S6)) { "Clone repository" }
            div color=(color::STRONG) font-family="monospace" white-space="preserve-wrap" { (url) }
            @if let Some(reference) = reference { div color=(color::MUTED) font-size=((typeface::LABEL)) margin-top=((space::XS)) { "ref: " (reference) } }
            @if let Some(destination) = destination { div color=(color::MUTED) font-size=((typeface::LABEL)) margin-top=((space::XS)) font-family="monospace" white-space="preserve-wrap" { "destination: " (destination) } }
        }
    })
}

pub(super) fn render_worktree_request(visual: &PluginVisualView) -> Option<Containers> {
    let arguments = visual
        .descriptor
        .payload
        .get("arguments")
        .unwrap_or(&visual.descriptor.payload);
    let operation = arguments
        .get("operation")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("worktree");
    let primary_path = arguments
        .get("path")
        .or_else(|| arguments.get("name"))
        .and_then(serde_json::Value::as_str)?;
    let cwd = arguments.get("cwd").and_then(serde_json::Value::as_str);
    let branch = arguments
        .get("branch")
        .or_else(|| arguments.get("new_branch"))
        .and_then(serde_json::Value::as_str);
    let base_ref = arguments
        .get("base_ref")
        .and_then(serde_json::Value::as_str);
    Some(container! {
        div border=((1, surface::BORDER)) border-radius=((radius::CONTROL)) background=(surface::INSET) padding=((space::S10)) margin-top=((space::SM)) {
            div direction=row gap=((space::SM)) align-items=center margin-bottom=((space::S6)) {
                span color=(color::INFO) { (operation) }
                span color=(color::MUTED) { "worktree" }
            }
            div color=(color::STRONG) font-family="monospace" white-space="preserve-wrap" { (primary_path) }
            @if let Some(cwd) = cwd { div color=(color::MUTED) font-size=((typeface::LABEL)) margin-top=((space::XS)) font-family="monospace" white-space="preserve-wrap" { "cwd: " (cwd) } }
            @if let Some(branch) = branch { div color=(color::MUTED) font-size=((typeface::LABEL)) margin-top=((space::XS)) { "branch: " (branch) } }
            @if let Some(base_ref) = base_ref { div color=(color::MUTED) font-size=((typeface::LABEL)) margin-top=((space::XS)) { "base ref: " (base_ref) } }
            @for key in ["detach", "force", "no_setup"] {
                @if let Some(value) = arguments.get(key).and_then(serde_json::Value::as_bool) {
                    div color=(color::MUTED) font-size=((typeface::LABEL)) margin-top=((space::XS)) { (key.replace('_', " ")) ": " (value.to_string()) }
                }
            }
        }
    })
}

pub(super) fn render_web_search_request(visual: &PluginVisualView) -> Option<Containers> {
    let arguments = visual
        .descriptor
        .payload
        .get("arguments")
        .unwrap_or(&visual.descriptor.payload);
    let query = arguments.get("query")?.as_str()?;
    let provider = arguments
        .get("provider")
        .and_then(serde_json::Value::as_str);
    let site = arguments.get("site").and_then(serde_json::Value::as_str);
    let freshness = arguments
        .get("freshness")
        .and_then(serde_json::Value::as_str);
    let region = arguments.get("region").and_then(serde_json::Value::as_str);
    let safe_search = arguments
        .get("safe_search")
        .and_then(serde_json::Value::as_str);
    let max_results = arguments
        .get("max_results")
        .and_then(serde_json::Value::as_u64);
    Some(container! {
        div border=((1, surface::BORDER)) border-radius=((radius::CONTROL)) background=(surface::INSET) padding=((space::S10)) margin-top=((space::SM)) {
            div direction=row gap=((space::SM)) align-items=center margin-bottom=((space::S6)) {
                span color=(color::INFO) { "Web search" }
                @if let Some(provider) = provider {
                    span color=(color::MUTED) { (provider) }
                }
            }
            div color=(color::STRONG) font-family="monospace" white-space="preserve-wrap" { (query) }
            @if let Some(site) = site {
                div color=(color::MUTED) font-size=((typeface::LABEL)) margin-top=((space::XS)) { "site: " (site) }
            }
            @if let Some(freshness) = freshness {
                div color=(color::MUTED) font-size=((typeface::LABEL)) margin-top=((space::XS)) { "freshness: " (freshness) }
            }
            @if let Some(region) = region {
                div color=(color::MUTED) font-size=((typeface::LABEL)) margin-top=((space::XS)) { "region: " (region) }
            }
            @if let Some(safe_search) = safe_search {
                div color=(color::MUTED) font-size=((typeface::LABEL)) margin-top=((space::XS)) { "safe search: " (safe_search) }
            }
            @if let Some(max_results) = max_results {
                div color=(color::MUTED) font-size=((typeface::LABEL)) margin-top=((space::XS)) { "max results: " (max_results.to_string()) }
            }
        }
    })
}

pub(super) fn render_web_fetch_request(visual: &PluginVisualView) -> Option<Containers> {
    let arguments = visual
        .descriptor
        .payload
        .get("arguments")
        .unwrap_or(&visual.descriptor.payload);
    let url = arguments.get("url")?.as_str()?;
    let provider = arguments
        .get("provider")
        .and_then(serde_json::Value::as_str);
    let render = arguments.get("render").and_then(serde_json::Value::as_bool);
    let max_bytes = arguments
        .get("max_bytes")
        .and_then(serde_json::Value::as_u64);
    let timeout_ms = arguments
        .get("timeout_ms")
        .and_then(serde_json::Value::as_u64);
    let prompt = arguments.get("prompt").and_then(serde_json::Value::as_str);
    Some(container! {
        div border=((1, surface::BORDER)) border-radius=((radius::CONTROL)) background=(surface::INSET) padding=((space::S10)) margin-top=((space::SM)) {
            div direction=row gap=((space::SM)) align-items=center margin-bottom=((space::S6)) {
                span color=(color::INFO) { "Fetch page" }
                @if let Some(provider) = provider {
                    span color=(color::MUTED) { (provider) }
                }
            }
            div color=(color::STRONG) font-family="monospace" white-space="preserve-wrap" { (url) }
            @if let Some(render) = render {
                div color=(color::MUTED) font-size=((typeface::LABEL)) margin-top=((space::XS)) { "rendered page fetch: " (render.to_string()) }
            }
            @if let Some(max_bytes) = max_bytes {
                div color=(color::MUTED) font-size=((typeface::LABEL)) margin-top=((space::XS)) { "max bytes: " (max_bytes.to_string()) }
            }
            @if let Some(timeout_ms) = timeout_ms {
                div color=(color::MUTED) font-size=((typeface::LABEL)) margin-top=((space::XS)) { "timeout: " (timeout_ms.to_string()) " ms" }
            }
            @if let Some(prompt) = prompt {
                div color=(color::MUTED) font-size=((typeface::LABEL)) margin-top=((space::XS)) white-space="preserve-wrap" { "prompt: " (prompt) }
            }
        }
    })
}

pub(super) fn render_shell_request(visual: &PluginVisualView) -> Option<Containers> {
    let payload = &visual.descriptor.payload;
    let arguments = payload.get("arguments").unwrap_or(payload);
    let command = arguments.get("command")?.as_str()?;
    let cwd = arguments.get("cwd").and_then(serde_json::Value::as_str);
    let output = payload
        .pointer("/_bcode_runtime/output")
        .and_then(serde_json::Value::as_str);
    Some(container! {
        div border=((1, surface::BORDER)) border-radius=((radius::CONTROL)) background=(surface::INSET) padding=((space::S10)) margin-top=((space::SM)) {
            @if let Some(cwd) = cwd {
                div color=(color::MUTED) font-size=((typeface::DETAIL)) margin-bottom=((space::XS)) { (cwd) }
            }
            div color=(color::STRONG) font-family="monospace" white-space="preserve-wrap" { (command) }
            @if let Some(output) = output {
                div color=(color::TEXT) font-family="monospace" white-space="preserve-wrap" border-top=((1, surface::BORDER)) margin-top=((space::SM)) padding-top=((space::SM)) { (output) }
            }
        }
    })
}

const MAX_JSON_PANEL_CHARS: usize = 32_000;

pub(super) fn json_panel(title: &str, value: &serde_json::Value) -> Containers {
    let json = serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string());
    let (json, truncated) = json.char_indices().nth(MAX_JSON_PANEL_CHARS).map_or_else(
        || (json.as_str(), false),
        |(byte_index, _)| (&json[..byte_index], true),
    );
    container! {
        details margin-top=((space::SM)) {
            summary color=(color::MUTED) { (title) }
            div white-space="preserve-wrap" background=(surface::INSET) border=((1, surface::BORDER)) border-radius=((radius::CONTROL)) padding=((space::SM)) color=(color::TEXT) { (json) }
            @if truncated {
                div color=(color::WARNING) font-size=((typeface::DETAIL)) margin-top=((space::S6)) { "Structured details truncated for display." }
            }
        }
    }
}
