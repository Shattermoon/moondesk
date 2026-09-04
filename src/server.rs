use axum::{
    Router,
    body::{Body, Bytes},
    extract::{DefaultBodyLimit, Path as AxumPath, Request, State},
    http::{HeaderMap, HeaderValue, Response, StatusCode, header},
    middleware::{self, Next},
    response::Json,
    routing::{delete, get, post},
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::net::IpAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use subtle::ConstantTimeEq;

use crate::browser_runtime::BrowserRuntime;
use crate::command_jobs::CommandJobManager;
use crate::mcp::{self, JsonRpcRequest};
use crate::state::{
    AddWorkspaceError, CommandActivityState, FlowDirection, ServerUiEvent, SharedState,
    UiEventSender, add_workspace,
};
use crate::workspaces::{
    self, WorkspaceId, WorkspaceRequestContext, WorkspaceRequestLease, WorkspaceRuntime,
};
use uuid::Uuid;

const STATELESS_FLOW_ID: &str = "stateless";
const STATELESS_FLOW_LABEL: &str = "stateless";
const MAX_HOST_CONTROL_BODY_BYTES: usize = 16 * 1024;
pub const HOST_CONTROL_ROUTE: &str = "/__moondesk/workspaces";
pub const HOST_CONTROL_HEADER: &str = "x-moondesk-host-token";

#[derive(Clone)]
struct ServerState {
    app: SharedState,
    browser_runtime: Option<Arc<BrowserRuntime>>,
    command_jobs: CommandJobManager,
    ui_events: UiEventSender,
    host_control_token: Arc<str>,
}

fn is_loopback_origin_host(host: &str) -> bool {
    if host.eq_ignore_ascii_case("localhost") {
        return true;
    }
    let host = host
        .strip_prefix('[')
        .and_then(|value| value.strip_suffix(']'))
        .unwrap_or(host);
    host.parse::<IpAddr>()
        .is_ok_and(|address| address.is_loopback())
}

fn same_http_origin(candidate: &reqwest::Url, expected: &reqwest::Url) -> bool {
    candidate.scheme().eq_ignore_ascii_case(expected.scheme())
        && candidate
            .host_str()
            .zip(expected.host_str())
            .is_some_and(|(candidate, expected)| candidate.eq_ignore_ascii_case(expected))
        && candidate.port_or_known_default() == expected.port_or_known_default()
}

async fn request_origin_allowed(state: &ServerState, headers: &HeaderMap) -> bool {
    let Some(origin) = headers.get(header::ORIGIN) else {
        // Non-browser MCP clients normally omit Origin. The MCP transport rule
        // only requires rejecting an Origin when one is present and invalid.
        return true;
    };
    let Ok(origin) = origin.to_str() else {
        return false;
    };
    let Ok(url) = reqwest::Url::parse(origin) else {
        return false;
    };
    if !matches!(url.scheme(), "http" | "https")
        || !url.username().is_empty()
        || url.password().is_some()
        || !matches!(url.path(), "" | "/")
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return false;
    }
    let Some(host) = url.host_str() else {
        return false;
    };
    if is_loopback_origin_host(host) {
        return true;
    }

    let app = state.app.lock().await;
    if app
        .ngrok_domain
        .as_deref()
        .and_then(|domain| reqwest::Url::parse(&format!("https://{domain}")).ok())
        .is_some_and(|expected| same_http_origin(&url, &expected))
    {
        return true;
    }
    app.ngrok_url
        .as_deref()
        .and_then(|value| reqwest::Url::parse(value).ok())
        .is_some_and(|expected| same_http_origin(&url, &expected))
}

async fn validate_request_origin(
    State(state): State<ServerState>,
    request: Request,
    next: Next,
) -> Response<Body> {
    if !request_origin_allowed(&state, request.headers()).await {
        return response_with_body(
            StatusCode::FORBIDDEN,
            "application/json",
            Body::from(r#"{"error":"forbidden origin"}"#),
        );
    }
    next.run(request).await
}
/// Build the axum router.
pub fn router(
    app_state: SharedState,
    browser_runtime: Option<Arc<BrowserRuntime>>,
    command_jobs: CommandJobManager,
    ui_events: UiEventSender,
    host_control_token: Arc<str>,
) -> Router {
    let state = ServerState {
        app: app_state,
        browser_runtime,
        command_jobs,
        ui_events,
        host_control_token,
    };
    let mcp_routes = Router::new()
        .route("/{slug}/mcp", post(post_mcp))
        .route("/{slug}/mcp", get(get_mcp))
        .route("/{slug}/mcp", delete(delete_mcp))
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            validate_request_origin,
        ));

    Router::new()
        .route("/", get(health))
        .route(
            HOST_CONTROL_ROUTE,
            post(register_workspace_from_local_host)
                .layer(DefaultBodyLimit::max(MAX_HOST_CONTROL_BODY_BYTES)),
        )
        .merge(mcp_routes)
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

fn tool_rate_limit_response(retry_after: Duration) -> Response<Body> {
    let mut response = jsonrpc_error_response(
        StatusCode::TOO_MANY_REQUESTS,
        -32000,
        "Tool invocation rate limit exceeded; retry shortly",
    );
    let retry_after_seconds = retry_after
        .as_secs()
        .saturating_add(u64::from(retry_after.subsec_nanos() != 0))
        .max(1);
    let header_value = match HeaderValue::from_str(&retry_after_seconds.to_string()) {
        Ok(value) => value,
        Err(_) => HeaderValue::from_static("1"),
    };
    response
        .headers_mut()
        .insert(header::RETRY_AFTER, header_value);
    response
}

async fn resolve_workspace(
    state: &ServerState,
    slug: &str,
) -> Option<(
    WorkspaceRequestContext,
    Arc<WorkspaceRuntime>,
    WorkspaceRequestLease,
)> {
    let app = state.app.lock().await;
    let workspace = workspaces::resolve_workspace_by_slug(&app.workspaces, slug)?;
    let runtime = app.workspace_runtimes.get(&workspace.workspace_id)?.clone();
    let lease = runtime.try_acquire()?;
    Some((workspace, runtime, lease))
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
) -> (Option<CommandUiRequest>, Option<ServerUiEvent>) {
    if req.get("method").and_then(Value::as_str) != Some("tools/call") {
        return (None, None);
    }
    let Some(tool) = request_tool_name(req) else {
        return (None, None);
    };
    let Some(arguments) = tool_arguments(req) else {
        return (None, None);
    };
    match tool.as_str() {
        "run_command" | "start_command" => {
            let Some(command) = arguments.get("command").and_then(Value::as_str) else {
                return (None, None);
            };
            let activity_id = Uuid::new_v4().to_string();
            let background = tool == "start_command";
            let event = ServerUiEvent::CommandStarted {
                workspace_id: workspace_id.clone(),
                activity_id: activity_id.clone(),
                command: command.to_string(),
                background,
            };
            let request = if background {
                CommandUiRequest::Start { activity_id }
            } else {
                CommandUiRequest::Run { activity_id }
            };
            (Some(request), Some(event))
        }
        "poll_command" => (
            arguments
                .get("job_id")
                .and_then(Value::as_str)
                .map(|job_id| CommandUiRequest::Poll {
                    job_id: job_id.to_string(),
                }),
            None,
        ),
        "cancel_command" => (
            arguments
                .get("job_id")
                .and_then(Value::as_str)
                .map(|job_id| CommandUiRequest::Cancel {
                    job_id: job_id.to_string(),
                }),
            None,
        ),
        _ => (None, None),
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
) -> Vec<ServerUiEvent> {
    let state = command_response_state(response);
    let exit_code = command_response_exit_code(response);
    let preview = command_response_preview(response);
    match request {
        CommandUiRequest::Run { activity_id } => vec![ServerUiEvent::CommandUpdated {
            workspace_id: workspace_id.clone(),
            activity_id: Some(activity_id.clone()),
            job_id: None,
            state,
            exit_code,
            preview,
        }],
        CommandUiRequest::Start { activity_id } => {
            let mut events = Vec::with_capacity(2);
            if let Some(job_id) = tool_structured_content(response)
                .and_then(|value| value.get("jobId"))
                .and_then(Value::as_str)
            {
                events.push(ServerUiEvent::CommandBoundToJob {
                    workspace_id: workspace_id.clone(),
                    activity_id: activity_id.clone(),
                    job_id: job_id.to_string(),
                });
            }
            events.push(ServerUiEvent::CommandUpdated {
                workspace_id: workspace_id.clone(),
                activity_id: Some(activity_id.clone()),
                job_id: None,
                state,
                exit_code,
                preview,
            });
            events
        }
        CommandUiRequest::Poll { job_id } | CommandUiRequest::Cancel { job_id } => {
            vec![ServerUiEvent::CommandUpdated {
                workspace_id: workspace_id.clone(),
                activity_id: None,
                job_id: Some(job_id.clone()),
                state,
                exit_code,
                preview,
            }]
        }
    }
}

#[derive(Deserialize)]
struct HostWorkspaceRegistrationRequest {
    root: String,
    name: Option<String>,
}

fn host_control_authorized(headers: &HeaderMap, expected_token: &str) -> bool {
    let Some(candidate) = headers
        .get(HOST_CONTROL_HEADER)
        .and_then(|value| value.to_str().ok())
    else {
        return false;
    };
    candidate.as_bytes().ct_eq(expected_token.as_bytes()).into()
}

fn host_workspace_add_error_status(error: &AddWorkspaceError) -> StatusCode {
    match error {
        AddWorkspaceError::Validation(_) => StatusCode::CONFLICT,
        AddWorkspaceError::Persistence(_) => StatusCode::INTERNAL_SERVER_ERROR,
    }
}

async fn register_workspace_from_local_host(
    State(s): State<ServerState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response<Body> {
    // This route is reachable through the same local HTTP server that ngrok can
    // tunnel, so possession of the per-host runtime token is mandatory. Invalid
    // callers get the same generic response as an unknown route.
    if !host_control_authorized(&headers, &s.host_control_token) {
        return not_found_response();
    }
    if body.len() > MAX_HOST_CONTROL_BODY_BYTES {
        return response_with_body(
            StatusCode::PAYLOAD_TOO_LARGE,
            "application/json",
            Body::from(r#"{"error":"request too large"}"#),
        );
    }

    let request: HostWorkspaceRegistrationRequest = match serde_json::from_slice(&body) {
        Ok(request) => request,
        Err(_) => {
            return response_with_body(
                StatusCode::BAD_REQUEST,
                "application/json",
                Body::from(r#"{"error":"invalid request"}"#),
            );
        }
    };

    let requested_root = PathBuf::from(request.root);
    let canonical_root = match tokio::task::spawn_blocking(move || {
        workspaces::canonicalize_existing_workspace_root(&requested_root)
    })
    .await
    {
        Ok(Ok(root)) => root,
        Ok(Err(error)) => {
            return response_with_body(
                StatusCode::BAD_REQUEST,
                "application/json",
                Body::from(json!({"error": error}).to_string()),
            );
        }
        Err(error) => {
            return response_with_body(
                StatusCode::INTERNAL_SERVER_ERROR,
                "application/json",
                Body::from(
                    json!({"error": format!("workspace path validation failed: {error}")})
                        .to_string(),
                ),
            );
        }
    };

    let existing = {
        let app = s.app.lock().await;
        app.workspaces
            .iter()
            .find(|workspace| workspace.root == canonical_root)
            .cloned()
    };
    if let Some(workspace) = existing {
        return response_with_body(
            StatusCode::OK,
            "application/json",
            Body::from(
                json!({
                    "status": "ok",
                    "workspaceName": workspace.name,
                    "alreadyRegistered": true
                })
                .to_string(),
            ),
        );
    }

    let name = request
        .name
        .as_deref()
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| workspaces::derive_workspace_name(&canonical_root));
    match add_workspace(&s.app, name, canonical_root).await {
        Ok(workspace) => response_with_body(
            StatusCode::CREATED,
            "application/json",
            Body::from(
                json!({
                    "status": "ok",
                    "workspaceName": workspace.name,
                    "alreadyRegistered": false
                })
                .to_string(),
            ),
        ),
        Err(error) => {
            let status = host_workspace_add_error_status(&error);
            response_with_body(
                status,
                "application/json",
                Body::from(json!({"error": error.to_string()}).to_string()),
            )
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
    let Some((workspace, runtime, _request_lease)) = resolve_workspace(&s, &slug).await else {
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

    if body.get("method").and_then(Value::as_str) == Some("tools/call")
        && let Err(retry_after) = runtime.check_tool_invocation()
    {
        return tool_rate_limit_response(retry_after);
    }

    {
        let mut app = s.app.lock().await;
        app.apply_server_ui_event(ServerUiEvent::IncrementRequestCount {
            workspace_id: workspace.workspace_id.clone(),
        });
        app.apply_server_ui_event(ServerUiEvent::SetRemoteConnected {
            workspace_id: workspace.workspace_id.clone(),
            connected: true,
        });
    }

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

    s.app
        .lock()
        .await
        .apply_server_ui_event(ServerUiEvent::RecordFlow {
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
    // TUI-only observability stays local to MoonDesk. Apply state transitions
    // directly so a full best-effort log queue cannot lose command lifecycle state.
    let (command_ui_request, command_start_event) =
        begin_command_ui_request(&body, &workspace.workspace_id);
    if let Some(event) = command_start_event {
        s.app.lock().await.apply_server_ui_event(event);
    }

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
            browser_runtime: &s.browser_runtime,
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
            let events = finish_command_ui_request(
                command_ui_request,
                &response_value,
                &workspace.workspace_id,
            );
            let mut app = s.app.lock().await;
            for event in events {
                app.apply_server_ui_event(event);
            }
        }
        response_json = Some(response_value);
    }

    {
        if req.id.is_some() {
            s.app
                .lock()
                .await
                .apply_server_ui_event(ServerUiEvent::RecordFlow {
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
    let Some((_workspace, _runtime, _request_lease)) = resolve_workspace(&s, &slug).await else {
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
    let Some((workspace, _runtime, _request_lease)) = resolve_workspace(&s, &slug).await else {
        return not_found_response();
    };
    {
        let mut app = s.app.lock().await;
        app.apply_server_ui_event(ServerUiEvent::SetRemoteConnected {
            workspace_id: workspace.workspace_id.clone(),
            connected: false,
        });
        app.apply_server_ui_event(ServerUiEvent::BeginFlowClose {
            workspace_id: workspace.workspace_id.clone(),
            flow_id: STATELESS_FLOW_ID.to_string(),
        });
    }
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
        let (observation, start_event) = begin_command_ui_request(&start_request, &workspace_id);
        let observation = observation.expect("observe start command");
        let activity_id = match start_event.expect("immediate command-start event") {
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
        let events = finish_command_ui_request(&observation, &response, &workspace_id);
        assert_eq!(response, response_before);
        assert_eq!(events.len(), 2);

        match &events[0] {
            ServerUiEvent::CommandBoundToJob {
                activity_id: bound_activity_id,
                job_id,
                ..
            } => {
                assert_eq!(bound_activity_id, &activity_id);
                assert_eq!(job_id, "job-1");
            }
            _ => panic!("expected command-job binding"),
        }
        match &events[1] {
            ServerUiEvent::CommandUpdated { state, .. } => {
                assert_eq!(*state, CommandActivityState::Running);
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
        let (poll_observation, poll_start_event) =
            begin_command_ui_request(&poll_request, &workspace_id);
        let poll_observation = poll_observation.expect("observe poll command");
        assert!(
            poll_start_event.is_none(),
            "poll must not add a command row"
        );
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
        let poll_events =
            finish_command_ui_request(&poll_observation, &poll_response, &workspace_id);
        assert_eq!(poll_events.len(), 1);
        match &poll_events[0] {
            ServerUiEvent::CommandUpdated {
                job_id,
                state,
                preview,
                ..
            } => {
                assert_eq!(job_id.as_deref(), Some("job-1"));
                assert_eq!(*state, CommandActivityState::Succeeded);
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

    #[test]
    fn host_workspace_registration_maps_internal_persistence_failures_to_500() {
        let validation = AddWorkspaceError::Validation("bad workspace".into());
        let persistence = AddWorkspaceError::Persistence("disk failure".into());
        assert_eq!(
            host_workspace_add_error_status(&validation),
            StatusCode::CONFLICT
        );
        assert_eq!(
            host_workspace_add_error_status(&persistence),
            StatusCode::INTERNAL_SERVER_ERROR
        );
    }

    #[tokio::test]
    async fn host_control_route_rejects_oversized_body_before_handler_extraction() {
        let workspace_root = unique_temp_path("moondesk-host-body-limit-workspace");
        let config_root = unique_temp_path("moondesk-host-body-limit-config");
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
        let (ui_tx, _ui_rx) = ui_event_channel();
        let app = router(
            app_state,
            None,
            CommandJobManager::new(),
            ui_tx,
            Arc::from("test-host-control-token"),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind test server");
        let address = listener.local_addr().expect("test server address");
        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let response = reqwest::Client::new()
            .post(format!("http://{address}{HOST_CONTROL_ROUTE}"))
            .header(HOST_CONTROL_HEADER, "test-host-control-token")
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(vec![b'x'; MAX_HOST_CONTROL_BODY_BYTES + 1])
            .send()
            .await
            .expect("send oversized host-control request");
        assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);

        server.abort();
        let _ = server.await;
        let _ = std::fs::remove_file(config_path);
        let _ = std::fs::remove_dir_all(config_root);
        let _ = std::fs::remove_dir_all(workspace_root);
    }

    #[tokio::test]
    async fn router_rejects_invalid_origins_without_blocking_normal_clients() {
        let workspace_root = unique_temp_path("moondesk-origin-workspace");
        let config_root = unique_temp_path("moondesk-origin-config");
        let config_path = config_root.join("config.toml");
        std::fs::create_dir_all(&workspace_root).expect("create workspace");
        std::fs::create_dir_all(&config_root).expect("create config dir");

        let mut app = AppState::new_for_test(
            8787,
            workspace_root.to_string_lossy().into_owned(),
            config_path.clone(),
        )
        .expect("create app state");
        app.ngrok_domain = Some("moon-origin-test.ngrok-free.app".into());
        let slug = app.mcp_slug.clone();
        let app_state = Arc::new(Mutex::new(app));
        let (ui_tx, _ui_rx) = ui_event_channel();
        let app = router(
            app_state,
            None,
            CommandJobManager::new(),
            ui_tx,
            Arc::from("test-host-control-token"),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind test server");
        let address = listener.local_addr().expect("test server address");
        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        let endpoint = format!("http://{address}/{slug}/mcp");
        let payload = r#"{"jsonrpc":"2.0","id":"origin-check","method":"ping","params":{}}"#;
        let client = reqwest::Client::new();

        let no_origin = client
            .post(&endpoint)
            .body(payload)
            .send()
            .await
            .expect("send request without Origin");
        assert_eq!(no_origin.status(), StatusCode::OK);

        for local_origin in [
            "http://localhost:9999",
            "http://127.0.0.1:9999",
            "http://127.0.0.2:9999",
            "http://[::1]:9999",
        ] {
            let response = client
                .post(&endpoint)
                .header(reqwest::header::ORIGIN, local_origin)
                .body(payload)
                .send()
                .await
                .expect("send local-origin request");
            assert_eq!(response.status(), StatusCode::OK, "{local_origin}");
        }

        let public_origin = client
            .post(&endpoint)
            .header(
                reqwest::header::ORIGIN,
                "https://moon-origin-test.ngrok-free.app",
            )
            .body(payload)
            .send()
            .await
            .expect("send configured-origin request");
        assert_eq!(public_origin.status(), StatusCode::OK);

        let public_origin_default_port = client
            .post(&endpoint)
            .header(
                reqwest::header::ORIGIN,
                "https://moon-origin-test.ngrok-free.app:443",
            )
            .body(payload)
            .send()
            .await
            .expect("send configured-origin request with explicit default port");
        assert_eq!(public_origin_default_port.status(), StatusCode::OK);

        for invalid_public_origin in [
            "http://moon-origin-test.ngrok-free.app",
            "https://moon-origin-test.ngrok-free.app:8443",
        ] {
            let response = client
                .post(&endpoint)
                .header(reqwest::header::ORIGIN, invalid_public_origin)
                .body(payload)
                .send()
                .await
                .expect("send wrong public origin tuple");
            assert_eq!(
                response.status(),
                StatusCode::FORBIDDEN,
                "{invalid_public_origin}"
            );
        }

        let bad_origin = client
            .post(&endpoint)
            .header(reqwest::header::ORIGIN, "https://attacker.example")
            .body(payload)
            .send()
            .await
            .expect("send invalid-origin request");
        assert_eq!(bad_origin.status(), StatusCode::FORBIDDEN);

        let bad_get_origin = client
            .get(&endpoint)
            .header(reqwest::header::ORIGIN, "https://attacker.example")
            .send()
            .await
            .expect("send invalid-origin GET request");
        assert_eq!(bad_get_origin.status(), StatusCode::FORBIDDEN);

        let bad_delete_origin = client
            .delete(&endpoint)
            .header(reqwest::header::ORIGIN, "https://attacker.example")
            .send()
            .await
            .expect("send invalid-origin DELETE request");
        assert_eq!(bad_delete_origin.status(), StatusCode::FORBIDDEN);

        let health_with_foreign_origin = client
            .get(format!("http://{address}/"))
            .header(reqwest::header::ORIGIN, "https://attacker.example")
            .send()
            .await
            .expect("send health request with unrelated Origin");
        assert_eq!(
            health_with_foreign_origin.status(),
            StatusCode::OK,
            "Origin validation must stay scoped to MCP routes"
        );

        let opaque_origin = client
            .post(&endpoint)
            .header(reqwest::header::ORIGIN, "null")
            .body(payload)
            .send()
            .await
            .expect("send opaque-origin request");
        assert_eq!(opaque_origin.status(), StatusCode::FORBIDDEN);

        for invalid_origin in [
            "https://moon-origin-test.ngrok-free.app/path",
            "https://user@moon-origin-test.ngrok-free.app",
            "file://moon-origin-test.ngrok-free.app",
        ] {
            let response = client
                .post(&endpoint)
                .header(reqwest::header::ORIGIN, invalid_origin)
                .body(payload)
                .send()
                .await
                .expect("send syntactically invalid Origin");
            assert_eq!(response.status(), StatusCode::FORBIDDEN, "{invalid_origin}");
        }

        server.abort();
        let _ = server.await;
        let _ = std::fs::remove_file(config_path);
        let _ = std::fs::remove_dir_all(config_root);
        let _ = std::fs::remove_dir_all(workspace_root);
    }

    #[tokio::test]
    async fn host_workspace_registration_is_authenticated_and_idempotent() {
        let workspace_a = unique_temp_path("moondesk-host-register-a");
        let workspace_b = unique_temp_path("moondesk-host-register-b");
        let config_root = unique_temp_path("moondesk-host-register-config");
        let config_path = config_root.join("config.toml");
        std::fs::create_dir_all(&workspace_a).expect("create workspace A");
        std::fs::create_dir_all(&workspace_b).expect("create workspace B");
        std::fs::create_dir_all(&config_root).expect("create config dir");

        let app = AppState::new_for_test(
            8787,
            workspace_a.to_string_lossy().into_owned(),
            config_path.clone(),
        )
        .expect("create app state");
        let app_state = Arc::new(Mutex::new(app));
        let (ui_tx, _ui_rx) = ui_event_channel();
        let server_state = ServerState {
            app: app_state.clone(),
            browser_runtime: None,
            command_jobs: CommandJobManager::new(),
            ui_events: ui_tx,
            host_control_token: Arc::from("test-host-control-token"),
        };
        let request = Bytes::from(
            serde_json::to_vec(&json!({"root": workspace_b.to_string_lossy()}))
                .expect("serialize registration request"),
        );

        let denied = register_workspace_from_local_host(
            State(server_state.clone()),
            HeaderMap::new(),
            request.clone(),
        )
        .await;
        assert_eq!(denied.status(), StatusCode::NOT_FOUND);
        assert_eq!(app_state.lock().await.workspaces.len(), 1);

        let mut headers = HeaderMap::new();
        headers.insert(
            HOST_CONTROL_HEADER,
            HeaderValue::from_static("test-host-control-token"),
        );

        let oversized_name = "x".repeat(workspaces::MAX_WORKSPACE_NAME_CHARS + 1);
        let invalid_name_request = Bytes::from(
            serde_json::to_vec(&json!({
                "root": workspace_b.to_string_lossy(),
                "name": oversized_name,
            }))
            .expect("serialize invalid registration request"),
        );
        let invalid_name = register_workspace_from_local_host(
            State(server_state.clone()),
            headers.clone(),
            invalid_name_request,
        )
        .await;
        assert_eq!(invalid_name.status(), StatusCode::CONFLICT);
        assert_eq!(app_state.lock().await.workspaces.len(), 1);

        let oversized_body = Bytes::from(vec![b'x'; MAX_HOST_CONTROL_BODY_BYTES + 1]);
        let oversized = register_workspace_from_local_host(
            State(server_state.clone()),
            headers.clone(),
            oversized_body,
        )
        .await;
        assert_eq!(oversized.status(), StatusCode::PAYLOAD_TOO_LARGE);

        let created = register_workspace_from_local_host(
            State(server_state.clone()),
            headers.clone(),
            request.clone(),
        )
        .await;
        assert_eq!(created.status(), StatusCode::CREATED);
        assert_eq!(app_state.lock().await.workspaces.len(), 2);

        let repeated =
            register_workspace_from_local_host(State(server_state), headers, request).await;
        assert_eq!(repeated.status(), StatusCode::OK);
        assert_eq!(app_state.lock().await.workspaces.len(), 2);

        let _ = std::fs::remove_file(config_path);
        let _ = std::fs::remove_dir_all(config_root);
        let _ = std::fs::remove_dir_all(workspace_a);
        let _ = std::fs::remove_dir_all(workspace_b);
    }

    #[tokio::test]
    async fn tool_invocation_rate_limit_returns_429_without_blocking_ping() {
        let workspace_root = unique_temp_path("moondesk-rate-workspace");
        let config_root = unique_temp_path("moondesk-rate-config");
        let config_path = config_root.join("config.toml");
        std::fs::create_dir_all(&workspace_root).expect("create workspace");
        std::fs::create_dir_all(&config_root).expect("create config dir");

        let app = AppState::new_for_test(
            8787,
            workspace_root.to_string_lossy().into_owned(),
            config_path.clone(),
        )
        .expect("create app state");
        let slug = app.mcp_slug.clone();
        let workspace_id = app.workspaces[0].id.clone();
        let runtime = app.workspace_runtimes[&workspace_id].clone();
        for _ in 0..workspaces::MAX_TOOL_INVOCATIONS_PER_WINDOW {
            assert!(runtime.check_tool_invocation().is_ok());
        }

        let app_state = Arc::new(Mutex::new(app));
        let (ui_tx, _ui_rx) = ui_event_channel();
        let server_state = ServerState {
            app: app_state,
            browser_runtime: None,
            command_jobs: CommandJobManager::new(),
            ui_events: ui_tx,
            host_control_token: Arc::from("test-host-control-token"),
        };

        let limited = post_mcp(
            AxumPath(slug.clone()),
            State(server_state.clone()),
            tool_call_body("read", json!({ "path": "README.md" })),
        )
        .await;
        assert_eq!(limited.status(), StatusCode::TOO_MANY_REQUESTS);
        let retry_after = limited
            .headers()
            .get(header::RETRY_AFTER)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse::<u64>().ok())
            .expect("429 response should include an integer Retry-After header");
        assert!((1..=workspaces::TOOL_INVOCATION_RATE_WINDOW.as_secs()).contains(&retry_after));

        let ping = post_mcp(
            AxumPath(slug),
            State(server_state),
            Bytes::from_static(
                br#"{"jsonrpc":"2.0","id":"rate-ping","method":"ping","params":{}}"#,
            ),
        )
        .await;
        assert_eq!(ping.status(), StatusCode::OK);

        let _ = std::fs::remove_file(config_path);
        let _ = std::fs::remove_dir_all(config_root);
        let _ = std::fs::remove_dir_all(workspace_root);
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
            browser_runtime: None,
            command_jobs: command_jobs.clone(),
            ui_events: ui_tx,
            host_control_token: Arc::from("test-host-control-token"),
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
            browser_runtime: None,
            command_jobs: CommandJobManager::new(),
            ui_events: ui_tx,
            host_control_token: Arc::from("test-host-control-token"),
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
    async fn post_mcp_preserves_native_image_content() {
        use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64_STANDARD};

        let workspace_root = unique_temp_path("moondesk-post-mcp-image-workspace");
        let config_root = unique_temp_path("moondesk-post-mcp-image-config");
        let config_path = config_root.join("config.toml");
        std::fs::create_dir_all(&workspace_root).expect("create workspace");
        std::fs::create_dir_all(&config_root).expect("create config dir");
        let image_path = workspace_root.join("sample.png");
        image::RgbaImage::from_pixel(32, 24, image::Rgba([25, 100, 210, 255]))
            .save(&image_path)
            .expect("write PNG fixture");
        let original = std::fs::read(&image_path).expect("read original PNG fixture");

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
            app: app_state,
            browser_runtime: None,
            command_jobs: CommandJobManager::new(),
            ui_events: ui_tx,
            host_control_token: Arc::from("test-host-control-token"),
        };

        let response = post_mcp(
            AxumPath(mcp_slug),
            State(server_state),
            tool_call_body("view_image", json!({ "path": "sample.png" })),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("read image response body");
        let payload: Value = serde_json::from_slice(&body).expect("parse image response JSON");
        assert_eq!(
            payload
                .pointer("/result/content/0/type")
                .and_then(Value::as_str),
            Some("image")
        );
        assert_eq!(
            payload
                .pointer("/result/content/0/mimeType")
                .and_then(Value::as_str),
            Some("image/png")
        );
        let attached = payload
            .pointer("/result/content/0/data")
            .and_then(Value::as_str)
            .and_then(|data| BASE64_STANDARD.decode(data).ok())
            .expect("decode native MCP image payload");
        assert_eq!(
            attached, original,
            "HTTP MCP transport must preserve image bytes"
        );
        assert_eq!(
            payload
                .pointer("/result/structuredContent/width")
                .and_then(Value::as_u64),
            Some(32)
        );
        assert_eq!(
            payload
                .pointer("/result/structuredContent/height")
                .and_then(Value::as_u64),
            Some(24)
        );

        let _ = std::fs::remove_file(image_path);
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
            browser_runtime: None,
            command_jobs: CommandJobManager::new(),
            ui_events: ui_tx,
            host_control_token: Arc::from("test-host-control-token"),
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

        let external_file = unique_temp_path("moondesk-routing-external-file");
        std::fs::write(&external_file, "external-readable\n")
            .expect("write external readable file");
        let external_read = post_mcp(
            AxumPath(slug_a.clone()),
            State(server_state.clone()),
            tool_call_body("read", json!({ "path": external_file.to_string_lossy() })),
        )
        .await;
        assert_eq!(external_read.status(), StatusCode::OK);
        let external_body = to_bytes(external_read.into_body(), usize::MAX)
            .await
            .expect("read external-file response");
        let external_payload: Value =
            serde_json::from_slice(&external_body).expect("parse external-file response");
        assert_eq!(
            external_payload
                .pointer("/result/structuredContent/text")
                .and_then(Value::as_str),
            Some("external-readable\n")
        );

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
                .pointer("/result/structuredContent/text")
                .and_then(Value::as_str),
            Some("workspace-b\n"),
            "MultiTools explicit absolute reads must not gain special-case restrictions just because the target is another registered workspace"
        );

        let workspace_b_image = workspace_b.join("other-workspace.png");
        image::RgbaImage::from_pixel(20, 16, image::Rgba([90, 120, 150, 255]))
            .save(&workspace_b_image)
            .expect("write workspace B image");
        let cross_root_vision = post_mcp(
            AxumPath(slug_a.clone()),
            State(server_state.clone()),
            tool_call_body(
                "view_image",
                json!({ "path": workspace_b_image.to_string_lossy() }),
            ),
        )
        .await;
        assert_eq!(cross_root_vision.status(), StatusCode::OK);
        let cross_vision_body = to_bytes(cross_root_vision.into_body(), usize::MAX)
            .await
            .expect("read cross-root vision response");
        let cross_vision_payload: Value =
            serde_json::from_slice(&cross_vision_body).expect("parse cross-root vision response");
        assert_ne!(
            cross_vision_payload
                .pointer("/result/isError")
                .and_then(Value::as_bool),
            Some(true),
            "MultiTools explicit absolute image reads must stay usable across project roots"
        );
        assert_eq!(
            cross_vision_payload
                .pointer("/result/content/0/type")
                .and_then(Value::as_str),
            Some("image")
        );

        let cross_root_batch_vision = post_mcp(
            AxumPath(slug_a.clone()),
            State(server_state.clone()),
            tool_call_body(
                "view_images",
                json!({ "paths": [workspace_b_image.to_string_lossy()] }),
            ),
        )
        .await;
        assert_eq!(cross_root_batch_vision.status(), StatusCode::OK);
        let cross_batch_body = to_bytes(cross_root_batch_vision.into_body(), usize::MAX)
            .await
            .expect("read cross-root batch vision response");
        let cross_batch_payload: Value = serde_json::from_slice(&cross_batch_body)
            .expect("parse cross-root batch vision response");
        assert_ne!(
            cross_batch_payload
                .pointer("/result/isError")
                .and_then(Value::as_bool),
            Some(true),
            "MultiTools explicit absolute image batches must stay usable across project roots"
        );
        assert_eq!(
            cross_batch_payload
                .pointer("/result/content/1/type")
                .and_then(Value::as_str),
            Some("image")
        );

        let _ = std::fs::remove_file(external_file);

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
            browser_runtime: None,
            command_jobs: CommandJobManager::new(),
            ui_events: ui_tx,
            host_control_token: Arc::from("test-host-control-token"),
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
        let (ui_tx, _ui_rx) = ui_event_channel();
        let server_state = ServerState {
            app: app_state.clone(),
            browser_runtime: None,
            command_jobs: CommandJobManager::new(),
            ui_events: ui_tx,
            host_control_token: Arc::from("test-host-control-token"),
        };

        let response = delete_mcp(AxumPath(slug_a), State(server_state)).await;
        assert_eq!(response.status(), StatusCode::OK);
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
    async fn command_state_survives_a_full_transient_log_queue() {
        let workspace_root = unique_temp_path("moondesk-full-ui-queue-workspace");
        let config_root = unique_temp_path("moondesk-full-ui-queue-config");
        let config_path = config_root.join("config.toml");
        std::fs::create_dir_all(&workspace_root).expect("create workspace");
        std::fs::create_dir_all(&config_root).expect("create config root");

        let app = AppState::new_for_test(
            8787,
            workspace_root.to_string_lossy().into_owned(),
            config_path.clone(),
        )
        .expect("create app state");
        let slug = app.workspaces[0].mcp_slug.clone();
        let app_state = Arc::new(Mutex::new(app));
        let (ui_tx, _ui_rx) = ui_event_channel();
        for index in 0..crate::state::UI_EVENT_QUEUE_CAPACITY {
            ui_tx
                .send(ServerUiEvent::Log {
                    workspace_id: None,
                    level: "INFO",
                    message: format!("queued-log-{index}"),
                })
                .expect("fill transient log queue");
        }
        assert!(
            ui_tx
                .send(ServerUiEvent::Log {
                    workspace_id: None,
                    level: "INFO",
                    message: "overflow".into(),
                })
                .is_err(),
            "test must actually saturate the transient queue"
        );

        let server_state = ServerState {
            app: app_state.clone(),
            browser_runtime: None,
            command_jobs: CommandJobManager::new(),
            ui_events: ui_tx,
            host_control_token: Arc::from("test-host-control-token"),
        };
        let command = if cfg!(windows) {
            "Write-Output queue-ok"
        } else {
            "printf 'queue-ok\n'"
        };
        let response = post_mcp(
            AxumPath(slug),
            State(server_state),
            tool_call_body("run_command", json!({ "command": command })),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);

        let app = app_state.lock().await;
        let activity = app
            .command_activities
            .back()
            .expect("command state must bypass transient log queue");
        assert_eq!(activity.command, command);
        assert_eq!(activity.state, CommandActivityState::Succeeded);
        drop(app);

        let _ = std::fs::remove_file(config_path);
        let _ = std::fs::remove_dir_all(config_root);
        let _ = std::fs::remove_dir_all(workspace_root);
    }

    #[tokio::test]
    async fn routine_request_logs_do_not_expose_workspace_secret_slug() {
        let workspace_root = unique_temp_path("moondesk-secret-log-workspace");
        let config_root = unique_temp_path("moondesk-secret-log-config");
        let config_path = config_root.join("config.toml");
        std::fs::create_dir_all(&workspace_root).expect("create workspace");
        std::fs::create_dir_all(&config_root).expect("create config root");

        let app = AppState::new_for_test(
            8787,
            workspace_root.to_string_lossy().into_owned(),
            config_path.clone(),
        )
        .expect("create app state");
        let slug = app.workspaces[0].mcp_slug.clone();
        let app_state = Arc::new(Mutex::new(app));
        let (ui_tx, mut ui_rx) = ui_event_channel();
        let server_state = ServerState {
            app: app_state,
            browser_runtime: None,
            command_jobs: CommandJobManager::new(),
            ui_events: ui_tx,
            host_control_token: Arc::from("test-host-control-token"),
        };

        let response = post_mcp(
            AxumPath(slug.clone()),
            State(server_state),
            Bytes::from_static(
                br#"{"jsonrpc":"2.0","id":"secret-log-check","method":"ping","params":{}}"#,
            ),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);

        let mut observed_log = false;
        while let Ok(event) = ui_rx.try_recv() {
            if let ServerUiEvent::Log { message, .. } = event {
                observed_log = true;
                assert!(
                    !message.contains(&slug),
                    "routine request log leaked workspace MCP secret: {message}"
                );
            }
        }
        assert!(
            observed_log,
            "request should emit at least one local log entry"
        );

        let _ = std::fs::remove_file(config_path);
        let _ = std::fs::remove_dir_all(config_root);
        let _ = std::fs::remove_dir_all(workspace_root);
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
            browser_runtime: None,
            command_jobs: CommandJobManager::new(),
            ui_events: ui_tx,
            host_control_token: Arc::from("test-host-control-token"),
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
