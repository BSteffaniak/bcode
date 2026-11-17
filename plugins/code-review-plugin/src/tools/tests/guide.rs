//! Guided-review tools and the code reads at pinned revisions, driven through the plugin entry
//! point.

use super::{Harness, SESSION};
use bcode_code_review::testing::fixture::GitFixture;
use bcode_code_review_models::{
    BindReviewAssistantRequest, GetGuideVersionRequest, GetReviewAssistantRequest, GrantMode,
    GuideGrant, ReviewAssistantLookup,
};
use bcode_tool::{OP_LIST_TOOLS, ToolList};
use serde_json::{Value, json};

const GUIDE_TOOLS: [&str; 6] = [
    "code_review.guide.start",
    "code_review.guide.write",
    "code_review.guide.get",
    "code_review.guide.check",
    "code_review.guide.plan",
    "code_review.guide.export",
];
const CODE_TOOLS: [&str; 3] = [
    "code_review.search_code",
    "code_review.diff_hunks",
    "code_review.related_files",
];

/// A quote of `path` at `line` of the review's head (the last commit).
fn line_text(harness: &Harness, path: &str, line: u32) -> String {
    harness
        .fixture
        .git(&["show", &format!("HEAD:{path}")])
        .lines()
        .nth(usize::try_from(line - 1).expect("line"))
        .expect("the line exists")
        .to_owned()
}

/// Give `session`'s binding `grant`.
fn grant(harness: &Harness, session: &str, grant: Option<GuideGrant>) {
    let store = harness.store();
    let mut binding = store
        .assistant_binding(&GetReviewAssistantRequest {
            repo_path: harness.fixture.root().to_path_buf(),
            lookup: ReviewAssistantLookup::Session {
                session_id: session.to_owned(),
            },
        })
        .expect("binding")
        .binding
        .expect("bound");
    binding.guide_grant = grant;
    store
        .bind_assistant(BindReviewAssistantRequest {
            repo_path: harness.fixture.root().to_path_buf(),
            binding,
        })
        .expect("bind");
}

/// Start a guide with an outline and write its step with a verified citation; returns the
/// guide id, the cited path, and the cited line.
fn started(harness: &mut Harness) -> (String, String, u32) {
    let path = GitFixture::modified_path(0);
    let line = harness.added_line(&path);
    let quote = line_text(harness, &path, line);
    let started = harness.json(
        "code_review.guide.start",
        &json!({
            "title": "Walkthrough",
            "directions": "focus on the first file",
            "outline": [
                {"id": "s1", "kind": "purpose", "title": "Purpose"},
                {"id": "s2", "kind": "scope", "title": "Scope"},
                {"id": "s3", "kind": "reading_order", "title": "Reading order", "children": [
                    {"id": "s3.1", "kind": "step", "title": "First file"}
                ]}
            ]
        }),
        SESSION,
    );
    assert_eq!(started["version"], 1, "{started}");
    assert_eq!(started["status"], "generating");
    assert_eq!(started["outline"].as_array().map(Vec::len), Some(4));
    assert_eq!(started["outline"][3]["state"]["kind"], "planned");
    let guide = started["guide_id"].as_str().expect("guide id").to_owned();
    let written = harness.json(
        "code_review.guide.write",
        &json!({
            "guide_id": guide,
            "ops": [
                {"op": "put_section", "id": "s1", "body": "Changes the first files."},
                {"op": "put_section", "id": "s3.1", "body": "The change starts here [[1]].",
                 "citations": [{"path": path, "start_line": line, "end_line": line, "quote": quote, "relation_hint": "diff"}]}
            ]
        }),
        SESSION,
    );
    assert_eq!(written["written"], true, "{written}");
    assert_eq!(written["citations"][0]["status"], "verified", "{written}");
    assert_eq!(written["citations"][0]["relation"], "changed", "{written}");
    (guide, path, line)
}

#[test]
fn guide_and_code_tools_are_listed_with_their_permissions() {
    let mut harness = Harness::new();
    harness.bind(SESSION);
    let list: ToolList = harness
        .service(OP_LIST_TOOLS, &json!({}))
        .payload_json()
        .expect("list");
    for name in GUIDE_TOOLS.iter().chain(&CODE_TOOLS) {
        let tool = list
            .tools
            .iter()
            .find(|tool| tool.name == *name)
            .unwrap_or_else(|| panic!("{name} is listed"));
        assert_eq!(tool.input_schema["type"], "object", "{name}");
        let prepared = harness.prepare(name, &json!({"path": "src/lib.rs", "query": "x"}), SESSION);
        let metadata = &prepared.authorization[0].metadata;
        let writes = matches!(*name, "code_review.guide.start" | "code_review.guide.write");
        assert_eq!(
            metadata["permission_category"],
            if writes {
                "code_review.guide"
            } else {
                "code_review.read"
            },
            "{name}"
        );
        assert_eq!(
            metadata["requires_permission"], false,
            "{name} is allowed by default"
        );
        assert_eq!(
            list.discovery.get(*name).expect("discovery").read_only,
            !writes,
            "{name}"
        );
    }
}

#[test]
fn reads_declare_the_paths_they_read_so_deny_rules_apply() {
    let mut harness = Harness::new();
    harness.bind(SESSION);
    let root = harness.fixture.root().to_path_buf();
    let declared = |harness: &mut Harness, tool: &str, arguments: Value| {
        harness.prepare(tool, &arguments, SESSION).authorization[0].metadata["operation"]
            .to_string()
    };
    let secret = root.join("src/.env").display().to_string();
    for (tool, arguments) in [
        (
            "code_review.search_code",
            json!({"query": "x", "paths": ["src/.env"]}),
        ),
        ("code_review.diff_hunks", json!({"path": "src/.env"})),
        ("code_review.related_files", json!({"path": "src/.env"})),
        (
            "code_review.guide.check",
            json!({"citations": [{"path": "src/.env", "start_line": 1, "end_line": 1}]}),
        ),
        (
            "code_review.guide.write",
            json!({"guide_id": "g", "ops": [{"op": "put_section", "id": "s1",
            "citations": [{"path": "src/.env", "start_line": 1, "end_line": 1}]}]}),
        ),
    ] {
        let operation = declared(&mut harness, tool, arguments);
        assert!(operation.contains(&secret), "{tool}: {operation}");
    }
    let whole = declared(
        &mut harness,
        "code_review.search_code",
        json!({"query": "x"}),
    );
    assert!(
        whole.contains(&root.display().to_string()),
        "a search without paths declares the repository: {whole}"
    );
    for (tool, arguments) in [
        (
            "code_review.search_code",
            json!({"query": "x", "paths": ["../outside"]}),
        ),
        ("code_review.diff_hunks", json!({"path": "../outside"})),
    ] {
        assert!(
            harness
                .prepare_raw(tool, &arguments, SESSION)
                .error
                .is_some(),
            "{tool} refuses paths outside the repository"
        );
    }

    std::fs::create_dir_all(root.join("docs/adr")).expect("dir");
    std::fs::write(root.join("docs/adr/.env"), "SECRET=1\n").expect("write");
    std::fs::write(root.join("docs/adr/001.md"), "Use buckets.\n").expect("write");
    harness.guide =
        Some(json!({"context_dirs": ["docs/adr"], "context_files": ["README.guide.md"]}));
    for tool in ["code_review.guide.start", "code_review.guide.plan"] {
        let operation = declared(&mut harness, tool, json!({}));
        for file in ["docs/adr/.env", "docs/adr/001.md", "README.guide.md"] {
            assert!(
                operation.contains(&root.join(file).display().to_string()),
                "{tool} declares {file}: {operation}"
            );
        }
    }
}

#[test]
fn blank_arguments_never_hide_the_paths_a_call_reads() {
    let mut harness = Harness::new();
    harness.bind(SESSION);
    let secret = harness
        .fixture
        .root()
        .join("src/.env")
        .display()
        .to_string();
    let cited = json!([{"path": "src/.env", "side": "", "start_line": 1, "end_line": 1}]);
    for (tool, arguments) in [
        (
            "code_review.guide.write",
            json!({"guide_id": "g", "strict": "", "ops": [{"op": "put_section", "id": "s1",
            "citations": [{"path": "src/.env", "start_line": 1, "end_line": 3}]}]}),
        ),
        (
            "code_review.guide.write",
            json!({"guide_id": "g", "ops": [{"op": "put_section", "id": "s1", "citations": cited}]}),
        ),
        ("code_review.guide.check", json!({"citations": cited})),
        (
            "code_review.guide.check",
            json!({"section": {"id": "s1", "citations": cited}, "version": ""}),
        ),
        (
            "code_review.diff_hunks",
            json!({"path": "src/.env", "side": ""}),
        ),
        (
            "code_review.search_code",
            json!({"query": "x", "mode": "", "paths": ["src/.env"]}),
        ),
    ] {
        let operation =
            harness.prepare(tool, &arguments, SESSION).authorization[0].metadata["operation"]
                .to_string();
        assert!(
            operation.contains(&secret),
            "{tool} {arguments}: {operation}"
        );
    }
    for (tool, arguments) in [
        (
            "code_review.guide.write",
            json!({"guide_id": "g", "strict": "yes", "ops": [{"op": "put_section", "id": "s1",
            "citations": [{"path": "src/.env", "start_line": 1, "end_line": 1}]}]}),
        ),
        (
            "code_review.guide.write",
            json!({"guide_id": "g", "ops": [{"op": "no_such_op"}]}),
        ),
        (
            "code_review.diff_hunks",
            json!({"path": "src/.env", "side": "sideways"}),
        ),
    ] {
        assert!(
            harness
                .prepare_raw(tool, &arguments, SESSION)
                .error
                .is_some(),
            "{tool} {arguments}: unreadable arguments are refused, not declared as nothing"
        );
    }
}

#[test]
fn a_call_naming_more_paths_than_are_declared_is_refused() {
    let mut harness = Harness::new();
    harness.bind(SESSION);
    let mut citations: Vec<Value> = (0..200)
        .map(|index| json!({"path": format!("src/f{index}.rs"), "start_line": 1, "end_line": 1}))
        .collect();
    let at_cap = json!({"guide_id": "g", "ops": [{"op": "put_section", "id": "s1", "citations": citations}]});
    assert!(
        harness
            .prepare_raw("code_review.guide.write", &at_cap, SESSION)
            .error
            .is_none(),
        "200 paths are declared"
    );
    citations.push(json!({"path": "src/.env", "start_line": 1, "end_line": 1}));
    let over = json!({"guide_id": "g", "ops": [{"op": "put_section", "id": "s1", "citations": citations}]});
    let refused = harness.prepare_raw("code_review.guide.write", &over, SESSION);
    assert!(
        refused.error.is_some(),
        "the 201st path is refused, not left undeclared"
    );
    let paths: Vec<String> = (0..201).map(|index| format!("src/f{index}.rs")).collect();
    assert!(
        harness
            .prepare_raw(
                "code_review.search_code",
                &json!({"query": "x", "paths": paths}),
                SESSION
            )
            .error
            .is_some()
    );

    let section: Vec<Value> = (0..101)
        .map(|_| json!({"path": "src/lib.rs", "start_line": 1, "end_line": 1}))
        .collect();
    let checked = harness.call(
        "code_review.guide.check",
        &json!({"section": {"id": "s1", "citations": section}}),
        SESSION,
    );
    assert!(
        checked.is_error && checked.output.contains("at most 100 citations"),
        "a section's citations count toward the check's cap: {}",
        checked.output
    );
}

#[test]
fn a_guide_is_started_written_read_checked_and_exported() {
    let mut harness = Harness::new();
    harness.bind(SESSION);
    let (guide, path, line) = started(&mut harness);

    let read = harness.json(
        "code_review.guide.get",
        &json!({"include": ["outline", "bodies", "citations", "freshness", "versions"]}),
        SESSION,
    );
    assert_eq!(
        read["guide"]["guide_id"],
        guide.as_str(),
        "the only guide is read"
    );
    assert_eq!(
        read["version"]["origin"]["directions"],
        "focus on the first file"
    );
    let step = read["sections"]
        .as_array()
        .expect("sections")
        .iter()
        .find(|section| section["id"] == "s3.1")
        .expect("step");
    assert_eq!(step["body"], "The change starts here [[c1]].");
    assert_eq!(read["citations"]["c1"]["path"], path.as_str());
    assert_eq!(
        read["freshness"]["citations"]["c1"]["kind"], "fresh",
        "{read}"
    );
    assert_eq!(read["versions"].as_array().map(Vec::len), Some(1));

    let checked = harness.json(
        "code_review.guide.check",
        &json!({"citations": [
            {"path": path, "start_line": line, "end_line": line, "quote": "this text is nowhere"},
            {"path": "no/such/file.rs", "start_line": 1, "end_line": 1, "quote": "x"}
        ]}),
        SESSION,
    );
    assert_eq!(checked["citations"][0]["status"], "problem", "{checked}");
    assert_eq!(
        checked["citations"][1]["problems"][0]["code"],
        "path_not_found"
    );

    let linted = harness.json(
        "code_review.guide.check",
        &json!({"section": {"id": "s9", "kind": "risks", "title": "Risks", "body": "No risks found."}}),
        SESSION,
    );
    assert!(
        linted["section"]["lint"]
            .as_array()
            .is_some_and(|items| items.iter().any(|item| item["code"] == "boilerplate")),
        "{linted}"
    );

    harness.json(
        "code_review.guide.write",
        &json!({"guide_id": guide, "ops": [{"op": "set_status", "status": "complete"}]}),
        SESSION,
    );
    let markdown = harness.ok("code_review.guide.export", &json!({}), SESSION);
    assert!(markdown.starts_with("# Walkthrough · v1"), "{markdown}");
    assert!(
        markdown.contains("The change starts here [1]."),
        "{markdown}"
    );
    let head = harness.fixture.git(&["rev-parse", "HEAD"]);
    assert!(
        markdown.contains(&format!("1. `{path}:{line}` @ {}", &head.trim()[..7])),
        "{markdown}"
    );
    assert!(
        markdown.contains("_Not written yet._"),
        "planned sections say so: {markdown}"
    );

    let again = harness.json("code_review.guide.start", &json!({"copy": true}), SESSION);
    assert_eq!(
        again["version"], 2,
        "the scope's guide gets a new version: {again}"
    );
    assert_eq!(again["based_on"], 1);
    assert_eq!(
        again["outline"].as_array().map(Vec::len),
        Some(4),
        "a copy keeps sections"
    );
}

#[test]
fn an_operation_outside_the_grant_is_refused_and_nothing_is_written() {
    let mut harness = Harness::new();
    harness.bind(SESSION);
    let (guide, _, _) = started(&mut harness);
    grant(
        &harness,
        SESSION,
        Some(GuideGrant {
            guide_id: guide.clone(),
            version: None,
            mode: GrantMode::Expand {
                section_id: "s3.1".to_owned(),
            },
        }),
    );
    let before = harness
        .store()
        .guide_version(&GetGuideVersionRequest {
            repo_path: harness.fixture.root().to_path_buf(),
            guide_id: guide.clone(),
            version: None,
        })
        .expect("read")
        .version
        .expect("version");
    let refused = harness.call(
        "code_review.guide.write",
        &json!({"guide_id": guide, "ops": [
            {"op": "add_expansion", "under": "s3.1", "title": "Why"},
            {"op": "put_section", "id": "s1", "body": "Rewritten."}
        ]}),
        SESSION,
    );
    assert!(refused.is_error, "{}", refused.output);
    let body: Value = serde_json::from_str(&refused.output).expect("json");
    assert_eq!(body["written"], false);
    assert_eq!(body["errors"][0]["op_index"], 1);
    assert_eq!(body["errors"][0]["code"], "outside_grant");
    assert!(
        body["errors"][0]["message"]
            .as_str()
            .is_some_and(|message| message.contains("s3.1")),
        "{body}"
    );
    let after = harness
        .store()
        .guide_version(&GetGuideVersionRequest {
            repo_path: harness.fixture.root().to_path_buf(),
            guide_id: guide.clone(),
            version: None,
        })
        .expect("read")
        .version
        .expect("version");
    assert_eq!(after.revision, before.revision, "nothing was written");

    let expanded = harness.json(
        "code_review.guide.write",
        &json!({"guide_id": guide, "ops": [{"op": "add_expansion", "under": "s3.1", "title": "Why"}]}),
        SESSION,
    );
    assert_eq!(expanded["sections_written"][0], "s3.1.x1");

    let other = "22222222-2222-4333-8444-555555555555";
    harness.bind(other);
    let foreign = harness.call(
        "code_review.guide.write",
        &json!({"guide_id": guide, "ops": [{"op": "put_section", "id": "s1", "body": "x"}]}),
        other,
    );
    assert!(
        foreign.is_error,
        "a session without a grant may not write another's version"
    );
    assert!(
        foreign.output.contains("outside_grant"),
        "{}",
        foreign.output
    );
    let refused = harness.call(
        "code_review.guide.start",
        &json!({"mode": "resume", "guide_id": guide}),
        SESSION,
    );
    assert!(
        refused.is_error && refused.output.contains("outside_grant"),
        "{}",
        refused.output
    );
}

#[test]
fn strict_writes_reject_the_whole_batch_when_a_citation_has_a_problem() {
    let mut harness = Harness::new();
    harness.bind(SESSION);
    let (guide, path, line) = started(&mut harness);
    let refused = harness.call(
        "code_review.guide.write",
        &json!({"guide_id": guide, "strict": true, "ops": [
            {"op": "put_section", "id": "s2", "body": "Fine."},
            {"op": "put_section", "id": "s3.1", "citations": [
                {"path": path, "start_line": line, "end_line": line, "quote": "not at these lines anywhere"}
            ]}
        ]}),
        SESSION,
    );
    assert!(refused.is_error, "{}", refused.output);
    let body: Value = serde_json::from_str(&refused.output).expect("json");
    assert_eq!(body["code"], "citation_problems");
    let read = harness.json(
        "code_review.guide.get",
        &json!({"sections": ["s2"]}),
        SESSION,
    );
    let scope = read["sections"]
        .as_array()
        .expect("sections")
        .iter()
        .find(|section| section["id"] == "s2")
        .expect("scope");
    assert_eq!(
        scope["state"]["kind"], "planned",
        "nothing was written: {scope}"
    );

    let kept = harness.json(
        "code_review.guide.write",
        &json!({"guide_id": guide, "ops": [
            {"op": "put_section", "id": "s3.1", "citations": [
                {"path": path, "start_line": line, "end_line": line, "quote": "not at these lines anywhere"}
            ]}
        ]}),
        SESSION,
    );
    assert_eq!(
        kept["citations"][0]["status"], "problem",
        "without strict the citation is kept: {kept}"
    );
}

#[test]
fn guide_reads_are_bounded_and_name_what_was_cut() {
    let mut harness = Harness::new();
    harness.bind(SESSION);
    let started = harness.json("code_review.guide.start", &json!({}), SESSION);
    let guide = started["guide_id"].as_str().expect("id").to_owned();
    let body = "word ".repeat(3_000);
    for batch in 0..4 {
        let ops: Vec<Value> = (0..10)
            .map(|n| json!({"op": "put_section", "id": format!("s{batch}_{n}"), "kind": "note", "title": "Note", "body": &body}))
            .collect();
        harness.json(
            "code_review.guide.write",
            &json!({"guide_id": guide, "ops": ops}),
            SESSION,
        );
    }
    let output = harness.ok("code_review.guide.get", &json!({}), SESSION);
    assert!(output.len() <= 128 * 1024, "{}", output.len());
    let read: Value = serde_json::from_str(&output).expect("whole JSON, cut by section");
    let cut = read["cut"][0].as_str().expect("named cut");
    assert!(
        cut.starts_with("bodies of ") && cut.contains("s3_9"),
        "{cut}"
    );
    assert_eq!(
        read["sections"].as_array().map(Vec::len),
        Some(40),
        "the outline is whole"
    );
    let one = harness.json(
        "code_review.guide.get",
        &json!({"sections": ["s3_9"]}),
        SESSION,
    );
    assert!(one.get("cut").is_none(), "{one}");
}

#[test]
fn plan_gives_one_agent_for_a_small_review_and_areas_for_a_large_multi_package_one() {
    let mut harness = Harness::new();
    harness.bind(SESSION);
    let small = harness.json("code_review.guide.plan", &json!({}), SESSION);
    assert_eq!(small["plan"]["strategy"], "single", "{small}");
    assert_eq!(
        small["template"]["required_sections"],
        json!(["purpose", "scope", "reading_order"])
    );

    let fixture = GitFixture::empty();
    let write = |path: &str, lines: usize, salt: &str| {
        let full = fixture.root().join(path);
        std::fs::create_dir_all(full.parent().expect("parent")).expect("dir");
        let text = (0..lines).fold(String::new(), |mut text, n| {
            use std::fmt::Write as _;
            let _ = writeln!(text, "line {n} {salt}");
            text
        });
        std::fs::write(full, text).expect("write");
    };
    for package in ["crates/net", "crates/store", "web"] {
        let manifest = if package == "web" {
            "package.json"
        } else {
            "Cargo.toml"
        };
        write(&format!("{package}/{manifest}"), 1, "manifest");
        for n in 0..20 {
            write(&format!("{package}/src/m{n}.rs"), 40, "old");
        }
    }
    fixture.run(&["add", "."]);
    fixture.run(&["commit", "-q", "-m", "base"]);
    for package in ["crates/net", "crates/store", "web"] {
        for n in 0..20 {
            write(&format!("{package}/src/m{n}.rs"), 40, "new");
        }
    }
    fixture.run(&["commit", "-q", "-am", "change"]);
    let mut large = Harness::with_fixture(fixture);
    large.bind(SESSION);
    let plan = large.json("code_review.guide.plan", &json!({}), SESSION);
    assert_eq!(plan["plan"]["strategy"], "areas", "{plan}");
    let areas: Vec<&str> = plan["plan"]["areas"]
        .as_array()
        .expect("areas")
        .iter()
        .filter_map(|area| area["area_id"].as_str())
        .collect();
    assert_eq!(areas, ["crates/net", "crates/store", "web"]);
    let single = large.json(
        "code_review.guide.plan",
        &json!({"strategy": "single"}),
        SESSION,
    );
    assert_eq!(single["plan"]["strategy"], "single");
}

#[test]
fn code_is_searched_and_read_at_the_reviews_revisions() {
    let mut harness = Harness::new();
    harness.bind(SESSION);
    let path = GitFixture::modified_path(0);
    let line = harness.added_line(&path);
    let quote = line_text(&harness, &path, line);
    let found = harness.json(
        "code_review.search_code",
        &json!({"query": quote.trim(), "paths": ["src/**"]}),
        SESSION,
    );
    let hit = found["hits"]
        .as_array()
        .expect("hits")
        .iter()
        .find(|hit| hit["path"] == path.as_str() && hit["line"] == line)
        .unwrap_or_else(|| panic!("{found}"));
    assert_eq!(hit["in_review"], true);
    assert_eq!(hit["relation"], "changed");

    let bad = harness.call(
        "code_review.search_code",
        &json!({"query": "a b", "mode": "symbol"}),
        SESSION,
    );
    assert!(
        bad.is_error && bad.output.contains("identifier"),
        "{}",
        bad.output
    );

    let hunks = harness.ok(
        "code_review.diff_hunks",
        &json!({"path": path, "start_line": line, "end_line": line}),
        SESSION,
    );
    assert!(hunks.contains("@@ -"), "{hunks}");
    assert!(hunks.contains(quote.trim()), "{hunks}");
    let none = harness.ok(
        "code_review.diff_hunks",
        &json!({"path": path, "start_line": 100_000, "end_line": 100_001}),
        SESSION,
    );
    assert!(none.contains("no hunk overlaps"), "{none}");

    let related = harness.json("code_review.related_files", &json!({"path": path}), SESSION);
    assert_eq!(related["path"], path.as_str(), "{related}");

    let base = harness.fixture.git(&["rev-parse", "HEAD~1"]);
    let older = harness.ok(
        "code_review.file_content",
        &json!({"path": path, "revision": &base.trim()[..10], "start_line": 1, "count": 1}),
        SESSION,
    );
    assert!(older.contains(&format!("{path} at")), "{older}");
    let unknown = harness.call(
        "code_review.file_content",
        &json!({"path": path, "revision": "deadbeefdeadbeef"}),
        SESSION,
    );
    assert!(
        unknown.is_error && unknown.output.contains("not a version"),
        "{}",
        unknown.output
    );
}

/// Version `number` of `guide`, as stored.
fn stored_version(
    harness: &Harness,
    guide: &str,
    number: Option<u32>,
) -> bcode_code_review_models::GuideVersionRecord {
    harness
        .store()
        .guide_version(&GetGuideVersionRequest {
            repo_path: harness.fixture.root().to_path_buf(),
            guide_id: guide.to_owned(),
            version: number,
        })
        .expect("read")
        .version
        .expect("version")
}

/// The refusal code of a call that failed.
fn refusal(harness: &mut Harness, tool: &str, arguments: &Value, session: &str) -> String {
    let response = harness.call(tool, arguments, session);
    assert!(response.is_error, "{tool} {arguments}: {}", response.output);
    response.output
}

#[test]
fn a_review_named_by_id_is_written_under_the_sessions_grant() {
    let mut harness = Harness::new();
    harness.bind(SESSION);
    let review = harness.json(
        "code_review.create_review",
        &json!({"sources": [{"kind": "last_commit"}]}),
        SESSION,
    )["review"]["id"]
        .as_str()
        .expect("id")
        .to_owned();
    let started = harness.json(
        "code_review.guide.start",
        &json!({"review": review, "outline": [
            {"id": "s1", "kind": "purpose", "title": "Purpose"},
            {"id": "s2", "kind": "scope", "title": "Scope"}
        ]}),
        SESSION,
    );
    let guide = started["guide_id"].as_str().expect("guide").to_owned();
    harness.json(
        "code_review.guide.write",
        &json!({"review": review, "guide_id": guide, "ops": [{"op": "set_status", "status": "complete"}]}),
        SESSION,
    );
    grant(
        &harness,
        SESSION,
        Some(GuideGrant {
            guide_id: guide.clone(),
            version: None,
            mode: GrantMode::Expand {
                section_id: "s2".to_owned(),
            },
        }),
    );
    let before = stored_version(&harness, &guide, None);
    for (tool, arguments) in [
        (
            "code_review.guide.start",
            json!({"review": review, "guide_id": guide, "mode": "resume"}),
        ),
        (
            "code_review.guide.start",
            json!({"review": review, "guide_id": guide, "mode": "new_version"}),
        ),
        (
            "code_review.guide.write",
            json!({"review": review, "guide_id": guide, "ops": [{"op": "put_section", "id": "s1", "body": "Rewritten."}]}),
        ),
    ] {
        let output = refusal(&mut harness, tool, &arguments, SESSION);
        assert!(output.contains("outside_grant"), "{tool}: {output}");
    }
    let after = stored_version(&harness, &guide, None);
    assert_eq!(after.revision, before.revision, "nothing was written");
    assert_eq!(after.version.status.as_str(), "complete");
    let expanded = harness.json(
        "code_review.guide.write",
        &json!({"review": review, "guide_id": guide, "ops": [{"op": "add_expansion", "under": "s2", "title": "Why"}]}),
        SESSION,
    );
    assert_eq!(expanded["written"], true, "{expanded}");

    let output = refusal(
        &mut harness,
        "code_review.guide.start",
        &json!({"title": "Another"}),
        SESSION,
    );
    assert!(
        output.contains("outside_grant"),
        "a granted session starts no other guide: {output}"
    );
}

#[test]
fn resuming_takes_over_only_versions_the_session_may_write() {
    let mut harness = Harness::new();
    harness.bind(SESSION);
    let (guide, _, _) = started(&mut harness);
    let second = harness.json(
        "code_review.guide.start",
        &json!({"guide_id": guide, "mode": "new_version", "copy": true}),
        SESSION,
    );
    assert_eq!(second["version"], 2);

    grant(
        &harness,
        SESSION,
        Some(GuideGrant {
            guide_id: guide.clone(),
            version: Some(1),
            mode: GrantMode::Generate,
        }),
    );
    for arguments in [
        json!({"guide_id": guide, "mode": "resume", "version": 2}),
        json!({"guide_id": guide, "mode": "new_version"}),
    ] {
        let output = refusal(&mut harness, "code_review.guide.start", &arguments, SESSION);
        assert!(output.contains("version 1"), "{arguments}: {output}");
    }
    let resumed = harness.json(
        "code_review.guide.start",
        &json!({"guide_id": guide, "mode": "resume", "version": 1}),
        SESSION,
    );
    assert_eq!(resumed["version"], 1, "{resumed}");
    assert_eq!(
        stored_version(&harness, &guide, Some(2))
            .version
            .status
            .as_str(),
        "generating",
        "the version the grant does not name is left to its writer"
    );
    grant(&harness, SESSION, None);

    let other = "22222222-2222-4333-8444-555555555555";
    harness.bind(other);
    let resume = json!({"guide_id": guide, "mode": "resume", "version": 2});
    let output = refusal(&mut harness, "code_review.guide.start", &resume, other);
    assert!(
        output.contains("in_use"),
        "a version being written is not taken over: {output}"
    );
    harness.json(
        "code_review.guide.write",
        &json!({"guide_id": guide, "version": 2, "ops": [{"op": "set_status", "status": "complete"}]}),
        SESSION,
    );
    let output = refusal(&mut harness, "code_review.guide.start", &resume, other);
    assert!(
        output.contains("new_version"),
        "a complete version of another conversation is not reopened: {output}"
    );
    harness.json(
        "code_review.guide.write",
        &json!({"guide_id": guide, "version": 2, "ops": [{"op": "set_status", "status": "failed"}]}),
        SESSION,
    );
    let taken = harness.json("code_review.guide.start", &resume, other);
    assert_eq!(
        taken["status"], "generating",
        "a failed version is resumed: {taken}"
    );
    assert_eq!(
        stored_version(&harness, &guide, Some(2))
            .version
            .origin
            .session_id
            .as_deref(),
        Some(other)
    );
    let output = refusal(&mut harness, "code_review.guide.start", &resume, SESSION);
    assert!(output.contains("in_use"), "{output}");
    harness.json(
        "code_review.guide.write",
        &json!({"guide_id": guide, "version": 2, "ops": [{"op": "set_status", "status": "cancelled"}]}),
        other,
    );
    let back = harness.json("code_review.guide.start", &resume, SESSION);
    assert_eq!(back["version"], 2, "a stopped version is resumed: {back}");
}
