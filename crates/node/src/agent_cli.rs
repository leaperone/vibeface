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
        Command::Send(log, outgoing) => {
            let delivery = match claim(&log, &outgoing)? {
                Claim::Done(delivery) => delivery,
                Claim::ReadBack => {
                    let delivery = match session
                        .read_back(&outgoing.thread_id, &outgoing.client_id)
                        .await
                    {
                        Ok(Some(turn_id)) => Delivery::Delivered {
                            mode: "existing".into(),
                            turn_id,
                        },
                        Ok(None) => Delivery::Unknown {
                            reason: "the message is not in the thread; send it with a new --id"
                                .into(),
                        },
                        Err(error) => Delivery::Unknown {
                            reason: error.message,
                        },
                    };
                    record(&log, &outgoing, &delivery)?;
                    delivery
                }
                Claim::Send => {
                    let delivery = session.send(&outgoing).await;
                    record(&log, &outgoing, &delivery)?;
                    delivery
                }
            };
            let level = match delivery {
                Delivery::Delivered { .. } => "info",
                _ => "warn",
            };
            crate::logging::send_to_node(json!({"level":level,"event":"codex.send","message":format!("message to thread: {}", serde_json::to_string(&delivery)?),
                "thread_id":outgoing.thread_id,"fields":{"client_id":outgoing.client_id}}))
            .await;
            println!(
                "{}",
                json!({"client_id":outgoing.client_id,"delivery":delivery})
            );
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

#[derive(Debug, PartialEq)]
enum Claim {
    Send,
    ReadBack,
    Done(Delivery),
}

fn entry_path(log: &Path, outgoing: &Outgoing) -> std::path::PathBuf {
    log.join(crate::crypto::digest(outgoing.client_id.as_bytes()))
}

/// One send per client ID: the entry is created exclusively before the send, so a rerun, even
/// a concurrent one, only reads back. The same ID for another message is refused.
fn claim(log: &Path, outgoing: &Outgoing) -> Result<Claim> {
    use std::io::Write;
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(log)?;
    let path = entry_path(log, outgoing);
    let entry = json!({"thread_id":outgoing.thread_id,"text":outgoing.text});
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)
    {
        Ok(mut file) => {
            file.write_all(entry.to_string().as_bytes())?;
            file.sync_all()?;
            Ok(Claim::Send)
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            let existing: Value = serde_json::from_slice(&std::fs::read(&path)?)
                .context("unreadable send log entry")?;
            anyhow::ensure!(
                existing["thread_id"] == entry["thread_id"] && existing["text"] == entry["text"],
                "message ID reused with a different message"
            );
            Ok(
                match serde_json::from_value::<Delivery>(existing["delivery"].clone()) {
                    Ok(delivery @ (Delivery::Delivered { .. } | Delivery::Rejected { .. })) => {
                        Claim::Done(delivery)
                    }
                    _ => Claim::ReadBack,
                },
            )
        }
        Err(error) => Err(error.into()),
    }
}

fn record(log: &Path, outgoing: &Outgoing, delivery: &Delivery) -> Result<()> {
    let entry = json!({"thread_id":outgoing.thread_id,"text":outgoing.text,"delivery":delivery});
    crate::config::private_write(&entry_path(log, outgoing), entry.to_string().as_bytes())
}

pub enum Command {
    Call {
        method: String,
        params: Option<String>,
    },
    /// The send log directory, then the message.
    Send(std::path::PathBuf, Outgoing),
    Watch {
        thread_id: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_client_id_is_sent_once_and_then_only_read_back() -> Result<()> {
        let log = std::env::temp_dir().join(format!("vibeface-sends-{}", uuid::Uuid::new_v4()));
        let outgoing = Outgoing {
            thread_id: "t".into(),
            text: "go".into(),
            client_id: "m1".into(),
            ..Default::default()
        };
        assert_eq!(claim(&log, &outgoing)?, Claim::Send);
        // Cut off before the result: a rerun reads back instead of sending again.
        assert_eq!(claim(&log, &outgoing)?, Claim::ReadBack);
        let unknown = Delivery::Unknown {
            reason: "lost".into(),
        };
        record(&log, &outgoing, &unknown)?;
        assert_eq!(claim(&log, &outgoing)?, Claim::ReadBack);
        let delivered = Delivery::Delivered {
            mode: "start".into(),
            turn_id: "r".into(),
        };
        record(&log, &outgoing, &delivered)?;
        assert_eq!(claim(&log, &outgoing)?, Claim::Done(delivered));
        let other = Outgoing {
            text: "different".into(),
            ..outgoing.clone()
        };
        assert!(claim(&log, &other).is_err());
        let moved = Outgoing {
            thread_id: "u".into(),
            ..outgoing
        };
        assert!(claim(&log, &moved).is_err());
        std::fs::remove_dir_all(&log)?;
        Ok(())
    }
}
