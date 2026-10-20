//! The unified downlink: a decision from a web card or webhook reaches its thread through the
//! same send path as `vibeface codex send`, or answers the App Server request its card holds.
//! A local journal keeps one decision ID to one send.
use super::Runtime;
use crate::model::{ATTENTION_SCOPE, AlertResolution, EventBody};
use anyhow::{Context as _, Result, ensure};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::sync::Arc;
use vibeface_bridge::codex::{Delivery, DirectSession, Outgoing};

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct DecisionRequest {
    /// The device whose App Server owns the thread; the local Node when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node: Option<String>,
    pub thread_id: String,
    #[serde(default)]
    pub text: String,
    pub decision_id: String,
    /// The card this decision answers; it is resolved once the decision lands.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub alert_id: Option<String>,
    /// Index into the card's choices, instead of free text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub choice: Option<usize>,
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

struct Journal {
    thread_id: String,
    text: String,
    /// `None` while the send is in flight or was cut off by a restart.
    delivery: Option<Delivery>,
}

fn load_journal(connection: &Connection, decision_id: &str) -> Result<Option<Journal>> {
    let row = connection
        .query_row(
            "SELECT thread_id,text,delivery FROM decision_journal WHERE decision_id=?",
            [decision_id],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, Option<String>>(2)?,
                ))
            },
        )
        .optional()?;
    row.map(|(thread_id, text, delivery)| {
        Ok(Journal {
            thread_id,
            text,
            delivery: delivery.as_deref().map(serde_json::from_str).transpose()?,
        })
    })
    .transpose()
}

fn save_journal(connection: &Connection, decision_id: &str, journal: &Journal) -> Result<()> {
    connection.execute(
        "INSERT INTO decision_journal(decision_id,thread_id,text,delivery) VALUES(?,?,?,?)
         ON CONFLICT(decision_id) DO UPDATE SET thread_id=excluded.thread_id,text=excluded.text,delivery=excluded.delivery",
        params![
            decision_id,
            journal.thread_id,
            journal.text,
            journal.delivery.as_ref().map(serde_json::to_string).transpose()?
        ],
    )?;
    Ok(())
}

pub async fn send_decision(rt: &Arc<Runtime>, mut req: DecisionRequest) -> Result<DecisionOutcome> {
    ensure!(
        !req.decision_id.is_empty() && req.decision_id.len() <= 128,
        "decision_id is required"
    );
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
    let card = match &req.alert_id {
        Some(alert_id) => rt.decision_card(alert_id).await?,
        None => None,
    };
    // A choice on a card that holds an App Server request answers that request.
    let mut answer = None;
    if let Some(index) = req.choice {
        let card = card.as_ref().context("this card offers no choices")?;
        let choice = card.choices.get(index).context("no such choice")?;
        match &card.request_id {
            Some(request_id) => {
                req.text = format!("choice:{index}:{}", choice.label);
                answer = Some((request_id.clone(), choice.reply.clone()));
            }
            None => req.text = choice.label.clone(),
        }
    }
    ensure!(!req.text.trim().is_empty(), "text is required");
    let outcome = |delivery| DecisionOutcome {
        decision_id: req.decision_id.clone(),
        thread_id: req.thread_id.clone(),
        delivery,
    };
    {
        let store = rt.store.lock().await;
        if let Some(existing) = load_journal(&store.connection, &req.decision_id)? {
            ensure!(
                existing.thread_id == req.thread_id && existing.text == req.text,
                "decision ID reused with a different message"
            );
            match existing.delivery {
                Some(delivery @ (Delivery::Delivered { .. } | Delivery::Rejected { .. })) => {
                    return Ok(outcome(delivery));
                }
                // The first send may have landed: read back, never send again.
                _ if answer.is_none() => {
                    drop(store);
                    let delivery = read_back(rt, &req).await;
                    if matches!(delivery, Delivery::Delivered { .. }) {
                        record(rt, &req, &delivery).await?;
                        resolve(rt, &network, &req).await;
                    }
                    return Ok(outcome(delivery));
                }
                // An answer is idempotent per request: asking again returns the first answer.
                _ => {}
            }
        }
        let journal = Journal {
            thread_id: req.thread_id.clone(),
            text: req.text.clone(),
            delivery: None,
        };
        save_journal(&store.connection, &req.decision_id, &journal)?;
    }
    let delivery = match answer {
        Some((request_id, reply)) => {
            let answered = rt
                .answer_codex_request(&request_id, Some(Ok(reply)), "user")
                .await?;
            if answered["answered"] == true {
                Delivery::Delivered {
                    mode: "answer".into(),
                    turn_id: answered["turn_id"].as_str().unwrap_or_default().into(),
                }
            } else {
                Delivery::Rejected {
                    reason: answered["error"]
                        .as_str()
                        .unwrap_or("the request was answered already")
                        .into(),
                }
            }
        }
        None => match DirectSession::connect(&rt.codex.socket).await {
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
        },
    };
    record(rt, &req, &delivery).await?;
    if matches!(delivery, Delivery::Delivered { .. }) {
        resolve(rt, &network, &req).await;
    }
    tracing::info!(decision = %req.decision_id, thread = %req.thread_id,
        status = ?std::mem::discriminant(&delivery), "decision sent");
    Ok(outcome(delivery))
}

/// The card is done once its decision landed; a resolution that fails to append is only logged,
/// the user can still dismiss the card.
async fn resolve(rt: &Arc<Runtime>, network: &str, req: &DecisionRequest) {
    let Some(alert_id) = &req.alert_id else {
        return;
    };
    let resolved = rt
        .append(
            network,
            ATTENTION_SCOPE,
            EventBody::AttentionResolved {
                alert_id: alert_id.clone(),
                resolution: AlertResolution::Decided,
                attestation: None,
            },
        )
        .await;
    if let Err(error) = resolved {
        tracing::warn!(%error, alert = %alert_id, "decided card was not resolved");
    }
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

async fn record(rt: &Arc<Runtime>, req: &DecisionRequest, delivery: &Delivery) -> Result<()> {
    let journal = Journal {
        thread_id: req.thread_id.clone(),
        text: req.text.clone(),
        // An unknown send stays open so the next attempt only reads back.
        delivery: (!matches!(delivery, Delivery::Unknown { .. })).then(|| delivery.clone()),
    };
    save_journal(
        &rt.store.lock().await.connection,
        &req.decision_id,
        &journal,
    )
}

impl Runtime {
    /// The web's view of Codex: `codex_state` is a status summary and `send_decision` the
    /// downlink, on this device or forwarded to the device named by `node`. The web never calls
    /// App Server methods.
    pub(super) async fn codex_rpc(
        self: &Arc<Self>,
        network: &str,
        method: &str,
        input: &Value,
    ) -> Result<Value> {
        if method == "send_decision" {
            // A device forwards the request as `request`; the web sends it as the input itself.
            let req: DecisionRequest =
                serde_json::from_value(input.get("request").unwrap_or(input).clone())?;
            return Ok(json!(send_decision(self, req).await?));
        }
        if let Some(node) = input["node"].as_str()
            && node != self.identity.device_id()?
        {
            return self
                .peer_request(network, node, json!({"type":method,"human":true}))
                .await;
        }
        Ok(self.codex.status().await)
    }
}
