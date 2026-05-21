//! `/healthz` (liveness, always 200) and `/readyz` (gated on the
//! `ready` flag). Hand-rolled to avoid pulling in axum for two routes.

use std::{
    net::SocketAddr,
    sync::{atomic::AtomicBool, Arc},
};

use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
};

pub fn spawn(
    bind: SocketAddr,
    ready: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let listener = match TcpListener::bind(bind).await {
            Ok(l) => l,
            Err(e) => {
                tracing::error!(addr = %bind, error = %e, "health server bind failed");
                return;
            }
        };
        tracing::info!(addr = %bind, "health server listening");

        loop {
            // 250ms accept ceiling so SIGTERM isn't stuck behind an
            // idle keep-alive.
            let accept_or_stop = tokio::select! {
                res = listener.accept() => Some(res),
                _ = tokio::time::sleep(std::time::Duration::from_millis(250)) => None,
            };
            if stop.load(std::sync::atomic::Ordering::Relaxed) {
                tracing::info!("health server stopping");
                return;
            }
            let Some(accept_res) = accept_or_stop else {
                continue;
            };
            let (sock, _peer) = match accept_res {
                Ok(p) => p,
                Err(e) => {
                    tracing::warn!(error = %e, "health server accept failed");
                    continue;
                }
            };
            let ready = ready.clone();
            tokio::spawn(async move {
                if let Err(e) = handle_conn(sock, ready).await {
                    tracing::debug!(error = %e, "health server conn ended with error");
                }
            });
        }
    })
}

async fn handle_conn(mut sock: TcpStream, ready: Arc<AtomicBool>) -> std::io::Result<()> {
    let mut buf = [0u8; 1024];
    let n = sock.read(&mut buf).await?;
    if n == 0 {
        return Ok(());
    }
    let request = std::str::from_utf8(&buf[..n]).unwrap_or("");
    let first_line = request.lines().next().unwrap_or("");
    let mut parts = first_line.split_whitespace();
    let _method = parts.next().unwrap_or("");
    let path = parts.next().unwrap_or("");

    let response: &[u8] = match path {
        "/healthz" => b"HTTP/1.1 200 OK\r\nContent-Length: 3\r\nContent-Type: text/plain\r\nConnection: close\r\n\r\nok\n",
        "/readyz" => {
            if ready.load(std::sync::atomic::Ordering::Relaxed) {
                b"HTTP/1.1 200 OK\r\nContent-Length: 6\r\nContent-Type: text/plain\r\nConnection: close\r\n\r\nready\n"
            } else {
                b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 12\r\nContent-Type: text/plain\r\nConnection: close\r\n\r\nnot ready\n"
            }
        }
        _ => b"HTTP/1.1 404 Not Found\r\nContent-Length: 10\r\nContent-Type: text/plain\r\nConnection: close\r\n\r\nnot found\n",
    };
    sock.write_all(response).await?;
    sock.shutdown().await?;
    Ok(())
}
