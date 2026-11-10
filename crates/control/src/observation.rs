//! Pure session observations shared by the CLI and Core.
use serde_json::Value;
use sha2::{Digest, Sha256};

pub fn hex32(parts: &[&str]) -> String {
    let mut hash = Sha256::new();
    for part in parts {
        hash.update(part.as_bytes());
        hash.update([0]);
    }
    hex::encode(hash.finalize())[..32].to_owned()
}

/// Whether a Codex session's `source` (a rollout's `session_meta`, or an App Server thread)
/// is one the user drives. `codex exec`, MCP and sub-agent sessions run without anyone
/// watching them.
pub fn interactive_codex(source: &Value, originator: Option<&str>) -> bool {
    let background = match source {
        Value::String(source) => matches!(source.to_ascii_lowercase().as_str(), "exec" | "mcp"),
        Value::Object(source) => source
            .keys()
            .any(|key| key.eq_ignore_ascii_case("subagent")),
        _ => false,
    };
    !background && !originator.is_some_and(|originator| originator.ends_with("_exec"))
}

/// The `session_meta` payload on a Codex rollout's first line, if it has one.
pub fn rollout_meta(first_line: &str) -> Value {
    serde_json::from_str::<Value>(first_line)
        .ok()
        .filter(|line| line["type"] == "session_meta")
        .map(|line| line["payload"].clone())
        .unwrap_or(Value::Null)
}

/// Each question of a request with its option labels, as `text [A | B]`, or its bare text when
/// it offers none; the Agent answers with the labels.
pub fn question_lines(questions: &Value) -> Vec<String> {
    questions
        .as_array()
        .into_iter()
        .flatten()
        .map(|question| {
            let text = question["question"].as_str().unwrap_or_default();
            let options: Vec<&str> = question["options"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|option| option["label"].as_str())
                .collect();
            if options.is_empty() {
                text.to_owned()
            } else {
                format!("{text} [{}]", options.join(" | "))
            }
        })
        .collect()
}
