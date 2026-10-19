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

/// Errors that mean the model is unreachable: network/transport failures, timeouts, 5xx, 429
/// and auth failures. Anything else (rejected input, tool loops, empty or filtered replies)
/// could be caused by a visitor and must not mark the chat as down.
pub fn is_availability_failure(detail: &str) -> bool {
    const OUTAGE_MARKERS: [&str; 7] = [
        "request failed",
        "stream read failed",
        "timed out",
        "stalled",
        "HTTP 5",
        "HTTP 429",
        "HTTP 401",
    ];
    OUTAGE_MARKERS.iter().any(|m| detail.contains(m))
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

/// Text-only models can't call tools, so they signal a call-back request with one hidden line.
const TEXT_ONLY_LEAD_INSTRUCTIONS: &str = "\n\nCall-back requests: when, and only when, the visitor has asked to be called or contacted and has given their name and a phone number or email, end your reply with one extra final line in exactly this form: LEAD: {\"name\":\"...\",\"phone\":\"...\",\"email\":\"...\",\"best_time\":\"...\",\"message\":\"...\"} using null for anything they did not give and only details the visitor actually wrote. The visitor will be shown their details to confirm. Never mention or explain this line.";

/// Splits a text-only reply into what the visitor sees and the optional `LEAD: {…}` arguments.
pub(crate) fn split_lead_line(raw: &str) -> (String, Option<serde_json::Map<String, Value>>) {
    let mut lead = None;
    let mut visible = Vec::new();
    for line in raw.lines() {
        if let Some(rest) = line.trim().strip_prefix("LEAD:") {
            if let Some(map) = serde_json::from_str::<Value>(rest.trim())
                .ok()
                .and_then(|v| v.as_object().cloned())
            {
                lead = Some(map);
            }
            continue; // never show the line, even if it doesn't parse
        }
        visible.push(line);
    }
    (visible.join("\n").trim().to_owned(), lead)
}

/// `{"type":"resume","token":"…"|null}`: the widget asking to continue (or start) a session.
fn parse_resume(text: &str) -> Option<Option<String>> {
    if !text.starts_with('{') {
        return None;
    }
    let v: Value = serde_json::from_str(text).ok()?;
    (v["type"] == "resume").then(|| v["token"].as_str().map(str::to_owned))
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
    /// Rate-limit key of the visitor (IP or /64), used by the availability breaker.
    client: String,
    /// Token under which this conversation is parked so the next page can resume it.
    resume_token: Option<String>,
    seen_first_message: bool,
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
            client: String::new(),
            resume_token: None,
            seen_first_message: false,
        }
    }

    pub fn with_client(mut self, client: String) -> Self {
        self.client = client;
        self
    }

    pub async fn handle_message(&mut self, text: &str, tx: &Sender) {
        let first = !self.seen_first_message;
        self.seen_first_message = true;
        if let Some(token) = parse_resume(text) {
            // Only honoured as a connection's first message, so a live chat can't be swapped.
            if first {
                self.resume(token.as_deref(), tx);
            }
            return;
        }
        self.handle_chat_message(text, tx).await;
        self.save();
    }

    fn park(&self) -> crate::ParkedChat {
        crate::ParkedChat {
            history: self.history.iter().skip(1).cloned().collect(),
            turns: self.turns,
            leads: self.pending_leads.values().cloned().collect(),
        }
    }

    fn save(&self) {
        if let Some(token) = &self.resume_token {
            self.shared.save_session(token, self.park());
        }
    }

    fn resume(&mut self, token: Option<&str>, tx: &Sender) {
        let restored = token
            .filter(|t| t.len() == 64 && t.bytes().all(|b| b.is_ascii_hexdigit()))
            .and_then(|t| self.shared.take_session(t));
        let resumed = restored.is_some();
        let mut leads = Vec::new();
        if let Some(chat) = restored {
            // Keep today's system prompt; restore only the conversation.
            self.history.truncate(1);
            self.history.extend(chat.history);
            self.turns = chat.turns;
            leads = chat.leads;
        }
        let token = crate::random_token();
        self.resume_token = Some(token.clone());
        for lead in leads {
            self.pending_leads.insert(lead.lead_id.clone(), lead);
        }
        self.save();
        send(
            tx,
            json!({ "type": "session", "token": token, "resumed": resumed, "turns": self.turns }),
        );
        // Leads awaiting confirmation are shown again so the visitor can still confirm them.
        for lead in self.pending_leads.values() {
            send(
                tx,
                json!({ "type": "card", "card": { "type": "lead_confirmation", "lead": lead } }),
            );
        }
    }

    async fn handle_chat_message(&mut self, text: &str, tx: &Sender) {
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
                json!({ "type": "error", "message": "This conversation has reached its limit. Please contact the business directly for anything else." }),
            );
            return;
        }
        self.turns += 1;
        let result = self.run_turn(text, tx).await;
        match &result {
            Ok(()) => self.shared.record_model_result(true, &self.client),
            // Only outages count, so a visitor can't hide the bot by sending rejected input.
            Err(detail) if is_availability_failure(detail) => {
                self.shared.record_model_result(false, &self.client)
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
        // Park the question straight away: a visitor who moves page mid-reply cancels this turn.
        self.save();
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
            if self.shared.config.lead_capture {
                system.content.push_str(TEXT_ONLY_LEAD_INSTRUCTIONS);
            }
        }
        // Text-only providers answer in one piece, so buffer it to take out the hidden lead line.
        let mut raw = String::new();
        let mut stream = self.shared.llm.stream(&messages, &[]).await?;
        while let Some(event) = stream.next().await {
            if let LlmEvent::Token(value) = event? {
                raw.push_str(&value);
            }
        }
        let (reply, lead_args) = split_lead_line(&raw);
        if !reply.is_empty() {
            send(tx, json!({ "type": "token", "value": reply }));
        }
        self.history.push(Message::new(Role::Assistant, reply));
        self.history = trim_history(&self.history, max_history);

        if let Some(arguments) = lead_args.filter(|_| self.shared.config.lead_capture) {
            let call = ToolCall {
                id: "text-lead".into(),
                name: "propose_lead".into(),
                arguments,
            };
            if let ToolOutcome::LeadProposed(lead) = tools::execute(&self.shared.kb, &call) {
                self.offer_lead(lead, tx);
            }
            send(tx, json!({ "type": "done" }));
            log_event(
                "turn_complete",
                json!({ "session": self.session_id, "hops": 0 }),
            );
            return Ok(());
        }

        // Fallback when the model didn't add a lead line: offer a card for an email in the text.
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
    fn lead_line_is_split_from_the_visible_reply() {
        let (visible, lead) =
            split_lead_line("Thanks!\nLEAD: {\"name\":\"Jo\",\"phone\":\"07700 900123\"}");
        assert_eq!(visible, "Thanks!");
        assert_eq!(lead.unwrap()["phone"], "07700 900123");
        let (visible, lead) = split_lead_line("Hi there\nLEAD: not json");
        assert_eq!(
            (visible.as_str(), lead.is_none()),
            ("Hi there", true),
            "a broken line is hidden, not shown"
        );
        let (visible, lead) = split_lead_line("No lead here.");
        assert_eq!((visible.as_str(), lead.is_none()), ("No lead here.", true));
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
        assert!(is_availability_failure(
            "bridge request failed: connection refused"
        ));
        assert!(is_availability_failure("timed out waiting for the model"));
        assert!(is_availability_failure("HTTP 401: invalid key"));
        assert!(!is_availability_failure("bridge HTTP 403"));
        assert!(!is_availability_failure("HTTP 400: bad request"));
        assert!(!is_availability_failure("too many tool hops"));
        assert!(!is_availability_failure("bridge returned no text"));
        assert!(!is_availability_failure("provider error: content filtered"));
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
