//! Connection-owned work. Only the transport task owns Noise and the observation lease.
use super::*;
use tokio::task::JoinSet;

const LONG_CALLS: usize = 4;
const READ_CALLS: usize = 8;
const CONTROL_CALLS: usize = 2;
const ORDERED_QUEUE: usize = 16;
// IDs cannot be reused, even after completion or rejection. Bound this history by requiring a
// new connection rather than evicting IDs and accidentally executing a retried write again.
const REQUEST_IDS: usize = 4096;

struct LocalAuthority {
    generation: u64,
    session: crate::local_access::LocalSession,
}

/// Display queries and transport delivery use committed authority. Mutations and controls
/// still authenticate against the writer; a WAL reader or this cache never grants a write.
pub(super) struct Authority {
    local: Option<LocalAuthority>,
    local_changes: tokio::sync::watch::Receiver<u64>,
}

impl Authority {
    pub(super) fn new(
        node: &Runtime,
        session: &Session,
        local_changes: tokio::sync::watch::Receiver<u64>,
    ) -> Result<Self> {
        // Before the transport pump starts, validate a cookie once under LocalAccess's lock.
        // Afterwards use its immutable expiry and revocation generation without that lock.
        let (_, state) = node.client_membership();
        let local = if let (Some(cookie), Some(state)) = (&session.cookie, state) {
            let view = state.project()?;
            let generation = *local_changes.borrow();
            match node.local_access.session(cookie, &node.identity.device_id()?, &view.network_id) {
                Ok(local) if local.device_id == session.device_id && *local_changes.borrow() == generation => {
                    Some(LocalAuthority { generation, session: local })
                }
                Ok(_) if view.client(&session.device_id).is_ok() => None,
                Ok(_) => bail!("local browser cookie is bound to another device or was revoked"),
                Err(error) if view.client(&session.device_id).is_err() => return Err(error),
                Err(_) => None,
            }
        } else {
            None
        };
        Ok(Self { local, local_changes })
    }

    pub(super) fn principal(&self, node: &Runtime, session: &Session) -> Result<Principal> {
        let (generation, state) = node.client_membership();
        let view = state.context("personal network is not initialized")?.project()?;
        let node_id = node.identity.device_id()?;
        let principal = if let Some(member) = member_principal(&view, &node_id, session)? {
            member
        } else {
            view.node(&node_id)?;
            let local = self.local.as_ref().context("this device is not a member of the personal network")?;
            ensure!(
                *self.local_changes.borrow() == local.generation
                    && local.session.expires_at_ms() > crate::runtime::now_ms()
                    && local.session.device_id == session.device_id
                    && local.session.network_id == view.network_id,
                "local browser access was revoked"
            );
            Principal {
                source: "local_session",
                network_id: local.session.network_id.clone(),
                device_id: local.session.device_id.clone(),
                epoch: view.epoch,
                can_manage: false,
            }
        };
        ensure!(node.client_membership().0 == generation, "membership changed during client authorization");
        if principal.source == "local_session" {
            ensure!(self.local.as_ref().is_some_and(|local| *self.local_changes.borrow() == local.generation), "local browser access was revoked");
        }
        Ok(principal)
    }

    pub(super) fn check(&self, node: &Runtime, session: &Session, was: &Principal) -> Result<()> {
        ensure!(self.principal(node, session)? == *was, "this device's permission changed during a client request");
        Ok(())
    }
}

pub(super) struct PreparedPush {
    authority: Principal,
    payload: Value,
}

#[derive(Default)]
pub(super) struct Pushes {
    tasks: JoinSet<Result<PreparedPush>>,
}

impl Pushes {
    pub(super) fn is_empty(&self) -> bool {
        self.tasks.is_empty()
    }

    pub(super) fn start(
        &mut self,
        node: &Arc<Runtime>,
        session: &Session,
        expected: Principal,
        payload: Value,
    ) {
        debug_assert!(self.is_empty());
        let snapshot = Snapshot::from(session);
        let node = node.clone();
        self.tasks.spawn(async move {
            let session = snapshot.session();
            let authority = principal(&node, &session).await?;
            ensure!(authority == expected, "this device's permission changed before a push");
            let payload = pushed(&node, &authority.network_id, &session.device_id, payload).await;
            Ok(PreparedPush { authority, payload })
        });
    }

    pub(super) async fn next(&mut self) -> Result<PreparedPush> {
        self.tasks.join_next().await.context("client push task missing")?
            .context("client push task failed")?
    }

    pub(super) fn finish(
        &self,
        node: &Runtime,
        session: &Session,
        authority: &Authority,
        prepared: PreparedPush,
    ) -> Result<Value> {
        authority.check(node, session, &prepared.authority)?;
        Ok(json!({"type":"changed","payload":prepared.payload}))
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Lane {
    Long,
    Read,
    Control,
    Ordered,
}

impl Lane {
    fn for_request(request: &Request) -> Self {
        match request.method.as_str() {
            "provider_probe" | "provider_models" | "test_codex_app_server" | "test_webhook"
            | "logs" | "diagnostics" => Self::Long,
            "info" | "directory" | "write_state" | "node_settings" | "triage_settings"
            | "relay_settings" | "share_link" | "entrances" => Self::Read,
            "rpc" => match request.params["method"].as_str() {
                Some("interrupt_work" | "cancel_manager") => Self::Control,
                Some(
                    "nodes" | "conversations" | "open_works" | "work_detail"
                    | "workspace_overview" | "workspace_state" | "workspace_history" | "workspace_command_receipt" | "signed_receipt"
                    | "input_receipt" | "scope_context" | "read_artifact" | "bots"
                    | "questions" | "memory_state" | "notifications",
                ) => Self::Read,
                // Conversation reads replace the observation lease. Unrecognized operations
                // and writes retain arrival order; they are never retried by this scheduler.
                _ => Self::Ordered,
            },
            _ => Self::Ordered,
        }
    }

    fn capacity(self) -> usize {
        match self {
            Self::Long => LONG_CALLS,
            Self::Read => READ_CALLS,
            Self::Control => CONTROL_CALLS,
            Self::Ordered => 1,
        }
    }
}

/// A request gets credentials, never the live subscription or observation guard. Dispatch
/// still authenticates at its boundary, and the transport checks the captured authority again
/// before releasing a result or installing a newly acquired observation.
#[derive(Clone)]
struct Snapshot {
    device_id: String,
    cookie: Option<String>,
    operator: bool,
}

impl From<&Session> for Snapshot {
    fn from(session: &Session) -> Self {
        Self {
            device_id: session.device_id.clone(),
            cookie: session.cookie.clone(),
            operator: session.operator,
        }
    }
}

impl Snapshot {
    fn session(self) -> Session {
        Session {
            device_id: self.device_id,
            cookie: self.cookie,
            operator: self.operator,
            subscribed: None,
            observation: None,
        }
    }
}

enum Effect {
    Subscribe(Option<bool>),
    Observe(Option<crate::runtime::ObservationGuard>),
}

pub(super) struct Completed {
    id: String,
    result: Result<Value>,
    effect: Option<Effect>,
    authority: Option<Principal>,
    method: String,
}

struct Active {
    lane: Lane,
    authority: Option<Principal>,
}

#[derive(Default)]
pub(super) struct Calls {
    tasks: JoinSet<Completed>,
    active: BTreeMap<String, Active>,
    waiting: VecDeque<Request>,
    seen: BTreeSet<String>,
}

impl Calls {
    pub(super) fn is_empty(&self) -> bool {
        self.tasks.is_empty()
    }

    fn count(&self, lane: Lane) -> usize {
        self.active.values().filter(|call| call.lane == lane).count()
    }

    pub(super) fn accept(
        &mut self,
        node: &Arc<Runtime>,
        session: &Session,
        authority: &Authority,
        request: Request,
    ) -> Result<Option<Response>> {
        ensure!(
            !self.seen.contains(&request.id),
            "duplicate request ID; the original outcome may be unknown"
        );
        ensure!(
            self.seen.len() < REQUEST_IDS,
            "client request ID capacity reached; reconnect before sending more requests"
        );
        self.seen.insert(request.id.clone());
        let lane = Lane::for_request(&request);
        if lane == Lane::Ordered {
            if self.waiting.len() < ORDERED_QUEUE {
                self.waiting.push_back(request);
                return Ok(None);
            }
        } else if self.count(lane) < lane.capacity() {
            self.start(node, session, authority, request, lane);
            return Ok(None);
        }
        Ok(Some(Response {
            id: request.id,
            result: Value::Null,
            error: Some("too many client requests; this request was not started".into()),
            code: Some("client_overloaded"),
        }))
    }

    pub(super) fn start_ordered(
        &mut self,
        node: &Arc<Runtime>,
        session: &Session,
        authority: &Authority,
    ) {
        if self.count(Lane::Ordered) == 0
            && let Some(request) = self.waiting.pop_front()
        {
            self.start(node, session, authority, request, Lane::Ordered);
        }
    }

    fn start(
        &mut self,
        node: &Arc<Runtime>,
        session: &Session,
        authority: &Authority,
        request: Request,
        lane: Lane,
    ) {
        // Guest and signed governance methods deliberately remain usable after authority
        // changes, including self-revoke/recovery replies. All independent work and the two
        // connection-state operations are fenced by the principal that started them.
        let guest_method = GUEST_METHODS.contains(&request.method.as_str());
        let fenced = lane != Lane::Ordered
            || (!guest_method && !SIGNED_METHODS.contains(&request.method.as_str()));
        let display = snapshot_request(&request.method, &request.params);
        let display_authority = display.then(|| authority.principal(node, session));
        self.active
            .insert(request.id.clone(), Active { lane, authority: fenced.then(|| authority.principal(node, session).ok()).flatten() });
        let snapshot = Snapshot::from(session);
        let node = node.clone();
        self.tasks.spawn(async move {
            let mut request_session = snapshot.session();
            let (authority, denied) = if fenced {
                let authenticated = match display_authority {
                    Some(authority) => authority,
                    None => principal(&node, &request_session).await,
                };
                match authenticated {
                    Ok(authority) => (Some(authority), None),
                    Err(_) if guest_method => (None, None),
                    Err(error) => (None, Some(error)),
                }
            } else {
                (None, None)
            };
            let observing = request.method == "rpc" && request.params["method"] == "conversation";
            let result = match denied {
                Some(error) => Err(error),
                None => dispatch_inner(
                    &node,
                    &mut request_session,
                    &request.method,
                    request.params,
                    if display { authority.clone() } else { None },
                )
                .await,
            };
            let effect = match request.method.as_str() {
                "subscribe" => Some(Effect::Subscribe(request_session.subscribed)),
                "rpc" if observing && result.is_ok() => {
                    Some(Effect::Observe(request_session.observation.take()))
                }
                _ => None,
            };
            Completed {
                id: request.id,
                result,
                effect,
                authority,
                method: request.method,
            }
        });
    }

    pub(super) async fn next(&mut self) -> Result<Completed> {
        self.tasks
            .join_next()
            .await
            .context("client request task missing")?
            .context("client request task failed; outcome may be unknown")
    }

    pub(super) fn check_authority(
        &self,
        node: &Runtime,
        session: &Session,
        authority: &Authority,
    ) -> Result<()> {
        if self.active.values().any(|call| call.authority.is_some()) {
            let now = authority.principal(node, session)?;
            ensure!(
                self.active
                    .values()
                    .all(|call| call.authority.as_ref().is_none_or(|was| was == &now)),
                "this device's permission changed during a client request"
            );
        }
        Ok(())
    }

    pub(super) fn finish(
        &mut self,
        node: &Runtime,
        session: &mut Session,
        authority: &Authority,
        completed: Completed,
    ) -> Result<Response> {
        // Check even if membership/local-access notifications have not won select yet. A
        // completed task must not leak its old result after a revocation raced its completion.
        let active = self.active.remove(&completed.id).context("unknown client result")?;
        if let Some(was) = active.authority {
            ensure!(
                authority.principal(node, session)? == was,
                "this device's permission changed during a client request"
            );
        }
        if let Some(was) = completed.authority.as_ref() {
            authority.check(node, session, was)?;
        }
        if let Ok(result) = &completed.result {
            if completed.method == "info" && result["network_id"].is_string() {
                ensure!(result["network_id"] == authority.principal(node, session)?.network_id,
                    "this device's permission changed before client information was delivered");
            }
            if matches!(completed.method.as_str(), "authenticate" | "recover" | "revoke" | "grant" | "veto")
                && result["principal"].is_object()
            {
                ensure!(result["principal"] == serde_json::to_value(authority.principal(node, session)?)?,
                    "this device's permission changed before authentication was delivered");
            }
        }
        match completed.effect {
            Some(Effect::Subscribe(subscribed)) => session.subscribed = subscribed,
            Some(Effect::Observe(observation)) => session.observation = observation,
            None => {}
        }
        Ok(match completed.result {
            Ok(result) => Response {
                id: completed.id,
                result,
                error: None,
                code: None,
            },
            Err(error) => {
                if matches!(error.downcast_ref::<crate::store::actor::StoreCallError>(),
                    Some(crate::store::actor::StoreCallError::OutcomeUnknown))
                {
                    // An accepted writer operation may have committed before its reply was
                    // lost. Closing keeps the client outcome unknown instead of refusing it.
                    return Err(error.context("client request outcome may be unknown; reconcile before retrying"));
                }
                Response {
                id: completed.id,
                result: Value::Null,
                error: Some(crate::logging::scrub(&error.to_string())),
                code: error
                    .downcast_ref::<crate::discovery::CodexMissing>()
                    .map(|_| crate::discovery::CodexMissing::CODE),
                }
            }
        })
    }
}

#[cfg(all(test, unix))]
mod tests;
