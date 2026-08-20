use axum::{
    Router,
    body::{Body, Bytes},
    extract::{Path as AxumPath, State},
    http::{HeaderValue, Response, StatusCode, header},
    response::Json,
    routing::{delete, get, post},
};
use serde_json::{Value, json};
use std::sync::Arc;
use tokio::sync::Mutex;

use crate::command_jobs::CommandJobManager;
use crate::devtools::DevtoolsBridge;
use crate::mcp::{self, JsonRpcRequest};
use crate::state::{
    CommandActivityState, FlowDirection, ServerUiEvent, SharedState, UiEventSender,
};
use crate::workspaces::{self, WorkspaceId, WorkspaceRequestContext, WorkspaceRequestLease};
use uuid::Uuid;

const STATELESS_FLOW_ID: &str = "stateless";
const STATELESS_FLOW_LABEL: &str = "stateless";

#[derive(Clone)]
struct ServerState {
    app: SharedState,
    devtools: Option<Arc<Mutex<DevtoolsBridge>>>,
    command_jobs: CommandJobManager,
    ui_events: UiEventSender,
}

/// Build the axum router.
pub fn router(
    app_state: SharedState,
    devtools: Option<Arc<Mutex<DevtoolsBridge>>>,
    command_jobs: CommandJobManager,
    ui_events: UiEventSender,
) -> Router {
    let state = ServerState {
        app: app_state,
        devtools,
        command_jobs,
        ui_events,
    };
    Router::new()
        .route("/", get(health))
        .route("/{slug}/mcp", post(post_mcp))
        .route("/{slug}/mcp", get(get_mcp))
        .route("/{slug}/mcp", delete(delete_mcp))
        .with_state(state)
}

fn response_with_body(
    status: StatusCode,
    content_type: &'static str,
    body: Body,
) -> Response<Body> {
    let mut response = Response::new(body);
    *response.status_mut() = status;
    response
        .headers_mut()
        .insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
    response
}

fn jsonrpc_error_response(status: StatusCode, code: i64, msg: &str) -> Response<Body> {
    let body = json!({
        "jsonrpc": "2.0",
        "error": {"code": code, "message": msg}
    })
    .to_string();
    response_with_body(status, "application/json", Body::from(body))
}

async fn resolve_workspace(
    state: &ServerState,
    slug: &str,
) -> Option<(WorkspaceRequestContext, WorkspaceRequestLease)> {
    let app = state.app.lock().await;
    let workspace = workspaces::resolve_workspace_by_slug(&app.workspaces, slug)?;
    let runtime = app.workspace_runtimes.get(&workspace.workspace_id)?.clone();
    let lease = runtime.try_acquire()?;
    Some((workspace, lease))
}

fn not_found_response() -> Response<Body> {
    response_with_body(
        StatusCode::NOT_FOUND,
        "application/json",
        Body::from(r#"{"error":"not found"}"#),
    )
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

#[derive(Clone, Debug)]
enum CommandUiRequest {
    Run { activity_id: String },
    Start { activity_id: String },
    Poll { job_id: String },
    Cancel { job_id: String },
}

fn tool_arguments(req: &Value) -> Option<&Value> {
    req.get("params")?.get("arguments")
}

fn begin_command_ui_request(
    req: &Value,
    workspace_id: &WorkspaceId,
    ui_events: &UiEventSender,
) -> Option<CommandUiRequest> {
    if req.get("method").and_then(Value::as_str) != Some("tools/call") {
        return None;
    }
    let tool = request_tool_name(req)?;
    let arguments = tool_arguments(req)?;
    match tool.as_str() {
        "run_command" | "start_command" => {
            let command = arguments.get("command")?.as_str()?.to_string();
            let activity_id = Uuid::new_v4().to_string();
            let background = tool == "start_command";
            let _ = ui_events.send(ServerUiEvent::CommandStarted {
                workspace_id: workspace_id.clone(),
                activity_id: activity_id.clone(),
                command,
                background,
            });
            if background {
                Some(CommandUiRequest::Start { activity_id })
            } else {
                Some(CommandUiRequest::Run { activity_id })
            }
        }
        "poll_command" => arguments
            .get("job_id")
            .and_then(Value::as_str)
            .map(|job_id| CommandUiRequest::Poll {
                job_id: job_id.to_string(),
            }),
        "cancel_command" => arguments
            .get("job_id")
            .and_then(Value::as_str)
            .map(|job_id| CommandUiRequest::Cancel {
                job_id: job_id.to_string(),
            }),
        _ => None,
    }
}

fn tool_structured_content(response: &Value) -> Option<&Value> {
    response.get("result")?.get("structuredContent")
}

fn compact_command_preview(text: &str) -> Option<String> {
    const MAX_PREVIEW_CHARS: usize = 240;
    let mut tail = text
        .chars()
        .rev()
        .take(MAX_PREVIEW_CHARS + 1)
        .collect::<Vec<_>>();
    let was_trimmed = tail.len() > MAX_PREVIEW_CHARS;
    if was_trimmed {
        tail.truncate(MAX_PREVIEW_CHARS);
    }
    tail.reverse();
    let compact = tail
        .into_iter()
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if compact.is_empty() {
        None
    } else if was_trimmed {
        Some(format!("…{compact}"))
    } else {
        Some(compact)
    }
}

fn command_response_preview(response: &Value) -> Option<String> {
    if let Some(structured) = tool_structured_content(response) {
        for field in ["stderr", "output", "stdout", "message"] {
            if let Some(text) = structured.get(field).and_then(Value::as_str)
                && let Some(preview) = compact_command_preview(text)
            {
                return Some(preview);
            }
        }
    }
    response
        .get("error")
        .and_then(|error| error.get("message"))
        .and_then(Value::as_str)
        .and_then(compact_command_preview)
}

fn command_response_state(response: &Value) -> CommandActivityState {
    let structured = tool_structured_content(response);
    if structured
        .and_then(|value| value.get("timedOut"))
        .and_then(Value::as_bool)
        == Some(true)
    {
        return CommandActivityState::TimedOut;
    }
    if let Some(state) = structured
        .and_then(|value| value.get("state"))
        .and_then(Value::as_str)
    {
        return match state {
            "succeeded" => CommandActivityState::Succeeded,
            "failed" => CommandActivityState::Failed,
            "cancelled" => CommandActivityState::Cancelled,
            "timed_out" => CommandActivityState::TimedOut,
            _ => CommandActivityState::Running,
        };
    }
    let is_error = response.get("error").is_some()
        || response
            .get("result")
            .and_then(|result| result.get("isError"))
            .and_then(Value::as_bool)
            == Some(true);
    if is_error {
        CommandActivityState::Failed
    } else {
        CommandActivityState::Succeeded
    }
}

fn command_response_exit_code(response: &Value) -> Option<i32> {
    tool_structured_content(response)
        .and_then(|value| value.get("exitCode"))
        .and_then(Value::as_i64)
        .and_then(|code| i32::try_from(code).ok())
}

fn finish_command_ui_request(
    request: &CommandUiRequest,
    response: &Value,
    workspace_id: &WorkspaceId,
    ui_events: &UiEventSender,
) {
    let state = command_response_state(response);
    let exit_code = command_response_exit_code(response);
    let preview = command_response_preview(response);
    match request {
        CommandUiRequest::Run { activity_id } => {
            let _ = ui_events.send(ServerUiEvent::CommandUpdated {
                workspace_id: workspace_id.clone(),
                activity_id: Some(activity_id.clone()),
                job_id: None,
                state,
                exit_code,
                preview,
            });
        }
        CommandUiRequest::Start { activity_id } => {
            if let Some(job_id) = tool_structured_content(response)
                .and_then(|value| value.get("jobId"))
                .and_then(Value::as_str)
            {
                let _ = ui_events.send(ServerUiEvent::CommandBoundToJob {
                    workspace_id: workspace_id.clone(),
                    activity_id: activity_id.clone(),
                    job_id: job_id.to_string(),
                });
            }
            let _ = ui_events.send(ServerUiEvent::CommandUpdated {
                workspace_id: workspace_id.clone(),
                activity_id: Some(activity_id.clone()),
                job_id: None,
                state,
                exit_code,
                preview,
            });
        }
        CommandUiRequest::Poll { job_id } | CommandUiRequest::Cancel { job_id } => {
            let _ = ui_events.send(ServerUiEvent::CommandUpdated {
                workspace_id: workspace_id.clone(),
                activity_id: None,
                job_id: Some(job_id.clone()),
                state,
                exit_code,
                preview,
            });
        }
    }
}

// ── GET / — health ──────────────────────────────────────────

async fn health() -> Json<Value> {
    Json(json!({
        "status": "ok",
        "name": "MoonDesk"
    }))
}

// ── POST /<slug>/mcp ────────────────────────────────────────

async fn post_mcp(
    AxumPath(slug): AxumPath<String>,
    State(s): State<ServerState>,
    body_bytes: Bytes,
) -> Response<Body> {
    let Some((workspace, _request_lease)) = resolve_workspace(&s, &slug).await else {
        return not_found_response();
    };

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

    let _ = s.ui_events.send(ServerUiEvent::IncrementRequestCount {
        workspace_id: workspace.workspace_id.clone(),
    });
    let _ = s.ui_events.send(ServerUiEvent::SetRemoteConnected {
        workspace_id: workspace.workspace_id.clone(),
        connected: true,
    });

    let has_method = body.get("method").and_then(Value::as_str).is_some();
    if !has_method {
        let _ = s.ui_events.send(ServerUiEvent::Log {
            workspace_id: Some(workspace.workspace_id.clone()),
            level: "INFO",
            message: format!(
                "POST workspace={} flow={STATELESS_FLOW_LABEL} accepted non-request JSON-RPC message",
                workspace.name
            ),
        });
        return response_with_body(StatusCode::ACCEPTED, "application/json", Body::empty());
    }

    let request_summary = summarize_request(&body);
    let request_flow_event = request_flow_label(&body);

    let _ = s.ui_events.send(ServerUiEvent::RecordFlow {
        workspace_id: workspace.workspace_id.clone(),
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
    // TUI-only observability: publish the shell command as soon as the MCP call
    // has been parsed. This channel is independent of the MCP response and does
    // not add command text or result data to ChatGPT's conversation state.
    let command_ui_request = begin_command_ui_request(&body, &workspace.workspace_id, &s.ui_events);

    let workspace_root = workspace.root.to_string_lossy().into_owned();
    let (mode, tool_mode, set_moondesk_as_co_author) = {
        let app = s.app.lock().await;
        (app.mode, app.tool_mode, app.set_moondesk_as_co_author)
    };

    let mut response_json: Option<Value> = None;
    if let Some(resp) = mcp::handle_request(
        &req,
        mcp::McpRequestContext {
            workspace_id: &workspace.workspace_id,
            workspace_root: &workspace_root,
            mode,
            tool_mode,
            set_moondesk_as_co_author,
            command_jobs: &s.command_jobs,
            devtools: &s.devtools,
        },
    )
    .await
    {
        if req.method == "tools/call"
            && let Some(result) = resp.result.as_ref()
        {
            let (tool_input_tokens, tool_output_tokens) =
                mcp::estimate_turn_token_usage(&req, result);
            let mut app = s.app.lock().await;
            app.record_turn_usage(tool_input_tokens, tool_output_tokens);
        }
        let response_value = match serde_json::to_value(resp) {
            Ok(value) => value,
            Err(error) => {
                return jsonrpc_error_response(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    -32603,
                    &format!("Internal error: failed to serialize JSON-RPC response: {error}"),
                );
            }
        };
        if let Some(command_ui_request) = command_ui_request.as_ref() {
            finish_command_ui_request(
                command_ui_request,
                &response_value,
                &workspace.workspace_id,
                &s.ui_events,
            );
        }
        response_json = Some(response_value);
    }

    {
        if req.id.is_some() {
            let _ = s.ui_events.send(ServerUiEvent::RecordFlow {
                workspace_id: workspace.workspace_id.clone(),
                flow_id: STATELESS_FLOW_ID.to_string(),
                events: vec![request_flow_event.clone()],
                direction: FlowDirection::Backward,
            });
        }
        let _ = s.ui_events.send(ServerUiEvent::Log {
            workspace_id: Some(workspace.workspace_id.clone()),
            level: "INFO",
            message: format!(
                "POST workspace={} flow={STATELESS_FLOW_LABEL} [{}]",
                workspace.name, request_summary,
            ),
        });
        if let Some(ref resp_json) = response_json {
            let response_summary = summarize_response(resp_json);
            let _ = s.ui_events.send(ServerUiEvent::Log {
                workspace_id: Some(workspace.workspace_id.clone()),
                level: "INFO",
                message: format!(
                    "POST workspace={} flow={STATELESS_FLOW_LABEL} response [{response_summary}]",
                    workspace.name
                ),
            });
        }
    }

    if req.id.is_none() {
        return response_with_body(StatusCode::ACCEPTED, "application/json", Body::empty());
    }

    let Some(response_json) = response_json else {
        return jsonrpc_error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            -32603,
            "Internal error: request did not produce a JSON-RPC response",
        );
    };
    let response_body = response_json.to_string();
    response_with_body(
        StatusCode::OK,
        "application/json",
        Body::from(response_body),
    )
}

// ── GET /<slug>/mcp — pure HTTP mode (no SSE) ───────────────

async fn get_mcp(AxumPath(slug): AxumPath<String>, State(s): State<ServerState>) -> Response<Body> {
    let Some((_workspace, _request_lease)) = resolve_workspace(&s, &slug).await else {
        return not_found_response();
    };
    response_with_body(
        StatusCode::METHOD_NOT_ALLOWED,
        "application/json",
        Body::from(
            r#"{"jsonrpc":"2.0","error":{"code":-32601,"message":"GET SSE stream is disabled in pure HTTP mode"}}"#,
        ),
    )
}

// ── DELETE /<slug>/mcp ──────────────────────────────────────

async fn delete_mcp(
    AxumPath(slug): AxumPath<String>,
    State(s): State<ServerState>,
) -> Response<Body> {
    let Some((workspace, _request_lease)) = resolve_workspace(&s, &slug).await else {
        return not_found_response();
    };
    let _ = s.ui_events.send(ServerUiEvent::SetRemoteConnected {
        workspace_id: workspace.workspace_id.clone(),
        connected: false,
    });
    let _ = s.ui_events.send(ServerUiEvent::BeginFlowClose {
        workspace_id: workspace.workspace_id.clone(),
        flow_id: STATELESS_FLOW_ID.to_string(),
    });
    let _ = s.ui_events.send(ServerUiEvent::Log {
        workspace_id: Some(workspace.workspace_id.clone()),
        level: "INFO",
        message: "DELETE mcp endpoint: stateless reset".to_string(),
    });
    response_with_body(
        StatusCode::OK,
        "application/json",
        Body::from(r#"{"status":"ok"}"#),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{AppState, Mode, ToolMode, rotate_workspace_secret, ui_event_channel};
    use crate::workspaces::WorkspaceConfig;
    use axum::body::to_bytes;
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::time::{SystemTime, UNIX_EPOCH};
    use tokio::sync::Mutex;

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

    #[test]
    fn command_ui_events_track_background_lifecycle_without_mutating_mcp_response() {
        let (ui_tx, mut ui_rx) = ui_event_channel();
        let workspace_id = WorkspaceId::test_default();
        let start_request = json!({
            "jsonrpc": "2.0",
            "id": "req-start",
            "method": "tools/call",
            "params": {
                "name": "start_command",
                "arguments": { "command": "cargo test" }
            }
        });
        let observation = begin_command_ui_request(&start_request, &workspace_id, &ui_tx)
            .expect("observe start command");
        let activity_id = match ui_rx.try_recv().expect("immediate command-start event") {
            ServerUiEvent::CommandStarted {
                activity_id,
                command,
                background,
                ..
            } => {
                assert_eq!(command, "cargo test");
                assert!(background);
                activity_id
            }
            _ => panic!("expected command-start event"),
        };

        let response = json!({
            "jsonrpc": "2.0",
            "id": "req-start",
            "result": {
                "content": [],
                "structuredContent": { "jobId": "job-1", "state": "running" },
                "isError": false
            }
        });
        let response_before = response.clone();
        finish_command_ui_request(&observation, &response, &workspace_id, &ui_tx);
        assert_eq!(response, response_before);

        match ui_rx.try_recv().expect("job binding event") {
            ServerUiEvent::CommandBoundToJob {
                activity_id: bound_activity_id,
                job_id,
                ..
            } => {
                assert_eq!(bound_activity_id, activity_id);
                assert_eq!(job_id, "job-1");
            }
            _ => panic!("expected command-job binding"),
        }
        match ui_rx.try_recv().expect("running update") {
            ServerUiEvent::CommandUpdated { state, .. } => {
                assert_eq!(state, CommandActivityState::Running);
            }
            _ => panic!("expected command update"),
        }

        let poll_request = json!({
            "jsonrpc": "2.0",
            "id": "req-poll",
            "method": "tools/call",
            "params": {
                "name": "poll_command",
                "arguments": { "job_id": "job-1", "after": 0 }
            }
        });
        let poll_observation = begin_command_ui_request(&poll_request, &workspace_id, &ui_tx)
            .expect("observe poll command");
        assert!(ui_rx.try_recv().is_err(), "poll must not add a command row");
        let poll_response = json!({
            "jsonrpc": "2.0",
            "id": "req-poll",
            "result": {
                "content": [],
                "structuredContent": {
                    "state": "succeeded",
                    "output": "test result: ok. 109 passed; 0 failed",
                    "nextCursor": 7
                },
                "isError": false
            }
        });
        finish_command_ui_request(&poll_observation, &poll_response, &workspace_id, &ui_tx);
        match ui_rx.try_recv().expect("terminal update") {
            ServerUiEvent::CommandUpdated {
                job_id,
                state,
                preview,
                ..
            } => {
                assert_eq!(job_id.as_deref(), Some("job-1"));
                assert_eq!(state, CommandActivityState::Succeeded);
                assert_eq!(
                    preview.as_deref(),
                    Some("test result: ok. 109 passed; 0 failed")
                );
            }
            _ => panic!("expected terminal command update"),
        }
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
        let workspace_root = unique_temp_path("moondesk-post-mcp-command-job");
        let config_root = unique_temp_path("moondesk-post-mcp-command-job-config");
        let config_path = config_root.join("config.toml");
        std::fs::create_dir_all(&workspace_root).expect("create workspace");
        std::fs::create_dir_all(&config_root).expect("create config dir");

        let app = AppState::new_for_test(
            8787,
            workspace_root.to_string_lossy().into_owned(),
            config_path.clone(),
        )
        .expect("create app state");
        let mcp_slug = app.mcp_slug.clone();
        let app_state = Arc::new(Mutex::new(app));
        let (ui_tx, _ui_rx) = ui_event_channel();
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
            AxumPath(mcp_slug.clone()),
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
                AxumPath(mcp_slug.clone()),
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
        let workspace_root = unique_temp_path("moondesk-post-mcp-workspace");
        let config_root = unique_temp_path("moondesk-post-mcp-config");
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
        let mcp_slug = app.mcp_slug.clone();
        let app_state = Arc::new(Mutex::new(app));
        let (ui_tx, _ui_rx) = ui_event_channel();
        let server_state = ServerState {
            app: app_state.clone(),
            devtools: None,
            command_jobs: CommandJobManager::new(),
            ui_events: ui_tx,
        };

        let response = post_mcp(
            AxumPath(mcp_slug),
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
            "tool result must not return MoonDesk UI/token metadata"
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

    #[tokio::test]
    async fn different_slugs_route_to_different_workspace_roots() {
        let workspace_a = unique_temp_path("moondesk-routing-a");
        let workspace_b = unique_temp_path("moondesk-routing-b");
        let config_root = unique_temp_path("moondesk-routing-config");
        let config_path = config_root.join("config.toml");
        std::fs::create_dir_all(&workspace_a).expect("create workspace A");
        std::fs::create_dir_all(&workspace_b).expect("create workspace B");
        std::fs::create_dir_all(&config_root).expect("create config dir");
        std::fs::write(workspace_a.join("marker.txt"), "workspace-a\n").expect("write marker A");
        std::fs::write(workspace_b.join("marker.txt"), "workspace-b\n").expect("write marker B");

        let mut app = AppState::new_for_test(
            8787,
            workspace_a.to_string_lossy().into_owned(),
            config_path.clone(),
        )
        .expect("create app state");
        let slug_a = app.workspaces[0].mcp_slug.clone();
        let second = WorkspaceConfig::new(
            "Workspace B",
            &workspace_b,
            crate::workspaces::generate_mcp_slug(),
        )
        .expect("create second workspace");
        let slug_b = second.mcp_slug.clone();
        app.workspace_runtimes.insert(
            second.id.clone(),
            Arc::new(crate::workspaces::WorkspaceRuntime::default()),
        );
        app.workspaces.push(second);

        let app_state = Arc::new(Mutex::new(app));
        let (ui_tx, _ui_rx) = ui_event_channel();
        let server_state = ServerState {
            app: app_state,
            devtools: None,
            command_jobs: CommandJobManager::new(),
            ui_events: ui_tx,
        };

        for (slug, expected) in [
            (slug_a.clone(), "workspace-a\n"),
            (slug_b.clone(), "workspace-b\n"),
        ] {
            let response = post_mcp(
                AxumPath(slug),
                State(server_state.clone()),
                tool_call_body("read", json!({ "path": "marker.txt" })),
            )
            .await;
            assert_eq!(response.status(), StatusCode::OK);
            let body = to_bytes(response.into_body(), usize::MAX)
                .await
                .expect("read response");
            let payload: Value = serde_json::from_slice(&body).expect("parse response");
            assert_eq!(
                payload
                    .pointer("/result/structuredContent/text")
                    .and_then(Value::as_str),
                Some(expected)
            );
        }

        let cross_root = post_mcp(
            AxumPath(slug_a.clone()),
            State(server_state.clone()),
            tool_call_body(
                "read",
                json!({ "path": workspace_b.join("marker.txt").to_string_lossy() }),
            ),
        )
        .await;
        assert_eq!(cross_root.status(), StatusCode::OK);
        let cross_body = to_bytes(cross_root.into_body(), usize::MAX)
            .await
            .expect("read cross-root response");
        let cross_payload: Value =
            serde_json::from_slice(&cross_body).expect("parse cross-root response");
        assert_eq!(
            cross_payload
                .pointer("/result/isError")
                .and_then(Value::as_bool),
            Some(true)
        );

        // ngrok restart clears host connection state but must not mutate the
        // workspace registry or endpoint routing table.
        {
            let mut app = server_state.app.lock().await;
            app.ngrok_url = Some("https://old.example".into());
            app.clear_remote_connection_state();
            app.ngrok_url = Some("https://new.example".into());
        }
        for slug in [slug_a, slug_b] {
            let response = post_mcp(
                AxumPath(slug),
                State(server_state.clone()),
                Bytes::from_static(
                    br#"{"jsonrpc":"2.0","id":"after-tunnel-reset","method":"ping"}"#,
                ),
            )
            .await;
            assert_eq!(response.status(), StatusCode::OK);
        }

        let unknown = post_mcp(
            AxumPath("definitely-not-a-workspace".to_string()),
            State(server_state),
            Bytes::from_static(br#"{"jsonrpc":"2.0","id":"unknown","method":"ping"}"#),
        )
        .await;
        assert_eq!(unknown.status(), StatusCode::NOT_FOUND);

        let _ = std::fs::remove_file(config_path);
        let _ = std::fs::remove_dir_all(config_root);
        let _ = std::fs::remove_dir_all(workspace_b);
        let _ = std::fs::remove_dir_all(workspace_a);
    }

    #[tokio::test]
    async fn workspace_routes_support_concurrent_reads_and_writes() {
        let workspace_a = unique_temp_path("moondesk-concurrent-a");
        let workspace_b = unique_temp_path("moondesk-concurrent-b");
        let config_root = unique_temp_path("moondesk-concurrent-config");
        let config_path = config_root.join("config.toml");
        std::fs::create_dir_all(&workspace_a).expect("create workspace A");
        std::fs::create_dir_all(&workspace_b).expect("create workspace B");
        std::fs::create_dir_all(&config_root).expect("create config dir");

        let mut app = AppState::new_for_test(
            8787,
            workspace_a.to_string_lossy().into_owned(),
            config_path.clone(),
        )
        .expect("create app state");
        let slug_a = app.workspaces[0].mcp_slug.clone();
        let second = WorkspaceConfig::new(
            "Workspace B",
            &workspace_b,
            crate::workspaces::generate_mcp_slug(),
        )
        .expect("create second workspace");
        let slug_b = second.mcp_slug.clone();
        app.workspace_runtimes.insert(
            second.id.clone(),
            Arc::new(crate::workspaces::WorkspaceRuntime::default()),
        );
        app.workspaces.push(second);

        let app_state = Arc::new(Mutex::new(app));
        let (ui_tx, _ui_rx) = ui_event_channel();
        let server_state = ServerState {
            app: app_state,
            devtools: None,
            command_jobs: CommandJobManager::new(),
            ui_events: ui_tx,
        };

        // tool_call_body deliberately uses the same JSON-RPC id for both calls.
        let (write_a, write_b) = tokio::join!(
            post_mcp(
                AxumPath(slug_a.clone()),
                State(server_state.clone()),
                tool_call_body("write", json!({ "path": "concurrent.txt", "content": "A" })),
            ),
            post_mcp(
                AxumPath(slug_b.clone()),
                State(server_state.clone()),
                tool_call_body("write", json!({ "path": "concurrent.txt", "content": "B" })),
            )
        );
        assert_eq!(write_a.status(), StatusCode::OK);
        assert_eq!(write_b.status(), StatusCode::OK);
        assert_eq!(
            std::fs::read_to_string(workspace_a.join("concurrent.txt")).expect("read A file"),
            "A"
        );
        assert_eq!(
            std::fs::read_to_string(workspace_b.join("concurrent.txt")).expect("read B file"),
            "B"
        );

        let (read_a, read_b) = tokio::join!(
            post_mcp(
                AxumPath(slug_a),
                State(server_state.clone()),
                tool_call_body("read", json!({ "path": "concurrent.txt" })),
            ),
            post_mcp(
                AxumPath(slug_b),
                State(server_state),
                tool_call_body("read", json!({ "path": "concurrent.txt" })),
            )
        );
        let read_a_body = to_bytes(read_a.into_body(), usize::MAX)
            .await
            .expect("read A response");
        let read_b_body = to_bytes(read_b.into_body(), usize::MAX)
            .await
            .expect("read B response");
        let read_a_payload: Value = serde_json::from_slice(&read_a_body).expect("parse A response");
        let read_b_payload: Value = serde_json::from_slice(&read_b_body).expect("parse B response");
        assert_eq!(
            read_a_payload
                .pointer("/result/structuredContent/text")
                .and_then(Value::as_str),
            Some("A")
        );
        assert_eq!(
            read_b_payload
                .pointer("/result/structuredContent/text")
                .and_then(Value::as_str),
            Some("B")
        );

        let _ = std::fs::remove_file(config_path);
        let _ = std::fs::remove_dir_all(config_root);
        let _ = std::fs::remove_dir_all(workspace_b);
        let _ = std::fs::remove_dir_all(workspace_a);
    }

    #[tokio::test]
    async fn deleting_one_workspace_endpoint_does_not_disconnect_another() {
        let workspace_a_root = unique_temp_path("moondesk-delete-isolation-a");
        let workspace_b_root = unique_temp_path("moondesk-delete-isolation-b");
        let config_root = unique_temp_path("moondesk-delete-isolation-config");
        let config_path = config_root.join("config.toml");
        std::fs::create_dir_all(&workspace_a_root).expect("create workspace A");
        std::fs::create_dir_all(&workspace_b_root).expect("create workspace B");
        std::fs::create_dir_all(&config_root).expect("create config dir");

        let mut app = AppState::new_for_test(
            8787,
            workspace_a_root.to_string_lossy().into_owned(),
            config_path.clone(),
        )
        .expect("create app state");
        let workspace_a_id = app.workspaces[0].id.clone();
        let slug_a = app.workspaces[0].mcp_slug.clone();
        let workspace_b = WorkspaceConfig::new(
            "Workspace B",
            &workspace_b_root,
            crate::workspaces::generate_mcp_slug(),
        )
        .expect("create workspace B");
        let workspace_b_id = workspace_b.id.clone();
        app.workspace_runtimes.insert(
            workspace_b_id.clone(),
            Arc::new(crate::workspaces::WorkspaceRuntime::default()),
        );
        app.workspaces.push(workspace_b);
        app.apply_server_ui_event(ServerUiEvent::SetRemoteConnected {
            workspace_id: workspace_a_id.clone(),
            connected: true,
        });
        app.apply_server_ui_event(ServerUiEvent::SetRemoteConnected {
            workspace_id: workspace_b_id.clone(),
            connected: true,
        });

        let app_state = Arc::new(Mutex::new(app));
        let (ui_tx, mut ui_rx) = ui_event_channel();
        let server_state = ServerState {
            app: app_state.clone(),
            devtools: None,
            command_jobs: CommandJobManager::new(),
            ui_events: ui_tx,
        };

        let response = delete_mcp(AxumPath(slug_a), State(server_state)).await;
        assert_eq!(response.status(), StatusCode::OK);
        while let Ok(event) = ui_rx.try_recv() {
            app_state.lock().await.apply_server_ui_event(event);
        }

        let app = app_state.lock().await;
        assert!(app.remote_connected);
        assert!(!app.workspace_runtimes[&workspace_a_id].remote_connected());
        assert!(app.workspace_runtimes[&workspace_b_id].remote_connected());
        drop(app);

        let _ = std::fs::remove_file(config_path);
        let _ = std::fs::remove_dir_all(config_root);
        let _ = std::fs::remove_dir_all(workspace_b_root);
        let _ = std::fs::remove_dir_all(workspace_a_root);
    }

    #[tokio::test]
    async fn health_response_does_not_expose_workspace_or_runtime_configuration() {
        let Json(payload) = health().await;
        assert_eq!(payload.get("status").and_then(Value::as_str), Some("ok"));
        assert_eq!(
            payload.get("name").and_then(Value::as_str),
            Some("MoonDesk")
        );
        let mut keys = payload
            .as_object()
            .expect("health payload must be an object")
            .keys()
            .map(String::as_str)
            .collect::<Vec<_>>();
        keys.sort_unstable();
        assert_eq!(keys, vec!["name", "status"]);
    }

    #[tokio::test]
    async fn rotating_mcp_slug_immediately_rejects_old_slug_and_accepts_new_slug() {
        let workspace_root = unique_temp_path("moondesk-slug-rotation-workspace");
        let workspace_b_root = unique_temp_path("moondesk-slug-rotation-workspace-b");
        let config_root = unique_temp_path("moondesk-slug-rotation-config");
        let config_path = config_root.join("config.toml");
        std::fs::create_dir_all(&workspace_root).expect("create workspace");
        std::fs::create_dir_all(&workspace_b_root).expect("create workspace B");
        std::fs::create_dir_all(&config_root).expect("create config dir");

        let mut app = AppState::new_for_test(
            8787,
            workspace_root.to_string_lossy().into_owned(),
            config_path.clone(),
        )
        .expect("create app state");
        let old_slug = app.mcp_slug.clone();
        let workspace_id = app.workspaces[0].id.clone();
        let workspace_b = WorkspaceConfig::new(
            "Workspace B",
            &workspace_b_root,
            crate::workspaces::generate_mcp_slug(),
        )
        .expect("create workspace B");
        let workspace_b_slug = workspace_b.mcp_slug.clone();
        app.workspace_runtimes.insert(
            workspace_b.id.clone(),
            Arc::new(crate::workspaces::WorkspaceRuntime::default()),
        );
        app.workspaces.push(workspace_b);
        let app_state = Arc::new(Mutex::new(app));
        let (ui_tx, _ui_rx) = ui_event_channel();
        let server_state = ServerState {
            app: app_state.clone(),
            devtools: None,
            command_jobs: CommandJobManager::new(),
            ui_events: ui_tx,
        };
        let ping = Bytes::from_static(
            br#"{"jsonrpc":"2.0","id":"slug-check","method":"ping","params":{}}"#,
        );

        let before_rotation = post_mcp(
            AxumPath(old_slug.clone()),
            State(server_state.clone()),
            ping.clone(),
        )
        .await;
        assert_eq!(before_rotation.status(), StatusCode::OK);

        let new_slug = rotate_workspace_secret(&app_state, &workspace_id)
            .await
            .expect("rotate workspace secret");
        assert_ne!(old_slug, new_slug);

        let old_response = post_mcp(
            AxumPath(old_slug),
            State(server_state.clone()),
            ping.clone(),
        )
        .await;
        assert_eq!(old_response.status(), StatusCode::NOT_FOUND);

        {
            let app = app_state.lock().await;
            assert!(
                app.workspaces
                    .iter()
                    .any(|workspace| workspace.mcp_slug == workspace_b_slug),
                "rotating workspace A must not change workspace B's secret"
            );
        }
        let workspace_b_response = post_mcp(
            AxumPath(workspace_b_slug),
            State(server_state.clone()),
            ping.clone(),
        )
        .await;
        assert_eq!(workspace_b_response.status(), StatusCode::OK);

        let new_response = post_mcp(AxumPath(new_slug), State(server_state), ping).await;
        assert_eq!(new_response.status(), StatusCode::OK);

        let _ = std::fs::remove_file(config_path);
        let _ = std::fs::remove_dir_all(workspace_root);
        let _ = std::fs::remove_dir_all(workspace_b_root);
        let _ = std::fs::remove_dir_all(config_root);
    }
}
