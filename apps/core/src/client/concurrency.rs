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

    pub(super) async fn accept(
        &mut self,
        node: &Arc<Runtime>,
        session: &Session,
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
            self.start(node, session, request, lane).await?;
            return Ok(None);
        }
        Ok(Some(Response {
            id: request.id,
            result: Value::Null,
            error: Some("too many client requests; this request was not started".into()),
            code: Some("client_overloaded"),
        }))
    }

    pub(super) async fn start_ordered(
        &mut self,
        node: &Arc<Runtime>,
        session: &Session,
    ) -> Result<()> {
        if self.count(Lane::Ordered) == 0
            && let Some(request) = self.waiting.pop_front()
        {
            self.start(node, session, request, Lane::Ordered).await?;
        }
        Ok(())
    }

    async fn start(
        &mut self,
        node: &Arc<Runtime>,
        session: &Session,
        request: Request,
        lane: Lane,
    ) -> Result<()> {
        // Guest and signed governance methods deliberately remain usable after authority
        // changes, including self-revoke/recovery replies. All independent work and the two
        // connection-state operations are fenced by the principal that started them.
        let guest_method = GUEST_METHODS.contains(&request.method.as_str());
        let fenced = lane != Lane::Ordered
            || (!guest_method && !SIGNED_METHODS.contains(&request.method.as_str()));
        let (authority, denied) = if fenced {
            match principal(node, session).await {
                Ok(authority) => (Some(authority), None),
                Err(_) if guest_method => (None, None),
                Err(error) => (None, Some(error)),
            }
        } else {
            (None, None)
        };
        self.active
            .insert(request.id.clone(), Active { lane, authority });
        let snapshot = Snapshot::from(session);
        let node = node.clone();
        self.tasks.spawn(async move {
            let mut request_session = snapshot.session();
            let observing = request.method == "rpc" && request.params["method"] == "conversation";
            let result = match denied {
                Some(error) => Err(error),
                None => dispatch(
                    &node,
                    &mut request_session,
                    &request.method,
                    request.params,
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
            }
        });
        Ok(())
    }

    pub(super) async fn next(&mut self) -> Result<Completed> {
        self.tasks
            .join_next()
            .await
            .context("client request task missing")?
            .context("client request task failed; outcome may be unknown")
    }

    pub(super) async fn check_authority(
        &self,
        node: &Runtime,
        session: &Session,
    ) -> Result<()> {
        if self.active.values().any(|call| call.authority.is_some()) {
            let now = principal(node, session).await?;
            ensure!(
                self.active
                    .values()
                    .all(|call| call.authority.as_ref().is_none_or(|was| was == &now)),
                "this device's permission changed during a client request"
            );
        }
        Ok(())
    }

    pub(super) async fn finish(
        &mut self,
        node: &Runtime,
        session: &mut Session,
        completed: Completed,
    ) -> Result<Response> {
        // Check even if membership/local-access notifications have not won select yet. A
        // completed task must not leak its old result after a revocation raced its completion.
        let active = self.active.remove(&completed.id).context("unknown client result")?;
        if let Some(was) = active.authority {
            ensure!(
                principal(node, session).await? == was,
                "this device's permission changed during a client request"
            );
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
            Err(error) => Response {
                id: completed.id,
                result: Value::Null,
                error: Some(crate::logging::scrub(&error.to_string())),
                code: error
                    .downcast_ref::<crate::discovery::CodexMissing>()
                    .map(|_| crate::discovery::CodexMissing::CODE),
            },
        })
    }
}

#[cfg(all(test, unix))]
mod tests;
