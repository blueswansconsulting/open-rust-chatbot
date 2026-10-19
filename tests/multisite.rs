//! Multi-site mode: many bots from one server, settings loaded from a sites API.
//! Each test runs a fake sites API (an axum router) and the chatbot server in-process.

use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use futures_util::{SinkExt, StreamExt};
use open_rust_chatbot::config::Config;
use open_rust_chatbot::{app, build_state};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message;

const TOKEN: &str = "internal-token-0123456789abcdef0123456789"; // gitleaks:allow (test fixture)
const SECRET: &str = "0123456789abcdef0123456789abcdef"; // gitleaks:allow (test fixture)
const ALPHA_ORIGIN: &str = "https://alpha.example";
const BETA_ORIGIN: &str = "https://beta.example";

type Ws =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

#[derive(Default)]
struct FakeSites {
    bots: Mutex<HashMap<String, Value>>,
    fetches: Mutex<HashMap<String, usize>>,
    down: AtomicBool,
    /// Delay after reading the bot, to simulate a slow response carrying old settings.
    delay_ms: AtomicU64,
}

impl FakeSites {
    fn fetches(&self, id: &str) -> usize {
        self.fetches.lock().unwrap().get(id).copied().unwrap_or(0)
    }
    fn set(&self, id: &str, bot: Value) {
        self.bots.lock().unwrap().insert(id.into(), bot);
    }
}

fn bot(id: &str, origin: &str, kb: &str, marker: &str) -> Value {
    json!({
        "id": id,
        "status": "active",
        "name": format!("{marker} Co"),
        "botName": format!("{marker} Bot"),
        "title": format!("{marker} Chat"),
        "subtitle": format!("{marker} subtitle"),
        "greeting": format!("Hello from {marker}"),
        "placeholder": format!("Ask {marker}..."),
        "privacyUrl": null,
        "allowedOrigins": [origin],
        "instructions": format!("You are {{bot_name}}. {marker}-PROMPT. {{lead_instructions}}"),
        "knowledgeBase": kb,
        "leadCapture": true,
        "updatedAt": "2026-10-19T17:00:00.000Z",
    })
}

async fn fake_bot(
    State(sites): State<Arc<FakeSites>>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> axum::response::Response {
    *sites.fetches.lock().unwrap().entry(id.clone()).or_default() += 1;
    if headers.get("authorization").and_then(|v| v.to_str().ok())
        != Some(&format!("Bearer {TOKEN}"))
    {
        return (StatusCode::UNAUTHORIZED, "no").into_response();
    }
    if sites.down.load(Ordering::SeqCst) {
        return (StatusCode::BAD_GATEWAY, "down").into_response();
    }
    let found = sites.bots.lock().unwrap().get(&id).cloned();
    let delay = sites.delay_ms.load(Ordering::SeqCst);
    if delay > 0 {
        tokio::time::sleep(Duration::from_millis(delay)).await;
    }
    match found {
        Some(b) if b["status"] == "active" => axum::Json(b).into_response(),
        _ => (
            StatusCode::NOT_FOUND,
            axum::Json(json!({"error": "not_found"})),
        )
            .into_response(),
    }
}

async fn spawn(router: axum::Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(
            listener,
            router.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await
        .unwrap()
    });
    addr.to_string()
}

struct Harness {
    addr: String,
    sites: Arc<FakeSites>,
}

async fn start(extra: &[(&str, &str)]) -> Harness {
    let sites = Arc::new(FakeSites::default());
    sites.set(
        "alpha",
        bot(
            "alpha",
            ALPHA_ORIGIN,
            "# Opening hours\n\nAlpha opens at 7am every day of the week.",
            "Alpha",
        ),
    );
    sites.set(
        "beta-02",
        bot(
            "beta-02",
            BETA_ORIGIN,
            "# Opening hours\n\nBeta opens at 10am on Sundays only.",
            "Beta",
        ),
    );
    let mut paused = bot("paused", ALPHA_ORIGIN, "# x\n\ny", "Paused");
    paused["status"] = json!("paused");
    sites.set("paused", paused);

    let api = axum::Router::new()
        .route("/internal/bots/{id}", get(fake_bot))
        .with_state(sites.clone());
    let api_addr = spawn(api).await;
    let api_url = format!("http://{api_addr}/internal/bots");
    let mut env = vec![
        ("DEV_MODE", "true"),
        ("MOCK_LLM", "true"),
        ("MOCK_TOKEN_DELAY_MS", "0"),
        ("SITES_API_URL", api_url.as_str()),
        ("INTERNAL_API_TOKEN", TOKEN),
    ];
    env.extend_from_slice(extra);
    let router = app(build_state(Config::from_pairs(&env).unwrap()).unwrap());
    Harness {
        addr: spawn(router).await,
        sites,
    }
}

async fn connect(addr: &str, bot: &str, origin: &str) -> Result<Ws, String> {
    let mut req = format!("ws://{addr}/b/{bot}/ws")
        .into_client_request()
        .unwrap();
    req.headers_mut().insert("origin", origin.parse().unwrap());
    tokio_tungstenite::connect_async(req)
        .await
        .map(|(ws, _)| ws)
        .map_err(|e| e.to_string())
}

async fn until_done(ws: &mut Ws) -> Vec<Value> {
    let mut frames = Vec::new();
    let read = async {
        while let Some(Ok(Message::Text(t))) = ws.next().await {
            let v: Value = serde_json::from_str(&t).unwrap();
            let end = v["type"] == "done" || v["type"] == "error";
            frames.push(v);
            if end {
                break;
            }
        }
    };
    tokio::time::timeout(Duration::from_secs(10), read)
        .await
        .expect("reply within 10s");
    frames
}

fn text_of(frames: &[Value]) -> String {
    frames
        .iter()
        .filter(|f| f["type"] == "token")
        .filter_map(|f| f["value"].as_str())
        .collect()
}

async fn ask(addr: &str, bot: &str, origin: &str, question: &str) -> String {
    let mut ws = connect(addr, bot, origin).await.unwrap();
    ws.send(Message::Text(question.into())).await.unwrap();
    text_of(&until_done(&mut ws).await)
}

async fn status(url: String) -> u16 {
    reqwest::get(url).await.unwrap().status().as_u16()
}

async fn invalidate(addr: &str, id: &str, token: &str) -> u16 {
    reqwest::Client::new()
        .post(format!("http://{addr}/internal/bots/{id}/invalidate"))
        .bearer_auth(token)
        .send()
        .await
        .unwrap()
        .status()
        .as_u16()
}

#[tokio::test]
async fn each_bot_answers_from_its_own_knowledge_base() {
    let h = start(&[]).await;
    let a = ask(
        &h.addr,
        "alpha",
        ALPHA_ORIGIN,
        "What are your opening hours?",
    )
    .await;
    assert!(a.contains("Alpha opens at 7am"), "{a}");
    let b = ask(
        &h.addr,
        "beta-02",
        BETA_ORIGIN,
        "What are your opening hours?",
    )
    .await;
    assert!(b.contains("Beta opens at 10am"), "{b}");
    assert!(!b.contains("Alpha"), "{b}");
}

#[tokio::test]
async fn origin_of_one_bot_is_rejected_on_another() {
    let h = start(&[]).await;
    assert!(connect(&h.addr, "beta-02", ALPHA_ORIGIN)
        .await
        .unwrap_err()
        .contains("403"));
    assert!(connect(&h.addr, "alpha", "https://evil.example")
        .await
        .unwrap_err()
        .contains("403"));
    assert!(connect(&h.addr, "alpha", ALPHA_ORIGIN).await.is_ok());
}

#[tokio::test]
async fn unknown_and_paused_bots_are_404_on_every_route() {
    let h = start(&[]).await;
    for id in ["nobody", "paused"] {
        for path in ["widget.js", "widget.css", "availability", "config", "ws"] {
            assert_eq!(
                status(format!("http://{}/b/{id}/{path}", h.addr)).await,
                404,
                "{id}/{path}"
            );
        }
        assert!(connect(&h.addr, id, ALPHA_ORIGIN)
            .await
            .unwrap_err()
            .contains("404"));
    }
    // A 200 that says "paused" is treated as not found too.
    let mut paused = bot("gamma", ALPHA_ORIGIN, "# x\n\ny", "Gamma");
    paused["status"] = json!("paused");
    h.sites.bots.lock().unwrap().insert("gamma".into(), paused);
    assert_eq!(
        status(format!("http://{}/b/gamma/config", h.addr)).await,
        404
    );
}

#[tokio::test]
async fn bad_ids_are_rejected_without_fetching() {
    let h = start(&[]).await;
    for id in ["a", "UPPER", "has_underscore", &"x".repeat(41), "dot.dot"] {
        assert_eq!(
            status(format!("http://{}/b/{id}/config", h.addr)).await,
            404,
            "{id}"
        );
        assert_eq!(h.sites.fetches(id), 0, "{id} must not be fetched");
    }
}

#[tokio::test]
async fn unknown_ids_are_negatively_cached() {
    let h = start(&[]).await;
    for _ in 0..5 {
        assert_eq!(
            status(format!("http://{}/b/nobody/availability", h.addr)).await,
            404
        );
    }
    assert_eq!(h.sites.fetches("nobody"), 1);
}

#[tokio::test]
async fn config_is_public_cacheable_json() {
    let h = start(&[]).await;
    let res = reqwest::get(format!("http://{}/b/alpha/config", h.addr))
        .await
        .unwrap();
    assert_eq!(res.status(), 200);
    assert_eq!(res.headers()["access-control-allow-origin"], "*");
    assert_eq!(res.headers()["cache-control"], "public, max-age=60");
    let v: Value = res.json().await.unwrap();
    assert_eq!(
        v,
        json!({
            "title": "Alpha Chat",
            "subtitle": "Alpha subtitle",
            "greeting": "Hello from Alpha",
            "placeholder": "Ask Alpha...",
            "privacyUrl": null,
        })
    );
    let avail = reqwest::get(format!("http://{}/b/alpha/availability", h.addr))
        .await
        .unwrap();
    assert_eq!(avail.status(), 200);
    assert_eq!(avail.headers()["access-control-allow-origin"], "*");
    let js = reqwest::get(format!("http://{}/b/alpha/widget.js", h.addr))
        .await
        .unwrap();
    assert_eq!(js.status(), 200);
    assert!(js.text().await.unwrap().contains("openRustChatbot"));
}

#[tokio::test]
async fn cache_is_reused_within_the_ttl_and_refetched_after() {
    let h = start(&[("SITES_CACHE_SECONDS", "1")]).await;
    for _ in 0..3 {
        status(format!("http://{}/b/alpha/config", h.addr)).await;
    }
    assert_eq!(h.sites.fetches("alpha"), 1);
    tokio::time::sleep(Duration::from_millis(1100)).await;
    // Expired: the old copy is served at once while one background fetch refreshes it.
    let mut changed = bot("alpha", ALPHA_ORIGIN, "# x\n\ny", "Renamed");
    changed["updatedAt"] = json!("2026-10-19T18:00:00.000Z");
    h.sites.set("alpha", changed);
    h.sites.delay_ms.store(1500, Ordering::SeqCst);
    let started = std::time::Instant::now();
    for _ in 0..3 {
        let v: Value = reqwest::get(format!("http://{}/b/alpha/config", h.addr))
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(v["title"], "Alpha Chat");
    }
    assert!(
        started.elapsed() < Duration::from_millis(1000),
        "known bots never wait on the sites API"
    );
    tokio::time::sleep(Duration::from_millis(1700)).await;
    assert_eq!(h.sites.fetches("alpha"), 2, "one background refresh");
    let v: Value = reqwest::get(format!("http://{}/b/alpha/config", h.addr))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(v["title"], "Renamed Chat");
}

#[tokio::test]
async fn invalidate_changes_the_answer_immediately_and_needs_the_token() {
    let h = start(&[("SITES_CACHE_SECONDS", "300")]).await;
    let a = ask(
        &h.addr,
        "alpha",
        ALPHA_ORIGIN,
        "What are your opening hours?",
    )
    .await;
    assert!(a.contains("7am"), "{a}");
    let mut changed = bot(
        "alpha",
        ALPHA_ORIGIN,
        "# Opening hours\n\nAlpha now opens at noon.",
        "Alpha",
    );
    changed["updatedAt"] = json!("2026-10-19T18:00:00.000Z");
    h.sites.set("alpha", changed);
    assert_eq!(invalidate(&h.addr, "alpha", "wrong-token").await, 401);
    let still = ask(
        &h.addr,
        "alpha",
        ALPHA_ORIGIN,
        "What are your opening hours?",
    )
    .await;
    assert!(still.contains("7am"), "cached until invalidated: {still}");
    assert_eq!(invalidate(&h.addr, "alpha", TOKEN).await, 204);
    let now = ask(
        &h.addr,
        "alpha",
        ALPHA_ORIGIN,
        "What are your opening hours?",
    )
    .await;
    assert!(now.contains("noon"), "{now}");
    // Bad ids and missing tokens on the internal route.
    assert_eq!(invalidate(&h.addr, "BAD_ID", TOKEN).await, 404);
    assert_eq!(
        reqwest::Client::new()
            .post(format!("http://{}/internal/bots/alpha/invalidate", h.addr))
            .send()
            .await
            .unwrap()
            .status(),
        401
    );
}

#[tokio::test]
async fn pausing_mid_chat_stops_answers() {
    let h = start(&[]).await;
    let mut ws = connect(&h.addr, "alpha", ALPHA_ORIGIN).await.unwrap();
    ws.send(Message::Text("hello".into())).await.unwrap();
    until_done(&mut ws).await;
    h.sites.bots.lock().unwrap().remove("alpha");
    assert_eq!(invalidate(&h.addr, "alpha", TOKEN).await, 204);
    ws.send(Message::Text("What are your opening hours?".into()))
        .await
        .unwrap();
    let frames = until_done(&mut ws).await;
    assert_eq!(frames.last().unwrap()["type"], "error", "{frames:?}");
    assert!(!text_of(&frames).contains("7am"));
}

#[tokio::test]
async fn stale_copy_is_served_while_the_api_is_down_then_503() {
    let h = start(&[("SITES_CACHE_SECONDS", "1"), ("SITES_STALE_SECONDS", "2")]).await;
    assert_eq!(
        status(format!("http://{}/b/alpha/config", h.addr)).await,
        200
    );
    h.sites.down.store(true, Ordering::SeqCst);
    tokio::time::sleep(Duration::from_millis(1100)).await;
    assert_eq!(
        status(format!("http://{}/b/alpha/config", h.addr)).await,
        200,
        "stale copy served"
    );
    let a = ask(
        &h.addr,
        "alpha",
        ALPHA_ORIGIN,
        "What are your opening hours?",
    )
    .await;
    assert!(a.contains("7am"), "{a}");
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert_eq!(
        status(format!("http://{}/b/alpha/config", h.addr)).await,
        503
    );
    assert_eq!(
        status(format!("http://{}/b/alpha/availability", h.addr)).await,
        503
    );
    // Never cached: an id that was never loaded is 503 (not 404) while the API is down.
    assert_eq!(
        status(format!("http://{}/b/beta-02/config", h.addr)).await,
        503
    );
    h.sites.down.store(false, Ordering::SeqCst);
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(
        status(format!("http://{}/b/alpha/config", h.addr)).await,
        200
    );
}

#[tokio::test]
async fn bot_prompt_and_name_are_used_with_a_text_only_model() {
    let prompts: Arc<Mutex<Vec<String>>> = Arc::default();
    let sink = prompts.clone();
    let bridge = axum::Router::new().route(
        "/bridge",
        post(move |axum::Json(body): axum::Json<Value>| {
            let sink = sink.clone();
            async move {
                sink.lock()
                    .unwrap()
                    .push(body["message"].as_str().unwrap_or_default().to_owned());
                axum::Json(json!({ "text": "Sure." }))
            }
        }),
    );
    let bridge_url = format!("http://{}/bridge", spawn(bridge).await);
    let h = start(&[
        ("MOCK_LLM", "false"),
        ("LLM_PROVIDER", "openclaw"),
        ("OPENCLAW_BRIDGE_URL", &bridge_url),
        ("OPENCLAW_BRIDGE_TOKEN", "t"),
    ])
    .await;
    ask(
        &h.addr,
        "beta-02",
        BETA_ORIGIN,
        "When do you open on Sunday?",
    )
    .await;
    let p = prompts.lock().unwrap()[0].clone();
    assert!(p.contains("You are Beta Bot. Beta-PROMPT."), "{p}");
    assert!(p.contains("Beta opens at 10am"), "{p}");
    assert!(!p.contains("Alpha"), "{p}");
    assert!(!p.contains("{lead_instructions}"), "{p}");
}

#[tokio::test]
async fn lead_webhook_url_carries_the_bot_id_as_site() {
    let received: Arc<Mutex<Vec<(String, Value)>>> = Arc::default();
    let sink = received.clone();
    let hook = axum::Router::new().route(
        "/api/leads",
        post(
            move |axum::extract::RawQuery(q): axum::extract::RawQuery, body: String| {
                let sink = sink.clone();
                async move {
                    sink.lock()
                        .unwrap()
                        .push((q.unwrap_or_default(), serde_json::from_str(&body).unwrap()));
                    "ok"
                }
            },
        ),
    );
    let hook_url = format!("http://{}/api/leads?site=default&x=1", spawn(hook).await);
    let h = start(&[
        ("LEAD_WEBHOOK_URL", &hook_url),
        ("LEAD_WEBHOOK_SECRET", SECRET),
    ])
    .await;
    let mut ws = connect(&h.addr, "beta-02", BETA_ORIGIN).await.unwrap();
    ws.send(Message::Text("Please call me, I'm sam@example.com".into()))
        .await
        .unwrap();
    let frames = until_done(&mut ws).await;
    let card = frames.iter().find(|f| f["type"] == "card").unwrap();
    let lead_id = card["card"]["lead"]["leadId"].as_str().unwrap().to_owned();
    ws.send(Message::Text(
        json!({"type": "confirm_lead", "leadId": lead_id})
            .to_string()
            .into(),
    ))
    .await
    .unwrap();
    until_done(&mut ws).await;
    let calls = received.lock().unwrap().clone();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].0, "x=1&site=beta-02");
    assert_eq!(calls[0].1["bot"], "Beta Bot");
}

#[tokio::test]
async fn bot_with_lead_capture_off_never_offers_a_card() {
    let h = start(&[]).await;
    let mut b = bot("quiet", ALPHA_ORIGIN, "# x\n\ny", "Quiet");
    b["leadCapture"] = json!(false);
    h.sites.set("quiet", b);
    let mut ws = connect(&h.addr, "quiet", ALPHA_ORIGIN).await.unwrap();
    ws.send(Message::Text("Please call me, I'm sam@example.com".into()))
        .await
        .unwrap();
    let frames = until_done(&mut ws).await;
    assert!(frames.iter().all(|f| f["type"] != "card"), "{frames:?}");
}

fn resume_frame(token: Option<&str>) -> Message {
    Message::Text(json!({"type": "resume", "token": token}).to_string().into())
}

async fn session_frame(ws: &mut Ws) -> Value {
    loop {
        if let Some(Ok(Message::Text(t))) = ws.next().await {
            let v: Value = serde_json::from_str(&t).unwrap();
            if v["type"] == "session" {
                return v;
            }
        }
    }
}

#[tokio::test]
async fn resume_tokens_are_bound_to_their_bot() {
    let mut beta = bot(
        "beta-02",
        ALPHA_ORIGIN,
        "# Opening hours\n\nBeta opens at 10am on Sundays only.",
        "Beta",
    );
    beta["allowedOrigins"] = json!([ALPHA_ORIGIN, BETA_ORIGIN]);
    let h = start(&[]).await;
    h.sites.set("beta-02", beta);

    let mut a = connect(&h.addr, "alpha", ALPHA_ORIGIN).await.unwrap();
    a.send(resume_frame(None)).await.unwrap();
    let token = session_frame(&mut a).await["token"]
        .as_str()
        .unwrap()
        .to_owned();
    a.send(Message::Text("hello".into())).await.unwrap();
    until_done(&mut a).await;
    a.close(None).await.unwrap();

    let mut b = connect(&h.addr, "beta-02", ALPHA_ORIGIN).await.unwrap();
    b.send(resume_frame(Some(&token))).await.unwrap();
    assert_eq!(session_frame(&mut b).await["resumed"], json!(false));

    // ...and the token still works on its own bot.
    let mut a2 = connect(&h.addr, "alpha", ALPHA_ORIGIN).await.unwrap();
    a2.send(resume_frame(Some(&token))).await.unwrap();
    assert_eq!(session_frame(&mut a2).await["resumed"], json!(true));
}

#[tokio::test]
async fn single_site_routes_are_off_in_multi_site_mode_without_origins() {
    let h = start(&[]).await;
    assert_eq!(status(format!("http://{}/availability", h.addr)).await, 404);
    let mut req = format!("ws://{}/ws", h.addr).into_client_request().unwrap();
    req.headers_mut()
        .insert("origin", ALPHA_ORIGIN.parse().unwrap());
    assert!(tokio_tungstenite::connect_async(req).await.is_err());
    assert_eq!(status(format!("http://{}/health", h.addr)).await, 200);
}

#[test]
fn multi_site_config_is_validated() {
    let base = [
        ("LLM_API_KEY", "k"),
        ("SITES_API_URL", "http://sites:8787/x"),
    ];
    let err = Config::from_pairs(&base).unwrap_err();
    assert!(
        err.0.iter().any(|e| e.contains("INTERNAL_API_TOKEN")),
        "{err}"
    );
    assert!(
        !err.0.iter().any(|e| e.contains("ALLOWED_ORIGINS")),
        "origins come from the sites API: {err}"
    );
    let mut ok = base.to_vec();
    ok.push(("INTERNAL_API_TOKEN", TOKEN));
    let c = Config::from_pairs(&ok).unwrap();
    let sites = c.sites.unwrap();
    assert_eq!(sites.cache_ttl, Duration::from_secs(30));
    assert_eq!(sites.api_url, "http://sites:8787/x");
    let mut short = base.to_vec();
    short.push(("INTERNAL_API_TOKEN", "short"));
    assert!(Config::from_pairs(&short).is_err());
    let mut bad = ok.clone();
    bad[1] = ("SITES_API_URL", "ftp://sites");
    assert!(Config::from_pairs(&bad).is_err());
}

#[tokio::test]
async fn an_invalidation_during_a_slow_fetch_is_not_overwritten_by_old_settings() {
    let h = start(&[("SITES_CACHE_SECONDS", "300")]).await;
    h.sites.delay_ms.store(400, Ordering::SeqCst);
    let url = format!("http://{}/b/alpha/config", h.addr);
    let slow = tokio::spawn(reqwest::get(url.clone()));
    tokio::time::sleep(Duration::from_millis(150)).await;
    let mut changed = bot("alpha", ALPHA_ORIGIN, "# x\n\ny", "Renamed");
    changed["updatedAt"] = json!("2026-10-19T18:00:00.000Z");
    h.sites.set("alpha", changed);
    assert_eq!(invalidate(&h.addr, "alpha", TOKEN).await, 204);
    let old: Value = slow.await.unwrap().unwrap().json().await.unwrap();
    assert_eq!(
        old["title"], "Alpha Chat",
        "the slow fetch read the old settings"
    );
    h.sites.delay_ms.store(0, Ordering::SeqCst);
    let now: Value = reqwest::get(url).await.unwrap().json().await.unwrap();
    assert_eq!(now["title"], "Renamed Chat");
}

#[tokio::test]
async fn origins_from_the_sites_api_are_matched_case_insensitively() {
    let h = start(&[]).await;
    h.sites.set(
        "mixed",
        bot("mixed", "https://Alpha.Example/", "# x\n\ny", "Mixed"),
    );
    assert!(connect(&h.addr, "mixed", ALPHA_ORIGIN).await.is_ok());
}

#[tokio::test]
async fn oversized_sites_api_responses_are_refused() {
    let h = start(&[]).await;
    let huge = format!("# Big\n\n{}", "word ".repeat(300_000));
    h.sites
        .set("huge", bot("huge", ALPHA_ORIGIN, &huge, "Huge"));
    assert_eq!(
        status(format!("http://{}/b/huge/config", h.addr)).await,
        503
    );
}

#[tokio::test]
async fn parked_lead_cards_are_dropped_when_lead_capture_is_turned_off() {
    let h = start(&[]).await;
    let mut ws = connect(&h.addr, "alpha", ALPHA_ORIGIN).await.unwrap();
    ws.send(resume_frame(None)).await.unwrap();
    let token = session_frame(&mut ws).await["token"]
        .as_str()
        .unwrap()
        .to_owned();
    ws.send(Message::Text("Please call me, I'm sam@example.com".into()))
        .await
        .unwrap();
    let frames = until_done(&mut ws).await;
    assert!(frames.iter().any(|f| f["type"] == "card"));
    ws.close(None).await.unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;

    let mut off = bot(
        "alpha",
        ALPHA_ORIGIN,
        "# Opening hours\n\nAlpha opens at 7am every day of the week.",
        "Alpha",
    );
    off["leadCapture"] = json!(false);
    off["updatedAt"] = json!("2026-10-19T18:00:00.000Z");
    h.sites.set("alpha", off);
    assert_eq!(invalidate(&h.addr, "alpha", TOKEN).await, 204);

    let mut ws = connect(&h.addr, "alpha", ALPHA_ORIGIN).await.unwrap();
    ws.send(resume_frame(Some(&token))).await.unwrap();
    assert_eq!(session_frame(&mut ws).await["resumed"], json!(true));
    let extra = tokio::time::timeout(Duration::from_millis(300), ws.next()).await;
    assert!(extra.is_err(), "no lead card is shown again: {extra:?}");
}

fn partner_bot(id: &str) -> Value {
    let mut b = bot(
        id,
        ALPHA_ORIGIN,
        "# Hours\n\nPartner opens at 8am.",
        "Partner",
    );
    b["partnerId"] = json!("tradesites");
    b["version"] = json!(3);
    b["caps"] = json!({"repliesPerHour": 60, "repliesPerDay": 300, "partnerRepliesPerDay": 20000});
    b
}

#[tokio::test]
async fn partner_details_never_reach_the_public_config() {
    let h = start(&[]).await;
    h.sites.set("ts-abc", partner_bot("ts-abc"));
    let v: Value = reqwest::get(format!("http://{}/b/ts-abc/config", h.addr))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let mut keys: Vec<&String> = v.as_object().unwrap().keys().collect();
    keys.sort();
    assert_eq!(
        keys,
        ["greeting", "placeholder", "privacyUrl", "subtitle", "title"]
    );
    assert!(!v.to_string().contains("tradesites"));
}

#[tokio::test]
async fn partner_bots_are_served_stale_for_less_time() {
    let h = start(&[
        ("SITES_CACHE_SECONDS", "1"),
        ("SITES_STALE_SECONDS", "600"),
        ("SITES_PARTNER_STALE_SECONDS", "2"),
    ])
    .await;
    h.sites.set("ts-abc", partner_bot("ts-abc"));
    for id in ["alpha", "ts-abc"] {
        assert_eq!(
            status(format!("http://{}/b/{id}/config", h.addr)).await,
            200
        );
    }
    h.sites.down.store(true, Ordering::SeqCst);
    tokio::time::sleep(Duration::from_millis(2600)).await;
    assert_eq!(
        status(format!("http://{}/b/alpha/config", h.addr)).await,
        200,
        "a normal bot is still served stale"
    );
    assert_eq!(
        status(format!("http://{}/b/ts-abc/config", h.addr)).await,
        503,
        "a partner bot is not"
    );
}

#[tokio::test]
async fn a_new_version_is_reparsed_even_with_the_same_updated_at() {
    // No invalidation: the cached copy expires and is refetched with the same updatedAt.
    let h = start(&[("SITES_CACHE_SECONDS", "1")]).await;
    h.sites.set("ts-abc", partner_bot("ts-abc"));
    let a = ask(&h.addr, "ts-abc", ALPHA_ORIGIN, "What are your hours?").await;
    assert!(a.contains("8am"), "{a}");
    let mut changed = partner_bot("ts-abc");
    changed["knowledgeBase"] = json!("# Hours\n\nPartner opens at 9am.");
    changed["version"] = json!(4);
    h.sites.set("ts-abc", changed);
    tokio::time::sleep(Duration::from_millis(1100)).await;
    // The first request after expiry is served the old copy while it refreshes.
    status(format!("http://{}/b/ts-abc/config", h.addr)).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    let a = ask(&h.addr, "ts-abc", ALPHA_ORIGIN, "What are your hours?").await;
    assert!(a.contains("9am"), "{a}");
}
