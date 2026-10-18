use regex::Regex;
use serde_json::{Map, Value};
use std::sync::LazyLock;

static EMAIL_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)[A-Z0-9._%+-]+@[A-Z0-9.-]+\.[A-Z]{2,}").unwrap());
static PHONE_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\+?\d[\d\s().-]{7,}\d").unwrap());

pub fn redact(value: Value) -> Value {
    match value {
        Value::String(s) => {
            let s = EMAIL_RE.replace_all(&s, "[redacted-email]");
            Value::String(PHONE_RE.replace_all(&s, "[redacted-phone]").into_owned())
        }
        Value::Array(items) => Value::Array(items.into_iter().map(redact).collect()),
        Value::Object(map) => Value::Object(map.into_iter().map(|(k, v)| (k, redact(v))).collect()),
        other => other,
    }
}

pub fn log_event(event: &str, payload: Value) {
    let mut line = Map::new();
    line.insert("event".into(), Value::String(event.into()));
    if let Value::Object(map) = redact(payload) {
        line.extend(map);
    }
    line.insert("at".into(), Value::String(crate::now_iso()));
    println!("{}", Value::Object(line));
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn redacts_emails_and_phones_recursively() {
        let out = redact(json!({"a": "mail jo@x.com", "b": ["call +44 7700 900123"], "n": 5}));
        assert_eq!(
            out,
            json!({"a": "mail [redacted-email]", "b": ["call [redacted-phone]"], "n": 5})
        );
    }
}
