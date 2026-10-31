//! The coordination Agent's own path to the App Server. It talks to the socket directly; the
//! Node only registers dispatched work before the send and settles it afterwards.
use anyhow::{Context as _, Result, bail, ensure};
use serde_json::{Value, json};
use std::{future::Future, path::Path};
use vibeface_bridge::{
    codex::{DirectSession, Outgoing, StartReport},
    failure::{ExecFailure, FailureKind},
};
use vibeface_protocol::now_ms;

fn outgoing_model(tracked: &Value) -> Result<Option<String>> {
    let value = tracked
        .get("resolved")
        .context("the Node returned no frozen model selection")?;
    value
        .get("model")
        .context("the frozen model selection has no model")?;
    let resolved: crate::model::ResolvedModel = serde_json::from_value(value.clone())
        .context("the Node returned an invalid frozen model selection")?;
    Ok(resolved.model)
}

fn thread_arguments(args: &Value, cwd: &str) -> Value {
    let mut args = args.clone();
    args["cwd"] = json!(cwd);
    if args["title"]
        .as_str()
        .is_none_or(|title| title.trim().is_empty())
    {
        let text = args["prompt"].as_str().unwrap_or_default();
        let line = text
            .lines()
            .find(|line| !line.trim().is_empty())
            .unwrap_or(text);
        args["title"] = json!(line.trim().chars().take(120).collect::<String>());
    }
    args
}

async fn readback<L, LF, T, TF>(args: &Value, lookup: &L, track: &T) -> Result<Option<Value>>
where
    L: Fn(String) -> LF,
    LF: Future<Output = Result<Value>>,
    T: Fn(Value) -> TF,
    TF: Future<Output = Result<Value>>,
{
    let found = lookup(args["call_ref"].as_str().unwrap_or_default().to_owned()).await?;
    if found["found"] == false {
        return Ok(None);
    }
    ensure!(
        found.get("conversation_id").is_some() && found.get("work_id").is_some(),
        "call_ref has no confirmed work projection; preserve it and query call_lookup before retrying"
    );
    outgoing_model(&found)?;
    let work: crate::model::WorkProjection =
        serde_json::from_value(found).context("the Node returned an invalid work projection")?;
    let args = if work.kind == crate::model::WorkKind::Agent(crate::providers::Provider::Codex)
        && work.thread.is_some()
        && args["thread"].is_string()
    {
        thread_arguments(args, &work.cwd)
    } else {
        args.clone()
    };
    let tracked = track(args).await?;
    outgoing_model(&tracked)?;
    ensure!(
        tracked["may_send"] == false,
        "durable work readback must not grant another send"
    );
    Ok(Some(tracked))
}

/// Registers the work with the Node (`track`); a work the Node runs on Codex is sent here.
/// The CLI reports its thread before the send, sends the prompt with `clientUserMessageId` =
/// work ID and reports how the send ended (`bind`). Without `thread` the work starts a turn in
/// a new thread (or the one it continues); with `thread` it goes into that existing thread,
/// steering its running turn or starting one. Durable retries are revalidated before readiness;
/// a first dispatch reads the thread's directory before registering. Only
/// the registration that created the work sends, and only inside its window. `ready` names the
/// provider the Node runs the work on, after waiting for the Node's own App Server
/// connection when that is Codex, so a send right after the Node starts is not refused while
/// the App Server comes up. A first send into a thread fails, with nothing registered, when `ready`
/// cannot be asked: without the answer it would skip reading the thread.
pub async fn dispatch<L, LF, T, TF, B, BF, R, RF>(
    args: &Value,
    lookup: L,
    track: T,
    bind: B,
    ready: R,
    socket: Option<&Path>,
) -> Result<Value>
where
    L: Fn(String) -> LF,
    LF: Future<Output = Result<Value>>,
    T: Fn(Value) -> TF,
    TF: Future<Output = Result<Value>>,
    B: Fn(String, StartReport) -> BF,
    BF: Future<Output = Result<Value>>,
    R: Fn(Value) -> RF,
    RF: Future<Output = Result<Option<String>>>,
{
    let original = args;
    let mut args = args.clone();
    let existing = args["thread"].as_str().map(str::to_owned);
    if existing.is_some()
        && let Some(tracked) = readback(original, &lookup, &track).await?
    {
        return Ok(tracked);
    }
    // A Codex send reads its thread before registering, so a missing thread leaves nothing
    // behind. The Node checks the session another provider would take the work in.
    let prepared = async {
        let readied = match &existing {
            Some(thread) => {
                let agent = ready(args.clone()).await.with_context(|| {
                    format!("cannot ask the Node which agent has thread {thread}; this attempt registered or sent nothing, so the send can be retried")
                })?;
                Some(agent.with_context(|| {
                    format!("no execution agent is ready for thread {thread}; this attempt registered or sent nothing, so the send can be retried")
                })?)
            }
            None => None,
        };
        if let (Some(thread), Some("codex")) = (&existing, readied.as_deref()) {
            let socket = socket.context("--socket or VIBEFACE_CODEX_SOCKET is required")?;
            let mut session = DirectSession::connect(socket)
                .await
                .map_err(|error| anyhow::anyhow!(error.message))?;
            let read = session
                .request("thread/read", json!({"threadId":thread}))
                .await;
            session.close().await;
            let read = read
                .map_err(|error| anyhow::anyhow!("cannot read thread {thread}: {}", error.message))?;
            ensure!(
                read["thread"]["id"].as_str() == Some(thread.as_str()),
                "thread {thread} was not returned by the App Server"
            );
            // Codex refuses direct input on a sub-agent; its parent thread drives it.
            if let Some(sub_agent) = read["thread"]["source"].get("subAgent") {
                match (read["thread"]["parentThreadId"].as_str())
                    .or(sub_agent["thread_spawn"]["parent_thread_id"].as_str())
                {
                    Some(parent) => bail!(
                        "thread {thread} is a Codex sub-agent and takes no direct input; send to its parent thread {parent} instead"
                    ),
                    None => bail!(
                        "thread {thread} is a Codex sub-agent and takes no direct input; dispatch it without thread"
                    ),
                }
            }
            args = thread_arguments(&args, read["thread"]["cwd"].as_str().unwrap_or_default());
        }
        Ok::<_, anyhow::Error>(readied)
    }.await;
    let readied = match prepared {
        Ok(readied) => readied,
        Err(error) => {
            return match readback(original, &lookup, &track).await? {
                Some(tracked) => Ok(tracked),
                None => Err(error),
            };
        }
    };
    let tracked = match track(args.clone()).await {
        Ok(tracked) => tracked,
        Err(error) => return match readback(original, &lookup, &track).await {
            Ok(Some(tracked)) => Ok(tracked),
            Ok(None) => Err(error.context("dispatch result is unknown; preserve call_ref and query call_lookup before retrying")),
            Err(readback) => Err(error.context(format!("dispatch result is unknown; durable readback failed: {readback:#}; preserve call_ref"))),
        },
    };
    if tracked["may_send"] != true {
        return Ok(tracked);
    }
    let model = outgoing_model(&tracked)?;
    let work_id = tracked["work_id"]
        .as_str()
        .context("the Node returned no work ID")?
        .to_owned();
    let report = |report| bind(work_id.clone(), report);
    let fail = |failure: ExecFailure| report(StartReport::Rejected(failure));
    // Only a wait for the App Server; the send below reports whatever it meets.
    if readied.is_none() {
        let _ = ready(args.clone()).await;
    }
    let Some(socket) = socket else {
        return fail(ExecFailure::new(
            FailureKind::Transport,
            "--socket or VIBEFACE_CODEX_SOCKET is required",
        ))
        .await;
    };
    let mut session = match DirectSession::connect(socket).await {
        Ok(session) => session,
        Err(error) => return fail(error.failure()).await,
    };
    let thread = match tracked["thread_id"].as_str() {
        Some(thread) => thread.to_owned(),
        None => {
            let started = session
                .request(
                    "thread/start",
                    json!({"cwd":tracked["cwd"],"approvalPolicy":"never","sandbox":"danger-full-access"}),
                )
                .await;
            match started.map(|reply| reply["thread"]["id"].as_str().map(str::to_owned)) {
                Ok(Some(thread)) => thread,
                Ok(None) => {
                    let failure =
                        ExecFailure::new(FailureKind::Unknown, "thread/start returned no thread");
                    return fail(failure).await;
                }
                Err(error) => return fail(error.failure()).await,
            }
        }
    };
    report(StartReport::Thread {
        thread_id: thread.clone(),
    })
    .await?;
    if now_ms() >= tracked["send_by_ms"].as_u64().unwrap_or(0) {
        let failure = ExecFailure::new(
            FailureKind::TimedOut,
            "the send window passed before the turn was sent",
        );
        return fail(failure).await;
    }
    let outgoing = Outgoing {
        thread_id: thread,
        text: args["prompt"].as_str().unwrap_or_default().into(),
        client_id: work_id.clone(),
        model,
        effort: args["effort"].as_str().map(str::to_owned),
        images: serde_json::from_value(args["images"].clone()).unwrap_or_default(),
        start_only: existing.is_none(),
        full_access: true,
    };
    let sent = session.send(&outgoing).await;
    session.close().await;
    report(sent).await
}

/// App Server methods that start, steer or stop a turn. A turn the Node did not register is
/// one it cannot watch, report or stop, so these go through dispatch_work and interrupt_work.
const TURN_WRITES: &[&str] = &[
    "turn/start",
    "turn/steer",
    "turn/interrupt",
    "thread/start",
    "thread/fork",
    "thread/compact/start",
    "thread/shellCommand",
    "thread/revert",
    "thread/inject_items",
    "review/start",
];

/// `vibeface codex call|watch`: a passthrough to the App Server socket for everything but
/// the turn writes above.
pub async fn run(socket: &Path, command: Command) -> Result<()> {
    if let Command::Call { method, .. } = &command
        && TURN_WRITES.contains(&method.as_str())
    {
        bail!(
            "`codex call {method}` would start, steer or stop a turn the Node does not track; inside a bot turn start or steer with `vibeface codex send` or dispatch_work and stop with interrupt_work"
        );
    }
    let mut session = DirectSession::connect(socket)
        .await
        .map_err(|error| anyhow::anyhow!(error.message))?;
    match command {
        Command::Call { method, params } => {
            let params: Value = match params {
                Some(params) => serde_json::from_str(&params).context("params must be JSON")?,
                None => json!({}),
            };
            let thread_id = params["threadId"].as_str().map(str::to_owned);
            let reply = session.request(&method, params).await;
            crate::logging::send_to_node(match &reply {
                Ok(_) => json!({"level":"info","event":"codex.call","message":format!("{method} succeeded"),"thread_id":thread_id}),
                Err(error) => json!({"level":"warn","event":"codex.call_failed","message":format!("{method} failed: {}", error.message),"thread_id":thread_id,"fields":{"uncertain":error.uncertain}}),
            })
            .await;
            match reply {
                Ok(result) => println!("{result}"),
                Err(error) if error.uncertain => bail!(
                    "{}; the result is unknown, read the state back before calling again",
                    error.message
                ),
                Err(error) => bail!(error.message),
            }
        }
        Command::Watch { thread_id } => {
            session
                .subscribe(&thread_id)
                .await
                .map_err(|error| anyhow::anyhow!(error.message))?;
            loop {
                let notification = session
                    .notification()
                    .await
                    .map_err(|error| anyhow::anyhow!(error.message))?;
                println!("{notification}");
            }
        }
    }
    session.close().await;
    Ok(())
}

pub enum Command {
    Call {
        method: String,
        params: Option<String>,
    },
    Watch {
        thread_id: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::{SinkExt as _, StreamExt as _};
    use std::sync::{Arc, Mutex};

    #[test]
    fn thread_arguments_use_the_current_prompt_and_keep_explicit_titles() {
        let args = json!({"prompt":format!("\n  {}  \nnext", "字".repeat(130)),"cwd":"ignored"});
        let normalized = thread_arguments(&args, "/original");
        assert_eq!(normalized["cwd"], "/original");
        assert_eq!(normalized["title"], "字".repeat(120));
        for title in [Value::Null, json!("  "), json!(42)] {
            let mut args = args.clone();
            args["title"] = title;
            assert_eq!(
                thread_arguments(&args, "/original")["title"],
                "字".repeat(120)
            );
        }
        let args = json!({"prompt":"changed","title":" explicit title "});
        assert_eq!(
            thread_arguments(&args, "/original")["title"],
            " explicit title "
        );
    }

    #[test]
    fn outgoing_model_requires_the_frozen_selection_and_keeps_the_provider_default() -> Result<()> {
        assert_eq!(
            outgoing_model(
                &json!({"resolved":{"provider":"codex","model":"execution-default"},"model":"request"})
            )?,
            Some("execution-default".into())
        );
        assert_eq!(
            outgoing_model(
                &json!({"resolved":{"provider":"codex","model":null},"model":"request"})
            )?,
            None
        );
        for tracked in [
            json!({"model":"request"}),
            json!({"resolved":null}),
            json!({"resolved":{"provider":"codex"}}),
            json!({"resolved":{"provider":"unknown","model":null}}),
        ] {
            assert!(outgoing_model(&tracked).is_err());
        }
        Ok(())
    }

    #[tokio::test]
    async fn direct_and_discovered_thread_calls_derive_the_same_title_and_directory() -> Result<()>
    {
        let dir =
            std::path::PathBuf::from(format!("/tmp/vf-cli-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir)?;
        let socket = dir.join("app.sock");
        let listener = tokio::net::UnixListener::bind(&socket)?;
        let methods = Arc::new(Mutex::new(vec![]));
        let seen = methods.clone();
        let server = tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let Ok(mut server) = tokio_tungstenite::accept_async(stream).await else {
                    continue;
                };
                while let Some(Ok(message)) = server.next().await {
                    let Ok(request) = serde_json::from_slice::<Value>(&message.into_data()) else {
                        continue;
                    };
                    seen.lock().unwrap().push(request["method"].clone());
                    let result = match request["method"].as_str() {
                        Some("initialize") => json!({"userAgent":"codex/fixture"}),
                        Some("thread/read") => {
                            json!({"thread":{"id":"t","cwd":"/work/tree","source":"cli"}})
                        }
                        _ => json!({}),
                    };
                    if request.get("id").is_some() {
                        let _ = server
                            .send(
                                json!({"id":request["id"],"result":result})
                                    .to_string()
                                    .into(),
                            )
                            .await;
                    }
                }
            }
        });
        let observed = Arc::new(Mutex::new(vec![]));
        let args = json!({"thread":"t","prompt":"follow up","call_ref":"r"});
        for (name, args) in [
            ("dispatch_work", args.clone()),
            (
                "tool_call",
                json!({"name":"dispatch_work","arguments":args}),
            ),
        ] {
            let (tool, args) = crate::tools::Tool::invocation(name, args)?;
            assert_eq!(tool, crate::tools::Tool::DispatchWork);
            let observed = observed.clone();
            dispatch(
                &args,
                |_| async { Ok(json!({"found":false})) },
                move |args| {
                    observed.lock().unwrap().push(args);
                    async { Ok(json!({"may_send":false,"work_id":"existing"})) }
                },
                |_, _| async { bail!("an existing call must not bind or send again") },
                |_| async { Ok(Some("codex".to_owned())) },
                Some(&socket),
            )
            .await?;
        }
        let observed = observed.lock().unwrap();
        assert_eq!(observed.len(), 2);
        assert_eq!(observed[0], observed[1]);
        assert_eq!(observed[0]["cwd"], "/work/tree");
        assert_eq!(observed[0]["title"], "follow up");
        assert_eq!(observed[0]["call_ref"], "r");
        assert_eq!(
            methods
                .lock()
                .unwrap()
                .iter()
                .filter(|method| *method == "thread/read")
                .count(),
            2
        );
        assert!(!methods.lock().unwrap().iter().any(|method| {
            method
                .as_str()
                .is_some_and(|method| method.starts_with("turn/"))
        }));
        server.abort();
        std::fs::remove_dir_all(dir)?;
        Ok(())
    }

    #[tokio::test]
    async fn a_codex_sub_agent_thread_is_refused_before_anything_is_registered() -> Result<()> {
        let dir =
            std::path::PathBuf::from(format!("/tmp/vf-cli-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir)?;
        let socket = dir.join("app.sock");
        let listener = tokio::net::UnixListener::bind(&socket)?;
        let methods = Arc::new(Mutex::new(vec![]));
        let seen = methods.clone();
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let Ok(mut server) = tokio_tungstenite::accept_async(stream).await else {
                    continue;
                };
                while let Some(Ok(message)) = server.next().await {
                    let Ok(request) = serde_json::from_slice::<Value>(&message.into_data()) else {
                        continue;
                    };
                    seen.lock().unwrap().push(request["method"].clone());
                    let result = match request["method"].as_str() {
                        Some("initialize") => json!({"userAgent":"codex/fixture"}),
                        Some("thread/read") => json!({"thread":{"id":"sub","cwd":"/w",
                            "parentThreadId":"lead","source":{"subAgent":{"thread_spawn":
                            {"depth":1,"parent_thread_id":"lead"}}}}}),
                        _ => json!({}),
                    };
                    if request.get("id").is_some() {
                        let reply = json!({"id":request["id"],"result":result});
                        let _ = server.send(reply.to_string().into()).await;
                    }
                }
            }
        });
        let tracked = Arc::new(Mutex::new(false));
        let track = |_| {
            *tracked.lock().unwrap() = true;
            async { Ok(json!({"may_send":true,"work_id":"w"})) }
        };
        let args = json!({"thread":"sub","prompt":"p","title":"t","call_ref":"r","agent":"codex"});
        let refused = dispatch(
            &args,
            |_| async { Ok(json!({"found":false})) },
            track,
            |_, _| async { Ok(json!({})) },
            |_| async { Ok(Some("codex".to_owned())) },
            Some(&socket),
        )
        .await;
        std::fs::remove_dir_all(&dir)?;
        let refused = refused.expect_err("Codex takes no direct input on a sub-agent");
        assert!(
            refused.to_string().contains("parent thread lead"),
            "{refused:#}"
        );
        assert!(!*tracked.lock().unwrap(), "no work was registered");
        assert!(
            !methods.lock().unwrap().iter().any(|method| method
                .as_str()
                .is_some_and(|method| method.starts_with("turn/") || method == "thread/resume")),
            "nothing was sent into the thread"
        );
        Ok(())
    }

    #[tokio::test]
    async fn a_send_into_a_thread_fails_when_the_node_cannot_say_who_has_it() -> Result<()> {
        let tracked = Arc::new(Mutex::new(false));
        let track = |_| {
            *tracked.lock().unwrap() = true;
            async { Ok(json!({"may_send":true,"work_id":"w"})) }
        };
        let args = json!({"thread":"t-1","prompt":"p","title":"t","call_ref":"r"});
        let failed = dispatch(
            &args,
            |_| async { Ok(json!({"found":false})) },
            track,
            |_, _| async { Ok(json!({})) },
            |_| async { bail!("the Node did not answer") },
            Some(Path::new("/nonexistent/app.sock")),
        )
        .await;
        let failed = failed.expect_err("an unanswered ready is not a skipped read");
        assert!(failed.to_string().contains("can be retried"), "{failed:#}");
        assert!(!*tracked.lock().unwrap(), "no work was registered");
        let failed = dispatch(
            &args,
            |_| async { Ok(json!({"found":false})) },
            track,
            |_, _| async { Ok(json!({})) },
            |_| async { Ok(None) },
            None,
        )
        .await
        .expect_err("ready None is a first-dispatch failure");
        assert!(
            failed.to_string().contains("no execution agent"),
            "{failed:#}"
        );
        assert!(!*tracked.lock().unwrap(), "no work was registered");
        Ok(())
    }

    #[tokio::test]
    async fn a_pending_dispatch_summary_cannot_authorize_another_admission() {
        let args = json!({"thread":"t","prompt":"p","call_ref":"r"});
        let failure = dispatch(
            &args,
            |_| async {
                Ok(json!({"work_id":"w","call_ref":"r","state":"pending","prompt":"summary"}))
            },
            |_| async { bail!("must not track an unconfirmed work") },
            |_, _| async { bail!("must not bind or send") },
            |_| async { bail!("must not ask readiness") },
            None,
        )
        .await
        .expect_err("a pending dispatch has no frozen admission");
        assert!(failure.to_string().contains("no confirmed work projection"));
    }

    #[tokio::test]
    async fn a_failed_track_with_no_durable_record_keeps_the_result_unknown() {
        let args = json!({"thread":"t","prompt":"p","call_ref":"r","cwd":"/w","title":"p"});
        let lookups = std::sync::atomic::AtomicUsize::new(0);
        let tracks = std::sync::atomic::AtomicUsize::new(0);
        let failed = dispatch(
            &args,
            |_| {
                lookups.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                async { Ok(json!({"found":false})) }
            },
            |_| {
                tracks.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                async { bail!("track response lost") }
            },
            |_, _| async { bail!("must not bind") },
            |_| async { Ok(Some("pi".into())) },
            None,
        )
        .await
        .expect_err("an absent record cannot prove a failed write");
        assert!(
            failed.to_string().contains("result is unknown"),
            "{failed:#}"
        );
        assert!(!failed.to_string().contains("can be retried"));
        assert_eq!(lookups.load(std::sync::atomic::Ordering::SeqCst), 2);
        assert_eq!(tracks.load(std::sync::atomic::Ordering::SeqCst), 1);
    }
}
