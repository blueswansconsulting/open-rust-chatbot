//! Tools the model may call. They are deliberately narrow: read-only knowledge-base search,
//! and a lead *proposal* that only the visitor (not the model) can confirm.

use crate::kb::KnowledgeBase;
use regex::Regex;
use serde::Serialize;
use serde_json::{json, Map, Value};
use std::sync::LazyLock;

#[derive(Debug, Clone, PartialEq)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: Map<String, Value>,
}

pub struct ToolDefinition {
    pub name: &'static str,
    pub description: &'static str,
    pub parameters: Value,
}

/// A lead the model has proposed and the visitor has not yet confirmed.
#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Lead {
    pub lead_id: String,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub phone: Option<String>,
    /// When the visitor would like a call back, in their own words ("after 5pm").
    #[serde(skip_serializing_if = "Option::is_none")]
    pub best_time: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

pub enum ToolOutcome {
    /// Plain result fed back to the model.
    Result(Value),
    /// A lead awaiting the visitor's explicit confirmation in the widget.
    LeadProposed(Lead),
}

const MAX_FIELD_CHARS: usize = 200;
const MAX_LEAD_MESSAGE_CHARS: usize = 1000;

static EMAIL_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^[A-Za-z0-9._%+-]{1,64}@[A-Za-z0-9.-]{1,253}\.[A-Za-z]{2,24}$").unwrap()
});
static PHONE_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\+?[0-9 ()\-]{6,24}$").unwrap());

fn object(required: &[&str], properties: Value) -> Value {
    json!({ "type": "object", "additionalProperties": false, "required": required, "properties": properties })
}

pub fn definitions() -> Vec<ToolDefinition> {
    vec![
        ToolDefinition {
            name: "search_knowledge_base",
            description: "Search the business's knowledge base. Use this before answering any question about the business.",
            parameters: object(&["query"], json!({ "query": { "type": "string" } })),
        },
        ToolDefinition {
            name: "propose_lead",
            description: "Show the visitor a confirmation card with their contact details so the team can call or email them back. Nothing is sent until the visitor presses confirm. Only use after the visitor has asked to be contacted and given their name plus a phone number or email.",
            parameters: object(
                &["name", "email", "phone", "best_time", "message"],
                json!({
                    "name": { "type": "string" },
                    "email": { "type": ["string", "null"] },
                    "phone": { "type": ["string", "null"] },
                    "best_time": { "type": ["string", "null"], "description": "When they'd like a call back, in their words" },
                    "message": { "type": ["string", "null"], "description": "One-line summary of what they want to talk about" },
                }),
            ),
        },
    ]
}

fn string_arg(args: &Map<String, Value>, key: &str) -> String {
    args.get(key)
        .and_then(Value::as_str)
        .map(|s| s.trim().to_owned())
        .unwrap_or_default()
}

fn clip(s: String, max: usize) -> String {
    s.chars()
        .filter(|c| !c.is_control() || *c == '\n')
        .take(max)
        .collect()
}

pub fn execute(kb: &KnowledgeBase, call: &ToolCall) -> ToolOutcome {
    match call.name.as_str() {
        "search_knowledge_base" => {
            let query = clip(string_arg(&call.arguments, "query"), MAX_FIELD_CHARS);
            let matches = kb.search(&query, 3);
            if matches.is_empty() {
                return ToolOutcome::Result(json!({
                    "found": false,
                    "answer": "Nothing in the knowledge base matched. Do not guess; offer to take contact details instead.",
                }));
            }
            ToolOutcome::Result(json!({
                "found": true,
                "results": matches.iter().map(|m| json!({ "section": m.header_path.join(" > "), "text": m.text })).collect::<Vec<_>>(),
            }))
        }
        "propose_lead" => {
            let name = clip(string_arg(&call.arguments, "name"), MAX_FIELD_CHARS);
            let email =
                Some(clip(string_arg(&call.arguments, "email"), MAX_FIELD_CHARS).to_lowercase())
                    .filter(|e| !e.is_empty());
            let best_time = Some(clip(
                string_arg(&call.arguments, "best_time"),
                MAX_FIELD_CHARS,
            ))
            .filter(|t| !t.is_empty());
            let phone = Some(clip(string_arg(&call.arguments, "phone"), MAX_FIELD_CHARS))
                .filter(|p| !p.is_empty());
            let message = Some(clip(
                string_arg(&call.arguments, "message"),
                MAX_LEAD_MESSAGE_CHARS,
            ))
            .filter(|m| !m.is_empty());
            let mut problems = Vec::new();
            if name.is_empty() {
                problems.push("name is missing");
            }
            if email.is_none() && phone.is_none() {
                problems.push("a phone or email is needed so the team can get back to them");
            }
            if email.as_ref().is_some_and(|e| !EMAIL_RE.is_match(e)) {
                problems.push("email is not valid");
            }
            if phone.as_ref().is_some_and(|p| !PHONE_RE.is_match(p)) {
                problems.push("phone is not valid");
            }
            if !problems.is_empty() {
                return ToolOutcome::Result(json!({ "status": "invalid", "problems": problems }));
            }
            ToolOutcome::LeadProposed(Lead {
                lead_id: uuid::Uuid::new_v4().to_string(),
                name,
                email,
                phone,
                best_time,
                message,
            })
        }
        other => ToolOutcome::Result(json!({ "error": format!("unknown tool {other}") })),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(name: &str, args: Value) -> ToolCall {
        ToolCall {
            id: "c".into(),
            name: name.into(),
            arguments: args.as_object().unwrap().clone(),
        }
    }

    fn kb() -> KnowledgeBase {
        KnowledgeBase::from_markdown("# Opening hours\nWe are open 9am to 5pm Monday to Friday.")
    }

    #[test]
    fn schemas_are_strict() {
        for def in definitions() {
            assert_eq!(def.parameters["additionalProperties"], false);
            let required = def.parameters["required"].as_array().unwrap();
            for key in def.parameters["properties"].as_object().unwrap().keys() {
                assert!(required.contains(&json!(key)), "{}.{key}", def.name);
            }
        }
    }

    #[test]
    fn search_returns_sections_or_a_no_guess_instruction() {
        let ToolOutcome::Result(hit) = execute(
            &kb(),
            &call("search_knowledge_base", json!({"query": "opening hours"})),
        ) else {
            panic!()
        };
        assert_eq!(hit["results"][0]["section"], "Opening hours");
        let ToolOutcome::Result(miss) = execute(
            &kb(),
            &call("search_knowledge_base", json!({"query": "zzz"})),
        ) else {
            panic!()
        };
        assert_eq!(miss["found"], false);
    }

    #[test]
    fn propose_lead_validates_and_normalises() {
        let ok = execute(
            &kb(),
            &call(
                "propose_lead",
                json!({"name": " Sam ", "email": "Sam@Example.com", "phone": null, "message": "call me"}),
            ),
        );
        let ToolOutcome::LeadProposed(lead) = ok else {
            panic!()
        };
        assert_eq!(
            (
                lead.name.as_str(),
                lead.email.as_deref().unwrap_or_default()
            ),
            ("Sam", "sam@example.com")
        );
        assert_eq!(lead.lead_id.len(), 36);

        let bad = execute(
            &kb(),
            &call(
                "propose_lead",
                json!({"name": "", "email": "nope", "phone": "<script>", "message": null}),
            ),
        );
        let ToolOutcome::Result(v) = bad else {
            panic!()
        };
        assert_eq!(v["problems"].as_array().unwrap().len(), 3);
    }

    #[test]
    fn lead_fields_are_length_capped_and_stripped_of_control_chars() {
        let long = "a".repeat(5000);
        let out = execute(
            &kb(),
            &call(
                "propose_lead",
                json!({"name": format!("Sam\u{0007}{long}"), "email": "s@x.io", "phone": null, "message": long}),
            ),
        );
        let ToolOutcome::LeadProposed(lead) = out else {
            panic!()
        };
        assert_eq!(lead.name.chars().count(), MAX_FIELD_CHARS);
        assert!(!lead.name.contains('\u{0007}'));
        assert_eq!(
            lead.message.unwrap().chars().count(),
            MAX_LEAD_MESSAGE_CHARS
        );
    }

    #[test]
    fn call_back_request_needs_a_phone_or_email_and_keeps_best_time() {
        let phone_only = execute(
            &kb(),
            &call(
                "propose_lead",
                json!({"name": "Jo", "email": null, "phone": "07700 900123", "best_time": "after 5pm", "message": "wants a demo"}),
            ),
        );
        let ToolOutcome::LeadProposed(lead) = phone_only else {
            panic!("phone-only call back must be valid")
        };
        assert_eq!(lead.email, None);
        assert_eq!(lead.best_time.as_deref(), Some("after 5pm"));

        let neither = execute(
            &kb(),
            &call(
                "propose_lead",
                json!({"name": "Jo", "email": null, "phone": null, "best_time": null, "message": null}),
            ),
        );
        let ToolOutcome::Result(v) = neither else {
            panic!()
        };
        assert!(v["problems"].to_string().contains("phone or email"));
    }

    #[test]
    fn unknown_tool_is_reported_not_executed() {
        let ToolOutcome::Result(v) = execute(&kb(), &call("delete_everything", json!({}))) else {
            panic!()
        };
        assert!(v["error"].as_str().unwrap().contains("unknown tool"));
    }
}
