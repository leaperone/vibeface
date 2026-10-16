use anyhow::{Context, Result, bail, ensure};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{path::Path, time::Duration};
use tokio::{
    io::{AsyncWriteExt, BufReader},
    net::UnixStream,
};
use vibeface_protocol::{
    Command, CommandRecord, CommandRequest, CommandStatus, TaskInput, TaskUpdate,
};

pub const READ_ONLY_TOOLS: [&str; 6] = [
    "session_list",
    "session_read",
    "session_turns",
    "session_status",
    "task_list",
    "task_read",
];
const MAX_FRAME_BYTES: usize = 4_194_304;

pub async fn call_local(socket: &Path, request: &CommandRequest) -> Result<CommandRecord> {
    request.validate().map_err(anyhow::Error::msg)?;
    let exchange = async {
        let mut stream = UnixStream::connect(socket)
            .await
            .context("connect to local Bridge")?;
        let mut frame = serde_json::to_vec(request)?;
        ensure!(frame.len() <= MAX_FRAME_BYTES, "local command is too large");
        frame.push(b'\n');
        stream.write_all(&frame).await?;
        let line = crate::agent::bounded_line(&mut BufReader::new(stream), MAX_FRAME_BYTES).await?;
        ensure!(
            !line.is_empty(),
            "local Bridge closed without a command result"
        );
        let record: CommandRecord =
            serde_json::from_slice(&line).context("invalid local Bridge command result")?;
        ensure!(
            &record.request == request,
            "local Bridge returned a mismatched command result"
        );
        Ok(record)
    };
    if matches!(&request.command, Command::Manage { .. }) {
        exchange.await
    } else {
        tokio::time::timeout(Duration::from_secs(60), exchange)
            .await
            .context("local Bridge command timed out; command outcome is unknown")?
    }
}

pub async fn serve_mcp(socket: &Path) -> Result<()> {
    let read_only = std::env::var("VIBEFACE_MCP_READ_ONLY").as_deref() == Ok("1");
    let task_id = current_task_id()?;
    let mut stdin = BufReader::new(tokio::io::stdin());
    let mut stdout = tokio::io::stdout();
    loop {
        let line = crate::agent::bounded_line(&mut stdin, MAX_FRAME_BYTES).await?;
        if line.is_empty() {
            return Ok(());
        }
        let response = match serde_json::from_slice::<Value>(&line) {
            Ok(request) => handle_request(socket, request, read_only, task_id.as_deref()).await,
            Err(_) => Some(rpc_error(Value::Null, -32700, "invalid JSON")),
        };
        if let Some(response) = response {
            let mut frame = serde_json::to_vec(&response)?;
            frame.push(b'\n');
            stdout.write_all(&frame).await?;
            stdout.flush().await?;
        }
    }
}

async fn handle_request(
    socket: &Path,
    request: Value,
    read_only: bool,
    task_id: Option<&str>,
) -> Option<Value> {
    let id = request.get("id").cloned();
    let method = request.get("method").and_then(Value::as_str);
    if request.get("jsonrpc").and_then(Value::as_str) != Some("2.0") || method.is_none() {
        return Some(rpc_error(
            id.unwrap_or(Value::Null),
            -32600,
            "invalid JSON-RPC request",
        ));
    }
    let id = id?;
    let params = request.get("params").cloned().unwrap_or_else(|| json!({}));
    let result = match method.unwrap() {
        "initialize" => {
            let requested = params
                .get("protocolVersion")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let version = match requested {
                "2024-11-05" | "2025-03-26" | "2025-06-18" => requested,
                _ => "2025-06-18",
            };
            json!({"protocolVersion": version, "capabilities": {"tools": {"listChanged": false}},
                "serverInfo": {"name": "vibeface", "version": env!("CARGO_PKG_VERSION")}})
        }
        "ping" => json!({}),
        "tools/list" => json!({"tools": tools_for(read_only, task_id)}),
        "tools/call" => {
            let result = async {
                let name = params
                    .get("name")
                    .and_then(Value::as_str)
                    .context("tool name is required")?;
                let arguments = params
                    .get("arguments")
                    .cloned()
                    .unwrap_or_else(|| json!({}));
                let command = tool_command_scoped(name, arguments, read_only, task_id)?;
                let request = CommandRequest {
                    id: format!("local-{}", uuid::Uuid::new_v4()),
                    command,
                };
                call_local(socket, &request).await
            }
            .await;
            match result {
                Ok(record) => {
                    json!({"content": [{"type": "text", "text": json!(record).to_string()}],
                    "isError": record.status != CommandStatus::Succeeded})
                }
                Err(error) => {
                    json!({"content": [{"type": "text", "text": format!("{error:#}")}], "isError": true})
                }
            }
        }
        _ => return Some(rpc_error(id, -32601, "method not found")),
    };
    Some(json!({"jsonrpc": "2.0", "id": id, "result": result}))
}

fn rpc_error(id: Value, code: i32, message: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
}

#[cfg(test)]
fn tools(read_only: bool) -> Vec<Value> {
    tools_for(read_only, None)
}

fn tools_for(read_only: bool, task_id: Option<&str>) -> Vec<Value> {
    let text = || json!({"type": "string", "minLength": 1, "maxLength": 65536});
    let cursor = || json!({"type": ["string", "null"], "maxLength": 65536});
    vec![
        tool(
            "session_list",
            "List Codex sessions. Use cursor for the next page.",
            json!({"cursor": cursor()}),
            &[],
        ),
        tool(
            "session_read",
            "Read a relevant Codex session and its available history.",
            json!({"thread_id": text()}),
            &["thread_id"],
        ),
        tool(
            "session_resume",
            "Load an existing Codex session for further control.",
            json!({"thread_id": text()}),
            &["thread_id"],
        ),
        tool(
            "session_turns",
            "List turns in a Codex session. Use cursor for the next page.",
            json!({"thread_id": text(), "cursor": cursor()}),
            &["thread_id"],
        ),
        tool(
            "session_start",
            "Start a Codex session in the explicitly authorized working directory.",
            json!({"cwd": text()}),
            &["cwd"],
        ),
        tool(
            "session_send",
            "Start a new turn only within the user's authorized task.",
            json!({"thread_id": text(), "text": text()}),
            &["thread_id", "text"],
        ),
        tool(
            "session_steer",
            "Send a steering message to a running turn within the authorized task.",
            json!({"thread_id": text(), "turn_id": text(), "text": text()}),
            &["thread_id", "turn_id", "text"],
        ),
        tool(
            "session_interrupt",
            "Interrupt the specified Codex turn when authorized.",
            json!({"thread_id": text(), "turn_id": text()}),
            &["thread_id", "turn_id"],
        ),
        tool(
            "session_status",
            "Read Bridge connectivity, current session state, and pending human decisions. This does not approve requests.",
            json!({}),
            &[],
        ),
        tool("task_list", "List tracked Codex tasks.", json!({}), &[]),
        tool(
            "task_read",
            "Read one tracked Codex task.",
            json!({"task_id": text()}),
            &["task_id"],
        ),
        tool(
            "task_create",
            "Create a tracked Codex task. Automatic continuation is disabled for MCP-created tasks.",
            json!({"task": task_input_schema()}),
            &["task"],
        ),
        tool(
            "task_update",
            "Update a tracked Codex task within its authorized scope.",
            json!({"task_id": text(), "update": task_update_schema(task_id.is_some())}),
            &["task_id", "update"],
        ),
    ].into_iter().filter(|tool| {
        let name = tool.get("name").and_then(Value::as_str).unwrap_or_default();
        if read_only {
            READ_ONLY_TOOLS.contains(&name)
        } else if task_id.is_some() {
            name != "task_create"
        } else {
            true
        }
    }).collect()
}

fn task_input_schema() -> Value {
    json!({
        "type": "object", "additionalProperties": false,
        "properties": {
            "id": {"type": "string", "minLength": 1, "maxLength": 128},
            "goal": {"type": "string", "minLength": 1, "maxLength": 65536},
            "cwd": {"type": "string", "minLength": 1, "maxLength": 65536},
            "acceptance_criteria": {"type": "array", "maxItems": 128, "items": {"type": "string", "minLength": 1, "maxLength": 65536}},
            "auto_continue": {"type": "boolean"},
            "decision_limit": {"type": "integer", "minimum": 0}
        },
        "required": ["id", "goal", "cwd", "acceptance_criteria", "auto_continue", "decision_limit"]
    })
}

fn task_update_schema(scoped: bool) -> Value {
    let mut schema = json!({
        "type": "object", "additionalProperties": false,
        "properties": {
            "status": {"type": ["string", "null"]},
            "summary": {"type": ["string", "null"], "maxLength": 65536},
            "evidence": {"type": ["string", "null"], "maxLength": 65536},
            "thread_ids": {"type": ["array", "null"], "maxItems": 128, "items": {"type": "string", "minLength": 1, "maxLength": 128}}
        }
    });
    if scoped {
        schema["properties"]
            .as_object_mut()
            .unwrap()
            .remove("thread_ids");
    }
    schema
}

fn tool(name: &str, description: &str, properties: Value, required: &[&str]) -> Value {
    json!({"name": name, "description": description,
        "inputSchema": {"type": "object", "properties": properties, "required": required, "additionalProperties": false}})
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Empty {}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Page {
    cursor: Option<String>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Thread {
    thread_id: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Turns {
    thread_id: String,
    cursor: Option<String>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Start {
    cwd: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Send {
    thread_id: String,
    text: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Steer {
    thread_id: String,
    turn_id: String,
    text: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Interrupt {
    thread_id: String,
    turn_id: String,
}

#[cfg(test)]
fn tool_command(name: &str, arguments: Value, read_only: bool) -> Result<Command> {
    tool_command_scoped(name, arguments, read_only, None)
}

fn tool_command_scoped(
    name: &str,
    arguments: Value,
    read_only: bool,
    task_id: Option<&str>,
) -> Result<Command> {
    ensure!(
        !read_only || READ_ONLY_TOOLS.contains(&name),
        "tool is unavailable in read-only management mode"
    );
    ensure!(arguments.is_object(), "tool arguments must be an object");
    for value in arguments.as_object().unwrap().values() {
        if let Some(text) = value.as_str() {
            ensure!(
                !text.trim().is_empty() && text.len() <= vibeface_protocol::MAX_TEXT_BYTES,
                "tool fields must be nonempty and at most 65536 bytes"
            );
        }
    }
    let mut command = match name {
        "session_list" => {
            let input: Page = serde_json::from_value(arguments)?;
            Command::ListThreads {
                cursor: input.cursor,
            }
        }
        "session_read" => {
            let input: Thread = serde_json::from_value(arguments)?;
            Command::ReadThread {
                thread_id: input.thread_id,
            }
        }
        "session_resume" => {
            let input: Thread = serde_json::from_value(arguments)?;
            Command::ResumeThread {
                thread_id: input.thread_id,
            }
        }
        "session_turns" => {
            let input: Turns = serde_json::from_value(arguments)?;
            Command::ListTurns {
                thread_id: input.thread_id,
                cursor: input.cursor,
            }
        }
        "session_start" => {
            let input: Start = serde_json::from_value(arguments)?;
            Command::StartThread { cwd: input.cwd }
        }
        "session_send" => {
            let input: Send = serde_json::from_value(arguments)?;
            Command::StartTurn {
                thread_id: input.thread_id,
                text: input.text,
            }
        }
        "session_steer" => {
            let input: Steer = serde_json::from_value(arguments)?;
            Command::SteerTurn {
                thread_id: input.thread_id,
                turn_id: input.turn_id,
                text: input.text,
            }
        }
        "session_interrupt" => {
            let input: Interrupt = serde_json::from_value(arguments)?;
            Command::InterruptTurn {
                thread_id: input.thread_id,
                turn_id: input.turn_id,
            }
        }
        "session_status" => {
            let _: Empty = serde_json::from_value(arguments)?;
            Command::Snapshot
        }
        "task_list" => {
            let _: Empty = serde_json::from_value(arguments)?;
            Command::ListTasks
        }
        "task_read" => {
            let input: TaskRead = serde_json::from_value(arguments)?;
            if let Some(scope) = task_id {
                ensure!(
                    scope == input.task_id,
                    "task is outside the current management scope"
                );
            }
            Command::ReadTask {
                task_id: input.task_id,
            }
        }
        "task_create" => {
            ensure!(
                task_id.is_none(),
                "task_create is unavailable in automatic task scope"
            );
            let input: TaskCreate = serde_json::from_value(arguments)?;
            let mut task = input.task;
            task.auto_continue = false;
            task.decision_limit = 0;
            Command::CreateTask { task }
        }
        "task_update" => {
            let input: TaskUpdateCommand = serde_json::from_value(arguments)?;
            if let Some(scope) = task_id {
                ensure!(
                    scope == input.task_id,
                    "task is outside the current management scope"
                );
                ensure!(
                    input.update.thread_ids.is_none(),
                    "automatic task scope cannot assign thread_ids"
                );
            }
            Command::UpdateTask {
                task_id: input.task_id,
                update: input.update,
            }
        }
        _ => bail!("unknown or unavailable tool"),
    };
    if let Some(scope) = task_id
        && matches!(
            name,
            "session_resume"
                | "session_start"
                | "session_send"
                | "session_steer"
                | "session_interrupt"
        )
    {
        command = Command::TaskCommand {
            task_id: scope.to_owned(),
            command: Box::new(command),
        };
    }
    command.validate().map_err(anyhow::Error::msg)?;
    Ok(command)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TaskRead {
    task_id: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TaskCreate {
    task: TaskInput,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TaskUpdateCommand {
    task_id: String,
    update: TaskUpdate,
}

fn current_task_id() -> Result<Option<String>> {
    let Ok(task_id) = std::env::var("VIBEFACE_MCP_TASK_ID") else {
        return Ok(None);
    };
    vibeface_protocol::validate_id(&task_id).map_err(anyhow::Error::msg)?;
    Ok(Some(task_id))
}

#[cfg(test)]
mod tests {
    use super::*;

    const TOOL_NAMES: [&str; 9] = [
        "session_list",
        "session_read",
        "session_resume",
        "session_turns",
        "session_start",
        "session_send",
        "session_steer",
        "session_interrupt",
        "session_status",
    ];

    #[test]
    fn exposed_tools_map_to_only_the_allowed_commands() {
        let cases = [
            (
                "session_list",
                json!({"cursor": "next"}),
                Command::ListThreads {
                    cursor: Some("next".into()),
                },
            ),
            (
                "session_read",
                json!({"thread_id": "t"}),
                Command::ReadThread {
                    thread_id: "t".into(),
                },
            ),
            (
                "session_resume",
                json!({"thread_id": "t"}),
                Command::ResumeThread {
                    thread_id: "t".into(),
                },
            ),
            (
                "session_turns",
                json!({"thread_id": "t"}),
                Command::ListTurns {
                    thread_id: "t".into(),
                    cursor: None,
                },
            ),
            (
                "session_start",
                json!({"cwd": "/project"}),
                Command::StartThread {
                    cwd: "/project".into(),
                },
            ),
            (
                "session_send",
                json!({"thread_id": "t", "text": "continue"}),
                Command::StartTurn {
                    thread_id: "t".into(),
                    text: "continue".into(),
                },
            ),
            (
                "session_steer",
                json!({"thread_id": "t", "turn_id": "u", "text": "stop here"}),
                Command::SteerTurn {
                    thread_id: "t".into(),
                    turn_id: "u".into(),
                    text: "stop here".into(),
                },
            ),
            (
                "session_interrupt",
                json!({"thread_id": "t", "turn_id": "u"}),
                Command::InterruptTurn {
                    thread_id: "t".into(),
                    turn_id: "u".into(),
                },
            ),
            ("session_status", json!({}), Command::Snapshot),
        ];
        assert_eq!(tools(false).len(), TOOL_NAMES.len() + 4);
        for (index, (name, input, expected)) in cases.into_iter().enumerate() {
            assert_eq!(tools(false)[index]["name"], name);
            assert_eq!(TOOL_NAMES[index], name);
            assert_eq!(tool_command(name, input, false).unwrap(), expected);
        }
        assert!(
            tool_command(
                "respond_request",
                json!({"request_id": 1, "response": {"decision": "accept"}}),
                false
            )
            .is_err()
        );
        assert!(tool_command("manage", json!({"prompt": "recursive"}), false).is_err());
        assert!(
            tool_command(
                "session_send",
                json!({"thread_id": "t", "text": "hello", "approve": true}),
                false
            )
            .is_err()
        );
        assert!(tool_command("session_read", json!({"thread_id": " "}), false).is_err());
    }

    #[test]
    fn task_scope_wraps_writes_and_blocks_cross_task_updates() {
        let scoped_update = tools_for(false, Some("task-1"))
            .into_iter()
            .find(|tool| tool["name"] == "task_update")
            .unwrap();
        assert!(
            scoped_update["inputSchema"]["properties"]["update"]["properties"]
                .get("thread_ids")
                .is_none()
        );
        let manual_update = tools(false)
            .into_iter()
            .find(|tool| tool["name"] == "task_update")
            .unwrap();
        assert!(
            manual_update["inputSchema"]["properties"]["update"]["properties"]
                .get("thread_ids")
                .is_some()
        );
        assert_eq!(
            tool_command_scoped(
                "session_send",
                json!({"thread_id":"t","text":"continue"}),
                false,
                Some("task-1")
            )
            .unwrap(),
            Command::TaskCommand {
                task_id: "task-1".into(),
                command: Box::new(Command::StartTurn {
                    thread_id: "t".into(),
                    text: "continue".into()
                })
            }
        );
        assert!(
            tool_command_scoped(
                "task_read",
                json!({"task_id":"other"}),
                false,
                Some("task-1")
            )
            .is_err()
        );
        assert!(
            tool_command_scoped(
                "task_update",
                json!({"task_id":"task-1","update":{"thread_ids":["t"]}}),
                false,
                Some("task-1")
            )
            .is_err()
        );
        assert!(tool_command_scoped("task_create", json!({"task":{"id":"task-1","goal":"g","cwd":"/p","acceptance_criteria":["done"],"auto_continue":true,"decision_limit":1}}), false, Some("task-1")).is_err());
        let command = tool_command("task_create", json!({"task":{"id":"task-1","goal":"g","cwd":"/p","acceptance_criteria":["done"],"auto_continue":true,"decision_limit":1}}), false).unwrap();
        assert_eq!(
            command,
            Command::CreateTask {
                task: TaskInput {
                    id: "task-1".into(),
                    goal: "g".into(),
                    cwd: "/p".into(),
                    acceptance_criteria: vec!["done".into()],
                    auto_continue: false,
                    decision_limit: 0
                }
            }
        );
    }

    #[tokio::test]
    async fn read_only_management_rejects_mutations_before_contacting_bridge() {
        let names: Vec<_> = tools(true)
            .into_iter()
            .map(|tool| tool["name"].as_str().unwrap().to_owned())
            .collect();
        assert_eq!(names, READ_ONLY_TOOLS);
        assert_eq!(
            tool_command("session_read", json!({"thread_id": "t"}), true).unwrap(),
            Command::ReadThread {
                thread_id: "t".into()
            }
        );
        for name in ["session_send", "session_interrupt"] {
            let response = handle_request(Path::new("/no-such-bridge.socket"), json!({
                "jsonrpc": "2.0", "id": 1, "method": "tools/call",
                "params": {"name": name, "arguments": {"thread_id": "t", "turn_id": "u", "text": "continue"}}
            }), true, None).await.unwrap();
            assert_eq!(response["result"]["isError"], true);
            assert!(
                response["result"]["content"][0]["text"]
                    .as_str()
                    .unwrap()
                    .contains("read-only management mode")
            );
        }
    }
}
