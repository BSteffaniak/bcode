//! Offline loopback transport tests. Production configuration still requires HTTPS.

use super::*;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::thread::{self, JoinHandle};
use std::time::Instant;

struct Step {
    path: &'static str,
    status: u16,
    response: serde_json::Value,
    check: fn(serde_json::Value),
}

struct Mock {
    broker: Broker,
    worker: JoinHandle<()>,
}
impl Mock {
    fn new(steps: Vec<Step>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("loopback listener");
        listener.set_nonblocking(true).expect("bounded accept");
        let url = Url::parse(&format!(
            "http://{}/",
            listener.local_addr().expect("address")
        ))
        .expect("test URL");
        let worker = thread::spawn(move || {
            for step in steps {
                let deadline = Instant::now() + Duration::from_secs(10);
                let mut stream = loop {
                    match listener.accept() {
                        Ok((stream, _)) => break stream,
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            assert!(
                                Instant::now() < deadline,
                                "expected mock request did not arrive"
                            );
                            thread::sleep(Duration::from_millis(5));
                        }
                        Err(error) => panic!("accept failed: {error}"),
                    }
                };
                stream.set_nonblocking(false).expect("blocking mock stream");
                stream
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .expect("read bound");
                stream
                    .set_write_timeout(Some(Duration::from_secs(5)))
                    .expect("write bound");
                let (path, body) = read_request(&mut stream);
                assert_eq!(path, step.path);
                (step.check)(body);
                let response = serde_json::to_vec(&step.response).expect("response encoding");
                write!(stream, "HTTP/1.1 {} Mock\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    step.status, response.len()).expect("response headers");
                stream.write_all(&response).expect("response body");
            }
        });
        Self {
            broker: Broker {
                url,
                token: "synthetic-lease".to_owned(),
                expires_at: now() + 300,
            },
            worker,
        }
    }
    fn finish(self) {
        self.worker.join().expect("mock request validation");
    }
}

fn read_request(stream: &mut TcpStream) -> (String, serde_json::Value) {
    let mut bytes = Vec::new();
    let header_end = loop {
        let mut chunk = [0; 1024];
        let count = stream.read(&mut chunk).expect("read request");
        assert_ne!(count, 0, "incomplete headers");
        bytes.extend_from_slice(&chunk[..count]);
        assert!(bytes.len() <= LIMIT, "bounded request");
        if let Some(index) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
            break index + 4;
        }
    };
    let headers = String::from_utf8(bytes[..header_end].to_vec()).expect("HTTP headers");
    let mut lines = headers.lines();
    let first = lines.next().expect("request line");
    let mut parts = first.split_whitespace();
    assert_eq!(parts.next(), Some("POST"));
    let path = parts.next().expect("route").to_owned();
    let mut length = 0;
    let mut authorization = Vec::new();
    for line in lines {
        if let Some((name, value)) = line.split_once(':') {
            match name.to_ascii_lowercase().as_str() {
                "content-length" => length = value.trim().parse().expect("body length"),
                "authorization" => authorization.push(value.trim().to_owned()),
                _ => {}
            }
        }
    }
    assert_eq!(authorization, vec!["Bearer synthetic-lease"]);
    assert!(length <= LIMIT);
    while bytes.len() - header_end < length {
        let mut chunk = [0; 1024];
        let count = stream.read(&mut chunk).expect("read body");
        assert_ne!(count, 0, "incomplete body");
        bytes.extend_from_slice(&chunk[..count]);
        assert!(bytes.len() <= LIMIT + header_end);
    }
    let body = &bytes[header_end..header_end + length];
    (
        path,
        if body.is_empty() {
            serde_json::Value::Null
        } else {
            serde_json::from_slice(body).expect("JSON request body")
        },
    )
}

fn gateway_start(mut body: serde_json::Value) -> wire::StartRequest {
    let fields = body.as_object().expect("start object");
    assert_eq!(fields.len(), 6, "exact public start fields");
    for name in fields.keys() {
        assert!(
            [
                "request_id",
                "messages",
                "tools",
                "tool_call_policy",
                "tool_schema_mode",
                "max_output_tokens"
            ]
            .contains(&name.as_str())
        );
    }
    body["scope_id"] = serde_json::json!("mock-scope");
    body["expires_at"] = serde_json::json!(now() + 60);
    let request: wire::StartRequest =
        serde_json::from_value(body).expect("exact server deserialization");
    request
        .validate_public()
        .expect("server semantic validation");
    request
}

fn check_start(body: serde_json::Value) {
    let request = gateway_start(body);
    assert_eq!(request.request_id, "initial");
    assert_eq!(request.tools[0].name, "read");
    assert_eq!(
        request.tool_schema_mode,
        Some(model::ToolSchemaMode::Permissive)
    );
    assert_eq!(
        request.messages[0].content,
        vec![model::ContentBlock::Text {
            text: "read README".to_owned()
        }]
    );
}
fn check_continuation(body: serde_json::Value) {
    // The HTTP body must carry a string tool output, not an ad hoc JSON object.
    assert_eq!(
        body["messages"][2]["content"][0]["result"]["output"],
        "file text"
    );
    let request = gateway_start(body);
    assert_eq!(request.request_id, "continuation");
    let model::ContentBlock::ToolCall { call } = &request.messages[1].content[0] else {
        panic!("call")
    };
    assert_eq!(call.id, "call-1");
    let model::ContentBlock::ToolResult { result } = &request.messages[2].content[0] else {
        panic!("result")
    };
    assert_eq!(result.call_id, call.id);
    assert_eq!(result.output, "file text");
}
fn check_poll(body: serde_json::Value) {
    assert_eq!(body, serde_json::json!({"after_sequence":0}));
    drop(body);
}
fn check_empty(body: serde_json::Value) {
    assert!(body.is_null());
    drop(body);
}
fn request(operation: &str, id: &str) -> ServiceRequest {
    ServiceRequest {
        interface_id: model::MODEL_PROVIDER_INTERFACE_ID.to_owned(),
        operation: operation.to_owned(),
        payload: serde_json::to_vec(&model::FinishTurnRequest {
            provider_turn_id: id.to_owned(),
        })
        .expect("request"),
    }
}
fn initial() -> model::ModelTurnRequest {
    serde_json::from_value(serde_json::json!({
        "session_id":"00000000-0000-0000-0000-000000000001", "turn_id":"initial", "model_id":"broker-scoped",
        "messages":[{"role":"user","content":[{"type":"text","text":"read README"}]}],
        "tools":[{"name":"read","description":"Read text","input_schema":{"type":"object"}}],
        "tool_schema_mode":"permissive", "tool_call_policy":{"choice":{"mode":"auto"},"parallel":false}
    })).expect("initial model request")
}
fn insert(plugin: &AuthBrokerProviderPlugin, id: &str, broker: Broker) {
    let mut state = plugin.state.lock().expect("state");
    state.turns.insert(
        id.to_owned(),
        Turn {
            broker,
            cursor: 0,
            busy: false,
        },
    );
    drop(state);
}

fn http_tool_steps() -> Vec<Step> {
    let call = model::ToolCall {
        id: "call-1".to_owned(),
        name: "read".to_owned(),
        arguments: serde_json::json!({"path":"README.md"}),
    };
    let page = wire::PollResponse {
        events: vec![wire::SequencedEvent {
            sequence: 1,
            event: model::ProviderTurnEvent::ToolCallFinished { call },
        }],
        next_sequence: 1,
        terminal: true,
    };
    vec![
        Step {
            path: "/v1/turns",
            status: 200,
            response: serde_json::json!({"turn_id":"turn-1"}),
            check: check_start,
        },
        Step {
            path: "/v1/turns/turn-1/poll",
            status: 200,
            response: serde_json::to_value(page).expect("page"),
            check: check_poll,
        },
        Step {
            path: "/v1/turns",
            status: 200,
            response: serde_json::json!({"turn_id":"turn-2"}),
            check: check_continuation,
        },
        Step {
            path: "/v1/turns/turn-1/cancel",
            status: 200,
            response: serde_json::json!({}),
            check: check_empty,
        },
        Step {
            path: "/v1/turns/turn-1/finish",
            status: 503,
            response: serde_json::json!({"code":"cleanup_pending"}),
            check: check_empty,
        },
        Step {
            path: "/v1/turns/turn-1/finish",
            status: 200,
            response: serde_json::json!({}),
            check: check_empty,
        },
    ]
}

#[test]
fn http_tool_round_trip_validates_server_shape_and_cleanup_acknowledgement() {
    let call = model::ToolCall {
        id: "call-1".to_owned(),
        name: "read".to_owned(),
        arguments: serde_json::json!({"path":"README.md"}),
    };
    let mock = Mock::new(http_tool_steps());
    let plugin = AuthBrokerProviderPlugin::default();
    let mut model_request = initial();
    let started: wire::StartResponse = plugin
        .remote(
            mock.broker.clone(),
            "/v1/turns".to_owned(),
            Some(
                serde_json::to_vec(&start_body(&model_request).expect("start body"))
                    .expect("encode"),
            ),
        )
        .expect("start HTTP");
    insert(&plugin, &started.turn_id, mock.broker.clone());
    plugin
        .poll(&request(model::OP_POLL_TURN_EVENTS, &started.turn_id))
        .expect("poll HTTP");
    model_request.turn_id = "continuation".to_owned();
    model_request.messages.extend([
        model::ModelMessage {
            role: model::MessageRole::Assistant,
            content: vec![model::ContentBlock::ToolCall { call }],
        },
        model::ModelMessage {
            role: model::MessageRole::Tool,
            content: vec![model::ContentBlock::ToolResult {
                result: model::ToolResult {
                    call_id: "call-1".to_owned(),
                    output: "file text".to_owned(),
                    is_error: false,
                    content: vec![model::ToolResultContent::Text {
                        text: "file text".to_owned(),
                    }],
                },
            }],
        },
    ]);
    let _: wire::StartResponse = plugin
        .remote(
            mock.broker.clone(),
            "/v1/turns".to_owned(),
            Some(
                serde_json::to_vec(&start_body(&model_request).expect("continuation body"))
                    .expect("encode"),
            ),
        )
        .expect("continuation HTTP");
    plugin
        .lifecycle(&request(model::OP_CANCEL_TURN, &started.turn_id))
        .expect("cancel HTTP");
    let finish = request(model::OP_FINISH_TURN, &started.turn_id);
    assert!(matches!(
        plugin.lifecycle(&finish),
        Err("broker_cleanup_unverified")
    ));
    {
        let state = plugin.state.lock().expect("state");
        let turn = state
            .turns
            .get(&started.turn_id)
            .expect("uncertain cleanup retained");
        let cursor = turn.cursor;
        let busy = turn.busy;
        drop(state);
        assert_eq!(cursor, 1);
        assert!(!busy);
    }
    plugin
        .lifecycle(&finish)
        .expect("explicit verified finish retry");
    assert!(
        !plugin
            .state
            .lock()
            .expect("state")
            .turns
            .contains_key(&started.turn_id)
    );
    mock.finish();
}

#[test]
fn malformed_poll_surfaces_unverified_cleanup_and_preserves_retry_identity() {
    let mock = Mock::new(vec![
        Step {
            path: "/v1/turns/turn-1/poll",
            status: 200,
            response: serde_json::json!({"events":[],"next_sequence":3,"terminal":false}),
            check: check_poll,
        },
        Step {
            path: "/v1/turns/turn-1/finish",
            status: 503,
            response: serde_json::json!({"code":"cleanup_pending"}),
            check: check_empty,
        },
        Step {
            path: "/v1/turns/turn-1/finish",
            status: 200,
            response: serde_json::json!({}),
            check: check_empty,
        },
    ]);
    let plugin = AuthBrokerProviderPlugin::default();
    insert(&plugin, "turn-1", mock.broker.clone());
    assert!(matches!(
        plugin.poll(&request(model::OP_POLL_TURN_EVENTS, "turn-1")),
        Err("broker_cleanup_unverified")
    ));
    assert_eq!(plugin.claim("turn-1").expect("retry identity").1, 0);
    plugin
        .state
        .lock()
        .expect("state")
        .complete("turn-1", None, false)
        .expect("release claim");
    plugin
        .lifecycle(&request(model::OP_FINISH_TURN, "turn-1"))
        .expect("explicit cleanup retry");
    mock.finish();
}

#[test]
fn server_schema_rejects_map_tool_output_before_network() {
    let mut wire = serde_json::json!({
        "scope_id":"test", "expires_at":now()+60, "request_id":"continuation",
        "messages":[{"role":"tool","content":[{"type":"tool_result","result":{
            "call_id":"call-1","output":{"text":"file text"},"is_error":false,"content":[]}}]}],
        "tools":[], "max_output_tokens":null
    });
    assert!(serde_json::from_value::<wire::StartRequest>(wire.clone()).is_err());
    wire["messages"][0]["content"][0]["result"]["output"] = serde_json::json!("file text");
    let decoded = serde_json::from_value::<wire::StartRequest>(wire).expect("string output shape");
    // Correct wire shape alone is insufficient: a result without its matching call fails.
    assert!(decoded.validate_public().is_err());
}
