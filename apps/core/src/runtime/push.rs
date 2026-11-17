//! Entry-Node-local push subscriptions. Nothing here is part of the shared event log.
use super::*;
use rusqlite::OptionalExtension as _;
use serde::{Deserialize, Serialize};

const STATE_KEY: &str = "expo-push";
const EXPO_ENDPOINT: &str = "https://exp.host/--/api/v2/push/send";
const LEASE_MS: i64 = 30 * 24 * 60 * 60_000;
const RESPONSE_LIMIT: usize = 64 * 1024;
const BATCH_LIMIT: usize = 50;

#[derive(Default, Serialize, Deserialize)]
struct PushState {
    subscriptions: Vec<Subscription>,
}

// Deliberately no Debug: tokens must never appear in diagnostics.
#[derive(Serialize, Deserialize)]
struct Subscription {
    network: String,
    device: String,
    token: Option<String>,
    generation: String,
    expires_at_ms: i64,
    sound: bool,
    badge: bool,
    muted: BTreeSet<String>,
    // Includes the initial unread baseline and deliberately suppressed notifications. Keep
    // tombstones across projection rebuilds, preference/token changes and unregistering.
    handled: BTreeSet<String>,
    retries: BTreeMap<String, Retry>,
}

#[derive(Default, Serialize, Deserialize)]
struct Retry {
    attempts: u32,
    after_ms: i64,
}

#[derive(Clone)]
struct Job {
    network: String,
    device: String,
    generation: String,
    notification: Notification,
}

#[derive(Debug, PartialEq, Eq)]
enum Delivery {
    Accepted,
    DeviceNotRegistered,
}

type SendFuture<'a> = Pin<Box<dyn Future<Output = Result<Delivery>> + Send + 'a>>;
trait PushTransport: Send + Sync {
    fn send(&self, payload: Value) -> SendFuture<'_>;
}

struct ExpoTransport(reqwest::Client);
impl ExpoTransport {
    fn new() -> Result<Self> {
        Ok(Self(
            reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .connect_timeout(Duration::from_secs(5))
                .timeout(Duration::from_secs(10))
                .build()?,
        ))
    }

    // The URL is private to this transport. Production callers can only use EXPO_ENDPOINT.
    async fn send_to(&self, endpoint: &str, payload: Value) -> Result<Delivery> {
        let mut response = self.0.post(endpoint).json(&payload).send().await?;
        ensure!(
            response.status().is_success(),
            "push service refused request"
        );
        ensure!(
            response
                .content_length()
                .is_none_or(|n| n <= RESPONSE_LIMIT as u64),
            "push response exceeds limit"
        );
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await? {
            ensure!(
                bytes.len() + chunk.len() <= RESPONSE_LIMIT,
                "push response exceeds limit"
            );
            bytes.extend_from_slice(&chunk);
        }
        parse_ticket(&bytes)
    }
}
impl PushTransport for ExpoTransport {
    fn send(&self, payload: Value) -> SendFuture<'_> {
        Box::pin(self.send_to(EXPO_ENDPOINT, payload))
    }
}

fn parse_ticket(bytes: &[u8]) -> Result<Delivery> {
    let value: Value = serde_json::from_slice(bytes)?;
    let data = &value["data"];
    let ticket = if let Some(tickets) = data.as_array() {
        ensure!(tickets.len() == 1, "unexpected push ticket count");
        &tickets[0]
    } else {
        data
    };
    if ticket["status"] == "error" && ticket["details"]["error"] == "DeviceNotRegistered" {
        return Ok(Delivery::DeviceNotRegistered);
    }
    ensure!(
        ticket["status"] == "ok" && ticket["id"].as_str().is_some_and(|id| !id.is_empty()),
        "push service did not accept notification"
    );
    // A ticket proves acceptance by Expo, not delivery to APNs/FCM or device display.
    Ok(Delivery::Accepted)
}

fn push_principal(access: &Access, input: &Value) -> Result<(String, String)> {
    ensure!(
        matches!(
            access,
            Access::User {
                scope: UserScope::Network,
                ..
            }
        ),
        "push registration requires a network-scoped user"
    );
    ensure!(
        input.get("conversation_id").is_none(),
        "push registration is device-scoped"
    );
    // client.rs injects this from the authenticated Noise principal and rejects overrides.
    Ok((access.network_id().to_owned(), id(input, "client_id")?))
}

fn valid_token(token: &str) -> bool {
    if token.len() > 256 {
        return false;
    }
    let inner = token
        .strip_prefix("ExpoPushToken[")
        .or_else(|| token.strip_prefix("ExponentPushToken["))
        .and_then(|v| v.strip_suffix(']'));
    inner.is_some_and(|value| {
        !value.is_empty()
            && value
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'-')
    })
}

impl Store {
    fn push_state(&self) -> Result<PushState> {
        let stored: Option<String> = self
            .connection
            .query_row(
                "SELECT value FROM identity_meta WHERE key=?",
                [STATE_KEY],
                |row| row.get(0),
            )
            .optional()?;
        Ok(stored
            .map(|s| serde_json::from_str(&s))
            .transpose()?
            .unwrap_or_default())
    }

    fn keep_push_state(&self, state: &PushState) -> Result<()> {
        self.connection.execute(
            "INSERT INTO identity_meta(key,value) VALUES(?,?) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
            [STATE_KEY, &serde_json::to_string(state)?],
        )?;
        Ok(())
    }

    fn push_authorized(&self, network: &str, device: &str, node: &str) -> Result<bool> {
        let state = self.public_state()?;
        if state.genesis.network_id != network {
            return Ok(false);
        }
        let view = state.project()?;
        Ok(view.node(node).is_ok() && view.client(device).is_ok())
    }

    fn push_jobs(&self, node: &str, now: i64) -> Result<Vec<Job>> {
        let mut state = self.push_state()?;
        let mut jobs = Vec::new();
        let mut changed = false;
        for subscription in &mut state.subscriptions {
            if subscription.token.is_none() {
                continue;
            }
            if subscription.expires_at_ms <= now
                || !self.push_authorized(&subscription.network, &subscription.device, node)?
            {
                subscription.token = None;
                subscription.retries.clear();
                changed = true;
                continue;
            }
            let pending =
                self.pending_notifications(&subscription.network, &subscription.device)?;
            let retry_count = subscription.retries.len();
            subscription
                .retries
                .retain(|id, _| pending.iter().any(|n| &n.notification_id == id));
            changed |= subscription.retries.len() != retry_count;
            for notification in pending {
                if subscription.handled.contains(&notification.notification_id) {
                    continue;
                }
                if !self.push_visible(subscription, &notification)? {
                    subscription
                        .handled
                        .insert(notification.notification_id.clone());
                    subscription.retries.remove(&notification.notification_id);
                    changed = true;
                    continue;
                }
                if jobs.len() < BATCH_LIMIT
                    && subscription
                        .retries
                        .get(&notification.notification_id)
                        .is_none_or(|r| r.after_ms <= now)
                {
                    jobs.push(Job {
                        network: subscription.network.clone(),
                        device: subscription.device.clone(),
                        generation: subscription.generation.clone(),
                        notification,
                    });
                }
            }
        }
        if changed {
            self.keep_push_state(&state)?;
        }
        Ok(jobs)
    }

    fn push_visible(
        &self,
        subscription: &Subscription,
        notification: &Notification,
    ) -> Result<bool> {
        if subscription.muted.contains(&notification.conversation_id)
            || !matches!(
                notification.kind.as_str(),
                "manager_response" | "manager_attention" | "bot_message" | "question"
            )
        {
            return Ok(false);
        }
        self.notification_visible(&subscription.network, notification)
    }

    /// Recheck immediately before the HTTP boundary: unregistering, governance changes,
    /// expiry, a read acknowledgement or preference changes invalidate a prepared job.
    fn push_payload(&self, job: &Job, node: &str, now: i64) -> Result<Option<Value>> {
        let state = self.push_state()?;
        let Some(subscription) = state.subscriptions.iter().find(|s| {
            s.network == job.network && s.device == job.device && s.generation == job.generation
        }) else {
            return Ok(None);
        };
        let Some(token) = &subscription.token else {
            return Ok(None);
        };
        if subscription.expires_at_ms <= now
            || !self.push_authorized(&job.network, &job.device, node)?
            || subscription
                .handled
                .contains(&job.notification.notification_id)
            || !self.push_visible(subscription, &job.notification)?
            || !self
                .pending_notifications(&job.network, &job.device)?
                .iter()
                .any(|n| n.notification_id == job.notification.notification_id)
        {
            return Ok(None);
        }
        let mut payload = json!({
            "to":token,
            "title":"Vibeface",
            "body":if job.notification.kind == "question" { "Bot 有一个问题需要你查看" } else { "Bot 有新回复" },
            "channelId":if subscription.sound { "vibeface" } else { "vibeface-silent" },
            "ttl":300,
            "collapseId":job.notification.notification_id,
            "tag":job.notification.notification_id,
            "data":{"destination":{
                "network_id":job.network,
                "device_id":job.device,
                "notification_id":job.notification.notification_id,
                "conversation_id":job.notification.conversation_id,
            }},
        });
        if subscription.sound {
            payload["sound"] = json!("default");
        }
        if subscription.badge {
            // Count only this device's visible unread notifications; pushing never acks them.
            let mut count = 0;
            for notification in self.pending_notifications(&job.network, &job.device)? {
                if self.push_visible(subscription, &notification)? {
                    count += 1;
                }
            }
            payload["badge"] = json!(count);
        }
        Ok(Some(payload))
    }

    fn push_result(&self, job: &Job, result: Option<Delivery>, now: i64) -> Result<()> {
        let mut state = self.push_state()?;
        let Some(subscription) = state.subscriptions.iter_mut().find(|s| {
            s.network == job.network
                && s.device == job.device
                && s.generation == job.generation
                && s.token.is_some()
        }) else {
            return Ok(());
        };
        match result {
            Some(Delivery::Accepted) => {
                subscription
                    .handled
                    .insert(job.notification.notification_id.clone());
                subscription
                    .retries
                    .remove(&job.notification.notification_id);
            }
            Some(Delivery::DeviceNotRegistered) => {
                subscription.token = None;
                subscription.retries.clear();
            }
            None => {
                let retry = subscription
                    .retries
                    .entry(job.notification.notification_id.clone())
                    .or_default();
                retry.attempts = retry.attempts.saturating_add(1);
                retry.after_ms = now
                    .saturating_add((15_000_i64 * (1 << retry.attempts.min(8))).min(60 * 60_000));
            }
        }
        self.keep_push_state(&state)
    }
}

impl Runtime {
    pub(super) async fn register_push(&self, access: &Access, input: &Value) -> Result<Value> {
        let (network, device) = push_principal(access, input)?;
        let token = field(input, "token")?.to_owned();
        ensure!(valid_token(&token), "invalid Expo push token");
        let sound = input["sound"]
            .as_bool()
            .context("sound must be a boolean")?;
        let badge = input["badge"]
            .as_bool()
            .context("badge must be a boolean")?;
        let values = input["muted_conversations"]
            .as_array()
            .context("muted_conversations must be an array")?;
        ensure!(values.len() <= 256, "too many muted conversations");
        let mut muted = BTreeSet::new();
        for value in values {
            let value = value.as_str().context("invalid muted conversation")?;
            ensure!(valid_business_id(value), "invalid muted conversation");
            muted.insert(value.to_owned());
        }
        crate::logging::add_credential(&token);
        let local = self.identity.device_id()?;
        let expires_at_ms = now_ms() + LEASE_MS;
        self.store
            .call("push.register", move |store| {
                ensure!(
                    store.push_authorized(&network, &device, &local)?,
                    "push device is not authorized"
                );
                let mut state = store.push_state()?;
                // One token can only address one authenticated device on this entry Node.
                for other in &mut state.subscriptions {
                    if other.token.as_deref() == Some(&token)
                        && (other.network != network || other.device != device)
                    {
                        other.token = None;
                        other.retries.clear();
                    }
                }
                let subscription = if let Some(index) = state
                    .subscriptions
                    .iter()
                    .position(|s| s.network == network && s.device == device)
                {
                    &mut state.subscriptions[index]
                } else {
                    let handled = store
                        .pending_notifications(&network, &device)?
                        .into_iter()
                        .map(|n| n.notification_id)
                        .collect();
                    state.subscriptions.push(Subscription {
                        network,
                        device,
                        token: None,
                        generation: String::new(),
                        expires_at_ms,
                        sound,
                        badge,
                        muted: BTreeSet::new(),
                        handled,
                        retries: BTreeMap::new(),
                    });
                    state.subscriptions.last_mut().expect("just inserted")
                };
                let inactive =
                    subscription.token.is_none() || subscription.expires_at_ms <= now_ms();
                for notification in
                    store.pending_notifications(&subscription.network, &subscription.device)?
                {
                    // Freeze the previous muted period even if preferences change before the
                    // worker's next poll. Re-enabling also baselines time spent unregistered.
                    if inactive
                        || subscription.muted.contains(&notification.conversation_id)
                        || muted.contains(&notification.conversation_id)
                    {
                        subscription
                            .handled
                            .insert(notification.notification_id.clone());
                        subscription.retries.remove(&notification.notification_id);
                    }
                }
                if subscription.token.as_deref() != Some(&token) {
                    subscription.generation = uuid::Uuid::new_v4().to_string();
                }
                subscription.token = Some(token);
                subscription.expires_at_ms = expires_at_ms;
                subscription.sound = sound;
                subscription.badge = badge;
                subscription.muted = muted;
                store.keep_push_state(&state)
            })
            .await?;
        self.publish_reread(access.network_id());
        Ok(json!({"registered":true,"lease_expires_at_ms":expires_at_ms}))
    }

    pub(super) async fn unregister_push(&self, access: &Access, input: &Value) -> Result<Value> {
        let (network, device) = push_principal(access, input)?;
        let local = self.identity.device_id()?;
        self.store
            .call("push.unregister", move |store| {
                ensure!(
                    store.push_authorized(&network, &device, &local)?,
                    "push device is not authorized"
                );
                let mut state = store.push_state()?;
                if let Some(subscription) = state
                    .subscriptions
                    .iter_mut()
                    .find(|s| s.network == network && s.device == device)
                {
                    subscription.token = None;
                    subscription.retries.clear();
                }
                store.keep_push_state(&state)
            })
            .await?;
        Ok(json!({"registered":false}))
    }

    pub(super) fn start_push_worker(self: &Arc<Self>) -> Result<()> {
        let transport = ExpoTransport::new()?;
        let weak = Arc::downgrade(self);
        let mut events = self.events.subscribe();
        let mut membership = self.membership.subscribe();
        self.spawn_background(async move {
            loop {
                let Some(node) = weak.upgrade() else { return };
                // Errors and service responses may contain a token; never log their bodies.
                if node.deliver_pushes(&transport, now_ms()).await.is_err() {
                    tracing::debug!(event = "push.pending", "push delivery remains pending");
                }
                drop(node);
                tokio::select! {
                    _ = tokio::time::sleep(Duration::from_secs(15)) => {},
                    event = events.recv() => if matches!(event, Err(broadcast::error::RecvError::Closed)) { return },
                    changed = membership.changed() => if changed.is_err() { return },
                }
            }
        })
    }

    async fn deliver_pushes(&self, transport: &impl PushTransport, now: i64) -> Result<()> {
        // Committed client authority can revoke access while the writer is busy. Fail closed
        // until its matching database projection is available to the per-send checks.
        let (_, authority) = self.ready_client_membership().await?;
        let authority = authority.context("personal network is not installed")?;
        let started = Instant::now();
        let local = self.identity.device_id()?;
        let jobs = self
            .store
            .call("push.jobs", {
                let local = local.clone();
                move |store| {
                    ensure!(
                        &store.public_state()? == authority.as_ref(),
                        "push authority changed"
                    );
                    store.push_jobs(&local, now)
                }
            })
            .await?;
        for job in jobs {
            let send_now = now.saturating_add(started.elapsed().as_millis() as i64);
            let (_, authority) = self.ready_client_membership().await?;
            let payload = self
                .store
                .call("push.payload", {
                    let job = job.clone();
                    let local = local.clone();
                    move |store| {
                        ensure!(
                            authority.as_deref() == Some(&store.public_state()?),
                            "push authority changed"
                        );
                        store.push_payload(&job, &local, send_now)
                    }
                })
                .await?;
            let Some(payload) = payload else { continue };
            let result = transport.send(payload).await.ok();
            let completed_at = now.saturating_add(started.elapsed().as_millis() as i64);
            self.store
                .call("push.result", move |store| {
                    store.push_result(&job, result, completed_at)
                })
                .await?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::command::business_fixture;
    use crate::identity::{GovernanceEvent, Member, Revocation};
    use std::collections::VecDeque;
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    #[derive(Default)]
    struct FixtureTransport {
        results: std::sync::Mutex<VecDeque<Option<Delivery>>>,
        payloads: std::sync::Mutex<Vec<Value>>,
    }
    impl PushTransport for FixtureTransport {
        fn send(&self, payload: Value) -> SendFuture<'_> {
            self.payloads.lock().unwrap().push(payload);
            let result = self
                .results
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or(Some(Delivery::Accepted));
            Box::pin(async move { result.context("controlled temporary failure") })
        }
    }

    fn access(network: &str) -> Access {
        Access::User {
            network_id: network.into(),
            scope: UserScope::Network,
        }
    }

    fn registration(device: &str) -> Value {
        json!({"method":"register_push","client_id":device,"token":"ExpoPushToken[controlled_fixture]","sound":true,"badge":true,"muted_conversations":[]})
    }

    async fn rpc(node: &Arc<Runtime>, access: Access, params: Value) -> Result<Value> {
        node.rpc(access, business_fixture(params)).await
    }

    async fn notification(
        node: &Runtime,
        network: &str,
        id: &str,
        scope: &str,
        kind: &str,
    ) -> Result<()> {
        let (network, id, scope, kind) = (
            network.to_owned(),
            id.to_owned(),
            scope.to_owned(),
            kind.to_owned(),
        );
        node.store.call("push.test.notification", move |store| {
            store.connection.execute(
                "INSERT INTO notifications(network_id,notification_id,conversation_id,kind,text,rank) VALUES(?,?,?,?,?,?)",
                rusqlite::params![network, id, scope, kind, "secret conversation body", id],
            )?;
            Ok(())
        }).await
    }

    async fn finish(node: Arc<Runtime>, dir: PathBuf) -> Result<()> {
        node.shutdown().await;
        drop(node);
        tokio::fs::remove_dir_all(dir).await?;
        Ok(())
    }

    #[tokio::test]
    async fn push_registration_checks_user_network_device_and_token() -> Result<()> {
        let (node, admin, dir) = test_support::node().await?;
        let network = node.network_id().await?;
        let device = admin.device_id()?;
        let params = registration(&device);
        assert!(
            rpc(
                &node,
                Access::Agent(AgentAccess::ReadOnly {
                    network_id: network.clone(),
                    conversation_id: "bot".into()
                }),
                params.clone()
            )
            .await
            .is_err()
        );
        assert!(
            rpc(
                &node,
                Access::User {
                    network_id: network.clone(),
                    scope: UserScope::Conversation("bot".into())
                },
                params.clone()
            )
            .await
            .is_err()
        );
        assert!(
            rpc(&node, access("different-network"), params.clone())
                .await
                .is_err()
        );
        for claimed in [
            LocalIdentity::generate().device_id()?,
            node.identity.device_id()?,
        ] {
            let mut params = params.clone();
            params["client_id"] = json!(claimed);
            assert!(rpc(&node, access(&network), params).await.is_err());
        }
        for bad in [
            "",
            "ExpoPushToken[]",
            "ExpoPushToken[x]garbage",
            "https://evil.invalid",
            "ExpoPushToken[space token]",
            &format!("ExpoPushToken[{}]", "a".repeat(256)),
        ] {
            let mut params = params.clone();
            params["token"] = json!(bad);
            assert!(rpc(&node, access(&network), params).await.is_err());
        }
        let mut params2 = params.clone();
        params2["muted_conversations"] = json!(vec!["bot"; 257]);
        assert!(rpc(&node, access(&network), params2).await.is_err());
        let result = rpc(&node, access(&network), params).await?;
        assert_eq!(result["registered"], true);
        assert!(result.get("token").is_none());
        assert!(result["lease_expires_at_ms"].as_i64().unwrap() > now_ms());
        assert!(valid_token("ExponentPushToken[legacy_fixture-1]"));
        finish(node, dir).await
    }

    #[tokio::test]
    async fn push_baseline_retry_success_preferences_and_restart_are_private_and_durable()
    -> Result<()> {
        let (node, admin, dir) = test_support::node().await?;
        let network = node.network_id().await?;
        let device = admin.device_id()?;
        notification(&node, &network, "historical", "bot", "manager_response").await?;
        let mut params = registration(&device);
        params["sound"] = json!(false);
        params["badge"] = json!(false);
        params["muted_conversations"] = json!(["muted"]);
        rpc(&node, access(&network), params.clone()).await?;
        notification(&node, &network, "new", "bot", "question").await?;
        notification(&node, &network, "quiet", "muted", "manager_response").await?;
        notification(&node, &network, "session", "bot", "session_attributed").await?;
        let transport = FixtureTransport::default();
        transport.results.lock().unwrap().push_back(None);
        let now = now_ms();
        node.deliver_pushes(&transport, now).await?;
        let payload = transport.payloads.lock().unwrap()[0].clone();
        assert_eq!(
            payload["data"]["destination"],
            json!({"network_id":network,"device_id":device,"notification_id":"new","conversation_id":"bot"})
        );
        assert_eq!(payload["channelId"], "vibeface-silent");
        assert_eq!(payload["tag"], "new");
        assert!(payload.get("sound").is_none() && payload.get("badge").is_none());
        assert!(!payload.to_string().contains("secret conversation body"));
        node.deliver_pushes(&transport, now + 1).await?;
        assert_eq!(transport.payloads.lock().unwrap().len(), 1);
        let device2 = device.clone();
        node.store
            .call("push.test.failed_state", move |store| {
                let state = store.push_state()?;
                let subscription = state
                    .subscriptions
                    .iter()
                    .find(|s| s.device == device2)
                    .unwrap();
                assert!(!subscription.handled.contains("new"));
                assert!(subscription.retries.contains_key("new"));
                Ok(())
            })
            .await?;
        node.shutdown().await;
        drop(node);
        let node = Arc::new(Runtime::open(&dir).await?);
        node.deliver_pushes(&transport, now + 60_000).await?;
        assert_eq!(transport.payloads.lock().unwrap().len(), 2);
        // Token and preference changes preserve the historical/accepted/suppressed IDs.
        params["token"] = json!("ExpoPushToken[replacement_fixture]");
        params["sound"] = json!(true);
        params["badge"] = json!(true);
        params["muted_conversations"] = json!([]);
        rpc(&node, access(&network), params).await?;
        node.deliver_pushes(&transport, now + 120_000).await?;
        assert_eq!(transport.payloads.lock().unwrap().len(), 2);
        let query_network = network.clone();
        node.store
            .call("push.test.private_state", move |store| {
                assert_eq!(
                    store.pending_notifications(&query_network, &device)?.len(),
                    4
                );
                assert_eq!(
                    store.connection.query_row(
                        "SELECT count(*) FROM notification_acks",
                        [],
                        |r| r.get::<_, u64>(0)
                    )?,
                    0
                );
                let leaked: u64 = store.connection.query_row(
                    "SELECT count(*) FROM events WHERE envelope LIKE '%ExpoPushToken%'",
                    [],
                    |r| r.get(0),
                )?;
                assert_eq!(leaked, 0);
                Ok(())
            })
            .await?;
        notification(
            &node,
            &network,
            "with-preferences",
            "bot",
            "manager_response",
        )
        .await?;
        node.deliver_pushes(&transport, now + 180_000).await?;
        let payloads = transport.payloads.lock().unwrap();
        assert_eq!(payloads.last().unwrap()["sound"], "default");
        assert_eq!(payloads.last().unwrap()["badge"], 4);
        drop(payloads);
        finish(node, dir).await
    }

    #[tokio::test]
    async fn push_unregister_expiry_device_revocation_and_invalid_token_stop_delivery() -> Result<()>
    {
        let (node, admin, dir) = test_support::node().await?;
        let network = node.network_id().await?;
        let device = admin.device_id()?;
        let transport = FixtureTransport::default();
        rpc(&node, access(&network), registration(&device)).await?;
        notification(&node, &network, "after-register", "bot", "manager_response").await?;
        rpc(
            &node,
            access(&network),
            json!({"method":"unregister_push","client_id":device}),
        )
        .await?;
        node.deliver_pushes(&transport, now_ms()).await?;
        assert!(transport.payloads.lock().unwrap().is_empty());
        rpc(&node, access(&network), registration(&device)).await?;
        node.deliver_pushes(&transport, now_ms()).await?;
        assert!(transport.payloads.lock().unwrap().is_empty());
        notification(&node, &network, "invalid-device", "bot", "question").await?;
        transport
            .results
            .lock()
            .unwrap()
            .push_back(Some(Delivery::DeviceNotRegistered));
        node.deliver_pushes(&transport, now_ms()).await?;
        node.deliver_pushes(&transport, now_ms() + 60_000).await?;
        assert_eq!(transport.payloads.lock().unwrap().len(), 1);
        rpc(&node, access(&network), registration(&device)).await?;
        notification(&node, &network, "expired", "bot", "question").await?;
        node.deliver_pushes(&transport, now_ms() + LEASE_MS + 1)
            .await?;
        assert_eq!(transport.payloads.lock().unwrap().len(), 1);
        rpc(&node, access(&network), registration(&device)).await?;
        notification(&node, &network, "revoked", "bot", "question").await?;
        let mut state = node.membership_state().await?;
        // A second management device keeps the network governable after revoking admin.
        let second = LocalIdentity::generate();
        state.append(GovernanceEvent::grant(
            &admin,
            &state.project()?,
            Member::client(&second, "second", true)?,
        )?)?;
        state.append(GovernanceEvent::revoke(
            &second,
            &state.project()?,
            Revocation::new(&device, None)?,
        )?)?;
        node.install_membership(state).await?;
        node.deliver_pushes(&transport, now_ms()).await?;
        assert_eq!(transport.payloads.lock().unwrap().len(), 1);
        let state = node
            .store
            .call("push.test.revoked", |store| store.push_state())
            .await?;
        assert!(state.subscriptions.iter().all(|s| s.token.is_none()));
        finish(node, dir).await
    }

    #[tokio::test]
    async fn push_session_mute_projection_rebuild_and_prepared_jobs_recheck_authority() -> Result<()>
    {
        let (node, admin, dir) = test_support::node().await?;
        let network = node.network_id().await?;
        let device = admin.device_id()?;
        let local = node.identity.device_id()?;
        test_support::bot(&node, &admin, "bot", "Bot", &[]).await?;
        rpc(&node, access(&network), registration(&device)).await?;
        node.append(
            &network,
            "bot",
            EventBody::SessionAttributed {
                node: local.clone(),
                agent: Provider::Codex,
                session_id: "session".into(),
                cwd: "/controlled".into(),
                summary: "private session".into(),
            },
        )
        .await?;
        node.append(
            &network,
            "bot",
            EventBody::SessionMuted {
                node: local.clone(),
                session_id: "session".into(),
            },
        )
        .await?;
        let question = |question_id: &str, session_id: &str| EventBody::QuestionAsked {
            question_id: format!("q-{question_id}"),
            target: Some(crate::workspace::InputTarget::BotMain {
                bot_id: "bot".into(),
            }),
            attention: AttentionRequest {
                kind: AttentionKind::Question,
                device_id: Some(local.clone()),
            },
            severity: Severity::Attention,
            text: "private question".into(),
            choices: vec![],
            about: Subject {
                node: local.clone(),
                agent: Some(Provider::Codex),
                session_id: session_id.into(),
                cwd: "/controlled".into(),
                summary: "private".into(),
                request_id: None,
            },
        };
        node.append(&network, "bot", question("muted-question", "session"))
            .await?;
        let displayed = rpc(
            &node,
            access(&network),
            json!({"method":"notifications","client_id":device}),
        )
        .await?;
        assert!(
            displayed["notifications"]
                .as_array()
                .unwrap()
                .iter()
                .all(|notification| notification["kind"] != "question")
        );
        let transport = FixtureTransport::default();
        node.deliver_pushes(&transport, now_ms()).await?;
        assert!(transport.payloads.lock().unwrap().is_empty());
        node.append(
            &network,
            "bot",
            question("visible-question", "other-session"),
        )
        .await?;
        node.deliver_pushes(&transport, now_ms()).await?;
        assert_eq!(transport.payloads.lock().unwrap().len(), 1);
        let displayed = rpc(
            &node,
            access(&network),
            json!({"method":"notifications","client_id":device}),
        )
        .await?;
        assert_eq!(
            displayed["notifications"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|notification| notification["kind"] == "question")
                .count(),
            1
        );
        // Revoking an author with an effective event forces a real projection rebuild.
        let mut state = node.membership_state().await?;
        let other = LocalIdentity::generate();
        state.append(GovernanceEvent::grant(
            &admin,
            &state.project()?,
            Member::node(&other, "other")?,
        )?)?;
        node.install_membership(state).await?;
        let other_device = other.device_id()?;
        let query_network = network.clone();
        node.store
            .call("push.test.event_to_rebuild", move |store| {
                store.append_event(
                    &other,
                    &query_network,
                    "bot",
                    EventBody::SessionAttributed {
                        node: other_device,
                        agent: Provider::Codex,
                        session_id: "foreign-session".into(),
                        cwd: "/controlled".into(),
                        summary: "private".into(),
                    },
                    vec![],
                    now_ms(),
                )?;
                Ok(())
            })
            .await?;
        let mut state = node.membership_state().await?;
        let other_device = state
            .project()?
            .members
            .into_iter()
            .find(|m| m.name == "other")
            .unwrap()
            .device_id;
        state.append(GovernanceEvent::revoke(
            &admin,
            &state.project()?,
            Revocation::new(&other_device, None)?,
        )?)?;
        node.install_membership(state).await?;
        node.deliver_pushes(&transport, now_ms()).await?;
        assert_eq!(transport.payloads.lock().unwrap().len(), 1);
        node.append(
            &network,
            "bot",
            question("prepared-question", "other-session"),
        )
        .await?;
        let query_local = local.clone();
        let job = node
            .store
            .call("push.test.prepared", move |store| {
                Ok(store.push_jobs(&query_local, now_ms())?.remove(0))
            })
            .await?;
        rpc(
            &node,
            access(&network),
            json!({"method":"unregister_push","client_id":device}),
        )
        .await?;
        let query_local = local.clone();
        node.store
            .call("push.test.unregistered_before_send", move |store| {
                assert!(store.push_payload(&job, &query_local, now_ms())?.is_none());
                Ok(())
            })
            .await?;
        rpc(&node, access(&network), registration(&device)).await?;
        node.append(
            &network,
            "bot",
            question("revoked-node-question", "other-session"),
        )
        .await?;
        let mut state = node.membership_state().await?;
        state.append(GovernanceEvent::revoke(
            &admin,
            &state.project()?,
            Revocation::new(&local, None)?,
        )?)?;
        node.install_membership(state).await?;
        node.deliver_pushes(&transport, now_ms()).await?;
        assert_eq!(transport.payloads.lock().unwrap().len(), 1);
        finish(node, dir).await
    }

    async fn http_fixture(
        status: &str,
        body: Vec<u8>,
        extra: &str,
    ) -> Result<(String, tokio::task::JoinHandle<()>)> {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        let response = format!(
            "HTTP/1.1 {status}\r\nContent-Length: {}\r\n{extra}Connection: close\r\n\r\n",
            body.len()
        );
        let handle = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = vec![0; 8192];
            let _ = stream.read(&mut request).await.unwrap();
            stream.write_all(response.as_bytes()).await.unwrap();
            let _ = stream.write_all(&body).await;
        });
        Ok((format!("http://{address}/fixture"), handle))
    }

    #[tokio::test]
    async fn push_http_transport_validates_tickets_errors_limits_and_no_redirects() -> Result<()> {
        let transport = ExpoTransport::new()?;
        for (body, expected) in [
            (
                br#"{"data":{"status":"ok","id":"ticket"}}"#.to_vec(),
                Some(Delivery::Accepted),
            ),
            (
                br#"{"data":[{"status":"error","details":{"error":"DeviceNotRegistered"}}]}"#
                    .to_vec(),
                Some(Delivery::DeviceNotRegistered),
            ),
            (br#"{"data":{"status":"ok"}}"#.to_vec(), None),
            (
                br#"{"data":{"status":"error","details":{"error":"MessageRateExceeded"}}}"#
                    .to_vec(),
                None,
            ),
            (
                br#"{"data":[{"status":"ok","id":"a"},{"status":"ok","id":"b"}]}"#.to_vec(),
                None,
            ),
            (b"not-json".to_vec(), None),
            (vec![b'a'; RESPONSE_LIMIT + 1], None),
        ] {
            let (url, handle) = http_fixture("200 OK", body, "").await?;
            assert_eq!(
                transport
                    .send_to(&url, json!({"to":"controlled"}))
                    .await
                    .ok(),
                expected
            );
            handle.await?;
        }
        let (url, handle) = http_fixture("429 Too Many Requests", vec![], "").await?;
        assert!(transport.send_to(&url, json!({})).await.is_err());
        handle.await?;
        let (url, handle) = http_fixture(
            "302 Found",
            vec![],
            "Location: http://127.0.0.1:1/never\r\n",
        )
        .await?;
        let error = transport.send_to(&url, json!({})).await.unwrap_err();
        assert_eq!(error.to_string(), "push service refused request");
        handle.await?;
        Ok(())
    }
}
