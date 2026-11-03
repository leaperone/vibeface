//! Pure hook normalization and installation helpers used by the human CLI.
//!
//! The Core owns ingestion and persistence.  This module only translates provider hook input
//! into the wire event shape and edits provider-owned hook files.

use anyhow::{Context, Result};
use clap::ValueEnum;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

pub const HOOK_ANSWER_WITHIN_SECS: u64 = 30;
pub const SPOOL_DIR: &str = "hook-spool";

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, ValueEnum)]
#[serde(rename_all = "snake_case")]
pub enum Provider {
    #[default]
    Codex,
    Cursor,
    Claude,
    Pi,
    Opencode,
}

impl Provider {
    pub const ALL: [Self; 5] = [
        Self::Codex,
        Self::Cursor,
        Self::Claude,
        Self::Pi,
        Self::Opencode,
    ];

    pub const fn name(self) -> &'static str {
        match self {
            Self::Codex => "codex",
            Self::Cursor => "cursor",
            Self::Claude => "claude",
            Self::Pi => "pi",
            Self::Opencode => "opencode",
        }
    }

    pub const fn watch(self) -> Option<Watch> {
        match self {
            Self::Codex => Some(Watch::Hooks {
                file: "hooks.json",
                events: &["Stop", "PermissionRequest"],
            }),
            Self::Claude => Some(Watch::Hooks {
                file: ".claude/settings.json",
                events: &["Stop", "StopFailure", "Notification", "PermissionRequest"],
            }),
            Self::Pi => Some(Watch::Extension {
                file: ".pi/agent/extensions/vibeface.ts",
                source: include_str!("../../core/extensions/pi.ts"),
            }),
            Self::Opencode => Some(Watch::Extension {
                file: ".config/opencode/plugins/vibeface.ts",
                source: include_str!("../../core/extensions/opencode.ts"),
            }),
            Self::Cursor => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Watch {
    Hooks {
        file: &'static str,
        events: &'static [&'static str],
    },
    Extension {
        file: &'static str,
        source: &'static str,
    },
}

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

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AgentEvent {
    pub schema_version: u32,
    pub event_id: String,
    pub source: Source,
    pub host: String,
    pub node: String,
    pub agent: Provider,
    pub event: Kind,
    pub status: Option<String>,
    pub session_id: String,
    pub turn_id: String,
    pub cwd: String,
    pub repo: Option<String>,
    pub git_branch: Option<String>,
    pub git_head: Option<String>,
    pub last_message: String,
    pub detail: String,
    pub ts_ms: i64,
    pub test: bool,
    pub request_id: Option<String>,
    pub hold: Option<RequestKind>,
}

fn string(input: &Value, key: &str) -> String {
    input[key].as_str().unwrap_or_default().to_owned()
}

fn hex32(parts: &[&str]) -> String {
    let mut hash = Sha256::new();
    for part in parts {
        hash.update(part.as_bytes());
        hash.update([0]);
    }
    hex::encode(hash.finalize())[..32].to_owned()
}

pub fn from_hook(agent: Provider, input: &Value, transcript_bytes: u64) -> Option<AgentEvent> {
    match agent {
        Provider::Codex => from_codex_hook(input),
        Provider::Claude => from_claude_hook(input, transcript_bytes),
        Provider::Pi => from_pi_extension(input),
        Provider::Opencode => from_opencode_plugin(input),
        Provider::Cursor => None,
    }
}

fn from_codex_hook(input: &Value) -> Option<AgentEvent> {
    let (event, status, detail) = match input["hook_event_name"].as_str()? {
        "Stop" => (Kind::TurnEnd, Some("completed".to_owned()), String::new()),
        "PermissionRequest" => (Kind::NeedsInput, None, string(input, "tool_name")),
        _ => return None,
    };
    Some(AgentEvent {
        source: Source::Hook,
        agent: Provider::Codex,
        event,
        status,
        session_id: string(input, "session_id"),
        turn_id: string(input, "turn_id"),
        cwd: string(input, "cwd"),
        last_message: string(input, "last_assistant_message"),
        detail,
        ..Default::default()
    })
}

fn question_lines(questions: &Value) -> Vec<String> {
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

fn request_kind(input: &Value) -> RequestKind {
    if input["tool_name"] == "AskUserQuestion" {
        RequestKind::Question
    } else {
        RequestKind::Permission
    }
}

fn describe_request(input: &Value) -> String {
    let tool = input["tool_name"].as_str().unwrap_or("a tool");
    let tool_input = &input["tool_input"];
    if request_kind(input) == RequestKind::Question {
        return format!(
            "Claude Code asks: {}. Answer with {{\"answers\":{{\"QUESTION\":\"ANSWER\"}}}}.",
            question_lines(&tool_input["questions"]).join("; ")
        );
    }
    let what = tool_input["description"]
        .as_str()
        .or_else(|| tool_input["file_path"].as_str())
        .map(|what| format!(": {what}"))
        .unwrap_or_default();
    format!(
        "{tool}{what}. Answer with {{\"behavior\":\"allow\"}} or {{\"behavior\":\"deny\",\"message\":\"WHY\"}}."
    )
}

fn from_claude_hook(input: &Value, transcript_bytes: u64) -> Option<AgentEvent> {
    let (event, status, detail) = match input["hook_event_name"].as_str()? {
        "Stop" => (Kind::TurnEnd, Some("completed".to_owned()), String::new()),
        "StopFailure" => (
            Kind::TurnEnd,
            Some("failed".to_owned()),
            [string(input, "error"), string(input, "error_details")]
                .join(" ")
                .trim()
                .to_owned(),
        ),
        "Notification" if input["notification_type"] == "permission_prompt" => return None,
        "Notification" => (
            Kind::Notification,
            None,
            [string(input, "notification_type"), string(input, "message")]
                .join(" ")
                .trim()
                .to_owned(),
        ),
        "PermissionRequest" => (Kind::NeedsInput, None, describe_request(input)),
        _ => return None,
    };
    let turn_id = if event == Kind::NeedsInput {
        format!(
            "tx:{transcript_bytes}:{}",
            hex32(&[
                &string(input, "tool_name"),
                &input["tool_input"].to_string()
            ])
        )
    } else {
        format!("tx:{transcript_bytes}")
    };
    Some(AgentEvent {
        source: Source::Hook,
        agent: Provider::Claude,
        event,
        status,
        session_id: string(input, "session_id"),
        turn_id,
        cwd: string(input, "cwd"),
        last_message: string(input, "last_assistant_message"),
        detail,
        hold: (event == Kind::NeedsInput).then(|| request_kind(input)),
        ..Default::default()
    })
}

fn from_pi_extension(input: &Value) -> Option<AgentEvent> {
    let (status, detail) = match input["stop_reason"].as_str()? {
        "stop" => ("completed", String::new()),
        "length" => ("failed", "pi stopped at the model's output limit".into()),
        "error" => ("failed", string(input, "error")),
        _ => return None,
    };
    Some(AgentEvent {
        source: Source::Hook,
        agent: Provider::Pi,
        event: Kind::TurnEnd,
        status: Some(status.into()),
        session_id: string(input, "session_id"),
        turn_id: string(input, "turn_id"),
        cwd: string(input, "cwd"),
        last_message: string(input, "last_message"),
        detail,
        hold: Some(RequestKind::Continue),
        ..Default::default()
    })
}

fn from_opencode_plugin(input: &Value) -> Option<AgentEvent> {
    let request = || input["request_id"].as_str().filter(|id| !id.is_empty());
    let (event, status, turn_id, detail, hold) = match input["event"].as_str()? {
        "session.idle" => {
            let status = input["status"]
                .as_str()
                .filter(|status| matches!(*status, "completed" | "interrupted" | "failed"))?;
            (
                Kind::TurnEnd,
                Some(status),
                string(input, "turn_id"),
                string(input, "error"),
                None,
            )
        }
        "permission.asked" => (
            Kind::NeedsInput,
            None,
            request()?.to_owned(),
            format!(
                "opencode asks to use {}. Answer with {{\"reply\":\"once\"}}, {{\"reply\":\"always\"}} or {{\"reply\":\"reject\"}}.",
                input["permission"].as_str().unwrap_or("a tool")
            ),
            Some(RequestKind::Permission),
        ),
        "question.asked" => (
            Kind::NeedsInput,
            None,
            request()?.to_owned(),
            format!(
                "Answer with {{\"answers\":[[\"LABEL\"]]}}, the labels chosen for each question in order, or {{\"reject\":true}}. opencode asks: {}.",
                question_lines(&input["questions"]).join("; ")
            ),
            Some(RequestKind::Question),
        ),
        _ => return None,
    };
    Some(AgentEvent {
        source: Source::Hook,
        agent: Provider::Opencode,
        event,
        status: status.map(String::from),
        session_id: string(input, "session_id"),
        turn_id,
        cwd: string(input, "cwd"),
        last_message: string(input, "last_message"),
        detail,
        hold,
        ..Default::default()
    })
}

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

pub fn interactive_claude(entrypoint: Option<&str>) -> bool {
    !entrypoint.is_some_and(|entrypoint| entrypoint.starts_with("sdk"))
}

pub fn rollout_meta(first_line: &str) -> Value {
    serde_json::from_str::<Value>(first_line)
        .ok()
        .filter(|line| line["type"] == "session_meta")
        .map(|line| line["payload"].clone())
        .unwrap_or(Value::Null)
}

pub struct HookFile {
    pub agent: Provider,
    pub path: PathBuf,
    pub events: &'static [&'static str],
}

pub fn hook_files_with_claude_root(
    home: &Path,
    claude_home: &Path,
    codex_home: &Path,
) -> Vec<HookFile> {
    Provider::ALL
        .into_iter()
        .filter_map(|agent| match agent.watch()? {
            Watch::Hooks { file, events } => Some(HookFile {
                agent,
                path: match agent {
                    Provider::Codex => codex_home.join(file),
                    Provider::Claude => claude_home.join("settings.json"),
                    _ => home.join(file),
                },
                events,
            }),
            Watch::Extension { .. } => None,
        })
        .collect()
}

pub fn extension_code(source: &str, program: &Path, agent: Provider, data_dir: &Path) -> String {
    let hook = json!([
        program.to_string_lossy(),
        "hook",
        agent.name(),
        "--data-dir",
        data_dir.to_string_lossy()
    ]);
    source.replace("__VIBEFACE_HOOK__", &hook.to_string())
}

pub fn hook_command(program: &Path, agent: Provider, data_dir: &Path) -> String {
    let quote = |text: &str| format!("'{}'", text.replace('\'', r"'\''"));
    format!(
        "{} hook {} --data-dir {}",
        quote(&program.to_string_lossy()),
        agent.name(),
        quote(&data_dir.to_string_lossy())
    )
}

fn ours(entry: &Value, agent: Provider) -> bool {
    entry["hooks"].as_array().is_some_and(|hooks| {
        hooks.iter().any(|hook| {
            hook["command"].as_str().is_some_and(|command| {
                command.contains(&format!(" hook {} ", agent.name()))
                    && command.contains("vibeface")
            })
        })
    })
}

pub fn add_hooks(settings: &mut Value, file: &HookFile, command: &str) -> Result<Vec<String>> {
    if settings.is_null() {
        *settings = json!({});
    }
    let hooks = settings
        .as_object_mut()
        .context("hook file is not a JSON object")?
        .entry("hooks")
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .context("`hooks` is not an object")?;
    let mut added = Vec::new();
    for event in file.events {
        let entries = hooks
            .entry(*event)
            .or_insert_with(|| json!([]))
            .as_array_mut()
            .with_context(|| format!("`hooks.{event}` is not a list"))?;
        if entries.iter().any(|entry| ours(entry, file.agent)) {
            continue;
        }
        let timeout = match (file.agent, *event) {
            (Provider::Claude, "PermissionRequest") => HOOK_ANSWER_WITHIN_SECS + 15,
            _ => 10,
        };
        entries.push(json!({"hooks":[{"type":"command","command":command,"timeout":timeout}]}));
        added.push((*event).to_owned());
    }
    Ok(added)
}

pub fn remove_hooks(settings: &mut Value, file: &HookFile) -> Vec<String> {
    let mut removed = Vec::new();
    if let Some(hooks) = settings["hooks"].as_object_mut() {
        for event in file.events {
            if let Some(entries) = hooks.get_mut(*event).and_then(Value::as_array_mut) {
                let before = entries.len();
                entries.retain(|entry| !ours(entry, file.agent));
                if entries.len() != before {
                    removed.push((*event).to_owned());
                }
            }
        }
    }
    removed
}

pub fn installed(settings: &Value, file: &HookFile) -> Vec<String> {
    file.events
        .iter()
        .filter(|event| {
            settings["hooks"][**event]
                .as_array()
                .is_some_and(|entries| entries.iter().any(|entry| ours(entry, file.agent)))
        })
        .map(|event| (*event).to_owned())
        .collect()
}

pub fn hook_output(input: &Value, answer: &Value) -> Option<Value> {
    if input["hook_event_name"] != "PermissionRequest" {
        return None;
    }
    let decision = match (&answer["answers"], answer["behavior"].as_str()) {
        (Value::Object(answers), _) if request_kind(input) == RequestKind::Question => {
            if !answers.values().all(Value::is_string) {
                return None;
            }
            let mut updated = input["tool_input"].clone();
            updated.as_object_mut()?;
            updated["answers"] = Value::Object(answers.clone());
            json!({"behavior":"allow","updatedInput":updated})
        }
        (_, Some("allow")) if answer.get("updatedInput").is_some() => return None,
        (_, Some("allow")) => json!({"behavior":"allow"}),
        (_, Some("deny")) => {
            json!({"behavior":"deny","message":answer["message"].as_str().unwrap_or("The owner's bot declined this."),"interrupt":answer["interrupt"] == true})
        }
        _ => return None,
    };
    Some(json!({"hookSpecificOutput":{"hookEventName":"PermissionRequest","decision":decision}}))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hook_mapping_preserves_provider_specific_fields() {
        let event = from_hook(Provider::Codex, &json!({"hook_event_name":"Stop","session_id":"s","turn_id":"t","cwd":"/p","last_assistant_message":"done"}), 0).unwrap();
        assert_eq!(event.status.as_deref(), Some("completed"));
        assert_eq!(event.agent, Provider::Codex);
        let event = from_hook(
            Provider::Pi,
            &json!({"stop_reason":"length","session_id":"s","turn_id":"t","cwd":"/p"}),
            0,
        )
        .unwrap();
        assert_eq!(event.hold, Some(RequestKind::Continue));
    }
}
