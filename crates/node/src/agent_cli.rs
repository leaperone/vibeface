//! The coordination Agent's own path to the App Server. It talks to the socket directly; the
//! Node only registers dispatched work before the send and settles it afterwards.
use anyhow::{Context as _, Result, bail};
use serde_json::{Value, json};
use std::{future::Future, path::Path};
use vibeface_bridge::codex::{Delivery, DirectSession, Outgoing};
use vibeface_protocol::now_ms;

/// Registers the work with the Node (`track`), binds its thread before the send, sends the prompt
/// with `clientUserMessageId` = work ID and binds the result. Without `thread` the work starts a
/// turn in a new thread (or the one it continues); with `thread` it goes into that existing
/// thread, steering its running turn or starting one, and the thread's directory is read first.
/// Only the registration that created the work sends, and only inside its window. Each connect
/// first waits on `ready` for the Node's own App Server connection, so a send right after the
/// Node starts is not refused while the App Server comes up.
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
    B: Fn(Value) -> BF,
    BF: Future<Output = Result<Value>>,
    R: Fn() -> RF,
    RF: Future<Output = ()>,
{
    let mut args = args.clone();
    let existing = args["thread"].as_str().map(str::to_owned);
    // A send reads its thread before registering, so a missing thread leaves nothing behind.
    if let Some(thread) = &existing {
        ready().await;
        let mut session = DirectSession::connect(socket)
            .await
            .map_err(|error| anyhow::anyhow!(error.message))?;
        let read = session
            .request("thread/read", json!({"threadId":thread}))
            .await;
        session.close().await;
        let read = read
            .map_err(|error| anyhow::anyhow!("cannot read thread {thread}: {}", error.message))?;
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
    let fail = |reason: String| bind(json!({"work_id":work_id,"failed":reason}));
    ready().await;
    let mut session = match DirectSession::connect(socket).await {
        Ok(session) => session,
        Err(error) => return fail(error.message).await,
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
                Ok(None) => return fail("thread/start returned no thread".into()).await,
                Err(error) => return fail(error.message).await,
            }
        }
    };
    bind(json!({"work_id":work_id,"thread_id":thread})).await?;
    if now_ms() >= tracked["send_by_ms"].as_u64().unwrap_or(0) {
        return fail("the send window passed before the turn was sent".into()).await;
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
    let delivery = session.send(&outgoing).await;
    session.close().await;
    let report = match delivery {
        Delivery::Delivered { turn_id, .. } => json!({"work_id":work_id,"turn_id":turn_id}),
        Delivery::Unknown { reason } => json!({"work_id":work_id,"unknown":reason}),
        Delivery::Rejected { reason } => json!({"work_id":work_id,"failed":reason}),
    };
    bind(report).await
}

/// `vibeface codex call|watch`: a plain passthrough to the App Server socket.
pub async fn run(socket: &Path, command: Command) -> Result<()> {
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
