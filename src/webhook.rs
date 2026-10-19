//! Delivers confirmed leads to the operator's webhook, signed so the receiver can verify them.
//!
//! Headers sent:
//! - `X-ORC-Timestamp`: unix seconds
//! - `X-ORC-Signature`: `sha256=<hex HMAC-SHA256(secret, "<timestamp>.<body>")>`
//!
//! Receivers should recompute the HMAC, compare in constant time, and reject timestamps
//! older than a few minutes to stop replays.

use crate::tools::Lead;
use hmac::{Hmac, KeyInit, Mac};
use serde_json::json;
use sha2::Sha256;
use std::time::Duration;

pub fn sign(secret: &str, timestamp: u64, body: &[u8]) -> String {
    let mut mac =
        Hmac::<Sha256>::new_from_slice(secret.as_bytes()).expect("hmac accepts any key length");
    mac.update(timestamp.to_string().as_bytes());
    mac.update(b".");
    mac.update(body);
    format!("sha256={}", hex::encode(mac.finalize().into_bytes()))
}

pub fn payload(lead: &Lead, bot_name: &str) -> Vec<u8> {
    json!({ "type": "lead", "bot": bot_name, "lead": lead, "submittedAt": crate::now_iso() })
        .to_string()
        .into_bytes()
}

pub async fn deliver(
    http: &reqwest::Client,
    url: &str,
    secret: &str,
    body: Vec<u8>,
) -> Result<(), String> {
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let signature = sign(secret, timestamp, &body);
    let mut last_error = String::new();
    for attempt in 0..3u32 {
        if attempt > 0 {
            tokio::time::sleep(Duration::from_millis(500 * 3u64.pow(attempt - 1))).await;
        }
        let res = http
            .post(url)
            .timeout(Duration::from_secs(10))
            .header("content-type", "application/json")
            .header("x-orc-timestamp", timestamp.to_string())
            .header("x-orc-signature", &signature)
            .body(body.clone())
            .send()
            .await;
        match res {
            Ok(r) if r.status().is_success() => return Ok(()),
            Ok(r) if r.status().is_client_error() && r.status().as_u16() != 429 => {
                return Err(format!(
                    "webhook rejected lead with HTTP {}",
                    r.status().as_u16()
                ));
            }
            Ok(r) => last_error = format!("webhook HTTP {}", r.status().as_u16()),
            Err(e) => last_error = format!("webhook request failed: {}", e.without_url()),
        }
    }
    Err(last_error)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signature_matches_known_vector() {
        // echo -n '1700000000.{}' | openssl dgst -sha256 -hmac 'secret'
        assert_eq!(
            sign("secret", 1_700_000_000, b"{}"),
            "sha256=b8569b78799ff9e3cbff0fc2d63a33a2b57f3282abd07c37ae5e8e7d79a5f163"
        );
    }
}
