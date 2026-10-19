use axum::routing::post;
use futures_util::{SinkExt, StreamExt};
use open_rust_chatbot::config::Config;
use open_rust_chatbot::{app, build_state, webhook};
use serde_json::{json, Value};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message;

const ORIGIN: &str = "https://shop.example";
const SECRET: &str = "0123456789abcdef0123456789abcdef";

type Ws =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

async fn serve(extra: &[(&str, &str)]) -> String {
    let mut env = vec![
        ("DEV_MODE", "true"),
        ("MOCK_LLM", "true"),
        ("MOCK_TOKEN_DELAY_MS", "0"),
        ("ALLOWED_ORIGINS", ORIGIN),
        ("KNOWLEDGE_BASE_PATH", "examples/knowledge-base.md"),
    ];
    env.extend_from_slice(extra);
    let router = app(build_state(Config::from_pairs(&env).unwrap()).unwrap());
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

async fn connect_from(addr: &str, origin: &str, client_ip: &str) -> Result<Ws, String> {
    let mut req = format!("ws://{addr}/ws").into_client_request().unwrap();
    req.headers_mut().insert("origin", origin.parse().unwrap());
    req.headers_mut()
        .insert("x-forwarded-for", client_ip.parse().unwrap());
    tokio_tungstenite::connect_async(req)
        .await
        .map(|(ws, _)| ws)
        .map_err(|e| e.to_string())
}

async fn connect(addr: &str, origin: &str) -> Result<Ws, String> {
    let mut req = format!("ws://{addr}/ws").into_client_request().unwrap();
    req.headers_mut().insert("origin", origin.parse().unwrap());
    tokio_tungstenite::connect_async(req)
        .await
        .map(|(ws, _)| ws)
        .map_err(|e| e.to_string())
}

async fn until_done(ws: &mut Ws) -> Vec<Value> {
    let mut frames = Vec::new();
    while let Some(Ok(Message::Text(t))) = ws.next().await {
        let v: Value = serde_json::from_str(&t).unwrap();
        let end = v["type"] == "done" || v["type"] == "error";
        frames.push(v);
        if end {
            break;
        }
    }
    frames
}

fn text_of(frames: &[Value]) -> String {
    frames
        .iter()
        .filter(|f| f["type"] == "token")
        .filter_map(|f| f["value"].as_str())
        .collect()
}

#[tokio::test]
async fn answers_questions_from_the_knowledge_base() {
    let addr = serve(&[]).await;
    let mut ws = connect(&addr, ORIGIN).await.unwrap();
    ws.send(Message::Text("What are your opening hours?".into()))
        .await
        .unwrap();
    let frames = until_done(&mut ws).await;
    assert_eq!(
        frames[0],
        json!({"type": "tool_start", "name": "search_knowledge_base"})
    );
    assert!(text_of(&frames).contains("8am to 6pm"), "{frames:?}");
}

#[tokio::test]
async fn rejects_other_origins_and_missing_origin() {
    let addr = serve(&[]).await;
    assert!(connect(&addr, "https://evil.example")
        .await
        .unwrap_err()
        .contains("403"));
    let req = format!("ws://{addr}/ws").into_client_request().unwrap();
    assert!(tokio_tungstenite::connect_async(req)
        .await
        .unwrap_err()
        .to_string()
        .contains("403"));
}

#[tokio::test]
async fn lead_is_only_sent_after_visitor_confirms_and_is_signed() {
    let received: Arc<Mutex<Vec<(String, String, String)>>> = Arc::default();
    let sink = received.clone();
    let hook = axum::Router::new().route(
        "/hook",
        post(move |headers: axum::http::HeaderMap, body: String| {
            let sink = sink.clone();
            async move {
                let h = |k: &str| headers[k].to_str().unwrap().to_owned();
                sink.lock()
                    .unwrap()
                    .push((h("x-orc-timestamp"), h("x-orc-signature"), body));
                "ok"
            }
        }),
    );
    let hook_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let hook_url = format!("http://{}/hook", hook_listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(hook_listener, hook).await.unwrap() });

    let addr = serve(&[
        ("LEAD_WEBHOOK_URL", &hook_url),
        ("LEAD_WEBHOOK_SECRET", SECRET),
    ])
    .await;
    let mut ws = connect(&addr, ORIGIN).await.unwrap();
    ws.send(Message::Text("Please call me, I'm sam@example.com".into()))
        .await
        .unwrap();
    let frames = until_done(&mut ws).await;
    let card = frames
        .iter()
        .find(|f| f["type"] == "card")
        .expect("confirmation card");
    assert_eq!(card["card"]["type"], "lead_confirmation");
    assert!(
        received.lock().unwrap().is_empty(),
        "nothing is sent before the visitor confirms"
    );

    let lead_id = card["card"]["lead"]["leadId"].as_str().unwrap().to_owned();

    // A different visitor cannot confirm it.
    let mut other = connect(&addr, ORIGIN).await.unwrap();
    other
        .send(Message::Text(
            json!({"type": "confirm_lead", "leadId": lead_id})
                .to_string()
                .into(),
        ))
        .await
        .unwrap();
    let stolen = until_done(&mut other).await;
    assert_eq!(stolen[0]["type"], "error");
    assert!(received.lock().unwrap().is_empty());

    ws.send(Message::Text(
        json!({"type": "confirm_lead", "leadId": lead_id})
            .to_string()
            .into(),
    ))
    .await
    .unwrap();
    let confirmed = until_done(&mut ws).await;
    assert_eq!(confirmed[0]["card"]["type"], "lead_submitted");

    let calls = received.lock().unwrap().clone();
    assert_eq!(calls.len(), 1);
    let (ts, sig, body) = &calls[0];
    assert_eq!(
        sig,
        &webhook::sign(SECRET, ts.parse().unwrap(), body.as_bytes())
    );
    let payload: Value = serde_json::from_str(body).unwrap();
    assert_eq!(payload["lead"]["email"], "sam@example.com");

    // Confirming twice does not resend.
    ws.send(Message::Text(
        json!({"type": "confirm_lead", "leadId": lead_id})
            .to_string()
            .into(),
    ))
    .await
    .unwrap();
    until_done(&mut ws).await;
    assert_eq!(received.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn enforces_message_size_and_connection_limits() {
    let addr = serve(&[("MAX_CONNECTIONS_PER_IP", "1"), ("MAX_MESSAGE_CHARS", "10")]).await;
    let mut ws = connect(&addr, ORIGIN).await.unwrap();
    assert!(connect(&addr, ORIGIN).await.unwrap_err().contains("429"));

    ws.send(Message::Text("this is far too long".into()))
        .await
        .unwrap();
    let frames = until_done(&mut ws).await;
    assert!(frames[0]["message"]
        .as_str()
        .unwrap()
        .contains("limited to 10"));

    // Oversized frames close the socket instead of being buffered.
    ws.send(Message::Text("x".repeat(64 * 1024).into()))
        .await
        .unwrap();
    let closed = tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            match ws.next().await {
                None | Some(Err(_)) | Some(Ok(Message::Close(_))) => return true,
                _ => continue,
            }
        }
    })
    .await
    .unwrap_or(false);
    assert!(closed);

    // The slot is released, so the visitor can reconnect.
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    assert!(connect(&addr, ORIGIN).await.is_ok());
}

#[tokio::test]
async fn rate_limits_messages() {
    let addr = serve(&[("RATE_LIMIT_MESSAGES", "1")]).await;
    let mut ws = connect(&addr, ORIGIN).await.unwrap();
    ws.send(Message::Text("hello".into())).await.unwrap();
    until_done(&mut ws).await;
    ws.send(Message::Text("again".into())).await.unwrap();
    let frames = until_done(&mut ws).await;
    assert!(frames[0]["message"]
        .as_str()
        .unwrap()
        .contains("too quickly"));
}

#[tokio::test]
async fn serves_compressed_cacheable_widget_and_hides_demo_in_production() {
    let addr = serve(&[]).await;
    let client = reqwest::Client::builder().no_gzip().build().unwrap();
    let res = client
        .get(format!("http://{addr}/widget.js"))
        .header("accept-encoding", "gzip")
        .send()
        .await
        .unwrap();
    assert_eq!(res.headers()["content-encoding"], "gzip");
    assert_eq!(res.headers()["cache-control"], "public, max-age=3600");
    assert_eq!(res.headers()["x-content-type-options"], "nosniff");

    let prod_env = [("ALLOWED_ORIGINS", ORIGIN), ("LLM_API_KEY", "test")];
    let router = app(build_state(Config::from_pairs(&prod_env).unwrap()).unwrap());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let prod = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(
            listener,
            router.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await
        .unwrap()
    });
    assert_eq!(
        reqwest::get(format!("http://{prod}/"))
            .await
            .unwrap()
            .status(),
        404
    );
}

#[tokio::test]
async fn openclaw_bridge_gets_kb_context_and_server_offers_lead_card() {
    let prompts: Arc<Mutex<Vec<Value>>> = Arc::default();
    let sink = prompts.clone();
    let bridge = axum::Router::new().route(
        "/bridge",
        post(
            move |headers: axum::http::HeaderMap, axum::Json(body): axum::Json<Value>| {
                let sink = sink.clone();
                async move {
                    assert_eq!(headers["authorization"], "Bearer bridge-token");
                    sink.lock().unwrap().push(body);
                    axum::Json(json!({ "text": "We open at 8am on weekdays." }))
                }
            },
        ),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let bridge_url = format!("http://{}/bridge", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, bridge).await.unwrap() });

    let addr = serve(&[
        ("MOCK_LLM", "false"),
        ("LLM_PROVIDER", "openclaw"),
        ("OPENCLAW_BRIDGE_URL", &bridge_url),
        ("OPENCLAW_BRIDGE_TOKEN", "bridge-token"),
    ])
    .await;
    let mut ws = connect(&addr, ORIGIN).await.unwrap();
    ws.send(Message::Text(
        "What are your opening hours? I'm sam@example.com".into(),
    ))
    .await
    .unwrap();
    let frames = until_done(&mut ws).await;
    assert_eq!(text_of(&frames), "We open at 8am on weekdays.");
    let card = frames
        .iter()
        .find(|f| f["type"] == "card")
        .expect("server-side lead card");
    assert_eq!(card["card"]["lead"]["email"], "sam@example.com");

    let sent = prompts.lock().unwrap()[0]["message"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(
        sent.contains("8am to 6pm Monday to Friday"),
        "KB excerpt missing: {sent}"
    );
    assert!(sent.contains("untrusted"), "{sent}");
}

#[tokio::test]
async fn flooding_without_reading_gets_the_socket_closed_with_few_replies() {
    let addr = serve(&[("RATE_LIMIT_MESSAGES", "2")]).await;
    let mut ws = connect(&addr, ORIGIN).await.unwrap();
    for _ in 0..500 {
        if ws.send(Message::Text("x".into())).await.is_err() {
            break;
        }
    }
    let mut errors = 0;
    let closed = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            match ws.next().await {
                Some(Ok(Message::Text(t))) if t.contains("\"error\"") => errors += 1,
                None | Some(Err(_)) | Some(Ok(Message::Close(_))) => return true,
                _ => {}
            }
        }
    })
    .await
    .unwrap_or(false);
    assert!(closed, "flooding socket must be closed");
    assert!(
        errors <= 21,
        "replies are bounded by the close threshold, got {errors}"
    );
}

#[test]
fn client_ip_trusts_only_the_configured_header_and_groups_ipv6() {
    use axum::http::HeaderMap;
    use open_rust_chatbot::client_ip;
    let peer: SocketAddr = "10.0.0.1:5000".parse().unwrap();
    let mut headers = HeaderMap::new();
    headers.insert("x-forwarded-for", "1.1.1.1, 203.0.113.9".parse().unwrap());
    headers.insert("cf-connecting-ip", "198.51.100.7".parse().unwrap());

    let direct = Config::from_pairs(&[("DEV_MODE", "true"), ("MOCK_LLM", "true")]).unwrap();
    assert_eq!(client_ip(&direct, &headers, peer), "10.0.0.1");

    let proxied = Config::from_pairs(&[
        ("DEV_MODE", "true"),
        ("MOCK_LLM", "true"),
        ("TRUST_PROXY_HEADERS", "true"),
    ])
    .unwrap();
    assert_eq!(client_ip(&proxied, &headers, peer), "203.0.113.9");

    let cloudflare = Config::from_pairs(&[
        ("DEV_MODE", "true"),
        ("MOCK_LLM", "true"),
        ("TRUST_PROXY_HEADERS", "true"),
        ("CLIENT_IP_HEADER", "CF-Connecting-IP"),
    ])
    .unwrap();
    assert_eq!(client_ip(&cloudflare, &headers, peer), "198.51.100.7");

    let v6: SocketAddr = "[2001:db8:1:2:aaaa:bbbb:cccc:dddd]:5000".parse().unwrap();
    assert_eq!(
        client_ip(&direct, &HeaderMap::new(), v6),
        "2001:db8:1:2::/64"
    );
}

#[test]
fn rate_limit_wait_is_human_readable() {
    assert_eq!(open_rust_chatbot::human_wait(249_000), "about 5 minutes");
    assert_eq!(open_rust_chatbot::human_wait(45_000), "45 seconds");
    assert_eq!(open_rust_chatbot::human_wait(60_000), "about 1 minute");
}

#[tokio::test]
async fn openclaw_bridge_blip_is_retried_once_and_policy_errors_are_not() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = calls.clone();
    let bridge = axum::Router::new().route(
        "/bridge",
        post(move |axum::Json(body): axum::Json<Value>| {
            let counter = counter.clone();
            async move {
                let n = counter.fetch_add(1, Ordering::SeqCst);
                let msg = body["message"].as_str().unwrap_or_default().to_owned();
                if msg.contains("forbidden") {
                    return (
                        axum::http::StatusCode::FORBIDDEN,
                        axum::Json(json!({"error": "no"})),
                    );
                }
                if n == 0 {
                    return (
                        axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                        axum::Json(json!({"error": "blip"})),
                    );
                }
                (
                    axum::http::StatusCode::OK,
                    axum::Json(json!({"text": "Recovered."})),
                )
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let bridge_url = format!("http://{}/bridge", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, bridge).await.unwrap() });

    let addr = serve(&[
        ("MOCK_LLM", "false"),
        ("LLM_PROVIDER", "openclaw"),
        ("OPENCLAW_BRIDGE_URL", &bridge_url),
        ("OPENCLAW_BRIDGE_TOKEN", "t"),
    ])
    .await;
    let mut ws = connect(&addr, ORIGIN).await.unwrap();
    ws.send(Message::Text("hello".into())).await.unwrap();
    assert_eq!(text_of(&until_done(&mut ws).await), "Recovered.");
    assert_eq!(calls.load(Ordering::SeqCst), 2);

    ws.send(Message::Text("forbidden please".into()))
        .await
        .unwrap();
    let frames = until_done(&mut ws).await;
    assert_eq!(frames[0]["type"], "error");
    assert_eq!(calls.load(Ordering::SeqCst), 3, "403 is not retried");
}

#[tokio::test]
async fn availability_is_readable_cross_origin_and_never_cached() {
    let addr = serve(&[]).await;
    let res = reqwest::Client::new()
        .get(format!("http://{addr}/availability"))
        .header("origin", "https://shop.example")
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 200);
    assert_eq!(res.headers()["access-control-allow-origin"], "*");
    assert_eq!(res.headers()["cache-control"], "no-store");
}

#[tokio::test]
async fn health_reports_down_after_repeated_model_failures_and_recovers() {
    use std::sync::atomic::{AtomicBool, Ordering};
    let failing = Arc::new(AtomicBool::new(true));
    let flag = failing.clone();
    let bridge = axum::Router::new().route(
        "/bridge",
        post(move || {
            let flag = flag.clone();
            async move {
                if flag.load(Ordering::SeqCst) {
                    (
                        axum::http::StatusCode::SERVICE_UNAVAILABLE,
                        axum::Json(json!({"error": "down"})),
                    )
                } else {
                    (
                        axum::http::StatusCode::OK,
                        axum::Json(json!({"text": "Back."})),
                    )
                }
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let bridge_url = format!("http://{}/bridge", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, bridge).await.unwrap() });
    let addr = serve(&[
        ("MOCK_LLM", "false"),
        ("LLM_PROVIDER", "openclaw"),
        ("OPENCLAW_BRIDGE_URL", &bridge_url),
        ("OPENCLAW_BRIDGE_TOKEN", "t"),
        ("MODEL_FAILURE_COOLDOWN_SECONDS", "1"),
        ("TRUST_PROXY_HEADERS", "true"),
    ])
    .await;
    let health = || async {
        reqwest::get(format!("http://{addr}/availability"))
            .await
            .unwrap()
            .status()
            .as_u16()
    };
    // One visitor alone can't hide the bot, however many failures they cause.
    let mut ws = connect_from(&addr, ORIGIN, "198.51.100.1").await.unwrap();
    for i in 0..4 {
        ws.send(Message::Text(format!("question {i}").into()))
            .await
            .unwrap();
        until_done(&mut ws).await;
    }
    assert_eq!(
        health().await,
        200,
        "a single visitor's failures don't trip the breaker"
    );

    // Outages seen by a second visitor do.
    let mut other = connect_from(&addr, ORIGIN, "198.51.100.2").await.unwrap();
    other.send(Message::Text("hello".into())).await.unwrap();
    until_done(&mut other).await;
    assert_eq!(
        health().await,
        503,
        "repeated outages across visitors mark the chat as down"
    );
    let liveness = reqwest::get(format!("http://{addr}/health"))
        .await
        .unwrap()
        .status();
    assert_eq!(
        liveness, 200,
        "liveness stays up so the container isn't restarted"
    );

    // After the cooldown one request is let through; a success makes it healthy again.
    failing.store(false, std::sync::atomic::Ordering::SeqCst);
    tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
    assert_eq!(health().await, 200);
    ws.send(Message::Text("again".into())).await.unwrap();
    assert_eq!(text_of(&until_done(&mut ws).await), "Back.");
    assert_eq!(health().await, 200);
}

#[tokio::test]
async fn rejected_visitor_input_does_not_hide_the_bot() {
    let bridge = axum::Router::new().route(
        "/bridge",
        post(|| async {
            (
                axum::http::StatusCode::FORBIDDEN,
                axum::Json(json!({"error": "not allowed"})),
            )
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let bridge_url = format!("http://{}/bridge", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, bridge).await.unwrap() });
    let addr = serve(&[
        ("MOCK_LLM", "false"),
        ("LLM_PROVIDER", "openclaw"),
        ("OPENCLAW_BRIDGE_URL", &bridge_url),
        ("OPENCLAW_BRIDGE_TOKEN", "t"),
    ])
    .await;
    let mut ws = connect(&addr, ORIGIN).await.unwrap();
    for i in 0..5 {
        ws.send(Message::Text(format!("bad {i}").into()))
            .await
            .unwrap();
        until_done(&mut ws).await;
    }
    assert_eq!(
        reqwest::get(format!("http://{addr}/availability"))
            .await
            .unwrap()
            .status(),
        200
    );
}

async fn next_frame(ws: &mut Ws) -> Value {
    let read = async {
        loop {
            if let Some(Ok(Message::Text(t))) = ws.next().await {
                return serde_json::from_str::<Value>(&t).unwrap();
            }
        }
    };
    tokio::time::timeout(std::time::Duration::from_secs(5), read)
        .await
        .expect("frame within 5s")
}

fn resume_frame(token: Option<&str>) -> Message {
    Message::Text(json!({"type": "resume", "token": token}).to_string().into())
}

#[tokio::test]
async fn conversation_resumes_on_the_next_page_with_a_single_use_token() {
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
                axum::Json(json!({ "text": "Noted." }))
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let bridge_url = format!("http://{}/bridge", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, bridge).await.unwrap() });
    let addr = serve(&[
        ("MOCK_LLM", "false"),
        ("LLM_PROVIDER", "openclaw"),
        ("OPENCLAW_BRIDGE_URL", &bridge_url),
        ("OPENCLAW_BRIDGE_TOKEN", "t"),
    ])
    .await;

    // Page 1: the widget asks for a session and gets a resume token.
    let mut page1 = connect(&addr, ORIGIN).await.unwrap();
    page1.send(resume_frame(None)).await.unwrap();
    let session = next_frame(&mut page1).await;
    assert_eq!(
        (session["type"].as_str(), session["resumed"].as_bool()),
        (Some("session"), Some(false))
    );
    let token = session["token"].as_str().unwrap().to_owned();
    assert_eq!(token.len(), 64);
    page1
        .send(Message::Text(
            "I need an EICR for my flat in Croydon".into(),
        ))
        .await
        .unwrap();
    until_done(&mut page1).await;
    page1.close(None).await.unwrap();

    // Page 2: resuming restores the conversation for the model, under a fresh token.
    let mut page2 = connect(&addr, ORIGIN).await.unwrap();
    page2.send(resume_frame(Some(&token))).await.unwrap();
    let resumed = next_frame(&mut page2).await;
    assert_eq!(
        (resumed["resumed"].as_bool(), resumed["turns"].as_u64()),
        (Some(true), Some(1))
    );
    assert_ne!(resumed["token"], json!(token));
    page2
        .send(Message::Text("How often do I need one?".into()))
        .await
        .unwrap();
    until_done(&mut page2).await;
    let last = prompts.lock().unwrap().last().unwrap().clone();
    assert!(
        last.contains("EICR for my flat in Croydon"),
        "earlier turn restored: {last}"
    );

    // The old token is single-use, so a copied token or second tab can't take over the chat.
    let mut page3 = connect(&addr, ORIGIN).await.unwrap();
    page3.send(resume_frame(Some(&token))).await.unwrap();
    assert_eq!(next_frame(&mut page3).await["resumed"], json!(false));
}

#[tokio::test]
async fn resume_is_only_accepted_as_the_first_message() {
    let addr = serve(&[]).await;
    let mut page1 = connect(&addr, ORIGIN).await.unwrap();
    page1.send(resume_frame(None)).await.unwrap();
    let token = next_frame(&mut page1).await["token"]
        .as_str()
        .unwrap()
        .to_owned();
    page1.send(Message::Text("hello".into())).await.unwrap();
    until_done(&mut page1).await;
    page1.close(None).await.unwrap();

    let mut page2 = connect(&addr, ORIGIN).await.unwrap();
    page2.send(Message::Text("hi".into())).await.unwrap();
    until_done(&mut page2).await;
    page2.send(resume_frame(Some(&token))).await.unwrap();
    page2
        .send(Message::Text("still here".into()))
        .await
        .unwrap();
    let frames = until_done(&mut page2).await;
    assert!(
        frames.iter().all(|f| f["type"] != "session"),
        "ignored mid-conversation: {frames:?}"
    );
}

#[tokio::test]
async fn a_pending_lead_survives_a_page_change_and_can_still_be_confirmed() {
    let addr = serve(&[]).await; // mock model, DEV_MODE: lead capture on
    let mut page1 = connect(&addr, ORIGIN).await.unwrap();
    page1.send(resume_frame(None)).await.unwrap();
    let token = next_frame(&mut page1).await["token"]
        .as_str()
        .unwrap()
        .to_owned();
    page1
        .send(Message::Text("Call me on sam@example.com".into()))
        .await
        .unwrap();
    let frames = until_done(&mut page1).await;
    let lead_id = frames.iter().find(|f| f["type"] == "card").unwrap()["card"]["lead"]["leadId"]
        .as_str()
        .unwrap()
        .to_owned();
    page1.close(None).await.unwrap();

    let mut page2 = connect(&addr, ORIGIN).await.unwrap();
    page2.send(resume_frame(Some(&token))).await.unwrap();
    assert_eq!(next_frame(&mut page2).await["resumed"], json!(true));
    let card = next_frame(&mut page2).await;
    assert_eq!(
        card["card"]["lead"]["leadId"],
        json!(lead_id),
        "pending card shown again"
    );
    page2
        .send(Message::Text(
            json!({"type": "confirm_lead", "leadId": lead_id})
                .to_string()
                .into(),
        ))
        .await
        .unwrap();
    let confirmed = until_done(&mut page2).await;
    assert_eq!(confirmed[0]["card"]["type"], "lead_submitted");
}

#[tokio::test]
async fn expired_conversations_are_swept_from_memory() {
    let addr = serve(&[("RESUME_TTL_SECONDS", "1")]).await;
    let mut page1 = connect(&addr, ORIGIN).await.unwrap();
    page1.send(resume_frame(None)).await.unwrap();
    let token = next_frame(&mut page1).await["token"]
        .as_str()
        .unwrap()
        .to_owned();
    page1.close(None).await.unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(1200)).await;
    let mut page2 = connect(&addr, ORIGIN).await.unwrap();
    page2.send(resume_frame(Some(&token))).await.unwrap();
    assert_eq!(
        next_frame(&mut page2).await["resumed"],
        json!(false),
        "expired after the TTL"
    );
}

#[test]
fn parked_store_sweeps_expired_entries_on_save() {
    let config = Config::from_pairs(&[
        ("DEV_MODE", "true"),
        ("MOCK_LLM", "true"),
        ("RESUME_TTL_SECONDS", "1"),
    ])
    .unwrap();
    let shared = build_state(config).unwrap();
    shared.save_session("abandoned", Default::default());
    std::thread::sleep(std::time::Duration::from_millis(1100));
    // Any later save sweeps expired chats, even ones nobody ever comes back for.
    shared.save_session("active", Default::default());
    assert_eq!(
        shared.parked_sessions(),
        1,
        "the abandoned chat was removed from memory"
    );
    assert!(shared.take_session("active").is_some());
}
