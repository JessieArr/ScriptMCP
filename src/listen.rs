use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::{Context, Result};
use rmcp::transport::stdio;
use rmcp::transport::streamable_http_server::{
    session::local::LocalSessionManager, StreamableHttpServerConfig, StreamableHttpService,
};
use rmcp::ServiceExt;
use tracing::info;

use crate::config::Opts;
use crate::mcp_config::http_url;
use crate::server::ScriptMcp;

pub struct HttpReady {
    pub addr: SocketAddr,
    pub server: ScriptMcp,
}

pub async fn serve_stdio(opts: &Opts) -> Result<()> {
    let server = ScriptMcp::start(opts).await?;
    info!("starting MCP stdio transport");
    let service = server.serve(stdio()).await?;
    service.waiting().await?;
    Ok(())
}

pub async fn serve_http(
    opts: &Opts,
    mut ready: Option<tokio::sync::oneshot::Sender<Result<HttpReady, String>>>,
) -> Result<()> {
    let notify = |ready: &mut Option<tokio::sync::oneshot::Sender<Result<HttpReady, String>>>,
                  value: Result<HttpReady, String>| {
        if let Some(tx) = ready.take() {
            let _ = tx.send(value);
        }
    };

    let bind = match opts.socket_addr() {
        Ok(bind) => bind,
        Err(error) => {
            notify(&mut ready, Err(error.to_string()));
            return Err(error);
        }
    };
    let server = match ScriptMcp::start(opts).await {
        Ok(server) => server,
        Err(error) => {
            notify(&mut ready, Err(error.to_string()));
            return Err(error);
        }
    };
    let http_server = server.clone();
    let service = StreamableHttpService::new(
        move || Ok(server.clone()),
        Arc::new(LocalSessionManager::default()),
        StreamableHttpServerConfig::default(),
    );
    let router = axum::Router::new().nest_service("/mcp", service);
    let listener = match tokio::net::TcpListener::bind(bind).await {
        Ok(listener) => listener,
        Err(error) => {
            let message = format!("failed to bind MCP HTTP server at {bind}: {error}");
            notify(&mut ready, Err(message.clone()));
            anyhow::bail!(message);
        }
    };
    let actual = listener.local_addr().unwrap_or(bind);
    notify(
        &mut ready,
        Ok(HttpReady {
            addr: actual,
            server: http_server,
        }),
    );
    info!("MCP Streamable HTTP listening on {}", http_url(actual));
    axum::serve(listener, router)
        .await
        .context("MCP HTTP server stopped")?;
    Ok(())
}

pub fn parse_bind(bind: &str) -> Result<SocketAddr> {
    bind.parse()
        .with_context(|| format!("invalid --bind address: {bind}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_bind_localhost() {
        let addr = parse_bind("127.0.0.1:8788").unwrap();
        assert_eq!(addr.port(), 8788);
        assert!(addr.ip().is_loopback());
    }
}
