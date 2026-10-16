//! Safe, line-delimited CLI transports for Cursor Agent and Claude Code.
use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};
use std::{future::Future, process::Stdio, time::Duration};
use tokio::{
    io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWriteExt, BufReader},
    process::Command,
    sync::mpsc::UnboundedSender,
};
use uuid::Uuid;

use super::{
    EngineAction, EngineError, EngineResult, ExecutorConfig, ExecutorKind, ExecutorSession,
};

const MAX_LINE_BYTES: usize = 1_048_576;
const MAX_OUTPUT_BYTES: usize = 8 * 1024 * 1024;

pub async fn execute<F, Fut>(
    config: &ExecutorConfig,
    session: &ExecutorSession,
    action: EngineAction,
    persist_native: F,
    events: UnboundedSender<Value>,
) -> std::result::Result<EngineResult, EngineError>
where
    F: FnOnce(String) -> Fut,
    Fut: Future<Output = Result<()>>,
{
    if !matches!(action, EngineAction::Open {} | EngineAction::Send { .. }) {
        return Err(EngineError::rejected(
            session,
            "this CLI executor supports only open and send; read, steer, interrupt, and respond_request are unsupported",
        ));
    }
    if matches!(action, EngineAction::Open {}) && session.native_session_id.is_some() {
        return Err(EngineError::rejected(
            session,
            "execution session is already open",
        ));
    }
    // Claude has no model-free native session creation command. Allocate its real
    // UUID at first send, then persist it before starting the CLI.
    if session.executor == ExecutorKind::ClaudeCode && matches!(action, EngineAction::Open {}) {
        return Ok(EngineResult {
            native_session_id: None,
            result: json!({"state":"ready","native_session_pending":true}),
        });
    }

    let mut native = session.native_session_id.clone();
    let mut invoked = false;
    let mut session_started = false;
    let operation = async {
        if native.is_none() {
            let id = match session.executor {
                ExecutorKind::ClaudeCode => Uuid::new_v4().to_string(),
                ExecutorKind::CursorAgent => {
                    create_cursor_chat(config, session, &mut invoked).await?
                }
                ExecutorKind::Codex => bail!("Codex must use its native transport"),
            };
            vibeface_protocol::validate_id(&id).map_err(anyhow::Error::msg)?;
            native = Some(id.clone());
            persist_native(id)
                .await
                .context("persist native session before sending a prompt")?;
        }
        let id = native.as_deref().context("native session is not open")?;
        let result = match action {
            EngineAction::Open {} => json!({"state":"ready"}),
            EngineAction::Send { text } => {
                let outcome = send_process(config, session, id, &text, &events, &mut invoked).await;
                session_started = invoked;
                let summary = outcome?;
                if let Some(failure) = summary.failure {
                    // A provider terminal failure is a known result, unlike a lost stream.
                    invoked = false;
                    bail!(
                        "executor reported failure: {}",
                        redact_text(&failure, &secrets())
                    );
                }
                summary.result.context(
                    "executor stream ended without a terminal result; completion is unknown",
                )?
            }
            _ => unreachable!("unsupported actions were rejected before opening a session"),
        };
        Ok::<_, anyhow::Error>(EngineResult {
            native_session_id: native.clone(),
            result: redact_value(result, &secrets()),
        })
    };
    let outcome = tokio::time::timeout(Duration::from_secs(config.timeout_secs), operation).await;
    if session.executor == ExecutorKind::ClaudeCode && !session_started && !invoked {
        native = session.native_session_id.clone();
    }
    match outcome {
        Ok(Ok(result)) => Ok(result),
        Ok(Err(error)) => Err(EngineError {
            uncertain: invoked,
            native_session_id: native,
            message: redact_text(&format!("{error:#}"), &secrets()),
        }),
        Err(_) => Err(EngineError {
            uncertain: invoked,
            native_session_id: native,
            message: "executor process timed out; reconcile the native session before retrying"
                .into(),
        }),
    }
}

async fn create_cursor_chat(
    config: &ExecutorConfig,
    session: &ExecutorSession,
    invoked: &mut bool,
) -> Result<String> {
    let mut child = Command::new(&config.cursor_bin)
        .args(["create-chat", "--workspace"])
        .arg(&session.cwd)
        .current_dir(&session.cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .context("start Cursor create-chat")?;
    *invoked = true;
    let stdout = child
        .stdout
        .take()
        .context("Cursor create-chat stdout unavailable")?;
    let mut bytes = Vec::new();
    stdout
        .take((MAX_LINE_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .await?;
    ensure!(
        bytes.len() <= MAX_LINE_BYTES,
        "Cursor create-chat output is too large"
    );
    let status = child.wait().await.context("wait for Cursor create-chat")?;
    ensure!(
        status.success(),
        "Cursor create-chat exited without success"
    );
    parse_created_id(
        std::str::from_utf8(&bytes).context("Cursor create-chat returned invalid UTF-8")?,
    )
    .context("Cursor create-chat returned no valid native session ID")
}

async fn send_process(
    config: &ExecutorConfig,
    session: &ExecutorSession,
    id: &str,
    text: &str,
    events: &UnboundedSender<Value>,
    invoked: &mut bool,
) -> Result<StreamSummary> {
    let mut command = match session.executor {
        ExecutorKind::CursorAgent => {
            let mut command = Command::new(&config.cursor_bin);
            command
                .args([
                    "--resume",
                    id,
                    "--print",
                    "--output-format",
                    "stream-json",
                    "--sandbox",
                    "enabled",
                    "--workspace",
                ])
                .arg(&session.cwd);
            command
        }
        ExecutorKind::ClaudeCode => {
            let mut command = Command::new(&config.claude_bin);
            command.args([
                if session.native_session_id.is_some() {
                    "--resume"
                } else {
                    "--session-id"
                },
                id,
                "--print",
                "--verbose",
                "--output-format",
                "stream-json",
                "--permission-mode",
                "manual",
                "--permission-prompts",
                "none",
            ]);
            command
        }
        ExecutorKind::Codex => bail!("Codex must use its native transport"),
    };
    let mut child = command
        .current_dir(&session.cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        // Provider stderr can include credentials; surface only normalized errors.
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .context("start executor process")?;
    *invoked = true;
    let mut stdin = child.stdin.take().context("executor stdin unavailable")?;
    let stdout = child.stdout.take().context("executor stdout unavailable")?;
    let write = async {
        stdin.write_all(text.as_bytes()).await?;
        stdin.write_all(b"\n").await?;
        stdin.shutdown().await?;
        drop(stdin);
        Ok::<_, anyhow::Error>(())
    };
    let (_, summary) = tokio::try_join!(write, read_stream(stdout, session.executor, id, events))?;
    let status = child.wait().await.context("wait for executor process")?;
    ensure!(
        status.success() || summary.failure.is_some(),
        "executor exited unsuccessfully; completion is unknown"
    );
    Ok(summary)
}

#[derive(Default, Debug)]
struct StreamSummary {
    result: Option<Value>,
    failure: Option<String>,
}

async fn read_stream<R: AsyncRead + Unpin>(
    reader: R,
    executor: ExecutorKind,
    native_id: &str,
    events: &UnboundedSender<Value>,
) -> Result<StreamSummary> {
    let mut reader = BufReader::new(reader);
    let secret_values = secrets();
    let mut summary = StreamSummary::default();
    let mut total = 0usize;
    let mut line = Vec::new();
    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            if !line.is_empty() {
                observe_line(
                    &mut summary,
                    &line,
                    executor,
                    native_id,
                    events,
                    &secret_values,
                )?;
            }
            return Ok(summary);
        }
        let length = available
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(available.len(), |end| end + 1);
        total += length;
        ensure!(
            total <= MAX_OUTPUT_BYTES && line.len() + length <= MAX_LINE_BYTES,
            "executor stream exceeded output limits"
        );
        line.extend_from_slice(&available[..length]);
        let complete = line.last() == Some(&b'\n');
        reader.consume(length);
        if complete {
            observe_line(
                &mut summary,
                &line,
                executor,
                native_id,
                events,
                &secret_values,
            )?;
            line.clear();
        }
    }
}

fn observe_line(
    summary: &mut StreamSummary,
    line: &[u8],
    executor: ExecutorKind,
    native_id: &str,
    events: &UnboundedSender<Value>,
    secret_values: &[String],
) -> Result<()> {
    if line.iter().all(u8::is_ascii_whitespace) {
        return Ok(());
    }
    let value: Value =
        serde_json::from_slice(line).context("executor emitted invalid stream-json")?;
    let object = value
        .as_object()
        .context("executor stream event must be an object")?;
    ensure!(
        summary.result.is_none() && summary.failure.is_none(),
        "executor emitted data after its terminal result"
    );
    if let Some(observed) = object.get("session_id").and_then(Value::as_str) {
        ensure!(
            observed == native_id,
            "executor returned a different native session ID"
        );
    }
    let kind = object
        .get("type")
        .and_then(Value::as_str)
        .context("executor stream event has no type")?;
    if matches!(kind, "result" | "error") {
        let mut success = false;
        if kind == "result" {
            ensure!(
                object.get("session_id").and_then(Value::as_str) == Some(native_id),
                "executor terminal result has no matching native session ID"
            );
            let subtype = object
                .get("subtype")
                .and_then(Value::as_str)
                .context("executor terminal result has no status")?;
            let is_error = object
                .get("is_error")
                .and_then(Value::as_bool)
                .context("executor terminal result has no error indicator")?;
            success = subtype == "success" && !is_error;
            ensure!(
                success || is_error || subtype.starts_with("error"),
                "executor terminal result has an unknown status"
            );
        }
        if success {
            summary.result = Some(value);
        } else {
            summary.failure = Some(
                ["error", "message", "result", "subtype"]
                    .into_iter()
                    .find_map(|key| object.get(key).and_then(Value::as_str))
                    .unwrap_or("provider returned an unsuccessful terminal result")
                    .into(),
            );
        }
        return Ok(());
    }
    let executor = match executor {
        ExecutorKind::CursorAgent => "cursor_agent",
        ExecutorKind::ClaudeCode => "claude_code",
        ExecutorKind::Codex => "codex",
    };
    let _ = events.send(json!({"kind":"executor_progress","executor":executor,"native_session_id":native_id,"event":redact_value(value, secret_values)}));
    Ok(())
}

fn parse_created_id(output: &str) -> Option<String> {
    let output = output.trim();
    let parsed = serde_json::from_str::<Value>(output).ok();
    let id = parsed
        .as_ref()
        .and_then(|value| {
            value.as_str().or_else(|| {
                ["session_id", "sessionId", "chat_id", "chatId", "id"]
                    .into_iter()
                    .find_map(|key| value.get(key).and_then(Value::as_str))
            })
        })
        .unwrap_or(output);
    Uuid::parse_str(id).ok().map(|id| id.to_string())
}

fn secrets() -> Vec<String> {
    let mut secrets = std::env::vars_os()
        .filter_map(|(key, value)| {
            let (Ok(key), Ok(value)) = (key.into_string(), value.into_string()) else {
                return None;
            };
            let key = key.to_ascii_uppercase();
            (key.contains("TOKEN")
                || key.contains("SECRET")
                || key.contains("PASSWORD")
                || key.ends_with("_KEY"))
            .then_some(value)
        })
        .filter(|value| !value.is_empty())
        .collect::<Vec<_>>();
    secrets.sort_by_key(|value| std::cmp::Reverse(value.len()));
    secrets.dedup();
    secrets
}

fn redact_text(text: &str, secrets: &[String]) -> String {
    secrets.iter().fold(text.to_owned(), |text, secret| {
        text.replace(secret, "[REDACTED]")
    })
}

fn redact_value(value: Value, secrets: &[String]) -> Value {
    match value {
        Value::String(value) => Value::String(redact_text(&value, secrets)),
        Value::Array(values) => Value::Array(
            values
                .into_iter()
                .map(|value| redact_value(value, secrets))
                .collect(),
        ),
        Value::Object(values) => Value::Object(
            values
                .into_iter()
                .map(|(key, value)| (redact_text(&key, secrets), redact_value(value, secrets)))
                .collect(),
        ),
        value => value,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn observe(values: &[Value]) -> Result<StreamSummary> {
        let mut summary = StreamSummary::default();
        let (events, _receiver) = tokio::sync::mpsc::unbounded_channel();
        for value in values {
            observe_line(
                &mut summary,
                &serde_json::to_vec(value)?,
                ExecutorKind::ClaudeCode,
                "native-1",
                &events,
                &[],
            )?;
        }
        Ok(summary)
    }

    #[test]
    fn terminal_success_requires_success_status_and_matching_session() {
        let result = observe(&[
            json!({"type":"system","subtype":"init"}),
            json!({"type":"result","subtype":"success","is_error":false,"session_id":"native-1","result":"done"}),
        ]).unwrap();
        assert_eq!(result.result.unwrap()["result"], "done");
        assert!(result.failure.is_none());
        assert!(observe(&[json!({"type":"result","subtype":"success","is_error":false,"session_id":"different"})]).is_err());
    }

    #[test]
    fn failure_and_unfinished_streams_do_not_report_success() {
        let failure = observe(&[json!({"type":"result","subtype":"error","is_error":true,"session_id":"native-1","result":"denied"})]).unwrap();
        assert!(failure.result.is_none());
        assert_eq!(failure.failure.as_deref(), Some("denied"));
        let unfinished = observe(&[json!({"type":"assistant","message":"working"})]).unwrap();
        assert!(unfinished.result.is_none() && unfinished.failure.is_none());
        assert!(observe(&[json!({"type":"result","subtype":"success","is_error":false})]).is_err());
        assert!(observe(&[json!({"type":"result","session_id":"native-1"})]).is_err());
        let terminal =
            json!({"type":"result","subtype":"success","is_error":false,"session_id":"native-1"});
        assert!(observe(&[terminal.clone(), terminal.clone()]).is_err());
        assert!(observe(&[terminal, json!({"type":"assistant"})]).is_err());
    }

    #[test]
    fn progress_is_redacted_before_leaving_the_executor() -> Result<()> {
        let (events, mut receiver) = tokio::sync::mpsc::unbounded_channel();
        let mut summary = StreamSummary::default();
        observe_line(
            &mut summary,
            br#"{"type":"assistant","message":{"text":"credential-test-secret"}}"#,
            ExecutorKind::ClaudeCode,
            "native-1",
            &events,
            &["credential-test-secret".into()],
        )?;
        let progress = receiver.try_recv()?;
        assert_eq!(progress["event"]["message"]["text"], "[REDACTED]");
        assert!(!serde_json::to_string(&progress)?.contains("credential-test-secret"));
        Ok(())
    }

    #[test]
    fn native_id_and_redaction_are_bounded_and_literal() {
        assert_eq!(
            parse_created_id("{\"chatId\":\"c6b62c6f-7ead-4fd6-9922-e952131177ff\"}\n"),
            Some("c6b62c6f-7ead-4fd6-9922-e952131177ff".into())
        );
        assert_eq!(
            parse_created_id("c6b62c6f-7ead-4fd6-9922-e952131177ff\n"),
            Some("c6b62c6f-7ead-4fd6-9922-e952131177ff".into())
        );
        assert!(parse_created_id("Error: locked keychain").is_none());
        assert!(parse_created_id("authentication_token_with_no_session").is_none());
        assert_eq!(
            redact_value(json!({"result":"secret-value"}), &["secret-value".into()])["result"],
            "[REDACTED]"
        );
    }

    #[tokio::test]
    async fn oversized_lines_and_unfinished_streams_are_distinct() {
        let (events, _receiver) = tokio::sync::mpsc::unbounded_channel();
        let unfinished = b"{\"type\":\"assistant\"}\n";
        assert!(
            read_stream(
                &unfinished[..],
                ExecutorKind::ClaudeCode,
                "native-1",
                &events
            )
            .await
            .unwrap()
            .result
            .is_none()
        );
        assert!(
            read_stream(
                &vec![b'x'; MAX_LINE_BYTES + 1][..],
                ExecutorKind::ClaudeCode,
                "native-1",
                &events
            )
            .await
            .is_err()
        );
        assert!(
            read_stream(
                &b"not json\n"[..],
                ExecutorKind::ClaudeCode,
                "native-1",
                &events
            )
            .await
            .is_err()
        );
    }
}
