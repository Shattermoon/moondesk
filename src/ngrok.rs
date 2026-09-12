use crate::state::SharedState;
use ngrok::prelude::*;
use reqwest::{StatusCode, Url};
use std::{fmt, time::Duration};

const PUBLIC_TUNNEL_PROBE_TIMEOUT: Duration = Duration::from_secs(2);
const PUBLIC_TUNNEL_BAD_GATEWAY_RETRIES: usize = 3;
const PUBLIC_TUNNEL_RETRY_DELAY: Duration = Duration::from_millis(200);
const FORWARDER_CLOSE_TIMEOUT: Duration = Duration::from_secs(2);
const BAD_GATEWAY_DETAIL_LIMIT: usize = 512;

#[derive(Debug)]
pub enum StartFailure {
    Authentication(String),
    Other(String),
}

impl StartFailure {
    pub fn is_authentication(&self) -> bool {
        matches!(self, Self::Authentication(_))
    }
}

impl fmt::Display for StartFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Authentication(message) | Self::Other(message) => formatter.write_str(message),
        }
    }
}

impl std::error::Error for StartFailure {}

fn local_forward_targets(port: u16) -> [String; 2] {
    [
        format!("http://127.0.0.1:{port}"),
        format!("http://localhost:{port}"),
    ]
}

async fn public_tunnel_bad_gateway(public_url: &str) -> Result<Option<String>, String> {
    let endpoint = format!("{}/", public_url.trim_end_matches('/'));
    let client = reqwest::Client::builder()
        .timeout(PUBLIC_TUNNEL_PROBE_TIMEOUT)
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|error| format!("could not create tunnel probe client: {error}"))?;

    let mut last_detail = None;
    for attempt in 0..PUBLIC_TUNNEL_BAD_GATEWAY_RETRIES {
        match client
            .get(&endpoint)
            .header("ngrok-skip-browser-warning", "true")
            .send()
            .await
        {
            Ok(response) if response.status() == StatusCode::BAD_GATEWAY => {
                let detail = match response.bytes().await {
                    Ok(body) => {
                        let detail_len = body.len().min(BAD_GATEWAY_DETAIL_LIMIT);
                        String::from_utf8_lossy(&body[..detail_len])
                            .trim()
                            .to_string()
                    }
                    Err(_) => String::new(),
                };
                last_detail = Some(if detail.is_empty() {
                    "ngrok returned HTTP 502 Bad Gateway".to_string()
                } else {
                    format!("ngrok returned HTTP 502 Bad Gateway: {detail}")
                });
                if attempt + 1 < PUBLIC_TUNNEL_BAD_GATEWAY_RETRIES {
                    tokio::time::sleep(PUBLIC_TUNNEL_RETRY_DELAY).await;
                }
            }
            Ok(_) => return Ok(None),
            Err(error) => {
                return Err(format!("could not self-probe public ngrok URL: {error}"));
            }
        }
    }

    Ok(last_detail)
}

fn ensure_local_server_running(server_running: bool) -> Result<(), StartFailure> {
    if server_running {
        Ok(())
    } else {
        Err(StartFailure::Other(
            "Local MCP server exited before the ngrok tunnel could be published".into(),
        ))
    }
}

async fn close_forwarder<T>(forwarder: &mut ngrok::forwarder::Forwarder<T>)
where
    T: TunnelCloser + TunnelInfo + Send,
{
    let _ = forwarder.close().await;
    if tokio::time::timeout(FORWARDER_CLOSE_TIMEOUT, forwarder.join())
        .await
        .is_err()
    {
        forwarder.join().abort();
        let _ = forwarder.join().await;
    }
}

/// Start an ngrok HTTP tunnel using the embedded Rust SDK.
pub async fn start(state: SharedState) -> Result<(), StartFailure> {
    let (port, authtoken, configured_domain) = {
        let app = state.lock().await;
        if app.ngrok_running {
            return Err(StartFailure::Other("ngrok is already running".into()));
        }
        let authtoken = app.ngrok_authtoken().map(str::to_string).ok_or_else(|| {
            StartFailure::Authentication("ngrok authtoken is not configured".into())
        })?;
        (app.port, authtoken, app.ngrok_domain.clone())
    };

    let session = ngrok::Session::builder()
        .authtoken(authtoken)
        .connect()
        .await
        .map_err(|error| {
            let message = format!("Failed to connect ngrok session: {error}");
            if matches!(error, ::ngrok::session::ConnectError::Auth(_)) {
                StartFailure::Authentication(message)
            } else {
                StartFailure::Other(message)
            }
        })?;

    let forward_targets = local_forward_targets(port);
    let mut selected_forwarder = None;
    let mut selected_url = None;
    let mut selected_target = None;

    for (index, target) in forward_targets.iter().enumerate() {
        let forwards_to: Url = target
            .parse()
            .map_err(|error| StartFailure::Other(format!("Invalid forward URL: {error}")))?;
        let mut http_endpoint = session.http_endpoint();
        if let Some(domain) = configured_domain.as_deref()
            && !domain.is_empty()
        {
            http_endpoint.domain(domain);
        }

        let mut candidate = http_endpoint
            .listen_and_forward(forwards_to)
            .await
            .map_err(|error| {
                StartFailure::Other(format!("Failed to open ngrok tunnel: {error}"))
            })?;
        let candidate_url = candidate.url().to_string();

        match public_tunnel_bad_gateway(&candidate_url).await {
            Ok(Some(detail)) if index + 1 < forward_targets.len() => {
                state.lock().await.log(
                    "WARN",
                    format!(
                        "ngrok could not reach local upstream {target}: {detail}; retrying with {}",
                        forward_targets[index + 1]
                    ),
                );
                close_forwarder(&mut candidate).await;
                tokio::time::sleep(PUBLIC_TUNNEL_RETRY_DELAY).await;
            }
            Ok(Some(detail)) => {
                close_forwarder(&mut candidate).await;
                return Err(StartFailure::Other(format!(
                    "ngrok tunnel is online but cannot reach MoonDesk locally after trying {} and {}: {detail}",
                    forward_targets[0], forward_targets[1]
                )));
            }
            Ok(None) => {
                selected_url = Some(candidate_url);
                selected_target = Some(target.clone());
                selected_forwarder = Some(candidate);
                break;
            }
            Err(error) => {
                state.lock().await.log(
                    "WARN",
                    format!(
                        "Could not self-probe public ngrok URL ({error}); keeping local upstream {target}"
                    ),
                );
                selected_url = Some(candidate_url);
                selected_target = Some(target.clone());
                selected_forwarder = Some(candidate);
                break;
            }
        }
    }

    let mut forwarder = selected_forwarder.ok_or_else(|| {
        StartFailure::Other("Failed to establish a working ngrok forwarder".into())
    })?;
    let url = selected_url
        .ok_or_else(|| StartFailure::Other("ngrok forwarder did not expose a URL".into()))?;
    let forward_target = selected_target
        .ok_or_else(|| StartFailure::Other("ngrok forwarder did not select an upstream".into()))?;

    let mut app = state.lock().await;
    if let Err(error) = ensure_local_server_running(app.server_running) {
        drop(app);
        close_forwarder(&mut forwarder).await;
        return Err(error);
    }

    let state_clone = state.clone();
    let watcher = tokio::spawn(async move {
        let result = forwarder.join().await;
        let mut app = state_clone.lock().await;
        match result {
            Ok(Ok(())) => app.log("WARN", "ngrok tunnel exited".into()),
            Ok(Err(error)) => app.log("ERROR", format!("ngrok tunnel failed: {error}")),
            Err(error) if error.is_cancelled() => return,
            Err(error) => app.log("ERROR", format!("ngrok tunnel join failed: {error}")),
        }
        app.ngrok_running = false;
        app.ngrok_url = None;
        app.clear_remote_connection_state();
    });

    app.ngrok_task = Some(watcher);
    app.ngrok_running = true;
    app.ngrok_url = Some(url.clone());
    let workspace_count = app.workspaces.len();
    app.log("INFO", "ngrok SDK tunnel started".into());
    app.log("INFO", format!("ngrok local upstream: {forward_target}"));
    app.log("INFO", format!("ngrok URL: {url}"));
    app.log(
        "INFO",
        format!("Workspace MCP endpoints ready: {workspace_count}"),
    );

    if app.ngrok_domain.is_none()
        && let Ok(parsed_url) = reqwest::Url::parse(&url)
        && let Some(host) = parsed_url.host_str()
    {
        app.ngrok_domain = Some(host.to_string());
        app.log("INFO", format!("Auto-saved ngrok static domain: {host}"));
        app.mark_config_dirty();
    }

    Ok(())
}

/// Stop the active tunnel and clear all connection state.
pub async fn stop(state: SharedState) {
    let task = {
        let mut app = state.lock().await;
        let task = app.ngrok_task.take();
        app.ngrok_running = false;
        app.ngrok_url = None;
        app.clear_remote_connection_state();
        task
    };

    if let Some(task) = task {
        task.abort();
        let _ = task.await;
    }
}

/// Rebuild the tunnel from the current in-memory configuration.
pub async fn restart(state: SharedState) -> Result<(), StartFailure> {
    stop(state.clone()).await;
    start(state).await
}

#[cfg(test)]
mod tests {
    use super::{
        StartFailure, ensure_local_server_running, local_forward_targets, public_tunnel_bad_gateway,
    };
    use axum::{Router, http::StatusCode, routing::get};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[test]
    fn authentication_failures_are_distinguished_for_token_recovery() {
        let authentication = StartFailure::Authentication("authentication failure".into());
        let other = StartFailure::Other("network failure".into());

        assert!(authentication.is_authentication());
        assert!(!other.is_authentication());
        assert_eq!(authentication.to_string(), "authentication failure");
        assert_eq!(other.to_string(), "network failure");
    }

    #[test]
    fn local_forward_targets_try_numeric_loopback_before_localhost_fallback() {
        assert_eq!(
            local_forward_targets(3200),
            [
                "http://127.0.0.1:3200".to_string(),
                "http://localhost:3200".to_string(),
            ]
        );
    }

    #[tokio::test]
    async fn public_probe_detects_repeated_ngrok_bad_gateway() {
        let app = Router::new().route(
            "/",
            get(|| async {
                (
                    StatusCode::BAD_GATEWAY,
                    "failed to dial backend: connection refused",
                )
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind probe server");
        let address = listener.local_addr().expect("probe server address");
        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let detail = public_tunnel_bad_gateway(&format!("http://{address}"))
            .await
            .expect("probe should complete")
            .expect("502 should be detected");
        assert!(detail.contains("502 Bad Gateway"));
        assert!(detail.contains("connection refused"));

        server.abort();
        let _ = server.await;
    }

    #[test]
    fn tunnel_publication_requires_live_local_server() {
        assert!(ensure_local_server_running(true).is_ok());
        let error =
            ensure_local_server_running(false).expect_err("dead server must block publication");
        assert!(
            error
                .to_string()
                .contains("Local MCP server exited before the ngrok tunnel could be published")
        );
    }

    #[tokio::test]
    async fn public_probe_keeps_502_fallback_when_diagnostic_body_is_truncated() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind truncated-502 server");
        let address = listener.local_addr().expect("truncated-502 server address");
        let server = tokio::spawn(async move {
            for _ in 0..super::PUBLIC_TUNNEL_BAD_GATEWAY_RETRIES {
                let (mut stream, _) = listener.accept().await.expect("accept probe request");
                let mut request = [0_u8; 2048];
                let _ = stream.read(&mut request).await;
                stream
                    .write_all(
                        b"HTTP/1.1 502 Bad Gateway\r\nContent-Length: 128\r\nConnection: close\r\n\r\nshort",
                    )
                    .await
                    .expect("write truncated 502 response");
                let _ = stream.shutdown().await;
            }
        });

        let detail = public_tunnel_bad_gateway(&format!("http://{address}"))
            .await
            .expect("truncated 502 body must not turn into a generic probe error")
            .expect("502 must still be detected");
        assert_eq!(detail, "ngrok returned HTTP 502 Bad Gateway");

        server.await.expect("truncated-502 server task");
    }

    #[tokio::test]
    async fn public_probe_accepts_non_bad_gateway_response() {
        let app = Router::new().route("/", get(|| async { "ok" }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind probe server");
        let address = listener.local_addr().expect("probe server address");
        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let result = public_tunnel_bad_gateway(&format!("http://{address}"))
            .await
            .expect("probe should complete");
        assert!(result.is_none());

        server.abort();
        let _ = server.await;
    }
}
