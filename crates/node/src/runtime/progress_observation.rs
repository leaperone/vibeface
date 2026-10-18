//! Temporary progress is read from its source; it never becomes a command or shared history.

use super::{Access, Runtime, Update, field, now_ms};
use anyhow::{Context, Result, ensure};
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{BTreeSet, HashMap, VecDeque},
    sync::Arc,
    time::{Duration, Instant},
};
use vibeface_secure::authorization::{
    AUTH_VERSION, Action, Actor, Audience, DeviceId, Invocation, Nonce, PublicKey, ResourceId,
    ServiceId, Signed, SpaceId,
};

const VERSION: u64 = 1;
const MAX_SCOPES: usize = 16;
const MAX_EVENTS: usize = 64;
const MAX_EVENT_BYTES: usize = 64 * 1024;
const MAX_SCOPE_BYTES: usize = 256 * 1024;
#[cfg(test)]
const LEASE: Duration = Duration::from_secs(45);
const RETENTION: Duration = Duration::from_secs(90);

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Cursor {
    generation: String,
    sequence: u64,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, PartialOrd, Ord)]
struct Stream {
    network: String,
    scope: String,
    source: String,
    resource: ResourceId,
}

struct Progress {
    generation: String,
    sequence: u64,
    bytes: usize,
    events: VecDeque<(u64, usize, Value)>,
    touched: Instant,
}

impl Progress {
    fn new() -> Self {
        Self {
            generation: uuid::Uuid::new_v4().to_string(),
            sequence: 0,
            bytes: 0,
            events: VecDeque::new(),
            touched: Instant::now(),
        }
    }

    fn push(&mut self, event: Value, bytes: usize) {
        self.sequence += 1;
        self.bytes += bytes;
        self.events.push_back((self.sequence, bytes, event));
        self.touched = Instant::now();
        while self.events.len() > MAX_EVENTS || self.bytes > MAX_SCOPE_BYTES {
            if let Some((_, bytes, _)) = self.events.pop_front() {
                self.bytes -= bytes;
            }
        }
    }

    fn read(&self, since: Option<&Cursor>) -> Value {
        let reset = since.map_or_else(
            || self.events.front().is_some_and(|(first, _, _)| *first > 1),
            |cursor| {
                cursor.generation != self.generation
                    || cursor.sequence > self.sequence
                    || self
                        .events
                        .front()
                        .is_some_and(|(first, _, _)| cursor.sequence < first.saturating_sub(1))
            },
        );
        let after = since.filter(|_| !reset).map_or(0, |cursor| cursor.sequence);
        json!({
            "version":VERSION,
            "cursor":Cursor {generation:self.generation.clone(), sequence:self.sequence},
            "reset":reset,
            "events":self.events.iter().filter(|(sequence,_,_)| *sequence > after).map(|(_,_,event)| event).collect::<Vec<_>>()
        })
    }
}

struct Lease {
    access: Arc<Access>,
    scope: String,
    targets: BTreeSet<String>,
    expires: Instant,
}

#[derive(Default)]
pub(crate) struct ObservationState {
    progress: HashMap<(String, String), Progress>,
    leases: HashMap<String, Lease>,
    cursors: HashMap<Stream, Cursor>,
}

impl ObservationState {
    fn prune(&mut self) {
        let now = Instant::now();
        self.leases.retain(|_, lease| lease.expires > now);
        self.progress
            .retain(|_, progress| now.duration_since(progress.touched) < RETENTION);
        let streams = self.streams();
        self.cursors.retain(|stream, _| streams.contains(stream));
    }

    fn streams(&self) -> BTreeSet<Stream> {
        self.leases
            .values()
            .flat_map(|lease| {
                lease.targets.iter().map(|source| Stream {
                    network: lease.access.network_id.clone(),
                    scope: lease.scope.clone(),
                    source: source.clone(),
                    resource: lease.access.authorization.resource().resource.clone(),
                })
            })
            .collect()
    }

    fn remove(&mut self, id: &str) {
        self.leases.remove(id);
        self.prune();
    }
}

fn valid_progress(event: &Value, scope: &str, source: &str) -> bool {
    matches!(
        event["type"].as_str(),
        Some("manager_progress" | "execution_progress")
    ) && event["conversation_id"].as_str() == Some(scope)
        && event["source_node"].as_str() == Some(source)
        && event["sequence"]
            .as_u64()
            .is_some_and(|sequence| sequence > 0)
        && (event["type"] != "execution_progress" || event["target_node"].as_str() == Some(source))
        && serde_json::to_vec(event).is_ok_and(|bytes| bytes.len() <= MAX_EVENT_BYTES)
}

fn bounded_text(text: &str, limit: usize) -> String {
    text.chars()
        .filter(|character| !character.is_control() || matches!(*character, '\n' | '\t'))
        .take(limit)
        .collect()
}

/// Provider tool arguments, raw outputs and reasoning never enter the live UI protocol.
pub(super) fn execution_progress(event: &Value) -> Value {
    let method = event["method"].as_str().unwrap_or("");
    let params = &event["params"];
    let item = &params["item"];
    let item_id = params["itemId"]
        .as_str()
        .or_else(|| item["id"].as_str())
        .unwrap_or("assistant");
    if method == "item/agentMessage/delta"
        && let Some(text) = params["delta"].as_str()
    {
        return json!({"type":"text_delta","item_id":bounded_text(item_id,128),"text":bounded_text(text,8000)});
    }
    if matches!(method, "item/started" | "item/completed")
        && item["type"] == "agentMessage"
        && let Some(text) = item["text"].as_str()
    {
        return json!({"type":"text_snapshot","item_id":bounded_text(item_id,128),"text":bounded_text(text,8000)});
    }
    if event["kind"] == "executor_progress" {
        let content = &event["event"];
        if content["type"] == "stream_event" {
            let stream = &content["event"];
            if stream["type"] == "content_block_delta"
                && stream["delta"]["type"] == "text_delta"
                && let Some(text) = stream["delta"]["text"].as_str()
            {
                return json!({"type":"text_delta","item_id":"assistant","text":bounded_text(text,8000)});
            }
            if stream["type"] == "message_start" {
                return json!({"type":"text_snapshot","item_id":"assistant","text":""});
            }
        }
        if content["type"] == "assistant" {
            let message = &content["message"];
            let body = message
                .get("content")
                .or_else(|| message.get("text"))
                .or_else(|| content.get("text"))
                .unwrap_or(message);
            let text = body.as_str().map(String::from).or_else(|| {
                body.as_array().map(|blocks| {
                    blocks
                        .iter()
                        .filter(|block| block["type"] == "text")
                        .filter_map(|block| block["text"].as_str())
                        .map(|text| bounded_text(text, 8000))
                        .collect::<Vec<_>>()
                        .join("\n")
                })
            });
            if let Some(text) = text.filter(|text| !text.is_empty()) {
                return json!({"type":"text_snapshot","item_id":"assistant","text":bounded_text(&text,8000)});
            }
        }
        let kind = content["type"].as_str().unwrap_or("working");
        return match kind {
            "tool_use" | "tool_call" | "tool_result" => {
                json!({"type":"tool","name":"executor","status":"running"})
            }
            "error" => json!({"type":"status","status":"failed"}),
            "result" => json!({"type":"status","status":"completed"}),
            _ => json!({"type":"status","status":"running"}),
        };
    }
    if method == "turn/completed" {
        let status = match params["turn"]["status"].as_str() {
            Some("completed") => "completed",
            Some("interrupted") => "interrupted",
            Some("failed") => "failed",
            _ => "unknown",
        };
        return json!({"type":"status","status":status});
    }
    if method.ends_with("requestApproval") || event.get("id").is_some() {
        return json!({"type":"status","status":"awaiting_input"});
    }
    if method.starts_with("item/") {
        let name = match item["type"].as_str() {
            Some("commandExecution") => "command",
            Some("fileChange") => "file change",
            Some("mcpToolCall" | "dynamicToolCall") => "tool",
            Some("webSearch") => "web search",
            _ => "executor",
        };
        return json!({"type":"tool","item_id":bounded_text(item_id,128),"name":name,"status":if method == "item/completed" {"completed"} else {"running"}});
    }
    json!({"type":"status","status":"running"})
}

impl Runtime {
    /// Only locally produced progress enters this ring. Proxied reads use events.send directly.
    pub(crate) fn publish_progress(&self, network: &str, mut payload: Value) {
        let Ok(source) = self.identity.node_id() else {
            return;
        };
        payload["source_node"] = json!(source);
        payload["source"] = json!(source);
        let Some(scope) = payload["conversation_id"].as_str() else {
            return;
        };
        if !valid_progress(&payload, scope, &source) {
            return;
        }
        let Ok(bytes) = serde_json::to_vec(&payload) else {
            return;
        };
        if let Ok(mut state) = self.progress_observation.lock() {
            state.prune();
            let key = (network.to_owned(), scope.to_owned());
            if !state.progress.contains_key(&key)
                && state.progress.len() >= MAX_SCOPES
                && let Some(oldest) = state
                    .progress
                    .iter()
                    .min_by_key(|(_, progress)| progress.touched)
                    .map(|(key, _)| key.clone())
            {
                state.progress.remove(&oldest);
            }
            state
                .progress
                .entry(key)
                .or_insert_with(Progress::new)
                .push(payload.clone(), bytes.len());
        }
        let _ = self.events.send(Update {
            network_id: network.into(),
            payload,
        });
    }

    pub(crate) async fn read_progress(&self, access: &Access, input: &Value) -> Result<Value> {
        access.require_body(input)?;
        ensure!(
            input["method"] == "progress_read"
                && access.authorization.action() == Action::ConversationRead,
            "progress read requires signed conversation read permission"
        );
        ensure!(
            input["version"].as_u64() == Some(VERSION),
            "unsupported progress version"
        );
        let scope = field(input, "conversation_id")?;
        self.validate_subscription(access).await?;
        self.require_scope(access, scope).await?;
        let since = input
            .get("cursor")
            .filter(|cursor| !cursor.is_null())
            .map(|cursor| serde_json::from_value::<Cursor>(cursor.clone()))
            .transpose()?;
        ensure!(
            since
                .as_ref()
                .is_none_or(|cursor| cursor.generation.len() <= 64),
            "invalid progress cursor"
        );
        let source = self.identity.node_id()?;
        let mut state = self
            .progress_observation
            .lock()
            .map_err(|_| anyhow::anyhow!("progress observer unavailable"))?;
        state.prune();
        let key = (access.network_id.clone(), scope.to_owned());
        let mut reply = if let Some(progress) = state.progress.get(&key) {
            progress.read(since.as_ref())
        } else {
            json!({"version":VERSION,"cursor":null,"reset":since.is_some(),"events":[]})
        };
        reply["source_node"] = json!(source);
        reply["conversation_id"] = json!(scope);
        Ok(reply)
    }

    pub(crate) fn start_observation_worker(self: &Arc<Self>) {
        let weak = Arc::downgrade(self);
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_millis(500));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tick.tick().await;
                let Some(node) = weak.upgrade() else { break };
                node.poll_observed_progress().await;
            }
        });
    }

    async fn poll_observed_progress(self: &Arc<Self>) {
        let streams = {
            let Ok(mut state) = self.progress_observation.lock() else {
                return;
            };
            state.prune();
            state.streams()
        };
        futures_util::stream::iter(streams)
            .for_each_concurrent(8, |stream| async move {
                if let Err(error) = self.poll_progress_source(&stream).await {
                    tracing::debug!(%error, "read-only progress source unavailable");
                }
            })
            .await;
    }

    async fn authorized_progress_observers(&self, stream: &Stream) -> Result<bool> {
        let observers = {
            let mut state = self
                .progress_observation
                .lock()
                .map_err(|_| anyhow::anyhow!("progress observer unavailable"))?;
            state.prune();
            state
                .leases
                .iter()
                .filter(|(_, lease)| {
                    lease.access.network_id == stream.network
                        && lease.scope == stream.scope
                        && lease.access.authorization.resource().resource == stream.resource
                        && lease.targets.contains(&stream.source)
                })
                .map(|(id, lease)| (id.clone(), lease.access.clone()))
                .collect::<Vec<_>>()
        };
        let mut authorized = false;
        for (id, access) in observers {
            let current = async {
                let current = self.refresh_subscription(&access).await?;
                ensure!(
                    current.account_actor(),
                    "progress observer is not an Account"
                );
                self.require_scope(&current, &stream.scope).await?;
                let store = self.store.lock().await;
                store.authorize_node(&stream.network, &stream.source, None)?;
                Ok::<_, anyhow::Error>(current)
            }
            .await;
            let mut state = self
                .progress_observation
                .lock()
                .map_err(|_| anyhow::anyhow!("progress observer unavailable"))?;
            state.prune();
            match current {
                Ok(current) => {
                    if let Some(lease) = state.leases.get_mut(&id) {
                        lease.access = Arc::new(current);
                        authorized = true;
                    }
                }
                Err(_) => state.remove(&id),
            }
        }
        Ok(authorized)
    }

    async fn request_progress(&self, stream: &Stream, cursor: Option<Cursor>) -> Result<Value> {
        let identity = vibeface_secure::Identity::from_secret_hex(&self.identity.secret_key_hex)
            .map_err(anyhow::Error::msg)?;
        let key = PublicKey::of(&identity);
        let service = ServiceId::from_key(&key);
        let body = json!({
            "method":"progress_read", "version":VERSION, "space_id":stream.network,
            "resource_id":stream.resource, "conversation_id":stream.scope,
            "authorization_id":uuid::Uuid::new_v4().to_string(), "cursor":cursor,
        });
        let invocation = {
            let store = self.store.lock().await;
            let now = now_ms() as u64;
            let snapshot = store.connection.unchecked_transaction()?;
            let current =
                crate::identity::journal::PolicyState::reload_verified(&snapshot, &store.policy)?;
            let target = store.authorize_node(&stream.network, &stream.source, None)?;
            let target_key = PublicKey::parse(&target.public_key)?;
            let audience = Audience {
                service: ServiceId::from_key(&target_key),
                device: DeviceId::from_key(&target_key),
            };
            let facts = crate::identity::policy::request_facts_from(
                &snapshot,
                current.verified(),
                &body,
                &audience,
            )?;
            ensure!(
                facts.action == Action::ConversationRead,
                "invalid progress read action"
            );
            let space = current
                .verified()
                .space(&SpaceId::parse(&stream.network)?)
                .context("progress read space policy is missing")?;
            let mut invocation = None;
            for leaf in &space.grants {
                let Ok(grant) = current
                    .verified()
                    .verify_service_grant(&service, leaf, &facts, now)
                else {
                    continue;
                };
                ensure!(
                    grant.device() == &DeviceId::from_key(&key),
                    "progress read grant belongs to another service device"
                );
                let expires_at_ms = now
                    .saturating_add(vibeface_secure::authorization::INVOCATION_MAX_AGE_MS)
                    .min(grant.expires_at_ms());
                ensure!(now < expires_at_ms, "progress read grant expired");
                invocation = Some(Signed::sign(
                    Invocation {
                        version: AUTH_VERSION,
                        actor: Actor::Service(service.clone()),
                        device: grant.device().clone(),
                        initiator: grant.initiator().clone(),
                        initiating_grant: grant.initiating_grant().cloned(),
                        resource: facts.resource.clone(),
                        action: facts.action,
                        audience: facts.audience.clone(),
                        presenter: grant.device().clone(),
                        operation_id: facts.operation_id.clone(),
                        body_hash: facts.body_hash.clone(),
                        grant_leaf: grant.grant_leaf().clone(),
                        expected_policies: grant.heads().to_vec(),
                        context: facts.context.clone(),
                        nonce: Nonce::random()?,
                        not_before_ms: now,
                        expires_at_ms,
                    },
                    &identity,
                )?);
                break;
            }
            snapshot.commit()?;
            invocation.context(
                "service has no current conversation read grant for this progress source",
            )?
        };
        self.peer_request(
            &stream.network,
            &stream.source,
            json!({
                "type":"progress_read", "authorization_body":body, "invocation":invocation,
            }),
        )
        .await
    }

    async fn poll_progress_source(&self, stream: &Stream) -> Result<()> {
        if !self.authorized_progress_observers(stream).await? {
            return Ok(());
        }
        if !self
            .hub
            .online(&stream.network)
            .await
            .contains(&stream.source)
        {
            return Ok(());
        }
        let cursor = {
            let state = self
                .progress_observation
                .lock()
                .map_err(|_| anyhow::anyhow!("progress observer unavailable"))?;
            if !state.streams().contains(stream) {
                return Ok(());
            }
            state.cursors.get(stream).cloned()
        };
        let reply = tokio::time::timeout(
            Duration::from_secs(2),
            self.request_progress(stream, cursor),
        )
        .await
        .context("progress read timed out")??;
        ensure!(
            reply["version"].as_u64() == Some(VERSION),
            "unsupported progress version"
        );
        ensure!(
            reply["source_node"].as_str() == Some(&stream.source)
                && reply["conversation_id"].as_str() == Some(&stream.scope),
            "progress identity mismatch"
        );
        let events = reply["events"]
            .as_array()
            .context("invalid progress events")?;
        ensure!(
            events.len() <= MAX_EVENTS
                && serde_json::to_vec(&reply)?.len() <= MAX_SCOPE_BYTES + 65536,
            "oversized progress reply"
        );
        ensure!(
            events
                .iter()
                .all(|event| valid_progress(event, &stream.scope, &stream.source)),
            "invalid progress source"
        );
        let cursor = reply
            .get("cursor")
            .filter(|cursor| !cursor.is_null())
            .map(|cursor| serde_json::from_value::<Cursor>(cursor.clone()))
            .transpose()?;
        ensure!(
            cursor
                .as_ref()
                .is_none_or(|cursor| cursor.generation.len() <= 64),
            "invalid progress cursor"
        );
        if !self.authorized_progress_observers(stream).await? {
            return Ok(());
        }
        {
            let mut state = self
                .progress_observation
                .lock()
                .map_err(|_| anyhow::anyhow!("progress observer unavailable"))?;
            state.prune();
            if !state.streams().contains(stream) {
                return Ok(());
            }
            if let Some(cursor) = cursor {
                state.cursors.insert(stream.clone(), cursor);
            } else {
                state.cursors.remove(stream);
            }
        }
        // Forward locally only: capturing these frames would create proxy loops and false sources.
        if reply["reset"].as_bool() == Some(true) {
            let _ = self.events.send(Update {
                network_id: stream.network.clone(),
                payload: json!({"type":"changed","conversation_id":stream.scope}),
            });
        }
        for payload in events {
            let _ = self.events.send(Update {
                network_id: stream.network.clone(),
                payload: payload.clone(),
            });
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::identity::{
        policy::RequestTransport, task_execution_context::TaskScopedAuthorization,
        test_support::TestNetwork,
    };
    use vibeface_secure::{
        authorization::CheckStage, signature_verification::SignatureVerification,
    };

    fn test_access() -> Result<Arc<Access>> {
        let network = TestNetwork::new()?;
        let mut store = network.memory()?;
        network.conversation(&mut store, "chat")?;
        let body = network.body("conversation", "chat");
        let now = now_ms() as u64;
        let (authorization, transport) = network.authorization(&store, &body, now)?;
        let freshness =
            network.freshness(&mut store, &authorization, CheckStage::RemoteRead, now)?;
        Ok(Arc::new(Access {
            network_id: network.space.to_string(),
            conversation_id: Some("chat".into()),
            authorization: TaskScopedAuthorization::from_primary(authorization),
            freshness,
            transport: RequestTransport::Existing(transport),
            body,
            verification: SignatureVerification::new(),
            manager: None,
        }))
    }

    fn event(scope: &str, source: &str, sequence: u64) -> Value {
        json!({"type":"manager_progress","conversation_id":scope,"source_node":source,"sequence":sequence,"progress":{"type":"text_snapshot","item_id":"one","text":"hello"}})
    }

    #[tokio::test]
    async fn progress_requires_current_signed_resource_authority() -> Result<()> {
        let (network, node) = crate::runtime::tests::runtime_fixture().await?;
        let mut body = network.body("progress_read", "conversation");
        body["version"] = json!(VERSION);
        let access = {
            let now = now_ms() as u64;
            let mut store = node.store.lock().await;
            let (authorization, transport) = network.authorization(&store, &body, now)?;
            let freshness =
                network.freshness(&mut store, &authorization, CheckStage::RemoteRead, now)?;
            Arc::new(Access {
                network_id: network.space.to_string(),
                conversation_id: Some("conversation".into()),
                authorization: TaskScopedAuthorization::from_primary(authorization),
                freshness,
                transport: RequestTransport::Existing(transport),
                body: body.clone(),
                verification: SignatureVerification::new(),
                manager: None,
            })
        };
        let source = node.identity.node_id()?;
        node.publish_progress(network.space.as_str(), event("conversation", &source, 1));
        let result = node.read_progress(&access, &body).await?;
        assert_eq!(result["events"].as_array().unwrap().len(), 1);
        let mut changed = body.clone();
        changed["conversation_id"] = json!("another-conversation");
        assert!(node.read_progress(&access, &changed).await.is_err());
        let stream = Stream {
            network: network.space.to_string(),
            scope: "conversation".into(),
            source: source.clone(),
            resource: access.authorization.resource().resource.clone(),
        };
        node.progress_observation.lock().unwrap().leases.insert(
            "reader".into(),
            Lease {
                access: access.clone(),
                scope: stream.scope.clone(),
                targets: BTreeSet::from([source]),
                expires: Instant::now() + LEASE,
            },
        );
        assert!(node.authorized_progress_observers(&stream).await?);
        network.revoke_client(&mut *node.store.lock().await)?;
        assert!(!node.authorized_progress_observers(&stream).await?);
        assert!(node.progress_observation.lock().unwrap().leases.is_empty());
        assert!(node.read_progress(&access, &body).await.is_err());
        Ok(())
    }

    #[test]
    fn execution_text_distinguishes_deltas_snapshots_and_private_tool_arguments() {
        let delta = execution_progress(
            &json!({"method":"item/agentMessage/delta","params":{"itemId":"reply","delta":"你好\n"}}),
        );
        assert_eq!(
            delta,
            json!({"type":"text_delta","item_id":"reply","text":"你好\n"})
        );
        let snapshot = execution_progress(
            &json!({"method":"item/completed","params":{"item":{"id":"reply","type":"agentMessage","text":"你好\n完成"}}}),
        );
        assert_eq!(
            snapshot,
            json!({"type":"text_snapshot","item_id":"reply","text":"你好\n完成"})
        );
        let cli = execution_progress(
            &json!({"kind":"executor_progress","event":{"type":"assistant","message":{"content":[{"type":"text","text":"第一行"},{"type":"text","text":"第二行"},{"type":"tool_use","input":{"secret":"private-value"}}]}}}),
        );
        assert_eq!(cli["text"], "第一行\n第二行");
        assert_eq!(
            execution_progress(
                &json!({"kind":"executor_progress","event":{"type":"assistant","message":"working"}})
            )["text"],
            "working"
        );
        assert_eq!(
            execution_progress(
                &json!({"kind":"executor_progress","event":{"type":"assistant","message":{"text":"working"}}})
            )["text"],
            "working"
        );
        let tool = execution_progress(
            &json!({"method":"item/completed","params":{"item":{"id":"tool","type":"commandExecution","command":"private-value","output":"private-value"}}}),
        );
        assert_eq!(tool["type"], "tool");
        assert_eq!(tool["status"], "completed");
        assert!(!tool.to_string().contains("private-value"));
        let reasoning = execution_progress(
            &json!({"method":"item/reasoning/textDelta","params":{"delta":"private-value"}}),
        );
        assert!(!reasoning.to_string().contains("private-value"));
    }

    #[test]
    fn progress_is_bounded_and_cursor_gaps_require_snapshot() {
        let mut progress = Progress::new();
        let cursor = Cursor {
            generation: progress.generation.clone(),
            sequence: 0,
        };
        for sequence in 1..=100 {
            let event = event("chat", "node", sequence);
            progress.push(event.clone(), serde_json::to_vec(&event).unwrap().len());
        }
        let reply = progress.read(Some(&cursor));
        assert_eq!(reply["reset"], true);
        assert_eq!(progress.read(None)["reset"], true);
        assert_eq!(reply["events"].as_array().unwrap().len(), MAX_EVENTS);
        assert_eq!(reply["events"][0]["sequence"], 37);
        let cursor: Cursor = serde_json::from_value(reply["cursor"].clone()).unwrap();
        assert!(
            progress.read(Some(&cursor))["events"]
                .as_array()
                .unwrap()
                .is_empty()
        );
        let restarted = Progress::new();
        assert_eq!(restarted.read(Some(&cursor))["reset"], true);
    }

    #[test]
    fn streams_are_network_scope_and_source_isolated_and_expire() -> Result<()> {
        let access = test_access()?;
        let network = access.network_id.clone();
        let other = test_access()?;
        let mut state = ObservationState::default();
        state.leases.insert(
            "live".into(),
            Lease {
                access: access.clone(),
                scope: "chat".into(),
                targets: BTreeSet::from(["manager".into(), "executor".into()]),
                expires: Instant::now() + LEASE,
            },
        );
        state.leases.insert(
            "expired".into(),
            Lease {
                access: other,
                scope: "chat".into(),
                targets: BTreeSet::from(["private".into()]),
                expires: Instant::now() - Duration::from_secs(1),
            },
        );
        state.prune();
        assert_eq!(state.streams().len(), 2);
        assert!(
            state
                .streams()
                .iter()
                .all(|stream| stream.network == network && stream.scope == "chat")
        );
        state.remove("live");
        assert!(state.streams().is_empty());
        assert!(valid_progress(
            &event("chat", "manager", 1),
            "chat",
            "manager"
        ));
        assert!(!valid_progress(
            &event("chat", "manager", 1),
            "other",
            "manager"
        ));
        assert!(!valid_progress(
            &event("chat", "proxy", 1),
            "chat",
            "manager"
        ));
        assert!(!valid_progress(
            &event("chat", "manager", 0),
            "chat",
            "manager"
        ));
        Ok(())
    }
}
