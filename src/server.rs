use axum::{
    Router,
    body::{Body, Bytes},
    extract::State,
    http::{Response, StatusCode, header},
    response::Json,
    routing::{delete, get, post},
};
use serde_json::{Value, json};
use std::sync::Arc;
use tokio::sync::{Mutex, mpsc::UnboundedSender};

use crate::command_jobs::CommandJobManager;
use crate::devtools::DevtoolsBridge;
use crate::mcp::{self, JsonRpcRequest};
use crate::state::{FlowDirection, ServerUiEvent, SharedState};

const STATELESS_FLOW_ID: &str = "stateless";
const STATELESS_FLOW_LABEL: &str = "stateless";

#[derive(Clone)]
struct ServerState {
    app: SharedState,
    devtools: Option<Arc<Mutex<DevtoolsBridge>>>,
    command_jobs: CommandJobManager,
    ui_events: UnboundedSender<ServerUiEvent>,
}

/// Build the axum router.
pub fn router(
    app_state: SharedState,
    devtools: Option<Arc<Mutex<DevtoolsBridge>>>,
    command_jobs: CommandJobManager,
    mcp_path: String,
    ui_events: UnboundedSender<ServerUiEvent>,
) -> Router {
    let state = ServerState {
        app: app_state,
        devtools,
        command_jobs,
        ui_events,
    };
    Router::new()
        .route("/", get(health))
        .route(&mcp_path, post(post_mcp))
        .route(&mcp_path, get(get_mcp))
        .route(&mcp_path, delete(delete_mcp))
        .with_state(state)
}

fn jsonrpc_error_response(status: StatusCode, code: i64, msg: &str) -> Response<Body> {
    let body = serde_json::to_string(&json!({
        "jsonrpc": "2.0",
        "error": {"code": code, "message": msg}
    }))
    .unwrap();
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body))
        .unwrap()
}

fn request_id(req: &Value) -> String {
    req.get("id").map_or("-".into(), |v| match v {
        Value::String(s) => s.clone(),
        _ => v.to_string(),
    })
}

fn request_tool_name(req: &Value) -> Option<String> {
    req.get("params")
        .and_then(|v| v.get("name"))
        .and_then(Value::as_str)
        .map(|s| s.to_string())
}

fn request_flow_label(req: &Value) -> String {
    let method = req
        .get("method")
        .and_then(Value::as_str)
        .unwrap_or("<invalid-method>");
    if method == "tools/call" {
        let tool = request_tool_name(req).unwrap_or_else(|| "?".into());
        return format!("tools/call:{tool}");
    }
    method.to_string()
}

fn summarize_request(req: &Value) -> String {
    let method = req
        .get("method")
        .and_then(Value::as_str)
        .unwrap_or("<invalid-method>");
    let id = request_id(req);
    if method == "tools/call" {
        let tool = request_tool_name(req).unwrap_or_else(|| "?".into());
        return format!("tools/call({tool},id={id})");
    }
    format!("{method}(id={id})")
}

fn summarize_response(resp: &Value) -> String {
    let id = resp.get("id").map_or("-".into(), |v| match v {
        Value::String(s) => s.clone(),
        _ => v.to_string(),
    });
    if let Some(err) = resp.get("error") {
        let code = err.get("code").and_then(Value::as_i64).unwrap_or(-32000);
        let msg = err
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("Unknown error");
        return format!("id={id}:error({code} {msg})");
    }
    if let Some(result) = resp.get("result") {
        if let Some(protocol_version) = result.get("protocolVersion").and_then(Value::as_str) {
            return format!("id={id}:result protocolVersion={protocol_version}");
        }
        return format!("id={id}:result");
    }
    format!("id={id}:unknown")
}

// ── GET / — health ──────────────────────────────────────────

async fn health(State(s): State<ServerState>) -> Json<Value> {
    let app = s.app.lock().await;
    Json(json!({
        "status": "ok",
        "name": "CatDesk",
        "description": "MCP Tools for ChatGPT to control your computer and browser",
        "mode": app.mode.label(),
        "tool_mode": app.tool_mode.label(),
        "workspace": app.workspace_root,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{AppState, Mode, ToolMode};
    use axum::body::to_bytes;
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::time::{SystemTime, UNIX_EPOCH};
    use tokio::sync::{Mutex, mpsc::unbounded_channel};

    #[test]
    fn summarize_initialize_response_includes_protocol_version() {
        let response = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": {
                "protocolVersion": "2025-11-25",
                "capabilities": {}
            }
        });

        assert_eq!(
            summarize_response(&response),
            "id=1:result protocolVersion=2025-11-25"
        );
    }

    fn unique_temp_path(prefix: &str) -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        std::env::temp_dir().join(format!("{prefix}-{unique}"))
    }

    fn tool_call_body(name: &str, arguments: Value) -> Bytes {
        Bytes::from(
            serde_json::to_vec(&json!({
                "jsonrpc": "2.0",
                "id": "req-tool",
                "method": "tools/call",
                "params": {
                    "name": name,
                    "arguments": arguments,
                }
            }))
            .expect("serialize tool call"),
        )
    }

    #[tokio::test]
    async fn background_command_survives_separate_stateless_http_requests() {
        let workspace_root = unique_temp_path("catdesk-post-mcp-command-job");
        let config_root = unique_temp_path("catdesk-post-mcp-command-job-config");
        let config_path = config_root.join("config.toml");
        std::fs::create_dir_all(&workspace_root).expect("create workspace");
        std::fs::create_dir_all(&config_root).expect("create config dir");

        let app = AppState::new_for_test(
            8787,
            workspace_root.to_string_lossy().into_owned(),
            config_path.clone(),
        )
        .expect("create app state");
        let app_state = Arc::new(Mutex::new(app));
        let (ui_tx, _ui_rx) = unbounded_channel();
        let command_jobs = CommandJobManager::new();
        let server_state = ServerState {
            app: app_state,
            devtools: None,
            command_jobs: command_jobs.clone(),
            ui_events: ui_tx,
        };
        let command = if cfg!(windows) {
            "Start-Sleep -Milliseconds 250; Write-Output http-job-done"
        } else {
            "sleep 0.25; printf 'http-job-done\\n'"
        };

        let start_response = post_mcp(
            State(server_state.clone()),
            tool_call_body(
                "start_command",
                json!({ "command": command, "timeout": 5_000 }),
            ),
        )
        .await;
        assert_eq!(start_response.status(), StatusCode::OK);
        let start_body = to_bytes(start_response.into_body(), usize::MAX)
            .await
            .expect("read start response");
        let start_payload: Value =
            serde_json::from_slice(&start_body).expect("parse start response");
        let job_id = start_payload
            .get("result")
            .and_then(|result| result.get("structuredContent"))
            .and_then(|structured| structured.get("jobId"))
            .and_then(Value::as_str)
            .expect("start response job id")
            .to_string();

        let mut cursor = 0u64;
        let mut seen = String::new();
        let mut terminal = None;
        for _ in 0..20 {
            let poll_response = post_mcp(
                State(server_state.clone()),
                tool_call_body(
                    "poll_command",
                    json!({ "job_id": job_id, "after": cursor, "wait_ms": 250 }),
                ),
            )
            .await;
            assert_eq!(poll_response.status(), StatusCode::OK);
            let poll_body = to_bytes(poll_response.into_body(), usize::MAX)
                .await
                .expect("read poll response");
            let poll_payload: Value =
                serde_json::from_slice(&poll_body).expect("parse poll response");
            let structured = poll_payload
                .get("result")
                .and_then(|result| result.get("structuredContent"))
                .expect("poll structured content");
            if let Some(output) = structured.get("output").and_then(Value::as_str) {
                seen.push_str(output);
            }
            cursor = structured
                .get("nextCursor")
                .and_then(Value::as_u64)
                .unwrap_or(cursor);
            let state = structured.get("state").and_then(Value::as_str);
            let has_more = structured
                .get("hasMoreOutput")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            if state == Some("succeeded") && !has_more {
                terminal = Some(poll_payload);
                break;
            }
        }

        let terminal = terminal.expect("background job did not finish across HTTP requests");
        let structured = terminal
            .get("result")
            .and_then(|result| result.get("structuredContent"))
            .expect("terminal structured content");
        assert_eq!(
            structured.get("state").and_then(Value::as_str),
            Some("succeeded")
        );
        assert!(structured.get("exitCode").is_none());
        assert!(structured.get("commandSuccess").is_none());
        assert_eq!(seen.matches("http-job-done").count(), 1);

        command_jobs.cancel_all().await;
        let _ = std::fs::remove_file(config_path);
        let _ = std::fs::remove_dir_all(workspace_root);
        let _ = std::fs::remove_dir_all(config_root);
    }

    #[tokio::test]
    async fn post_mcp_accumulates_usage_without_returning_token_metadata() {
        let workspace_root = unique_temp_path("catdesk-post-mcp-workspace");
        let config_root = unique_temp_path("catdesk-post-mcp-config");
        let config_path = config_root.join("config.toml");
        std::fs::create_dir_all(&workspace_root).expect("create workspace");
        std::fs::create_dir_all(&config_root).expect("create config dir");
        std::fs::write(workspace_root.join("hello.txt"), "hello world\n").expect("write file");

        let app = AppState::new_for_test(
            8787,
            workspace_root.to_string_lossy().into_owned(),
            config_path.clone(),
        )
        .expect("create app state");
        let app_state = Arc::new(Mutex::new(app));
        let (ui_tx, _ui_rx) = unbounded_channel();
        let server_state = ServerState {
            app: app_state.clone(),
            devtools: None,
            command_jobs: CommandJobManager::new(),
            ui_events: ui_tx,
        };

        let response = post_mcp(
            State(server_state),
            tool_call_body("run_command", json!({ "command": "find ." })),
        )
        .await;

        assert_eq!(response.status(), StatusCode::OK);

        let body = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("read response body");
        let payload: Value = serde_json::from_slice(&body).expect("parse json body");

        let result = payload.get("result").expect("missing tool result");
        assert!(
            result.get("_meta").is_none(),
            "tool result must not return CatDesk UI/token metadata"
        );

        let app = app_state.lock().await;
        let all_time_usage = app.all_time_usage_totals();
        assert!(all_time_usage.total_tokens > 0);
        assert_eq!(all_time_usage.tool_call_count, 1);
        assert_eq!(app.session_usage_totals, all_time_usage);
        assert!(matches!(app.mode, Mode::Both));
        assert!(matches!(app.tool_mode, ToolMode::MultiTools));
        drop(app);

        let _ = std::fs::remove_file(workspace_root.join("hello.txt"));
        let _ = std::fs::remove_file(config_path);
        let _ = std::fs::remove_dir_all(workspace_root);
        let _ = std::fs::remove_dir_all(config_root);
    }
}

// ── POST /<slug>/mcp ────────────────────────────────────────

async fn post_mcp(State(s): State<ServerState>, body_bytes: Bytes) -> Response<Body> {
    let body: Value = match serde_json::from_slice(&body_bytes) {
        Ok(v) => v,
        Err(e) => {
            return jsonrpc_error_response(
                StatusCode::BAD_REQUEST,
                -32700,
                &format!("Parse error: {e}"),
            );
        }
    };
    if !body.is_object() {
        return jsonrpc_error_response(
            StatusCode::BAD_REQUEST,
            -32600,
            "Invalid request: expected a single JSON-RPC message object",
        );
    }

    let _ = s.ui_events.send(ServerUiEvent::IncrementRequestCount);
    let _ = s.ui_events.send(ServerUiEvent::SetRemoteConnected(true));

    let has_method = body.get("method").and_then(Value::as_str).is_some();
    if !has_method {
        let mcp_path = {
            let app = s.app.lock().await;
            app.mcp_path()
        };
        let _ = s.ui_events.send(ServerUiEvent::Log {
            level: "INFO",
            message: format!(
                "POST {mcp_path} flow={STATELESS_FLOW_LABEL} accepted non-request JSON-RPC message"
            ),
        });
        return Response::builder()
            .status(StatusCode::ACCEPTED)
            .body(Body::empty())
            .unwrap();
    }

    let request_summary = summarize_request(&body);
    let request_flow_event = request_flow_label(&body);

    let _ = s.ui_events.send(ServerUiEvent::RecordFlow {
        flow_id: STATELESS_FLOW_ID.to_string(),
        events: vec![request_flow_event.clone()],
        direction: FlowDirection::Forward,
    });

    let req: JsonRpcRequest = match serde_json::from_value(body.clone()) {
        Ok(r) => r,
        Err(e) => {
            return jsonrpc_error_response(
                StatusCode::BAD_REQUEST,
                -32600,
                &format!("Invalid request: {e}"),
            );
        }
    };

    let (workspace_root, mode, tool_mode, set_catdesk_as_co_author) = {
        let app = s.app.lock().await;
        (
            app.workspace_root.clone(),
            app.mode,
            app.tool_mode,
            app.set_catdesk_as_co_author,
        )
    };

    let mut response_json: Option<Value> = None;
    if let Some(resp) = mcp::handle_request(
        &req,
        &workspace_root,
        mode,
        tool_mode,
        set_catdesk_as_co_author,
        &s.command_jobs,
        &s.devtools,
    )
    .await
    {
        if req.method == "tools/call" {
            if let Some(result) = resp.result.as_ref() {
                let (tool_input_tokens, tool_output_tokens) =
                    mcp::estimate_turn_token_usage(&req, result);
                let mut app = s.app.lock().await;
                app.record_turn_usage(tool_input_tokens, tool_output_tokens);
                app.persist_state_with_log();
            }
        }
        response_json = Some(serde_json::to_value(resp).unwrap());
    }

    {
        let app = s.app.lock().await;
        let mcp_path = app.mcp_path();
        drop(app);
        if req.id.is_some() {
            let _ = s.ui_events.send(ServerUiEvent::RecordFlow {
                flow_id: STATELESS_FLOW_ID.to_string(),
                events: vec![request_flow_event.clone()],
                direction: FlowDirection::Backward,
            });
        }
        let _ = s.ui_events.send(ServerUiEvent::Log {
            level: "INFO",
            message: format!(
                "POST {mcp_path} flow={STATELESS_FLOW_LABEL} [{}]",
                request_summary,
            ),
        });
        if let Some(ref resp_json) = response_json {
            let response_summary = summarize_response(resp_json);
            let _ = s.ui_events.send(ServerUiEvent::Log {
                level: "INFO",
                message: format!(
                    "POST {mcp_path} flow={STATELESS_FLOW_LABEL} response [{response_summary}]"
                ),
            });
        }
    }

    if req.id.is_none() {
        return Response::builder()
            .status(StatusCode::ACCEPTED)
            .body(Body::empty())
            .unwrap();
    }

    let Some(response_json) = response_json else {
        return jsonrpc_error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            -32603,
            "Internal error: request did not produce a JSON-RPC response",
        );
    };
    let response_body = serde_json::to_string(&response_json).unwrap();

    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(response_body))
        .unwrap()
}

// ── GET /<slug>/mcp — pure HTTP mode (no SSE) ───────────────

async fn get_mcp() -> Response<Body> {
    Response::builder()
        .status(StatusCode::METHOD_NOT_ALLOWED)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(
            r#"{"jsonrpc":"2.0","error":{"code":-32601,"message":"GET SSE stream is disabled in pure HTTP mode"}}"#,
        ))
        .unwrap()
}

// ── DELETE /<slug>/mcp ──────────────────────────────────────

async fn delete_mcp(State(s): State<ServerState>) -> Response<Body> {
    let _ = s.ui_events.send(ServerUiEvent::SetRemoteConnected(false));
    let _ = s.ui_events.send(ServerUiEvent::BeginFlowClose {
        flow_id: STATELESS_FLOW_ID.to_string(),
    });
    let _ = s.ui_events.send(ServerUiEvent::Log {
        level: "INFO",
        message: "DELETE mcp endpoint: stateless reset".to_string(),
    });
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(r#"{"status":"ok"}"#))
        .unwrap()
}
