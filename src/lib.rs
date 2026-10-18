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
// Every rejected frame gets an error reply (so the widget never waits forever), and a socket
// that keeps sending rejected frames is closed, which bounds the replies queued for it.
const MAX_REJECTED_FRAMES: usize = 20;
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
    model_health: std::sync::Mutex<ModelHealth>,
}

/// Circuit breaker for the model: the widget hides its button while the chat can't answer.
#[derive(Default)]
struct ModelHealth {
    consecutive_failures: u32,
    last_failure: Option<std::time::Instant>,
}

const MODEL_FAILURES_BEFORE_DOWN: u32 = 3;

impl Shared {
    pub fn record_model_result(&self, ok: bool) {
        let mut h = self.model_health.lock().unwrap();
        if ok {
            *h = ModelHealth::default();
        } else {
            h.consecutive_failures += 1;
            h.last_failure = Some(std::time::Instant::now());
        }
    }

    /// Down after repeated failures; after the cooldown it reports up again so one real
    /// request can test the model (a success resets it, a failure trips it again).
    pub fn chat_available(&self) -> bool {
        let h = self.model_health.lock().unwrap();
        h.consecutive_failures < MODEL_FAILURES_BEFORE_DOWN
            || h.last_failure
                .is_none_or(|t| t.elapsed() >= self.config.limits.model_failure_cooldown)
    }
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
        tool_definitions: tools::definitions()
            .into_iter()
            .filter(|t| config.lead_capture || t.name != "propose_lead")
            .collect(),
        open_connections: AtomicUsize::new(0),
        model_health: Default::default(),
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
        .route("/health", get(health))
        .route("/ws", get(ws_upgrade))
        .merge(assets);
    if !state.config.production {
        router = router.route("/", get(|| async { Html(DEMO_HTML) }));
    }
    router
        .layer(middleware::from_fn(security_headers))
        .with_state(state)
}

/// Readable from any site (the widget checks it before showing its button) and never cached.
async fn health(State(shared): State<Arc<Shared>>) -> Response {
    let ok = shared.chat_available();
    let status = if ok {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    (
        status,
        [
            (header::ACCESS_CONTROL_ALLOW_ORIGIN, "*"),
            (header::CACHE_CONTROL, "no-store"),
        ],
        Json(json!({ "ok": ok })),
    )
        .into_response()
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

/// The rate-limit key for a visitor. Only the one header named by CLIENT_IP_HEADER is trusted,
/// and only when TRUST_PROXY_HEADERS=true. IPv6 clients are grouped by /64, since one
/// connection usually controls a whole /64.
pub fn client_ip(config: &Config, headers: &HeaderMap, peer: SocketAddr) -> String {
    let mut ip = peer.ip();
    if config.trust_proxy_headers {
        let value = headers
            .get(config.client_ip_header.as_str())
            .and_then(|v| v.to_str().ok());
        // For X-Forwarded-For the right-most entry is the one our own proxy appended.
        let candidate = value.and_then(|v| v.split(',').map(str::trim).rfind(|s| !s.is_empty()));
        if let Some(parsed) = candidate.and_then(|c| c.parse::<std::net::IpAddr>().ok()) {
            ip = parsed;
        }
    }
    match ip {
        std::net::IpAddr::V6(v6) if v6.to_ipv4_mapped().is_none() => {
            let s = v6.segments();
            format!("{:x}:{:x}:{:x}:{:x}::/64", s[0], s[1], s[2], s[3])
        }
        std::net::IpAddr::V6(v6) => v6.to_ipv4_mapped().unwrap().to_string(),
        v4 => v4.to_string(),
    }
}

async fn verify_turnstile(shared: &Shared, token: &str, secret: &str, ip: &str) -> bool {
    let res = shared
        .http
        .post("https://challenges.cloudflare.com/turnstile/v0/siteverify")
        .timeout(Duration::from_secs(10))
        .form(&[("secret", secret), ("response", token), ("remoteip", ip)])
        .send()
        .await;
    match res {
        // Also require the token to have been issued on one of our own sites.
        Ok(r) if r.status().is_success() => r.json::<serde_json::Value>().await.is_ok_and(|v| {
            v["success"] == true
                && v["hostname"].as_str().is_some_and(|host| {
                    shared.config.allowed_origins.iter().any(|origin| {
                        origin
                            .split("://")
                            .nth(1)
                            .is_some_and(|h| h.split(':').next() == Some(host))
                    })
                })
        }),
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
    // Claim connection slots before any outbound verification call, so unauthenticated
    // upgrade floods are capped by the same limits as real chats.
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
    if let Some(secret) = &shared.config.turnstile_secret {
        let Some(token) = query.get("ts").filter(|t| !t.is_empty() && t.len() < 4096) else {
            return reject(StatusCode::FORBIDDEN, "verification required");
        };
        if !verify_turnstile(&shared, token, secret, &slot.ip).await {
            return reject(StatusCode::FORBIDDEN, "verification failed");
        }
    }
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

    let limits = &shared.config.limits;
    let session_deadline = tokio::time::Instant::now() + limits.max_session;
    let mut idle_deadline = tokio::time::Instant::now() + limits.idle_timeout;
    // Rejected frames (rate-limited or sent mid-reply) get a few error replies, then the socket
    // is closed, so a client that floods without reading cannot grow the outgoing queue.
    let mut rejected = 0usize;
    loop {
        let deadline = idle_deadline.min(session_deadline);
        let msg = match tokio::time::timeout_at(deadline, inbound.next()).await {
            Ok(Some(Ok(msg))) => msg,
            _ => break, // idle or lifetime limit, protocol error, or client gone
        };
        let text = match msg {
            WsMessage::Text(t) => t.to_string(),
            WsMessage::Close(_) => break,
            WsMessage::Binary(_) => break, // the widget never sends binary
            _ => continue,                 // pings do not count as activity
        };
        idle_deadline = tokio::time::Instant::now() + limits.idle_timeout;
        let error = match shared.rate_limiter.record_message(&slot.ip, now_ms()) {
            MessageResult::RateLimited { retry_after_ms } => Some(format!(
                "You're sending messages too quickly. Please try again in {}.",
                human_wait(retry_after_ms)
            )),
            MessageResult::Ok => match msg_tx.try_send(text) {
                Ok(()) => None,
                Err(_) => Some("Please wait for the current reply.".to_owned()),
            },
        };
        if let Some(message) = error {
            rejected += 1;
            if rejected > MAX_REJECTED_FRAMES {
                break;
            }
            let _ = out_tx.send(json!({ "type": "error", "message": message }).to_string());
        }
    }

    // Stop model work for a visitor who has gone, then let the writer flush and close.
    worker.abort();
    drop(out_tx);
    let _ = tokio::time::timeout(Duration::from_secs(2), writer).await;
    drop(slot);
}

/// "about 4 minutes" rather than "249s".
pub fn human_wait(ms: u64) -> String {
    let secs = ms.div_ceil(1000);
    if secs < 60 {
        format!("{secs} seconds")
    } else {
        let mins = secs.div_ceil(60);
        format!("about {mins} minute{}", if mins == 1 { "" } else { "s" })
    }
}
