//! The coordination Agent's own path to the App Server. It talks to the socket directly; the
//! Node only registers dispatched work before the send and settles it afterwards.
use anyhow::{Context as _, Result, bail};
use serde_json::{Value, json};
use std::{future::Future, path::Path};
use vibeface_bridge::codex::{Delivery, DirectSession, Outgoing};
use vibeface_protocol::now_ms;

/// Registers the work with the Node (`track`), starts its thread, binds the thread before
/// `turn/start`, sends the prompt with `clientUserMessageId` = work ID and binds the result.
/// Only the registration that created the work sends, and only inside its window.
pub async fn dispatch<T, TF, B, BF>(args: &Value, track: T, bind: B, socket: &Path) -> Result<Value>
where
    T: FnOnce(Value) -> TF,
    TF: Future<Output = Result<Value>>,
    B: Fn(Value) -> BF,
    BF: Future<Output = Result<Value>>,
{
    let tracked = track(args.clone()).await?;
    if tracked["may_send"] != true {
        return Ok(tracked);
    }
    let work_id = tracked["work_id"]
        .as_str()
        .context("the Node returned no work ID")?
        .to_owned();
    let fail = |reason: String| bind(json!({"work_id":work_id,"failed":reason}));
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
        start_only: true,
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

/// `vibeface codex call|send|watch`: a plain passthrough to the App Server socket.
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
            match session.request(&method, params).await {
                Ok(result) => println!("{result}"),
                Err(error) if error.uncertain => bail!(
                    "{}; the result is unknown, read the state back before calling again",
                    error.message
                ),
                Err(error) => bail!(error.message),
            }
        }
        Command::Send(outgoing) => {
            let delivery = session.send(&outgoing).await;
            println!("{}", json!({"client_id":outgoing.client_id,"delivery":delivery}));
        }
        Command::Watch { thread_id } => {
            session
                .request("thread/resume", json!({"threadId":thread_id}))
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
    Send(Outgoing),
    Watch {
        thread_id: String,
    },
}
