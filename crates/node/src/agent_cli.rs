//! The coordination Agent's own path to the App Server. It talks to the socket directly; the
//! Node only registers dispatched work before the send and settles it afterwards.
use anyhow::{Context as _, Result, bail};
use serde_json::{Value, json};
use std::{future::Future, path::Path};
use vibeface_bridge::{
    codex::{DirectSession, Outgoing, StartReport},
    failure::{ExecFailure, FailureKind},
};
use vibeface_protocol::now_ms;

/// Registers the work with the Node (`track`); a work the Node runs on Codex is sent here.
/// The CLI reports its thread before the send, sends the prompt with `clientUserMessageId` =
/// work ID and reports how the send ended (`bind`). Without `thread` the work starts a turn in
/// a new thread (or the one it continues); with `thread` it goes into that existing thread,
/// steering its running turn or starting one, and the thread's directory is read first. Only
/// the registration that created the work sends, and only inside its window. `ready` names the
/// provider the Node runs a work naming `agent` on, after waiting for the Node's own App Server
/// connection when that is Codex, so a send right after the Node starts is not refused while
/// the App Server comes up.
pub async fn dispatch<T, TF, B, BF, R, RF>(
    args: &Value,
    track: T,
    bind: B,
    ready: R,
    socket: &Path,
) -> Result<Value>
where
    T: FnOnce(Value) -> TF,
    TF: Future<Output = Result<Value>>,
    B: Fn(String, StartReport) -> BF,
    BF: Future<Output = Result<Value>>,
    R: Fn(Value) -> RF,
    RF: Future<Output = Option<String>>,
{
    let mut args = args.clone();
    let existing = args["thread"].as_str().map(str::to_owned);
    // A Codex send reads its thread before registering, so a missing thread leaves nothing
    // behind. The Node checks the session another provider would take the work in.
    let readied = match &existing {
        Some(_) => Some(ready(args["agent"].clone()).await),
        None => None,
    };
    if let (Some(thread), Some(Some("codex"))) = (&existing, readied.as_ref().map(Option::as_deref))
    {
        let mut session = DirectSession::connect(socket)
            .await
            .map_err(|error| anyhow::anyhow!(error.message))?;
        let read = session
            .request("thread/read", json!({"threadId":thread}))
            .await;
        session.close().await;
        let read = read
            .map_err(|error| anyhow::anyhow!("cannot read thread {thread}: {}", error.message))?;
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
        args["cwd"] = json!(read["thread"]["cwd"].as_str().unwrap_or_default());
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
    }
    let tracked = track(args.clone()).await?;
    if tracked["may_send"] != true {
        return Ok(tracked);
    }
    let work_id = tracked["work_id"]
        .as_str()
        .context("the Node returned no work ID")?
        .to_owned();
    let report = |report| bind(work_id.clone(), report);
    let fail = |failure: ExecFailure| report(StartReport::Rejected(failure));
    if readied.is_none() {
        ready(args["agent"].clone()).await;
    }
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
        model: args["model"].as_str().map(str::to_owned),
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
            track,
            |_, _| async { Ok(json!({})) },
            |_| async { Some("codex".to_owned()) },
            &socket,
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
}
