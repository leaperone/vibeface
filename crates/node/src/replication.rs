use anyhow::{Context, Result, ensure};
use serde::Deserialize;
use serde_json::{Value, json};
use vibeface_secure::authorization::{
    Action, ObjectHash, ResourceId, SpaceId, VerifiedAuthorization,
};

use crate::store::Store;

const MAX_PAGE: usize = 256;
const MAX_CHUNK: usize = 65_536;

#[derive(Debug, Deserialize)]
struct ReadRequest {
    space_id: SpaceId,
    resource_id: ResourceId,
    authorization_id: String,
    #[serde(flatten)]
    query: ReadQuery,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "method", deny_unknown_fields)]
enum ReadQuery {
    #[serde(rename = "replica_inventory")]
    Inventory {
        after: Option<String>,
        #[serde(default = "page_size")]
        limit: usize,
    },
    #[serde(rename = "replica_event")]
    Event { hash: String },
    #[serde(rename = "replica_blob")]
    Blob {
        hash: String,
        offset: u64,
        #[serde(default = "chunk_size")]
        limit: usize,
    },
}

fn page_size() -> usize {
    128
}
fn chunk_size() -> usize {
    MAX_CHUNK
}
fn valid_hash(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}
fn parse_request(body: &Value) -> Result<ReadRequest> {
    let request: ReadRequest = serde_json::from_value(body.clone())?;
    match &request.query {
        ReadQuery::Inventory { after, limit } => {
            ensure!(
                (1..=MAX_PAGE).contains(limit),
                "invalid inventory page size"
            );
            ensure!(
                after.as_deref().is_none_or(valid_hash),
                "invalid inventory cursor"
            );
        }
        ReadQuery::Event { hash } => ensure!(valid_hash(hash), "invalid event hash"),
        ReadQuery::Blob { hash, limit, .. } => {
            ensure!(valid_hash(hash), "invalid blob hash");
            ensure!((1..=MAX_CHUNK).contains(limit), "invalid blob chunk size");
        }
    }
    Ok(request)
}

// The caller holds the same Store lock used for the current authorization check.
pub fn query(store: &Store, auth: &VerifiedAuthorization, body: &Value) -> Result<Value> {
    ensure!(
        auth.action() == Action::ReplicaRead,
        "replica read grant required"
    );
    ensure!(
        auth.body_hash() == &ObjectHash::body(body)?,
        "replica request body mismatch"
    );
    let request = parse_request(body)?;
    let resource = auth.resource();
    ensure!(
        request.space_id == resource.space
            && request.resource_id == resource.resource
            && request.authorization_id == auth.operation_id(),
        "replica authorization scope mismatch"
    );
    let space = resource.space.as_str();
    match request.query {
        ReadQuery::Inventory { after, limit } => {
            let mut events = store.resource_event_hashes(
                space,
                &resource.resource,
                after.as_deref(),
                limit + 1,
            )?;
            let next_cursor = if events.len() > limit {
                Some(events[limit - 1].clone())
            } else {
                None
            };
            events.truncate(limit);
            Ok(json!({"events":events,"next_cursor":next_cursor}))
        }
        ReadQuery::Event { hash } => {
            let envelope = store.event(space, &hash)?.context("event unavailable")?;
            let event = store.verify_event(space, &envelope)?;
            ensure!(
                event.resource_id == resource.resource && envelope.hash == hash,
                "event unavailable"
            );
            Ok(json!({"event":envelope}))
        }
        ReadQuery::Blob {
            hash,
            offset,
            limit,
        } => {
            ensure!(
                store.resource_blob_allowed(space, &resource.resource, &hash)?,
                "blob unavailable"
            );
            let bytes = store.get_blob(space, &hash)?.context("blob unavailable")?;
            let start = usize::try_from(offset).context("invalid blob offset")?;
            ensure!(start <= bytes.len(), "invalid blob offset");
            Ok(json!({
                "hash":hash,"offset":offset,"size":bytes.len(),
                "data":hex::encode(&bytes[start..start.saturating_add(limit).min(bytes.len())])
            }))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn body(query: Value) -> Value {
        let mut body = query.as_object().unwrap().clone();
        body.insert(
            "space_id".into(),
            json!(format!("spc2_{}", "11".repeat(32))),
        );
        body.insert(
            "resource_id".into(),
            json!(format!("res2_{}", "22".repeat(32))),
        );
        body.insert("authorization_id".into(), json!("replica:page-1"));
        Value::Object(body)
    }

    #[test]
    fn inventory_pages_have_strict_bounds_and_no_legacy_scope_fields() {
        let request = parse_request(&body(json!({"method":"replica_inventory"}))).unwrap();
        assert!(matches!(
            request.query,
            ReadQuery::Inventory {
                after: None,
                limit: 128
            }
        ));
        assert!(
            parse_request(&body(
                json!({"method":"replica_inventory","limit":256,"after":"ab".repeat(32)})
            ))
            .is_ok()
        );
        for limit in [
            json!(0),
            json!(257),
            json!(-1),
            json!(1.5),
            json!("128"),
            Value::Null,
        ] {
            assert!(
                parse_request(&body(json!({"method":"replica_inventory","limit":limit}))).is_err()
            );
        }
        assert!(
            parse_request(&body(
                json!({"method":"replica_inventory","after":"AB".repeat(32)})
            ))
            .is_err()
        );
        assert!(
            parse_request(&body(
                json!({"method":"replica_inventory","network_id":"legacy"})
            ))
            .is_err()
        );
        assert!(parse_request(&body(json!({"method":"inventory"}))).is_err());
    }

    #[test]
    fn event_and_blob_requests_reject_ambiguous_or_unbounded_inputs() {
        let hash = "ab".repeat(32);
        assert!(parse_request(&body(json!({"method":"replica_event","hash":hash}))).is_ok());
        let blob = parse_request(&body(
            json!({"method":"replica_blob","hash":hash,"offset":0}),
        ))
        .unwrap();
        assert!(matches!(
            blob.query,
            ReadQuery::Blob {
                offset: 0,
                limit: MAX_CHUNK,
                ..
            }
        ));
        for query in [
            json!({"method":"replica_event","hash":"not-a-hash"}),
            json!({"method":"replica_event","hash":hash,"offset":0}),
            json!({"method":"replica_blob","hash":hash,"offset":-1}),
            json!({"method":"replica_blob","hash":hash,"offset":0,"limit":65_537}),
            json!({"method":"replica_blob","hash":hash,"offset":0,"limit":0}),
        ] {
            assert!(parse_request(&body(query)).is_err());
        }
        let mut missing = body(json!({"method":"replica_inventory"}));
        missing.as_object_mut().unwrap().remove("resource_id");
        assert!(parse_request(&missing).is_err());
    }
}
