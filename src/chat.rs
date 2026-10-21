//! One visitor's conversation: history, the tool-calling loop, and lead confirmation.
//! All state lives on the connection, so nothing can leak between visitors.

use crate::llm::{LlmEvent, Message, Role};
use crate::log::log_event;
use crate::sites::{Site, SiteError};
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
/// The visitor's previous message, used as retrieval context so a follow-up such as "how often
/// do I need one?" still finds its topic.
fn previous_question<'a>(history: &'a [Message], current: &str) -> &'a str {
    history
        .iter()
        .rev()
        .filter(|m| m.role == Role::User && m.content != current)
        .map(|m| m.content.as_str())
        .next()
        .unwrap_or_default()
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
/// Something that may be a phone number the visitor typed, in any common format ("07700
/// 900123", "07700-900-123", "(07700) 900123", "+44 (0)7700 900123"); [`phone_digits`] decides.
static PHONE_IN_TEXT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\+?\(?\d[\d \t().-]{7,22}\d").unwrap());
/// The visitor introducing themselves ("I'm Sam Test", "my name is Jo", "this is Priya Shah"):
/// the introduction, then up to three capitalised words. What follows is checked in code.
static NAME_IN_TEXT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i:\b(?:my name is|i['\u{2019}]m|i am|this is)\s+)(\p{Lu}[\p{L}'\u{2019}-]*(?:[ \t]+\p{Lu}[\p{L}'\u{2019}-]*){0,2})").unwrap()
});
/// Capitalised words after "I'm" that are not a name ("I'm Looking for…", "I'm Sorry").
const NOT_A_NAME: &str =
    "monday tuesday wednesday thursday friday saturday sunday january february \
march april may june july august september october november december ready very also unsure \
urgent sorry looking gas safe here interested after just not based in from wondering hoping trying \
sure the a an available back home away out on at with";
/// The hidden lead marker: `LEAD:`, any case, `LEAD :`, or bold `**LEAD:**`.
static LEAD_MARKER: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)(?:\*{1,2})?\bLEAD(?:\*{1,2})?\s*:(?:\*{1,2})?\s*").unwrap());
/// A phone or email in the visitor's message, with the "on"/"at" before it, for the card text.
static CONTACT_IN_MESSAGE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i:\b(?:on|at|via|is)\s+)?(?:\+?\(?\d[\d \t().-]{7,22}\d|[A-Za-z0-9._%+-]+@[A-Za-z0-9.-]+\.[A-Za-z]{2,})").unwrap()
});
/// The visitor asking to be called back, so a phone number they typed is a lead.
static CALL_ME: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b(?:call|ring|phone|text|contact)\s+(?:me|us)\b|\bcall[- ]?back\b|\bget (?:back )?in touch\b").unwrap()
});

fn send(tx: &Sender, frame: Value) {
    let _ = tx.send(frame.to_string());
}

/// Text-only models can't call tools, so they signal a call-back request with one hidden line.
const TEXT_ONLY_LEAD_INSTRUCTIONS: &str = "\n\nCall-back requests: when, and only when, the visitor has asked to be called or contacted and has given a phone number or email (a name too if they gave one), end your reply with one extra final line in exactly this form: LEAD: {\"name\":\"...\",\"phone\":\"...\",\"email\":\"...\",\"best_time\":\"...\",\"message\":\"...\"} using null for anything they did not give and only details the visitor actually wrote. A phone number alone is enough: do not ask for an email as well. The visitor will be shown their details to confirm. Never mention or explain this line.";
/// Directory bots: leads go to the directory's own team, not to the listed businesses.
const TEXT_ONLY_DIRECTORY_LEADS: &str = " This site is a directory: a call-back request goes to the directory team, who follow up themselves. Say so, and never say a listed business has been told, will call, or has been booked.";

/// Contact details a visitor typed: an email, or a phone number when they ask to be called.
fn fallback_contact(text: &str) -> Option<(Option<String>, Option<String>)> {
    let email = EMAIL_IN_TEXT.find(text).map(|m| m.as_str().to_owned());
    let phone = typed_phones(text)
        .into_iter()
        .next()
        .filter(|_| CALL_ME.is_match(text))
        .map(|(typed, digits)| {
            // As typed when the card accepts it ("(07700) 900123"), else plain digits.
            if typed
                .chars()
                .all(|c| c.is_ascii_digit() || " ()+-".contains(c))
            {
                typed
            } else {
                digits
            }
        });
    (email.is_some() || phone.is_some()).then_some((email, phone))
}

/// UK phone numbers in `text`: (as typed, digits with a leading 0).
fn typed_phones(text: &str) -> Vec<(String, String)> {
    PHONE_IN_TEXT
        .find_iter(text)
        .filter_map(|m| {
            let typed = m.as_str().trim().to_owned();
            phone_digits(&typed).map(|digits| (typed, digits))
        })
        .collect()
}

/// The visitor's own name and where its introduction is in `text`: "I'm Sam Test (Claude QA),
/// 07700 900123" gives "Sam Test". The name is one to three capitalised words, followed by a
/// comma, full stop, "and", a bracket, digits or the end; never a weekday, "Sorry", "Looking"….
fn introduced_name(text: &str) -> Option<(String, std::ops::Range<usize>)> {
    let not_a_name: std::collections::HashSet<&str> = NOT_A_NAME.split_whitespace().collect();
    for c in NAME_IN_TEXT.captures_iter(text) {
        let (whole, name) = (c.get(0)?, c.get(1)?);
        // Try the longest run of capitalised words first, then shorter ones.
        let words: Vec<(usize, usize)> = name
            .as_str()
            .split_whitespace()
            .scan(0, |from, w| {
                let at = name.as_str()[*from..].find(w)? + *from;
                *from = at + w.len();
                Some((name.start() + at, name.start() + at + w.len()))
            })
            .collect();
        for n in (1..=words.len()).rev() {
            let end = words[n - 1].1;
            let after = text[end..].trim_start();
            let follows = after.is_empty()
                || after.starts_with([',', '.', '(', '!'])
                || after.starts_with(|c: char| c.is_ascii_digit() || c == '+')
                || after.to_lowercase().starts_with("and ");
            let candidate = &text[words[0].0..end];
            let stop = candidate
                .split_whitespace()
                .any(|w| not_a_name.contains(w.to_lowercase().as_str()));
            if follows && !stop {
                let name = candidate
                    .trim_end_matches(['\'', '\u{2019}', '-'])
                    .to_owned();
                return Some((name, whole.start()..end));
            }
        }
    }
    None
}

fn name_in_text(text: &str) -> Option<String> {
    introduced_name(text).map(|(name, _)| name)
}

/// The visitor's message for the card, without the contact details and (when a name was taken
/// from it) the self-introduction it repeats ("Can someone call me about a rewire in Bolton?").
fn lead_message(text: &str) -> String {
    let mut out = text.to_owned();
    if let Some((_, range)) = introduced_name(&out) {
        // Drop the introduction and a bracketed aside right after it ("(Claude QA)").
        let mut end = range.end;
        let rest = &out[end..];
        if let Some(stripped) = rest.trim_start().strip_prefix('(') {
            if let Some(close) = stripped.find(')') {
                end += rest.len() - stripped.len() + close + 1;
            }
        }
        out.replace_range(range.start..end, " ");
    }
    out = CONTACT_IN_MESSAGE.replace_all(&out, " ").into_owned();
    let joined = out.split_whitespace().collect::<Vec<_>>().join(" ");
    // Rejoin, dropping spaces before punctuation and punctuation left dangling after other
    // punctuation ("Bolton? , ." -> "Bolton?").
    let mut cleaned = String::new();
    for c in joined.chars() {
        let last = cleaned.trim_end().chars().last();
        if ",;:.".contains(c) && (last.is_none() || last.is_some_and(|l| ".?!,;:".contains(l))) {
            continue;
        }
        if ",;:.?!".contains(c) && cleaned.ends_with(' ') {
            cleaned.pop();
        }
        cleaned.push(c);
    }
    // Words left hanging where a number was ("call me on", "my number is").
    const DANGLING: [&str; 12] = [
        "on", "at", "via", "is", "number", "mobile", "phone", "email", "tel", "my", "me:", "and",
    ];
    let mut message = cleaned
        .trim_matches(|c: char| ",;:-".contains(c) || c.is_whitespace())
        .to_owned();
    loop {
        let trimmed = message.trim_end_matches(|c: char| ",;:-".contains(c) || c.is_whitespace());
        let Some((head, last)) = trimmed.rsplit_once(' ').or(Some(("", trimmed))) else {
            break;
        };
        if !DANGLING.contains(&last.to_lowercase().as_str()) || last.is_empty() {
            message = trimmed.to_owned();
            break;
        }
        message = head.to_owned();
    }
    let message: String = message.chars().take(300).collect();
    if message.trim().is_empty() {
        "Asked to be called back".into()
    } else {
        message
    }
}

/// Where a JSON value that may be cut off ends: just after its matching `}`, or the end of
/// `text`.
fn json_extent(text: &str) -> usize {
    let (mut depth, mut in_string, mut escaped) = (0usize, false, false);
    for (i, c) in text.char_indices() {
        if in_string {
            match c {
                _ if escaped => escaped = false,
                '\\' => escaped = true,
                '"' => in_string = false,
                _ => {}
            }
            continue;
        }
        match c {
            '"' => in_string = true,
            '{' => depth += 1,
            '}' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return i + 1;
                }
            }
            _ => {}
        }
    }
    text.len()
}

/// Splits a text-only reply into what the visitor sees and the `LEAD: {…}` arguments (the
/// first one, if several). The marker may be any case, spaced or bold (`**LEAD:**`), on its
/// own line or mid-line, with a pretty-printed or fenced object. It only counts when a `{`
/// follows, so "Typical lead: 2 weeks" or "MISLEAD:" is left alone. From the marker to the end
/// of its JSON (and any objects straight after it) is never shown; JSON that doesn't parse is
/// hidden up to its closing `}` or the end of the reply.
pub(crate) fn split_lead_line(raw: &str) -> (String, Option<serde_json::Map<String, Value>>) {
    let mut lead = None;
    let mut visible = String::new();
    let mut rest = raw;
    loop {
        // The next marker that is followed by an object.
        let found = LEAD_MARKER.find_iter(rest).find_map(|m| {
            let after = &rest[m.end()..];
            let body = after.trim_start_matches(|c: char| c.is_whitespace() || c == '`');
            let body = body.strip_prefix("json").map_or(body, str::trim_start);
            // An object, or nothing at all (a bare marker ending the reply).
            (body.starts_with('{') || body.trim().is_empty())
                .then(|| (m.start(), rest.len() - body.len()))
        });
        let Some((at, body_at)) = found else {
            break;
        };
        // An opening code fence just before the marker belongs to it.
        let before = rest[..at].trim_end();
        let before = before
            .strip_suffix("```json")
            .or_else(|| before.strip_suffix("```"))
            .unwrap_or(before);
        visible.push_str(before);
        let mut body = &rest[body_at..];
        let mut first = true;
        while body.starts_with('{') {
            let mut values = serde_json::Deserializer::from_str(body).into_iter::<Value>();
            match values.next() {
                Some(Ok(value)) => {
                    if let (true, Value::Object(map)) = (first && lead.is_none(), value) {
                        lead = Some(map);
                    }
                    body = &body[values.byte_offset()..];
                }
                // Cut off or broken: hide it up to its closing brace or the end.
                _ => body = &body[json_extent(body)..],
            }
            first = false;
            // Objects straight after it go with it.
            body = body.trim_start();
        }
        // A closing fence and punctuation after the JSON go with it.
        rest =
            body.trim_start_matches(|c: char| c.is_whitespace() || c == '`' || ".,;!".contains(c));
        if !rest.is_empty() && !visible.is_empty() {
            visible.push('\n');
        }
    }
    visible.push_str(rest);
    (visible.trim().to_owned(), lead)
}

/// The digits of a UK phone number with a leading 0 ("+44 (0)7700 900123", "0044 7700
/// 900123" and "07700-900-123" all give "07700900123"), or None if it isn't one.
fn phone_digits(phone: &str) -> Option<String> {
    let mut digits: String = phone.chars().filter(char::is_ascii_digit).collect();
    if let Some(rest) = digits.strip_prefix("00") {
        digits = rest.to_owned();
    }
    if let Some(rest) = digits.strip_prefix("44") {
        // "+44 (0)7700…" keeps its 0; "+44 7700…" gets one.
        digits = if rest.starts_with('0') {
            rest.to_owned()
        } else {
            format!("0{rest}")
        };
    }
    (digits.starts_with('0') && (10..=11).contains(&digits.len())).then_some(digits)
}

/// Keeps only the phone and email in a model's lead that the visitor actually wrote in this
/// conversation, so a listed business's number in the prompt can't become the visitor's.
/// Returns false if no contact detail is left.
fn keep_visitors_contacts(
    lead: &mut serde_json::Map<String, Value>,
    visitor_texts: &[&str],
) -> bool {
    let phones: Vec<String> = visitor_texts
        .iter()
        .flat_map(|t| typed_phones(t).into_iter().map(|(_, digits)| digits))
        .collect();
    let emails: Vec<String> = visitor_texts
        .iter()
        .flat_map(|t| {
            EMAIL_IN_TEXT
                .find_iter(t)
                .map(|m| m.as_str().trim().to_lowercase())
        })
        .collect();
    let phone_ok = lead
        .get("phone")
        .and_then(Value::as_str)
        .and_then(phone_digits)
        .is_some_and(|d| phones.contains(&d));
    if !phone_ok {
        lead.insert("phone".into(), Value::Null);
    }
    let email_ok = lead
        .get("email")
        .and_then(Value::as_str)
        .map(|e| e.trim().to_lowercase())
        .is_some_and(|e| emails.contains(&e));
    if !email_ok {
        lead.insert("email".into(), Value::Null);
    }
    phone_ok || email_ok
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
    /// The bot this conversation is with (refreshed each message in multi-site mode).
    site: Arc<Site>,
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
        let site = shared.default_site.clone();
        let system = Message::new(Role::System, site.system_prompt.clone());
        Self {
            shared,
            site,
            session_id,
            history: vec![system],
            pending_leads: HashMap::new(),
            turns: 0,
            client: String::new(),
            resume_token: None,
            seen_first_message: false,
        }
    }

    pub fn with_site(mut self, site: Arc<Site>) -> Self {
        self.history[0].content = site.system_prompt.clone();
        self.site = site;
        self
    }

    /// Picks up the bot's latest settings, so edits apply mid-chat and a paused or deleted bot
    /// stops answering. Returns false if the bot is gone. While the sites API is down the
    /// current settings are kept.
    async fn refresh_site(&mut self) -> bool {
        let Some(store) = self
            .shared
            .sites
            .as_ref()
            .filter(|_| !self.site.id.is_empty())
        else {
            return true;
        };
        match store.get(&self.site.id).await {
            Ok(site) => {
                if !Arc::ptr_eq(&site, &self.site) {
                    self.history[0].content = site.system_prompt.clone();
                    // Lead cards from a bot that no longer captures leads can't be confirmed.
                    if !site.lead_capture {
                        self.pending_leads.clear();
                    }
                    self.site = site;
                }
                true
            }
            Err(SiteError::NotFound) => false,
            Err(SiteError::Unavailable) => true,
        }
    }

    pub fn with_client(mut self, client: String) -> Self {
        self.client = client;
        self
    }

    pub async fn handle_message(&mut self, text: &str, tx: &Sender) {
        let first = !self.seen_first_message;
        self.seen_first_message = true;
        if !self.refresh_site().await {
            send(
                tx,
                json!({ "type": "error", "message": "Sorry, this chat is no longer available." }),
            );
            return;
        }
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
            site: self.site.id.clone(),
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
            .and_then(|t| self.shared.take_session_for(t, &self.site.id));
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
        // Cards from before lead capture was turned off can no longer be confirmed.
        for lead in leads.into_iter().filter(|_| self.site.lead_capture) {
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
        let now = crate::usage::unix_now();
        if !self.shared.meter.admit(&self.site, &self.client, now) {
            log_event(
                "turn_capped",
                json!({ "session": self.session_id, "bot": self.site.id }),
            );
            send(
                tx,
                json!({ "type": "error", "code": "budget_exceeded", "message": "Sorry, this chat is busy right now. Please try again later or contact the business directly." }),
            );
            return;
        }
        self.turns += 1;
        let result = self.run_turn(text, tx).await;
        let reply_chars = match (&result, self.history.last()) {
            (Ok(()), Some(m)) if m.role == Role::Assistant => m.content.len(),
            _ => 0,
        };
        self.shared
            .meter
            .record(&self.site, result.is_ok(), text.len(), reply_chars);
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
                    &self.site.tool_definitions,
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
        let previous = previous_question(&self.history, text);
        let excerpts: Vec<String> = self
            .site
            .kb
            .retrieve_for_turn(text, previous)
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
            // Directory bots: the matching listings, looked up in code (no tools here).
            if let Some(listings) = &self.site.listings {
                system.content.push_str(&listings.block(text, previous));
            }
            if self.site.lead_capture {
                system.content.push_str(TEXT_ONLY_LEAD_INSTRUCTIONS);
                if self.site.listings.is_some() {
                    system.content.push_str(TEXT_ONLY_DIRECTORY_LEADS);
                }
            }
        }
        // Buffer the whole reply before sending anything, so the hidden lead line can never be
        // streamed to the widget: it may come anywhere, even mid-line.
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

        let visitor_texts: Vec<&str> = self
            .history
            .iter()
            .filter(|m| m.role == Role::User)
            .map(|m| m.content.as_str())
            .chain(std::iter::once(text))
            .collect();
        let lead_args = lead_args.and_then(|mut lead| {
            if keep_visitors_contacts(&mut lead, &visitor_texts) {
                Some(lead)
            } else {
                log_event(
                    "lead_line_dropped",
                    json!({ "session": self.session_id, "reason": "contact not typed by the visitor" }),
                );
                None
            }
        });
        if let Some(mut arguments) = lead_args.filter(|_| self.site.lead_capture) {
            // A phone number or email is enough; the name is optional for text-only models.
            let named = arguments
                .get("name")
                .and_then(Value::as_str)
                .is_some_and(|n| !n.trim().is_empty());
            if !named {
                arguments.insert("name".into(), json!("Website visitor"));
            }
            let call = ToolCall {
                id: "text-lead".into(),
                name: "propose_lead".into(),
                arguments,
            };
            if let ToolOutcome::LeadProposed(lead) = tools::execute(&self.site.kb, None, &call) {
                // The model's lead is the card: no second card from the fallback below.
                self.offer_lead(lead, tx);
                send(tx, json!({ "type": "done" }));
                log_event(
                    "turn_complete",
                    json!({ "session": self.session_id, "hops": 0 }),
                );
                return Ok(());
            }
            // An invalid lead line (no usable phone or email) falls back to what the visitor typed.
        }

        // Fallback when the model didn't add a lead line: offer a card for an email in the text,
        // or for a phone number when the visitor asks to be called.
        if let Some((email, phone)) = self
            .site
            .lead_capture
            .then(|| fallback_contact(text))
            .flatten()
        {
            let mut arguments = serde_json::Map::new();
            let name = name_in_text(text).unwrap_or_else(|| "Website visitor".into());
            arguments.insert("name".into(), json!(name));
            arguments.insert("email".into(), email.map_or(Value::Null, |e| json!(e)));
            arguments.insert("phone".into(), phone.map_or(Value::Null, |p| json!(p)));
            arguments.insert("message".into(), json!(lead_message(text)));
            let call = ToolCall {
                id: "server-lead".into(),
                name: "propose_lead".into(),
                arguments,
            };
            if let ToolOutcome::LeadProposed(lead) = tools::execute(&self.site.kb, None, &call) {
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
            .site
            .tool_definitions
            .iter()
            .any(|t| t.name == call.name);
        let outcome = if offered {
            tools::execute(&self.site.kb, self.site.listings.as_deref(), call)
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
        let delivered = match (
            &self.site.lead_webhook_url,
            &self.shared.config.lead_webhook_secret,
        ) {
            (Some(url), Some(secret)) => {
                let body = crate::webhook::payload(&lead, &self.site.bot_name, &self.site.id);
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
        // No object after the marker: ordinary prose, shown as written.
        let (visible, lead) = split_lead_line("Hi there\nLEAD: not json");
        assert_eq!(
            (visible.as_str(), lead.is_none()),
            ("Hi there\nLEAD: not json", true)
        );
        let (visible, lead) = split_lead_line("No lead here.");
        assert_eq!((visible.as_str(), lead.is_none()), ("No lead here.", true));
    }

    /// The reply seen in production, with the lead on the same line as the sentence.
    const LIVE_REPLY: &str = "The directory team can follow up about your rewire request in Bolton. LEAD: {\"name\":\"Sam Test (Claude QA)\",\"phone\":\"07700 900123\",\"email\":null,\"best_time\":null,\"message\":\"Rewire in Bolton\"}";

    #[test]
    fn a_lead_on_the_same_line_is_parsed_and_hidden() {
        let (visible, lead) = split_lead_line(LIVE_REPLY);
        assert_eq!(
            visible,
            "The directory team can follow up about your rewire request in Bolton."
        );
        let lead = lead.expect("parsed");
        assert_eq!(lead["name"], "Sam Test (Claude QA)");
        assert_eq!(lead["phone"], "07700 900123");
    }

    #[test]
    fn lead_lines_in_other_shapes_are_parsed_and_hidden() {
        let json = r#"{"name":"Jo","phone":"07700 900123","email":null}"#;
        for raw in [
            format!("Thanks Jo.\nLEAD: {json}"),
            format!("Thanks Jo. LEAD: {json}."),
            format!("Thanks Jo.\n\nLEAD:{json}\n"),
            format!("Thanks Jo.\n```json\nLEAD: {json}\n```"),
            format!("Thanks Jo.\n```\nLEAD: {json}\n```\n"),
            format!("Thanks Jo.\nLEAD: ```json\n{json}\n```"),
            "Thanks Jo.\nLEAD: {\n  \"name\": \"Jo\",\n  \"phone\": \"07700 900123\"\n}".to_owned(),
        ] {
            let (visible, lead) = split_lead_line(&raw);
            assert_eq!(visible, "Thanks Jo.", "{raw:?}");
            assert_eq!(lead.expect("parsed")["phone"], "07700 900123", "{raw:?}");
        }
        // Text after the lead is still shown; a broken lead mid-line hides only that line's rest.
        let (visible, lead) = split_lead_line(&format!("Thanks. LEAD: {json} Speak soon!"));
        assert_eq!(
            (visible.as_str(), lead.is_some()),
            ("Thanks.\nSpeak soon!", true)
        );
        // Broken JSON with no closing brace: hidden to the end of the reply.
        let (visible, lead) = split_lead_line("Thanks. LEAD: {broken\nAnything else?");
        assert_eq!((visible.as_str(), lead.is_none()), ("Thanks.", true));
        assert!(!split_lead_line("ok LEAD:").0.contains("LEAD"));
    }

    #[test]
    fn marker_variants_are_hidden_and_prose_is_left_alone() {
        // QA M1.
        let json = r#"{"name":"Jo","phone":"07700 900123"}"#;
        for marker in [
            "lead:",
            "Lead:",
            "LEAD :",
            "**LEAD:**",
            "**LEAD**:",
            "*Lead:*",
            "LEAD:",
        ] {
            for raw in [
                format!("Thanks Jo. {marker} {json}"),
                format!("Thanks Jo.\n{marker} {json}"),
            ] {
                let (visible, lead) = split_lead_line(&raw);
                assert_eq!(visible, "Thanks Jo.", "{raw:?}");
                assert_eq!(lead.expect("parsed")["phone"], "07700 900123", "{raw:?}");
            }
        }
        for prose in [
            "Typical lead: 2 weeks from survey to install.",
            "MISLEAD: no.",
            "Lead time: about a fortnight",
        ] {
            let (visible, lead) = split_lead_line(prose);
            assert_eq!((visible.as_str(), lead.is_none()), (prose, true), "{prose}");
        }
    }

    #[test]
    fn cut_off_json_after_the_marker_is_never_shown() {
        // QA M2.
        let (visible, lead) =
            split_lead_line("Thanks Jo.\nLEAD: {\n  \"name\": \"Jo\",\n  \"phone\": \"07700 9001");
        assert_eq!((visible.as_str(), lead.is_none()), ("Thanks Jo.", true));
        // Broken but closed: hidden up to the matching brace, the rest is shown.
        let (visible, lead) =
            split_lead_line("Thanks. LEAD: {\"name\": \"Jo\" \"phone\": \"x {}\"} See you soon.");
        assert_eq!(
            (visible.as_str(), lead.is_none()),
            ("Thanks.\nSee you soon.", true)
        );
    }

    #[test]
    fn the_first_lead_is_kept_and_back_to_back_objects_are_hidden() {
        let (visible, lead) = split_lead_line(
            r#"Thanks. LEAD: {"name":"First","phone":"07700 900123"} {"name":"Second"} LEAD: {"name":"Third"}"#,
        );
        assert_eq!(visible, "Thanks.");
        assert_eq!(lead.unwrap()["name"], "First");
    }

    #[test]
    fn a_lead_keeps_only_contact_details_the_visitor_typed() {
        // QA M4: a listed business's number from the prompt is not the visitor's.
        let mut lead = serde_json::Map::new();
        lead.insert("phone".into(), json!("01204 579975"));
        lead.insert("email".into(), json!("info@hayvern.example"));
        assert!(!keep_visitors_contacts(
            &mut lead,
            &["Can Hayvern call me? I'm Sam"]
        ));
        assert_eq!(
            (lead["phone"].is_null(), lead["email"].is_null()),
            (true, true)
        );
        // Typed differently is still the visitor's: +44, spaces, upper case.
        let mut lead = serde_json::Map::new();
        lead.insert("phone".into(), json!("07700900123"));
        lead.insert("email".into(), json!("Sam@Example.com"));
        assert!(keep_visitors_contacts(
            &mut lead,
            &["hi", "ring me on +44 7700 900123 or sam@example.com"]
        ));
        assert_eq!(lead["phone"], "07700900123");
        assert_eq!(lead["email"], "Sam@Example.com");
        // Dashes, brackets and +44 (0) are the same number.
        for typed in [
            "07700-900-123",
            "(07700) 900123",
            "+44 (0)7700 900123",
            "0044 7700 900123",
        ] {
            let mut lead = serde_json::Map::new();
            lead.insert("phone".into(), json!("07700 900123"));
            assert!(
                keep_visitors_contacts(&mut lead, &[&format!("call me on {typed}")]),
                "{typed}"
            );
        }
        // An email must be one the visitor typed, not part of one.
        let mut lead = serde_json::Map::new();
        lead.insert("email".into(), json!("am@x.co"));
        assert!(!keep_visitors_contacts(
            &mut lead,
            &["email me at sam@x.com"]
        ));
        // Part of a longer number doesn't count.
        let mut lead = serde_json::Map::new();
        lead.insert("phone".into(), json!("0770090012"));
        assert!(!keep_visitors_contacts(&mut lead, &["call 07700 900123"]));
    }

    #[test]
    fn phones_in_any_common_format_are_read_and_normalised() {
        for (typed, digits) in [
            ("07700 900123", "07700900123"),
            ("07700-900-123", "07700900123"),
            ("(07700) 900123", "07700900123"),
            ("+44 (0)7700 900123", "07700900123"),
            ("+44 7700 900123", "07700900123"),
            ("0044 7700 900123", "07700900123"),
            ("01204 579975", "01204579975"),
        ] {
            assert_eq!(phone_digits(typed).as_deref(), Some(digits), "{typed}");
            let found = fallback_contact(&format!("please call me on {typed} thanks"));
            assert_eq!(
                found
                    .and_then(|(_, p)| p)
                    .and_then(|p| phone_digits(&p))
                    .as_deref(),
                Some(digits),
                "{typed}"
            );
        }
        for not_a_phone in ["2026-10-21", "12345", "1234567890123456"] {
            assert_eq!(phone_digits(not_a_phone), None, "{not_a_phone}");
        }
        assert_eq!(
            lead_message("Please call me on (07700) 900-123 about a rewire"),
            "Please call me about a rewire"
        );
    }

    #[test]
    fn names_are_only_taken_from_a_real_introduction() {
        // QA M3.
        for (text, want) in [
            ("I'm Sam Test (Claude QA), 07700 900123.", Some("Sam Test")),
            (
                "my name is Jo Bloggs and I need a rewire",
                Some("Jo Bloggs"),
            ),
            ("This is Priya Shah, ring me", Some("Priya Shah")),
            ("I am Dave. Call me on 07700 900123", Some("Dave")),
            ("I'm Sam 07700 900123", Some("Sam")),
            ("I'm Sam", Some("Sam")),
            ("It's Sam, call me", None),
            ("I'm Ready when you are, call me", None),
            ("I'm Monday free, call me", None),
            ("I'm Sorry to bother you", None),
            ("I'm Very keen", None),
            ("I'm Looking for an electrician", None),
            ("I'm Gas Safe registered myself", None),
            ("I'm Urgent", None),
            ("this is Urgent, call me", None),
            ("I'm Interested in a rewire", None),
            ("my name is jo", None),
            ("I'm Sam Test Smith Jones, call me", None),
        ] {
            assert_eq!(name_in_text(text).as_deref(), want, "{text}");
        }
    }

    #[test]
    fn the_card_message_has_no_dangling_fragments() {
        for (text, want) in [
            (
                "Can someone call me about a rewire in Bolton? I'm Sam Test (Claude QA), 07700 900123.",
                "Can someone call me about a rewire in Bolton?",
            ),
            ("Please call me on 07700 900123", "Please call me"),
            ("my number is 07700 900123", "Asked to be called back"),
            (", call me at sam@example.com", "call me"),
            ("Ring me on 07700 900123 about an EICR", "Ring me about an EICR"),
            // No name accepted: the introduction stays in the message.
            ("I'm Sorry, call me on 07700 900123", "I'm Sorry, call me"),
        ] {
            assert_eq!(lead_message(text), want, "{text}");
        }
    }

    #[test]
    fn the_fallback_card_uses_the_visitors_name_and_a_clean_message() {
        let text =
            "Can someone call me about a rewire in Bolton? I'm Sam Test (Claude QA), 07700 900123.";
        assert_eq!(name_in_text(text).as_deref(), Some("Sam Test"));
        assert_eq!(
            lead_message(text),
            "Can someone call me about a rewire in Bolton?"
        );
        assert_eq!(name_in_text("my name is jo, call me on 07700 900123"), None);
        assert_eq!(
            name_in_text("Hi, this is Priya Shah, ring me").as_deref(),
            Some("Priya Shah")
        );
        assert_eq!(name_in_text("I'm looking for an electrician"), None);
        assert_eq!(name_in_text("I'm Looking for an electrician"), None);
        assert_eq!(name_in_text("call me on 07700 900123"), None);
        assert_eq!(lead_message("07700 900123"), "Asked to be called back");
    }

    #[test]
    fn a_typed_phone_number_is_a_lead_only_when_asking_for_a_call() {
        assert_eq!(
            fallback_contact("Please call me back on 07700 900123"),
            Some((None, Some("07700 900123".into())))
        );
        assert_eq!(
            fallback_contact("ring me on +44 7700 900123 after 5"),
            Some((None, Some("+44 7700 900123".into())))
        );
        assert_eq!(
            fallback_contact("Is 01204 579975 the right number for Hayvern?"),
            None
        );
        assert_eq!(
            fallback_contact("sam@example.com"),
            Some((Some("sam@example.com".into()), None))
        );
        assert_eq!(fallback_contact("call me maybe"), None);
    }

    #[test]
    fn text_only_lead_instructions_accept_a_phone_alone_and_name_no_tool() {
        assert!(TEXT_ONLY_LEAD_INSTRUCTIONS.contains("phone number or email"));
        assert!(TEXT_ONLY_LEAD_INSTRUCTIONS.contains("A phone number alone is enough"));
        assert!(!TEXT_ONLY_LEAD_INSTRUCTIONS.contains("propose_lead"));
        assert!(TEXT_ONLY_DIRECTORY_LEADS.contains("directory team"));
    }

    #[test]
    fn retrieval_context_is_the_previous_question() {
        let h = vec![
            Message::new(Role::System, "s"),
            Message::new(Role::User, "I need an EICR in Croydon"),
            Message::new(Role::Assistant, "ok"),
            Message::new(Role::User, "How often do I need one?"),
        ];
        assert_eq!(
            previous_question(&h, "How often do I need one?"),
            "I need an EICR in Croydon"
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
