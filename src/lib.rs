//! open-rust-chatbot: a self-hosted AI chat widget server.
#![forbid(unsafe_code)]

pub mod chat;
pub mod config;
pub mod kb;
pub mod llm;
pub mod log;
pub mod rate_limit;
pub mod tools;
pub mod webhook;

use axum::extract::ws::{Message as WsMessage, WebSocket, WebSocketUpgrade};
use axum::extract::{ConnectInfo, Query, Request, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use chat::ChatSession;
use config::Config;
use futures_util::{SinkExt, StreamExt};
use rate_limit::{MessageResult, RateLimiter};
use serde_json::json;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tower_http::compression::CompressionLayer;

pub fn now_iso() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

const WIDGET_JS: &str = include_str!("../static/widget.js");
const WIDGET_CSS: &str = include_str!("../static/widget.css");
const DEMO_HTML: &str = include_str!("../static/demo.html");
const MAX_WS_FRAME_BYTES: usize = 16 * 1024;
// tungstenite preallocates 128 KiB per socket by default; chat frames are tiny.
const WS_BUFFER_BYTES: usize = 4 * 1024;

/// Read-only state shared by every connection.
pub struct Shared {
    pub config: Config,
    pub kb: kb::KnowledgeBase,
    pub llm: llm::Llm,
    pub http: reqwest::Client,
    pub tool_definitions: Vec<tools::ToolDefinition>,
    rate_limiter: RateLimiter,
    open_connections: AtomicUsize,
}

pub fn build_state(config: Config) -> std::io::Result<Arc<Shared>> {
    let kb = match &config.knowledge_base_path {
        Some(path) => kb::KnowledgeBase::load(std::path::Path::new(path))?,
        None => kb::KnowledgeBase::new(Vec::new()),
    };
    let http = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .user_agent(concat!("open-rust-chatbot/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(std::io::Error::other)?;
    let limits = &config.limits;
    Ok(Arc::new(Shared {
        rate_limiter: RateLimiter::new(
            limits.max_messages_per_window,
            limits.rate_window.as_millis() as u64,
            limits.max_connections_per_ip,
        ),
        llm: llm::Llm::new(config.llm.clone(), http.clone()),
        tool_definitions: tools::definitions(),
        open_connections: AtomicUsize::new(0),
        kb,
        http,
        config,
    }))
}

pub fn app(state: Arc<Shared>) -> Router {
    let assets = Router::new()
        .route(
            "/widget.js",
            get(|| async { asset("text/javascript; charset=utf-8", WIDGET_JS) }),
        )
        .route(
            "/widget.css",
            get(|| async { asset("text/css; charset=utf-8", WIDGET_CSS) }),
        )
        .layer(CompressionLayer::new());
    let mut router = Router::new()
        .route("/health", get(|| async { Json(json!({ "ok": true })) }))
        .route("/ws", get(ws_upgrade))
        .merge(assets);
    if !state.config.production {
        router = router.route("/", get(|| async { Html(DEMO_HTML) }));
    }
    router
        .layer(middleware::from_fn(security_headers))
        .with_state(state)
}

fn asset(content_type: &'static str, body: &'static str) -> Response {
    (
        [
            (header::CONTENT_TYPE, content_type),
            (header::CACHE_CONTROL, "public, max-age=3600"),
            // Lets sites that set Cross-Origin-Embedder-Policy still load the widget.
            (
                header::HeaderName::from_static("cross-origin-resource-policy"),
                "cross-origin",
            ),
        ],
        body,
    )
        .into_response()
}

async fn security_headers(req: Request, next: Next) -> Response {
    let mut res = next.run(req).await;
    let h = res.headers_mut();
    h.insert(
        "x-content-type-options",
        HeaderValue::from_static("nosniff"),
    );
    h.insert("referrer-policy", HeaderValue::from_static("no-referrer"));
    h.insert("x-frame-options", HeaderValue::from_static("DENY"));
    h.insert(
        "content-security-policy",
        HeaderValue::from_static("default-src 'self'; connect-src 'self' ws: wss:; img-src 'self' data:; style-src 'self'; script-src 'self'; frame-ancestors 'none'; base-uri 'none'; form-action 'none'"),
    );
    res
}

fn client_ip(config: &Config, headers: &HeaderMap, peer: SocketAddr) -> String {
    if config.trust_proxy_headers {
        let header = |name: &str| headers.get(name).and_then(|v| v.to_str().ok());
        // The right-most X-Forwarded-For entry is the one our own proxy appended.
        if let Some(ip) = header("x-forwarded-for")
            .and_then(|xff| xff.split(',').map(str::trim).rfind(|s| !s.is_empty()))
        {
            return ip.to_owned();
        }
        if let Some(ip) = header("cf-connecting-ip") {
            return ip.trim().to_owned();
        }
    }
    peer.ip().to_string()
}

async fn verify_turnstile(http: &reqwest::Client, token: &str, secret: &str, ip: &str) -> bool {
    let res = http
        .post("https://challenges.cloudflare.com/turnstile/v0/siteverify")
        .timeout(Duration::from_secs(10))
        .form(&[("secret", secret), ("response", token), ("remoteip", ip)])
        .send()
        .await;
    match res {
        Ok(r) if r.status().is_success() => r
            .json::<serde_json::Value>()
            .await
            .is_ok_and(|v| v["success"] == true),
        _ => false,
    }
}

/// Releases the per-IP and global connection slots however the socket ends.
struct ConnectionSlot {
    shared: Arc<Shared>,
    ip: String,
}

impl Drop for ConnectionSlot {
    fn drop(&mut self) {
        self.shared.rate_limiter.release_connection(&self.ip);
        self.shared.open_connections.fetch_sub(1, Ordering::SeqCst);
    }
}

fn reject(status: StatusCode, message: &'static str) -> Response {
    (status, message).into_response()
}

async fn ws_upgrade(
    State(shared): State<Arc<Shared>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    Query(query): Query<HashMap<String, String>>,
    headers: HeaderMap,
    ws: Result<WebSocketUpgrade, axum::extract::ws::rejection::WebSocketUpgradeRejection>,
) -> Response {
    let origin = headers
        .get(header::ORIGIN)
        .and_then(|v| v.to_str().ok())
        .map(|o| o.trim_end_matches('/'));
    if !origin.is_some_and(|o| shared.config.allowed_origins.iter().any(|a| a == o)) {
        return reject(StatusCode::FORBIDDEN, "origin not allowed");
    }
    let ws = match ws {
        Ok(ws) => ws,
        Err(rejection) => return rejection.into_response(),
    };
    let ip = client_ip(&shared.config, &headers, peer);
    if let Some(secret) = &shared.config.turnstile_secret {
        let Some(token) = query.get("ts").filter(|t| !t.is_empty() && t.len() < 4096) else {
            return reject(StatusCode::FORBIDDEN, "verification required");
        };
        if !verify_turnstile(&shared.http, token, secret, &ip).await {
            return reject(StatusCode::FORBIDDEN, "verification failed");
        }
    }
    if shared.open_connections.fetch_add(1, Ordering::SeqCst)
        >= shared.config.limits.max_connections_total
    {
        shared.open_connections.fetch_sub(1, Ordering::SeqCst);
        return reject(StatusCode::SERVICE_UNAVAILABLE, "server busy");
    }
    if !shared.rate_limiter.acquire_connection(&ip) {
        shared.open_connections.fetch_sub(1, Ordering::SeqCst);
        return reject(StatusCode::TOO_MANY_REQUESTS, "too many connections");
    }
    let slot = ConnectionSlot {
        shared: shared.clone(),
        ip,
    };
    let session_id = uuid::Uuid::new_v4().to_string();
    ws.max_message_size(MAX_WS_FRAME_BYTES)
        .max_frame_size(MAX_WS_FRAME_BYTES)
        .read_buffer_size(WS_BUFFER_BYTES)
        .write_buffer_size(WS_BUFFER_BYTES)
        // If the upgrade fails the closure (and the slot inside it) is dropped, releasing it.
        .on_upgrade(move |socket| run_socket(socket, slot, session_id))
}

async fn run_socket(socket: WebSocket, slot: ConnectionSlot, session_id: String) {
    let shared = slot.shared.clone();
    let (mut sink, mut inbound) = socket.split();
    let (out_tx, mut out_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    let (msg_tx, mut msg_rx) = tokio::sync::mpsc::channel::<String>(8);

    let writer = tokio::spawn(async move {
        while let Some(frame) = out_rx.recv().await {
            if sink.send(WsMessage::Text(frame.into())).await.is_err() {
                return;
            }
        }
        let _ = sink.close().await;
    });

    // One worker per socket keeps a visitor's messages strictly in order.
    let worker_tx = out_tx.clone();
    let mut session = ChatSession::new(shared.clone(), session_id);
    let worker = tokio::spawn(async move {
        while let Some(text) = msg_rx.recv().await {
            session.handle_message(&text, &worker_tx).await;
        }
    });

    let idle = shared.config.limits.idle_timeout;
    loop {
        let msg = match tokio::time::timeout(idle, inbound.next()).await {
            Ok(Some(Ok(msg))) => msg,
            _ => break, // idle timeout, protocol error, or client gone
        };
        let text = match msg {
            WsMessage::Text(t) => t.to_string(),
            WsMessage::Close(_) => break,
            WsMessage::Binary(_) => break, // the widget never sends binary
            _ => continue,
        };
        if let MessageResult::RateLimited { retry_after_ms } =
            shared.rate_limiter.record_message(&slot.ip, now_ms())
        {
            let message = format!(
                "You're sending messages too quickly. Try again in {}s.",
                retry_after_ms.div_ceil(1000)
            );
            let _ = out_tx.send(json!({ "type": "error", "message": message }).to_string());
            continue;
        }
        // A full queue means the visitor is flooding while a reply is in progress.
        if msg_tx.try_send(text).is_err() {
            let _ = out_tx.send(
                json!({ "type": "error", "message": "Please wait for the current reply." })
                    .to_string(),
            );
        }
    }

    // Stop model work for a visitor who has gone, then let the writer flush and close.
    worker.abort();
    drop(out_tx);
    let _ = tokio::time::timeout(Duration::from_secs(2), writer).await;
    drop(slot);
}
