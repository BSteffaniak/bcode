//! Browser transcript feed against a real in-process backend: bounded history pages executed by
//! the shared engine loader, pushed live input with resume and duplicate tolerance, slim
//! snapshots, and caps/errors. The browser side decodes with `bcode_tui_web::feed`.
use super::{BTreeMap, BcodeClient, Path, SessionEventKind, SessionId, TestResult, json, require};
use bcode_hyperchad::{HyperChadAppState, router_from_state};
use bcode_tui_transcript::engine::HostRequest;
use bcode_tui_transcript::window::{
    HistoryLoadRequest, HistoryLoaded, IssuedRequest, LiveInput, LiveOutcome, PageRequest,
    WindowManager, WindowRequest, load_history,
};
use bcode_tui_web::feed::{
    Continuity, HISTORY_PAGE_ROUTE, HistoryPageRequest, LIVE_ROUTE, LiveBatch, LiveRequest,
    MAX_HISTORY_READ_EVENTS, SLIM_SNAPSHOT_QUERY, decode_history_page, decode_live_batch,
};
use hyperchad::renderer::Content;
use hyperchad::router::{RequestInfo, RouteRequest, Router};
use serde_json::Value;
use std::time::Duration;

const TOKEN: &str = "feed-test-token";

async fn send(router: &Router, path: &str, body: Option<Vec<u8>>) -> TestResult<Vec<u8>> {
    let mut request = RouteRequest::from_path(
        &format!(
            "{path}{}token={TOKEN}",
            if path.contains('?') { '&' } else { '?' }
        ),
        RequestInfo::default(),
    );
    if let Some(body) = body {
        request.method = "POST".parse()?;
        request
            .headers
            .insert("content-type".into(), "application/json".into());
        request.body = Some(std::sync::Arc::new(body.into()));
    }
    let content = router
        .navigate(request)
        .await
        .map_err(|error| std::io::Error::other(error.to_string()))?
        .ok_or("route returned no content")?;
    let Content::Raw { data, .. } = content else {
        return Err("route returned non-JSON".into());
    };
    Ok(data.to_vec())
}

async fn live(
    router: &Router,
    session_id: SessionId,
    cursor: Option<bcode_tui_web::feed::LiveCursor>,
) -> TestResult<LiveBatch> {
    let body = serde_json::to_vec(&LiveRequest { session_id, cursor })?;
    Ok(decode_live_batch(
        &send(router, LIVE_ROUTE, Some(body)).await?,
    )?)
}

/// The shared loader over the browser route, exactly as the WASM host reads.
async fn load_via_route(router: &Router, load: HistoryLoadRequest) -> TestResult<HistoryLoaded> {
    Ok(load_history(load, |query| async move {
        let body = serde_json::to_vec(&HistoryPageRequest {
            session_id: load.session_id,
            query,
        })
        .map_err(|error| error.to_string())?;
        let bytes = send(router, HISTORY_PAGE_ROUTE, Some(body))
            .await
            .map_err(|error| error.to_string())?;
        decode_history_page(&bytes).map_err(|error| error.to_string())
    })
    .await
    .map_err(|error| error.to_string())?)
}

/// The same loader over the daemon client, exactly as the native host reads.
async fn load_native(client: &BcodeClient, load: HistoryLoadRequest) -> TestResult<HistoryLoaded> {
    Ok(load_history(load, |query| {
        client.session_history_page(load.session_id, query)
    })
    .await
    .map_err(|error| error.to_string())?)
}

fn durable_sequences(inputs: &[LiveInput]) -> Vec<u64> {
    inputs
        .iter()
        .filter_map(|input| match input {
            LiveInput::Durable(event) => Some(event.sequence),
            LiveInput::Stream(_) => None,
        })
        .collect()
}

fn answered(inputs: &[LiveInput], text: &str) -> bool {
    inputs.iter().any(|input| {
        matches!(input, LiveInput::Durable(event) if matches!(
            &event.kind,
            SessionEventKind::AssistantMessage { text: actual }
            | SessionEventKind::AssistantResponseSegment { text: actual, .. }
            | SessionEventKind::PositionedAssistantResponseSegment { text: actual, .. }
                if actual == text
        ))
    })
}

async fn exercise(client: &BcodeClient, root: &Path) -> TestResult<()> {
    super::ready(client).await?;
    let session = client
        .create_session_in_working_directory(Some("browser-feed".into()), root.to_path_buf())
        .await?
        .id;
    let state = HyperChadAppState::new_in_working_directory(client.clone(), TOKEN, root.into());
    let router = &router_from_state(state);
    let (cursor, replayed) = stream_turn(router, session).await?;
    history_round_trip(router, client, session, replayed).await?;
    slim_snapshot_and_caps(router, session).await?;
    let large = client
        .create_session_in_working_directory(Some("browser-feed-large".into()), root.to_path_buf())
        .await?
        .id;
    oversized_tail_loads_in_byte_bounded_pages(router, client, large).await?;
    restart_reports_gap(client, root, session, cursor).await
}

/// A tail whose bounded event count serializes beyond the browser response limit still loads:
/// the route cuts each read to its byte budget and reports more history, so the shared loader
/// pages through it instead of failing (and retrying) without end.
async fn oversized_tail_loads_in_byte_bounded_pages(
    router: &Router,
    client: &BcodeClient,
    session: SessionId,
) -> TestResult<()> {
    let tail = HistoryLoadRequest {
        session_id: session,
        request: HostRequest::Window(IssuedRequest {
            generation: 1,
            request: WindowRequest::Tail,
        }),
        resident_last: None,
    };
    // Each turn is about 120 KB of canonical events (the prompt and its echo); stop once the
    // tail exceeds the browser response limit, well before the fake model's context fills.
    let filler = "large reply ".repeat(5_000);
    for turn in 0..20 {
        let HistoryLoaded::Window(native) = load_native(client, tail).await? else {
            return Err("tail window expected".into());
        };
        if serde_json::to_vec(&native.events)?.len() > 2 * 1024 * 1024 + 128 * 1024 {
            break;
        }
        let text = format!("turn {turn} {filler}");
        let submitted: Value = serde_json::from_slice(
            &send(
                router,
                "/api/session-view/submit-message",
                Some(serde_json::to_vec(&json!({
                    "submission_id": format!("large-{turn}"), "session_id": session, "text": text,
                }))?),
            )
            .await?,
        )?;
        require(
            submitted["outcome"].is_object(),
            &format!("submission accepted: {submitted}"),
        )?;
        let reply = format!("fake: {text}");
        tokio::time::timeout(Duration::from_secs(60), async {
            loop {
                let HistoryLoaded::Window(window) = load_native(client, tail).await? else {
                    return Err::<(), Box<dyn std::error::Error + Send + Sync>>(
                        "tail window expected".into(),
                    );
                };
                let inputs = window
                    .events
                    .into_iter()
                    .map(|event| LiveInput::Durable(Box::new(event)))
                    .collect::<Vec<_>>();
                if answered(&inputs, &reply) {
                    return Ok(());
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await??;
    }
    let HistoryLoaded::Window(native) = load_native(client, tail).await? else {
        return Err("tail window expected".into());
    };
    require(
        serde_json::to_vec(&native.events)?.len() > 2 * 1024 * 1024,
        "the native tail exceeds the browser response limit",
    )?;
    let HistoryLoaded::Window(browser) = load_via_route(router, tail).await? else {
        return Err("tail window expected".into());
    };
    require(
        !browser.events.is_empty()
            && browser.events.len() < native.events.len()
            && native.events.ends_with(&browser.events)
            && browser.has_older,
        "the browser tail is the newest byte-bounded part of the native tail",
    )?;
    let first = browser.events[0].sequence;
    let HistoryLoaded::Page(older) = load_via_route(
        router,
        HistoryLoadRequest {
            request: HostRequest::Page(IssuedRequest {
                generation: 1,
                request: PageRequest::Older {
                    before_sequence: first,
                },
            }),
            ..tail
        },
    )
    .await?
    else {
        return Err("older page expected".into());
    };
    require(
        older.events.last().map(|event| event.sequence) == Some(first - 1),
        "paging continues directly before the cut",
    )?;
    Ok(())
}

/// Subscribe, submit one turn and follow it live; then replay it from the start cursor.
/// Returns the final cursor and the replayed inputs.
async fn stream_turn(
    router: &Router,
    session: SessionId,
) -> TestResult<(bcode_tui_web::feed::LiveCursor, Vec<LiveInput>)> {
    // Subscribe before anything happens: the start cursor covers every later input.
    let start = live(router, session, None).await?;
    require(
        start.continuity == Continuity::Start && start.inputs.is_empty(),
        "subscription must start empty at the current cursor",
    )?;
    let text = "browser feed 日本語 🦀 e\u{301}";
    let submitted: Value = serde_json::from_slice(
        &send(
            router,
            "/api/session-view/submit-message",
            Some(serde_json::to_vec(
                &json!({"submission_id": "feed-1", "session_id": session, "text": text}),
            )?),
        )
        .await?,
    )?;
    require(
        submitted["outcome"].is_object(),
        &format!("submission accepted: {submitted}"),
    )?;
    let reply = format!("fake: {text}");
    let mut inputs = Vec::new();
    let mut cursor = start.cursor;
    tokio::time::timeout(Duration::from_secs(60), async {
        while !answered(&inputs, &reply) {
            let batch = live(router, session, Some(cursor)).await?;
            require(
                batch.continuity == Continuity::Continuous,
                "an attached feed resumes continuously",
            )?;
            require(
                batch.cursor.epoch == cursor.epoch && batch.cursor.offset >= cursor.offset,
                "cursors advance monotonically within one epoch",
            )?;
            cursor = batch.cursor;
            inputs.extend(batch.inputs);
        }
        Ok::<_, Box<dyn std::error::Error + Send + Sync>>(())
    })
    .await??;
    let sequences = durable_sequences(&inputs);
    require(
        sequences.windows(2).all(|pair| pair[0] < pair[1]),
        "durable live inputs arrive in canonical order",
    )?;

    // Lost response: re-polling the old cursor re-delivers the same inputs in order.
    let mut replayed = Vec::new();
    let mut replay_cursor = start.cursor;
    while replay_cursor != cursor {
        let batch = live(router, session, Some(replay_cursor)).await?;
        replay_cursor = batch.cursor;
        replayed.extend(batch.inputs);
    }
    require(replayed == inputs, "replay from a retained cursor is exact")?;
    Ok((cursor, replayed))
}

/// The browser route and the daemon client give the shared loader identical results, and the
/// engine window drops replayed durable inputs as duplicates.
async fn history_round_trip(
    router: &Router,
    client: &BcodeClient,
    session: SessionId,
    replayed: Vec<LiveInput>,
) -> TestResult<()> {
    // History pages: the browser route and the daemon client give one loader identical results.
    let tail = HistoryLoadRequest {
        session_id: session,
        request: HostRequest::Window(IssuedRequest {
            generation: 3,
            request: WindowRequest::Tail,
        }),
        resident_last: None,
    };
    // The turn may still append bookkeeping events; compare once history is quiescent.
    let browser_tail = tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let before = load_native(client, tail).await?;
            let browser = load_via_route(router, tail).await?;
            if before == load_native(client, tail).await? {
                require(
                    browser == before,
                    "browser and native history loads are identical",
                )?;
                return Ok::<_, Box<dyn std::error::Error + Send + Sync>>(browser);
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await??;
    let HistoryLoaded::Window(window) = &browser_tail else {
        return Err("tail window expected".into());
    };
    let last = window.events.last().ok_or("tail has events")?.sequence;
    let older = HistoryLoadRequest {
        request: HostRequest::Page(IssuedRequest {
            generation: 3,
            request: PageRequest::Older {
                before_sequence: last,
            },
        }),
        ..tail
    };
    let browser_older = load_via_route(router, older).await?;
    require(
        browser_older == load_native(client, older).await?,
        "older pages are identical across hosts",
    )?;

    // Duplicate tolerance: the engine window drops live durable events already resident.
    let mut manager = WindowManager::new();
    let mut view = bcode_session_view::SessionView::new();
    let issued = manager.request_window(WindowRequest::Tail);
    let HistoryLoaded::Window(result) = load_via_route(
        router,
        HistoryLoadRequest {
            request: HostRequest::Window(issued),
            ..tail
        },
    )
    .await?
    else {
        return Err("tail window expected".into());
    };
    manager
        .apply_window(&result, &mut view)
        .map_err(|stale| format!("{stale:?}"))?;
    for input in replayed
        .into_iter()
        .filter(|input| matches!(input, LiveInput::Durable(_)))
    {
        require(
            manager.apply_live(input, &mut view) == LiveOutcome::Duplicate,
            "replayed durable inputs are duplicates of the resident window",
        )?;
    }
    Ok(())
}

/// Slim snapshots and history read caps/errors.
async fn slim_snapshot_and_caps(router: &Router, session: SessionId) -> TestResult<()> {
    // Slim snapshots keep chrome state without transcript items.
    let full: Value = serde_json::from_slice(
        &send(router, &format!("/api/session-view/{session}"), None).await?,
    )?;
    let slim: Value = serde_json::from_slice(
        &send(
            router,
            &format!("/api/session-view/{session}?{SLIM_SNAPSHOT_QUERY}"),
            None,
        )
        .await?,
    )?;
    require(
        full["snapshot"]["transcript"]["items"]
            .as_array()
            .is_some_and(|items| !items.is_empty()),
        &format!("full snapshot has transcript items: {full}"),
    )?;
    require(
        slim["snapshot"]["transcript"]["items"]
            .as_array()
            .is_some_and(Vec::is_empty),
        &format!("slim snapshot omits transcript items: {slim}"),
    )?;
    for field in [
        "permissions",
        "interactions",
        "runtime",
        "execution_lifecycle",
    ] {
        require(
            slim["snapshot"][field] == full["snapshot"][field],
            &format!("slim snapshot keeps {field}"),
        )?;
    }

    // Caps and errors use the shared envelopes.
    let read = |limit: usize, session_id: SessionId| {
        serde_json::to_vec(&json!({
            "session_id": session_id,
            "query": {"cursor": null, "limit": limit, "direction": "backward"},
        }))
    };
    let at_cap = send(
        router,
        HISTORY_PAGE_ROUTE,
        Some(read(MAX_HISTORY_READ_EVENTS, session)?),
    )
    .await?;
    require(
        decode_history_page(&at_cap).is_ok(),
        "a read at the engine cap is served",
    )?;
    for (body, expected) in [
        (
            read(MAX_HISTORY_READ_EVENTS + 1, session)?,
            "history_limit_exceeded",
        ),
        (read(1, SessionId::new())?, "history_unavailable"),
    ] {
        require(
            decode_history_page(&send(router, HISTORY_PAGE_ROUTE, Some(body)).await?)
                == Err(bcode_tui_web::feed::FeedError::Server(expected.into())),
            expected,
        )?;
    }
    Ok(())
}

async fn restart_reports_gap(
    client: &BcodeClient,
    root: &Path,
    session: SessionId,
    cursor: bcode_tui_web::feed::LiveCursor,
) -> TestResult<()> {
    // A restarted web host has a new feed epoch: the old cursor is an explicit gap, and
    // polling continues from the new cursor.
    let restarted = router_from_state(HyperChadAppState::new_in_working_directory(
        client.clone(),
        TOKEN,
        root.into(),
    ));
    let gap = live(&restarted, session, Some(cursor)).await?;
    require(
        gap.continuity == Continuity::Gap
            && gap.inputs.is_empty()
            && gap.cursor.epoch != cursor.epoch,
        "a foreign epoch cursor reports a gap with the current cursor",
    )?;
    let resumed = tokio::time::timeout(
        Duration::from_secs(30),
        live(&restarted, session, Some(gap.cursor)),
    )
    .await??;
    require(
        resumed.continuity == Continuity::Continuous && resumed.cursor.epoch == gap.cursor.epoch,
        "polling resumes continuously from the gap cursor",
    )?;
    Ok(())
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn feed_routes_round_trip_against_a_real_backend() -> TestResult<()> {
    use std::os::unix::fs::PermissionsExt as _;

    let directory = tempfile::tempdir()?;
    std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700))?;
    let locations = bcode_config::resolve_state_location_set_with_environment(
        &bcode_config::StateConfig::default(),
        &bcode_config::StateLocationSelection {
            root: Some(directory.path().join("state")),
            profile: None,
        },
        &bcode_config::ConfigEnvironmentSnapshot::new(
            BTreeMap::new(),
            directory.path().to_path_buf(),
        ),
    )?;
    let endpoint = bcode_ipc::IpcEndpoint::unix_socket(directory.path().join("feed.sock"));
    // No ambient user configuration: the client presents exactly the daemon's startup
    // configuration, as a launcher of this isolated backend would.
    let config = super::config();
    // The daemon starts with the selection the presented configuration derives, so the
    // client's configuration is compatible by construction.
    let selection =
        bcode_config::plugin_selection_with_default_plugin_ids(&config, super::plugin_ids());
    let context = bcode_ipc::ClientRuntimeContext {
        working_directory: Some(directory.path().to_path_buf()),
        effective_config_toml: Some(Box::new(bcode_config::encode_effective_config(&config)?)),
        selected_provider_plugin_id: Some("bcode.fake-provider".into()),
        selected_model_id: Some("fake-echo".into()),
        ..bcode_ipc::ClientRuntimeContext::default()
    };
    let client = BcodeClient::for_state_location(endpoint.clone(), locations.primary())
        .with_runtime_context(Some(context));
    let shutdown = bcode_agent_runtime::CancellationToken::new();
    let server = bcode_server::run_embedded_with_locations_and_shutdown(
        endpoint,
        config.clone(),
        bcode_plugin::PluginRuntimeHost::load_defaults_with_static_bundled(
            &selection,
            &super::static_plugins(),
        )?,
        bcode_model_catalog::ModelCatalogResolver::embedded(),
        super::plugin_ids(),
        shutdown.clone(),
        Some(locations),
    );
    let work = async {
        let result = exercise(&client, directory.path()).await;
        shutdown.cancel();
        result
    };
    let (server_result, result) = tokio::join!(server, work);
    result?;
    server_result?;
    Ok(())
}
