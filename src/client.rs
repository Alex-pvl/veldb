//! Минимальный HTTP-клиент поверх `hyper`, который уже есть в дереве из-за axum.
//!
//! ponytail: без `reqwest`. Нужны ровно два глагола к своему же серверу;
//! отдельный HTTP-клиент со своим TLS-стеком и пулом соединений здесь не окупается.
//! Соединение на запрос — осознанно: и репликация, и CLI ходят редко.

use anyhow::{bail, Context, Result};
use http_body_util::BodyExt;
use hyper::Request;

/// Приводит `http://host:port/` и `host:port` к виду `host:port`.
pub fn normalize(url: &str) -> String {
    url.trim()
        .trim_start_matches("http://")
        .trim_start_matches("https://")
        .trim_end_matches('/')
        .to_string()
}

async fn send(host: &str, req: Request<String>) -> Result<Vec<u8>> {
    let stream = tokio::net::TcpStream::connect(host)
        .await
        .with_context(|| format!("подключение к {host}"))?;
    stream.set_nodelay(true).ok();
    let io = hyper_util::rt::TokioIo::new(stream);
    let (mut sender, conn) = hyper::client::conn::http1::handshake(io).await?;
    tokio::spawn(async move {
        let _ = conn.await;
    });

    let uri = req.uri().to_string();
    let resp = sender.send_request(req).await?;
    let status = resp.status();
    let body = resp.into_body().collect().await?.to_bytes().to_vec();
    if !status.is_success() {
        // Тело ошибки от veldb — это JSON с полем `error`; показываем именно его,
        // а не «HTTP 400».
        let msg = serde_json::from_slice::<serde_json::Value>(&body)
            .ok()
            .and_then(|v| v["error"].as_str().map(String::from))
            .unwrap_or_else(|| String::from_utf8_lossy(&body).into_owned());
        bail!("{host}{uri}: {msg}");
    }
    Ok(body)
}

pub async fn get(host: &str, path: &str) -> Result<Vec<u8>> {
    let req = Request::builder()
        .uri(path)
        .header("host", host)
        .body(String::new())?;
    send(host, req).await
}

pub async fn post_json(host: &str, path: &str, body: &serde_json::Value) -> Result<Vec<u8>> {
    let req = Request::builder()
        .method("POST")
        .uri(path)
        .header("host", host)
        .header("content-type", "application/json")
        .body(body.to_string())?;
    send(host, req).await
}
