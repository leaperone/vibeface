//! Noise proves device possession. Every business request carries an independent Invocation.

mod gateway_forward;

pub(crate) use gateway_forward::{GatewayForwardClient, GatewayForwardReply, GatewaySourceClient};

use crate::{
    crypto::LocalIdentity,
    identity::{
        node_execution_policy::{NodeReadRequest, NodeReadResponse},
        task_business_policy::{
            TaskBusinessPolicyImportContext, TaskBusinessPolicySession, TaskBusinessRequest,
            TaskBusinessResponse,
        },
    },
    network::{NetworkHub, NetworkSocket, Service},
    runtime::{Access, GatewaySourceRequest, Runtime},
};
use anyhow::{Context, Result, bail, ensure};
use axum::extract::ws::Message;
use futures_util::{SinkExt, StreamExt};
use serde::{
    Deserialize, Deserializer, Serialize,
    de::{self, MapAccess, SeqAccess, Visitor},
};
use serde_json::{Value, json};
use std::{
    collections::HashSet,
    sync::Arc,
    time::{Duration, Instant},
};
use vibeface_secure::authorization::{
    Actor, CheckStage, DeviceId, FreshnessReceipt, Invocation, MAX_PROOF_BYTES, ObjectHash,
    PublicKey, Signed, TransportEvidence, VerifiedClientAuthorization,
    execution_observation::ExecutionObservationRequest,
    node_execution_policy::{ActualPolicyNoisePeer, VerifiedTaskExecutionTrust},
    task_business_policy::{
        TaskBusinessPolicyExport, TaskBusinessPolicyHeader, TaskBusinessWire,
        VerifiedTaskBusinessOriginalSource,
        receipt::{TaskBusinessChallenge, TaskBusinessReceiptRead},
    },
};
use vibeface_secure::{Channel, Handshake};

const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
const CALL_TIMEOUT: Duration = Duration::from_secs(120);
const IDLE_TIMEOUT: Duration = Duration::from_secs(90);
const HEARTBEAT: Duration = Duration::from_secs(20);
const NODE_POLICY_READ_METHOD: &str = "node_policy_read";
const TASK_BUSINESS_METHOD: &str = "task_business";
const TASK_RECEIPT_READ_METHOD: &str = "task_business_receipt_read";
const GATEWAY_SOURCE_METHOD: &str = "gateway_source";
const GATEWAY_FORWARD_METHOD: &str = "gateway_forward";

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    id: String,
    method: String,
    #[serde(default)]
    params: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    invocation: Option<Signed<Invocation>>,
}
#[derive(Serialize)]
struct Response {
    id: String,
    result: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}
struct Session {
    public_key: String,
    noise_peer: ActualPolicyNoisePeer,
    purpose: SessionPurpose,
    authorization: Option<VerifiedClientAuthorization>,
    subscription: Option<Access>,
    remote_subscription: Option<RemoteSubscription>,
}
enum SessionPurpose {
    Client,
    NodePolicyRead,
    TaskReceiptRead,
    GatewaySource,
    GatewayForward,
    TaskBusiness(TaskBusinessPhase),
}
enum TaskBusinessPhase {
    AwaitingBegin,
    Importing(TaskBusinessPolicyImportContext),
    Ready(TaskBusinessPolicySession),
    Pending {
        session: TaskBusinessPolicySession,
        original: TaskBusinessPending,
    },
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum TaskBusinessKind {
    Business,
    Status,
}

struct TaskBusinessPending {
    wire: TaskBusinessWire,
    challenge: TaskBusinessChallenge,
    kind: TaskBusinessKind,
}

fn task_business_response_pending(
    header_hash: &ObjectHash,
    wire: TaskBusinessWire,
    kind: TaskBusinessKind,
    response: &TaskBusinessResponse,
) -> Result<Option<TaskBusinessPending>> {
    match response {
        TaskBusinessResponse::Challenge {
            source_context,
            business_wire_hash,
            challenge,
        } => {
            ensure!(
                source_context == header_hash && *business_wire_hash == wire.hash()?,
                "task business challenge changed its original context or wire"
            );
            let bound = match (&wire, challenge) {
                (TaskBusinessWire::Submit(wire), TaskBusinessChallenge::Submit(challenge)) => {
                    challenge.handoff_hash == wire.hash()?
                        && match kind {
                            TaskBusinessKind::Business => matches!(
                                challenge.stage,
                                CheckStage::Admit | CheckStage::RemoteRead
                            ),
                            TaskBusinessKind::Status => challenge.stage == CheckStage::RemoteRead,
                        }
                }
                (TaskBusinessWire::Observe(wire), TaskBusinessChallenge::Observe(challenge)) => {
                    challenge.observation_hash == wire.hash()?
                        && challenge.stage == CheckStage::RemoteRead
                }
                (TaskBusinessWire::Resolve(wire), TaskBusinessChallenge::Resolve(challenge)) => {
                    challenge.resolution_hash == wire.hash()?
                }
                _ => false,
            };
            ensure!(
                bound,
                "task business challenge changed its typed anchor or stage"
            );
            Ok(Some(TaskBusinessPending {
                wire,
                challenge: challenge.clone(),
                kind,
            }))
        }
        TaskBusinessResponse::Business(_) if kind == TaskBusinessKind::Business => Ok(None),
        TaskBusinessResponse::Status(_) => Ok(None),
        _ => bail!("task business response changed its request stage"),
    }
}

impl TaskBusinessPhase {
    fn apply_response(
        &mut self,
        wire: TaskBusinessWire,
        kind: TaskBusinessKind,
        response: &TaskBusinessResponse,
    ) -> Result<()> {
        let (session, previous_challenge) = match self {
            Self::Ready(session) => (session, None),
            Self::Pending { session, original } => (session, Some(original.challenge.hash()?)),
            _ => bail!("task business response has no completed source"),
        };
        let pending = task_business_response_pending(
            &session.source().header().hash()?,
            wire,
            kind,
            response,
        )?;
        if let (Some(previous), Some(next)) = (previous_challenge, pending.as_ref()) {
            ensure!(
                previous == next.challenge.hash()?,
                "pending task business challenge changed"
            );
        }
        let session = match std::mem::replace(self, Self::AwaitingBegin) {
            Self::Ready(session) | Self::Pending { session, .. } => session,
            _ => unreachable!(),
        };
        *self = match pending {
            Some(original) => Self::Pending { session, original },
            None => Self::Ready(session),
        };
        Ok(())
    }
}
impl SessionPurpose {
    fn select_method(&mut self, method: &str, authenticated: bool) -> Result<()> {
        match self {
            Self::Client
                if matches!(
                    method,
                    NODE_POLICY_READ_METHOD
                        | TASK_BUSINESS_METHOD
                        | TASK_RECEIPT_READ_METHOD
                        | GATEWAY_SOURCE_METHOD
                        | GATEWAY_FORWARD_METHOD
                ) =>
            {
                ensure!(
                    !authenticated,
                    "policy transports require an independent unauthenticated session"
                );
                *self = if method == NODE_POLICY_READ_METHOD {
                    Self::NodePolicyRead
                } else if method == TASK_BUSINESS_METHOD {
                    Self::TaskBusiness(TaskBusinessPhase::AwaitingBegin)
                } else if method == GATEWAY_SOURCE_METHOD {
                    Self::GatewaySource
                } else if method == GATEWAY_FORWARD_METHOD {
                    Self::GatewayForward
                } else {
                    Self::TaskReceiptRead
                };
            }
            Self::NodePolicyRead => ensure!(
                method == NODE_POLICY_READ_METHOD,
                "node policy reader sessions cannot change purpose"
            ),
            Self::TaskBusiness(_) => ensure!(
                method == TASK_BUSINESS_METHOD,
                "task business sessions cannot change purpose"
            ),
            Self::TaskReceiptRead => ensure!(
                method == TASK_RECEIPT_READ_METHOD,
                "task receipt reader sessions cannot change purpose"
            ),
            Self::GatewaySource => ensure!(
                method == GATEWAY_SOURCE_METHOD,
                "gateway source sessions cannot change purpose"
            ),
            Self::GatewayForward => ensure!(
                method == GATEWAY_FORWARD_METHOD,
                "gateway forward sessions cannot change purpose"
            ),
            Self::Client => {}
        }
        Ok(())
    }
}
impl Session {
    fn select_method(&mut self, method: &str) -> Result<()> {
        self.purpose.select_method(
            method,
            self.authorization.is_some()
                || self.subscription.is_some()
                || self.remote_subscription.is_some(),
        )
    }

    async fn task_business(
        &mut self,
        node: &Arc<Runtime>,
        request: TaskBusinessRequest,
    ) -> Result<Value> {
        let SessionPurpose::TaskBusiness(phase) = &mut self.purpose else {
            bail!("task business session purpose is missing");
        };
        let now = crate::runtime::now_ms() as u64;
        match request {
            TaskBusinessRequest::SourceBegin { header } => {
                ensure!(
                    matches!(phase, TaskBusinessPhase::AwaitingBegin),
                    "task business source has already begun"
                );
                let (context, response) = node.store.lock().await.begin_task_business_import(
                    &node.identity,
                    &self.noise_peer,
                    &header,
                    now,
                )?;
                *phase = TaskBusinessPhase::Importing(context);
                Ok(response)
            }
            TaskBusinessRequest::SourcePage { page } => {
                let TaskBusinessPhase::Importing(context) = phase else {
                    bail!("task business page requires an unfinished source import");
                };
                node.store
                    .lock()
                    .await
                    .append_task_business_policy(context, &page, now)
            }
            TaskBusinessRequest::SourceFinish { witness } => {
                let TaskBusinessPhase::Importing(context) = phase else {
                    bail!("task business finish requires an unfinished source import");
                };
                let (ready, response) = node
                    .store
                    .lock()
                    .await
                    .finish_task_business_policy(context, &witness, now)?;
                *phase = TaskBusinessPhase::Ready(ready);
                Ok(response)
            }
            TaskBusinessRequest::Business(wire) => {
                let TaskBusinessPhase::Ready(ready) = phase else {
                    bail!("finish the exact task business source before executing business");
                };
                let response = node
                    .task_business(ready, &self.noise_peer, wire.clone())
                    .await?;
                phase.apply_response(wire, TaskBusinessKind::Business, &response)?;
                Ok(serde_json::to_value(response)?)
            }
            TaskBusinessRequest::Status { wire } => {
                let TaskBusinessPhase::Ready(ready) = phase else {
                    bail!(
                        "task business status requires a completed source and no pending challenge"
                    );
                };
                let response = node
                    .task_business_status(ready, &self.noise_peer, wire.clone())
                    .await?;
                phase.apply_response(wire, TaskBusinessKind::Status, &response)?;
                Ok(serde_json::to_value(response)?)
            }
            TaskBusinessRequest::Receipts {
                source_context,
                business_wire_hash,
                challenge_hash,
                receipts,
            } => {
                let TaskBusinessPhase::Pending { session, original } = phase else {
                    bail!("task business receipts require an exact pending challenge");
                };
                ensure!(
                    source_context == session.source().header().hash()?
                        && business_wire_hash == original.wire.hash()?
                        && challenge_hash == original.challenge.hash()?
                        && receipts
                            .iter()
                            .all(|receipt| receipt.payload.challenge_hash == challenge_hash),
                    "task business receipts changed the pending context, wire or challenge"
                );
                let wire = original.wire.clone();
                let kind = original.kind;
                let response = node
                    .task_business_receipts(
                        session,
                        &self.noise_peer,
                        wire.clone(),
                        original.challenge.clone(),
                        receipts,
                    )
                    .await?;
                phase.apply_response(wire, kind, &response)?;
                Ok(serde_json::to_value(response)?)
            }
        }
    }
}
struct RemoteSubscription {
    body: Value,
    invocation: Signed<Invocation>,
    cursor: String,
}

pub struct Client {
    socket: NetworkSocket,
    channel: Channel,
}

pub(crate) struct NodePolicyReadClient {
    client: Client,
    noise_peer: ActualPolicyNoisePeer,
}

pub(crate) struct TaskBusinessClient {
    client: Client,
    header_hash: ObjectHash,
    wire_hash: ObjectHash,
    phase: TaskBusinessClientPhase,
}

enum TaskBusinessClientPhase {
    AwaitingBegin,
    Importing,
    Ready,
    Pending(TaskBusinessPending),
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TaskBusinessReply {
    id: String,
    result: Option<TaskBusinessResponse>,
    error: Option<String>,
}

pub(crate) struct TaskBusinessReceiptClient {
    client: Client,
    header_hash: ObjectHash,
    wire_hash: ObjectHash,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TaskBusinessReceiptReply {
    id: String,
    result: Option<Vec<Signed<FreshnessReceipt>>>,
    error: Option<String>,
}

impl TaskBusinessReceiptClient {
    pub(crate) async fn connect(
        network: &NetworkHub,
        identity: &LocalIdentity,
        source: &TaskBusinessPolicySession,
    ) -> Result<Self> {
        Self::connect_header(network, identity, source.source().header()).await
    }

    pub(crate) async fn connect_original(
        network: &NetworkHub,
        identity: &LocalIdentity,
        original: &VerifiedTaskBusinessOriginalSource,
    ) -> Result<Self> {
        Self::connect_header(network, identity, original.header()).await
    }

    async fn connect_header(
        network: &NetworkHub,
        identity: &LocalIdentity,
        header: &Signed<TaskBusinessPolicyHeader>,
    ) -> Result<Self> {
        ensure!(
            identity.public_key_hex()? == header.payload.scope.target_public_key.as_str(),
            "task receipt reader is not the original execution Node"
        );
        let target_node_id = crate::crypto::fingerprint(header.issuer.as_str())?;
        let socket = network.dial(&target_node_id, Service::Client).await?;
        let (client, peer) =
            Client::from_socket_with_policy_peer(socket, identity, &target_node_id).await?;
        ensure!(
            peer.public_key() == &header.issuer,
            "task receipt Noise source changed"
        );
        Ok(Self {
            client,
            header_hash: header.hash()?,
            wire_hash: header.payload.scope.business_wire_hash.clone(),
        })
    }

    pub(crate) async fn read(
        &mut self,
        request: &Signed<TaskBusinessReceiptRead>,
    ) -> Result<Vec<Signed<FreshnessReceipt>>> {
        ensure!(
            request.payload.source_header_hash == self.header_hash
                && request.payload.business_wire_hash == self.wire_hash,
            "task receipt read changed the original source context or wire"
        );
        let challenge_hash = request.payload.challenge.hash()?;
        let id = uuid::Uuid::new_v4().to_string();
        let wire = serde_json::to_vec(&Request {
            id: id.clone(),
            method: TASK_RECEIPT_READ_METHOD.to_owned(),
            params: serde_json::to_value(request)?,
            invocation: None,
        })?;
        require_node_policy_carrier(wire.len(), 0)?;
        tokio::time::timeout(CALL_TIMEOUT, async {
            for record in secure(self.client.channel.seal_message(&wire))? {
                send_frame(&mut self.client.socket, record).await?;
            }
            let bytes = receive_value(&mut self.client.socket, &mut self.client.channel).await?;
            require_node_policy_carrier(wire.len(), bytes.len())?;
            serde_json::from_slice::<UniqueJsonKeys>(&bytes)?;
            let reply: TaskBusinessReceiptReply = serde_json::from_slice(&bytes)?;
            ensure!(
                reply.id == id,
                "task receipt response changed its request ID"
            );
            let receipts = match (reply.result, reply.error) {
                (Some(receipts), None) => receipts,
                (None, Some(error)) => bail!("{error}"),
                _ => bail!("invalid task receipt response"),
            };
            ensure!(
                receipts
                    .iter()
                    .all(|receipt| receipt.payload.challenge_hash == challenge_hash),
                "task receipts changed their original typed challenge hash"
            );
            Ok(receipts)
        })
        .await
        .context("task receipt read timed out")?
    }
}

impl TaskBusinessClient {
    pub(crate) async fn connect(
        network: &NetworkHub,
        identity: &LocalIdentity,
        source: &TaskBusinessPolicyExport,
    ) -> Result<Self> {
        let target = source.target_public_key();
        let target_node_id = crate::crypto::fingerprint(target.as_str())?;
        let socket = network.dial(&target_node_id, Service::Client).await?;
        let (client, peer) =
            Client::from_socket_with_policy_peer(socket, identity, &target_node_id).await?;
        ensure!(
            peer.public_key() == target,
            "task business Noise target changed"
        );
        Ok(Self {
            client,
            header_hash: source.header().hash()?,
            wire_hash: source.scope().business_wire_hash.clone(),
            phase: TaskBusinessClientPhase::AwaitingBegin,
        })
    }

    pub(crate) async fn exchange(
        &mut self,
        request: &TaskBusinessRequest,
    ) -> Result<TaskBusinessResponse> {
        match (&self.phase, request) {
            (
                TaskBusinessClientPhase::AwaitingBegin,
                TaskBusinessRequest::SourceBegin { header },
            ) => {
                ensure!(
                    header.hash()? == self.header_hash,
                    "task business header changed"
                );
            }
            (TaskBusinessClientPhase::Importing, TaskBusinessRequest::SourcePage { page }) => {
                ensure!(
                    page.payload.header_hash == self.header_hash,
                    "task business page changed context"
                );
            }
            (TaskBusinessClientPhase::Importing, TaskBusinessRequest::SourceFinish { witness }) => {
                ensure!(
                    witness.payload.header_hash == self.header_hash,
                    "task business witness changed context"
                );
            }
            (TaskBusinessClientPhase::Ready, TaskBusinessRequest::Business(wire)) => {
                ensure!(wire.hash()? == self.wire_hash, "task business wire changed");
            }
            (TaskBusinessClientPhase::Ready, TaskBusinessRequest::Status { wire }) => {
                ensure!(
                    wire.hash()? == self.wire_hash,
                    "task business status changed its wire"
                );
            }
            (
                TaskBusinessClientPhase::Pending(original),
                TaskBusinessRequest::Receipts {
                    source_context,
                    business_wire_hash,
                    challenge_hash,
                    receipts,
                },
            ) => {
                ensure!(
                    *source_context == self.header_hash
                        && *business_wire_hash == self.wire_hash
                        && *challenge_hash == original.challenge.hash()?
                        && receipts
                            .iter()
                            .all(|receipt| receipt.payload.challenge_hash == *challenge_hash),
                    "task business receipts changed the pending context, wire or challenge"
                );
            }
            _ => bail!("task business request is out of order"),
        }
        let policy = !matches!(
            request,
            TaskBusinessRequest::Business(_) | TaskBusinessRequest::Status { .. }
        );
        let id = uuid::Uuid::new_v4().to_string();
        let wire = serde_json::to_vec(&Request {
            id: id.clone(),
            method: TASK_BUSINESS_METHOD.to_owned(),
            params: serde_json::to_value(request)?,
            invocation: None,
        })?;
        if policy {
            require_node_policy_carrier(wire.len(), 0)?;
        }
        let response = tokio::time::timeout(CALL_TIMEOUT, async {
            for record in secure(self.client.channel.seal_message(&wire))? {
                send_frame(&mut self.client.socket, record).await?;
            }
            let bytes = receive_value(&mut self.client.socket, &mut self.client.channel).await?;
            if policy {
                require_node_policy_carrier(wire.len(), bytes.len())?;
            }
            serde_json::from_slice::<UniqueJsonKeys>(&bytes)?;
            let reply: TaskBusinessReply = serde_json::from_slice(&bytes)?;
            ensure!(
                reply.id == id,
                "task business response changed its request ID"
            );
            match (reply.result, reply.error) {
                (Some(response), None) => Ok(response),
                (None, Some(error)) => bail!("{error}"),
                _ => bail!("invalid task business response"),
            }
        })
        .await
        .context("task business response timed out; reconcile before retrying business")??;
        let business = match request {
            TaskBusinessRequest::Business(wire) => Some((wire.clone(), TaskBusinessKind::Business)),
            TaskBusinessRequest::Status { wire } => Some((wire.clone(), TaskBusinessKind::Status)),
            TaskBusinessRequest::Receipts { .. } => {
                let TaskBusinessClientPhase::Pending(original) = &self.phase else {
                    unreachable!()
                };
                Some((original.wire.clone(), original.kind))
            }
            _ => None,
        };
        if let Some((wire, kind)) = business {
            let pending = task_business_response_pending(&self.header_hash, wire, kind, &response)?;
            if let (TaskBusinessClientPhase::Pending(original), Some(next)) =
                (&self.phase, pending.as_ref())
            {
                ensure!(
                    original.challenge.hash()? == next.challenge.hash()?,
                    "pending task business challenge changed"
                );
            }
            self.phase = match pending {
                Some(original) => TaskBusinessClientPhase::Pending(original),
                None => TaskBusinessClientPhase::Ready,
            };
            return Ok(response);
        }
        match (request, &response) {
            (
                TaskBusinessRequest::SourceBegin { .. },
                TaskBusinessResponse::SourceBegun { context_hash },
            ) => {
                ensure!(
                    *context_hash == self.header_hash,
                    "task business response changed context"
                );
                self.phase = TaskBusinessClientPhase::Importing;
            }
            (
                TaskBusinessRequest::SourcePage { page },
                TaskBusinessResponse::SourcePageAccepted {
                    context_hash,
                    next_page_index,
                    complete,
                },
            ) => {
                ensure!(
                    *context_hash == self.header_hash
                        && page.payload.page_index.checked_add(1) == Some(*next_page_index)
                        && page.payload.complete == *complete,
                    "task business response changed page progress"
                );
            }
            (
                TaskBusinessRequest::SourceFinish { .. },
                TaskBusinessResponse::SourceFinished { context_hash },
            ) => {
                ensure!(
                    *context_hash == self.header_hash,
                    "task business response changed context"
                );
                self.phase = TaskBusinessClientPhase::Ready;
            }
            _ => bail!("task business response changed its request stage"),
        }
        Ok(response)
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NodePolicyReadReply {
    id: String,
    result: Option<NodeReadResponse>,
    error: Option<String>,
}

impl NodePolicyReadClient {
    pub(crate) async fn connect(
        network: &NetworkHub,
        identity: &LocalIdentity,
        trust: &VerifiedTaskExecutionTrust,
    ) -> Result<Self> {
        let target = &trust.selection().target_public_key;
        let target_node_id = crate::crypto::fingerprint(target.as_str())?;
        let socket = network.dial(&target_node_id, Service::Client).await?;
        let (client, noise_peer) =
            Client::from_socket_with_policy_peer(socket, identity, &target_node_id).await?;
        ensure!(
            noise_peer.public_key() == target,
            "node policy Noise peer differs from the approved target key"
        );
        Ok(Self { client, noise_peer })
    }

    pub(crate) fn actual_noise_peer(&self) -> &ActualPolicyNoisePeer {
        &self.noise_peer
    }

    pub(crate) async fn read(&mut self, request: &NodeReadRequest) -> Result<NodeReadResponse> {
        let id = uuid::Uuid::new_v4().to_string();
        let wire = serde_json::to_vec(&Request {
            id: id.clone(),
            method: NODE_POLICY_READ_METHOD.to_owned(),
            params: serde_json::to_value(request)?,
            invocation: None,
        })?;
        require_node_policy_carrier(wire.len(), 0)?;
        tokio::time::timeout(CALL_TIMEOUT, async {
            for record in secure(self.client.channel.seal_message(&wire))? {
                send_frame(&mut self.client.socket, record).await?;
            }
            let bytes = receive_value(&mut self.client.socket, &mut self.client.channel).await?;
            require_node_policy_carrier(wire.len(), bytes.len())?;
            serde_json::from_slice::<UniqueJsonKeys>(&bytes)?;
            let response: NodePolicyReadReply = serde_json::from_slice(&bytes)?;
            ensure!(
                response.id == id,
                "node policy response changed its request ID"
            );
            let response = match (response.result, response.error) {
                (Some(response), None) => response,
                (None, Some(error)) => bail!("{error}"),
                _ => bail!("invalid node policy response"),
            };
            ensure!(
                matches!(
                    (request, &response),
                    (NodeReadRequest::Prepare(_), NodeReadResponse::Snapshot(_))
                        | (NodeReadRequest::Page(_), NodeReadResponse::Page(_))
                        | (NodeReadRequest::Finish(_), NodeReadResponse::Witness(_))
                        | (NodeReadRequest::Freshness(_), NodeReadResponse::Receipts(_))
                ),
                "node policy response changed its read stage"
            );
            Ok(response)
        })
        .await
        .context("node policy read timed out")?
    }
}

fn require_node_policy_carrier(request_bytes: usize, response_bytes: usize) -> Result<()> {
    ensure!(
        request_bytes.saturating_add(response_bytes) <= MAX_PROOF_BYTES,
        "node policy request and response exceed 64 KiB"
    );
    Ok(())
}

impl Client {
    pub async fn connect(
        url: &str,
        identity: &LocalIdentity,
        target: &str,
        relay: bool,
    ) -> Result<Self> {
        let endpoint = client_endpoint(url)?;
        let socket = if relay {
            crate::network::connect_relay(&endpoint, target, Service::Client).await?
        } else {
            crate::network::connect_direct(&endpoint, Service::Client).await?
        };
        Self::from_socket(socket, identity, target).await
    }
    async fn from_socket(
        socket: NetworkSocket,
        identity: &LocalIdentity,
        target: &str,
    ) -> Result<Self> {
        let (client, _) = Self::from_socket_with_policy_peer(socket, identity, target).await?;
        Ok(client)
    }
    async fn from_socket_with_policy_peer(
        mut socket: NetworkSocket,
        identity: &LocalIdentity,
        target: &str,
    ) -> Result<(Self, ActualPolicyNoisePeer)> {
        let (channel, noise_peer) = tokio::time::timeout(HANDSHAKE_TIMEOUT, async {
            let mut handshake = secure(Handshake::new(
                true,
                &identity.secret_key_hex,
                Some(target.to_owned()),
                b"vibeface:client:v2",
            ))?;
            send_frame(&mut socket, secure(handshake.write(&[]))?).await?;
            let second = next_binary(&mut socket).await?;
            ensure!(
                secure(handshake.read(&second))?.is_empty(),
                "unexpected client handshake payload"
            );
            send_frame(&mut socket, secure(handshake.write(&[]))?).await?;
            let noise_peer = ActualPolicyNoisePeer::from_finished_handshake(&handshake)?;
            Ok::<_, anyhow::Error>((secure(handshake.finish())?, noise_peer))
        })
        .await
        .context("client handshake timed out")??;
        Ok((Self { socket, channel }, noise_peer))
    }
    pub async fn call(&mut self, method: &str, params: Value) -> Result<Value> {
        let id = uuid::Uuid::new_v4().to_string();
        let request = Request {
            id: id.clone(),
            method: method.to_owned(),
            params,
            invocation: None,
        };
        send_value(&mut self.socket, &mut self.channel, &request).await?;
        tokio::time::timeout(CALL_TIMEOUT, async {
            loop {
                let value: Value = serde_json::from_slice(
                    &receive_value(&mut self.socket, &mut self.channel).await?,
                )?;
                if value["id"].as_str() != Some(&id) {
                    continue;
                }
                if let Some(error) = value["error"].as_str() {
                    bail!("{error}");
                }
                return Ok(value["result"].clone());
            }
        })
        .await
        .context("client response timed out; outcome may be unknown")?
    }
}

pub async fn serve(node: Arc<Runtime>, mut socket: NetworkSocket) -> Result<()> {
    let (mut channel, noise_peer) = tokio::time::timeout(HANDSHAKE_TIMEOUT, async {
        let mut handshake = secure(Handshake::new(
            false,
            &node.identity.secret_key_hex,
            None,
            b"vibeface:client:v2",
        ))?;
        ensure!(
            secure(handshake.read(&next_binary(&mut socket).await?))?.is_empty(),
            "unexpected first client payload"
        );
        send_frame(&mut socket, secure(handshake.write(&[]))?).await?;
        ensure!(
            secure(handshake.read(&next_binary(&mut socket).await?))?.is_empty(),
            "unexpected final client payload"
        );
        let noise_peer = ActualPolicyNoisePeer::from_finished_handshake(&handshake)?;
        Ok::<_, anyhow::Error>((secure(handshake.finish())?, noise_peer))
    })
    .await
    .context("client handshake timed out")??;
    let mut session = Session {
        public_key: noise_peer.public_key().as_str().to_owned(),
        noise_peer,
        purpose: SessionPurpose::Client,
        authorization: None,
        subscription: None,
        remote_subscription: None,
    };
    let mut events = node.events.subscribe();
    let mut heartbeat = tokio::time::interval(HEARTBEAT);
    heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut subscription_tick = tokio::time::interval(Duration::from_secs(5));
    subscription_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut received = Instant::now();
    let mut partial_started: Option<Instant> = None;
    loop {
        tokio::select! {
            frame = socket.next() => {
                received = Instant::now();
                match frame.context("client disconnected")?? {
                    Message::Binary(bytes) => {
                        if let Some(plaintext) = secure(channel.open_record(&bytes))? {
                            partial_started = None;
                            let request = parse_session_request(&plaintext, &session.purpose)
                                .context("invalid client request")?;
                            validate_request(&request)?;
                            let policy_carrier = matches!(request.method.as_str(), NODE_POLICY_READ_METHOD | TASK_RECEIPT_READ_METHOD | GATEWAY_SOURCE_METHOD | GATEWAY_FORWARD_METHOD)
                                || (request.method == TASK_BUSINESS_METHOD && !matches!(request.params["stage"].as_str(), Some("business" | "status")));
                            if policy_carrier {
                                require_node_policy_carrier(plaintext.len(), 0)?;
                            }
                            let result = dispatch(&node, &mut session, request).await;
                            let (id, result) = result;
                            let response = match result {
                                Ok(result) => Response { id, result, error: None },
                                Err(error) => Response { id, result: Value::Null, error: Some(error.to_string()) },
                            };
                            if policy_carrier || matches!(session.purpose, SessionPurpose::NodePolicyRead | SessionPurpose::TaskReceiptRead | SessionPurpose::GatewaySource | SessionPurpose::GatewayForward) {
                                require_node_policy_carrier(plaintext.len(), serde_json::to_vec(&response)?.len())?;
                            }
                            send_value(&mut socket, &mut channel, &response).await?;
                        } else { partial_started.get_or_insert_with(Instant::now); }
                    }
                    Message::Ping(_) | Message::Pong(_) => {},
                    _ => bail!("client channel accepts only encrypted binary frames"),
                }
            }
            event = events.recv(), if session.subscription.is_some() => {
                match event {
                    Ok(event) => {
                        let access = session.subscription.as_ref().context("subscription missing")?;
                        if node.subscription_allows(access, &event).await? {
                            send_value(&mut socket, &mut channel, &json!({"type":"changed"})).await?;
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                        session.subscription = None;
                        send_value(&mut socket, &mut channel, &json!({"type":"subscription_expired"})).await?;
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => return Ok(()),
                }
            }
            _ = subscription_tick.tick() => {
                let mut expired = false;
                if let Some(remote) = session.remote_subscription.as_mut() {
                    let client = session.authorization.as_ref().context("subscription authorization missing")?;
                    let result = tokio::time::timeout(Duration::from_secs(5), node.remote_subscription_cursor(
                        client, &remote.body, &remote.invocation, &session.public_key,
                    )).await;
                    match result {
                        Ok(Ok(cursor)) if cursor != remote.cursor => {
                            remote.cursor = cursor;
                            send_value(&mut socket, &mut channel, &json!({"type":"changed"})).await?;
                        }
                        Ok(Ok(_)) => {},
                        _ => expired = true,
                    }
                } else if let Some(access) = &session.subscription {
                    match node.refresh_subscription(access).await {
                        Ok(current) => session.subscription = Some(current),
                        Err(_) => expired = true,
                    }
                }
                if expired {
                    session.subscription = None;
                    session.remote_subscription = None;
                    send_value(&mut socket, &mut channel, &json!({"type":"subscription_expired"})).await?;
                }
            }
            _ = heartbeat.tick() => {
                ensure!(received.elapsed() < IDLE_TIMEOUT, "client channel idle");
                ensure!(partial_started.is_none_or(|at| at.elapsed() < IDLE_TIMEOUT), "client fragmented message timed out");
                tokio::time::timeout(HANDSHAKE_TIMEOUT, socket.send(Message::Ping(Vec::new().into()))).await.context("client heartbeat send timed out")??;
            }
        }
    }
}

async fn dispatch(
    node: &Arc<Runtime>,
    session: &mut Session,
    request: Request,
) -> (String, Result<Value>) {
    let id = request.id;
    let result = Box::pin(dispatch_request(
        node,
        session,
        &request.method,
        request.params,
        request.invocation,
    ))
    .await;
    (id, result)
}

async fn dispatch_request(
    node: &Arc<Runtime>,
    session: &mut Session,
    method: &str,
    params: Value,
    invocation: Option<Signed<Invocation>>,
) -> Result<Value> {
    session.select_method(method)?;
    match method {
        GATEWAY_SOURCE_METHOD => {
            ensure!(
                invocation.is_none(),
                "gateway source transport does not accept an outer Invocation"
            );
            let request: GatewaySourceRequest = serde_json::from_value(params)?;
            node.gateway_source_request(&session.noise_peer, request)
                .await
        }
        GATEWAY_FORWARD_METHOD => {
            ensure!(
                invocation.is_none(),
                "gateway forward transport does not accept an outer Invocation"
            );
            let request: gateway_forward::GatewayForwardRequest = serde_json::from_value(params)?;
            let reply = Box::pin(node.gateway_target_request(
                &session.noise_peer,
                request.header,
                request.original,
            ))
            .await?;
            Ok(serde_json::to_value(reply)?)
        }
        TASK_RECEIPT_READ_METHOD => {
            ensure!(
                invocation.is_none(),
                "task receipt reads do not accept an outer Invocation"
            );
            let request: Signed<TaskBusinessReceiptRead> = serde_json::from_value(params)?;
            let receipts = node
                .task_business_receipt_read(&session.noise_peer, request)
                .await?;
            Ok(serde_json::to_value(receipts)?)
        }
        TASK_BUSINESS_METHOD => {
            ensure!(
                invocation.is_none(),
                "task business transport does not accept an outer Invocation"
            );
            Box::pin(session.task_business(node, serde_json::from_value(params)?)).await
        }
        NODE_POLICY_READ_METHOD => {
            ensure!(
                invocation.is_none(),
                "node policy reads do not accept an Invocation"
            );
            let request: NodeReadRequest = serde_json::from_value(params)?;
            let response = node
                .node_execution_policy_read(&session.public_key, request)
                .await?;
            Ok(serde_json::to_value(response)?)
        }
        "info" => Ok(node.info()),
        "authenticate" => {
            let (verified, response) = {
                let mut store = node.store.lock().await;
                store.authenticate_confirmed_client_policy(
                    &node.identity,
                    &session.public_key,
                    &params,
                    crate::runtime::now_ms() as u64,
                )?
            };
            session.subscription = None;
            session.remote_subscription = None;
            session.authorization = Some(verified);
            Ok(response)
        }
        "prepare_invocation" => {
            let client = session
                .authorization
                .as_ref()
                .context("authenticate before preparing a request")?;
            let intent = serde_json::from_value(params["intent"].clone())?;
            node.prepare_client_request(client, params["body"].clone(), intent, &session.public_key)
                .await
        }
        "rpc" | "subscribe" => {
            let client = session
                .authorization
                .as_ref()
                .context("authenticate before sending a request")?;
            if params["method"] == "execution_observation" {
                ensure!(
                    method == "rpc" && invocation.is_none(),
                    "execution observation requires its dedicated signed request"
                );
                let observation: Signed<ExecutionObservationRequest> =
                    serde_json::from_value(params["observation_request"].clone())?;
                let request = &observation.payload.request;
                ensure!(
                    request.actor == Actor::Account(client.scope().account_id.clone())
                        && request.device == client.scope().device_id
                        && request.resource.space == client.scope().space_id
                        && &request.audience == client.audience(),
                    "observation differs from the authenticated client scope"
                );
                if client.audience() != &node.local_audience()? {
                    return node
                        .forward_client_execution_observation(client, &params, &session.public_key)
                        .await;
                }
                return node
                    .observe_client_execution(
                        params,
                        TransportEvidence::DirectNoise {
                            device: DeviceId::from_key(&PublicKey::parse(&session.public_key)?),
                        },
                    )
                    .await;
            }
            ensure!(
                params["space_id"].as_str() == Some(client.scope().space_id.as_str()),
                "request belongs to another space"
            );
            let invocation = invocation.context("signed Invocation is required")?;
            ensure!(
                invocation.payload.grant_leaf == *client.grant_leaf(),
                "request uses another grant"
            );
            if client.audience() != &node.local_audience()? {
                if method == "subscribe" {
                    let cursor = node
                        .remote_subscription_cursor(
                            client,
                            &params,
                            &invocation,
                            &session.public_key,
                        )
                        .await?;
                    session.subscription = None;
                    session.remote_subscription = Some(RemoteSubscription {
                        body: params,
                        invocation,
                        cursor,
                    });
                    return Ok(json!({"subscribed":true}));
                }
                return node
                    .forward_client_request(client, &params, &invocation, &session.public_key)
                    .await;
            }
            let device = DeviceId::from_key(&PublicKey::parse(&session.public_key)?);
            let transport = if invocation.payload.presenter == device {
                TransportEvidence::DirectNoise { device }
            } else {
                TransportEvidence::GatewayNoise {
                    device,
                    gateway: DeviceId::from_key(&PublicKey::parse(
                        &node.identity.public_key_hex()?,
                    )?),
                }
            };
            let access = node
                .authorize_rpc(params.clone(), invocation, transport)
                .await?;
            if method == "subscribe" {
                node.validate_subscription(&access).await?;
                session.remote_subscription = None;
                session.subscription = Some(access);
                Ok(json!({"subscribed":true}))
            } else {
                Box::pin(node.rpc(access, params)).await
            }
        }
        "identity" => {
            let method = params["method"]
                .as_str()
                .context("identity method is required")?;
            ensure!(
                method != "authenticate",
                "use the authenticate channel method to bind this session"
            );
            node.identity_command(&session.public_key, method, &params["params"])
                .await
        }
        _ => bail!("unknown client method"),
    }
}

fn client_endpoint(value: &str) -> Result<String> {
    let mut url = reqwest::Url::parse(value)?;
    let scheme = match url.scheme() {
        "http" | "ws" => "ws",
        "https" | "wss" => "wss",
        _ => bail!("invalid client URL scheme"),
    };
    url.set_scheme(scheme)
        .map_err(|_| anyhow::anyhow!("invalid client URL"))?;
    ensure!(
        url.username().is_empty()
            && url.password().is_none()
            && url.query().is_none()
            && url.fragment().is_none(),
        "client URL must not contain credentials, query, or fragment"
    );
    ensure!(
        matches!(url.path(), "/" | "/api/network/ws"),
        "client URL must be a node base URL or network endpoint"
    );
    url.set_path("/api/network/ws");
    Ok(url.into())
}

fn secure<T>(result: std::result::Result<T, String>) -> Result<T> {
    result.map_err(anyhow::Error::msg)
}

struct UniqueJsonKeys;

impl<'de> Deserialize<'de> for UniqueJsonKeys {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        deserializer.deserialize_any(Self)
    }
}

impl<'de> Visitor<'de> for UniqueJsonKeys {
    type Value = Self;

    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("JSON with unique object keys")
    }

    fn visit_bool<E>(self, _: bool) -> std::result::Result<Self, E> {
        Ok(Self)
    }

    fn visit_i64<E>(self, _: i64) -> std::result::Result<Self, E> {
        Ok(Self)
    }

    fn visit_u64<E>(self, _: u64) -> std::result::Result<Self, E> {
        Ok(Self)
    }

    fn visit_f64<E>(self, _: f64) -> std::result::Result<Self, E> {
        Ok(Self)
    }

    fn visit_str<E>(self, _: &str) -> std::result::Result<Self, E> {
        Ok(Self)
    }

    fn visit_unit<E>(self) -> std::result::Result<Self, E> {
        Ok(Self)
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut sequence: A) -> std::result::Result<Self, A::Error> {
        while sequence.next_element::<Self>()?.is_some() {}
        Ok(Self)
    }

    fn visit_map<A: MapAccess<'de>>(self, mut object: A) -> std::result::Result<Self, A::Error> {
        let mut keys = HashSet::new();
        while let Some(key) = object.next_key::<String>()? {
            if !keys.insert(key) {
                return Err(de::Error::custom("duplicate JSON object key"));
            }
            object.next_value::<Self>()?;
        }
        Ok(Self)
    }
}

fn parse_request(bytes: &[u8]) -> Result<Request> {
    serde_json::from_slice::<UniqueJsonKeys>(bytes)?;
    Ok(serde_json::from_slice(bytes)?)
}

#[derive(Deserialize)]
struct RequestMethodProbe {
    method: RequestMethodKind,
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum RequestMethodKind {
    GatewaySource,
    GatewayForward,
    #[serde(other)]
    Other,
}

fn parse_session_request(bytes: &[u8], purpose: &SessionPurpose) -> Result<Request> {
    if bytes.len() > MAX_PROOF_BYTES {
        // The probe skips params without constructing their JSON tree.
        let gateway = matches!(
            purpose,
            SessionPurpose::GatewaySource | SessionPurpose::GatewayForward
        ) || matches!(
            serde_json::from_slice::<RequestMethodProbe>(bytes)?.method,
            RequestMethodKind::GatewaySource | RequestMethodKind::GatewayForward
        );
        if gateway {
            require_node_policy_carrier(bytes.len(), 0)?;
        }
    }
    parse_request(bytes)
}

fn validate_request(request: &Request) -> Result<()> {
    ensure!(
        !request.id.is_empty()
            && request.id.len() <= 128
            && request
                .id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-_.:".contains(&b)),
        "invalid request ID"
    );
    ensure!(
        !request.method.is_empty() && request.method.len() <= 64 && request.params.is_object(),
        "invalid client method or params"
    );
    Ok(())
}
async fn send_frame(socket: &mut NetworkSocket, record: Vec<u8>) -> Result<()> {
    tokio::time::timeout(
        HANDSHAKE_TIMEOUT,
        socket.send(Message::Binary(record.into())),
    )
    .await
    .context("client send timed out")?
}
async fn next_binary(socket: &mut NetworkSocket) -> Result<Vec<u8>> {
    while let Some(frame) = socket.next().await {
        match frame? {
            Message::Binary(bytes) => {
                ensure!(bytes.len() <= 65_535, "client frame too large");
                return Ok(bytes.to_vec());
            }
            Message::Ping(_) | Message::Pong(_) => {}
            _ => bail!("client channel accepts only encrypted binary frames"),
        }
    }
    bail!("client disconnected")
}
async fn send_value<T: Serialize>(
    socket: &mut NetworkSocket,
    channel: &mut Channel,
    value: &T,
) -> Result<()> {
    let bytes = serde_json::to_vec(value)?;
    for record in secure(channel.seal_message(&bytes))? {
        send_frame(socket, record).await?;
    }
    Ok(())
}
async fn receive_value(socket: &mut NetworkSocket, channel: &mut Channel) -> Result<Vec<u8>> {
    loop {
        if let Some(value) = secure(channel.open_record(&next_binary(socket).await?))? {
            return Ok(value);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn node_policy_reader_uses_real_noise_and_cannot_upgrade_its_session() -> Result<()> {
        use crate::{
            config::{self, Config},
            identity::{foreign_execution_policy::tests::unseeded_fixture, test_support::now},
            network::{MAX_FRAME_BYTES, NetworkConfig},
            store::Store,
        };
        use axum::{
            Router,
            extract::{State as AxumState, WebSocketUpgrade},
            routing::get,
        };
        use tokio::{net::TcpListener, task::JoinSet};
        use vibeface_secure::{
            Identity,
            authorization::{
                AUTH_VERSION, Nonce, ObjectHash,
                node_execution_policy::{
                    NodeExecutionPolicyReadIntent, NodeExecutionPolicyReadRequest,
                },
                remote_execution_policy::RemoteExecutionPolicyPageRequest,
            },
        };

        fn policy_counts(store: &Store) -> Result<(u64, u64, u64)> {
            Ok(store.connection.query_row(
                "SELECT (SELECT count(*) FROM policy_pins),
                    (SELECT count(*) FROM policy_journal),
                    (SELECT count(*) FROM current_policy_heads)",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )?)
        }

        async fn upgrade(
            AxumState(hub): AxumState<Arc<NetworkHub>>,
            ws: WebSocketUpgrade,
        ) -> impl axum::response::IntoResponse {
            ws.max_message_size(MAX_FRAME_BYTES)
                .max_frame_size(MAX_FRAME_BYTES)
                .on_upgrade(|socket| async move {
                    let _ = hub.accept(socket).await;
                })
        }

        let mut fixture = unseeded_fixture()?;
        let before_b = policy_counts(&fixture.store)?;
        let before_c = policy_counts(&fixture.target)?;
        let b_node_id = fixture.project.node.node_id()?;
        assert_eq!(
            fixture.target.connection.query_row(
                "SELECT count(*) FROM node_certificates WHERE node_id=?",
                [&b_node_id],
                |row| row.get::<_, u64>(0),
            )?,
            0
        );
        assert!(
            fixture
                .target
                .authorize_transport_node(&b_node_id, now())
                .is_err()
        );
        assert!(
            fixture
                .store
                .authorization_state()
                .account(&fixture.executor.account)
                .is_none()
        );
        let trust = fixture.store.approve_task_execution_policy(
            &fixture.project.node,
            &fixture.project.client.public_key_hex(),
            &fixture.intent,
            now(),
        )?;
        let original = &trust.original_intent().payload;
        let signer = Identity::from_secret_hex(&fixture.project.node.secret_key_hex)
            .map_err(anyhow::Error::msg)?;
        let request = NodeExecutionPolicyReadRequest::Current(Signed::sign(
            NodeExecutionPolicyReadIntent {
                version: AUTH_VERSION,
                source_project: original.source_selection.resource.clone(),
                selection: original.selection.clone(),
                context: original.context.clone(),
                expected_action: original.expected_action,
                purpose: original.purpose,
                source_provenance: None,
                execution_pins: original.execution_pins.clone(),
                nonce: Nonce::random()?,
                issued_at_ms: now(),
                expires_at_ms: original.expires_at_ms,
            },
            &signer,
        )?);

        let dir = &fixture.executor.directory;
        config::private_write(
            &dir.join("data-epoch.json"),
            &serde_json::to_vec(&config::data_format())?,
        )?;
        let mut cfg = Config::local(
            "127.0.0.1:4790".parse()?,
            vec![fixture.executor.project.clone()],
            false,
        );
        cfg.codex_socket = Some(dir.join("unavailable.sock"));
        cfg.codex_bin = dir.join("missing-codex");
        cfg.claude_bin = dir.join("missing-claude");
        cfg.save(dir)?;
        config::private_write(
            &dir.join("identity.json"),
            &serde_json::to_vec(&fixture.executor.node)?,
        )?;
        drop(fixture.executor.at(&dir.join("node.sqlite3"))?);
        let node = Arc::new(Runtime::open(dir).await?);
        *node.store.lock().await = fixture.target;

        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let url = format!("ws://{}/api/network/ws", listener.local_addr()?);
        let (target_network, mut incoming) = NetworkHub::new(
            fixture.executor.node.clone(),
            NetworkConfig {
                advertise_urls: vec![url.clone()],
                ..Default::default()
            },
        );
        let router = Router::new()
            .route("/api/network/ws", get(upgrade))
            .with_state(target_network);
        let mut tasks = JoinSet::new();
        tasks.spawn(async move {
            let _ = axum::serve(listener, router).await;
        });
        let server = node.clone();
        tasks.spawn(async move {
            while let Some(incoming) = incoming.recv().await {
                assert_eq!(incoming.service, Service::Client);
                let _ = serve(server.clone(), incoming.socket).await;
            }
        });
        let (network, _) = NetworkHub::new(
            fixture.project.node.clone(),
            NetworkConfig {
                bootstrap_urls: vec![url],
                ..Default::default()
            },
        );

        let target_key = &trust.selection().target_public_key;
        let socket = network
            .dial(&fixture.executor.node.node_id()?, Service::Client)
            .await?;
        let wrong_pin = Client::from_socket(socket, &fixture.project.node, target_key.as_str())
            .await
            .err()
            .context("a public key must not pass the Noise fingerprint gate")?;
        assert!(
            wrong_pin
                .to_string()
                .contains("unexpected Noise peer identity")
        );

        let mut reader =
            NodePolicyReadClient::connect(&network, &fixture.project.node, &trust).await?;
        assert_eq!(reader.actual_noise_peer().public_key(), target_key);
        let NodeReadResponse::Snapshot(snapshot) = reader
            .read(&NodeReadRequest::Prepare(request.clone()))
            .await?
        else {
            anyhow::bail!("expected a Node policy snapshot");
        };
        snapshot.verify_signature()?;
        assert_eq!(&snapshot.issuer, target_key);
        assert_eq!(snapshot.payload.request_hash, ObjectHash::body(&request)?);
        for (method, params) in [
            ("authenticate", json!({})),
            ("rpc", json!({})),
            ("identity", json!({"method":"status","params":{}})),
            (TASK_BUSINESS_METHOD, json!({})),
            (TASK_RECEIPT_READ_METHOD, json!({})),
            (GATEWAY_SOURCE_METHOD, json!({})),
            (GATEWAY_FORWARD_METHOD, json!({})),
        ] {
            let error = reader
                .client
                .call(method, params)
                .await
                .err()
                .context("a policy reader must not upgrade its session")?;
            assert!(
                error.to_string().contains("cannot change purpose"),
                "{error}"
            );
        }

        let mut cursor = None;
        let mut previous = snapshot.hash()?;
        let mut page_count = 0;
        let mut entry_count = 0;
        loop {
            let NodeReadResponse::Page(page) = reader
                .read(&NodeReadRequest::Page(RemoteExecutionPolicyPageRequest {
                    snapshot_id: snapshot.payload.snapshot_id.clone(),
                    request_hash: snapshot.payload.request_hash.clone(),
                    cursor: cursor.clone(),
                }))
                .await?
            else {
                anyhow::bail!("expected a Node policy page");
            };
            page.verify_signature()?;
            assert_eq!(&page.issuer, target_key);
            assert_eq!(page.payload.snapshot_id, snapshot.payload.snapshot_id);
            assert_eq!(page.payload.request_hash, snapshot.payload.request_hash);
            assert_eq!(page.payload.page_index, page_count);
            assert_eq!(page.payload.request_cursor, cursor);
            assert_eq!(page.payload.previous_page_hash, previous);
            previous = page.hash()?;
            page_count += 1;
            entry_count += u64::from(page.payload.entry.is_some());
            if page.payload.complete {
                assert!(page.payload.next_cursor.is_none());
                break;
            }
            cursor = Some(
                page.payload
                    .next_cursor
                    .context("next policy page missing")?,
            );
            ensure!(page_count < 128, "Node policy stream did not finish");
        }
        let NodeReadResponse::Witness(witness) = reader
            .read(&NodeReadRequest::Finish(RemoteExecutionPolicyPageRequest {
                snapshot_id: snapshot.payload.snapshot_id.clone(),
                request_hash: snapshot.payload.request_hash.clone(),
                cursor: None,
            }))
            .await?
        else {
            anyhow::bail!("expected a Node policy witness");
        };
        witness.verify_signature()?;
        assert_eq!(&witness.issuer, target_key);
        assert_eq!(witness.payload.snapshot_id, snapshot.payload.snapshot_id);
        assert_eq!(witness.payload.request_hash, snapshot.payload.request_hash);
        assert_eq!(witness.payload.snapshot_hash, snapshot.hash()?);
        assert_eq!(witness.payload.final_page_hash, previous);
        assert_eq!(witness.payload.page_count, page_count);
        assert_eq!(witness.payload.entry_count, entry_count);
        assert!(page_count > 0 && entry_count > 0);
        assert_eq!(policy_counts(&fixture.store)?, before_b);
        assert!(
            fixture
                .store
                .authorization_state()
                .account(&fixture.executor.account)
                .is_none()
        );
        let target = node.store.lock().await;
        assert_eq!(policy_counts(&target)?, before_c);
        assert_eq!(
            target.connection.query_row(
                "SELECT count(*) FROM node_certificates WHERE node_id=?",
                [&b_node_id],
                |row| row.get::<_, u64>(0),
            )?,
            0
        );
        assert!(target.authorize_transport_node(&b_node_id, now()).is_err());
        drop(target);
        drop(reader);
        tasks.abort_all();
        while tasks.join_next().await.is_some() {}
        Ok(())
    }

    fn task_business_checked<T>(phase: &str, action: impl FnOnce() -> Result<T>) -> Result<T> {
        let signatures = vibeface_secure::signature_verification::SignatureVerification::new();
        let result = signatures.run(action).with_context(|| phase.to_owned())?;
        ensure!(
            signatures.counts().actual_verifications <= 32,
            "Task business {phase} exceeded its crypto budget"
        );
        Ok(result)
    }

    async fn task_business_operation_current(
        b: &Arc<Runtime>,
        c: &Arc<Runtime>,
        original: &VerifiedTaskBusinessOriginalSource,
        stage: CheckStage,
    ) -> Result<vibeface_secure::authorization::task_business_policy::original_operation::VerifiedTaskBusinessOperationCurrent>{
        use crate::runtime::now_ms;
        use vibeface_secure::{
            Identity,
            authorization::{
                AUTH_VERSION, INVOCATION_MAX_AGE_MS, Nonce,
                execution_handoff::ExecutionHandoffFreshness,
            },
        };
        let mut context = {
            let mut store = c.store.lock().await;
            task_business_checked("operation begin", || {
                store.begin_task_business_operation_check(
                    &c.identity,
                    original,
                    original.wire(),
                    original.acceptance(),
                    stage,
                    now_ms() as u64,
                )
            })?
        };
        let now = now_ms() as u64;
        let signer =
            Identity::from_secret_hex(&c.identity.secret_key_hex).map_err(anyhow::Error::msg)?;
        let request = Signed::sign(
            TaskBusinessReceiptRead {
                version: AUTH_VERSION,
                source_header_hash: original.header().hash()?,
                business_wire_hash: original.wire_hash().clone(),
                challenge: TaskBusinessChallenge::Submit(context.challenge().clone()),
                acceptance: Some(original.acceptance().clone()),
                nonce: Nonce::random()?,
                issued_at_ms: now,
                expires_at_ms: (now + INVOCATION_MAX_AGE_MS).min(context.challenge().expires_at_ms),
            },
            &signer,
        )?;
        let mut reader =
            TaskBusinessReceiptClient::connect_original(&c.network, &c.identity, original).await?;
        let mut receipts = context.local_receipts().to_vec();
        let foreign = reader.read(&request).await?;
        ensure!(
            !foreign.is_empty(),
            "B returned no current operation receipts"
        );
        let source_key = b.identity.public_key_hex()?;
        assert!(
            foreign
                .iter()
                .all(|receipt| receipt.issuer.as_str() == source_key)
        );
        receipts.extend(foreign);
        for method in [
            "authenticate",
            "rpc",
            TASK_BUSINESS_METHOD,
            NODE_POLICY_READ_METHOD,
        ] {
            let error = reader
                .client
                .call(method, json!({}))
                .await
                .err()
                .context("receipt reader changed purpose")?;
            assert!(
                error.to_string().contains("cannot change purpose"),
                "{error}"
            );
        }
        let proof = ExecutionHandoffFreshness {
            challenge: context.challenge().clone(),
            receipts,
        };
        let store = c.store.lock().await;
        task_business_checked("operation finish", || {
            store.finish_task_business_operation_check(
                &c.identity,
                &mut context,
                &proof,
                now_ms() as u64,
            )
        })
    }

    async fn task_business_rpc(
        client: &mut TaskBusinessClient,
        b: &Runtime,
        source: &crate::identity::task_business_policy::TaskBusinessPolicySourceContext,
        authorization: &vibeface_secure::authorization::execution_handoff::VerifiedExecutionHandoff,
        business: &TaskBusinessWire,
        submit: bool,
    ) -> Result<vibeface_secure::authorization::execution_handoff::ExecutionHandoffResult> {
        use vibeface_secure::authorization::task_business_policy::TaskBusinessAuthorization;
        let header_hash = source.header().hash()?;
        let wire_hash = business.hash()?;
        let request = if submit {
            TaskBusinessRequest::Business(business.clone())
        } else {
            TaskBusinessRequest::Status {
                wire: business.clone(),
            }
        };
        let TaskBusinessResponse::Challenge {
            source_context,
            business_wire_hash,
            challenge,
        } = client.exchange(&request).await?
        else {
            anyhow::bail!("Task business omitted its genuine current challenge");
        };
        assert_eq!(source_context, header_hash);
        assert_eq!(business_wire_hash, wire_hash);
        let TaskBusinessChallenge::Submit(typed) = &challenge else {
            anyhow::bail!("Submit changed its typed challenge");
        };
        assert_eq!(
            typed.stage,
            if submit {
                CheckStage::Admit
            } else {
                CheckStage::RemoteRead
            }
        );
        let receipts = {
            let store = b.store.lock().await;
            task_business_checked("B challenge receipts", || {
                store.task_business_challenge_receipts(
                    &b.identity,
                    source.current_policy(),
                    TaskBusinessAuthorization::Submit(authorization),
                    business,
                    &header_hash,
                    &challenge,
                    crate::runtime::now_ms() as u64,
                )
            })?
        };
        let response = client
            .exchange(&TaskBusinessRequest::Receipts {
                source_context: header_hash,
                business_wire_hash: wire_hash,
                challenge_hash: challenge.hash()?,
                receipts,
            })
            .await?;
        let value = match response {
            TaskBusinessResponse::Business(value) if submit => value,
            TaskBusinessResponse::Status(value) if !submit => value,
            _ => anyhow::bail!("Task business changed its terminal response purpose"),
        };
        Ok(serde_json::from_value(value)?)
    }

    async fn task_business_noise_lifecycle(revoke: bool) -> Result<()> {
        use crate::{
            config::{self, Config},
            identity::{
                execution_dispatch::tests::Fixture,
                foreign_execution_policy::tests::{
                    admit_unseeded_dispatch, import_current_policy, unseeded_fixture_with_source,
                },
                test_support::{TestNetwork, now},
            },
            model::OperationState,
            network::{MAX_FRAME_BYTES, NetworkConfig},
            runtime::now_ms,
            store::Store,
        };
        use axum::{
            Router,
            extract::{State as AxumState, WebSocketUpgrade},
            routing::get,
        };
        use tokio::{net::TcpListener, task::JoinSet};
        use vibeface_secure::authorization::{
            EventRef, ExecutionSourcePolicy, InvocationContext, JournalEntry, PolicyDomain,
            ResourceRef, SessionRef,
            execution_handoff::{ExecutionHandoffResult, ExecutionHandoffState},
            prepare::PrepareContext,
        };

        fn primary_counts(store: &Store) -> Result<(u64, u64, u64)> {
            Ok(store.connection.query_row(
                "SELECT (SELECT count(*) FROM policy_pins), (SELECT count(*) FROM policy_journal), (SELECT count(*) FROM current_policy_heads)",
                [], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )?)
        }
        async fn upgrade(
            AxumState(hub): AxumState<Arc<NetworkHub>>,
            ws: WebSocketUpgrade,
        ) -> impl axum::response::IntoResponse {
            ws.max_message_size(MAX_FRAME_BYTES)
                .max_frame_size(MAX_FRAME_BYTES)
                .on_upgrade(|socket| async move {
                    let _ = hub.accept(socket).await;
                })
        }
        async fn runtime(
            network: &TestNetwork,
            store: Store,
            peer: &str,
            own: &str,
        ) -> Result<Arc<Runtime>> {
            let dir = &network.directory;
            config::private_write(
                &dir.join("data-epoch.json"),
                &serde_json::to_vec(&config::data_format())?,
            )?;
            let mut cfg = Config::local(
                "127.0.0.1:4790".parse()?,
                vec![network.project.clone()],
                false,
            );
            cfg.codex_socket = Some(dir.join("unavailable.sock"));
            cfg.codex_bin = dir.join("missing-codex");
            cfg.claude_bin = dir.join("missing-claude");
            cfg.network.bootstrap_urls = vec![peer.into()];
            cfg.network.advertise_urls = vec![own.into()];
            cfg.save(dir)?;
            config::private_write(
                &dir.join("identity.json"),
                &serde_json::to_vec(&network.node)?,
            )?;
            drop(network.at(&dir.join("node.sqlite3"))?);
            let value = Arc::new(Runtime::open(dir).await?);
            *value.store.lock().await = store;
            Ok(value)
        }
        let project = TestNetwork::new()?;
        let executor = TestNetwork::new()?;
        let creator = TestNetwork::new()?;
        assert_ne!(creator.account, project.account);
        assert_ne!(creator.account, executor.account);
        let mut store = project.at(&project.directory.join("foreign-approval.sqlite3"))?;
        project.conversation(&mut store, "conversation")?;
        creator.install(&mut store)?;
        let task = Fixture::foreign_account_task(&project, &creator, &mut store)?.0;
        let target = PublicKey::parse(&executor.node.public_key_hex()?)?;
        let context = PrepareContext {
            context: InvocationContext {
                conversation: Some("conversation".into()),
                task: Some(EventRef::parse(&task.hash)?),
                execution_session: Some(SessionRef {
                    target: DeviceId::from_key(&target),
                    session_id: "remote-session".into(),
                }),
                executor_kind: Some("codex".into()),
                workspace_resource: Some(project.resource.clone()),
                sdk_session: None,
            },
            binding_generation: Some(1),
            canonical_cwd: Some(executor.project.to_string_lossy().into_owned()),
            execution_target_public_key: Some(target),
        };
        let mut pins = project.pins.clone();
        pins.extend(creator.pins.clone());
        pins.sort_by(|left, right| left.domain.cmp(&right.domain));
        let approved_source = ExecutionSourcePolicy {
            project: ResourceRef {
                space: project.space.clone(),
                resource: project.resource.clone(),
            },
            node_public_key: PublicKey::parse(&project.node.public_key_hex()?)?,
            pins,
        };
        let mut fixture = unseeded_fixture_with_source(
            project,
            executor,
            store,
            context,
            false,
            Some(approved_source),
        )?;
        let remote = import_current_policy(&mut fixture)?;
        let wire = admit_unseeded_dispatch(&mut fixture, &remote)?;
        let business = TaskBusinessWire::Submit(wire.clone());
        let (mut source, authorization) = task_business_checked("source factory", || {
            fixture
                .store
                .prepare_execution_dispatch_task_business_source(
                    &fixture.project.node,
                    &wire,
                    now_ms() as u64,
                )
        })?
        .context("unseeded dispatch bypassed its Task source factory")?;
        let before_c = primary_counts(&fixture.target)?;
        assert!(
            fixture
                .target
                .authorization_state()
                .account(&fixture.project.account)
                .is_none()
        );
        assert!(
            fixture
                .target
                .authorization_state()
                .account(&creator.account)
                .is_none()
        );
        assert!(
            fixture
                .store
                .authorization_state()
                .account(&fixture.executor.account)
                .is_none()
        );
        let execution = fixture.intent.payload.selection.execution_resource.clone();
        fixture.target.local_set(
            fixture.executor.space.as_str(),
            &format!("resource-root:{}", execution.resource),
            &json!(fixture.executor.project),
        )?;
        let b_listener = TcpListener::bind("127.0.0.1:0").await?;
        let c_listener = TcpListener::bind("127.0.0.1:0").await?;
        let b_url = format!("ws://{}/api/network/ws", b_listener.local_addr()?);
        let c_url = format!("ws://{}/api/network/ws", c_listener.local_addr()?);
        let b = runtime(&fixture.project, fixture.store, &c_url, &b_url).await?;
        let c = runtime(&fixture.executor, fixture.target, &b_url, &c_url).await?;
        fn start_server(
            tasks: &mut JoinSet<()>,
            node: Arc<Runtime>,
            listener: TcpListener,
            own: String,
            peer: String,
        ) {
            let (hub, mut incoming) = NetworkHub::new(
                node.identity.clone(),
                NetworkConfig {
                    advertise_urls: vec![own],
                    bootstrap_urls: vec![peer],
                    ..Default::default()
                },
            );
            let router = Router::new()
                .route("/api/network/ws", get(upgrade))
                .with_state(hub);
            tasks.spawn(async move {
                let _ = axum::serve(listener, router).await;
            });
            tasks.spawn(async move {
                let mut clients = JoinSet::new();
                while let Some(incoming) = incoming.recv().await {
                    assert_eq!(incoming.service, Service::Client);
                    let node = node.clone();
                    clients.spawn(async move {
                        let _ = serve(node, incoming.socket).await;
                    });
                }
            });
        }
        let mut tasks = JoinSet::new();
        start_server(
            &mut tasks,
            c.clone(),
            c_listener,
            c_url.clone(),
            b_url.clone(),
        );
        let mut client =
            TaskBusinessClient::connect(&b.network, &b.identity, source.export()).await?;
        client
            .exchange(&TaskBusinessRequest::SourceBegin {
                header: source.header().clone(),
            })
            .await?;
        let mut pages = 0;
        loop {
            let chunk = {
                let mut store = b.store.lock().await;
                task_business_checked("source page", || {
                    store.next_task_business_source(&b.identity, &mut source, now_ms() as u64)
                })?
            };
            if let Some(page) = chunk.page {
                pages += 1;
                client
                    .exchange(&TaskBusinessRequest::SourcePage { page })
                    .await?;
            }
            if let Some(witness) = chunk.witness {
                client
                    .exchange(&TaskBusinessRequest::SourceFinish { witness })
                    .await?;
                break;
            }
            ensure!(pages < 128, "Task source did not finish");
        }
        assert!(pages > 0);
        for method in [
            "authenticate",
            "rpc",
            TASK_RECEIPT_READ_METHOD,
            NODE_POLICY_READ_METHOD,
        ] {
            let error = client
                .client
                .call(method, json!({}))
                .await
                .err()
                .context("Task business upgraded its session")?;
            assert!(
                error.to_string().contains("cannot change purpose"),
                "{error}"
            );
        }
        let response = client
            .exchange(&TaskBusinessRequest::Status {
                wire: business.clone(),
            })
            .await?;
        assert!(
            matches!(response, TaskBusinessResponse::Status(value) if value == json!({"state":"not_found"}))
        );
        assert_eq!(
            c.store.lock().await.connection.query_row(
                "SELECT count(*) FROM execution_handoffs",
                [],
                |row| row.get::<_, u64>(0)
            )?,
            0
        );
        let admitted =
            task_business_rpc(&mut client, &b, &source, &authorization, &business, true).await?;
        assert_eq!(admitted.handoff, wire);
        assert_eq!(
            admitted.receipt.payload.state,
            ExecutionHandoffState::Accepted
        );
        let start_guard = c.hold_handoff_resolution_for_test().await;
        start_server(&mut tasks, b.clone(), b_listener, b_url, c_url);
        while Arc::strong_count(&c.maintenance) == 1 {
            tokio::task::yield_now().await;
        }
        let (acceptance, mut recovery) = {
            let store = c.store.lock().await;
            let (_, acceptance) = store.execution_handoff_original_acceptance(
                execution.space.as_str(),
                fixture.project.space.as_str(),
                &wire.invocation.payload.operation_id,
            )?;
            let recovery = task_business_checked("accepted original begin", || {
                store.begin_task_business_original_recovery(
                    &c.identity,
                    &business,
                    &acceptance,
                    now_ms() as u64,
                )
            })?;
            (acceptance, recovery)
        };
        let original = loop {
            let recovered = {
                let store = c.store.lock().await;
                task_business_checked("accepted original page", || {
                    store.advance_task_business_original_recovery(&mut recovery)
                })?
            };
            if let Some(original) = recovered {
                break original;
            }
            tokio::task::yield_now().await;
        };
        assert_eq!(original.acceptance(), &acceptance);
        let start = task_business_operation_current(&b, &c, &original, CheckStage::Start).await?;
        start.proof_usage().enforce_limits()?;
        let (running, fresh, first_deadline) = {
            let mut store = c.store.lock().await;
            task_business_checked("Start consume", || {
                store.claim_execution_handoff_operation_current(
                    &c.identity,
                    &start,
                    now_ms() as u64,
                )
            })?
        };
        assert!(fresh);
        assert_eq!(running.state, OperationState::Running);
        let renewed = task_business_operation_current(&b, &c, &original, CheckStage::Renew).await?;
        let deadline = {
            let mut store = c.store.lock().await;
            task_business_checked("Renew consume", || {
                store.renew_execution_handoff_operation_current(
                    &c.identity,
                    &renewed,
                    now_ms() as u64,
                )
            })?
        };
        assert!(deadline >= first_deadline);
        if revoke {
            fixture.project.revoke_client(&mut *b.store.lock().await)?;
            assert!(
                task_business_operation_current(&b, &c, &original, CheckStage::Renew)
                    .await
                    .is_err()
            );
            let mut store = c.store.lock().await;
            let domain = PolicyDomain::Space(fixture.executor.space.clone());
            let mut document = store
                .authorization_state()
                .space(&fixture.executor.space)
                .context("C space missing")?
                .clone();
            document.previous = store.authorization_state().head(&domain).cloned();
            document.sequence += 1;
            document
                .grants
                .retain(|grant| grant != &wire.invocation.payload.grant_leaf);
            task_business_checked("C current grant revocation", || {
                store.install_policy_journal(
                    &[JournalEntry::Space(Signed::sign(
                        document,
                        &fixture.executor.root,
                    )?)],
                    now() + 1,
                )
            })?;
            assert!(
                store
                    .begin_task_business_operation_check(
                        &c.identity,
                        &original,
                        &business,
                        &acceptance,
                        CheckStage::Renew,
                        now_ms() as u64
                    )
                    .is_err()
            );
        }
        let completed = {
            let mut store = c.store.lock().await;
            task_business_checked("Result consume", || {
                store.finish_execution_handoff_operation_current(
                    &c.identity,
                    &renewed,
                    OperationState::Succeeded,
                    json!({"protocol_fixture":true}),
                    running.execution_session.as_ref(),
                    now_ms() as u64,
                )
            })?
        };
        assert_eq!(
            completed.state,
            if revoke {
                OperationState::Unknown
            } else {
                OperationState::Succeeded
            }
        );
        if !revoke {
            let result = ExecutionHandoffResult {
                handoff: wire.clone(),
                receipt: completed
                    .receipt
                    .clone()
                    .context("completed C result has no receipt")?,
                result: completed.result.clone(),
            };
            let mut store = b.store.lock().await;
            let applied = task_business_checked("B result acceptance", || {
                store.apply_execution_handoff_receipt(&b.identity, &result, now_ms() as u64)
            })?;
            assert_eq!(applied.state, OperationState::Succeeded);
        }
        let c_store = c.store.lock().await;
        if !revoke {
            assert_eq!(primary_counts(&c_store)?, before_c);
        }
        assert_eq!(
            c_store
                .connection
                .query_row("SELECT count(*) FROM events", [], |row| row
                    .get::<_, u64>(0))?,
            0
        );
        assert!(
            c_store
                .authorization_state()
                .account(&fixture.project.account)
                .is_none()
        );
        assert!(
            c_store
                .authorization_state()
                .account(&creator.account)
                .is_none()
        );
        assert_eq!(
            c_store.connection.query_row(
                "SELECT count(*) FROM node_certificates WHERE node_id=?",
                [fixture.project.node.node_id()?],
                |row| row.get::<_, u64>(0)
            )?,
            0
        );
        assert!(
            c_store
                .authorize_transport_node(&fixture.project.node.node_id()?, now_ms() as u64)
                .is_err()
        );
        let reservations: u64 = c_store.connection.query_row(
            "SELECT count(*) FROM execution_handoff_reservations",
            [],
            |row| row.get(0),
        )?;
        assert_eq!(reservations > 0, revoke);
        drop(c_store);
        drop(start_guard);
        if !revoke {
            let status =
                task_business_rpc(&mut client, &b, &source, &authorization, &business, false)
                    .await?;
            assert_eq!(status.handoff, wire);
            assert_eq!(
                status.receipt.payload.state,
                ExecutionHandoffState::Succeeded
            );
        }
        drop(client);
        while Arc::strong_count(&c.maintenance) > 1 {
            tokio::task::yield_now().await;
        }
        tasks.abort_all();
        while tasks.join_next().await.is_some() {}
        Ok(())
    }

    #[tokio::test]
    async fn task_business_noise_uses_scoped_source_and_real_operation_receipts() -> Result<()> {
        tokio::time::timeout(
            Duration::from_secs(60),
            Box::pin(task_business_noise_lifecycle(false)),
        )
        .await
        .context("Task business lifecycle timed out")?
    }

    #[tokio::test]
    async fn task_business_noise_rejects_revoked_current_and_preserves_unknown_locks() -> Result<()>
    {
        tokio::time::timeout(
            Duration::from_secs(60),
            Box::pin(task_business_noise_lifecycle(true)),
        )
        .await
        .context("Task business revocation timed out")?
    }

    #[test]
    fn node_policy_reader_sessions_cannot_upgrade_to_other_methods() -> Result<()> {
        let mut purpose = SessionPurpose::Client;
        purpose.select_method("info", false)?;
        purpose.select_method(NODE_POLICY_READ_METHOD, false)?;
        purpose.select_method(NODE_POLICY_READ_METHOD, false)?;
        for method in [
            "info",
            "authenticate",
            "prepare_invocation",
            "rpc",
            "subscribe",
            "identity",
            TASK_BUSINESS_METHOD,
            TASK_RECEIPT_READ_METHOD,
            GATEWAY_SOURCE_METHOD,
            GATEWAY_FORWARD_METHOD,
            "unknown",
        ] {
            assert!(purpose.select_method(method, false).is_err(), "{method}");
        }
        assert!(matches!(purpose, SessionPurpose::NodePolicyRead));
        Ok(())
    }

    #[test]
    fn task_business_sessions_stay_separate_from_client_authorization() -> Result<()> {
        for method in [
            NODE_POLICY_READ_METHOD,
            TASK_BUSINESS_METHOD,
            TASK_RECEIPT_READ_METHOD,
            GATEWAY_SOURCE_METHOD,
            GATEWAY_FORWARD_METHOD,
        ] {
            let mut authenticated = SessionPurpose::Client;
            assert!(authenticated.select_method(method, true).is_err());
            assert!(matches!(authenticated, SessionPurpose::Client));
        }
        let mut purpose = SessionPurpose::Client;
        purpose.select_method(TASK_BUSINESS_METHOD, false)?;
        purpose.select_method(TASK_BUSINESS_METHOD, false)?;
        for method in [
            "authenticate",
            "rpc",
            "identity",
            "subscribe",
            NODE_POLICY_READ_METHOD,
            TASK_RECEIPT_READ_METHOD,
            GATEWAY_SOURCE_METHOD,
            GATEWAY_FORWARD_METHOD,
        ] {
            assert!(purpose.select_method(method, false).is_err(), "{method}");
        }
        assert!(matches!(
            purpose,
            SessionPurpose::TaskBusiness(TaskBusinessPhase::AwaitingBegin)
        ));
        Ok(())
    }

    #[test]
    fn task_receipt_reader_cannot_enter_a_business_or_client_lane() -> Result<()> {
        let mut purpose = SessionPurpose::Client;
        purpose.select_method(TASK_RECEIPT_READ_METHOD, false)?;
        purpose.select_method(TASK_RECEIPT_READ_METHOD, false)?;
        for method in [
            "authenticate",
            "rpc",
            "identity",
            "subscribe",
            NODE_POLICY_READ_METHOD,
            TASK_BUSINESS_METHOD,
            GATEWAY_SOURCE_METHOD,
            GATEWAY_FORWARD_METHOD,
        ] {
            assert!(purpose.select_method(method, false).is_err(), "{method}");
        }
        assert!(matches!(purpose, SessionPurpose::TaskReceiptRead));
        Ok(())
    }

    #[test]
    fn gateway_sessions_cannot_switch_between_source_target_or_other_purposes() -> Result<()> {
        for selected in [GATEWAY_SOURCE_METHOD, GATEWAY_FORWARD_METHOD] {
            let mut purpose = SessionPurpose::Client;
            purpose.select_method(selected, false)?;
            purpose.select_method(selected, false)?;
            for method in [
                "info",
                "authenticate",
                "prepare_invocation",
                "rpc",
                "subscribe",
                "identity",
                NODE_POLICY_READ_METHOD,
                TASK_BUSINESS_METHOD,
                TASK_RECEIPT_READ_METHOD,
                GATEWAY_SOURCE_METHOD,
                GATEWAY_FORWARD_METHOD,
            ] {
                if method != selected {
                    assert!(
                        purpose.select_method(method, false).is_err(),
                        "{selected}: {method}"
                    );
                }
            }
            match selected {
                GATEWAY_SOURCE_METHOD => assert!(matches!(purpose, SessionPurpose::GatewaySource)),
                GATEWAY_FORWARD_METHOD => {
                    assert!(matches!(purpose, SessionPurpose::GatewayForward))
                }
                _ => unreachable!(),
            }
        }
        Ok(())
    }

    #[test]
    fn gateway_carrier_limit_precedes_full_request_parsing() -> Result<()> {
        let oversized_invalid_json = vec![b'!'; MAX_PROOF_BYTES + 1];
        for purpose in [
            SessionPurpose::GatewaySource,
            SessionPurpose::GatewayForward,
        ] {
            assert!(
                parse_session_request(&oversized_invalid_json, &purpose)
                    .err()
                    .unwrap()
                    .to_string()
                    .contains("exceed 64 KiB")
            );
        }
        let padding = "x".repeat(MAX_PROOF_BYTES);
        for method in [GATEWAY_SOURCE_METHOD, GATEWAY_FORWARD_METHOD] {
            let bytes = serde_json::to_vec(&json!({
                "id":"large", "params":{"padding":&padding}, "method":method,
            }))?;
            let error = parse_session_request(&bytes, &SessionPurpose::Client)
                .err()
                .unwrap();
            assert!(error.to_string().contains("exceed 64 KiB"));
        }
        let escaped = format!(
            r#"{{"id":"escaped","params":{{"padding":"{padding}"}},"method":"gateway_\u0073ource"}}"#
        );
        assert!(
            parse_session_request(escaped.as_bytes(), &SessionPurpose::Client)
                .err()
                .unwrap()
                .to_string()
                .contains("exceed 64 KiB")
        );
        let bytes = serde_json::to_vec(&json!({
            "id":"ordinary", "method":"rpc", "params":{"padding":&padding},
        }))?;
        let ordinary = parse_session_request(&bytes, &SessionPurpose::Client)?;
        assert_eq!(ordinary.params["padding"].as_str(), Some(padding.as_str()));
        let gateway = parse_session_request(
            br#"{"id":"small","method":"gateway_source","params":{}}"#,
            &SessionPurpose::Client,
        )?;
        assert_eq!(gateway.method, GATEWAY_SOURCE_METHOD);
        Ok(())
    }

    #[test]
    fn raw_requests_reject_duplicate_keys_at_every_depth() -> Result<()> {
        let request = parse_request(
            br#" { "params": {"targets":[{"target":"a"},{"target":"b"}],"proof":{"issuer":"root","account":"owner"},"positive":18446744073709551615,"negative":-1,"fraction":1.5,"active":true,"empty":null}, "method":"rpc", "id":"read" } "#,
        )?;
        validate_request(&request)?;
        assert_eq!(request.id, "read");
        assert_eq!(request.method, "rpc");
        assert_eq!(
            request.params,
            json!({"targets":[{"target":"a"},{"target":"b"}],"proof":{"issuer":"root","account":"owner"},"positive":u64::MAX,"negative":-1,"fraction":1.5,"active":true,"empty":null})
        );
        for bytes in [
            br#"{"id":"read","method":"rpc","params":{},"params":{}}"#.as_slice(),
            br#"{"id":"read","method":"rpc","params":{"proof":{"issuer":"root","issuer":"root"}}}"#,
            br#"{"id":"read","method":"rpc","params":{"scope":{"account":"owner","account":"other"}}}"#,
            br#"{"id":"read","method":"rpc","params":{"bindings":[{"target":"a","target":"b"}]}}"#,
            br#"{"id":"read","method":"rpc","params":{"proof":{"issuer":"root","\u0069ssuer":"root"}}}"#,
        ] {
            let error = match parse_request(bytes) {
                Ok(_) => panic!("duplicate object key was accepted"),
                Err(error) => error,
            };
            assert!(error.to_string().contains("duplicate JSON object key"));
        }
        Ok(())
    }

    #[test]
    fn encrypted_requests_require_bounded_ids_and_reject_legacy_invocations() {
        let request: Request =
            serde_json::from_value(json!({"id":"read", "method":"rpc", "params":{}})).unwrap();
        assert!(validate_request(&request).is_ok());
        assert!(
            serde_json::from_value::<Request>(
                json!({"id":"read", "method":"rpc", "params":{}, "human":true})
            )
            .is_err()
        );
        let request = Request {
            id: "\n".into(),
            method: "rpc".into(),
            params: json!({}),
            invocation: None,
        };
        assert!(validate_request(&request).is_err());
    }
}
