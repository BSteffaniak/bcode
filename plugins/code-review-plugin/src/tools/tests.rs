//! Review tools driven through the plugin entry point, as the daemon calls them: listed,
//! prepared with the host's workspace context, and invoked with the preparation's descriptor.

use bcode_code_review::testing::fixture::{FixtureSpec, GitFixture};
use bcode_code_review::{CodeReviewConfig, ReviewData, ReviewStore};
use bcode_code_review_models::{
    BindReviewAssistantRequest, GetReviewWorkspaceRequest, ListDraftsRequest,
    REVIEW_ASSISTANT_BINDING_SCHEMA_VERSION, ResolveReviewSourcesRequest, ReviewAssistantBinding,
    ReviewFileHunksRequest, ReviewLineKind, ReviewTarget, ReviewWorkspace,
};
use bcode_plugin_sdk::prelude::{RustPlugin as _, ServiceResponse};
use bcode_tool::{
    OP_INVOKE_TOOL, OP_LIST_TOOLS, OP_PREPARE_TOOL, TOOL_SERVICE_INTERFACE_ID,
    ToolInvocationResponse, ToolList, ToolPreparationResponse,
};
use serde_json::{Value, json};

struct Harness {
    fixture: GitFixture,
    state: tempfile::TempDir,
    plugin: crate::CodeReviewPlugin,
}

impl Harness {
    fn new() -> Self {
        Self {
            fixture: GitFixture::generate(FixtureSpec::small()),
            state: tempfile::tempdir().expect("state"),
            plugin: crate::CodeReviewPlugin::default(),
        }
    }

    fn config(&self) -> Value {
        json!({ "state_dir": self.state.path() })
    }

    fn store(&self) -> ReviewStore {
        ReviewStore::new(CodeReviewConfig {
            state_dir: Some(self.state.path().to_path_buf()),
            ..CodeReviewConfig::default()
        })
    }

    fn service(&mut self, operation: &str, payload: &Value) -> ServiceResponse {
        let config = self.config();
        let context = serde_json::from_value(json!({
            "plugin_id": "bcode.code_review",
            "request": {
                "interface_id": TOOL_SERVICE_INTERFACE_ID,
                "operation": operation,
                "payload": serde_json::to_vec(payload).expect("payload"),
            },
            "config": { "config": config },
        }))
        .expect("context");
        self.plugin.invoke_service(context)
    }

    fn prepare(&mut self, tool: &str, arguments: &Value, session: &str) -> ToolPreparationResponse {
        let root = self.fixture.root().to_path_buf();
        let response = self.service(
            OP_PREPARE_TOOL,
            &json!({
                "invocation": {"invocation_id": "call-1", "tool_name": tool, "arguments": arguments},
                "host_context": [{
                    "schema": bcode_tool::TOOL_WORKSPACE_CONTEXT_SCHEMA,
                    "schema_version": bcode_tool::TOOL_WORKSPACE_CONTEXT_SCHEMA_VERSION,
                    "payload": {"working_directory": root, "workspace_id": session},
                }],
            }),
        );
        assert!(response.error.is_none(), "{tool}: {:?}", response.error);
        response.payload_json().expect("preparation")
    }

    /// Prepare and invoke `tool` as `session` does.
    fn call(&mut self, tool: &str, arguments: &Value, session: &str) -> ToolInvocationResponse {
        let prepared = self.prepare(tool, arguments, session);
        let response = self.service(
            OP_INVOKE_TOOL,
            &json!({
                "tool_call_id": "call-1",
                "name": tool,
                "arguments": arguments,
                "preparation_descriptor": prepared.descriptor,
            }),
        );
        assert!(response.error.is_none(), "{tool}: {:?}", response.error);
        response.payload_json().expect("invocation")
    }

    fn ok(&mut self, tool: &str, arguments: &Value, session: &str) -> String {
        let response = self.call(tool, arguments, session);
        assert!(!response.is_error, "{tool}: {}", response.output);
        response.output
    }

    fn json(&mut self, tool: &str, arguments: &Value, session: &str) -> Value {
        let output = self.ok(tool, arguments, session);
        serde_json::from_str(&output).unwrap_or_else(|error| panic!("{tool}: {error}: {output}"))
    }

    /// The transient last-commit review the app would show, bound to `session`.
    fn bind(&self, session: &str) -> ReviewWorkspace {
        let workspace = bcode_code_review::scope::transient_workspace(
            self.fixture.root().to_path_buf(),
            None,
            vec![bcode_code_review_models::ReviewSourceKind::LastCommit],
        );
        let resolved = ReviewData::default()
            .resolve_sources(&ResolveReviewSourcesRequest {
                repo_path: self.fixture.root().to_path_buf(),
                sources: workspace.sources.clone(),
                provider_resolutions: Vec::new(),
            })
            .expect("resolve");
        self.store()
            .bind_assistant(BindReviewAssistantRequest {
                repo_path: self.fixture.root().to_path_buf(),
                binding: ReviewAssistantBinding {
                    schema_version: REVIEW_ASSISTANT_BINDING_SCHEMA_VERSION,
                    session_id: session.to_owned(),
                    workspace: workspace.clone(),
                    resolved: resolved.sources,
                    stacks: resolved.stacks,
                    created_at_ms: 0,
                    updated_at_ms: 0,
                },
            })
            .expect("bind");
        workspace
    }

    /// The first added line of `path` in the last commit.
    fn added_line(&self, path: &str) -> u32 {
        let mut data = ReviewData::default();
        let resolved = data
            .resolve_sources(&ResolveReviewSourcesRequest {
                repo_path: self.fixture.root().to_path_buf(),
                sources: ReviewWorkspace::from_target(
                    self.fixture.root().to_path_buf(),
                    ReviewTarget::LastCommit,
                )
                .sources,
                provider_resolutions: Vec::new(),
            })
            .expect("resolve");
        let hunks = data
            .file_hunks_get(&ReviewFileHunksRequest {
                repo_path: self.fixture.root().to_path_buf(),
                source: resolved.sources[0].clone(),
                path: path.to_owned(),
                old_path: None,
                context_lines: Some(0),
                ignore_whitespace: false,
                max_lines: None,
            })
            .expect("hunks");
        hunks
            .hunks
            .iter()
            .flat_map(|hunk| &hunk.lines)
            .find(|line| line.kind == ReviewLineKind::Added)
            .and_then(|line| line.new_line)
            .expect("an added line")
    }
}

const SESSION: &str = "11111111-2222-4333-8444-555555555555";

#[test]
fn reads_are_read_only_and_changes_ask_for_permission() {
    let mut harness = Harness::new();
    let response = harness.service(OP_LIST_TOOLS, &json!({}));
    let list: ToolList = response.payload_json().expect("list");
    let names: Vec<&str> = list.tools.iter().map(|tool| tool.name.as_str()).collect();
    for name in [
        "code_review.list_reviews",
        "code_review.summary",
        "code_review.list_files",
        "code_review.file_diff",
        "code_review.file_content",
        "code_review.list_threads",
        "code_review.pull_request",
        "code_review.navigate",
        "code_review.add_comment",
        "code_review.reply",
        "code_review.resolve_thread",
        "code_review.mark_viewed",
        "code_review.suggest_change",
        "code_review.publish",
    ] {
        assert!(names.contains(&name), "{name} is listed");
        let read_only = list.discovery.get(name).expect("discovery").read_only;
        let prepared = harness.prepare(name, &json!({}), SESSION);
        let metadata = &prepared.authorization[0].metadata;
        let expected = match name {
            "code_review.add_comment"
            | "code_review.reply"
            | "code_review.resolve_thread"
            | "code_review.mark_viewed"
            | "code_review.suggest_change" => ("mutating", true, "code_review.write"),
            "code_review.publish" => ("mutating", true, "code_review.publish"),
            _ => ("read_only", false, "code_review.read"),
        };
        assert_eq!(metadata["operation"]["kind"], expected.0, "{name}");
        assert_eq!(metadata["requires_permission"], expected.1, "{name}");
        assert_eq!(metadata["permission_category"], expected.2, "{name}");
        assert_eq!(read_only, expected.0 == "read_only", "{name}");
        assert_eq!(prepared.descriptor["session_id"], SESSION, "{name}");
    }
}

#[test]
fn a_bound_session_reads_the_review_it_assists() {
    let mut harness = Harness::new();
    harness.bind(SESSION);
    let path = GitFixture::modified_path(0);

    let summary = harness.json("code_review.summary", &json!({}), SESSION);
    assert!(
        summary["files"].as_u64().unwrap_or_default() > 0,
        "{summary}"
    );
    assert_eq!(summary["id"], Value::Null, "an unsaved review has no id");

    let files = harness.json("code_review.list_files", &json!({}), SESSION);
    assert!(
        files["files"]
            .as_array()
            .expect("files")
            .iter()
            .any(|file| file["path"] == path.as_str() && file["viewed"] == false),
        "{files}"
    );

    let line = harness.added_line(&path);
    let diff = harness.ok("code_review.file_diff", &json!({"path": path}), SESSION);
    assert!(diff.contains("@@ -"), "{diff}");
    let number = line.to_string();
    assert!(
        diff.lines().any(|row| {
            let mut columns = row.split_whitespace();
            columns.next() == Some(number.as_str())
                && columns.next().is_some_and(|marked| marked.starts_with('+'))
        }),
        "line {line} is numbered as added: {diff}"
    );
    let content = harness.ok(
        "code_review.file_content",
        &json!({"path": path, "start_line": line, "count": 1}),
        SESSION,
    );
    assert!(
        content.contains(&format!("lines {line}-{line}")),
        "{content}"
    );

    let missing = harness.call(
        "code_review.file_diff",
        &json!({"path": "no/such/file.rs"}),
        SESSION,
    );
    assert!(missing.is_error && missing.output.contains("not_found"));

    let unbound = harness.call("code_review.summary", &json!({}), "another-session");
    assert!(
        unbound.is_error && unbound.output.contains("review_not_bound"),
        "{}",
        unbound.output
    );
}

#[test]
fn writes_land_where_the_review_app_reads_them() {
    let mut harness = Harness::new();
    let workspace = harness.bind(SESSION);
    let path = GitFixture::modified_path(0);
    let line = harness.added_line(&path);

    let written = harness.json(
        "code_review.add_comment",
        &json!({"path": path, "line": line, "body": "Why this value?", "kind": "question"}),
        SESSION,
    );
    let thread_id = written["thread_id"].as_str().expect("thread").to_owned();

    // The app lists annotations under the review's annotation scope.
    let (target, scope) = bcode_code_review::scope::annotation_scope(&workspace);
    let drafts = harness
        .store()
        .list_drafts(&ListDraftsRequest {
            repo_path: harness.fixture.root().to_path_buf(),
            target,
            scope: Some(scope),
        })
        .expect("drafts")
        .drafts;
    assert_eq!(drafts.len(), 1);
    let anchor = &drafts[0].anchor;
    assert_eq!(anchor.file_path, path);
    assert_eq!(anchor.new_line, Some(line));
    assert_eq!(anchor.line_kind, ReviewLineKind::Added);
    assert_eq!(anchor.source_id.as_deref(), Some("source-1"));
    assert_eq!(drafts[0].body, "Why this value?");

    harness.ok(
        "code_review.reply",
        &json!({"thread_id": thread_id, "body": "Because of the limit."}),
        SESSION,
    );
    harness.ok(
        "code_review.resolve_thread",
        &json!({"thread_id": thread_id}),
        SESSION,
    );
    let threads = harness.json(
        "code_review.list_threads",
        &json!({"include_resolved": true}),
        SESSION,
    );
    let thread = &threads["threads"][0];
    assert_eq!(thread["resolved"], true, "{threads}");
    assert_eq!(thread["kind"], "question");
    assert_eq!(thread["comments"].as_array().map(Vec::len), Some(2));
    let open = harness.json("code_review.list_threads", &json!({}), SESSION);
    assert_eq!(open["threads"].as_array().map(Vec::len), Some(0));
    assert_eq!(open["omitted"], 1);

    harness.ok(
        "code_review.mark_viewed",
        &json!({"paths": [path]}),
        SESSION,
    );
    let record = harness
        .store()
        .get_workspace(GetReviewWorkspaceRequest {
            repo_path: harness.fixture.root().to_path_buf(),
            workspace_id: bcode_code_review::scope::annotation_workspace_id(&workspace),
        })
        .expect("record")
        .workspace
        .expect("viewed record");
    assert!(record.viewed_files.contains(&path));
    let files = harness.json("code_review.list_files", &json!({}), SESSION);
    assert!(
        files["files"]
            .as_array()
            .expect("files")
            .iter()
            .any(|file| file["path"] == path.as_str() && file["viewed"] == true),
        "{files}"
    );

    harness.ok(
        "code_review.suggest_change",
        &json!({"path": path, "line": line, "replacement": "let limit = 10;", "body": "Name it."}),
        SESSION,
    );
    let threads = harness.json("code_review.list_threads", &json!({}), SESSION);
    let suggestion = threads["suggestions"][0]["body"]
        .as_str()
        .expect("suggestion");
    assert!(
        suggestion.contains("```suggestion\nlet limit = 10;\n```"),
        "{suggestion}"
    );

    let navigated = harness.ok(
        "code_review.navigate",
        &json!({"path": path, "line": line}),
        SESSION,
    );
    assert!(navigated.contains(&format!("{path}:{line}")), "{navigated}");
    let bad_line = harness.call(
        "code_review.add_comment",
        &json!({"path": path, "line": 0, "body": "x"}),
        SESSION,
    );
    assert!(bad_line.is_error && bad_line.output.contains("invalid_arguments"));
}

#[test]
fn publishing_previews_unless_asked_to_submit() {
    let mut harness = Harness::new();
    harness.bind(SESSION);
    harness.ok(
        "code_review.add_comment",
        &json!({"body": "Looks good overall."}),
        SESSION,
    );
    let preview = harness.ok("code_review.publish", &json!({}), SESSION);
    assert!(preview.contains("Looks good overall."), "{preview}");
    let output = harness.state.path().join("review.md");
    harness.ok(
        "code_review.publish",
        &json!({"publisher": "markdown_file", "submit": true, "output_path": output}),
        SESSION,
    );
    let written = std::fs::read_to_string(&output).expect("published file");
    assert!(written.contains("Looks good overall."), "{written}");
}
