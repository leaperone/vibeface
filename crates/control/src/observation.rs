//! Pure session observations shared by the CLI and Core.
use crate::provider::Provider;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    #[default]
    Hook,
    AppServer,
    SessionScan,
    Test,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    #[default]
    TurnEnd,
    NeedsInput,
    Notification,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RequestKind {
    Permission,
    Question,
    Continue,
}

/// Untrusted normalized observation. Identity and live request bindings belong to Core.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct HookEvent {
    pub source: Source,
    pub agent: Provider,
    pub event: Kind,
    pub status: Option<String>,
    pub session_id: String,
    /// Claude uses transcript bytes (and the request hash); App Server requests use request:<id>.
    /// The actual native turn remains in Core's captured request binding.
    pub turn_id: String,
    pub cwd: String,
    pub repo: Option<String>,
    pub git_branch: Option<String>,
    pub git_head: Option<String>,
    pub last_message: String,
    pub detail: String,
    pub ts_ms: i64,
    pub test: bool,
    /// The session intends to wait; this does not grant a transport or answer authority.
    pub hold: Option<RequestKind>,
}

/// Untrusted description of the request a hook is waiting on, outside HookEvent.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HookRequest {
    pub provider: Provider,
    pub session_id: String,
    pub turn_id: String,
    pub kind: RequestKind,
    pub questions: Option<Value>,
}

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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn hook_defaults_are_exactly_the_fifteen_external_fields() {
        let event: HookEvent = serde_json::from_value(json!({})).unwrap();
        assert_eq!(
            serde_json::to_value(event).unwrap(),
            json!({
                "source":"hook", "agent":"codex", "event":"turn_end", "status":null,
                "session_id":"", "turn_id":"", "cwd":"", "repo":null,
                "git_branch":null, "git_head":null, "last_message":"", "detail":"",
                "ts_ms":0, "test":false, "hold":null
            })
        );
        let minimal: HookEvent =
            serde_json::from_value(json!({"session_id":"s", "status":"provider-specific"}))
                .unwrap();
        assert_eq!(minimal.session_id, "s");
        assert_eq!(minimal.status.as_deref(), Some("provider-specific"));
        assert_eq!(
            (minimal.agent, minimal.source, minimal.event),
            (Provider::Codex, Source::Hook, Kind::TurnEnd)
        );
    }

    #[test]
    fn ingress_rejects_trusted_metadata_unknown_fields_and_invalid_types() {
        for (field, value) in [
            ("schema_version", json!(0)),
            ("event_id", json!("")),
            ("node", json!("")),
            ("host", json!("")),
            ("request_id", Value::Null),
            ("_triage", Value::Null),
            ("_answer", Value::Null),
            ("extra", json!(1)),
            ("_request", json!({})),
            ("source", json!("unknown")),
            ("agent", json!("unknown")),
            ("event", json!("unknown")),
            ("hold", json!("unknown")),
            ("ts_ms", json!(1.5)),
            ("ts_ms", json!(9223372036854775808u64)),
            ("test", json!(1)),
            ("session_id", json!(7)),
            ("repo", json!(false)),
        ] {
            let mut input = json!({"session_id":"s"});
            input[field] = value;
            assert!(
                serde_json::from_value::<HookEvent>(input).is_err(),
                "{field}"
            );
        }
        for field in [
            "source",
            "agent",
            "event",
            "session_id",
            "turn_id",
            "cwd",
            "last_message",
            "detail",
            "ts_ms",
            "test",
        ] {
            let mut input = json!({"session_id":"s"});
            input[field] = Value::Null;
            assert!(
                serde_json::from_value::<HookEvent>(input).is_err(),
                "null {field}"
            );
        }
    }

    #[test]
    fn every_wire_enum_and_request_schema_remains_explicit() {
        for (wire, provider) in [
            ("codex", Provider::Codex),
            ("cursor", Provider::Cursor),
            ("claude", Provider::Claude),
            ("pi", Provider::Pi),
            ("opencode", Provider::Opencode),
        ] {
            assert_eq!(
                serde_json::from_value::<Provider>(json!(wire)).unwrap(),
                provider
            );
            assert_eq!(serde_json::to_value(provider).unwrap(), wire);
            assert_eq!(provider.name(), wire);
        }
        for wire in ["hook", "app_server", "session_scan", "test"] {
            let value: Source = serde_json::from_value(json!(wire)).unwrap();
            assert_eq!(serde_json::to_value(value).unwrap(), wire);
        }
        for wire in ["turn_end", "needs_input", "notification"] {
            let value: Kind = serde_json::from_value(json!(wire)).unwrap();
            assert_eq!(serde_json::to_value(value).unwrap(), wire);
        }
        for wire in ["permission", "question", "continue"] {
            let value: RequestKind = serde_json::from_value(json!(wire)).unwrap();
            assert_eq!(serde_json::to_value(value).unwrap(), wire);
        }
        let request = json!({"provider":"claude","session_id":"s","turn_id":"t","kind":"question"});
        let parsed: HookRequest = serde_json::from_value(request.clone()).unwrap();
        assert!(parsed.questions.is_none());
        for field in ["provider", "session_id", "turn_id", "kind"] {
            let mut invalid = request.clone();
            invalid.as_object_mut().unwrap().remove(field);
            assert!(serde_json::from_value::<HookRequest>(invalid).is_err());
        }
        for invalid in [
            Value::Null,
            json!({"provider":"claude","session_id":"s","turn_id":"t","kind":"question","generation":"forged"}),
        ] {
            assert!(serde_json::from_value::<HookRequest>(invalid).is_err());
        }
        // The HTTP boundary first parses Value, so duplicate keys retain their last value.
        let mut input: Value = serde_json::from_str(r#"{"session_id":"first","session_id":"last","_request":{"provider":"claude","session_id":"last","turn_id":"t","kind":"question"}}"#).unwrap();
        let request: HookRequest =
            serde_json::from_value(input.as_object_mut().unwrap().remove("_request").unwrap())
                .unwrap();
        let event: HookEvent = serde_json::from_value(input).unwrap();
        assert_eq!(event.session_id, "last");
        assert_eq!(request.session_id, event.session_id);
    }
}
