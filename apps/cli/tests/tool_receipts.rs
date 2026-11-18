use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

type Reply = (&'static str, Option<(u16, Value)>);

async fn request(socket: &mut TcpStream) -> Result<Value> {
    let mut bytes = Vec::new();
    let mut buffer = [0; 2048];
    loop {
        if let Some(end) = bytes.windows(4).position(|section| section == b"\r\n\r\n") {
            let headers = std::str::from_utf8(&bytes[..end])?;
            let length: usize = headers
                .lines()
                .filter_map(|line| line.split_once(':'))
                .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
                .context("fixture request needs Content-Length")?
                .1
                .trim()
                .parse()?;
            if bytes.len() >= end + 4 + length {
                return Ok(serde_json::from_slice(&bytes[end + 4..end + 4 + length])?);
            }
        }
        ensure!(bytes.len() < 65536, "request too large");
        let read = socket.read(&mut buffer).await?;
        ensure!(read != 0, "request truncated");
        bytes.extend_from_slice(&buffer[..read]);
    }
}

async fn run_cli(
    args: &[&str],
    replies: Vec<Reply>,
    configure_socket: bool,
) -> Result<std::process::Output> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let url = format!("http://{}", listener.local_addr()?);
    let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_vibeface"));
    command
        .arg("tool")
        .args(args)
        .env("VIBEFACE_NODE_URL", url)
        .env("VIBEFACE_TOOL_TOKEN", "fixture")
        .kill_on_drop(true);
    if !configure_socket {
        command.env_remove("VIBEFACE_CODEX_SOCKET");
    }
    #[cfg(unix)]
    let (native, path) = {
        let path = std::env::temp_dir().join(format!("vf-{}.sock", uuid::Uuid::new_v4().simple()));
        let native = std::os::unix::net::UnixListener::bind(&path)?;
        native.set_nonblocking(true)?;
        if configure_socket {
            command.env("VIBEFACE_CODEX_SOCKET", &path);
        }
        (native, path)
    };
    let mut replies = replies.into_iter();
    let run = command.output();
    tokio::pin!(run);
    let output = tokio::time::timeout(std::time::Duration::from_secs(15), async {
        loop {
            tokio::select! {
                biased;
                connection = listener.accept() => {
                    let (mut socket, _) = connection?;
                    let value = request(&mut socket).await?;
                    let (name, reply) = replies.next().context("unexpected additional HTTP tool call")?;
                    assert_eq!(value["toolName"],name);
                    if let Some((status, body)) = reply {
                        let body = body.to_string();
                        socket.write_all(format!("HTTP/1.1 {status} Fixture\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await?;
                    }
                }
                output = &mut run => break Ok::<_,anyhow::Error>(output?),
            }
        }
    }).await??;
    assert!(replies.next().is_none(), "expected an HTTP request");
    #[cfg(unix)]
    {
        let attempted = native.accept();
        drop(native);
        std::fs::remove_file(path)?;
        assert_eq!(
            attempted.unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock,
            "unexpected native dispatch"
        );
    }
    Ok(output)
}

async fn run_failure(args: &[&str], replies: Vec<Reply>) -> Result<Value> {
    failure_record(run_cli(args, replies, true).await?)
}

fn failure_record(output: std::process::Output) -> Result<Value> {
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8(output.stderr)?;
    assert_eq!(stderr.lines().count(), 1, "{stderr}");
    Ok(serde_json::from_str(stderr.trim())?)
}

#[tokio::test]
async fn tool_http_failures_keep_server_evidence_and_lost_responses_stay_unknown() -> Result<()> {
    for (approval, execution, confirmation) in [
        ("denied", "not_started", "confirmed"),
        ("allowed", "unknown", "unknown"),
        ("unknown", "unknown", "unknown"),
    ] {
        let expected = json!({"error":"provider decision","code":"tool_execution_failed","call_record":{
            "tool":"apply_provider_config","call_ref":null,"stage":"execution",
            "execution":execution,"confirmation":confirmation,"approval":approval,"retry":"none"}});
        let reply = (approval != "unknown").then(|| (400, expected.clone()));
        let actual = run_failure(
            &[
                "apply_provider_config",
                "--json",
                r#"{"question_id":"fixture-question"}"#,
            ],
            vec![("apply_provider_config", reply)],
        )
        .await?;
        if approval == "unknown" {
            assert_eq!(actual["call_record"], expected["call_record"]);
        } else {
            assert_eq!(actual, expected);
        }
    }
    Ok(())
}

#[tokio::test]
async fn malformed_success_and_error_receipts_keep_the_original_write_unknown() -> Result<()> {
    let record = json!({"tool":"remember","call_ref":"one","stage":"result","execution":"unknown",
        "confirmation":"pending","approval":"not_required","retry":"lookup_same_ref"});
    let mut responses = vec![
        (200, json!({"result":null})),
        (200, json!({"result":{}})),
        (
            400,
            json!({"error":"invalid","code":"tool_execution_failed","call_record":{}}),
        ),
    ];
    for (key, value) in [
        ("tool", "forget"),
        ("call_ref", "different"),
        ("stage", "lookup"),
        ("retry", "none"),
        ("approval", "allowed"),
        ("execution", "read"),
        ("confirmation", "confirmed"),
    ] {
        let mut altered = record.clone();
        altered[key] = json!(value);
        responses.push((200, json!({"result":{"call_record":altered}})));
    }
    let mut wrong_effect = record;
    wrong_effect["execution"] = json!("read");
    wrong_effect["confirmation"] = json!("confirmed");
    wrong_effect["retry"] = json!("none");
    responses.push((200, json!({"result":{"call_record":wrong_effect}})));
    for response in responses {
        let actual = run_failure(
            &["remember", "--json", r#"{"text":"note","call_ref":"one"}"#],
            vec![("remember", Some(response))],
        )
        .await?;
        assert_eq!(
            actual["call_record"],
            json!({"tool":"remember","call_ref":"one","stage":"execution","execution":"unknown",
            "confirmation":"unknown","approval":"unknown","retry":"lookup_same_ref"})
        );
    }
    Ok(())
}

#[tokio::test]
async fn lost_readiness_before_registration_is_not_started() -> Result<()> {
    let absent = Some((
        200,
        json!({"result":{"found":false,"call_ref":"one","call_record":{
        "tool":"call_lookup","call_ref":"one","stage":"lookup","execution":"unknown","confirmation":"unknown",
        "approval":"unknown","retry":"lookup_same_ref"}}}),
    ));
    let actual = run_failure(
        &[
            "dispatch_work",
            "--json",
            r#"{"thread":"t","prompt":"hello","call_ref":"one"}"#,
        ],
        vec![
            ("call_lookup", absent.clone()),
            ("wait_work", None),
            ("call_lookup", absent),
        ],
    )
    .await?;
    assert_eq!(
        actual["call_record"],
        json!({"tool":"dispatch_work","call_ref":"one","stage":"execution","execution":"not_started",
        "confirmation":"confirmed","approval":"unknown","retry":"none"})
    );
    Ok(())
}

#[tokio::test]
async fn invalid_tool_arguments_emit_one_validation_receipt_without_a_request() -> Result<()> {
    for (name, raw) in [("dispatch_work", r#"{"thread":"t"}"#), ("remember", "{")] {
        let actual = run_failure(&[name, "--json", raw, "--call-ref", "one"], vec![]).await?;
        assert_eq!(actual["code"], "invalid_tool_arguments");
        assert_eq!(
            actual["call_record"],
            json!({"tool":name,"call_ref":"one","stage":"validation",
            "execution":"not_started","confirmation":"confirmed","approval":"unknown","retry":"none"})
        );
    }
    Ok(())
}

#[tokio::test]
async fn provider_application_requires_the_requested_question_and_decision() -> Result<()> {
    use vibeface_control::tool_receipt::{Approval, Execution};
    let args = [
        "apply_provider_config",
        "--json",
        r#"{"question_id":"q-one"}"#,
    ];
    for (question, status, execution, approval, valid) in [
        (
            "q-two",
            "applied",
            Execution::Applied,
            Approval::Allowed,
            false,
        ),
        (
            "q-one",
            "applied",
            Execution::Applied,
            Approval::NotRequired,
            false,
        ),
        (
            "q-two",
            "declined",
            Execution::NotStarted,
            Approval::Denied,
            false,
        ),
        (
            "q-one",
            "applied",
            Execution::Applied,
            Approval::Allowed,
            true,
        ),
        (
            "q-one",
            "declined",
            Execution::NotStarted,
            Approval::Denied,
            true,
        ),
    ] {
        let mut result = match status {
            "applied" => {
                json!({"status":"applied","question_id":question,"applied":[],"roots_added":1,"also_filled":[]})
            }
            _ => json!({"status":"declined","question_id":question,"answer":"不允许"}),
        };
        result["call_record"] =
            execution
                .with(approval)
                .record("apply_provider_config", None, "result");
        let output = run_cli(
            &args,
            vec![(
                "apply_provider_config",
                Some((200, json!({"result":result}))),
            )],
            true,
        )
        .await?;
        if valid {
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            assert_eq!(serde_json::from_slice::<Value>(&output.stdout)?, result);
        } else {
            let record = failure_record(output)?["call_record"].clone();
            assert_eq!(
                record,
                Execution::Unknown.with(Approval::Unknown).record(
                    "apply_provider_config",
                    None,
                    "execution"
                )
            );
        }
    }
    let refused = json!({"error":"work changes require a live agent invocation",
        "code":"tool_not_authorized","call_record":Execution::NotStarted.with(Approval::Denied)
            .record("apply_provider_config",None,"authorization")});
    assert_eq!(
        run_failure(
            &args,
            vec![("apply_provider_config", Some((400, refused.clone())))]
        )
        .await?,
        refused
    );
    Ok(())
}

#[tokio::test]
async fn composite_dispatch_binds_only_the_work_it_tracked() -> Result<()> {
    use vibeface_control::tool_receipt::{Approval, Execution};
    let args = [
        "dispatch_work",
        "--json",
        r#"{"title":"fixture","cwd":"/tmp","prompt":"hello","call_ref":"one"}"#,
    ];
    let reference = json!("one");
    let tracked = json!({"work_id":"work-one","call_ref":"one","may_send":true,
        "resolved":{"provider":"codex","model":null},
        "call_record":Execution::Pending.with(Approval::Unknown).record("track_work",Some(&reference),"result")});
    let ready = json!({"agent":"codex","thread_at":null,
        "call_record":Execution::Read.with(Approval::NotRequired).record("wait_work",None,"result")});
    for work in ["work-two", "work-one"] {
        // No native socket is configured. The real dispatch reports the rejection through bind_work.
        let bound = json!({"work_id":work,"state":{"state":"finished","outcome":"rejected"},
            "call_record":Execution::NotStarted.with(Approval::Unknown).record("bind_work",None,"result")});
        let output = run_cli(
            &args,
            vec![
                ("track_work", Some((200, json!({"result":tracked})))),
                ("wait_work", Some((200, json!({"result":ready})))),
                ("bind_work", Some((200, json!({"result":bound})))),
            ],
            false,
        )
        .await?;
        if work == "work-one" {
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            let result: Value = serde_json::from_slice(&output.stdout)?;
            assert_eq!(result["work_id"], work);
            assert_eq!(
                result["call_record"],
                Execution::NotStarted.with(Approval::Unknown).record(
                    "dispatch_work",
                    Some(&reference),
                    "result"
                )
            );
        } else {
            let record = failure_record(output)?["call_record"].clone();
            assert_eq!(
                record,
                Execution::Unknown.with(Approval::Unknown).record(
                    "dispatch_work",
                    Some(&reference),
                    "execution"
                )
            );
        }
    }
    Ok(())
}
