//! The state database: queries over review records, row decoding, and schema migrations.

use super::*;

pub struct CodeReviewDb<'a> {
    db: &'a dyn Database,
}

impl<'a> CodeReviewDb<'a> {
    pub const fn new(db: &'a dyn Database) -> Self {
        Self { db }
    }

    /// The database, for queries kept in other modules.
    pub(super) const fn db(&self) -> &'a dyn Database {
        self.db
    }

    pub async fn list_workspaces(
        &self,
        repo_root: &Path,
        include_archived: bool,
    ) -> Result<Vec<ReviewWorkspace>, StoreError> {
        let rows = self
            .db
            .select("review_workspaces")
            .columns(&[
                "workspace_id",
                "repo_root",
                "title",
                "sources_json",
                "created_at_ms",
                "updated_at_ms",
                "archived_at_ms",
                "viewed_files_json",
                "viewed_marks_json",
                "reviewed_revisions_json",
                "presentation_v2_json",
                "stack_snapshots_json",
            ])
            .filter(Box::new(where_eq(
                "repo_root",
                repo_root.display().to_string(),
            )))
            .execute(self.db)
            .await?;
        let mut workspaces = Vec::new();
        for row in rows {
            let archived_at_ms = optional_i64(&row, "archived_at_ms").map(i64_to_u64);
            if archived_at_ms.is_some() && !include_archived {
                continue;
            }
            let workspace = workspace_from_row(&row)?;
            // The viewed state of unsaved reviews is kept under their transient scope id; it is
            // not a review of its own.
            if crate::scope::is_transient_scope_id(&workspace.id) {
                continue;
            }
            workspaces.push(workspace);
        }
        workspaces.sort_by_key(|workspace| std::cmp::Reverse(workspace.updated_at_ms));
        Ok(workspaces)
    }

    pub async fn get_workspace(
        &self,
        workspace_id: &str,
    ) -> Result<Option<ReviewWorkspace>, StoreError> {
        let Some(row) = self
            .db
            .select("review_workspaces")
            .columns(&[
                "workspace_id",
                "repo_root",
                "title",
                "sources_json",
                "created_at_ms",
                "updated_at_ms",
                "archived_at_ms",
                "viewed_files_json",
                "viewed_marks_json",
                "reviewed_revisions_json",
                "presentation_v2_json",
                "stack_snapshots_json",
            ])
            .filter(Box::new(where_eq("workspace_id", workspace_id)))
            .execute_first(self.db)
            .await?
        else {
            return Ok(None);
        };
        Ok(Some(workspace_from_row(&row)?))
    }

    pub async fn save_workspace(&self, workspace: &ReviewWorkspace) -> Result<(), StoreError> {
        let sources_json = serde_json::to_string(&workspace.sources)?;
        let viewed_files_json = serde_json::to_string(&workspace.viewed_files)?;
        let viewed_marks_json = serde_json::to_string(&workspace.viewed_marks)?;
        let reviewed_revisions_json = serde_json::to_string(&workspace.reviewed_revisions)?;
        let presentation_json = workspace
            .presentation
            .as_ref()
            .map(serde_json::to_string)
            .transpose()?;
        let stack_snapshots_json = (!workspace.stack_snapshots.is_empty())
            .then(|| serde_json::to_string(&workspace.stack_snapshots))
            .transpose()?;
        if self.get_workspace(&workspace.id).await?.is_some() {
            self.db
                .update("review_workspaces")
                .value("repo_root", workspace.repo_root.display().to_string())
                .value("title", workspace.title.clone())
                .value("sources_json", sources_json)
                .value("viewed_files_json", viewed_files_json.clone())
                .value("viewed_marks_json", viewed_marks_json.clone())
                .value("reviewed_revisions_json", reviewed_revisions_json.clone())
                .value("presentation_v2_json", presentation_json.clone())
                .value("stack_snapshots_json", stack_snapshots_json.clone())
                .value(
                    "updated_at_ms",
                    u64_to_i64(workspace.updated_at_ms.unwrap_or_else(now_ms)),
                )
                .value("archived_at_ms", optional_u64(workspace.archived_at_ms))
                .filter(Box::new(where_eq("workspace_id", workspace.id.clone())))
                .execute(self.db)
                .await?;
            return Ok(());
        }
        self.db
            .insert("review_workspaces")
            .value("workspace_id", workspace.id.clone())
            .value("repo_root", workspace.repo_root.display().to_string())
            .value("title", workspace.title.clone())
            .value("sources_json", sources_json)
            .value("viewed_files_json", viewed_files_json)
            .value("viewed_marks_json", viewed_marks_json)
            .value("reviewed_revisions_json", reviewed_revisions_json)
            .value("presentation_v2_json", presentation_json)
            .value("stack_snapshots_json", stack_snapshots_json)
            .value(
                "created_at_ms",
                u64_to_i64(workspace.created_at_ms.unwrap_or_else(now_ms)),
            )
            .value(
                "updated_at_ms",
                u64_to_i64(workspace.updated_at_ms.unwrap_or_else(now_ms)),
            )
            .value("archived_at_ms", optional_u64(workspace.archived_at_ms))
            .execute(self.db)
            .await?;
        Ok(())
    }

    pub async fn save_external_import(
        &self,
        record: &ExternalReviewImportRecord,
    ) -> Result<(), StoreError> {
        let threads_json = serde_json::to_string(&record.threads)?;
        let warnings_json = serde_json::to_string(&record.warnings)?;
        let existing = self
            .db
            .select("review_external_imports")
            .columns(&["workspace_id"])
            .filter(Box::new(where_eq(
                "workspace_id",
                record.workspace_id.clone(),
            )))
            .filter(Box::new(where_eq(
                "importer_id",
                record.importer_id.clone(),
            )))
            .execute_first(self.db)
            .await?;
        if existing.is_some() {
            self.db
                .update("review_external_imports")
                .value("threads_json", threads_json)
                .value("warnings_json", warnings_json)
                .value("updated_at_ms", u64_to_i64(record.updated_at_ms))
                .filter(Box::new(where_eq(
                    "workspace_id",
                    record.workspace_id.clone(),
                )))
                .filter(Box::new(where_eq(
                    "importer_id",
                    record.importer_id.clone(),
                )))
                .execute(self.db)
                .await?;
            return Ok(());
        }
        self.db
            .insert("review_external_imports")
            .value("workspace_id", record.workspace_id.clone())
            .value("importer_id", record.importer_id.clone())
            .value("threads_json", threads_json)
            .value("warnings_json", warnings_json)
            .value("updated_at_ms", u64_to_i64(record.updated_at_ms))
            .execute(self.db)
            .await?;
        Ok(())
    }

    pub async fn list_external_imports(
        &self,
        workspace_id: &str,
    ) -> Result<Vec<ExternalReviewImportRecord>, StoreError> {
        let rows = self
            .db
            .select("review_external_imports")
            .columns(&[
                "workspace_id",
                "importer_id",
                "threads_json",
                "warnings_json",
                "updated_at_ms",
            ])
            .filter(Box::new(where_eq("workspace_id", workspace_id)))
            .execute(self.db)
            .await?;
        rows.iter().map(external_import_from_row).collect()
    }

    pub async fn save_publish_record(
        &self,
        record: &ReviewPublishRecord,
    ) -> Result<(), StoreError> {
        self.db
            .insert("review_publish_records")
            .value("record_id", record.id.clone())
            .value("workspace_id", record.workspace_id.clone())
            .value("review_id", record.review_id.clone())
            .value("publisher_id", record.publisher_id.clone())
            .value("submitted", i64::from(record.submitted))
            .value(
                "published_thread_anchors_json",
                serde_json::to_string(&record.published_thread_anchors)?,
            )
            .value("output", record.output.clone())
            .value("message", record.message.clone())
            .value("created_at_ms", u64_to_i64(record.created_at_ms))
            .execute(self.db)
            .await?;
        Ok(())
    }

    pub async fn list_publish_records(
        &self,
        workspace_id: &str,
    ) -> Result<Vec<ReviewPublishRecord>, StoreError> {
        let rows = self
            .db
            .select("review_publish_records")
            .columns(&[
                "record_id",
                "workspace_id",
                "review_id",
                "publisher_id",
                "submitted",
                "published_thread_anchors_json",
                "output",
                "message",
                "created_at_ms",
            ])
            .filter(Box::new(where_eq("workspace_id", workspace_id.to_string())))
            .execute(self.db)
            .await?;
        let mut records = rows
            .iter()
            .map(publish_record_from_row)
            .collect::<Result<Vec<_>, _>>()?;
        records.sort_by_key(|record| record.created_at_ms);
        Ok(records)
    }

    pub async fn last_publish_record(
        &self,
        workspace_id: Option<&str>,
    ) -> Result<Option<ReviewPublishRecord>, StoreError> {
        let Some(workspace_id) = workspace_id else {
            return Ok(None);
        };
        let rows = self
            .db
            .select("review_publish_records")
            .columns(&[
                "record_id",
                "workspace_id",
                "review_id",
                "publisher_id",
                "submitted",
                "published_thread_anchors_json",
                "output",
                "message",
                "created_at_ms",
            ])
            .filter(Box::new(where_eq("workspace_id", workspace_id.to_string())))
            .execute(self.db)
            .await?;
        rows.iter()
            .max_by_key(|row| required_i64(row, "created_at_ms").unwrap_or(0))
            .map(publish_record_from_row)
            .transpose()
    }

    pub async fn archive_workspace(
        &self,
        workspace_id: &str,
        now: u64,
    ) -> Result<bool, StoreError> {
        if self.get_workspace(workspace_id).await?.is_none() {
            return Ok(false);
        }
        self.db
            .update("review_workspaces")
            .value("archived_at_ms", u64_to_i64(now))
            .value("updated_at_ms", u64_to_i64(now))
            .filter(Box::new(where_eq("workspace_id", workspace_id.to_string())))
            .execute(self.db)
            .await?;
        Ok(true)
    }

    pub async fn list_ai_exchanges(
        &self,
        review_key: &str,
    ) -> Result<Vec<ReviewAiExchange>, StoreError> {
        let rows = self
            .db
            .select("review_ai_exchanges")
            .columns(&[
                "schema_version",
                "exchange_id",
                "anchor_json",
                "question",
                "session_id",
                "status",
                "error",
                "created_at_ms",
                "updated_at_ms",
            ])
            .filter(Box::new(where_eq("review_key", review_key)))
            .execute(self.db)
            .await?;
        let mut exchanges = rows
            .iter()
            .map(ai_exchange_from_row)
            .collect::<Result<Vec<_>, _>>()?;
        exchanges.sort_by_key(|exchange| (exchange.created_at_ms, exchange.exchange_id.clone()));
        Ok(exchanges)
    }

    pub async fn delete_ai_exchange(
        &self,
        review_key: &str,
        exchange_id: &str,
    ) -> Result<bool, StoreError> {
        let existing = self
            .db
            .select("review_ai_exchanges")
            .columns(&["exchange_id"])
            .filter(Box::new(where_eq("review_key", review_key.to_string())))
            .filter(Box::new(where_eq("exchange_id", exchange_id.to_string())))
            .execute_first(self.db)
            .await?;
        if existing.is_none() {
            return Ok(false);
        }
        self.db
            .delete("review_ai_exchanges")
            .filter(Box::new(where_eq("review_key", review_key.to_string())))
            .filter(Box::new(where_eq("exchange_id", exchange_id.to_string())))
            .execute(self.db)
            .await?;
        Ok(true)
    }

    pub async fn delete_suggestion(
        &self,
        review_key: &str,
        suggestion_id: &str,
    ) -> Result<bool, StoreError> {
        let existing = self
            .db
            .select("review_suggestions")
            .columns(&["suggestion_id"])
            .filter(Box::new(where_eq("review_key", review_key.to_string())))
            .filter(Box::new(where_eq(
                "suggestion_id",
                suggestion_id.to_string(),
            )))
            .execute_first(self.db)
            .await?;
        if existing.is_none() {
            return Ok(false);
        }
        self.db
            .delete("review_suggestions")
            .filter(Box::new(where_eq("review_key", review_key.to_string())))
            .filter(Box::new(where_eq(
                "suggestion_id",
                suggestion_id.to_string(),
            )))
            .execute(self.db)
            .await?;
        Ok(true)
    }

    pub async fn save_ai_exchange(
        &self,
        review_key: &str,
        repo_root: &Path,
        target_kind: &str,
        target_json: &str,
        mut exchange: ReviewAiExchange,
    ) -> Result<ReviewAiExchange, StoreError> {
        let now = now_ms();
        self.ensure_review(review_key, repo_root, target_kind, target_json, now)
            .await?;
        let existing = self
            .db
            .select("review_ai_exchanges")
            .columns(&["exchange_id", "created_at_ms"])
            .filter(Box::new(where_eq(
                "exchange_id",
                exchange.exchange_id.clone(),
            )))
            .execute_first(self.db)
            .await?;
        exchange.updated_at_ms = now;
        if let Some(row) = existing {
            exchange.created_at_ms = i64_to_u64(required_i64(&row, "created_at_ms")?);
            self.db
                .update("review_ai_exchanges")
                .value("schema_version", i64::from(exchange.schema_version))
                .value("anchor_json", serde_json::to_string(&exchange.anchor)?)
                .value("question", exchange.question.clone())
                .value("session_id", exchange.session_id.clone())
                .value("status", ai_exchange_status_to_str(exchange.status))
                .value("error", exchange.error.clone())
                .value("updated_at_ms", u64_to_i64(now))
                .filter(Box::new(where_eq(
                    "exchange_id",
                    exchange.exchange_id.clone(),
                )))
                .execute(self.db)
                .await?;
        } else {
            exchange.created_at_ms = now;
            self.db
                .insert("review_ai_exchanges")
                .value("schema_version", i64::from(exchange.schema_version))
                .value("exchange_id", exchange.exchange_id.clone())
                .value("review_key", review_key.to_string())
                .value("anchor_json", serde_json::to_string(&exchange.anchor)?)
                .value("question", exchange.question.clone())
                .value("session_id", exchange.session_id.clone())
                .value("status", ai_exchange_status_to_str(exchange.status))
                .value("error", exchange.error.clone())
                .value("created_at_ms", u64_to_i64(now))
                .value("updated_at_ms", u64_to_i64(now))
                .execute(self.db)
                .await?;
        }
        Ok(exchange)
    }

    /// Record `binding` for its session (replacing an earlier one), keyed by the annotation scope
    /// id `workspace_id` of its review.
    pub async fn save_assistant_binding(
        &self,
        workspace_id: &str,
        repo_root: &Path,
        mut binding: ReviewAssistantBinding,
    ) -> Result<ReviewAssistantBinding, StoreError> {
        let now = now_ms();
        let existing = self
            .assistant_binding_for_session(&binding.session_id)
            .await?;
        binding.updated_at_ms = now;
        binding.created_at_ms = existing.map_or(now, |existing| existing.created_at_ms);
        let json = serde_json::to_string(&binding)?;
        if self
            .db
            .select("review_assistant_sessions")
            .columns(&["session_id"])
            .filter(Box::new(where_eq("session_id", binding.session_id.clone())))
            .execute_first(self.db)
            .await?
            .is_some()
        {
            self.db
                .update("review_assistant_sessions")
                .value("workspace_id", workspace_id.to_string())
                .value("repo_root", repo_root.display().to_string())
                .value("binding_json", json)
                .value("updated_at_ms", u64_to_i64(now))
                .filter(Box::new(where_eq("session_id", binding.session_id.clone())))
                .execute(self.db)
                .await?;
        } else {
            self.db
                .insert("review_assistant_sessions")
                .value("session_id", binding.session_id.clone())
                .value("workspace_id", workspace_id.to_string())
                .value("repo_root", repo_root.display().to_string())
                .value("binding_json", json)
                .value("created_at_ms", u64_to_i64(binding.created_at_ms))
                .value("updated_at_ms", u64_to_i64(now))
                .execute(self.db)
                .await?;
        }
        Ok(binding)
    }

    /// The binding of `session_id`, when it has one this version can read.
    pub async fn assistant_binding_for_session(
        &self,
        session_id: &str,
    ) -> Result<Option<ReviewAssistantBinding>, StoreError> {
        let row = self
            .db
            .select("review_assistant_sessions")
            .columns(&["binding_json"])
            .filter(Box::new(where_eq("session_id", session_id.to_string())))
            .execute_first(self.db)
            .await?;
        Ok(row.as_ref().and_then(assistant_binding_from_row))
    }

    /// The most recently refreshed readable binding of the review scoped by `workspace_id`.
    pub async fn assistant_binding_for_review(
        &self,
        workspace_id: &str,
    ) -> Result<Option<ReviewAssistantBinding>, StoreError> {
        let rows = self
            .db
            .select("review_assistant_sessions")
            .columns(&["binding_json"])
            .filter(Box::new(where_eq("workspace_id", workspace_id.to_string())))
            .execute(self.db)
            .await?;
        Ok(rows
            .iter()
            .filter_map(assistant_binding_from_row)
            .max_by_key(|binding| (binding.updated_at_ms, binding.session_id.clone())))
    }

    pub async fn list_suggestions(
        &self,
        review_key: &str,
    ) -> Result<Vec<ReviewSuggestion>, StoreError> {
        let rows = self
            .db
            .select("review_suggestions")
            .columns(&[
                "suggestion_id",
                "anchor_json",
                "body",
                "session_id",
                "status",
                "rationale",
                "created_at_ms",
                "updated_at_ms",
            ])
            .filter(Box::new(where_eq("review_key", review_key)))
            .execute(self.db)
            .await?;
        rows.iter().map(suggestion_from_row).collect()
    }

    pub async fn save_suggestion(
        &self,
        review_key: &str,
        repo_root: &Path,
        target_kind: &str,
        target_json: &str,
        mut suggestion: ReviewSuggestion,
    ) -> Result<ReviewSuggestion, StoreError> {
        let now = now_ms();
        self.ensure_review(review_key, repo_root, target_kind, target_json, now)
            .await?;
        let existing = self
            .db
            .select("review_suggestions")
            .columns(&["suggestion_id", "created_at_ms"])
            .filter(Box::new(where_eq(
                "suggestion_id",
                suggestion.suggestion_id.clone(),
            )))
            .execute_first(self.db)
            .await?;
        suggestion.updated_at_ms = now;
        if let Some(row) = existing {
            suggestion.created_at_ms = i64_to_u64(required_i64(&row, "created_at_ms")?);
            self.db
                .update("review_suggestions")
                .value("anchor_json", serde_json::to_string(&suggestion.anchor)?)
                .value("body", suggestion.body.clone())
                .value("session_id", suggestion.session_id.clone())
                .value("status", suggestion_status_to_str(suggestion.status))
                .value("rationale", suggestion.rationale.clone())
                .value("updated_at_ms", u64_to_i64(now))
                .filter(Box::new(where_eq(
                    "suggestion_id",
                    suggestion.suggestion_id.clone(),
                )))
                .execute(self.db)
                .await?;
        } else {
            suggestion.created_at_ms = now;
            self.db
                .insert("review_suggestions")
                .value("suggestion_id", suggestion.suggestion_id.clone())
                .value("review_key", review_key.to_string())
                .value("anchor_json", serde_json::to_string(&suggestion.anchor)?)
                .value("body", suggestion.body.clone())
                .value("session_id", suggestion.session_id.clone())
                .value("status", suggestion_status_to_str(suggestion.status))
                .value("rationale", suggestion.rationale.clone())
                .value("created_at_ms", u64_to_i64(now))
                .value("updated_at_ms", u64_to_i64(now))
                .execute(self.db)
                .await?;
        }
        Ok(suggestion)
    }

    pub async fn list_drafts(&self, review_key: &str) -> Result<Vec<DraftComment>, StoreError> {
        let thread_rows = self
            .db
            .select("draft_threads")
            .columns(&[
                "thread_id",
                "anchor_kind",
                "session_id",
                "file_path",
                "diff_row",
                "start_diff_row",
                "end_diff_row",
                "old_start",
                "old_end",
                "new_start",
                "new_end",
                "old_line",
                "new_line",
                "line_kind",
                "thread_kind",
                "severity",
                "resolved_at_ms",
                "is_file_anchor",
                "surface_id",
                "source_id",
                "head_oid",
                "blob_oid",
                "reply_to",
            ])
            .filter(Box::new(where_eq("review_key", review_key)))
            .execute(self.db)
            .await?;
        let mut drafts = Vec::new();
        for thread in thread_rows {
            let thread_id = required_text(&thread, "thread_id")?;
            let session_id = optional_text(&thread, "session_id");
            let thread_kind = thread_kind_from_str(
                optional_text(&thread, "thread_kind")
                    .as_deref()
                    .unwrap_or("note"),
            )?;
            let severity = thread_severity_from_str(
                optional_text(&thread, "severity")
                    .as_deref()
                    .unwrap_or("info"),
            )?;
            let resolved_at_ms = optional_i64(&thread, "resolved_at_ms").map(i64_to_u64);
            let anchor = DraftAnchor {
                kind: anchor_kind_from_str(
                    optional_text(&thread, "anchor_kind")
                        .as_deref()
                        .unwrap_or("range"),
                )?,
                file_path: required_text(&thread, "file_path")?,
                diff_row: i64_to_u64(required_i64(&thread, "diff_row")?),
                start_diff_row: optional_i64(&thread, "start_diff_row").map(i64_to_u64),
                end_diff_row: optional_i64(&thread, "end_diff_row").map(i64_to_u64),
                old_start: optional_i64(&thread, "old_start").map(i64_to_u32),
                old_end: optional_i64(&thread, "old_end").map(i64_to_u32),
                new_start: optional_i64(&thread, "new_start").map(i64_to_u32),
                new_end: optional_i64(&thread, "new_end").map(i64_to_u32),
                old_line: optional_i64(&thread, "old_line").map(i64_to_u32),
                new_line: optional_i64(&thread, "new_line").map(i64_to_u32),
                line_kind: line_kind_from_str(&required_text(&thread, "line_kind")?)?,
                is_file_anchor: optional_bool(&thread, "is_file_anchor"),
                surface_id: optional_text(&thread, "surface_id"),
                source_id: optional_text(&thread, "source_id"),
                head_oid: optional_text(&thread, "head_oid"),
                blob_oid: optional_text(&thread, "blob_oid"),
                reply_to: optional_text(&thread, "reply_to"),
            };
            let comment_rows = self
                .db
                .select("draft_comments")
                .columns(&["comment_id", "body", "created_at_ms", "updated_at_ms"])
                .filter(Box::new(where_eq("thread_id", thread_id.clone())))
                .execute(self.db)
                .await?;
            drafts.extend(
                comment_rows
                    .into_iter()
                    .map(|row| {
                        comment_from_row(
                            &row,
                            &thread_id,
                            &anchor,
                            session_id.clone(),
                            resolved_at_ms,
                            thread_kind,
                            severity,
                        )
                    })
                    .collect::<Result<Vec<_>, _>>()?,
            );
        }
        Ok(drafts)
    }

    /// Thread and comment counts of `review_key`'s drafts, and when a comment last changed.
    pub async fn draft_counts(
        &self,
        review_key: &str,
    ) -> Result<(usize, usize, Option<u64>), StoreError> {
        let thread_rows = self
            .db
            .select("draft_threads")
            .columns(&["thread_id"])
            .filter(Box::new(where_eq("review_key", review_key)))
            .execute(self.db)
            .await?;
        let thread_count = thread_rows.len();
        let mut draft_count = 0usize;
        let mut latest = None;
        for thread in thread_rows {
            let thread_id = required_text(&thread, "thread_id")?;
            let comments = self
                .db
                .select("draft_comments")
                .columns(&["comment_id", "updated_at_ms"])
                .filter(Box::new(where_eq("thread_id", thread_id)))
                .execute(self.db)
                .await?;
            draft_count = draft_count.saturating_add(comments.len());
            latest = comments
                .iter()
                .filter_map(|comment| optional_i64(comment, "updated_at_ms").map(i64_to_u64))
                .chain(latest)
                .max();
        }
        Ok((thread_count, draft_count, latest))
    }

    pub async fn save_draft(
        &self,
        review_key: &str,
        repo_root: &Path,
        target_kind: &str,
        target_json: &str,
        anchor: DraftAnchor,
        body: &str,
    ) -> Result<DraftComment, StoreError> {
        let now = now_ms();
        let thread_id = thread_id(review_key, &anchor)?;
        let comment_id = comment_id(&thread_id, body, now);
        self.ensure_review(review_key, repo_root, target_kind, target_json, now)
            .await?;
        self.ensure_thread(review_key, &thread_id, &anchor, now)
            .await?;
        self.db
            .insert("draft_comments")
            .value("comment_id", comment_id.clone())
            .value("thread_id", thread_id.clone())
            .value("body", body.to_string())
            .value("created_at_ms", u64_to_i64(now))
            .value("updated_at_ms", u64_to_i64(now))
            .execute(self.db)
            .await?;
        Ok(DraftComment {
            comment_id,
            thread_id,
            anchor,
            body: body.to_string(),
            created_at_ms: now,
            updated_at_ms: now,
            session_id: None,
            resolved_at_ms: None,
            thread_kind: ReviewThreadKind::Note,
            severity: ReviewThreadSeverity::Info,
        })
    }

    pub async fn delete_draft(&self, comment_id: &str) -> Result<bool, StoreError> {
        let Some(row) = self
            .db
            .select("draft_comments")
            .columns(&["thread_id"])
            .filter(Box::new(where_eq("comment_id", comment_id)))
            .execute_first(self.db)
            .await?
        else {
            return Ok(false);
        };
        let thread_id = required_text(&row, "thread_id")?;
        self.db
            .delete("draft_comments")
            .filter(Box::new(where_eq("comment_id", comment_id)))
            .execute(self.db)
            .await?;
        let remaining = self
            .db
            .select("draft_comments")
            .columns(&["comment_id"])
            .filter(Box::new(where_eq("thread_id", thread_id.clone())))
            .execute_first(self.db)
            .await?;
        if remaining.is_none() {
            self.db
                .delete("draft_threads")
                .filter(Box::new(where_eq("thread_id", thread_id)))
                .execute(self.db)
                .await?;
        }
        Ok(true)
    }

    pub async fn link_thread_session(
        &self,
        review_key: &str,
        repo_root: &Path,
        target_kind: &str,
        target_json: &str,
        anchor: &DraftAnchor,
        session_id: &str,
    ) -> Result<LinkThreadSessionResponse, StoreError> {
        let now = now_ms();
        let thread_id = thread_id(review_key, anchor)?;
        self.ensure_review(review_key, repo_root, target_kind, target_json, now)
            .await?;
        self.ensure_thread(review_key, &thread_id, anchor, now)
            .await?;
        self.db
            .update("draft_threads")
            .value("session_id", session_id.to_string())
            .value("updated_at_ms", u64_to_i64(now))
            .filter(Box::new(where_eq("thread_id", thread_id.clone())))
            .execute(self.db)
            .await?;
        Ok(LinkThreadSessionResponse { thread_id })
    }

    pub async fn update_draft(
        &self,
        comment_id: &str,
        body: &str,
    ) -> Result<UpdateDraftResponse, StoreError> {
        let exists = self
            .db
            .select("draft_comments")
            .columns(&["comment_id"])
            .filter(Box::new(where_eq("comment_id", comment_id)))
            .execute_first(self.db)
            .await?
            .is_some();
        if !exists {
            return Ok(UpdateDraftResponse {
                updated: false,
                updated_at_ms: None,
            });
        }
        let now = now_ms();
        self.db
            .update("draft_comments")
            .value("body", body.to_string())
            .value("updated_at_ms", u64_to_i64(now))
            .filter(Box::new(where_eq("comment_id", comment_id)))
            .execute(self.db)
            .await?;
        Ok(UpdateDraftResponse {
            updated: true,
            updated_at_ms: Some(now),
        })
    }

    pub async fn resolve_thread(
        &self,
        review_key: &str,
        repo_root: &Path,
        target_kind: &str,
        target_json: &str,
        anchor: &DraftAnchor,
        resolved: bool,
    ) -> Result<ResolveThreadResponse, StoreError> {
        let now = now_ms();
        let thread_id = thread_id(review_key, anchor)?;
        self.ensure_review(review_key, repo_root, target_kind, target_json, now)
            .await?;
        self.ensure_thread(review_key, &thread_id, anchor, now)
            .await?;
        let resolved_at_ms = resolved.then_some(now);
        self.db
            .update("draft_threads")
            .value("resolved_at_ms", resolved_at_ms.map(u64_to_i64))
            .value("updated_at_ms", u64_to_i64(now))
            .filter(Box::new(where_eq("thread_id", thread_id.clone())))
            .execute(self.db)
            .await?;
        Ok(ResolveThreadResponse {
            thread_id,
            resolved_at_ms,
        })
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn classify_thread(
        &self,
        review_key: &str,
        repo_root: &Path,
        target_kind: &str,
        target_json: &str,
        anchor: &DraftAnchor,
        kind: ReviewThreadKind,
        severity: ReviewThreadSeverity,
    ) -> Result<bcode_code_review_models::ClassifyThreadResponse, StoreError> {
        let now = now_ms();
        let thread_id = thread_id(review_key, anchor)?;
        self.ensure_review(review_key, repo_root, target_kind, target_json, now)
            .await?;
        self.ensure_thread(review_key, &thread_id, anchor, now)
            .await?;
        self.db
            .update("draft_threads")
            .value("thread_kind", thread_kind_str(kind))
            .value("severity", thread_severity_str(severity))
            .value("updated_at_ms", u64_to_i64(now))
            .filter(Box::new(where_eq("thread_id", thread_id.clone())))
            .execute(self.db)
            .await?;
        Ok(bcode_code_review_models::ClassifyThreadResponse { thread_id })
    }

    pub async fn ensure_review(
        &self,
        review_key: &str,
        repo_root: &Path,
        target_kind: &str,
        target_json: &str,
        now: u64,
    ) -> Result<(), StoreError> {
        if self
            .db
            .select("reviews")
            .columns(&["review_key"])
            .filter(Box::new(where_eq("review_key", review_key)))
            .execute_first(self.db)
            .await?
            .is_some()
        {
            self.db
                .update("reviews")
                .value("updated_at_ms", u64_to_i64(now))
                .filter(Box::new(where_eq("review_key", review_key)))
                .execute(self.db)
                .await?;
            return Ok(());
        }
        self.db
            .insert("reviews")
            .value("review_key", review_key.to_string())
            .value("repo_root", repo_root.display().to_string())
            .value("target_kind", target_kind.to_string())
            .value("target_json", target_json.to_string())
            .value("created_at_ms", u64_to_i64(now))
            .value("updated_at_ms", u64_to_i64(now))
            .execute(self.db)
            .await?;
        Ok(())
    }

    pub async fn ensure_thread(
        &self,
        review_key: &str,
        thread_id: &str,
        anchor: &DraftAnchor,
        now: u64,
    ) -> Result<(), StoreError> {
        if self
            .db
            .select("draft_threads")
            .columns(&["thread_id"])
            .filter(Box::new(where_eq("thread_id", thread_id)))
            .execute_first(self.db)
            .await?
            .is_some()
        {
            self.db
                .update("draft_threads")
                .value("updated_at_ms", u64_to_i64(now))
                .filter(Box::new(where_eq("thread_id", thread_id)))
                .execute(self.db)
                .await?;
            return Ok(());
        }
        self.db
            .insert("draft_threads")
            .value("thread_id", thread_id.to_string())
            .value("review_key", review_key.to_string())
            .value("anchor_kind", anchor_kind_str(anchor.kind))
            .value("file_path", anchor.file_path.clone())
            .value("diff_row", u64_to_i64(anchor.diff_row))
            .value("start_diff_row", optional_u64(anchor.start_diff_row))
            .value("end_diff_row", optional_u64(anchor.end_diff_row))
            .value("old_start", optional_u32(anchor.old_start))
            .value("old_end", optional_u32(anchor.old_end))
            .value("new_start", optional_u32(anchor.new_start))
            .value("new_end", optional_u32(anchor.new_end))
            .value("old_line", optional_u32(anchor.old_line))
            .value("new_line", optional_u32(anchor.new_line))
            .value("line_kind", line_kind_str(anchor.line_kind))
            .value("thread_kind", thread_kind_str(ReviewThreadKind::Note))
            .value("severity", thread_severity_str(ReviewThreadSeverity::Info))
            .value("is_file_anchor", anchor.is_file_anchor)
            .value("surface_id", anchor.surface_id.clone())
            .value("source_id", anchor.source_id.clone())
            .value("head_oid", anchor.head_oid.clone())
            .value("blob_oid", anchor.blob_oid.clone())
            .value("reply_to", anchor.reply_to.clone())
            .value("resolved_at_ms", Option::<i64>::None)
            .value("created_at_ms", u64_to_i64(now))
            .value("updated_at_ms", u64_to_i64(now))
            .execute(self.db)
            .await?;
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatePaths {
    pub state_root: PathBuf,
    pub database_path: PathBuf,
}

pub fn state_paths(repo_root: &Path, config: &CodeReviewConfig) -> StatePaths {
    let state_root = env::var_os(CODE_REVIEW_STATE_DIR_ENV)
        .map_or_else(|| configured_state_root(repo_root, config), PathBuf::from);
    let database_path = state_root.join(DATABASE_FILE_NAME);
    StatePaths {
        state_root,
        database_path,
    }
}

pub fn configured_state_root(repo_root: &Path, config: &CodeReviewConfig) -> PathBuf {
    if let Some(state_dir) = &config.state_dir {
        if state_dir.is_absolute() {
            return state_dir.clone();
        }
        return repo_root.join(state_dir);
    }
    match config.state_location {
        CodeReviewStateLocation::User => {
            bcode_config::default_state_dir().join(DEFAULT_STATE_SUBDIR)
        }
        CodeReviewStateLocation::Repo => repo_root.join(DEFAULT_REPO_STATE_ROOT),
    }
}

pub fn with_database<T>(
    repo_root: &Path,
    config: &CodeReviewConfig,
    operation: impl for<'a> FnOnce(
        &'a dyn Database,
    ) -> Pin<Box<dyn Future<Output = Result<T, StoreError>> + 'a>>
    + Send
    + 'static,
) -> Result<T, StoreError>
where
    T: Send + 'static,
{
    let paths = state_paths(repo_root, config);
    std::fs::create_dir_all(&paths.state_root)?;
    std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("current-thread Tokio runtime should build");
        runtime.block_on(async {
            let database = open_database(&paths.database_path).await?;
            {
                // Callers in other processes (the app and the daemon's plugin) and other
                // threads of this one share the database; migrating it concurrently would apply
                // the same schema change twice.
                let _migrating = lock_migrations(&paths.state_root)?;
                run_migrations(database.as_ref()).await?;
            }
            operation(database.as_ref()).await
        })
    })
    .join()
    .map_err(|_| StoreError::InvalidRequest("database worker panicked".to_string()))?
}

/// Hold the state directory's migration lock until the returned file is dropped.
///
/// The lock is an exclusive advisory lock on a file beside the database. It excludes every
/// other holder, whether in another process or another thread of this one, because each
/// caller opens its own handle.
///
/// # Errors
///
/// * The lock file cannot be opened or created.
/// * Locking it fails.
pub fn lock_migrations(state_root: &Path) -> Result<std::fs::File, StoreError> {
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(state_root.join(MIGRATION_LOCK_FILE_NAME))?;
    file.lock()?;
    Ok(file)
}

pub async fn open_database(path: &Path) -> Result<Box<dyn Database>, StoreError> {
    let mut attempt = 0_u32;
    let mut delay = DATABASE_OPEN_INITIAL_RETRY_DELAY;
    loop {
        match switchy::database_connection::builder()
            .turso()
            .with_path(path)
            .with_busy_timeout(DATABASE_BUSY_TIMEOUT)
            // Avoid Turso's experimental multi-process WAL sidecars for local plugin state.
            .with_multiprocess_wal(false)
            .build()
            .await
        {
            Ok(db) => return Ok(db),
            Err(error)
                if is_database_lock_error(&error) && attempt < DATABASE_OPEN_RETRY_ATTEMPTS =>
            {
                attempt = attempt.saturating_add(1);
                tokio::time::sleep(delay).await;
                delay = delay.saturating_mul(2).min(DATABASE_OPEN_MAX_RETRY_DELAY);
            }
            Err(error) => return Err(error.into()),
        }
    }
}

pub fn is_database_lock_error(error: &switchy::database_connection::InitTursoError) -> bool {
    let message = error.to_string().to_ascii_lowercase();
    message.contains("locking error")
        || message.contains("failed locking file")
        || message.contains("database is locked")
        || message.contains("busy")
}

pub async fn run_migrations(database: &dyn Database) -> Result<(), StoreError> {
    reconcile_legacy_code_review_migrations(database).await?;
    let runner = MigrationRunner::new(Box::new(code_review_migrations()))
        .with_table_name(MIGRATIONS_TABLE.to_string());
    runner.run(database).await?;
    Ok(())
}

pub async fn reconcile_legacy_code_review_migrations(
    database: &dyn Database,
) -> Result<(), StoreError> {
    let has_draft_threads = database.table_exists("draft_threads").await?;
    let has_review_workspaces = database.table_exists("review_workspaces").await?;
    if !has_draft_threads && !has_review_workspaces {
        return Ok(());
    }

    let tracker = VersionTracker::with_table_name(MIGRATIONS_TABLE.to_string());
    tracker.ensure_table_exists(database).await?;

    if has_draft_threads {
        if database
            .column_exists("draft_threads", "anchor_kind")
            .await?
            && !tracker
                .is_migration_applied(database, "011_thread_anchor_kind_column")
                .await?
        {
            tracker
                .record_migration(database, "011_thread_anchor_kind_column")
                .await?;
        }

        if database
            .column_exists("draft_threads", "thread_kind")
            .await?
            && database.column_exists("draft_threads", "severity").await?
            && !tracker
                .is_migration_applied(database, "012_thread_metadata_columns")
                .await?
        {
            tracker
                .record_migration(database, "012_thread_metadata_columns")
                .await?;
        }

        if database
            .column_exists("draft_threads", "start_diff_row")
            .await?
            && !tracker
                .is_migration_applied(database, "005_thread_range_columns")
                .await?
        {
            tracker
                .record_migration(database, "005_thread_range_columns")
                .await?;
        }

        if database
            .column_exists("draft_threads", "is_file_anchor")
            .await?
            && !tracker
                .is_migration_applied(database, "006_thread_file_anchor_column")
                .await?
        {
            tracker
                .record_migration(database, "006_thread_file_anchor_column")
                .await?;
        }

        if database
            .column_exists("draft_threads", "surface_id")
            .await?
            && !tracker
                .is_migration_applied(database, "007_thread_surface_anchor_columns")
                .await?
        {
            tracker
                .record_migration(database, "007_thread_surface_anchor_columns")
                .await?;
        }
    }

    if has_review_workspaces
        && database
            .column_exists("review_workspaces", "viewed_files_json")
            .await?
        && !tracker
            .is_migration_applied(database, "008_workspace_viewed_files_column")
            .await?
    {
        tracker
            .record_migration(database, "008_workspace_viewed_files_column")
            .await?;
    }

    Ok(())
}

pub fn thread_resolved_column_migration() -> CodeMigration<'static> {
    CodeMigration::new(
        "008_thread_resolved_column".to_string(),
        Box::new(alter_table("draft_threads").add_column(
            "resolved_at_ms".to_string(),
            DataType::BigInt,
            true,
            None,
        )),
        None,
    )
}

pub fn workspace_table_migration() -> CodeMigration<'static> {
    CodeMigration::new(
        "006_review_workspaces_table".to_string(),
        Box::new(
            create_table("review_workspaces")
                .if_not_exists(true)
                .column(text_column("workspace_id"))
                .column(text_column("repo_root"))
                .column(text_column("title"))
                .column(text_column("sources_json"))
                .column(int_column("created_at_ms"))
                .column(int_column("updated_at_ms"))
                .column(nullable_int_column("archived_at_ms"))
                .primary_key("workspace_id"),
        ),
        None,
    )
}

pub fn workspace_viewed_files_column_migration() -> CodeMigration<'static> {
    CodeMigration::new(
        "008_workspace_viewed_files_column".to_string(),
        Box::new(alter_table("review_workspaces").add_column(
            "viewed_files_json".to_string(),
            DataType::Text,
            false,
            Some(DatabaseValue::String("[]".to_string())),
        )),
        None,
    )
}

pub fn publish_records_table_migration() -> CodeMigration<'static> {
    CodeMigration::new(
        "007_review_publish_records_table".to_string(),
        Box::new(
            create_table("review_publish_records")
                .if_not_exists(true)
                .column(text_column("record_id"))
                .column(nullable_text_column("workspace_id"))
                .column(text_column("review_id"))
                .column(text_column("publisher_id"))
                .column(int_column("submitted"))
                .column(nullable_text_column("output"))
                .column(text_column("message"))
                .column(int_column("created_at_ms"))
                .primary_key("record_id"),
        ),
        None,
    )
}

pub fn thread_anchor_kind_column_migration() -> CodeMigration<'static> {
    CodeMigration::new(
        "011_thread_anchor_kind_column".to_string(),
        Box::new(alter_table("draft_threads").add_column(
            "anchor_kind".to_string(),
            DataType::Text,
            false,
            Some(DatabaseValue::String("range".to_string())),
        )),
        None,
    )
}

pub fn thread_metadata_columns_migration() -> CodeMigration<'static> {
    CodeMigration::new(
        "012_thread_metadata_columns".to_string(),
        Box::new(
            alter_table("draft_threads")
                .add_column(
                    "thread_kind".to_string(),
                    DataType::Text,
                    false,
                    Some(DatabaseValue::String("note".to_string())),
                )
                .add_column(
                    "severity".to_string(),
                    DataType::Text,
                    false,
                    Some(DatabaseValue::String("info".to_string())),
                ),
        ),
        None,
    )
}

pub fn code_review_migrations() -> CodeMigrationSource<'static> {
    let mut source = CodeMigrationSource::new();
    source.add_migration(CodeMigration::new(
        "001_reviews_table".to_string(),
        Box::new(
            create_table("reviews")
                .if_not_exists(true)
                .column(text_column("review_key"))
                .column(text_column("repo_root"))
                .column(text_column("target_kind"))
                .column(text_column("target_json"))
                .column(int_column("created_at_ms"))
                .column(int_column("updated_at_ms"))
                .primary_key("review_key"),
        ),
        None,
    ));
    source.add_migration(CodeMigration::new(
        "002_draft_threads_table".to_string(),
        Box::new(
            create_table("draft_threads")
                .if_not_exists(true)
                .column(text_column("thread_id"))
                .column(text_column("review_key"))
                .column(text_column("file_path"))
                .column(int_column("diff_row"))
                .column(nullable_int_column("old_line"))
                .column(nullable_int_column("new_line"))
                .column(text_column("line_kind"))
                .column(int_column("created_at_ms"))
                .column(int_column("updated_at_ms"))
                .primary_key("thread_id"),
        ),
        None,
    ));
    source.add_migration(CodeMigration::new(
        "003_draft_comments_table".to_string(),
        Box::new(
            create_table("draft_comments")
                .if_not_exists(true)
                .column(text_column("comment_id"))
                .column(text_column("thread_id"))
                .column(text_column("body"))
                .column(int_column("created_at_ms"))
                .column(int_column("updated_at_ms"))
                .primary_key("comment_id"),
        ),
        None,
    ));
    source.add_migration(CodeMigration::new(
        "004_thread_session_column".to_string(),
        Box::new(alter_table("draft_threads").add_column(
            "session_id".to_string(),
            DataType::Text,
            true,
            None,
        )),
        None,
    ));
    source.add_migration(CodeMigration::new(
        "005_thread_range_columns".to_string(),
        Box::new(
            alter_table("draft_threads")
                .add_column("start_diff_row".to_string(), DataType::BigInt, true, None)
                .add_column("end_diff_row".to_string(), DataType::BigInt, true, None)
                .add_column("old_start".to_string(), DataType::BigInt, true, None)
                .add_column("old_end".to_string(), DataType::BigInt, true, None)
                .add_column("new_start".to_string(), DataType::BigInt, true, None)
                .add_column("new_end".to_string(), DataType::BigInt, true, None),
        ),
        None,
    ));
    source.add_migration(CodeMigration::new(
        "006_thread_file_anchor_column".to_string(),
        Box::new(alter_table("draft_threads").add_column(
            "is_file_anchor".to_string(),
            DataType::Bool,
            false,
            Some(DatabaseValue::Bool(false)),
        )),
        None,
    ));
    source.add_migration(thread_surface_anchor_columns_migration());
    source.add_migration(thread_resolved_column_migration());
    source.add_migration(workspace_table_migration());
    source.add_migration(workspace_viewed_files_column_migration());
    source.add_migration(publish_records_table_migration());
    source.add_migration(thread_anchor_kind_column_migration());
    source.add_migration(thread_metadata_columns_migration());
    source.add_migration(review_suggestions_table_migration());
    source.add_migration(publish_record_thread_anchors_migration());
    source.add_migration(external_review_imports_table_migration());
    source.add_migration(review_ai_exchanges_table_migration());
    add_later_migrations(&mut source);
    source
}

/// Migrations from 016 on, in order.
fn add_later_migrations(source: &mut CodeMigrationSource<'static>) {
    source.add_migration(workspace_presentation_state_column_migration());
    source.add_migration(workspace_state_columns_migration());
    source.add_migration(thread_head_oid_column_migration());
    source.add_migration(thread_blob_oid_column_migration());
    source.add_migration(review_assistant_sessions_table_migration());
    source.add_migration(workspace_stack_snapshots_column_migration());
    source.add_migration(super::history::layer_versions_table_migration());
    source.add_migration(thread_reply_to_column_migration());
}

/// The pull request thread a draft thread replies to (see `DraftAnchor::reply_to`).
pub fn thread_reply_to_column_migration() -> CodeMigration<'static> {
    CodeMigration::new(
        "024_thread_reply_to_column".to_string(),
        Box::new(alter_table("draft_threads").add_column(
            "reply_to".to_string(),
            DataType::Text,
            true,
            None,
        )),
        None,
    )
}

/// The stacks a review last opened with (see `ReviewWorkspace::stack_snapshots`).
pub fn workspace_stack_snapshots_column_migration() -> CodeMigration<'static> {
    CodeMigration::new(
        "022_workspace_stack_snapshots_column".to_string(),
        Box::new(alter_table("review_workspaces").add_column(
            "stack_snapshots_json".to_string(),
            DataType::Text,
            true,
            None,
        )),
        None,
    )
}

/// Sessions bound to the review they assist, with the binding as recorded.
pub fn review_assistant_sessions_table_migration() -> CodeMigration<'static> {
    CodeMigration::new(
        "021_review_assistant_sessions_table".to_string(),
        Box::new(
            create_table("review_assistant_sessions")
                .if_not_exists(true)
                .column(text_column("session_id"))
                .column(text_column("workspace_id"))
                .column(text_column("repo_root"))
                .column(text_column("binding_json"))
                .column(int_column("created_at_ms"))
                .column(int_column("updated_at_ms"))
                .primary_key("session_id"),
        ),
        None,
    )
}

/// Line anchors record the blob of the anchored file side
/// (`DraftAnchor::blob_oid`). Nullable, so threads written before the migration read as having
/// no recorded blob.
pub fn thread_blob_oid_column_migration() -> CodeMigration<'static> {
    CodeMigration::new(
        "020_thread_blob_oid_column".to_string(),
        Box::new(alter_table("draft_threads").add_column(
            "blob_oid".to_string(),
            DataType::Text,
            true,
            None,
        )),
        None,
    )
}

/// Anchors record the head commit they were made at (`DraftAnchor::head_oid`).
/// Nullable, so threads written before the migration read as having no recorded head.
pub fn thread_head_oid_column_migration() -> CodeMigration<'static> {
    CodeMigration::new(
        "019_thread_head_oid_column".to_string(),
        Box::new(alter_table("draft_threads").add_column(
            "head_oid".to_string(),
            DataType::Text,
            true,
            None,
        )),
        None,
    )
}

/// Workspace review state: identity-pinned viewed marks, reviewed revisions per source, and
/// presentation state. All columns are nullable JSON text, so older rows read as empty.
pub fn workspace_state_columns_migration() -> CodeMigration<'static> {
    CodeMigration::new(
        "018_workspace_v2_state_columns".to_string(),
        Box::new(
            alter_table("review_workspaces")
                .add_column("viewed_marks_json".to_string(), DataType::Text, true, None)
                .add_column(
                    "reviewed_revisions_json".to_string(),
                    DataType::Text,
                    true,
                    None,
                )
                .add_column(
                    "presentation_v2_json".to_string(),
                    DataType::Text,
                    true,
                    None,
                ),
        ),
        None,
    )
}

/// A retired presentation state column. The column stays so the migration history is
/// unchanged; nothing reads or writes it (presentation state lives in `presentation_v2_json`).
pub fn workspace_presentation_state_column_migration() -> CodeMigration<'static> {
    CodeMigration::new(
        "017_workspace_presentation_state_column".to_string(),
        Box::new(alter_table("review_workspaces").add_column(
            "presentation_state_json".to_string(),
            DataType::Text,
            true,
            None,
        )),
        None,
    )
}

pub fn review_ai_exchanges_table_migration() -> CodeMigration<'static> {
    CodeMigration::new(
        "016_review_ai_exchanges_table".to_string(),
        Box::new(
            create_table("review_ai_exchanges")
                .if_not_exists(true)
                .column(int_column("schema_version"))
                .column(text_column("exchange_id"))
                .column(text_column("review_key"))
                .column(text_column("anchor_json"))
                .column(text_column("question"))
                .column(nullable_text_column("session_id"))
                .column(text_column("status"))
                .column(nullable_text_column("error"))
                .column(int_column("created_at_ms"))
                .column(int_column("updated_at_ms"))
                .primary_key("exchange_id"),
        ),
        None,
    )
}

pub fn external_review_imports_table_migration() -> CodeMigration<'static> {
    CodeMigration::new(
        "015_external_review_imports_table".to_string(),
        Box::new(
            create_table("review_external_imports")
                .if_not_exists(true)
                .column(text_column("workspace_id"))
                .column(text_column("importer_id"))
                .column(text_column("threads_json"))
                .column(text_column("warnings_json"))
                .column(int_column("updated_at_ms"))
                .primary_key("workspace_id")
                .primary_key("importer_id"),
        ),
        None,
    )
}

pub fn publish_record_thread_anchors_migration() -> CodeMigration<'static> {
    CodeMigration::new(
        "014_publish_record_thread_anchors".to_string(),
        Box::new(alter_table("review_publish_records").add_column(
            "published_thread_anchors_json".to_string(),
            DataType::Text,
            false,
            Some(DatabaseValue::String("[]".to_string())),
        )),
        None,
    )
}

pub fn review_suggestions_table_migration() -> CodeMigration<'static> {
    CodeMigration::new(
        "013_review_suggestions_table".to_string(),
        Box::new(
            create_table("review_suggestions")
                .if_not_exists(true)
                .column(text_column("suggestion_id"))
                .column(text_column("review_key"))
                .column(text_column("anchor_json"))
                .column(text_column("body"))
                .column(nullable_text_column("session_id"))
                .column(text_column("status"))
                .column(nullable_text_column("rationale"))
                .column(int_column("created_at_ms"))
                .column(int_column("updated_at_ms"))
                .primary_key("suggestion_id"),
        ),
        None,
    )
}

pub fn thread_surface_anchor_columns_migration() -> CodeMigration<'static> {
    CodeMigration::new(
        "007_thread_surface_anchor_columns".to_string(),
        Box::new(
            alter_table("draft_threads")
                .add_column("surface_id".to_string(), DataType::Text, true, None)
                .add_column("source_id".to_string(), DataType::Text, true, None),
        ),
        None,
    )
}

pub fn text_column(name: &str) -> Column {
    Column {
        name: name.to_string(),
        nullable: false,
        auto_increment: false,
        data_type: DataType::Text,
        default: None,
    }
}

pub fn nullable_text_column(name: &str) -> Column {
    Column {
        name: name.to_string(),
        nullable: true,
        auto_increment: false,
        data_type: DataType::Text,
        default: None,
    }
}

pub fn int_column(name: &str) -> Column {
    Column {
        name: name.to_string(),
        nullable: false,
        auto_increment: false,
        data_type: DataType::BigInt,
        default: None,
    }
}

pub fn nullable_int_column(name: &str) -> Column {
    Column {
        name: name.to_string(),
        nullable: true,
        auto_increment: false,
        data_type: DataType::BigInt,
        default: None,
    }
}

pub fn external_import_from_row(row: &Row) -> Result<ExternalReviewImportRecord, StoreError> {
    Ok(ExternalReviewImportRecord {
        workspace_id: required_text(row, "workspace_id")?,
        importer_id: required_text(row, "importer_id")?,
        threads: serde_json::from_str(&required_text(row, "threads_json")?)?,
        warnings: serde_json::from_str(&required_text(row, "warnings_json")?)?,
        updated_at_ms: i64_to_u64(required_i64(row, "updated_at_ms")?),
    })
}

pub fn publish_record_from_row(row: &Row) -> Result<ReviewPublishRecord, StoreError> {
    Ok(ReviewPublishRecord {
        id: required_text(row, "record_id")?,
        workspace_id: optional_text(row, "workspace_id"),
        review_id: required_text(row, "review_id")?,
        publisher_id: required_text(row, "publisher_id")?,
        submitted: required_i64(row, "submitted")? != 0,
        published_thread_anchors: optional_text(row, "published_thread_anchors_json")
            .map_or_else(|| Ok(Vec::new()), |json| serde_json::from_str(&json))?,
        output: optional_text(row, "output"),
        message: required_text(row, "message")?,
        created_at_ms: i64_to_u64(required_i64(row, "created_at_ms")?),
    })
}

pub fn ai_exchange_from_row(row: &Row) -> Result<ReviewAiExchange, StoreError> {
    let schema_version = u16::try_from(required_i64(row, "schema_version")?).map_err(|_| {
        StoreError::InvalidRequest("review AI exchange schema version is out of range".to_string())
    })?;
    if schema_version != bcode_code_review_models::REVIEW_AI_EXCHANGE_SCHEMA_VERSION {
        return Err(StoreError::InvalidRequest(format!(
            "unsupported review AI exchange schema version: {schema_version}"
        )));
    }
    Ok(ReviewAiExchange {
        schema_version,
        exchange_id: required_text(row, "exchange_id")?,
        anchor: serde_json::from_str(&required_text(row, "anchor_json")?)?,
        question: required_text(row, "question")?,
        session_id: optional_text(row, "session_id"),
        status: ai_exchange_status_from_str(&required_text(row, "status")?)?,
        error: optional_text(row, "error"),
        created_at_ms: i64_to_u64(required_i64(row, "created_at_ms")?),
        updated_at_ms: i64_to_u64(required_i64(row, "updated_at_ms")?),
    })
}

pub const fn ai_exchange_status_to_str(status: ReviewAiExchangeStatus) -> &'static str {
    match status {
        ReviewAiExchangeStatus::Pending => "pending",
        ReviewAiExchangeStatus::Linked => "linked",
        ReviewAiExchangeStatus::Failed => "failed",
    }
}

pub fn ai_exchange_status_from_str(value: &str) -> Result<ReviewAiExchangeStatus, StoreError> {
    match value {
        "pending" => Ok(ReviewAiExchangeStatus::Pending),
        "linked" => Ok(ReviewAiExchangeStatus::Linked),
        "failed" => Ok(ReviewAiExchangeStatus::Failed),
        _ => Err(StoreError::InvalidRequest(format!(
            "unsupported review AI exchange status: {value}"
        ))),
    }
}

/// A stored binding, or `None` when it is unreadable or of a schema version this version does
/// not know (it is left in place for the version that wrote it).
pub fn assistant_binding_from_row(row: &Row) -> Option<ReviewAssistantBinding> {
    let json = optional_text(row, "binding_json")?;
    serde_json::from_str::<ReviewAssistantBinding>(&json)
        .ok()
        .filter(|binding| {
            binding.schema_version
                == bcode_code_review_models::REVIEW_ASSISTANT_BINDING_SCHEMA_VERSION
        })
}

pub fn suggestion_from_row(row: &Row) -> Result<ReviewSuggestion, StoreError> {
    Ok(ReviewSuggestion {
        suggestion_id: required_text(row, "suggestion_id")?,
        anchor: serde_json::from_str(&required_text(row, "anchor_json")?)?,
        body: required_text(row, "body")?,
        session_id: optional_text(row, "session_id"),
        status: suggestion_status_from_str(&required_text(row, "status")?)?,
        rationale: optional_text(row, "rationale"),
        created_at_ms: i64_to_u64(required_i64(row, "created_at_ms")?),
        updated_at_ms: i64_to_u64(required_i64(row, "updated_at_ms")?),
    })
}

pub const fn suggestion_status_to_str(status: ReviewSuggestionStatus) -> &'static str {
    match status {
        ReviewSuggestionStatus::Suggested => "suggested",
        ReviewSuggestionStatus::Refining => "refining",
        ReviewSuggestionStatus::Accepted => "accepted",
        ReviewSuggestionStatus::Rejected => "rejected",
    }
}

pub fn suggestion_status_from_str(value: &str) -> Result<ReviewSuggestionStatus, StoreError> {
    match value {
        "suggested" => Ok(ReviewSuggestionStatus::Suggested),
        "refining" => Ok(ReviewSuggestionStatus::Refining),
        "accepted" => Ok(ReviewSuggestionStatus::Accepted),
        "rejected" => Ok(ReviewSuggestionStatus::Rejected),
        _ => Err(StoreError::InvalidRequest(format!(
            "unsupported review suggestion status: {value}"
        ))),
    }
}

pub fn workspace_from_row(row: &Row) -> Result<ReviewWorkspace, StoreError> {
    let sources: Vec<ReviewSource> = serde_json::from_str(&required_text(row, "sources_json")?)?;
    let viewed_files = optional_text(row, "viewed_files_json")
        .map_or_else(|| Ok(BTreeSet::new()), |json| serde_json::from_str(&json))?;
    Ok(ReviewWorkspace {
        id: required_text(row, "workspace_id")?,
        title: required_text(row, "title")?,
        repo_root: PathBuf::from(required_text(row, "repo_root")?),
        sources,
        created_at_ms: Some(i64_to_u64(required_i64(row, "created_at_ms")?)),
        updated_at_ms: Some(i64_to_u64(required_i64(row, "updated_at_ms")?)),
        viewed_files,
        archived_at_ms: optional_i64(row, "archived_at_ms").map(i64_to_u64),
        viewed_marks: optional_text(row, "viewed_marks_json")
            .map_or_else(|| Ok(Vec::new()), |json| serde_json::from_str(&json))?,
        reviewed_revisions: optional_text(row, "reviewed_revisions_json")
            .map_or_else(|| Ok(BTreeMap::new()), |json| serde_json::from_str(&json))?,
        // Presentation state is disposable: an unreadable value, or one written by a newer
        // presentation schema, is dropped instead of failing the workspace or being guessed at.
        presentation: optional_text(row, "presentation_v2_json")
            .and_then(|json| {
                serde_json::from_str::<bcode_code_review_models::ReviewPresentationState>(&json)
                    .ok()
            })
            .filter(|state| {
                state.schema_version <= bcode_code_review_models::REVIEW_PRESENTATION_SCHEMA_VERSION
            }),
        // Disposable too: an unreadable value is dropped.
        stack_snapshots: optional_text(row, "stack_snapshots_json")
            .and_then(|json| serde_json::from_str(&json).ok())
            .unwrap_or_default(),
    })
}

pub fn comment_from_row(
    row: &Row,
    thread_id: &str,
    anchor: &DraftAnchor,
    session_id: Option<String>,
    resolved_at_ms: Option<u64>,
    thread_kind: ReviewThreadKind,
    severity: ReviewThreadSeverity,
) -> Result<DraftComment, StoreError> {
    Ok(DraftComment {
        comment_id: required_text(row, "comment_id")?,
        thread_id: thread_id.to_string(),
        anchor: anchor.clone(),
        body: required_text(row, "body")?,
        created_at_ms: i64_to_u64(required_i64(row, "created_at_ms")?),
        updated_at_ms: i64_to_u64(required_i64(row, "updated_at_ms")?),
        session_id,
        resolved_at_ms,
        thread_kind,
        severity,
    })
}

pub fn required_text(row: &Row, column: &'static str) -> Result<String, StoreError> {
    row.get(column)
        .and_then(|value| value.as_str().map(ToString::to_string))
        .ok_or(StoreError::MissingColumn(column))
}

pub fn optional_text(row: &Row, column: &'static str) -> Option<String> {
    row.get(column)
        .and_then(|value| value.as_str().map(ToString::to_string))
}

pub fn required_i64(row: &Row, column: &'static str) -> Result<i64, StoreError> {
    row.get(column)
        .and_then(|value| value.as_i64())
        .ok_or(StoreError::MissingColumn(column))
}

pub fn optional_i64(row: &Row, column: &'static str) -> Option<i64> {
    row.get(column).and_then(|value| value.as_i64())
}

pub fn optional_bool(row: &Row, column: &'static str) -> bool {
    row.get(column)
        .and_then(|value| value.as_bool())
        .unwrap_or(false)
}

pub fn surface_id(source_id: &str, path: &str, kind: ReviewSurfaceKind) -> String {
    let mut hasher = Sha256::new();
    hasher.update(source_id.as_bytes());
    hasher.update(b"\0");
    hasher.update(path.as_bytes());
    hasher.update(b"\0");
    hasher.update(format!("{kind:?}").as_bytes());
    format!("surface-{}", hex::encode(hasher.finalize()))
}

pub fn workspace_id(repo_root: &Path, title: &str, now: u64) -> String {
    let mut hasher = Sha256::new();
    hasher.update(repo_root.display().to_string().as_bytes());
    hasher.update(b"\0");
    hasher.update(title.as_bytes());
    hasher.update(b"\0");
    hasher.update(now.to_string().as_bytes());
    format!("workspace-{}", hex::encode(hasher.finalize()))
}

/// Review key of a workspace's annotations: `review-workspace-` plus its annotation scope id
/// (its id when durable; a per-target id when transient).
pub fn workspace_review_key(workspace: &ReviewWorkspace) -> String {
    format!(
        "review-workspace-{}",
        crate::scope::annotation_workspace_id(workspace)
    )
}

pub fn review_key_for_scope(
    repo_root: &Path,
    target: &ReviewTarget,
    scope: Option<&ReviewScope>,
) -> Result<String, StoreError> {
    match scope {
        Some(ReviewScope::Workspace { workspace_id, .. }) => {
            Ok(format!("review-workspace-{workspace_id}"))
        }
        Some(ReviewScope::Target { target }) => review_key(repo_root, target),
        None => review_key(repo_root, target),
    }
}

pub fn review_key(repo_root: &Path, target: &ReviewTarget) -> Result<String, StoreError> {
    let mut hasher = Sha256::new();
    hasher.update(repo_root.display().to_string().as_bytes());
    hasher.update(b"\0");
    hasher.update(serde_json::to_string(target)?.as_bytes());
    Ok(format!("review-{}", hex::encode(hasher.finalize())))
}

/// Thread identity: the review and the anchor's location. The anchor's head commit and blob are
/// not part of it, so a thread keeps its identity however its anchor was recorded (and anchors
/// without them hash exactly as they did before the fields existed).
pub fn thread_id(review_key: &str, anchor: &DraftAnchor) -> Result<String, StoreError> {
    let mut location = anchor.clone();
    location.head_oid = None;
    location.blob_oid = None;
    let mut hasher = Sha256::new();
    hasher.update(review_key.as_bytes());
    hasher.update(b"\0");
    hasher.update(serde_json::to_string(&location)?.as_bytes());
    Ok(format!("thread-{}", hex::encode(hasher.finalize())))
}

pub fn comment_id(thread_id: &str, body: &str, now: u64) -> String {
    let mut hasher = Sha256::new();
    hasher.update(thread_id.as_bytes());
    hasher.update(b"\0");
    hasher.update(body.as_bytes());
    hasher.update(b"\0");
    hasher.update(now.to_string().as_bytes());
    format!("comment-{}", hex::encode(hasher.finalize()))
}

pub const fn target_kind(target: &ReviewTarget) -> &'static str {
    match target {
        ReviewTarget::WorkingTreeUnstaged => "working_tree_unstaged",
        ReviewTarget::IndexStaged => "index_staged",
        ReviewTarget::WorkingTreeAndIndex => "working_tree_and_index",
        ReviewTarget::LastCommit => "last_commit",
        ReviewTarget::CommitRange { .. } => "commit_range",
        ReviewTarget::BranchCompare { .. } => "branch_compare",
        ReviewTarget::Repository => "repository",
    }
}

pub const fn thread_kind_str(kind: ReviewThreadKind) -> &'static str {
    match kind {
        ReviewThreadKind::Note => "note",
        ReviewThreadKind::Question => "question",
        ReviewThreadKind::Todo => "todo",
        ReviewThreadKind::Finding => "finding",
        ReviewThreadKind::Summary => "summary",
    }
}

pub fn thread_kind_from_str(value: &str) -> Result<ReviewThreadKind, StoreError> {
    match value {
        "note" => Ok(ReviewThreadKind::Note),
        "question" => Ok(ReviewThreadKind::Question),
        "todo" => Ok(ReviewThreadKind::Todo),
        "finding" => Ok(ReviewThreadKind::Finding),
        "summary" => Ok(ReviewThreadKind::Summary),
        _ => Err(StoreError::InvalidRequest(format!(
            "unknown thread kind: {value}"
        ))),
    }
}

pub const fn thread_severity_str(severity: ReviewThreadSeverity) -> &'static str {
    match severity {
        ReviewThreadSeverity::Info => "info",
        ReviewThreadSeverity::Nit => "nit",
        ReviewThreadSeverity::Warning => "warning",
        ReviewThreadSeverity::Blocker => "blocker",
    }
}

pub fn thread_severity_from_str(value: &str) -> Result<ReviewThreadSeverity, StoreError> {
    match value {
        "info" => Ok(ReviewThreadSeverity::Info),
        "nit" => Ok(ReviewThreadSeverity::Nit),
        "warning" => Ok(ReviewThreadSeverity::Warning),
        "blocker" => Ok(ReviewThreadSeverity::Blocker),
        _ => Err(StoreError::InvalidRequest(format!(
            "unknown thread severity: {value}"
        ))),
    }
}

pub const fn anchor_kind_str(kind: ReviewAnchorKind) -> &'static str {
    match kind {
        ReviewAnchorKind::Review => "review",
        ReviewAnchorKind::File => "file",
        ReviewAnchorKind::Range => "range",
    }
}

pub fn anchor_kind_from_str(value: &str) -> Result<ReviewAnchorKind, StoreError> {
    match value {
        "review" => Ok(ReviewAnchorKind::Review),
        "file" => Ok(ReviewAnchorKind::File),
        "range" => Ok(ReviewAnchorKind::Range),
        _ => Err(StoreError::InvalidRequest(format!(
            "unknown anchor kind: {value}"
        ))),
    }
}

pub const fn line_kind_str(kind: ReviewLineKind) -> &'static str {
    match kind {
        ReviewLineKind::Context => "context",
        ReviewLineKind::Added => "added",
        ReviewLineKind::Removed => "removed",
    }
}

pub fn line_kind_from_str(value: &str) -> Result<ReviewLineKind, StoreError> {
    match value {
        "context" => Ok(ReviewLineKind::Context),
        "added" => Ok(ReviewLineKind::Added),
        "removed" => Ok(ReviewLineKind::Removed),
        _ => Err(StoreError::InvalidRequest(format!(
            "unknown line kind: {value}"
        ))),
    }
}

pub fn optional_u32(value: Option<u32>) -> DatabaseValue {
    DatabaseValue::Int64Opt(value.map(i64::from))
}

pub fn optional_u64(value: Option<u64>) -> DatabaseValue {
    DatabaseValue::Int64Opt(value.map(u64_to_i64))
}

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| {
            u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
        })
}

pub fn u64_to_i64(value: u64) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

pub fn i64_to_u64(value: i64) -> u64 {
    u64::try_from(value).unwrap_or(0)
}

pub fn i64_to_u32(value: i64) -> u32 {
    u32::try_from(value).unwrap_or(0)
}
