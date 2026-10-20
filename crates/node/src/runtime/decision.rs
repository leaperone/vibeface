//! The unified downlink: a decision from a web card or webhook reaches its thread through the
//! same send path as `vibeface codex send`. A local journal keeps one decision ID to one send.
use super::Runtime;
use anyhow::{Context as _, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::sync::Arc;
use vibeface_bridge::codex::{Delivery, DirectSession, Outgoing};

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct DecisionRequest {
    /// The device whose App Server owns the thread; the local Node when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node: Option<String>,
    pub thread_id: String,
    pub text: String,
    pub decision_id: String,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub effort: Option<String>,
    #[serde(default)]
    pub images: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct DecisionOutcome {
    pub decision_id: String,
    pub thread_id: String,
    #[serde(flatten)]
    pub delivery: Delivery,
}

#[derive(Deserialize, Serialize)]
struct Journal {
    thread_id: String,
    text: String,
    /// `None` while the send is in flight or was cut off by a restart.
    delivery: Option<Delivery>,
}

fn key(decision_id: &str) -> String {
    format!("decision:v1:{decision_id}")
}

pub async fn send_decision(rt: &Arc<Runtime>, mut req: DecisionRequest) -> Result<DecisionOutcome> {
    ensure!(
        !req.decision_id.is_empty() && req.decision_id.len() <= 128,
        "decision_id is required"
    );
    ensure!(!req.text.trim().is_empty(), "text is required");
    let network = rt.membership_state().await?.genesis.network_id;
    if let Some(node) = req.node.take()
        && node != rt.identity.device_id()?
    {
        let reply = rt
            .peer_request(
                &network,
                &node,
                json!({"type":"send_decision","human":true,"request":req}),
            )
            .await?;
        return serde_json::from_value(reply).context("the device returned no decision outcome");
    }
    let outcome = |delivery| DecisionOutcome {
        decision_id: req.decision_id.clone(),
        thread_id: req.thread_id.clone(),
        delivery,
    };
    {
        let mut store = rt.store.lock().await;
        if let Some(existing) = store.local_value(&network, &key(&req.decision_id))? {
            let existing: Journal = serde_json::from_value(existing)?;
            ensure!(
                existing.thread_id == req.thread_id && existing.text == req.text,
                "decision ID reused with a different message"
            );
            match existing.delivery {
                Some(delivery @ (Delivery::Delivered { .. } | Delivery::Rejected { .. })) => {
                    return Ok(outcome(delivery));
                }
                // The first send may have landed: read back, never send again.
                _ => {
                    drop(store);
                    let delivery = read_back(rt, &req).await;
                    if matches!(delivery, Delivery::Delivered { .. }) {
                        record(rt, &network, &req, &delivery).await?;
                    }
                    return Ok(outcome(delivery));
                }
            }
        }
        let journal = Journal {
            thread_id: req.thread_id.clone(),
            text: req.text.clone(),
            delivery: None,
        };
        store.local_set(&network, &key(&req.decision_id), &json!(journal))?;
    }
    let delivery = match DirectSession::connect(&rt.codex.socket).await {
        Ok(mut session) => {
            let delivery = session
                .send(&Outgoing {
                    thread_id: req.thread_id.clone(),
                    text: req.text.clone(),
                    client_id: req.decision_id.clone(),
                    model: req.model.clone(),
                    effort: req.effort.clone(),
                    images: req.images.clone(),
                    start_only: false,
                    full_access: false,
                })
                .await;
            session.close().await;
            delivery
        }
        Err(error) => Delivery::Rejected {
            reason: error.message,
        },
    };
    record(rt, &network, &req, &delivery).await?;
    tracing::info!(decision = %req.decision_id, thread = %req.thread_id,
        status = ?std::mem::discriminant(&delivery), "decision sent");
    Ok(outcome(delivery))
}

async fn read_back(rt: &Arc<Runtime>, req: &DecisionRequest) -> Delivery {
    let unknown = |reason: String| Delivery::Unknown { reason };
    let mut session = match DirectSession::connect(&rt.codex.socket).await {
        Ok(session) => session,
        Err(error) => return unknown(error.message),
    };
    let found = session.read_back(&req.thread_id, &req.decision_id).await;
    session.close().await;
    match found {
        Ok(Some(turn_id)) => Delivery::Delivered {
            mode: "existing".into(),
            turn_id,
        },
        Ok(None) => {
            unknown("the message is not in the thread; send it again with a new decision ID".into())
        }
        Err(error) => unknown(error.message),
    }
}

async fn record(
    rt: &Arc<Runtime>,
    network: &str,
    req: &DecisionRequest,
    delivery: &Delivery,
) -> Result<()> {
    let journal = Journal {
        thread_id: req.thread_id.clone(),
        text: req.text.clone(),
        // An unknown send stays open so the next attempt only reads back.
        delivery: (!matches!(delivery, Delivery::Unknown { .. })).then(|| delivery.clone()),
    };
    rt.store
        .lock()
        .await
        .local_set(network, &key(&req.decision_id), &json!(journal))
}

impl Runtime {
    /// The web's App Server passthrough: `codex`, `codex_state` and `send_decision`, on this
    /// device or forwarded to the device named by `node`.
    pub(super) async fn codex_rpc(
        self: &Arc<Self>,
        network: &str,
        method: &str,
        input: &serde_json::Value,
    ) -> Result<serde_json::Value> {
        use vibeface_bridge::methods::{self, Access};
        if method == "send_decision" {
            let req: DecisionRequest = serde_json::from_value(input.clone())?;
            return Ok(json!(send_decision(self, req).await?));
        }
        if let Some(node) = input["node"].as_str()
            && node != self.identity.device_id()?
        {
            let mut forward = input.clone();
            forward["type"] = json!(method);
            forward["human"] = json!(true);
            forward.as_object_mut().map(|object| object.remove("node"));
            return self.peer_request(network, node, forward).await;
        }
        let mut reply = if method == "codex" {
            let name = input["method"].as_str().context("method is required")?;
            ensure!(
                methods::access(name) != Access::Hidden,
                "{name} is not available from the web"
            );
            let params = input.get("params").cloned().unwrap_or_else(|| json!({}));
            match self.codex.client().request(name, params).await {
                Ok(result) => result,
                Err(error) if error.uncertain => anyhow::bail!(
                    "{}; the result is unknown, read the state back before calling again",
                    error.message
                ),
                Err(error) => anyhow::bail!(error.message),
            }
        } else {
            let mut state = self.codex.snapshot().await;
            if let Some(thread) = input["thread_id"].as_str() {
                let one = state["threads"][thread].take();
                state["threads"] = json!({thread: one});
            }
            state
        };
        methods::sanitize(&mut reply);
        Ok(reply)
    }
}
