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
        let data = ReviewData::default();
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
        "code_review.get_thread",
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
    let workspace = harness.bind(SESSION);

    // Before the app read the pull request threads, there are none, and nothing says why for a
    // review without pull requests.
    let listed = harness.json("code_review.list_threads", &json!({}), SESSION);
    assert_eq!(listed["pull_request_threads"]["total"], 0, "{listed}");
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

    let (target, scope) = bcode_code_review::scope::annotation_scope(&workspace);
    harness
        .store()
        .save_conversation(bcode_code_review_models::SaveReviewConversationRequest {
            repo_path: harness.fixture.root().to_path_buf(),
            target,
            scope: Some(scope),
            snapshot: pull_snapshot(6),
        })
        .expect("save");

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
    // The file is matched whole, by where it was written or where the review shows it.
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
fn a_large_listing_is_shortened_to_whole_pages_that_continue() {
    let mut harness = Harness::new();
    let workspace = harness.bind(SESSION);
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
    let workspace = harness.bind(SESSION);
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
