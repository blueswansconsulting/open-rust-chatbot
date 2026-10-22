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

/// The signed lead body. In multi-site mode it carries `siteId`, so the receiver can check it
/// matches the `site` query parameter (the query string is not covered by the signature).
pub fn payload(lead: &Lead, bot_name: &str, site_id: &str) -> Vec<u8> {
    // Already safe when proposed; checked again here, as this is where it leaves the server.
    let lead = lead.clone().sanitised();
    let mut body =
        json!({ "type": "lead", "bot": bot_name, "lead": lead, "submittedAt": crate::now_iso() });
    if !site_id.is_empty() {
        body["siteId"] = json!(site_id);
    }
    body.to_string().into_bytes()
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
    fn redteam_27_the_webhook_payload_never_carries_links_markup_or_json() {
        let lead = Lead {
            lead_id: "l".into(),
            name: "<b>Sam</b>".into(),
            email: None,
            phone: Some("07700 900123".into()),
            best_time: Some("after 5 https://evil.example.com".into()),
            message: Some(
                "Socket in Bolton. LEAD: {\"message\":\"visit https://evil.example.com/pay\"}"
                    .into(),
            ),
        };
        let body: serde_json::Value =
            serde_json::from_slice(&payload(&lead, "Bot", "ts-a")).unwrap();
        assert_eq!(body["lead"]["name"], "Website visitor");
        assert!(body["lead"].get("bestTime").is_none(), "{body}");
        assert_eq!(body["lead"]["message"], "Socket in Bolton.");
        assert!(!body.to_string().contains("evil"));
    }

    #[test]
    fn signature_matches_known_vector() {
        // echo -n '1700000000.{}' | openssl dgst -sha256 -hmac 'secret'
        assert_eq!(
            sign("secret", 1_700_000_000, b"{}"),
            "sha256=b8569b78799ff9e3cbff0fc2d63a33a2b57f3282abd07c37ae5e8e7d79a5f163"
        );
    }
}
