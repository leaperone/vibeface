use crate::codex::{CodexClient, CodexError, validate_request};
use anyhow::{Context, Result, anyhow, bail, ensure};
use serde::Deserialize;
use serde_json::Value;
use std::{
    io::{BufRead, Read, Write},
    path::Path,
    time::Duration,
};
use tokio::sync::mpsc;

const MAX_FRAME_BYTES: usize = 1_048_576;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NativeResponse {
    id: Value,
    result: Value,
}

fn response(line: &[u8]) -> Result<NativeResponse> {
    ensure!(
        line.len() <= MAX_FRAME_BYTES,
        "native response exceeds the size limit"
    );
    serde_json::from_slice(line).context("stdin requires a native response with id and result")
}

fn emit(value: &Value) -> Result<()> {
    let mut output = std::io::stdout().lock();
    serde_json::to_writer(&mut output, value)?;
    output.write_all(b"\n")?;
    output.flush()?;
    Ok(())
}

fn native_error(error: CodexError) -> anyhow::Error {
    if error.uncertain {
        anyhow!(
            "{}; native outcome is unknown; reconcile before retrying",
            error.message
        )
    } else {
        error.into()
    }
}

/// Calls one native method once. Watch accepts only explicit human responses.
pub async fn call(socket: &Path, method: &str, params: Value, watch: bool) -> Result<()> {
    validate_request(method, &params)?;
    let (events, mut received) = mpsc::unbounded_channel();
    let client = CodexClient::start(socket.into(), events);
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let event = received.recv().await.context("Codex event stream closed")?;
            match event["method"].as_str() {
                Some("bridge.codex_connected") => return Ok(()),
                Some("bridge.codex_disconnected") => {
                    bail!("Codex connection failed: {}", event["params"]["reason"])
                }
                _ => {}
            }
        }
    })
    .await
    .context("Codex connection timed out")??;

    let (input, mut lines) = mpsc::channel(8);
    if watch {
        std::thread::spawn(move || {
            let stdin = std::io::stdin();
            let mut stdin = stdin.lock();
            loop {
                let mut line = Vec::new();
                let read = (&mut stdin)
                    .take((MAX_FRAME_BYTES + 1) as u64)
                    .read_until(b'\n', &mut line);
                let stop = !matches!(read, Ok(size) if size > 0) || line.len() > MAX_FRAME_BYTES;
                if input.blocking_send(read.map(|_| line)).is_err() || stop {
                    break;
                }
            }
        });
    } else {
        drop(input);
    }
    let request = client.request(method, params);
    tokio::pin!(request);
    let mut finished = false;
    let mut input_closed = !watch;
    loop {
        tokio::select! {
            biased;
            result = &mut request, if !finished => {
                emit(&result.map_err(native_error)?)?;
                if !watch { return Ok(()); }
                finished = true;
            }
            event = received.recv() => {
                let event = event.context("Codex event stream closed")?;
                if event["method"] == "bridge.codex_disconnected" {
                    bail!("Codex disconnected: {}; pending work may require reconciliation", event["params"]["reason"]);
                }
                if watch && !event["method"].as_str().is_some_and(|method| method.starts_with("bridge.")) {
                    emit(&event)?;
                }
            }
            line = lines.recv(), if !input_closed => {
                match line {
                    Some(Ok(line)) if !line.is_empty() => {
                        let response = response(&line)?;
                        client.respond(response.id, response.result).await.map_err(native_error)?;
                    }
                    Some(Err(error)) => return Err(error).context("read native response from stdin"),
                    _ => input_closed = true,
                }
            }
            _ = tokio::signal::ctrl_c() => {
                ensure!(finished, "native request interrupted before its result; outcome is unknown");
                return Ok(());
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn stdin_accepts_explicit_native_responses_only() {
        let parsed = response(br#"{"id":"approval-7","result":{"decision":"decline"}}"#).unwrap();
        assert_eq!(parsed.id, json!("approval-7"));
        assert_eq!(parsed.result, json!({"decision":"decline"}));
        assert!(
            response(br#"{"id":7,"method":"item/commandExecution/requestApproval","params":{}}"#)
                .is_err()
        );
        assert!(response(br#"{"id":7}"#).is_err());
        assert!(response(&vec![b' '; MAX_FRAME_BYTES + 1]).is_err());
    }
}
