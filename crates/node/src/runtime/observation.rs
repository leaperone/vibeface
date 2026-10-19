//! Temporary progress is read from its source; it never becomes a command or shared history.

use super::{Access, Runtime, Update, field};
use anyhow::{Context, Result, ensure};
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{BTreeSet, HashMap, VecDeque},
    sync::{Arc, Weak},
    time::{Duration, Instant},
};

const VERSION: u64 = 1;
const MAX_SCOPES: usize = 16;
const MAX_EVENTS: usize = 64;
const MAX_EVENT_BYTES: usize = 64 * 1024;
const MAX_SCOPE_BYTES: usize = 256 * 1024;
const MAX_OBSERVERS: usize = 32;
const MAX_TARGETS: usize = 8;
const MAX_STREAMS: usize = 64;
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
    network: String,
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
                    network: lease.network.clone(),
                    scope: lease.scope.clone(),
                    source: source.clone(),
                })
            })
            .collect()
    }

    fn remove(&mut self, id: &str) {
        self.leases.remove(id);
        self.prune();
    }
}

/// Held by the encrypted client connection; every return/error path drops its lease.
pub(crate) struct ObservationGuard {
    runtime: Weak<Runtime>,
    id: String,
}

impl ObservationGuard {
    pub(crate) fn refresh(&self) {
        if let Some(node) = self.runtime.upgrade()
            && let Ok(mut state) = node.observation.lock()
            && let Some(lease) = state.leases.get_mut(&self.id)
        {
            lease.expires = Instant::now() + LEASE;
        }
    }
}

impl Drop for ObservationGuard {
    fn drop(&mut self) {
        if let Some(node) = self.runtime.upgrade()
            && let Ok(mut state) = node.observation.lock()
        {
            state.remove(&self.id);
        }
    }
}

fn valid_progress(event: &Value, scope: &str, source: &str) -> bool {
    matches!(
        event["type"].as_str(),
        Some("manager_progress" | "native_progress")
    ) && event["conversation_id"].as_str() == Some(scope)
        && event["source_node"].as_str() == Some(source)
        && event["sequence"]
            .as_u64()
            .is_some_and(|sequence| sequence > 0)
        && (event["type"] != "native_progress" || event["target_node"].as_str() == Some(source))
        && serde_json::to_vec(event).is_ok_and(|bytes| bytes.len() <= MAX_EVENT_BYTES)
}

fn bounded_text(text: &str, limit: usize) -> String {
    text.chars()
        .filter(|character| !character.is_control() || matches!(*character, '\n' | '\t'))
        .take(limit)
        .collect()
}

/// Provider tool arguments, raw outputs and reasoning never enter the live UI protocol.
pub(super) fn native_progress(event: &Value) -> Value {
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
        let Ok(source) = self.identity.peer_id() else {
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
        if let Ok(mut state) = self.observation.lock() {
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

    pub(crate) async fn observe_conversation(
        self: &Arc<Self>,
        access: &Access,
        scope: &str,
        targets: Vec<String>,
    ) -> Result<ObservationGuard> {
        ensure!(access.human, "progress observation requires a client");
        self.require_scope(access, scope).await?;
        let local = self.identity.peer_id()?;
        let targets = targets
            .into_iter()
            .filter(|target| target != &local)
            .collect::<BTreeSet<_>>();
        ensure!(targets.len() <= MAX_TARGETS, "too many progress sources");
        {
            let store = self.store.lock().await;
            for target in &targets {
                store.authorize_node(&access.network_id, target, None)?;
            }
        }
        let mut state = self
            .observation
            .lock()
            .map_err(|_| anyhow::anyhow!("progress observer unavailable"))?;
        state.prune();
        ensure!(
            state.leases.len() < MAX_OBSERVERS,
            "too many progress observers"
        );
        let id = uuid::Uuid::new_v4().to_string();
        state.leases.insert(
            id.clone(),
            Lease {
                network: access.network_id.clone(),
                scope: scope.into(),
                targets,
                expires: Instant::now() + LEASE,
            },
        );
        if state.streams().len() > MAX_STREAMS {
            state.remove(&id);
            anyhow::bail!("too many observed progress streams");
        }
        Ok(ObservationGuard {
            runtime: Arc::downgrade(self),
            id,
        })
    }

    /// Called only from authenticated peer request dispatch, never from broadcast handling.
    pub(crate) async fn read_progress(
        &self,
        network: &str,
        origin: &str,
        input: &Value,
    ) -> Result<Value> {
        ensure!(
            input["version"].as_u64() == Some(VERSION),
            "unsupported progress version"
        );
        let scope = field(input, "conversation_id")?;
        self.membership_state().await?;
        self.require_scope(
            &Access {
                network_id: network.into(),
                human: false,
                conversation_id: Some(scope.into()),
                manager: None,
            },
            scope,
        )
        .await?;
        self.store
            .lock()
            .await
            .authorize_node(network, origin, None)?;
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
        let source = self.identity.peer_id()?;
        let mut state = self
            .observation
            .lock()
            .map_err(|_| anyhow::anyhow!("progress observer unavailable"))?;
        state.prune();
        let key = (network.to_owned(), scope.to_owned());
        let mut reply = if let Some(progress) = state.progress.get(&key) {
            progress.read(since.as_ref())
        } else {
            // No log is allocated by reads; idle polling cannot evict a live conversation.
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
            let Ok(mut state) = self.observation.lock() else {
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

    async fn poll_progress_source(&self, stream: &Stream) -> Result<()> {
        // Authorization is checked again before every poll, including after membership changes.
        let authorized = async {
            self.membership_state().await?;
            self.store
                .lock()
                .await
                .authorize_node(&stream.network, &stream.source, None)?;
            self.require_scope(
                &Access {
                    network_id: stream.network.clone(),
                    human: true,
                    conversation_id: Some(stream.scope.clone()),
                    manager: None,
                },
                &stream.scope,
            )
            .await
        }
        .await;
        if let Err(error) = authorized {
            if let Ok(mut state) = self.observation.lock() {
                state.leases.retain(|_, lease| {
                    lease.network != stream.network || lease.scope != stream.scope
                });
                state.prune();
            }
            return Err(error);
        }
        let cursor = {
            let state = self
                .observation
                .lock()
                .map_err(|_| anyhow::anyhow!("progress observer unavailable"))?;
            if !state.streams().contains(stream) {
                return Ok(());
            }
            state.cursors.get(stream).cloned()
        };
        let reply = tokio::time::timeout(
            Duration::from_secs(2),
            self.peer_request(
                &stream.network,
                &stream.source,
                json!({
                    "type":"progress_read", "version":VERSION,
                    "conversation_id":stream.scope, "cursor":cursor,
                }),
            ),
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
        {
            let store = self.store.lock().await;
            store.authorize_node(&stream.network, &stream.source, None)?;
            store.authorize_node(&stream.network, &self.identity.peer_id()?, None)?;
        }
        {
            let mut state = self
                .observation
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

    fn event(scope: &str, source: &str, sequence: u64) -> Value {
        json!({"type":"manager_progress","conversation_id":scope,"source_node":source,"sequence":sequence,"progress":{"type":"text_snapshot","item_id":"one","text":"hello"}})
    }

    #[test]
    fn execution_text_distinguishes_deltas_snapshots_and_private_tool_arguments() {
        let delta = native_progress(
            &json!({"method":"item/agentMessage/delta","params":{"itemId":"reply","delta":"你好\n"}}),
        );
        assert_eq!(
            delta,
            json!({"type":"text_delta","item_id":"reply","text":"你好\n"})
        );
        let snapshot = native_progress(
            &json!({"method":"item/completed","params":{"item":{"id":"reply","type":"agentMessage","text":"你好\n完成"}}}),
        );
        assert_eq!(
            snapshot,
            json!({"type":"text_snapshot","item_id":"reply","text":"你好\n完成"})
        );
        let tool = native_progress(
            &json!({"method":"item/completed","params":{"item":{"id":"tool","type":"commandExecution","command":"private-value","output":"private-value"}}}),
        );
        assert_eq!(tool["type"], "tool");
        assert_eq!(tool["status"], "completed");
        assert!(!tool.to_string().contains("private-value"));
        let reasoning = native_progress(
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
    fn streams_are_network_scope_and_source_isolated_and_expire() {
        let mut state = ObservationState::default();
        state.leases.insert(
            "live".into(),
            Lease {
                network: "mine".into(),
                scope: "chat".into(),
                targets: BTreeSet::from(["manager".into(), "executor".into()]),
                expires: Instant::now() + LEASE,
            },
        );
        state.leases.insert(
            "expired".into(),
            Lease {
                network: "other".into(),
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
                .all(|stream| stream.network == "mine" && stream.scope == "chat")
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
    }
}
