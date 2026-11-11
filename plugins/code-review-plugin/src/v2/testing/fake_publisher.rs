//! An in-process external review publisher and importer for app tests.
//!
//! It serves `bcode.review_publisher/v1` and `bcode.review_importer/v1`. The publisher
//! publishes pull requests of the fake review-source provider, previews each bundle as plain
//! text, and records every bundle it submits; the importer answers with configured threads.

use super::fake_provider::FAKE_PROVIDER;
use crate::v2::provider::ProviderCallError;
use bcode_code_review_models::{
    ExternalPublishReviewRequest, ExternalReviewImportRequest, ExternalReviewImportResponse,
    ExternalReviewThread, OP_REVIEW_IMPORTER_IMPORT, OP_REVIEW_IMPORTER_MANIFEST,
    OP_REVIEW_PUBLISHER_MANIFEST, OP_REVIEW_PUBLISHER_PREVIEW, OP_REVIEW_PUBLISHER_SUBMIT,
    PublishReviewPreviewResponse, PublishReviewResponse, REVIEW_IMPORTER_INTERFACE_ID,
    REVIEW_PUBLISHER_INTERFACE_ID, ReviewBundle, ReviewImporterManifest,
    ReviewPublisherCapabilities, ReviewPublisherManifest,
};
use std::fmt::Write as _;
use std::sync::Mutex;

/// Plugin id of the fake publisher and importer.
pub const FAKE_PUBLISHER_PLUGIN: &str = "test.fake_forge_reviews";
/// Publisher id.
pub const FAKE_PUBLISHER: &str = "fake_pr_review";
/// Importer id.
pub const FAKE_IMPORTER: &str = "fake_pr_import";

/// The fake publisher and importer.
#[derive(Debug, Default)]
pub struct FakePublisher {
    submitted: Mutex<Vec<ReviewBundle>>,
    imports: Mutex<Vec<ExternalReviewImportRequest>>,
    threads: Vec<ExternalReviewThread>,
    fail_submit_of: Option<u64>,
}

impl FakePublisher {
    /// A publisher whose importer answers with `threads`.
    #[must_use]
    pub fn with_threads(mut self, threads: Vec<ExternalReviewThread>) -> Self {
        self.threads = threads;
        self
    }

    /// Fail submitting the bundle for pull request `number`.
    #[must_use]
    pub const fn failing_submit_of(mut self, number: u64) -> Self {
        self.fail_submit_of = Some(number);
        self
    }

    /// Bundles submitted so far.
    ///
    /// # Panics
    ///
    /// Panics when the lock is poisoned.
    #[must_use]
    pub fn submitted(&self) -> Vec<ReviewBundle> {
        self.submitted.lock().expect("submitted lock").clone()
    }

    /// Import requests received so far.
    ///
    /// # Panics
    ///
    /// Panics when the lock is poisoned.
    #[must_use]
    pub fn imports(&self) -> Vec<ExternalReviewImportRequest> {
        self.imports.lock().expect("imports lock").clone()
    }

    fn preview(bundle: &ReviewBundle) -> String {
        let mut out = String::new();
        let target = bundle.pull_request.as_ref().map_or_else(
            || "default target".to_owned(),
            |target| format!("{}#{}", target.repository, target.number),
        );
        let _ = writeln!(out, "Fake review of {target}");
        for thread in &bundle.threads {
            let line = thread
                .anchor
                .new_line
                .or(thread.anchor.old_line)
                .map_or_else(String::new, |line| format!(":{line}"));
            let place = if thread.summary_only {
                "summary"
            } else {
                "inline"
            };
            let _ = writeln!(
                out,
                "- {place} {}{line}: {}",
                thread.anchor.file_path,
                thread
                    .comments
                    .first()
                    .map_or("", |comment| comment.body.as_str())
            );
        }
        out
    }

    /// Answer one operation of `interface`.
    ///
    /// # Errors
    ///
    /// Unknown operations, and the configured submit failure.
    ///
    /// # Panics
    ///
    /// Panics when a request does not decode or a lock is poisoned.
    pub fn answer(
        &self,
        interface: &str,
        operation: &str,
        payload: &[u8],
    ) -> Result<Vec<u8>, ProviderCallError> {
        let encode = |value: serde_json::Value| Ok(serde_json::to_vec(&value).expect("encodes"));
        match (interface, operation) {
            (REVIEW_PUBLISHER_INTERFACE_ID, OP_REVIEW_PUBLISHER_MANIFEST) => {
                encode(serde_json::to_value(manifest()).expect("manifest"))
            }
            (REVIEW_PUBLISHER_INTERFACE_ID, OP_REVIEW_PUBLISHER_PREVIEW) => {
                let request: ExternalPublishReviewRequest =
                    serde_json::from_slice(payload).expect("preview request");
                encode(
                    serde_json::to_value(PublishReviewPreviewResponse {
                        publisher_id: FAKE_PUBLISHER.to_owned(),
                        preview: Self::preview(&request.bundle),
                    })
                    .expect("preview"),
                )
            }
            (REVIEW_PUBLISHER_INTERFACE_ID, OP_REVIEW_PUBLISHER_SUBMIT) => {
                let request: ExternalPublishReviewRequest =
                    serde_json::from_slice(payload).expect("submit request");
                let number = request
                    .bundle
                    .pull_request
                    .as_ref()
                    .map(|target| target.number);
                if number.is_some() && number == self.fail_submit_of {
                    return Err(ProviderCallError {
                        code: "fake_failed".to_owned(),
                        message: format!("cannot publish to #{}", number.unwrap_or_default()),
                    });
                }
                self.submitted
                    .lock()
                    .expect("submitted lock")
                    .push(request.bundle.clone());
                encode(
                    serde_json::to_value(PublishReviewResponse {
                        publisher_id: FAKE_PUBLISHER.to_owned(),
                        submitted: true,
                        output: number.map(|number| format!("https://example.com/pull/{number}")),
                        message: format!(
                            "published {} threads to #{}",
                            request.bundle.threads.len(),
                            number.unwrap_or_default()
                        ),
                    })
                    .expect("response"),
                )
            }
            (REVIEW_IMPORTER_INTERFACE_ID, OP_REVIEW_IMPORTER_MANIFEST) => encode(
                serde_json::to_value(ReviewImporterManifest {
                    id: FAKE_IMPORTER.to_owned(),
                    label: "Fake forge review".to_owned(),
                    description: "Import fake pull request threads".to_owned(),
                    options_schema: serde_json::json!({
                        "type": "object",
                        "properties": { "pull_request": { "type": "string" } }
                    }),
                })
                .expect("manifest"),
            ),
            (REVIEW_IMPORTER_INTERFACE_ID, OP_REVIEW_IMPORTER_IMPORT) => {
                let request: ExternalReviewImportRequest =
                    serde_json::from_slice(payload).expect("import request");
                self.imports.lock().expect("imports lock").push(request);
                encode(
                    serde_json::to_value(ExternalReviewImportResponse {
                        importer_id: FAKE_IMPORTER.to_owned(),
                        threads: self.threads.clone(),
                        warnings: Vec::new(),
                    })
                    .expect("import"),
                )
            }
            _ => Err(ProviderCallError {
                code: "unsupported_operation".to_owned(),
                message: format!("{interface} {operation}"),
            }),
        }
    }
}

/// The fake publisher's manifest.
#[must_use]
pub fn manifest() -> ReviewPublisherManifest {
    ReviewPublisherManifest {
        id: FAKE_PUBLISHER.to_owned(),
        label: "Fake forge review".to_owned(),
        description: "Publish to fake pull requests".to_owned(),
        capabilities: ReviewPublisherCapabilities {
            preview: true,
            submit: true,
            update_existing: false,
            supports_threads: true,
            supports_ranges: true,
            supports_inline_comments: true,
            supports_summary_comment: true,
        },
        options_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "event": { "type": "string", "default": "COMMENT", "enum": ["COMMENT", "APPROVE"] },
                "summary": { "type": "string" }
            }
        }),
        route: None,
        pull_request_provider: Some(FAKE_PROVIDER.to_owned()),
    }
}
