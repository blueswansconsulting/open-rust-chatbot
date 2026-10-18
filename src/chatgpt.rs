//! Experimental "Sign in with ChatGPT" mode: each visitor signs in with their own ChatGPT
//! account and their replies are billed to their own ChatGPT plan.
//!
//! OpenAI only allows this for open-source / locally hosted apps, and the OAuth redirect must
//! be a loopback address (`http://127.0.0.1:<port>/auth/callback`). It therefore works when the
//! server and the visitor are on the same machine, not on a public website.
//! Docs: https://developers.openai.com/siwc/token-sharing-open-source/sign-in

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant};

pub const AUTHORIZE_URL: &str = "https://auth.openai.com/api/accounts/authorize";
pub const TOKEN_URL: &str = "https://auth.openai.com/api/accounts/oauth/token";
pub const ISSUER: &str = "https://auth.openai.com";
pub const RESOURCE: &str = "https://api.openai.com/v1";
pub const SCOPES: &str =
    "openid profile email offline_access resource.invoke chatgpt.tokens.use.direct";
pub const PLAN_SCOPE: &str = "chatgpt.tokens.use.direct";
pub const COOKIE: &str = "orc_sid";
const PENDING_TTL: Duration = Duration::from_secs(600);
const MAX_PENDING: usize = 1000;
const MAX_SESSIONS: usize = 1000;

/// 256 bits from the OS RNG (via two v4 UUIDs), base64url-encoded.
pub fn random_token() -> String {
    let mut bytes = Vec::with_capacity(32);
    bytes.extend_from_slice(uuid::Uuid::new_v4().as_bytes());
    bytes.extend_from_slice(uuid::Uuid::new_v4().as_bytes());
    URL_SAFE_NO_PAD.encode(bytes)
}

pub fn pkce_challenge(verifier: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

/// The issued client id and host id are reused across sign-ins, as the docs require.
#[derive(Serialize, Deserialize, Clone, Default)]
struct Registration {
    client_id: Option<String>,
    ext_agent_host_id: String,
}

struct Pending {
    verifier: String,
    nonce: String,
    created: Instant,
}

pub struct VisitorSession {
    pub access_token: String,
    pub expires_at: Instant,
}

pub struct ChatGpt {
    redirect_uri: String,
    registration_path: PathBuf,
    registration: Mutex<Registration>,
    pending: Mutex<HashMap<String, Pending>>,
    sessions: Mutex<HashMap<String, VisitorSession>>,
}

#[derive(Debug, PartialEq)]
pub enum CallbackError {
    UnknownState,
    Denied,
    MissingCode,
    ClientMismatch,
    Exchange(String),
    PlanUsageNotGranted,
    InvalidIdToken(&'static str),
}

impl ChatGpt {
    pub fn new(port: u16, data_dir: PathBuf) -> std::io::Result<Self> {
        std::fs::create_dir_all(&data_dir)?;
        let registration_path = data_dir.join("chatgpt-registration.json");
        let registration = match std::fs::read_to_string(&registration_path) {
            Ok(text) => serde_json::from_str(&text).unwrap_or_default(),
            Err(_) => Registration::default(),
        };
        let registration = if registration.ext_agent_host_id.is_empty() {
            Registration {
                ext_agent_host_id: format!("orc-{}", uuid::Uuid::new_v4()),
                ..registration
            }
        } else {
            registration
        };
        let this = Self {
            redirect_uri: format!("http://127.0.0.1:{port}/auth/callback"),
            registration_path,
            registration: Mutex::new(registration),
            pending: Mutex::default(),
            sessions: Mutex::default(),
        };
        this.save_registration()?;
        Ok(this)
    }

    fn save_registration(&self) -> std::io::Result<()> {
        let text = serde_json::to_string_pretty(&*self.registration.lock().unwrap())?;
        let tmp = self.registration_path.with_extension("tmp");
        std::fs::write(&tmp, text)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
        }
        std::fs::rename(tmp, &self.registration_path)
    }

    /// Builds the authorize URL for a new sign-in attempt and remembers its PKCE verifier.
    pub fn start(&self) -> String {
        let state = random_token();
        let verifier = random_token() + &random_token();
        let nonce = random_token();
        let challenge = pkce_challenge(&verifier);
        {
            let mut pending = self.pending.lock().unwrap();
            pending.retain(|_, p| p.created.elapsed() < PENDING_TTL);
            if pending.len() >= MAX_PENDING {
                pending.clear();
            }
            pending.insert(
                state.clone(),
                Pending {
                    verifier,
                    nonce: nonce.clone(),
                    created: Instant::now(),
                },
            );
        }
        let reg = self.registration.lock().unwrap().clone();
        let mut params: Vec<(&str, String)> = vec![
            (
                "client_id",
                reg.client_id
                    .clone()
                    .unwrap_or_else(|| "dynamic_agent_client".into()),
            ),
            ("ext_agent_host_id", reg.ext_agent_host_id),
            ("response_type", "code".into()),
            ("redirect_uri", self.redirect_uri.clone()),
            ("scope", SCOPES.into()),
            ("resource", RESOURCE.into()),
            ("state", state),
            ("nonce", nonce),
            ("code_challenge_method", "S256".into()),
            ("code_challenge", challenge),
        ];
        if reg.client_id.is_none() {
            params.push(("agent_name_hint", "open-rust-chatbot".into()));
        }
        let query: Vec<String> = params
            .iter()
            .map(|(k, v)| format!("{k}={}", urlencode(v)))
            .collect();
        format!("{AUTHORIZE_URL}?{}", query.join("&"))
    }

    /// Validates the callback, exchanges the code, checks plan-usage consent, and returns a new
    /// visitor session id (to be set as an HttpOnly cookie).
    pub async fn finish(
        &self,
        http: &reqwest::Client,
        query: &HashMap<String, String>,
    ) -> Result<String, CallbackError> {
        let state = query.get("state").ok_or(CallbackError::UnknownState)?;
        let pending = self
            .pending
            .lock()
            .unwrap()
            .remove(state)
            .ok_or(CallbackError::UnknownState)?;
        if pending.created.elapsed() > PENDING_TTL {
            return Err(CallbackError::UnknownState);
        }
        if query.contains_key("error") {
            return Err(CallbackError::Denied);
        }
        let code = query.get("code").ok_or(CallbackError::MissingCode)?;
        let known_client = self.registration.lock().unwrap().client_id.clone();
        let client_id = match (known_client, query.get("client_id")) {
            (Some(known), Some(returned)) if &known != returned => {
                return Err(CallbackError::ClientMismatch)
            }
            (Some(known), _) => known,
            (None, Some(issued)) => issued.clone(),
            (None, None) => return Err(CallbackError::ClientMismatch),
        };

        let form = [
            ("grant_type", "authorization_code"),
            ("client_id", client_id.as_str()),
            ("code", code.as_str()),
            ("code_verifier", pending.verifier.as_str()),
            ("redirect_uri", self.redirect_uri.as_str()),
            ("resource", RESOURCE),
        ];
        let res = http
            .post(TOKEN_URL)
            .timeout(Duration::from_secs(20))
            .form(&form)
            .send()
            .await
            .map_err(|e| CallbackError::Exchange(e.without_url().to_string()))?;
        let status = res.status();
        let body: Value = res
            .json()
            .await
            .map_err(|e| CallbackError::Exchange(e.without_url().to_string()))?;
        if !status.is_success() {
            let code = body["error"].as_str().unwrap_or("unknown").to_owned();
            return Err(CallbackError::Exchange(format!(
                "HTTP {}: {code}",
                status.as_u16()
            )));
        }
        let session = parse_token_response(&body, &client_id, &pending.nonce)?;

        if self.registration.lock().unwrap().client_id.is_none() {
            self.registration.lock().unwrap().client_id = Some(client_id);
            let _ = self.save_registration();
        }
        let sid = random_token();
        let mut sessions = self.sessions.lock().unwrap();
        sessions.retain(|_, s| s.expires_at > Instant::now());
        if sessions.len() >= MAX_SESSIONS {
            return Err(CallbackError::Exchange(
                "too many signed-in visitors".into(),
            ));
        }
        sessions.insert(sid.clone(), session);
        Ok(sid)
    }

    pub fn access_token(&self, sid: &str) -> Option<String> {
        let sessions = self.sessions.lock().unwrap();
        sessions
            .get(sid)
            .filter(|s| s.expires_at > Instant::now() + Duration::from_secs(30))
            .map(|s| s.access_token.clone())
    }

    pub fn sign_out(&self, sid: &str) {
        self.sessions.lock().unwrap().remove(sid);
    }
}

/// Checks granted scopes and the ID token claims. The ID token comes straight from the token
/// endpoint over TLS, so (per OIDC Core 3.1.3.7) we validate its claims rather than its signature.
pub fn parse_token_response(
    body: &Value,
    client_id: &str,
    nonce: &str,
) -> Result<VisitorSession, CallbackError> {
    let scopes = body["scope"]
        .as_str()
        .map(|s| s.split(' ').map(str::to_owned).collect::<Vec<_>>())
        .or_else(|| {
            body["scopes"].as_array().map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(str::to_owned))
                    .collect()
            })
        });
    if !scopes.unwrap_or_default().iter().any(|s| s == PLAN_SCOPE) {
        return Err(CallbackError::PlanUsageNotGranted);
    }
    let access_token = body["access_token"]
        .as_str()
        .filter(|t| !t.is_empty())
        .ok_or(CallbackError::InvalidIdToken("no access token"))?;
    let id_token = body["id_token"]
        .as_str()
        .ok_or(CallbackError::InvalidIdToken("no id token"))?;
    let claims = decode_jwt_claims(id_token).ok_or(CallbackError::InvalidIdToken("malformed"))?;
    if claims["iss"] != ISSUER {
        return Err(CallbackError::InvalidIdToken("issuer"));
    }
    let aud_ok = claims["aud"] == client_id
        || claims["aud"]
            .as_array()
            .is_some_and(|a| a.iter().any(|v| v == client_id));
    if !aud_ok {
        return Err(CallbackError::InvalidIdToken("audience"));
    }
    if claims["nonce"] != nonce {
        return Err(CallbackError::InvalidIdToken("nonce"));
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    if claims["exp"].as_u64().is_none_or(|exp| exp <= now) {
        return Err(CallbackError::InvalidIdToken("expired"));
    }
    let expires_in = body["expires_in"].as_u64().unwrap_or(3600).min(24 * 3600);
    Ok(VisitorSession {
        access_token: access_token.to_owned(),
        expires_at: Instant::now() + Duration::from_secs(expires_in),
    })
}

fn decode_jwt_claims(jwt: &str) -> Option<Value> {
    let payload = jwt.split('.').nth(1)?;
    serde_json::from_slice(&URL_SAFE_NO_PAD.decode(payload.trim_end_matches('=')).ok()?).ok()
}

fn urlencode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn jwt(claims: Value) -> String {
        format!("e30.{}.sig", URL_SAFE_NO_PAD.encode(claims.to_string()))
    }

    fn future() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
            + 600
    }

    #[test]
    fn pkce_matches_rfc7636_example() {
        assert_eq!(
            pkce_challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }

    #[test]
    fn start_builds_a_first_time_registration_url_with_loopback_redirect() {
        let dir = std::env::temp_dir().join(format!("orc-test-{}", uuid::Uuid::new_v4()));
        let g = ChatGpt::new(8899, dir.clone()).unwrap();
        let url = g.start();
        assert!(url.starts_with(AUTHORIZE_URL));
        for part in [
            "client_id=dynamic_agent_client",
            "agent_name_hint=open-rust-chatbot",
            "redirect_uri=http%3A%2F%2F127.0.0.1%3A8899%2Fauth%2Fcallback",
            "code_challenge_method=S256",
            "chatgpt.tokens.use.direct",
            "resource=https%3A%2F%2Fapi.openai.com%2Fv1",
        ] {
            assert!(url.contains(part), "missing {part} in {url}");
        }
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn token_response_requires_plan_scope_and_valid_claims() {
        let good = json!({"iss": ISSUER, "aud": "oaiapp_1", "nonce": "n", "exp": future()});
        let body = |claims: Value, scope: &str| json!({"access_token": "at", "id_token": jwt(claims), "scope": scope, "expires_in": 3600});
        assert!(parse_token_response(&body(good.clone(), SCOPES), "oaiapp_1", "n").is_ok());
        assert_eq!(
            parse_token_response(&body(good.clone(), "openid email"), "oaiapp_1", "n").err(),
            Some(CallbackError::PlanUsageNotGranted)
        );
        assert_eq!(
            parse_token_response(&body(good.clone(), SCOPES), "oaiapp_2", "n").err(),
            Some(CallbackError::InvalidIdToken("audience"))
        );
        assert_eq!(
            parse_token_response(&body(good, SCOPES), "oaiapp_1", "other").err(),
            Some(CallbackError::InvalidIdToken("nonce"))
        );
        let expired = json!({"iss": ISSUER, "aud": "oaiapp_1", "nonce": "n", "exp": 1});
        assert_eq!(
            parse_token_response(&body(expired, SCOPES), "oaiapp_1", "n").err(),
            Some(CallbackError::InvalidIdToken("expired"))
        );
    }

    #[tokio::test]
    async fn callback_rejects_unknown_state_and_denied_consent() {
        let dir = std::env::temp_dir().join(format!("orc-test-{}", uuid::Uuid::new_v4()));
        let g = ChatGpt::new(8899, dir.clone()).unwrap();
        let http = reqwest::Client::new();
        let q = |pairs: &[(&str, &str)]| {
            pairs
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect::<HashMap<_, _>>()
        };
        assert_eq!(
            g.finish(&http, &q(&[("state", "forged"), ("code", "c")]))
                .await
                .err(),
            Some(CallbackError::UnknownState)
        );
        let url = g.start();
        let state = url
            .split("state=")
            .nth(1)
            .unwrap()
            .split('&')
            .next()
            .unwrap()
            .to_owned();
        assert_eq!(
            g.finish(&http, &q(&[("state", &state), ("error", "access_denied")]))
                .await
                .err(),
            Some(CallbackError::Denied)
        );
        // State is single-use.
        assert_eq!(
            g.finish(&http, &q(&[("state", &state), ("code", "c")]))
                .await
                .err(),
            Some(CallbackError::UnknownState)
        );
        std::fs::remove_dir_all(dir).ok();
    }
}
