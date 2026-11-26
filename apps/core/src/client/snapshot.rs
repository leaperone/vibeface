//! Conditional conversation reads authenticate the full response before omitting any fields.

use crate::crypto::{LocalIdentity, digest, verify_payload};
use anyhow::{Context as _, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;

const RECEIPT_DOMAIN: &str = "conversation-snapshot-v1";
const MAX_FIELDS: usize = 64;
const MAX_RECEIPT_BYTES: usize = 16 * 1024;
const MAX_MESSAGE_ROWS: usize = 128;

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Receipt {
    fields: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    messages: Option<Vec<String>>,
    signature: String,
}

pub(super) struct Request {
    previous: Option<String>,
    receipt: Option<Receipt>,
}

fn valid_digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

impl Request {
    pub(super) fn parse(input: &Value) -> Result<Option<Self>> {
        let enabled = input
            .get("conditional")
            .map(|value| value.as_bool().context("conditional must be a boolean"))
            .transpose()?
            .unwrap_or(false);
        let previous = input
            .get("if_snapshot")
            .map(|value| {
                let value = value
                    .as_str()
                    .context("if_snapshot must be a SHA-256 digest")?;
                ensure!(valid_digest(value), "if_snapshot must be a SHA-256 digest");
                Ok::<_, anyhow::Error>(value.to_owned())
            })
            .transpose()?;
        if !enabled && previous.is_none() {
            return Ok(None);
        }
        let receipt = input.get("snapshot_receipt").and_then(|value| {
            let bytes = serde_json::to_vec(value).ok()?;
            if bytes.len() > MAX_RECEIPT_BYTES {
                return None;
            }
            let receipt: Receipt = serde_json::from_slice(&bytes).ok()?;
            (receipt.fields.len() <= MAX_FIELDS
                && receipt.signature.len() == 128
                && receipt
                    .fields
                    .iter()
                    .all(|(key, value)| key.len() <= 128 && valid_digest(value))
                && receipt.messages.as_ref().is_none_or(|messages| {
                    messages.len() <= MAX_MESSAGE_ROWS
                        && messages.iter().all(|hash| valid_digest(hash))
                }))
            .then_some(receipt)
        });
        Ok(Some(Self { previous, receipt }))
    }

    pub(super) fn respond(
        self,
        identity: &LocalIdentity,
        network: &str,
        scope: &str,
        mut response: Value,
    ) -> Result<Value> {
        let snapshot_id = digest(&serde_json::to_vec(&response)?);
        if self.previous.as_deref() == Some(&snapshot_id) {
            return Ok(json!({"unchanged":true,"snapshot_id":snapshot_id}));
        }
        let object = response
            .as_object()
            .context("conversation snapshot must be an object")?;
        let hashes = object
            .iter()
            .map(|(key, value)| Ok((key.clone(), digest(&serde_json::to_vec(value)?))))
            .collect::<Result<BTreeMap<_, _>>>()?;
        let messages = match response["messages"].as_array() {
            Some(messages)
                if messages.len() <= MAX_MESSAGE_ROWS
                    && serde_json::to_vec(messages)?.len() > messages.len() * 68 + 256 =>
            {
                Some(
                    messages
                        .iter()
                        .map(|message| Ok(digest(&serde_json::to_vec(message)?)))
                        .collect::<Result<Vec<_>>>()?,
                )
            }
            _ => None,
        };
        let mut receipt = Receipt {
            fields: hashes,
            messages,
            signature: String::new(),
        };
        if serde_json::to_vec(&receipt)?.len() + 128 > MAX_RECEIPT_BYTES {
            receipt.messages = None;
        }
        receipt.signature = identity.sign_payload(
            RECEIPT_DOMAIN,
            &serde_json::to_vec(&(
                network,
                scope,
                &snapshot_id,
                &receipt.fields,
                &receipt.messages,
            ))?,
        )?;
        let delta = match (self.previous, self.receipt) {
            (Some(previous), Some(prior))
                if verify_payload(
                    &identity.device_id()?,
                    RECEIPT_DOMAIN,
                    &serde_json::to_vec(&(
                        network,
                        scope,
                        &previous,
                        &prior.fields,
                        &prior.messages,
                    ))?,
                    &prior.signature,
                )
                .is_ok() =>
            {
                let changed: Map<String, Value> = object
                    .iter()
                    .filter(|(key, _)| prior.fields.get(*key) != receipt.fields.get(*key))
                    .map(|(key, value)| (key.clone(), value.clone()))
                    .collect();
                let removed: Vec<_> = prior
                    .fields
                    .keys()
                    .filter(|key| !receipt.fields.contains_key(*key))
                    .collect();
                let mut delta = json!({"snapshot_delta":true,"base_snapshot_id":previous,
                    "snapshot_id":snapshot_id,"snapshot_receipt":receipt,
                    "fields":changed,"removed":removed});
                if delta["fields"].get("messages").is_some()
                    && let (Some(prior_hashes), Some(hashes), Some(messages)) = (
                        prior.messages.as_ref(),
                        receipt.messages.as_ref(),
                        response["messages"].as_array(),
                    )
                {
                    let rows: Vec<_> = hashes
                        .iter()
                        .zip(messages)
                        .map(|(hash, message)| {
                            match prior_hashes.iter().position(|previous| previous == hash) {
                                Some(index) => json!({"previous":index}),
                                None => json!({"value":message}),
                            }
                        })
                        .collect();
                    let mut row_delta = delta.clone();
                    row_delta["fields"]
                        .as_object_mut()
                        .context("changed snapshot fields")?
                        .remove("messages");
                    row_delta["messages_delta"] = json!({"rows":rows});
                    if serde_json::to_vec(&row_delta)?.len() < serde_json::to_vec(&delta)?.len() {
                        delta = row_delta;
                    }
                }
                Some(delta)
            }
            _ => None,
        };
        response["snapshot_id"] = json!(snapshot_id);
        response["snapshot_receipt"] = serde_json::to_value(receipt)?;
        if let Some(delta) = delta
            && serde_json::to_vec(&delta)?.len() < serde_json::to_vec(&response)?.len()
        {
            return Ok(delta);
        }
        Ok(response)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn respond(identity: &LocalIdentity, input: Value, response: Value) -> Result<Value> {
        Request::parse(&input)?
            .context("conditional request")?
            .respond(identity, "network", "conversation", response)
    }

    fn follow_up(response: &Value) -> Value {
        json!({"conditional":true,"if_snapshot":response["snapshot_id"],
            "snapshot_receipt":response["snapshot_receipt"]})
    }

    fn restore(mut previous: Value, delta: &Value) -> Result<Value> {
        if let Some(rows) = delta["messages_delta"]["rows"].as_array() {
            let old = previous["messages"]
                .as_array()
                .context("previous messages")?;
            let messages = rows
                .iter()
                .map(|row| match row["previous"].as_u64() {
                    Some(index) => old[index as usize].clone(),
                    None => row["value"].clone(),
                })
                .collect::<Vec<_>>();
            previous["messages"] = json!(messages);
        }
        let object = previous.as_object_mut().context("raw snapshot")?;
        for key in delta["removed"].as_array().context("removed fields")? {
            object.remove(key.as_str().context("removed key")?);
        }
        object.extend(
            delta["fields"]
                .as_object()
                .context("changed fields")?
                .clone(),
        );
        Ok(previous)
    }

    #[test]
    fn unchanged_snapshot_is_compact_and_hashes_the_actual_payload() -> Result<()> {
        let identity = LocalIdentity::generate();
        let raw = json!({"messages":[{"text":"hello"}],"scheduler":{"running":false}});
        assert!(Request::parse(&json!({}))?.is_none());
        let first = respond(&identity, json!({"conditional":true}), raw.clone())?;
        assert_eq!(first["messages"], raw["messages"]);
        assert_eq!(first["snapshot_id"], digest(&serde_json::to_vec(&raw)?));
        let unchanged = respond(&identity, follow_up(&first), raw)?;
        assert_eq!(
            unchanged,
            json!({"unchanged":true,"snapshot_id":first["snapshot_id"]})
        );
        assert_eq!(serde_json::to_vec(&unchanged)?.len(), 99);
        Ok(())
    }

    #[test]
    fn signed_field_receipts_reconstruct_changed_snapshots_after_reconnect() -> Result<()> {
        let identity = LocalIdentity::generate();
        let first_raw = json!({"messages":[{"text":"history".repeat(4096)}],
            "scheduler":{"running":false},"removed":"old value","nullable":"before"});
        let first = respond(&identity, json!({"conditional":true}), first_raw.clone())?;
        let next_raw = json!({"messages":first_raw["messages"],"scheduler":{"running":true},
            "nullable":null,"added":[]});
        // A new Request has no connection state. Only the server-signed receipt is retained.
        let delta = respond(&identity, follow_up(&first), next_raw.clone())?;
        assert_eq!(delta["snapshot_delta"], true);
        assert_eq!(delta["base_snapshot_id"], first["snapshot_id"]);
        assert!(delta["fields"].get("messages").is_none());
        assert_eq!(
            delta["fields"],
            json!({"scheduler":{"running":true},"nullable":null,"added":[]})
        );
        assert_eq!(delta["removed"], json!(["removed"]));
        let mut restored = first_raw;
        let object = restored.as_object_mut().context("raw snapshot")?;
        for key in delta["removed"].as_array().context("removed fields")? {
            object.remove(key.as_str().context("removed key")?);
        }
        object.extend(
            delta["fields"]
                .as_object()
                .context("changed fields")?
                .clone(),
        );
        assert_eq!(restored, next_raw);
        assert_eq!(
            delta["snapshot_id"],
            digest(&serde_json::to_vec(&restored)?)
        );
        assert_eq!(
            respond(&identity, follow_up(&delta), restored)?,
            json!({"unchanged":true,"snapshot_id":delta["snapshot_id"]})
        );
        Ok(())
    }

    #[test]
    fn untrusted_or_misbound_receipts_fall_back_to_full_snapshots() -> Result<()> {
        let identity = LocalIdentity::generate();
        let raw = json!({"messages":["history".repeat(4096)],"scheduler":{"running":false}});
        let first = respond(&identity, json!({"conditional":true}), raw.clone())?;
        let next = json!({"messages":raw["messages"],"scheduler":{"running":true}});
        let expected = respond(&identity, json!({"conditional":true}), next.clone())?;
        for input in [
            json!({"if_snapshot":first["snapshot_id"]}),
            json!({"if_snapshot":first["snapshot_id"],"snapshot_receipt":{}}),
            json!({"if_snapshot":"0".repeat(64),"snapshot_receipt":first["snapshot_receipt"]}),
            {
                let mut input = follow_up(&first);
                input["snapshot_receipt"]["fields"]["messages"] = json!("0".repeat(64));
                input
            },
            {
                let mut input = follow_up(&first);
                input["snapshot_receipt"]["messages"] = json!(["0".repeat(64)]);
                input
            },
            {
                let mut input = follow_up(&first);
                input["snapshot_receipt"]["messages"] = json!(["bad-digest"]);
                input
            },
            {
                let mut input = follow_up(&first);
                input["snapshot_receipt"]["messages"] =
                    json!(vec!["0".repeat(64); MAX_MESSAGE_ROWS + 1]);
                input
            },
        ] {
            assert_eq!(respond(&identity, input, next.clone())?, expected);
        }
        for (network, scope, signer) in [
            ("another-network", "conversation", identity.clone()),
            ("network", "another-conversation", identity.clone()),
            ("network", "conversation", LocalIdentity::generate()),
        ] {
            let result = Request::parse(&follow_up(&first))?
                .context("request")?
                .respond(&signer, network, scope, next.clone())?;
            assert!(result.get("snapshot_delta").is_none());
            assert_eq!(result["messages"], next["messages"]);
            assert_eq!(result["scheduler"], next["scheduler"]);
        }
        Ok(())
    }

    #[test]
    fn message_rows_reuse_transcript_on_append_window_roll_and_reorder() -> Result<()> {
        let identity = LocalIdentity::generate();
        let message = |id: &str| json!({"message_id":id,"text":format!("{id}{}", "long transcript ".repeat(1024))});
        let (a, b, c, d) = (message("a"), message("b"), message("c"), message("d"));
        let old = json!({"messages":[a,b,c],"scheduler":{"running":false}});
        let first = respond(&identity, json!({"conditional":true}), old.clone())?;
        assert_eq!(
            first["snapshot_receipt"]["messages"]
                .as_array()
                .context("row proofs")?
                .len(),
            3
        );
        let edited = json!({"message_id":"b","text":"edited despite same ID"});
        for (rows, expected) in [
            (
                json!([a, b, c, d]),
                json!([{"previous":0},{"previous":1},{"previous":2},{"value":d}]),
            ),
            (
                json!([b, c, d]),
                json!([{"previous":1},{"previous":2},{"value":d}]),
            ),
            (json!([c, a]), json!([{"previous":2},{"previous":0}])),
            (
                json!([c, edited, a]),
                json!([{"previous":2},{"value":edited},{"previous":0}]),
            ),
        ] {
            let next = json!({"messages":rows,"scheduler":old["scheduler"]});
            let delta = respond(&identity, follow_up(&first), next.clone())?;
            assert_eq!(delta["snapshot_delta"], true);
            assert!(delta["fields"].get("messages").is_none());
            assert_eq!(delta["messages_delta"]["rows"], expected);
            let restored = restore(old.clone(), &delta)?;
            assert_eq!(restored, next);
            assert_eq!(
                delta["snapshot_id"],
                digest(&serde_json::to_vec(&restored)?)
            );
            let full = respond(&identity, json!({"conditional":true}), next)?;
            let (delta_bytes, full_bytes) = (
                serde_json::to_vec(&delta)?.len(),
                serde_json::to_vec(&full)?.len(),
            );
            assert!(
                delta_bytes < full_bytes / 2,
                "the old transcript must remain omitted"
            );
            eprintln!("message row delta: {delta_bytes} bytes; full snapshot: {full_bytes} bytes");
        }
        Ok(())
    }

    #[test]
    fn small_or_unbounded_message_windows_keep_simple_replies() -> Result<()> {
        let identity = LocalIdentity::generate();
        let small = respond(
            &identity,
            json!({"conditional":true}),
            json!({"messages":[{"text":"hi"}]}),
        )?;
        assert!(small["snapshot_receipt"].get("messages").is_none());
        let large = json!({"messages":vec![json!({"text":"large text".repeat(512)}); MAX_MESSAGE_ROWS + 1]});
        let full = respond(&identity, json!({"conditional":true}), large)?;
        assert!(full["snapshot_receipt"].get("messages").is_none());
        assert!(serde_json::to_vec(&full["snapshot_receipt"])?.len() <= MAX_RECEIPT_BYTES);
        Ok(())
    }

    #[tokio::test]
    async fn conditional_dispatch_preserves_authorization_and_observation() -> Result<()> {
        use super::super::{Session, command, dispatch};
        use crate::runtime::test_support::{bot, node, signed_main_input};

        let (node, admin, directory) = node().await?;
        let result = async {
            bot(&node, &admin, "conversation", "Snapshot", &[]).await?;
            let network = node.network_id().await?;
            let previous = signed_main_input(&admin, &network, "conversation", "old-message", &"previous transcript ".repeat(2048), vec![], 1)?;
            let identity = node.identity.clone();
            node.store.call("conditional_previous_message_fixture", move |store| {
                store.append_event(&identity, &network, "conversation", previous, vec![], crate::clock::now_ms())?;
                Ok(())
            }).await?;
            let mut session = Session { device_id:admin.device_id()?,transport:None,cookie:None,
                subscribed:None,observation:None,operator:false };
            async fn rpc(node: &std::sync::Arc<crate::runtime::Runtime>, session: &mut Session, params: Value) -> Result<Value> {
                dispatch(node, session, command::decode_client("rpc", command::Params::new(params)?)).await
            }
            let plain = rpc(&node, &mut session, json!({"method":"conversation","conversation_id":"conversation"})).await?;
            assert!(plain.get("snapshot_id").is_none());
            let first = rpc(&node, &mut session, json!({"method":"conversation","conversation_id":"conversation","conditional":true})).await?;
            assert!(session.observation.is_some());
            let mut request = follow_up(&first);
            request["method"] = json!("conversation");
            request["conversation_id"] = json!("conversation");
            // Dropping the first Session simulates a disconnected transport with no baseline cache.
            drop(session);
            let mut session = Session { device_id:admin.device_id()?,transport:None,cookie:None,
                subscribed:None,observation:None,operator:false };
            assert_eq!(rpc(&node, &mut session, request.clone()).await?,
                json!({"unchanged":true,"snapshot_id":first["snapshot_id"]}));
            assert!(session.observation.is_some());
            let network = node.network_id().await?;
            let body = signed_main_input(&admin, &network, "conversation", "new-message", "changed content", vec![], 2)?;
            let identity = node.identity.clone();
            node.store.call("conditional_snapshot_fixture", move |store| {
                store.append_event(&identity, &network, "conversation", body, vec![], crate::clock::now_ms())?;
                Ok(())
            }).await?;
            let changed = rpc(&node, &mut session, request.clone()).await?;
            assert_eq!(changed["messages_delta"]["rows"][0], json!({"previous":0}));
            assert!(changed["fields"].get("messages").is_none());
            let restored = restore(plain, &changed)?;
            assert_eq!(restored["messages"][1]["text"], "changed content");
            assert_ne!(changed["snapshot_id"], first["snapshot_id"]);
            assert!(session.observation.is_some());
            session.device_id = LocalIdentity::generate().device_id()?;
            assert!(rpc(&node, &mut session, request).await.is_err(), "a valid validator grants no authorization");
            Ok::<_, anyhow::Error>(())
        }.await;
        node.shutdown().await;
        drop(node);
        std::fs::remove_dir_all(directory)?;
        result
    }
}
