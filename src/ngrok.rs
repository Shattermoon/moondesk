use crate::state::SharedState;
use ngrok::prelude::*;
use reqwest::Url;

/// Start an ngrok HTTP tunnel using the embedded Rust SDK.
pub async fn start(state: SharedState) -> Result<(), String> {
    let (port, authtoken, configured_domain) = {
        let app = state.lock().await;
        if app.ngrok_running {
            return Err("ngrok is already running".into());
        }
        let authtoken = app
            .ngrok_authtoken()
            .map(str::to_string)
            .ok_or_else(|| "ngrok authtoken is not configured".to_string())?;
        (app.port, authtoken, app.ngrok_domain.clone())
    };

    let forwards_to: Url = format!("http://127.0.0.1:{port}")
        .parse()
        .map_err(|error| format!("Invalid forward URL: {error}"))?;

    let session = ngrok::Session::builder()
        .authtoken(authtoken)
        .connect()
        .await
        .map_err(|error| format!("Failed to connect ngrok session: {error}"))?;

    let mut http_endpoint = session.http_endpoint();
    if let Some(domain) = configured_domain.as_deref()
        && !domain.is_empty()
    {
        http_endpoint.domain(domain);
    }

    let mut forwarder = http_endpoint
        .listen_and_forward(forwards_to)
        .await
        .map_err(|error| format!("Failed to open ngrok tunnel: {error}"))?;
    let url = forwarder.url().to_string();

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

    {
        let mut app = state.lock().await;
        app.ngrok_task = Some(watcher);
        app.ngrok_running = true;
        app.ngrok_url = Some(url.clone());
        let workspace_count = app.workspaces.len();
        app.log("INFO", "ngrok SDK tunnel started".into());
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
pub async fn restart(state: SharedState) -> Result<(), String> {
    stop(state.clone()).await;
    start(state).await
}
