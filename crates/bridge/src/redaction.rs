use serde_json::Value;

pub fn redact_string(mut text: String, secrets: &[impl AsRef<str>]) -> String {
    for secret in secrets {
        let secret = secret.as_ref();
        if !secret.is_empty() {
            text = text.replace(secret, "[redacted]");
        }
    }
    text
}

pub fn redact(value: Value, secrets: &[impl AsRef<str>]) -> Value {
    match value {
        Value::String(text) => Value::String(redact_string(text, secrets)),
        Value::Array(items) => Value::Array(
            items
                .into_iter()
                .map(|item| redact(item, secrets))
                .collect(),
        ),
        Value::Object(items) => Value::Object(
            items
                .into_iter()
                .map(|(key, value)| (key, redact(value, secrets)))
                .collect(),
        ),
        value => value,
    }
}
