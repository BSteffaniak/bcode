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
        let response = self.prepare_raw(tool, arguments, session);
        assert!(response.error.is_none(), "{tool}: {:?}", response.error);
        response.payload_json().expect("preparation")
    }

    fn prepare_raw(&mut self, tool: &str, arguments: &Value, session: &str) -> ServiceResponse {
        let root = self.fixture.root().to_path_buf();
        self.service(
            OP_PREPARE_TOOL,
            &json!({
                "invocation": {"invocation_id": "call-1", "tool_name": tool, "arguments": arguments},
                "host_context": [{
                    "schema": bcode_tool::TOOL_WORKSPACE_CONTEXT_SCHEMA,
                    "schema_version": bcode_tool::TOOL_WORKSPACE_CONTEXT_SCHEMA_VERSION,
                    "payload": {"working_directory": root, "workspace_id": session},
                }],
            }),
        )
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
        self.bind_sources(session, false)
    }

    /// The transient last-commit review, with the pull request `o/r#7` among its sources when
    /// `pull_request` (only the last commit is resolved), bound to `session`.
    fn bind_sources(&self, session: &str, pull_request: bool) -> ReviewWorkspace {
        let mut kinds = vec![bcode_code_review_models::ReviewSourceKind::LastCommit];
        if pull_request {
            kinds.push(bcode_code_review_models::ReviewSourceKind::PullRequest {
                provider: "github".to_owned(),
                repository: "o/r".to_owned(),
                number: 7,
            });
        }
        let workspace = bcode_code_review::scope::transient_workspace(
            self.fixture.root().to_path_buf(),
            None,
            kinds,
        );
        let resolved = ReviewData::default()
            .resolve_sources(&ResolveReviewSourcesRequest {
                browse: bcode_code_review_models::BrowseOptions::default(),
                repo_path: self.fixture.root().to_path_buf(),
                sources: workspace.sources[..1].to_vec(),
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
                    stacks: bcode_code_review_models::ReviewAssistantStacks::from_stacks(
                        &resolved.stacks,
                    ),
                    created_at_ms: 0,
                    updated_at_ms: 0,
                    started: None,
                    detached_at_ms: None,
                },
            })
            .expect("bind");
        workspace
    }

    /// The first added line of `path` in the last commit.
    fn added_line(&self, path: &str) -> u32 {
        let data = ReviewData::default();
        let resolved = data
            .resolve_sources(&ResolveReviewSourcesRequest {
                browse: bcode_code_review_models::BrowseOptions::default(),
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
        "code_review.find_paths",
        "code_review.summary",
        "code_review.list_files",
        "code_review.file_diff",
        "code_review.file_content",
        "code_review.list_threads",
        "code_review.get_thread",
        "code_review.pull_request",
        "code_review.navigate",
        "code_review.add_comment",
        "code_review.reply",
        "code_review.resolve_thread",
        "code_review.mark_viewed",
        "code_review.suggest_change",
        "code_review.publish",
        "code_review.add_comments",
        "code_review.resolve_threads",
        "code_review.create_review",
        "code_review.update_review",
        "code_review.delete_review",
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
            | "code_review.suggest_change"
            | "code_review.add_comments"
            | "code_review.resolve_threads"
            | "code_review.create_review"
            | "code_review.update_review"
            | "code_review.delete_review" => ("mutating", true, "code_review.write"),
            "code_review.publish" => ("mutating", true, "code_review.publish"),
            // It reads GitHub with the user's credentials, so web rules decide it too.
            "code_review.pull_request" => ("compound", false, "code_review.read"),
            _ => ("read_only", false, "code_review.read"),
        };
        assert_eq!(metadata["operation"]["kind"], expected.0, "{name}");
        assert_eq!(metadata["requires_permission"], expected.1, "{name}");
        assert_eq!(metadata["permission_category"], expected.2, "{name}");
        assert_eq!(read_only, !expected.1, "{name}");
        assert_eq!(prepared.descriptor["session_id"], SESSION, "{name}");
    }
}

#[test]
fn the_default_agents_are_offered_every_review_tool_they_may_use() {
    let mut harness = Harness::new();
    let list: ToolList = harness
        .service(OP_LIST_TOOLS, &json!({}))
        .payload_json()
        .expect("list");
    let config = bcode_default_agents_plugin::default_config();
    let active = |agent: &str| {
        bcode_agent_policy::active_tools_for(&bcode_agent_policy::agent_config(&config, agent))
    };
    let (build, plan) = (
        active(bcode_agent_policy::BUILD_AGENT),
        active(bcode_agent_policy::PLAN_AGENT),
    );
    assert!(
        list.tools
            .iter()
            .any(|tool| tool.name == "code_review.get_thread")
    );
    for tool in &list.tools {
        let name = &tool.name;
        assert!(build.contains(name), "build agent is offered {name}");
        let read_only = list.discovery.get(name).expect("discovery").read_only;
        assert_eq!(plan.contains(name), read_only, "plan agent and {name}");
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
    harness.ok(
        "code_review.publish",
        &json!({"publisher": "markdown_file", "submit": true, "output_path": "exports/review.md"}),
        SESSION,
    );
    let output = harness.fixture.root().join("exports/review.md");
    let written = std::fs::read_to_string(&output).expect("published file");
    assert!(written.contains("Looks good overall."), "{written}");
}

#[test]
fn publishing_writes_only_inside_the_working_directory_and_declares_the_file() {
    let mut harness = Harness::new();
    let prepared = harness.prepare(
        "code_review.publish",
        &json!({"publisher": "markdown_file", "submit": true, "output_path": "exports/review.md"}),
        SESSION,
    );
    let operation = &prepared.authorization[0].metadata["operation"];
    let declared = harness
        .fixture
        .root()
        .join("exports/review.md")
        .display()
        .to_string();
    assert!(
        operation.to_string().contains(&declared),
        "the write is declared so write rules apply and the prompt names it: {operation}"
    );
    let outside = harness.state.path().join("review.md");
    for output_path in [
        outside.display().to_string(),
        "../review.md".to_owned(),
        "exports/../../review.md".to_owned(),
    ] {
        let response = harness.prepare_raw(
            "code_review.publish",
            &json!({"publisher": "markdown_file", "submit": true, "output_path": output_path}),
            SESSION,
        );
        assert!(response.error.is_some(), "{output_path} is refused");
    }
    assert!(!outside.exists());
}

#[test]
fn file_reads_declare_the_file_so_read_rules_apply() {
    let mut harness = Harness::new();
    for tool in ["code_review.file_diff", "code_review.file_content"] {
        let prepared = harness.prepare(tool, &json!({"path": "src/.env"}), SESSION);
        let operation = &prepared.authorization[0].metadata["operation"];
        let declared = harness
            .fixture
            .root()
            .join("src/.env")
            .display()
            .to_string();
        assert!(
            operation.to_string().contains(&declared),
            "{tool}: {operation}"
        );
        let escaping = harness.prepare_raw(tool, &json!({"path": "../secret"}), SESSION);
        assert!(
            escaping.error.is_some(),
            "{tool} refuses paths outside the repository"
        );
    }
}

/// The pull request threads the review app read, as the snapshot it keeps holds them: one
/// outdated thread written on a file the review no longer changes, with replies.
fn pull_snapshot(replies: usize) -> bcode_code_review_models::ReviewConversationSnapshot {
    use bcode_code_review_models::{
        DiffSide, Participant, ParticipantKind, PullThreadRecord, RemoteAnchor, RemoteComment,
        RemoteThread, ReviewConversationSnapshot, ThreadPlacement, pull_thread_ref,
    };
    let comment = |index: usize, login: &str, body: String| RemoteComment {
        id: format!("C{index}"),
        author: Participant {
            login: login.to_owned(),
            name: None,
            kind: ParticipantKind::User,
            url: None,
            is_viewer: false,
        },
        body,
        created_at_ms: Some(1_000 + u64::try_from(index).unwrap_or_default()),
        updated_at_ms: None,
        url: Some(format!("https://github.com/o/r/pull/7#discussion_r{index}")),
        review_id: None,
    };
    let mut comments = vec![comment(
        0,
        "kevin",
        "This retry loop never backs off.".to_owned(),
    )];
    comments.extend((1..=replies).map(|index| comment(index, "ana", format!("reply {index}"))));
    let outdated = PullThreadRecord {
        id: pull_thread_ref("o/r", 7, "PRRT_1"),
        provider: "github".to_owned(),
        provider_label: "GitHub".to_owned(),
        repository: "o/r".to_owned(),
        number: 7,
        location: "the review".to_owned(),
        placement: ThreadPlacement::OnLayer,
        thread: RemoteThread {
            id: "PRRT_1".to_owned(),
            anchor: RemoteAnchor {
                path: "src/retry.rs".to_owned(),
                side: DiffSide::New,
                original_line: Some(14),
                original_start_line: Some(12),
                original_commit_oid: Some("0ld".to_owned()),
                diff_hunk: Some("@@ -10,3 +10,6 @@ fn send()\n+loop {\n+    send()?;".to_owned()),
                ..RemoteAnchor::default()
            },
            resolved: false,
            outdated: true,
            can_reply: true,
            can_resolve: true,
            comments,
        },
    };
    let mut resolved = outdated.clone();
    resolved.id = pull_thread_ref("o/r", 7, "PRRT_2");
    resolved.thread.id = "PRRT_2".to_owned();
    resolved.thread.resolved = true;
    resolved.thread.outdated = false;
    resolved.thread.anchor.path = "README.md".to_owned();
    resolved.location = "README.md".to_owned();
    let mut moved = resolved.clone();
    moved.id = pull_thread_ref("o/r", 7, "PRRT_3");
    moved.thread.id = "PRRT_3".to_owned();
    moved.thread.anchor.path = "src/old_name.rs".to_owned();
    moved.location = "src/shown.rs:3".to_owned();
    moved.placement = ThreadPlacement::Moved;
    ReviewConversationSnapshot {
        threads: vec![outdated, resolved, moved],
        omitted: 0,
        read_at_ms: 42,
    }
}

#[test]
fn pull_request_threads_are_listed_and_read_from_the_apps_snapshot() {
    let mut harness = Harness::new();
    let workspace = harness.bind_sources(SESSION, true);

    // Before the app read the pull request threads, there are none, and the listing says why.
    let listed = harness.json("code_review.list_threads", &json!({}), SESSION);
    assert_eq!(listed["pull_request_threads"]["total"], 0, "{listed}");
    assert!(
        listed["pull_request_threads"]["note"]
            .as_str()
            .is_some_and(|note| note.contains("has not read")),
        "{listed}"
    );
    let unread = harness.call(
        "code_review.get_thread",
        &json!({"thread_id": "o/r#7:PRRT_1"}),
        SESSION,
    );
    assert!(
        unread.is_error && unread.output.contains("has not read"),
        "{}",
        unread.output
    );

    harness.save_snapshot(&workspace, pull_snapshot(6));

    let listed = harness.json("code_review.list_threads", &json!({}), SESSION);
    let page = &listed["pull_request_threads"];
    assert_eq!(page["total"], 1, "resolved ones are left out: {listed}");
    assert_eq!(page["omitted_resolved"], 2);
    assert_eq!(page["read_at_ms"], 42);
    let thread = &page["threads"][0];
    assert_eq!(thread["thread_id"], "o/r#7:PRRT_1");
    assert_eq!(thread["origin"], "pull_request");
    assert_eq!(thread["pull_request"], "o/r#7");
    assert_eq!(thread["outdated"], true);
    assert_eq!(thread["placement"], "on_layer");
    assert_eq!(thread["written_on"]["path"], "src/retry.rs");
    assert_eq!(thread["written_on"]["original_lines"], "12-14");
    assert_eq!(thread["written_on"]["commit"], "0ld");
    assert_eq!(thread["authors"], json!(["kevin", "ana"]));
    // A listing shows the first comment and the latest ones, and says what it left out.
    let bodies: Vec<&str> = thread["comments"]
        .as_array()
        .expect("comments")
        .iter()
        .filter_map(|comment| comment["body"].as_str())
        .collect();
    assert_eq!(
        bodies,
        [
            "This retry loop never backs off.",
            "reply 4",
            "reply 5",
            "reply 6"
        ]
    );
    assert_eq!(thread["comments_omitted"], 3);
    assert!(
        thread.get("diff_hunk").is_none(),
        "hunks only in full reads"
    );

    let all = harness.json(
        "code_review.list_threads",
        &json!({"include_resolved": true, "limit": 1}),
        SESSION,
    );
    let page = &all["pull_request_threads"];
    assert_eq!(
        (page["total"].as_u64(), page["next_offset"].as_u64()),
        (Some(3), Some(1))
    );
    let second = harness.json(
        "code_review.list_threads",
        &json!({"include_resolved": true, "limit": 1, "offset": 1}),
        SESSION,
    );
    assert_eq!(
        second["pull_request_threads"]["threads"][0]["thread_id"],
        "o/r#7:PRRT_2"
    );
    assert_eq!(second["pull_request_threads"]["next_offset"], 2);
    let on_file = harness.json(
        "code_review.list_threads",
        &json!({"include_resolved": true, "path": "README.md"}),
        SESSION,
    );
    assert_eq!(on_file["pull_request_threads"]["total"], 1, "{on_file}");

    let full = harness.json(
        "code_review.get_thread",
        &json!({"thread_id": "o/r#7:PRRT_1"}),
        SESSION,
    );
    assert_eq!(full["comments"].as_array().map(Vec::len), Some(7), "{full}");
    assert_eq!(full["comments"][6]["body"], "reply 6");
    assert!(
        full["diff_hunk"]
            .as_str()
            .is_some_and(|hunk| hunk.ends_with("+    send()?;")),
        "{full}"
    );
    assert_eq!(full["url"], "https://github.com/o/r/pull/7#discussion_r0");
}

impl Harness {
    /// Keep `snapshot` as the review app would for the review bound with `workspace`.
    fn save_snapshot(
        &self,
        workspace: &ReviewWorkspace,
        snapshot: bcode_code_review_models::ReviewConversationSnapshot,
    ) {
        let (target, scope) = bcode_code_review::scope::annotation_scope(workspace);
        self.store()
            .save_conversation(bcode_code_review_models::SaveReviewConversationRequest {
                repo_path: self.fixture.root().to_path_buf(),
                target,
                scope: Some(scope),
                snapshot,
            })
            .expect("save");
    }
}

#[test]
fn listing_pull_request_threads_by_path_matches_the_whole_file() {
    let mut harness = Harness::new();
    let workspace = harness.bind_sources(SESSION, true);
    harness.save_snapshot(&workspace, pull_snapshot(1));
    // By where it was written or where the review shows it.
    for (path, total) in [
        ("README", 0),
        ("README.md.orig", 0),
        ("src/retry", 0),
        ("src/retry.rs", 1),
        ("src/shown", 0),
        ("src/shown.rs", 1),
    ] {
        let listed = harness.json(
            "code_review.list_threads",
            &json!({"include_resolved": true, "path": path}),
            SESSION,
        );
        assert_eq!(
            listed["pull_request_threads"]["total"], total,
            "{path}: {listed}"
        );
    }
}

#[test]
fn a_large_listing_is_shortened_to_whole_pages_that_continue() {
    let mut harness = Harness::new();
    let workspace = harness.bind_sources(SESSION, true);
    let mut snapshot = pull_snapshot(5);
    let template = snapshot.threads[0].clone();
    snapshot.threads = (0..90)
        .map(|index| {
            let mut record = template.clone();
            record.thread.id = format!("PRRT_{index}");
            record.id = bcode_code_review_models::pull_thread_ref("o/r", 7, &record.thread.id);
            for comment in &mut record.thread.comments {
                comment.body = format!("{index} {}", "\"quoted\" ".repeat(200));
            }
            record
        })
        .collect();
    harness.save_snapshot(&workspace, snapshot);

    let mut seen = Vec::new();
    let mut offset = 0;
    loop {
        let output = harness.ok(
            "code_review.list_threads",
            &json!({"limit": 100, "offset": offset}),
            SESSION,
        );
        assert!(
            output.len() <= bcode_code_review::assistant::MAX_THREADS_OUTPUT_BYTES,
            "{}",
            output.len()
        );
        let listed: Value = serde_json::from_str(&output).expect("whole JSON");
        let page = &listed["pull_request_threads"];
        assert_eq!(page["total"], 90);
        assert_eq!(page["read_at_ms"], 42);
        let threads = page["threads"].as_array().expect("threads");
        assert!(!threads.is_empty(), "every page shows a thread");
        seen.extend(
            threads
                .iter()
                .filter_map(|thread| thread["thread_id"].as_str().map(str::to_owned)),
        );
        match page["next_offset"].as_u64() {
            Some(next) => offset = usize::try_from(next).expect("offset"),
            None => break,
        }
    }
    assert!(offset > 0, "the listing was shortened to pages");
    let expected: Vec<String> = (0..90).map(|index| format!("o/r#7:PRRT_{index}")).collect();
    assert_eq!(seen, expected, "every thread once, in order");
}

#[test]
fn a_long_thread_keeps_its_hunk_and_its_first_and_latest_comments() {
    let mut harness = Harness::new();
    let workspace = harness.bind_sources(SESSION, true);
    let mut snapshot = pull_snapshot(20);
    let record = &mut snapshot.threads[0];
    for (index, comment) in record.thread.comments.iter_mut().enumerate() {
        comment.body = format!("comment {index} {}", "y".repeat(8 * 1024));
    }
    let hunk: Vec<String> = (0..2_000).map(|line| format!("+line {line}")).collect();
    record.thread.anchor.diff_hunk = Some(hunk.join("\n"));
    harness.save_snapshot(&workspace, snapshot);

    let output = harness.ok(
        "code_review.get_thread",
        &json!({"thread_id": "o/r#7:PRRT_1"}),
        SESSION,
    );
    assert!(
        output.len() <= bcode_code_review::assistant::MAX_THREADS_OUTPUT_BYTES,
        "{}",
        output.len()
    );
    let full: Value = serde_json::from_str(&output).expect("whole JSON");
    let hunk = full["diff_hunk"].as_str().expect("the hunk is kept");
    assert!(
        hunk.starts_with(bcode_code_review::assistant::fit::HUNK_CUT),
        "{hunk:.80}"
    );
    assert!(hunk.ends_with("+line 1999"), "the commented end is kept");
    let comments = full["comments"].as_array().expect("comments");
    let first = comments
        .first()
        .and_then(|comment| comment["body"].as_str());
    let latest = comments.last().and_then(|comment| comment["body"].as_str());
    assert!(first.is_some_and(|body| body.starts_with("comment 0 ")));
    assert!(latest.is_some_and(|body| body.starts_with("comment 20 ")));
    let omitted = full["comments_omitted"].as_u64().expect("omitted counted");
    assert_eq!(
        usize::try_from(omitted).expect("count") + comments.len(),
        21,
        "{omitted} left out"
    );
}

#[test]
fn a_review_without_pull_requests_shows_no_pull_request_threads() {
    let mut harness = Harness::new();
    let with_pull = harness.bind_sources(SESSION, true);
    harness.save_snapshot(&with_pull, pull_snapshot(1));
    let summary = harness.json("code_review.summary", &json!({}), SESSION);
    assert_eq!(summary["pull_request_threads"], 3, "{summary}");

    // A snapshot left for a review that no longer has pull request sources is not shown.
    let without = harness.bind(SESSION);
    harness.save_snapshot(&without, pull_snapshot(1));
    let listed = harness.json("code_review.list_threads", &json!({}), SESSION);
    let page = &listed["pull_request_threads"];
    assert_eq!(page["total"], 0, "{listed}");
    assert!(page.get("note").is_none(), "{listed}");
    let summary = harness.json("code_review.summary", &json!({}), SESSION);
    assert_eq!(summary["pull_request_threads"], 0, "{summary}");
    let read = harness.call(
        "code_review.get_thread",
        &json!({"thread_id": "o/r#7:PRRT_1"}),
        SESSION,
    );
    assert!(
        read.is_error && read.output.contains("no pull requests"),
        "{}",
        read.output
    );
}

#[test]
fn pull_request_threads_are_not_replied_to_or_resolved_by_the_tools() {
    let mut harness = Harness::new();
    harness.bind(SESSION);
    for (tool, arguments) in [
        (
            "code_review.reply",
            json!({"thread_id": "o/r#7:PRRT_1", "body": "Fixed."}),
        ),
        (
            "code_review.resolve_thread",
            json!({"thread_id": "o/r#7:PRRT_1"}),
        ),
    ] {
        // The write still asks first like any other.
        let prepared = harness.prepare(tool, &arguments, SESSION);
        assert_eq!(
            prepared.authorization[0].metadata["requires_permission"], true,
            "{tool}"
        );
        let response = harness.call(tool, &arguments, SESSION);
        assert!(response.is_error, "{tool}");
        assert!(
            response.output.contains("pull request thread")
                && response.output.contains("local threads only"),
            "{tool}: {}",
            response.output
        );
    }
}

#[test]
fn a_detached_session_must_name_its_review() {
    let mut harness = Harness::new();
    let workspace = harness.bind(SESSION);
    let mut binding = harness
        .store()
        .assistant_binding(&bcode_code_review_models::GetReviewAssistantRequest {
            repo_path: harness.fixture.root().to_path_buf(),
            lookup: bcode_code_review_models::ReviewAssistantLookup::Session {
                session_id: SESSION.to_owned(),
            },
        })
        .expect("binding")
        .binding
        .expect("bound");
    binding.detached_at_ms = Some(1);
    harness
        .store()
        .bind_assistant(BindReviewAssistantRequest {
            repo_path: workspace.repo_root,
            binding,
        })
        .expect("detach");

    let detached = harness.call("code_review.summary", &json!({}), SESSION);
    assert!(detached.is_error, "{}", detached.output);
    assert!(
        detached.output.contains("review_not_bound")
            && detached
                .output
                .contains(bcode_code_review::assistant::DETACHED_MESSAGE),
        "{}",
        detached.output
    );
}

/// Reviews created through the tool, by kind.
fn created(harness: &mut Harness, arguments: &Value) -> Value {
    harness.json("code_review.create_review", arguments, SESSION)["review"].clone()
}

fn stored(harness: &Harness, root: &std::path::Path, id: &str) -> ReviewWorkspace {
    harness
        .store()
        .get_workspace(GetReviewWorkspaceRequest {
            repo_path: root.to_path_buf(),
            workspace_id: id.to_owned(),
        })
        .expect("get")
        .workspace
        .expect("saved")
}

#[test]
#[allow(clippy::too_many_lines)]
fn reviews_are_created_from_every_kind_of_source_and_listed() {
    use bcode_code_review_models::ReviewSourceKind as Kind;
    let mut harness = Harness::new();
    harness.fixture.run(&[
        "remote",
        "add",
        "origin",
        "https://github.com/acme/widgets.git",
    ]);
    let root = harness.fixture.root().to_path_buf();
    let kinds = |harness: &mut Harness, arguments: Value| -> (Value, Vec<Kind>) {
        let review = created(harness, &arguments);
        let id = review["id"].as_str().expect("id").to_owned();
        let kinds = stored(harness, &root, &id)
            .sources
            .into_iter()
            .map(|source| source.kind)
            .collect();
        (review, kinds)
    };
    let (review, stack) = kinds(
        &mut harness,
        json!({"sources": [{"kind": "stack_layers", "layers": [101, 102, 103], "titles": ["Base", "Middle", "Top"]}]}),
    );
    assert_eq!(review["kind"], "stack_step_through");
    assert_eq!(review["pull_requests"], json!([101, 102, 103]));
    assert_eq!(
        stack[0],
        Kind::StackLayer {
            provider: "github".to_owned(),
            repository: "acme/widgets".to_owned(),
            number: 101
        }
    );
    assert!(
        review["sources"][0]
            .as_str()
            .is_some_and(|label| label == "#101 Base"),
        "{review}"
    );
    let (review, pull) = kinds(
        &mut harness,
        json!({"sources": [{"kind": "pull_request", "url": "https://github.com/o/r/pull/7"}], "title": "Seven"}),
    );
    assert_eq!(
        (review["title"].as_str(), review["kind"].as_str()),
        (Some("Seven"), Some("pull_request"))
    );
    assert_eq!(
        pull,
        vec![Kind::PullRequest {
            provider: "github".to_owned(),
            repository: "o/r".to_owned(),
            number: 7
        }]
    );
    let (_, branch) = kinds(
        &mut harness,
        json!({"sources": [{"kind": "branch", "base": "fixture/base", "head": "main"}]}),
    );
    assert!(matches!(
        &branch[0],
        Kind::BranchCompare {
            merge_base: true,
            ..
        }
    ));
    let (_, range) = kinds(
        &mut harness,
        json!({"sources": [{"kind": "range", "base": "main~1", "head": "main"}, {"kind": "commits", "revs": ["main"]}]}),
    );
    assert!(matches!(
        &range[..],
        [
            Kind::CommitRange {
                merge_base: false,
                ..
            },
            Kind::Commits { .. }
        ]
    ));
    // Uncommitted changes in a directory: limited to it.
    let (review, scoped) = kinds(
        &mut harness,
        json!({"sources": [{"kind": "working_tree"}], "directory": "src/mod_000"}),
    );
    assert_eq!(
        scoped,
        vec![Kind::WorkingTreePaths {
            paths: vec!["src/mod_000".to_owned()],
            staged: true,
            unstaged: true
        }]
    );
    assert_eq!(review["kind"], "working_tree_paths");
    let id = review["id"].as_str().expect("id").to_owned();
    let files = harness.json("code_review.list_files", &json!({"review": id}), SESSION);
    let paths: Vec<&str> = files["files"]
        .as_array()
        .expect("files")
        .iter()
        .filter_map(|file| file["path"].as_str())
        .collect();
    assert!(
        !paths.is_empty() && paths.iter().all(|path| path.starts_with("src/mod_000/")),
        "{paths:?}"
    );

    // A directory in another repository creates the review there.
    let other = GitFixture::generate(FixtureSpec::small());
    let elsewhere = harness.json(
        "code_review.create_review",
        &json!({"sources": [{"kind": "staged"}], "directory": other.root().join("src")}),
        SESSION,
    );
    assert_eq!(elsewhere["other_repository"], true, "{elsewhere}");
    let id = elsewhere["review"]["id"].as_str().expect("id").to_owned();
    let other_root = other.root().canonicalize().expect("root");
    assert!(matches!(
        &stored(&harness, &other_root, &id).sources[0].kind,
        Kind::WorkingTreePaths {
            staged: true,
            unstaged: false,
            ..
        }
    ));

    // Listing: newest first, searchable by pull request number.
    let listed = harness.json("code_review.list_reviews", &json!({}), SESSION);
    assert_eq!(
        listed["reviews"].as_array().map(Vec::len),
        Some(5),
        "{listed}"
    );
    let found = harness.json(
        "code_review.list_reviews",
        &json!({"query": "#102"}),
        SESSION,
    );
    assert_eq!(found["reviews"][0]["kind"], "stack_step_through", "{found}");
    let bad = harness.call(
        "code_review.create_review",
        &json!({"sources": [{"kind": "pull_request"}]}),
        SESSION,
    );
    assert!(
        bad.is_error && bad.output.contains("number"),
        "{}",
        bad.output
    );
}

#[test]
fn reviews_are_renamed_archived_restored_and_deleted_in_batches() {
    let mut harness = Harness::new();
    let first = created(&mut harness, &json!({"sources": [{"kind": "last_commit"}]}));
    let second = created(&mut harness, &json!({"sources": [{"kind": "staged"}]}));
    let (first, second) = (
        first["id"].as_str().expect("id").to_owned(),
        second["id"].as_str().expect("id").to_owned(),
    );
    let renamed = harness.json(
        "code_review.update_review",
        &json!({"review": first, "title": "Renamed"}),
        SESSION,
    );
    assert_eq!(
        renamed["items"][0]["result"]["title"], "Renamed",
        "{renamed}"
    );
    let archived = harness.json(
        "code_review.update_review",
        &json!({"reviews": [first, second, "missing"], "archived": true}),
        SESSION,
    );
    assert_eq!(
        (archived["succeeded"].as_u64(), archived["failed"].as_u64()),
        (Some(2), Some(1)),
        "{archived}"
    );
    let listed = harness.json("code_review.list_reviews", &json!({}), SESSION);
    assert_eq!(listed["reviews"].as_array().map(Vec::len), Some(0));
    let all = harness.json(
        "code_review.list_reviews",
        &json!({"include_archived": true}),
        SESSION,
    );
    assert_eq!(all["reviews"][0]["archived"], true);
    harness.ok(
        "code_review.update_review",
        &json!({"review": second, "archived": false}),
        SESSION,
    );
    let listed = harness.json("code_review.list_reviews", &json!({}), SESSION);
    assert_eq!(listed["reviews"][0]["id"], second.as_str());
    let deleted = harness.json(
        "code_review.delete_review",
        &json!({"reviews": [first, second]}),
        SESSION,
    );
    assert_eq!(deleted["succeeded"], 2, "{deleted}");
    let all = harness.json(
        "code_review.list_reviews",
        &json!({"include_archived": true}),
        SESSION,
    );
    assert_eq!(all["reviews"].as_array().map(Vec::len), Some(0));
    let nothing = harness.call("code_review.delete_review", &json!({}), SESSION);
    assert!(nothing.is_error);
}

#[test]
#[allow(clippy::too_many_lines)]
fn files_are_marked_viewed_by_list_glob_layer_and_unchanged_since_a_pass() {
    let mut harness = Harness::new();
    let review = created(
        &mut harness,
        &json!({"sources": [{"kind": "range", "base": "fixture/base", "head": "main"}, {"kind": "last_commit"}]}),
    );
    let id = review["id"].as_str().expect("id").to_owned();
    let files = |harness: &mut Harness| -> Vec<(String, bool)> {
        let listed = harness.json("code_review.list_files", &json!({"review": id}), SESSION);
        listed["files"]
            .as_array()
            .expect("files")
            .iter()
            .map(|file| {
                (
                    file["path"].as_str().expect("path").to_owned(),
                    file["viewed"] == true,
                )
            })
            .collect()
    };
    let all_files = files(&mut harness);
    let first = GitFixture::modified_path(0);
    let listed = harness.json(
        "code_review.mark_viewed",
        &json!({"review": id, "paths": [first, "nope.rs"]}),
        SESSION,
    );
    assert_eq!(listed["changed"], json!([first]), "{listed}");
    assert_eq!(listed["not_found"], json!(["nope.rs"]));
    let again = harness.json(
        "code_review.mark_viewed",
        &json!({"review": id, "paths": [first]}),
        SESSION,
    );
    assert_eq!(
        (again["changed_count"].as_u64(), again["unchanged"].clone()),
        (Some(0), json!([first]))
    );
    let globbed = harness.json(
        "code_review.mark_viewed",
        &json!({"review": id, "glob": "src/**/*.rs"}),
        SESSION,
    );
    let expected_glob = all_files
        .iter()
        .filter(|(path, _)| {
            path.starts_with("src/")
                && std::path::Path::new(path)
                    .extension()
                    .is_some_and(|ext| ext == "rs")
                && *path != first
        })
        .map(|(path, _)| path.clone())
        .collect::<std::collections::BTreeSet<_>>();
    let changed: std::collections::BTreeSet<String> = globbed["changed"]
        .as_array()
        .expect("changed")
        .iter()
        .filter_map(|path| path.as_str().map(str::to_owned))
        .collect();
    assert_eq!(changed, expected_glob, "{globbed}");
    // Unmark every file of the second source (a layer named by its source id).
    let unmarked = harness.json(
        "code_review.mark_viewed",
        &json!({"review": id, "layers": ["source-2"], "all": true, "viewed": false}),
        SESSION,
    );
    assert!(
        unmarked["message"]
            .as_str()
            .is_some_and(|message| message.contains("not viewed")),
        "{unmarked}"
    );
    let bad = harness.call(
        "code_review.mark_viewed",
        &json!({"review": id, "layers": ["#999"]}),
        SESSION,
    );
    assert!(
        bad.is_error && bad.output.contains("#999"),
        "{}",
        bad.output
    );
    // Everything off, then a pass recorded on the same revisions: every file is unchanged since.
    harness.ok(
        "code_review.mark_viewed",
        &json!({"review": id, "all": true, "viewed": false}),
        SESSION,
    );
    assert!(files(&mut harness).iter().all(|(_, viewed)| !viewed));
    let none = harness.json(
        "code_review.mark_viewed",
        &json!({"review": id, "unchanged_since_look": true}),
        SESSION,
    );
    assert_eq!(none["changed_count"], 0, "no look yet: {none}");
    let mut workspace = stored(&harness, harness.fixture.root(), &id);
    let resolved = ReviewData::default()
        .resolve_sources(&ResolveReviewSourcesRequest {
            browse: bcode_code_review_models::BrowseOptions::default(),
            repo_path: harness.fixture.root().to_path_buf(),
            sources: workspace.sources.clone(),
            provider_resolutions: Vec::new(),
        })
        .expect("resolve");
    for source in &resolved.sources {
        workspace.reviewed_revisions.insert(
            source.source_id.clone(),
            bcode_code_review_models::ReviewedRevision::from_resolved(source, Some(1)),
        );
    }
    harness
        .store()
        .update_workspace(bcode_code_review_models::UpdateReviewWorkspaceRequest {
            repo_path: harness.fixture.root().to_path_buf(),
            workspace,
            base: None,
        })
        .expect("pass");
    let since = harness.json(
        "code_review.mark_viewed",
        &json!({"review": id, "unchanged_since_look": true}),
        SESSION,
    );
    assert!(
        since["changed_count"]
            .as_u64()
            .is_some_and(|count| count > 0),
        "{since}"
    );
}

#[test]
fn comments_and_resolutions_are_written_in_batches() {
    let mut harness = Harness::new();
    harness.bind(SESSION);
    let path = GitFixture::modified_path(0);
    let line = harness.added_line(&path);
    let written = harness.json(
        "code_review.add_comments",
        &json!({"comments": [
            {"path": path, "line": line, "body": "One"},
            {"body": "Overall fine", "kind": "summary"},
            {"path": "missing.rs", "body": "Nowhere"},
        ]}),
        SESSION,
    );
    assert_eq!(
        (written["succeeded"].as_u64(), written["failed"].as_u64()),
        (Some(2), Some(1)),
        "{written}"
    );
    let threads: Vec<String> = written["items"]
        .as_array()
        .expect("items")
        .iter()
        .filter_map(|item| item["result"]["thread_id"].as_str().map(str::to_owned))
        .collect();
    let resolved = harness.json(
        "code_review.resolve_threads",
        &json!({"thread_ids": threads}),
        SESSION,
    );
    assert_eq!(resolved["succeeded"], 2, "{resolved}");
    let open = harness.json("code_review.list_threads", &json!({}), SESSION);
    assert_eq!(open["threads"].as_array().map(Vec::len), Some(0), "{open}");
}

#[test]
fn a_stack_review_named_by_id_resolves_from_the_stack_it_last_opened_with() {
    use bcode_code_review_models::{
        PullRequestSummary, ReviewStack, ReviewStackLayer, ReviewStackSnapshot,
    };
    let mut harness = Harness::new();
    let fixture = &harness.fixture;
    let oid = |rev: &str| fixture.git(&["rev-parse", rev]).trim().to_owned();
    let branches = bcode_code_review::testing::fixture::STACK_BRANCHES;
    let layers: Vec<ReviewStackLayer> = branches
        .iter()
        .enumerate()
        .map(|(index, branch)| ReviewStackLayer {
            pull: PullRequestSummary {
                number: 101 + index as u64,
                title: format!("Layer {}", index + 1),
                author: None,
                base_branch: if index == 0 {
                    "main".to_owned()
                } else {
                    branches[index - 1].to_owned()
                },
                head_branch: (*branch).to_owned(),
                head_oid: oid(branch),
                base_oid: (index == 0).then(|| oid("main")),
                review_requested_from_me: false,
                draft: false,
                updated: None,
                url: None,
            },
            parent: (index > 0).then(|| 100 + index as u64),
        })
        .collect();
    let review = created(
        &mut harness,
        &json!({"sources": [{"kind": "stack_layers", "layers": [101, 102, 103], "repository": "acme/widgets"}]}),
    );
    let id = review["id"].as_str().expect("id").to_owned();
    // Without what it last opened with, the tools say how to get it resolved.
    let unresolved = harness.call("code_review.list_files", &json!({"review": id}), SESSION);
    assert!(
        unresolved.is_error && unresolved.output.contains("open_review"),
        "{}",
        unresolved.output
    );
    let root = harness.fixture.root().to_path_buf();
    let mut workspace = stored(&harness, &root, &id);
    workspace.stack_snapshots = vec![ReviewStackSnapshot {
        provider: "github".to_owned(),
        repository: "acme/widgets".to_owned(),
        stack: ReviewStack {
            trunk: "main".to_owned(),
            rooted_at_default_branch: true,
            layers,
            requested: 103,
            siblings: Vec::new(),
            warnings: Vec::new(),
        },
        confirmed_at_ms: 1,
    }];
    harness
        .store()
        .update_workspace(bcode_code_review_models::UpdateReviewWorkspaceRequest {
            repo_path: root,
            workspace,
            base: None,
        })
        .expect("snapshot");
    let files = harness.json("code_review.list_files", &json!({"review": id}), SESSION);
    let paths: Vec<&str> = files["files"]
        .as_array()
        .expect("files")
        .iter()
        .filter_map(|file| file["path"].as_str())
        .collect();
    for layer in 1..=3 {
        assert!(
            paths.contains(&GitFixture::stack_layer_path(layer).as_str()),
            "{paths:?}"
        );
    }
    let marked = harness.json(
        "code_review.mark_viewed",
        &json!({"review": id, "layers": ["102"], "all": true}),
        SESSION,
    );
    assert!(
        marked["changed"]
            .as_array()
            .expect("changed")
            .iter()
            .any(|path| path == GitFixture::stack_layer_path(2).as_str()),
        "{marked}"
    );
    assert!(
        !marked["changed"]
            .as_array()
            .expect("changed")
            .iter()
            .any(|path| path == GitFixture::stack_layer_path(1).as_str()),
        "only layer 102: {marked}"
    );
}

#[test]
fn blank_optional_arguments_are_taken_as_not_given() {
    let mut harness = Harness::new();
    harness.bind(SESSION);
    let path = GitFixture::modified_path(0);
    let line = harness.added_line(&path);
    let written = harness.json(
        "code_review.add_comment",
        &json!({
            "path": path, "line": line, "end_line": null, "side": "", "source": "",
            "kind": "", "severity": " ", "body": "Why this value?", "review": ""
        }),
        SESSION,
    );
    assert!(written["thread_id"].is_string(), "{written}");
    let shown = harness.ok(
        "code_review.navigate",
        &json!({"path": path, "line": line, "side": "", "source": ""}),
        SESSION,
    );
    assert!(shown.contains(&path), "{shown}");
    let missing = harness.call(
        "code_review.reply",
        &json!({"thread_id": "", "body": "x"}),
        SESSION,
    );
    assert!(missing.is_error, "{}", missing.output);
    assert!(missing.output.contains("thread_id"), "{}", missing.output);
}

#[test]
fn an_empty_replacement_suggests_deleting_the_lines() {
    let mut harness = Harness::new();
    harness.bind(SESSION);
    let path = GitFixture::modified_path(0);
    let line = harness.added_line(&path);
    harness.ok(
        "code_review.suggest_change",
        &json!({"path": path, "line": line, "replacement": "", "body": "Drop this line.", "source": ""}),
        SESSION,
    );
    let threads = harness.json("code_review.list_threads", &json!({}), SESSION);
    let suggestion = threads["suggestions"][0]["body"]
        .as_str()
        .expect("suggestion");
    assert!(suggestion.contains("```suggestion\n\n```"), "{suggestion}");
}

#[test]
fn find_paths_names_a_folder_that_create_review_reviews_as_it_is_now() {
    let mut harness = Harness::new();
    let root = harness.fixture.root().to_path_buf();
    for (path, text) in [
        (
            "examples/hello-plugin/Cargo.toml",
            "[package]\nname = \"hello-plugin\"\n",
        ),
        ("examples/hello-plugin/src/lib.rs", "pub fn hello() {}\n"),
        ("examples/hello-plugin/target.log", "ignored\n"),
        (".gitignore", "*.log\n"),
    ] {
        let full = root.join(path);
        std::fs::create_dir_all(full.parent().expect("parent")).expect("mkdir");
        std::fs::write(full, text).expect("write");
    }
    let found = harness.json(
        "code_review.find_paths",
        &json!({"query": "the hello plugin"}),
        SESSION,
    );
    assert_eq!(found["ambiguous"], false, "{found}");
    let best = &found["matches"][0];
    assert_eq!(
        (
            best["path"].as_str(),
            best["kind"].as_str(),
            best["files"].as_u64()
        ),
        (Some("examples/hello-plugin/"), Some("package"), Some(2)),
        "{found}"
    );
    let review = created(
        &mut harness,
        &json!({"sources": [{"kind": "files", "paths": [best["path"]]}]}),
    );
    assert_eq!(review["kind"], "files", "{review}");
    let id = review["id"].as_str().expect("id").to_owned();
    assert_eq!(
        stored(&harness, &root, &id).sources[0].kind,
        bcode_code_review_models::ReviewSourceKind::FileSet {
            include: vec![bcode_code_review_models::PathSelector::Folder {
                path: "examples/hello-plugin".to_owned()
            }],
            exclude: Vec::new(),
        }
    );
    // A file added to the folder later is part of the review when it is read again.
    std::fs::write(root.join("examples/hello-plugin/src/more.rs"), "// more\n").expect("write");
    let files = harness.json("code_review.list_files", &json!({"review": id}), SESSION);
    let listed = files.to_string();
    for path in [
        "examples/hello-plugin/Cargo.toml",
        "examples/hello-plugin/src/lib.rs",
        "examples/hello-plugin/src/more.rs",
    ] {
        assert!(listed.contains(path), "{path} in {listed}");
    }
    assert!(!listed.contains("target.log"), "{listed}");
    let missing = harness.call(
        "code_review.create_review",
        &json!({"sources": [{"kind": "files", "paths": ["examples/helo-plugin"]}]}),
        SESSION,
    );
    assert!(missing.is_error);
    assert!(
        missing
            .output
            .contains("examples/helo-plugin: no such file or folder"),
        "{}",
        missing.output
    );
}

#[test]
fn list_files_says_when_a_folder_is_cut_at_the_file_limit() {
    let mut harness = Harness::new();
    let root = harness.fixture.root().join("bulk");
    std::fs::create_dir_all(&root).expect("mkdir");
    let limit = bcode_code_review_models::REVIEW_FILES_DEFAULT_MAX_FILES;
    for index in 0..=limit {
        std::fs::write(root.join(format!("{index:05}.txt")), "x\n").expect("write");
    }
    let review = created(
        &mut harness,
        &json!({"sources": [{"kind": "files", "paths": ["bulk/"]}]}),
    );
    let id = review["id"].as_str().expect("id").to_owned();
    // The answer is long enough to be cut to the output limit; the warning comes first.
    let files = harness.ok("code_review.list_files", &json!({"review": id}), SESSION);
    let head: String = files.chars().take(400).collect();
    assert!(
        head.contains(&format!(
            "resolves to {} files; showing the first {limit}",
            limit + 1
        )),
        "{head}"
    );
}

#[test]
fn create_review_can_list_ignored_files_and_find_paths_flags_weak_matches() {
    let mut harness = Harness::new();
    let root = harness.fixture.root().to_path_buf();
    for (path, text) in [
        ("oauth-client/src/lib.rs", "//! OAuth.\n"),
        ("generated/out.rs", "// generated\n"),
        (".gitignore", "generated/\n"),
    ] {
        let full = root.join(path);
        std::fs::create_dir_all(full.parent().expect("parent")).expect("mkdir");
        std::fs::write(full, text).expect("write");
    }
    let weak = harness.json(
        "code_review.find_paths",
        &json!({"query": "the auth module"}),
        SESSION,
    );
    assert_eq!(weak["matches"][0]["path"], "oauth-client/", "{weak}");
    assert_eq!(weak["weak"], true, "{weak}");
    let found = harness.json(
        "code_review.find_paths",
        &json!({"query": "generated", "include_ignored": true}),
        SESSION,
    );
    assert_eq!(found["matches"][0]["path"], "generated/", "{found}");
    let review = created(
        &mut harness,
        &json!({"sources": [{"kind": "files", "paths": ["generated/"]}], "show_ignored": true}),
    );
    let id = review["id"].as_str().expect("id").to_owned();
    assert_eq!(
        stored(&harness, &root, &id)
            .presentation
            .and_then(|presentation| presentation.show_ignored),
        Some(true)
    );
    let files = harness.json("code_review.list_files", &json!({"review": id}), SESSION);
    assert!(
        files.to_string().contains("generated/out.rs"),
        "the assistant reads the review as the app lists it: {files}"
    );
}
