//! Pure credential detection and progress redaction helpers for provider output.

use serde_json::{Value, json};
use std::collections::BTreeMap;

/// Whether an environment value is a credential worth hiding in shared records. Ordinary values
/// such as `DEBUG=1` or a workspace path must not be replaced, or every "1" in a reply vanishes.
pub(crate) fn credential_value(name: &str, value: &str) -> bool {
    if value.len() < 8 {
        return false;
    }
    if ["_KEY", "_TOKEN", "_SECRET", "_PASSWORD"]
        .iter()
        .any(|suffix| name.ends_with(suffix))
    {
        return true;
    }
    let lower = value.to_ascii_lowercase();
    if [
        "-----begin",
        "bearer ",
        "password=",
        "password:",
        "passwd=",
        "secret=",
        "api_key=",
        "apikey=",
        "token=",
    ]
    .iter()
    .any(|marker| lower.contains(marker))
    {
        return true;
    }
    value
        .split(|c: char| c.is_whitespace() || "\"'`,;()[]{}<>".contains(c))
        .any(|word| {
            let lower = word.to_ascii_lowercase();
            url_with_password(word)
                || (word.len() >= 20
                    && [
                        "sk-",
                        "sk_",
                        "ghp_",
                        "gho_",
                        "ghs_",
                        "ghu_",
                        "github_pat_",
                        "xoxb-",
                        "xoxp-",
                        "xapp-",
                        "akia",
                        "aiza",
                        "glpat-",
                        "hf_",
                        "npm_",
                    ]
                    .iter()
                    .any(|prefix| lower.starts_with(prefix)))
                || (word.len() >= 32
                    && word
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || "-_+/=".contains(c))
                    && word.chars().any(|c| c.is_ascii_digit())
                    && word.chars().any(|c| c.is_ascii_uppercase())
                    && word.chars().any(|c| c.is_ascii_lowercase()))
        })
}

/// A connection string such as `postgres://user:pass@host/db` carries its password in the
/// authority, so the whole value is a credential whatever its variable is called.
fn url_with_password(word: &str) -> bool {
    let Some((_, rest)) = word.split_once("://") else {
        return false;
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    authority.rsplit_once('@').is_some_and(|(userinfo, _)| {
        userinfo
            .split_once(':')
            .is_some_and(|(_, pass)| !pass.is_empty())
    })
}

pub fn redact_text(text: String, secrets: &[impl AsRef<str>]) -> String {
    redact_prefix(&text, secrets, text.len(), 0).0
}

fn redact_prefix(
    text: &str,
    secrets: &[impl AsRef<str>],
    split: usize,
    mut redacted_until: usize,
) -> (String, usize) {
    let mut clean = String::with_capacity(split);
    for (index, ch) in text[..split].char_indices() {
        let end = secrets
            .iter()
            .map(AsRef::as_ref)
            .filter(|secret| !secret.is_empty() && text[index..].starts_with(secret))
            .map(|secret| index + secret.len())
            .max()
            .unwrap_or(index);
        if end > index {
            if index >= redacted_until {
                clean.push_str("[redacted]");
            }
            redacted_until = redacted_until.max(end);
        } else if index >= redacted_until {
            clean.push(ch);
        }
    }
    (clean, redacted_until.saturating_sub(split))
}

pub fn redact_progress(value: Value, secrets: &[impl AsRef<str>]) -> Value {
    match value {
        Value::String(text) => Value::String(redact_text(text, secrets)),
        Value::Array(items) => Value::Array(
            items
                .into_iter()
                .map(|item| redact_progress(item, secrets))
                .collect(),
        ),
        Value::Object(items) => Value::Object(
            items
                .into_iter()
                .map(|(key, value)| (key, redact_progress(value, secrets)))
                .collect(),
        ),
        value => value,
    }
}

#[derive(Default)]
pub(crate) struct PendingRedaction {
    text: String,
    redacted_until: usize,
}

pub(crate) fn redact_stream_progress(
    mut value: Value,
    secrets: &[&str],
    pending: &mut BTreeMap<String, PendingRedaction>,
) -> Value {
    if let Some(item) = value
        .get("item_id")
        .and_then(Value::as_str)
        .map(str::to_owned)
    {
        if value["type"] == "text_snapshot" {
            pending.remove(&item);
        }
        if value["type"] == "text_delta"
            && let Some(text) = value.get("text").and_then(Value::as_str)
        {
            let buffered = pending.entry(item).or_default();
            buffered.text.push_str(text);
            let max_prefix = secrets
                .iter()
                .map(|secret| secret.len().saturating_sub(1))
                .max()
                .unwrap_or(0);
            let split = buffered
                .text
                .char_indices()
                .map(|(index, _)| index)
                .filter(|index| buffered.text.len() - index <= max_prefix)
                .find(|index| {
                    secrets.iter().any(|secret| {
                        secret.len() > buffered.text.len() - index
                            && secret.starts_with(&buffered.text[*index..])
                    })
                })
                .unwrap_or(buffered.text.len());
            let (text, redacted_until) =
                redact_prefix(&buffered.text, secrets, split, buffered.redacted_until);
            buffered.text = buffered.text[split..].to_owned();
            buffered.redacted_until = redacted_until;
            value = redact_progress(value, secrets);
            value["text"] = json!(text);
            return value;
        }
    }
    redact_progress(value, secrets)
}

#[cfg(test)]
mod tests {
    use super::{credential_value, redact_progress, redact_stream_progress};
    use serde_json::json;
    use std::collections::BTreeMap;

    #[test]
    fn redaction_preserves_keys_and_masks_overlapping_secrets() {
        let secrets = [
            "synthetic-sensitive",
            "synthetic-sensitive-token",
            "token-suffix",
            "",
            "redacted",
        ];
        assert_eq!(
            redact_progress(
                json!({
                    "synthetic-sensitive-token": [
                        "prefix synthetic-sensitive-token-suffix ✓",
                        {"nested": "synthetic-sensitive"},
                    ],
                    "literal": true,
                    "number": 7,
                    "null": null,
                }),
                &secrets,
            ),
            json!({
                "synthetic-sensitive-token": [
                    "prefix [redacted] ✓",
                    {"nested": "[redacted]"},
                ],
                "literal": true,
                "number": 7,
                "null": null,
            }),
        );
    }

    #[test]
    fn redaction_buffers_nested_and_overlapping_secrets_across_frames() {
        let secrets = [
            "synthetic-sensitive",
            "synthetic-sensitive-token",
            "aba",
            "秘密🙂",
        ];
        for text in [
            "before synthetic-sensitive-token after",
            "before synthetic-sensitive after",
            "before ababa after",
            "before 秘密🙂 after",
        ] {
            let mut pending = BTreeMap::new();
            let mut streamed = String::new();
            for ch in text.chars() {
                let update = redact_stream_progress(
                    json!({"type": "text_delta", "item_id": "reply", "text": ch.to_string()}),
                    &secrets,
                    &mut pending,
                );
                streamed.push_str(update["text"].as_str().unwrap());
            }
            assert_eq!(streamed, "before [redacted] after");
            let snapshot = redact_stream_progress(
                json!({"type": "text_snapshot", "item_id": "reply", "text": text}),
                &secrets,
                &mut pending,
            );
            assert_eq!(snapshot["text"], "before [redacted] after");
            assert!(pending.is_empty());
        }
    }

    #[test]
    fn redaction_keeps_periodic_overlap_streams_bounded() {
        let secrets = ["aba"];
        let mut pending = BTreeMap::new();
        let mut streamed = String::new();
        for ch in format!("{}a end", "ab".repeat(4096)).chars() {
            let update = redact_stream_progress(
                json!({"type": "text_delta", "item_id": "reply", "text": ch.to_string()}),
                &secrets,
                &mut pending,
            );
            let buffered = pending["reply"].text.len();
            assert!(buffered <= 2, "pending grew to {buffered} bytes");
            streamed.push_str(update["text"].as_str().unwrap());
        }
        assert_eq!(streamed, "[redacted] end");
    }

    #[test]
    fn redaction_keeps_items_separate_and_does_not_reprocess_stream_markers() {
        let secrets = ["aba", "秘密🙂", "redacted"];
        let mut pending = BTreeMap::new();
        let mut streamed = BTreeMap::<&str, String>::new();
        for (item, chunk) in [
            ("left", "ab"),
            ("right", "秘"),
            ("left", "aba"),
            ("right", "密🙂 "),
            ("left", " "),
        ] {
            let update = redact_stream_progress(
                json!({"type": "text_delta", "item_id": item, "text": chunk}),
                &secrets,
                &mut pending,
            );
            streamed
                .entry(item)
                .or_default()
                .push_str(update["text"].as_str().unwrap());
        }
        assert_eq!(streamed["left"], "[redacted] ");
        assert_eq!(streamed["right"], "[redacted] ");
        let snapshot = redact_stream_progress(
            json!({"type": "text_snapshot", "item_id": "left", "text": "ababa"}),
            &secrets,
            &mut pending,
        );
        assert_eq!(snapshot["text"], "[redacted]");
        assert!(!pending.contains_key("left"));
        assert!(pending.contains_key("right"));
    }

    #[test]
    fn credential_values_are_selected_by_name_or_shape_only() {
        assert!(credential_value("CURSOR_API_KEY", "k!secret1"));
        assert!(!credential_value("FOO_TOKEN", "1"));
        assert!(credential_value(
            "DATABASE_URL",
            "postgres://app:s3cret@db.internal/app"
        ));
        assert!(credential_value(
            "X",
            "ghp_abcdefghijklmnopqrstuvwxyz0123456789"
        ));
        assert!(!credential_value("DEBUG", "1"));
        assert!(!credential_value("NODE_ENV", "production"));
        assert!(!credential_value("HOME", "/Users/me/project"));
        assert!(!credential_value("API_BASE", "http://host:8080/path"));
        assert!(!credential_value("ORIGIN", "user@example.com"));
    }
}
