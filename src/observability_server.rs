use crate::runtime_metrics::RuntimeMetrics;
use anyhow::{Context, Result};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tracing::{info, warn};

#[derive(Default)]
pub struct ServiceHealth {
    initialized: AtomicBool,
    connected: AtomicBool,
    journal_writable: AtomicBool,
    disk_healthy: AtomicBool,
    no_unresolved_orders: AtomicBool,
    safety_ok: AtomicBool,
}

impl ServiceHealth {
    pub fn mark_initialized(&self) {
        self.initialized.store(true, Ordering::Release);
    }

    pub fn set_connected(&self, connected: bool) {
        self.connected.store(connected, Ordering::Release);
    }

    pub fn set_journal_writable(&self, healthy: bool) {
        self.journal_writable.store(healthy, Ordering::Release);
    }

    pub fn set_disk_healthy(&self, healthy: bool) {
        self.disk_healthy.store(healthy, Ordering::Release);
    }

    pub fn set_no_unresolved_orders(&self, healthy: bool) {
        self.no_unresolved_orders.store(healthy, Ordering::Release);
    }

    pub fn set_safety_ok(&self, healthy: bool) {
        self.safety_ok.store(healthy, Ordering::Release);
    }

    pub fn ready(&self) -> bool {
        self.initialized.load(Ordering::Acquire)
            && self.connected.load(Ordering::Acquire)
            && self.journal_writable.load(Ordering::Acquire)
            && self.disk_healthy.load(Ordering::Acquire)
            && self.no_unresolved_orders.load(Ordering::Acquire)
            && self.safety_ok.load(Ordering::Acquire)
    }
}

pub async fn serve(
    bind: &str,
    health: Arc<ServiceHealth>,
    metrics: Arc<RuntimeMetrics>,
    shutdown: Arc<AtomicBool>,
) -> Result<()> {
    let listener = TcpListener::bind(bind)
        .await
        .with_context(|| format!("failed to bind observability server at {bind}"))?;
    info!(bind, "Observability server listening");
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let (stream, _) = accepted?;
                if let Err(error) = handle_connection(stream, &health, &metrics).await {
                    warn!(%error, "Observability request failed");
                }
            }
            _ = tokio::time::sleep(std::time::Duration::from_millis(250)) => {
                if shutdown.load(Ordering::Acquire) {
                    return Ok(());
                }
            }
        }
    }
}

async fn handle_connection(
    mut stream: TcpStream,
    health: &ServiceHealth,
    metrics: &RuntimeMetrics,
) -> Result<()> {
    let mut buffer = [0_u8; 4096];
    let count = stream.read(&mut buffer).await?;
    let request = String::from_utf8_lossy(&buffer[..count]);
    let path = request
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .unwrap_or("/");
    let (status, content_type, body) = response_for(path, health, metrics);
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(response.as_bytes()).await?;
    Ok(())
}

fn response_for(
    path: &str,
    health: &ServiceHealth,
    metrics: &RuntimeMetrics,
) -> (&'static str, &'static str, String) {
    match path {
        "/livez" => ("200 OK", "application/json", "{\"status\":\"live\"}".into()),
        "/healthz" | "/readyz" if health.ready() => (
            "200 OK",
            "application/json",
            "{\"status\":\"ready\"}".into(),
        ),
        "/healthz" | "/readyz" => (
            "503 Service Unavailable",
            "application/json",
            "{\"status\":\"not_ready\"}".into(),
        ),
        "/metrics" => ("200 OK", "text/plain; version=0.0.4", metrics.prometheus()),
        _ => ("404 Not Found", "text/plain", "not found\n".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn health_and_metrics_responses_reflect_runtime_state() {
        let health = ServiceHealth::default();
        let metrics = RuntimeMetrics::default();
        let (status, _, _) = response_for("/healthz", &health, &metrics);
        assert_eq!(status, "503 Service Unavailable");
        health.mark_initialized();
        health.set_connected(true);
        health.set_journal_writable(true);
        health.set_disk_healthy(true);
        health.set_no_unresolved_orders(true);
        health.set_safety_ok(true);
        let (status, _, body) = response_for("/healthz", &health, &metrics);
        assert_eq!(status, "200 OK");
        assert!(body.contains("ready"));
        assert_eq!(response_for("/livez", &health, &metrics).0, "200 OK");
        health.set_safety_ok(false);
        assert_eq!(
            response_for("/readyz", &health, &metrics).0,
            "503 Service Unavailable"
        );
        health.set_safety_ok(true);
        metrics.record_trade_started();
        let (status, content_type, body) = response_for("/metrics", &health, &metrics);
        assert_eq!(status, "200 OK");
        assert_eq!(content_type, "text/plain; version=0.0.4");
        assert!(body.contains("polypulse_trades_started_total 1"));
    }
}
