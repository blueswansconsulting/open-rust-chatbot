//! One visitor's conversation: history, the tool-calling loop, and lead confirmation.
//! All state lives on the connection, so nothing can leak between visitors.

use crate::llm::{LlmEvent, Message, Role};
use crate::log::log_event;
use crate::tools::{self, Lead, ToolCall, ToolOutcome};
use crate::Shared;
use futures_util::StreamExt;
use regex::Regex;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::{Arc, LazyLock};
use tokio::sync::mpsc::UnboundedSender;

const MAX_TOOL_HOPS: usize = 4;
const MAX_PENDING_LEADS: usize = 5;
pub const GENERIC_ERROR: &str = "Sorry, something went wrong. Please try again in a moment.";

/// Follow-ups like "how often do I need one?" only make sense with the previous question, so
/// knowledge-base search uses the visitor's last question as well as the current one.
fn retrieval_query(history: &[Message], current: &str) -> String {
    let previous = history
        .iter()
        .rev()
        .filter(|m| m.role == Role::User && m.content != current)
        .map(|m| m.content.as_str())
        .next()
        .unwrap_or_default();
    format!("{previous} {current}")
}

/// Errors that mean the model is unreachable (5xx, 429, timeouts, network, auth), as opposed to
/// a request the provider refused because of its content.
pub fn is_availability_failure(detail: &str) -> bool {
    const REQUEST_ERRORS: [&str; 5] = ["HTTP 400", "HTTP 403", "HTTP 404", "HTTP 413", "HTTP 422"];
    !REQUEST_ERRORS.iter().any(|code| detail.contains(code))
}

pub fn trim_history(history: &[Message], max_messages: usize) -> Vec<Message> {
    let Some((system, rest)) = history.split_first() else {
        return Vec::new();
    };
    let mut start = rest.len().saturating_sub(max_messages);
    // Never start mid tool-exchange: the provider rejects tool results without their call.
    while start < rest.len() && rest[start].role != Role::User {
        start += 1;
    }
    // ...but never drop the visitor's current question, however long its tool exchange is.
    if let Some(current) = rest.iter().rposition(|m| m.role == Role::User) {
        start = start.min(current);
    }
    std::iter::once(system.clone())
        .chain(rest[start..].iter().cloned())
        .collect()
}

type Sender = UnboundedSender<String>;

static EMAIL_IN_TEXT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"[A-Za-z0-9._%+-]+@[A-Za-z0-9.-]+\.[A-Za-z]{2,}").unwrap());

fn send(tx: &Sender, frame: Value) {
    let _ = tx.send(frame.to_string());
}

/// Inbound frames are either plain chat text or a JSON lead confirmation from the widget's button.
fn parse_confirmation(text: &str) -> Option<String> {
    if !text.starts_with('{') {
        return None;
    }
    let v: Value = serde_json::from_str(text).ok()?;
    (v["type"] == "confirm_lead")
        .then(|| {
            v["leadId"]
                .as_str()
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
        })
        .flatten()
}

pub struct ChatSession {
    shared: Arc<Shared>,
    session_id: String,
    history: Vec<Message>,
    pending_leads: HashMap<String, Lead>,
    turns: usize,
}

impl ChatSession {
    pub fn new(shared: Arc<Shared>, session_id: String) -> Self {
        let system = Message::new(Role::System, shared.config.system_prompt.clone());
        Self {
            shared,
            session_id,
            history: vec![system],
            pending_leads: HashMap::new(),
            turns: 0,
        }
    }

    pub async fn handle_message(&mut self, text: &str, tx: &Sender) {
        if let Some(lead_id) = parse_confirmation(text) {
            self.confirm_lead(&lead_id, tx).await;
            return;
        }
        let limits = &self.shared.config.limits;
        if text.trim().is_empty() {
            return;
        }
        if text.chars().count() > limits.max_message_chars {
            send(
                tx,
                json!({ "type": "error", "message": format!("Messages are limited to {} characters.", limits.max_message_chars) }),
            );
            return;
        }
        if self.turns >= limits.max_turns_per_connection {
            send(
                tx,
                json!({ "type": "error", "message": "This conversation has reached its limit. Please refresh to start a new one." }),
            );
            return;
        }
        self.turns += 1;
        let result = self.run_turn(text, tx).await;
        match &result {
            Ok(()) => self.shared.record_model_result(true),
            // Only outages count, so a visitor can't hide the bot by sending rejected input.
            Err(detail) if is_availability_failure(detail) => {
                self.shared.record_model_result(false)
            }
            Err(_) => {}
        }
        if let Err(detail) = result {
            log_event(
                "turn_failed",
                json!({ "session": self.session_id, "detail": detail }),
            );
            send(tx, json!({ "type": "error", "message": GENERIC_ERROR }));
        }
    }

    async fn run_turn(&mut self, text: &str, tx: &Sender) -> Result<(), String> {
        self.history.push(Message::new(Role::User, text));
        if !self.shared.llm.supports_tools() {
            return self.run_text_only_turn(text, tx).await;
        }
        let max_history = self.shared.config.limits.max_history_messages;
        for hop in 0..=MAX_TOOL_HOPS {
            let mut reply = String::new();
            let mut calls = Vec::new();
            let mut stream = self
                .shared
                .llm
                .stream(
                    &trim_history(&self.history, max_history),
                    &self.shared.tool_definitions,
                )
                .await?;
            while let Some(event) = stream.next().await {
                match event? {
                    LlmEvent::Token(value) => {
                        reply.push_str(&value);
                        send(tx, json!({ "type": "token", "value": value }));
                    }
                    LlmEvent::ToolCall(call) => calls.push(call),
                }
            }

            if calls.is_empty() {
                self.history.push(Message::new(Role::Assistant, reply));
                self.history = trim_history(&self.history, max_history);
                send(tx, json!({ "type": "done" }));
                log_event(
                    "turn_complete",
                    json!({ "session": self.session_id, "hops": hop }),
                );
                return Ok(());
            }
            if hop == MAX_TOOL_HOPS {
                return Err("too many tool hops".into());
            }

            let mut assistant = Message::new(Role::Assistant, reply);
            assistant.tool_calls = calls.clone();
            self.history.push(assistant);
            for call in &calls {
                self.run_tool(call, tx);
            }
        }
        Ok(())
    }

    /// For providers without tool calling: the server retrieves knowledge-base context itself and
    /// offers the lead card when the visitor types an email address.
    async fn run_text_only_turn(&mut self, text: &str, tx: &Sender) -> Result<(), String> {
        let max_history = self.shared.config.limits.max_history_messages;
        let mut messages = trim_history(&self.history, max_history);
        let excerpts: Vec<String> = self
            .shared
            .kb
            .search(&retrieval_query(&self.history, text), 3)
            .iter()
            .map(|c| format!("[{}] {}", c.header_path.join(" > "), c.text))
            .collect();
        if let Some(system) = messages.first_mut() {
            system.content.push_str(if excerpts.is_empty() {
                "\n\nNo knowledge-base excerpt matched this message; do not guess facts about the business."
            } else {
                "\n\nKnowledge-base excerpts (the only facts you may state about the business):\n"
            });
            system.content.push_str(&excerpts.join("\n"));
        }
        let mut reply = String::new();
        let mut stream = self.shared.llm.stream(&messages, &[]).await?;
        while let Some(event) = stream.next().await {
            if let LlmEvent::Token(value) = event? {
                reply.push_str(&value);
                send(tx, json!({ "type": "token", "value": value }));
            }
        }
        self.history.push(Message::new(Role::Assistant, reply));
        self.history = trim_history(&self.history, max_history);

        let email = EMAIL_IN_TEXT
            .find(text)
            .filter(|_| self.shared.config.lead_capture);
        if let Some(email) = email {
            let mut arguments = serde_json::Map::new();
            arguments.insert("name".into(), json!("Website visitor"));
            arguments.insert("email".into(), json!(email.as_str()));
            arguments.insert("phone".into(), Value::Null);
            arguments.insert("message".into(), json!(text));
            let call = ToolCall {
                id: "server-lead".into(),
                name: "propose_lead".into(),
                arguments,
            };
            if let ToolOutcome::LeadProposed(lead) = tools::execute(&self.shared.kb, &call) {
                self.offer_lead(lead, tx);
            }
        }
        send(tx, json!({ "type": "done" }));
        log_event(
            "turn_complete",
            json!({ "session": self.session_id, "hops": 0 }),
        );
        Ok(())
    }

    /// Shows the confirmation card. Returns false if too many cards are already waiting.
    fn offer_lead(&mut self, lead: Lead, tx: &Sender) -> bool {
        if self.pending_leads.len() >= MAX_PENDING_LEADS {
            return false;
        }
        send(
            tx,
            json!({ "type": "card", "card": { "type": "lead_confirmation", "lead": lead } }),
        );
        self.pending_leads.insert(lead.lead_id.clone(), lead);
        true
    }

    fn run_tool(&mut self, call: &ToolCall, tx: &Sender) {
        send(tx, json!({ "type": "tool_start", "name": call.name }));
        log_event(
            "tool_call",
            json!({ "session": self.session_id, "tool": call.name }),
        );
        let offered = self
            .shared
            .tool_definitions
            .iter()
            .any(|t| t.name == call.name);
        let outcome = if offered {
            tools::execute(&self.shared.kb, call)
        } else {
            ToolOutcome::Result(json!({ "error": format!("unknown tool {}", call.name) }))
        };
        let content = match outcome {
            ToolOutcome::Result(v) => v,
            // The model never learns it can confirm; only the visitor's button can.
            ToolOutcome::LeadProposed(lead) => match self.offer_lead(lead, tx) {
                true => json!({ "status": "awaiting_visitor_confirmation" }),
                false => {
                    json!({ "status": "too_many_pending", "message": "Ask the visitor to confirm the card already shown." })
                }
            },
        };
        let mut msg = Message::new(Role::Tool, content.to_string());
        msg.tool_call_id = Some(call.id.clone());
        self.history.push(msg);
    }

    async fn confirm_lead(&mut self, lead_id: &str, tx: &Sender) {
        let Some(lead) = self.pending_leads.remove(lead_id) else {
            send(
                tx,
                json!({ "type": "error", "message": "That request has expired. Please share your details again." }),
            );
            send(tx, json!({ "type": "done" }));
            return;
        };
        let config = &self.shared.config;
        let delivered = match (&config.lead_webhook_url, &config.lead_webhook_secret) {
            (Some(url), Some(secret)) => {
                let body = crate::webhook::payload(&lead, &config.bot_name);
                crate::webhook::deliver(&self.shared.http, url, secret, body).await
            }
            // No webhook configured: record that a lead arrived (PII redacted) so local demos work.
            _ => Ok(()),
        };
        match delivered {
            Ok(()) => {
                log_event(
                    "lead_submitted",
                    json!({ "session": self.session_id, "email": lead.email }),
                );
                send(
                    tx,
                    json!({ "type": "card", "card": { "type": "lead_submitted", "lead": lead } }),
                );
                const THANKS: &str = "Thanks! The team has your details and will be in touch.";
                self.history.push(Message::new(Role::Assistant, THANKS));
                send(tx, json!({ "type": "token", "value": THANKS }));
            }
            Err(detail) => {
                log_event(
                    "lead_failed",
                    json!({ "session": self.session_id, "detail": detail }),
                );
                self.pending_leads.insert(lead.lead_id.clone(), lead);
                send(
                    tx,
                    json!({ "type": "error", "message": "We couldn't send your details just now. Please try confirming again." }),
                );
            }
        }
        send(tx, json!({ "type": "done" }));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trim_keeps_system_and_starts_at_user() {
        let h = vec![
            Message::new(Role::System, "s"),
            Message::new(Role::User, "u1"),
            Message::new(Role::Assistant, "a1"),
            Message::new(Role::Tool, "t"),
            Message::new(Role::User, "u2"),
        ];
        let out: Vec<_> = trim_history(&h, 3).into_iter().map(|m| m.content).collect();
        assert_eq!(out, vec!["s", "u2"]);
    }

    #[test]
    fn trim_never_drops_the_current_question_during_long_tool_exchanges() {
        let mut h = vec![
            Message::new(Role::System, "s"),
            Message::new(Role::User, "q"),
        ];
        for _ in 0..13 {
            h.push(Message::new(Role::Tool, "t"));
        }
        let out = trim_history(&h, 12);
        assert_eq!(out[1].content, "q");
        assert_eq!(out.len(), 15);
    }

    #[test]
    fn retrieval_query_includes_the_previous_question() {
        let h = vec![
            Message::new(Role::System, "s"),
            Message::new(Role::User, "I need an EICR in Croydon"),
            Message::new(Role::Assistant, "ok"),
            Message::new(Role::User, "How often do I need one?"),
        ];
        assert_eq!(
            retrieval_query(&h, "How often do I need one?"),
            "I need an EICR in Croydon How often do I need one?"
        );
    }

    #[test]
    fn only_outages_count_towards_the_breaker() {
        assert!(is_availability_failure("bridge HTTP 503"));
        assert!(is_availability_failure("timed out waiting for the model"));
        assert!(is_availability_failure("HTTP 401: invalid key"));
        assert!(!is_availability_failure("bridge HTTP 403"));
        assert!(!is_availability_failure("HTTP 400: bad request"));
    }

    #[test]
    fn only_well_formed_confirmations_are_treated_as_confirmations() {
        assert_eq!(
            parse_confirmation(r#"{"type":"confirm_lead","leadId":"abc"}"#),
            Some("abc".into())
        );
        assert_eq!(
            parse_confirmation(r#"{"type":"confirm_lead","leadId":""}"#),
            None
        );
        assert_eq!(parse_confirmation("confirm_lead abc"), None);
    }
}
