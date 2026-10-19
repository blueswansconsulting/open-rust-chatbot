use open_rust_chatbot::config::Config;
use open_rust_chatbot::{app, build_state};
use std::net::SocketAddr;

// Chat is I/O-bound (waiting on the model); one thread avoids cross-thread wakeups per streamed token.
#[tokio::main(flavor = "current_thread")]
async fn main() {
    if std::env::args().nth(1).as_deref() == Some("healthcheck") {
        std::process::exit(healthcheck());
    }
    let config = match Config::from_env() {
        Ok(config) => config,
        Err(err) => {
            eprintln!("{err}");
            std::process::exit(2);
        }
    };
    let addr = format!("{}:{}", config.host, config.port);
    if !config.production {
        eprintln!("DEV_MODE is on: demo page at http://{addr}/ (never use DEV_MODE in production)");
    }
    let state = match build_state(config) {
        Ok(state) => state,
        Err(err) => {
            eprintln!("startup failed: {err}");
            std::process::exit(2);
        }
    };
    let sweeper = state.clone();
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(30));
        loop {
            tick.tick().await;
            sweeper.sweep_expired_sessions();
        }
    });
    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .unwrap_or_else(|e| {
            eprintln!("could not bind {addr}: {e}");
            std::process::exit(2);
        });
    eprintln!("open-rust-chatbot listening on http://{addr}");
    axum::serve(
        listener,
        app(state).into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown_signal())
    .await
    .expect("server error");
}

async fn shutdown_signal() {
    #[cfg(unix)]
    {
        let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("sigterm handler");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = term.recv() => {}
        }
    }
    #[cfg(not(unix))]
    let _ = tokio::signal::ctrl_c().await;
}

/// `open-rust-chatbot healthcheck` for container HEALTHCHECKs in images without curl.
fn healthcheck() -> i32 {
    use std::io::{Read, Write};
    let port = std::env::var("PORT").unwrap_or_else(|_| "8787".into());
    let Ok(mut stream) = std::net::TcpStream::connect(("127.0.0.1", port.parse().unwrap_or(8787)))
    else {
        return 1;
    };
    let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(3)));
    if stream
        .write_all(b"GET /health HTTP/1.0\r\nHost: localhost\r\n\r\n")
        .is_err()
    {
        return 1;
    }
    let mut response = String::new();
    let _ = stream.read_to_string(&mut response);
    if response.starts_with("HTTP/1.1 200") || response.starts_with("HTTP/1.0 200") {
        0
    } else {
        1
    }
}
