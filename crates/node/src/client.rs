//! Client-owned keys authenticate the encrypted channel; current membership authorizes each request.

use crate::{
    crypto::{self, LocalIdentity},
    identity::{
        self, AuthorizationView, Genesis, GovernanceAction, GovernanceEvent, HistoricalNode,
        IdentityKind, Member, NetworkState, RootCheckpoint,
        local::{InviteEnvelope, InviteRequest, PairConfirm, PairRequest},
    },
    network::{NetworkSocket, Service},
    runtime::{Access, Runtime},
};
use anyhow::{Context, Result, bail, ensure};
use axum::extract::ws::Message;
use futures_util::{SinkExt, StreamExt};
use rand::{RngCore, rngs::OsRng};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    sync::Arc,
    time::{Duration, Instant},
};
use vibeface_secure::{Channel, Handshake};

const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
const CALL_TIMEOUT: Duration = Duration::from_secs(120);
const IDLE_TIMEOUT: Duration = Duration::from_secs(90);
const HEARTBEAT: Duration = Duration::from_secs(20);

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    id: String,
    method: String,
    #[serde(default)]
    params: Value,
}
#[derive(Serialize)]
struct Response {
    id: String,
    result: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

#[derive(Clone, Serialize)]
struct RecoveryChallenge {
    challenge_id: String,
    network_id: String,
    owner_public_key: String,
    subject_id: String,

    target_peer_id: String,
    genesis_hash: String,
    nonce: String,
    expires_at_ms: i64,
}
struct Recovery {
    challenge: RecoveryChallenge,
    genesis: Genesis,
    authorized: bool,
    state: Option<NetworkState>,
    prepared: Option<RootCheckpoint>,
    authorized_until_ms: Option<i64>,
    minimum_epoch: u64,
}
struct Session {
    subject_id: String,
    public_key: String,
    subscribed: bool,
    recovery: Option<Recovery>,
    observation: Option<crate::runtime::ObservationGuard>,
    source: SessionSource,
}

enum SessionSource {
    Membership,
    LocalSession(String),
}

#[derive(Serialize)]
struct BrowserPrincipal {
    source: &'static str,
    network_id: String,
    subject_id: String,
    epoch: u64,
    can_manage: bool,
}

pub struct Client {
    socket: NetworkSocket,
    channel: Channel,
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
        mut socket: NetworkSocket,
        identity: &LocalIdentity,
        target: &str,
    ) -> Result<Self> {
        let channel = tokio::time::timeout(HANDSHAKE_TIMEOUT, async {
            let mut handshake = secure(Handshake::new(
                true,
                &identity.secret_key_hex,
                Some(target.to_owned()),
                b"vibeface:client:v1",
            ))?;
            send_frame(&mut socket, secure(handshake.write(&[]))?).await?;
            let second = next_binary(&mut socket).await?;
            ensure!(
                secure(handshake.read(&second))?.is_empty(),
                "unexpected client handshake payload"
            );
            send_frame(&mut socket, secure(handshake.write(&[]))?).await?;
            secure(handshake.finish())
        })
        .await
        .context("client handshake timed out")??;
        Ok(Self { socket, channel })
    }
    pub async fn call(&mut self, method: &str, params: Value) -> Result<Value> {
        let id = uuid::Uuid::new_v4().to_string();
        let request = Request {
            id: id.clone(),
            method: method.to_owned(),
            params,
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

pub async fn serve(node: Arc<Runtime>, socket: NetworkSocket) -> Result<()> {
    serve_session(node, socket, None).await
}

pub async fn serve_local(node: Arc<Runtime>, socket: NetworkSocket, cookie: String) -> Result<()> {
    serve_session(node, socket, Some(cookie)).await
}

async fn serve_session(
    node: Arc<Runtime>,
    mut socket: NetworkSocket,
    cookie: Option<String>,
) -> Result<()> {
    let mut local_changes = node.local_access.subscribe();
    let (mut channel, subject_id, public_key) = tokio::time::timeout(HANDSHAKE_TIMEOUT, async {
        let mut handshake = secure(Handshake::new(
            false,
            &node.identity.secret_key_hex,
            None,
            b"vibeface:client:v1",
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
        let subject_id = handshake.remote_id().context("client identity missing")?;
        let public_key = handshake
            .remote_public_key()
            .context("client public key missing")?;
        Ok::<_, anyhow::Error>((secure(handshake.finish())?, subject_id, public_key))
    })
    .await
    .context("client handshake timed out")??;
    let source = if let Some(cookie) = cookie {
        let view = node.membership_state().await?.project()?;
        if view
            .authorize_client(&subject_id)
            .is_ok_and(|member| member.public_key == public_key)
        {
            SessionSource::Membership
        } else {
            let local =
                node.local_access
                    .session(&cookie, &node.identity.peer_id()?, &view.network_id)?;
            ensure!(
                local.subject_id == subject_id && local.public_key == public_key,
                "local browser cookie is bound to another Noise identity"
            );
            SessionSource::LocalSession(cookie)
        }
    } else {
        SessionSource::Membership
    };
    let mut session = Session {
        subject_id,
        public_key,
        subscribed: false,
        recovery: None,
        observation: None,
        source,
    };
    let mut events = node.events.subscribe();
    let mut heartbeat = tokio::time::interval(HEARTBEAT);
    heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
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
                            let request: Request = serde_json::from_slice(&plaintext).context("invalid client request")?;
                            validate_request(&request)?;
                            let result = if matches!(session.source, SessionSource::LocalSession(_)) && request.method != "local_access" {
                                tokio::select! {
                                    result = dispatch(&node, &mut session, &request.method, request.params) => result,
                                    _ = local_changes.changed() => bail!("local browser access was revoked"),
                                }
                            } else {
                                dispatch(&node, &mut session, &request.method, request.params).await
                            };
                            let response = match result { Ok(result) => Response { id: request.id, result, error: None }, Err(error) => Response { id: request.id, result: Value::Null, error: Some(error.to_string()) } };
                            send_value(&mut socket, &mut channel, &response).await?;
                        } else { partial_started.get_or_insert_with(Instant::now); }
                    }
                    Message::Ping(_) | Message::Pong(_) => {},
                    _ => bail!("client channel accepts only encrypted binary frames"),
                }
            }
            event = events.recv(), if session.subscribed => {
                let principal = current_principal(&node, &session).await?;
                match event {
                    Ok(event) if event.network_id == principal.network_id => send_value(&mut socket, &mut channel, &json!({"type":"changed","payload":event.payload})).await?,
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => send_value(&mut socket, &mut channel, &json!({"type":"changed"})).await?,
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => return Ok(()),
                    _ => {},
                }
            }
            _ = heartbeat.tick() => {
                ensure!(received.elapsed() < IDLE_TIMEOUT, "client channel idle");
                ensure!(partial_started.is_none_or(|at| at.elapsed() < IDLE_TIMEOUT), "client fragmented message timed out");
                if session.subscribed || session.observation.is_some() { current_principal(&node, &session).await?; }
                if let Some(observation) = &session.observation { observation.refresh(); }
                tokio::time::timeout(HANDSHAKE_TIMEOUT, socket.send(Message::Ping(Vec::new().into()))).await.context("client heartbeat send timed out")??;
            }
            changed = local_changes.changed(), if matches!(session.source, SessionSource::LocalSession(_)) => {
                changed.context("local access state closed")?;
                bail!("local browser access was revoked");
            }
        }
    }
}

async fn dispatch(
    node: &Arc<Runtime>,
    session: &mut Session,
    method: &str,
    params: Value,
) -> Result<Value> {
    if matches!(session.source, SessionSource::LocalSession(_)) {
        ensure!(
            matches!(
                method,
                "info" | "authenticate" | "directory" | "subscribe" | "rpc" | "local_access"
            ),
            "local browser access cannot manage personal-network membership"
        );
        current_principal(node, session).await?;
    }
    let _work = if matches!(
        method,
        "info"
            | "authenticate"
            | "directory"
            | "subscribe"
            | "pair_status"
            | "pair_requests"
            | "recovery_challenge"
            | "recovery_context"
    ) {
        None
    } else {
        Some(node.maintenance.enter()?)
    };
    match method {
        "info" => Ok(
            json!({"initialized":node.has_membership().await?,"peer_id":node.identity.peer_id()?,"public_key":node.identity.public_key_hex()?,"roles":node.config.roles,"version":env!("CARGO_PKG_VERSION"),"connection_entry":connection_entry(node)?}),
        ),
        "authenticate" => {
            let authenticated = authentication(node, session).await?;
            if let Some(network) = params
                .get("network_id")
                .filter(|value| value.as_str().is_some_and(|text| !text.is_empty()))
            {
                ensure!(
                    network == &authenticated["principal"]["network_id"],
                    "client requested another personal network"
                );
            }
            Ok(authenticated)
        }
        "directory" => {
            current_principal(node, session).await?;
            let view = node.membership_state().await?.project()?;
            Ok(json!({"state":node.membership_state().await?,"view":view}))
        }
        "subscribe" => {
            current_principal(node, session).await?;
            session.subscribed = true;
            Ok(json!({"subscribed":true}))
        }
        "rpc" => {
            let principal = current_principal(node, session).await?;
            let mut params = params;
            bind_client_identity(&mut params, &principal.subject_id)?;
            let watched_scope = (params["method"] == "conversation")
                .then(|| params["conversation_id"].as_str().map(String::from))
                .flatten();
            let access = Access {
                network_id: principal.network_id,
                human: true,
                conversation_id: None,
                manager: None,
            };
            let mut result = node.rpc(access.clone(), params).await?;
            if let Some(scope) = watched_scope {
                session.observation.take();
                let targets = result["observation_targets"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|value| value.as_str().map(String::from))
                    .collect();
                match node.observe_conversation(&access, &scope, targets).await {
                    Ok(observation) => session.observation = Some(observation),
                    Err(error) => result["observation_error"] = json!(error.to_string()),
                }
            }
            Ok(result)
        }
        "local_access" => {
            let principal = current_principal(node, session).await?;
            if let Some(enabled) = params.get("enabled") {
                let enabled = enabled.as_bool().context("enabled must be a boolean")?;
                ensure!(
                    principal.can_manage || (principal.source == "local_session" && !enabled),
                    "only a management client can enable local browser access"
                );
                node.local_access.set_enabled(enabled)?;
            }
            Ok(json!({"enabled":node.local_access.enabled()}))
        }
        "prepare_genesis" => {
            ensure!(
                !node.has_membership().await?,
                "personal network already initialized"
            );
            let owner_public_key = text(&params, "owner_public_key")?.to_owned();
            let network_id = crypto::fingerprint(&owner_public_key)?;
            let mut initial_members = vec![
                Member::node(
                    &node.identity,
                    params["node_name"].as_str().unwrap_or("This device"),
                    node.config.roles.clone(),
                )?,
                session_member(
                    session,
                    text(&params, "client_name")?,
                    IdentityKind::Client,
                    vec![],
                    true,
                )?,
            ];
            initial_members.sort_by(|a, b| a.subject_id.cmp(&b.subject_id));
            let genesis = Genesis {
                version: identity::IDENTITY_VERSION,
                network_id,
                owner_public_key,
                initial_members,
                signature: String::new(),
            };
            let signing = signing("identity-genesis", genesis.signing_bytes()?);
            Ok(
                json!({"state":NetworkState{genesis,checkpoint:None,events:vec![]},"signing":signing}),
            )
        }
        "bootstrap" => {
            let state: NetworkState = from_field(&params, "state")?;
            validate_local_node(node, &state, true)?;
            if node.has_membership().await? {
                let current = node.membership_state().await?;
                ensure!(
                    current.genesis == state.genesis
                        && state.checkpoint.is_none()
                        && state.events.is_empty(),
                    "personal network already initialized"
                );
                ensure!(
                    current
                        .project()?
                        .principal(&session.subject_id)?
                        .can_manage,
                    "bootstrap requires its original management identity"
                );
            } else {
                node.store.lock().await.bootstrap_network(
                    state.clone(),
                    text(&params, "capability")?,
                    &session.subject_id,
                )?;
            }
            node.install_membership(state).await?;
            authentication(node, session).await
        }
        "prepare_invite" => {
            let view = manager_view(node, session).await?;
            let capability = random_hex();
            let kind: IdentityKind = from_field(&params, "kind")?;
            let mut roles: Vec<crate::model::NodeRole> = params
                .get("node_roles")
                .map(|v| serde_json::from_value(v.clone()))
                .transpose()?
                .unwrap_or_default();
            sort_roles(&mut roles)?;
            let payload = node.store.lock().await.create_invite(InviteRequest {
                request_id: request_id(&params)?,
                network_id: view.network_id,
                inviter_subject_id: session.subject_id.clone(),
                epoch: view.epoch,
                capability: capability.clone(),
                new_kind: kind,
                node_roles: roles,
                manage: false,
                expires_at_ms: None,
            })?;
            let signing = signing("identity-invite", payload.signing_bytes()?);
            Ok(
                json!({"invite":InviteEnvelope{payload,inviter_signature:String::new()},"capability":capability,"signing":signing}),
            )
        }
        "commit_invite" => {
            manager_view(node, session).await?;
            let invite: InviteEnvelope = from_field(&params, "invite")?;
            ensure!(
                invite.payload.inviter_subject_id == session.subject_id,
                "invitation signer differs from authenticated client"
            );
            let capability = text(&params, "capability")?;
            ensure!(
                crypto::digest(capability.as_bytes()) == invite.payload.capability_hash,
                "invitation capability mismatch"
            );
            node.store.lock().await.commit_invite(invite.clone())?;
            Ok(json!({"invite":invite,"capability":capability}))
        }
        "prepare_pair_request" => {
            let invite: InviteEnvelope = from_field(&params, "invite")?;
            let state = node.membership_state().await?;
            let view = state.project()?;
            ensure!(
                invite.payload.network_id == view.network_id && invite.payload.epoch == view.epoch,
                "invitation is for another network or epoch"
            );
            let inviter = view.authorize_client(&invite.payload.inviter_subject_id)?;
            ensure!(inviter.manage, "inviter no longer manages this network");
            crypto::verify_payload(
                &inviter.public_key,
                "identity-invite",
                &invite.signing_bytes()?,
                &invite.inviter_signature,
            )?;
            let capability = text(&params, "capability")?.to_owned();
            ensure!(
                crypto::digest(capability.as_bytes()) == invite.payload.capability_hash,
                "invalid invitation capability"
            );
            ensure!(
                invite.payload.expires_at_ms > crate::runtime::now_ms(),
                "invitation expired"
            );
            let member = session_member(
                session,
                text(&params, "name")?,
                invite.payload.new_kind,
                invite.payload.node_roles.clone(),
                false,
            )?;
            let request_id = request_id(&params)?;
            let nonce = crypto::digest(&identity::canonical_json(&(
                "pair-nonce",
                &request_id,
                &invite.payload.invite_id,
                &session.subject_id,
            ))?);
            let request = PairRequest {
                request_id,
                invite_id: invite.payload.invite_id,
                capability,
                member,
                nonce,
                proof: String::new(),
            };
            let signing = signing("identity-pair", request.signing_bytes()?);
            Ok(json!({"request":request,"signing":signing}))
        }
        "pair_request" => {
            node.membership_state().await?;
            let request: PairRequest = from_field(&params, "request")?;
            ensure!(
                request.member.subject_id == session.subject_id
                    && request.member.public_key == session.public_key,
                "pair request is for another channel identity"
            );
            let request_id = request.request_id.clone();
            let review = node.store.lock().await.pair_request(request)?;
            let signing = signing("identity-pair-review", review.signing_bytes()?);
            Ok(json!({"request_id":request_id,"review":review,"signing":signing}))
        }
        "pair_accept" => {
            node.membership_state().await?;
            let request_id = text(&params, "request_id")?;
            let review = node.store.lock().await.lookup_pair(request_id)?;
            ensure!(
                review.new_member.subject_id == session.subject_id
                    && review.new_member.public_key == session.public_key,
                "pair request is for another channel identity"
            );
            let state = node.membership_state().await?;
            if state
                .project()?
                .authorize_member(&session.subject_id)
                .is_ok_and(|member| member == review.new_member)
            {
                return Ok(json!({"status":"granted"}));
            }
            let receipt = node
                .store
                .lock()
                .await
                .accept_pair(review, text(&params, "signature")?.to_owned())?;
            let _ = node.events.send(crate::runtime::Update {
                network_id: state.genesis.network_id,
                payload: json!({"type":"pair_requested"}),
            });
            Ok(serde_json::to_value(receipt)?)
        }
        "pair_requests" => {
            manager_view(node, session).await?;
            Ok(serde_json::to_value(
                node.store.lock().await.pending_pairs(&session.subject_id)?,
            )?)
        }
        "prepare_pair" => {
            let view = manager_view(node, session).await?;
            let pair = node
                .store
                .lock()
                .await
                .pending_pairs(&session.subject_id)?
                .into_iter()
                .find(|pair| pair.request_id == params["request_id"])
                .context("pair request not found")?;
            ensure!(
                !pair.new_subject_signature.is_empty(),
                "waiting for the new device to confirm"
            );
            ensure!(pair.review.epoch == view.epoch, "pair review epoch changed");
            let event = grant_event(&pair.review)?;
            Ok(
                json!({"review":pair.review,"review_signing":signing("identity-pair-review",pair.review.signing_bytes()?),"event":event,"signing":signing("identity-governance",event.signing_bytes()?)}),
            )
        }
        "pair_confirm" => {
            manager_view(node, session).await?;
            let event: GovernanceEvent = from_field(&params, "event")?;
            ensure!(
                event.issuer == session.subject_id,
                "pair grant signer differs from authenticated client"
            );
            let current = node.membership_state().await?;
            if current.events.iter().any(|existing| existing == &event) {
                node.install_membership(current).await?;
                return authentication(node, session).await;
            }
            let pair = node
                .store
                .lock()
                .await
                .pending_pairs(&session.subject_id)?
                .into_iter()
                .find(|pair| pair.request_id == params["request_id"])
                .context("pair request not found")?;
            let mut expected = grant_event(&pair.review)?;
            expected.signature = event.signature.clone();
            ensure!(
                event == expected && event.issuer == session.subject_id,
                "grant differs from approved pair review"
            );
            node.store.lock().await.confirm_pair(PairConfirm {
                review: pair.review,
                inviter_signature: text(&params, "inviter_signature")?.to_owned(),
                new_subject_signature: pair.new_subject_signature,
            })?;
            let state = node.store.lock().await.store_signed_grant(event)?;
            let network_id = state.genesis.network_id.clone();
            node.install_membership(state.clone()).await?;
            announce_membership(node, &network_id, &state).await;
            authentication(node, session).await
        }
        "pair_status" => {
            node.membership_state().await?;
            let request_id = text(&params, "request_id")?;
            let review = match node.store.lock().await.lookup_pair(request_id) {
                Ok(review) => review,
                Err(error)
                    if error
                        .downcast_ref::<rusqlite::Error>()
                        .is_some_and(|error| {
                            matches!(error, rusqlite::Error::QueryReturnedNoRows)
                        }) =>
                {
                    return Ok(json!({"status":"not_found"}));
                }
                Err(error) => return Err(error),
            };
            ensure!(
                review.new_member.subject_id == session.subject_id
                    && review.new_member.public_key == session.public_key,
                "pair request is for another channel identity"
            );
            let state = node.membership_state().await?;
            if state
                .project()?
                .authorize_member(&session.subject_id)
                .is_ok_and(|member| member == review.new_member)
            {
                Ok(json!({"status":"granted","state":state}))
            } else {
                ensure!(
                    review.expires_at_ms > crate::runtime::now_ms(),
                    "pair review expired"
                );
                Ok(json!({"status":"pending","request_id":request_id,"review":review}))
            }
        }
        "prepare_revoke" => {
            let view = manager_view(node, session).await?;
            let subject_id = text(&params, "subject_id")?.to_owned();
            ensure!(
                !view.authorize_member(&subject_id)?.manage,
                "management-client changes require a root checkpoint"
            );
            let event = GovernanceEvent {
                version: identity::IDENTITY_VERSION,
                network_id: view.network_id,
                epoch: view.epoch,
                parents: view.heads,
                issuer: session.subject_id.clone(),
                action: GovernanceAction::Revoke { subject_id },
                signature: String::new(),
            };
            Ok(
                json!({"event":event,"signing":signing("identity-governance",event.signing_bytes()?)}),
            )
        }
        "revoke" => {
            manager_view(node, session).await?;
            let event: GovernanceEvent = from_field(&params, "event")?;
            ensure!(
                event.issuer == session.subject_id
                    && matches!(event.action, GovernanceAction::Revoke { .. }),
                "invalid revoke signer or action"
            );
            let mut state = node.membership_state().await?;
            state.append(event)?;
            let network_id = state.genesis.network_id.clone();
            node.install_membership(state.clone()).await?;
            announce_membership(node, &network_id, &state).await;
            authentication(node, session).await
        }
        "recovery_challenge" => recovery_challenge(node, session, &params).await,
        "recovery_context" => recovery_context(node, session, &params).await,
        "prepare_recovery" => {
            let latest = if node.has_membership().await? {
                Some(node.membership_state().await?)
            } else {
                None
            };
            let recovery = authorized_recovery(session, &params)?;
            if let Some(latest) = latest {
                ensure!(
                    latest.genesis == recovery.genesis,
                    "recovery network changed"
                );
                recovery.state = Some(latest);
            }
            let state = recovery
                .state
                .as_ref()
                .context("recovery context is required")?;
            let mut members: Vec<Member> = from_field(&params, "members")?;
            for member in &members {
                member.validate()?;
            }
            members.sort_by(|a, b| a.subject_id.cmp(&b.subject_id));
            let historical_nodes = recovery_history(state, &members)?;
            let minimum_epoch = recovery
                .minimum_epoch
                .checked_add(1)
                .context("recovery epoch overflow")?;
            let checkpoint = RootCheckpoint {
                version: identity::IDENTITY_VERSION,
                network_id: state.genesis.network_id.clone(),
                epoch: state
                    .epoch()
                    .checked_add(1)
                    .context("recovery epoch overflow")?
                    .max(minimum_epoch),
                members,
                historical_nodes,
                signature: String::new(),
            };
            let signing = signing("identity-recovery", checkpoint.signing_bytes()?);
            recovery.prepared = Some(checkpoint.clone());
            Ok(json!({"checkpoint":checkpoint,"signing":signing}))
        }
        "recover" => {
            let checkpoint: RootCheckpoint = from_field(&params, "checkpoint")?;
            let recovery = authorized_recovery(session, &params)?;
            let mut expected = recovery
                .prepared
                .clone()
                .context("prepare recovery before signing")?;
            expected.signature = checkpoint.signature.clone();
            ensure!(
                checkpoint == expected,
                "checkpoint differs from the confirmed recovery template"
            );
            let mut state = recovery
                .state
                .clone()
                .context("recovery context is required")?;
            state.recover(checkpoint)?;
            if node.has_membership().await? {
                let current = node.membership_state().await?;
                let prior = recovery
                    .state
                    .as_ref()
                    .context("recovery context is required")?;
                ensure!(
                    current.genesis == prior.genesis && current.project()? == prior.project()?,
                    "membership changed; review recovery again"
                );
            }
            let fresh = !node.has_membership().await?;
            validate_local_node(node, &state, fresh)?;
            if fresh {
                node.store.lock().await.bootstrap_recovered_network(
                    state.clone(),
                    text(&params, "capability")?,
                    &session.subject_id,
                )?;
            }
            node.install_membership(state).await?;
            session.recovery = None;
            authentication(node, session).await
        }
        _ => bail!("unknown client method"),
    }
}

async fn current_view(node: &Runtime, session: &Session) -> Result<AuthorizationView> {
    ensure!(
        matches!(session.source, SessionSource::Membership),
        "signed membership is required"
    );
    let view = node.membership_state().await?.project()?;
    let member = view.authorize_client(&session.subject_id)?;
    ensure!(
        member.public_key == session.public_key,
        "client identity key mismatch"
    );
    Ok(view)
}
fn connection_entry(node: &Runtime) -> Result<Value> {
    let relay = node.config.network.bootstrap_urls.first();
    let mut url = reqwest::Url::parse(relay.unwrap_or(&node.config.public_url))?;
    let scheme = match url.scheme() {
        "ws" => "http",
        "wss" => "https",
        scheme => scheme,
    }
    .to_owned();
    url.set_scheme(&scheme)
        .map_err(|_| anyhow::anyhow!("invalid public browser entry"))?;
    Ok(
        json!({"address":url.origin().ascii_serialization(),"peer_id":node.identity.peer_id()?,"relay":relay.is_some()}),
    )
}
async fn current_principal(node: &Runtime, session: &Session) -> Result<BrowserPrincipal> {
    match &session.source {
        SessionSource::Membership => {
            let principal = current_view(node, session)
                .await?
                .principal(&session.subject_id)?;
            Ok(BrowserPrincipal {
                source: "membership",
                network_id: principal.network_id,
                subject_id: principal.subject_id,
                epoch: principal.epoch,
                can_manage: principal.can_manage,
            })
        }
        SessionSource::LocalSession(cookie) => {
            let view = node.membership_state().await?.project()?;
            let peer_id = node.identity.peer_id()?;
            let member = view.authorize_node(&peer_id, None)?;
            ensure!(
                member.public_key == node.identity.public_key_hex()?,
                "local node membership key changed"
            );
            let local = node
                .local_access
                .session(cookie, &peer_id, &view.network_id)?;
            ensure!(
                local.subject_id == session.subject_id && local.public_key == session.public_key,
                "local browser session belongs to another Noise identity"
            );
            Ok(BrowserPrincipal {
                source: "local_session",
                network_id: local.network_id,
                subject_id: local.subject_id,
                epoch: view.epoch,
                can_manage: false,
            })
        }
    }
}
async fn manager_view(node: &Runtime, session: &Session) -> Result<AuthorizationView> {
    let view = current_view(node, session).await?;
    ensure!(
        view.principal(&session.subject_id)?.can_manage,
        "management permission required"
    );
    Ok(view)
}
async fn authentication(node: &Runtime, session: &Session) -> Result<Value> {
    let principal = current_principal(node, session).await?;
    let state = node.membership_state().await?;
    let view = state.project()?;
    ensure!(
        principal.network_id == view.network_id && principal.epoch == view.epoch,
        "personal-network membership changed during authentication"
    );
    Ok(
        json!({"principal":principal,"state":state,"view":view,"local_auto_access":node.local_access.enabled()}),
    )
}
fn session_member(
    session: &Session,
    name: &str,
    kind: IdentityKind,
    node_roles: Vec<crate::model::NodeRole>,
    manage: bool,
) -> Result<Member> {
    let member = Member {
        subject_id: session.subject_id.clone(),
        public_key: session.public_key.clone(),
        kind,
        name: name.to_owned(),
        node_roles,
        manage,
    };
    member.validate()?;
    Ok(member)
}
fn validate_local_node(node: &Runtime, state: &NetworkState, required: bool) -> Result<()> {
    let view = state.project()?;
    let local = view.authorize_node(&node.identity.peer_id()?, None);
    if required || local.is_ok() {
        let member = local?;
        ensure!(
            member.public_key == node.identity.public_key_hex()?
                && member
                    .node_roles
                    .iter()
                    .all(|role| node.config.roles.contains(role)),
            "local device authorization exceeds configured roles"
        );
    }
    Ok(())
}
fn recovery_history(state: &NetworkState, members: &[Member]) -> Result<Vec<HistoricalNode>> {
    let mut history: std::collections::BTreeMap<String, HistoricalNode> = state
        .historical_nodes()?
        .into_iter()
        .map(|node| (node.subject_id.clone(), node))
        .collect();
    for member in members
        .iter()
        .filter(|member| member.kind == IdentityKind::Node)
    {
        let node = HistoricalNode {
            subject_id: member.subject_id.clone(),
            public_key: member.public_key.clone(),
        };
        if let Some(previous) = history.insert(node.subject_id.clone(), node.clone()) {
            ensure!(previous == node, "historical device key conflict");
        }
    }
    Ok(history.into_values().collect())
}

fn grant_event(review: &crate::identity::local::PairReview) -> Result<GovernanceEvent> {
    ensure!(
        !review.new_member.manage,
        "management clients require a root checkpoint"
    );
    Ok(GovernanceEvent {
        version: identity::IDENTITY_VERSION,
        network_id: review.network_id.clone(),
        epoch: review.epoch,
        parents: review.parents.clone(),
        issuer: review.inviter_subject_id.clone(),
        action: GovernanceAction::Grant {
            member: review.new_member.clone(),
        },
        signature: String::new(),
    })
}

async fn announce_membership(node: &Runtime, network_id: &str, state: &NetworkState) {
    let _ = node
        .hub
        .broadcast(network_id, json!({"type":"trust","state":state}))
        .await;
}
async fn recovery_challenge(
    node: &Runtime,
    session: &mut Session,
    params: &Value,
) -> Result<Value> {
    let network_id = text(params, "network_id")?.to_owned();
    let owner_public_key = text(params, "owner_public_key")?.to_owned();
    ensure!(
        crypto::fingerprint(&owner_public_key)? == network_id,
        "recovery network ID mismatch"
    );
    let genesis = if node.has_membership().await? {
        node.membership_state().await?.genesis
    } else {
        from_field(params, "genesis")?
    };
    genesis.verify()?;
    ensure!(
        genesis.network_id == network_id && genesis.owner_public_key == owner_public_key,
        "recovery root differs from pinned network"
    );
    let challenge = RecoveryChallenge {
        challenge_id: uuid::Uuid::new_v4().to_string(),
        network_id,
        owner_public_key,
        subject_id: session.subject_id.clone(),
        target_peer_id: node.identity.peer_id()?,
        genesis_hash: crypto::digest(&identity::canonical_json(&genesis)?),
        nonce: random_hex(),
        expires_at_ms: crate::runtime::now_ms() + 90_000,
    };
    let signing = signing(
        "identity-recovery-access",
        identity::canonical_json(&challenge)?,
    );
    session.recovery = Some(Recovery {
        challenge: challenge.clone(),
        genesis,
        authorized: false,
        state: None,
        prepared: None,
        authorized_until_ms: None,
        minimum_epoch: 1,
    });
    Ok(json!({"challenge_id":challenge.challenge_id,"challenge":challenge,"signing":signing}))
}
async fn recovery_context(node: &Runtime, session: &mut Session, params: &Value) -> Result<Value> {
    let recovery = session
        .recovery
        .as_mut()
        .context("recovery challenge is required")?;
    check_recovery(recovery, params)?;
    crypto::verify_payload(
        &recovery.challenge.owner_public_key,
        "identity-recovery-access",
        &identity::canonical_json(&recovery.challenge)?,
        text(params, "signature")?,
    )?;
    let state = if node.has_membership().await? {
        node.membership_state().await?
    } else {
        node.store
            .lock()
            .await
            .verify_bootstrap_capability(text(params, "capability")?)?;
        from_field(params, "state")?
    };
    ensure!(
        state.genesis == recovery.genesis,
        "recovery state belongs to another genesis"
    );
    recovery.minimum_epoch = params["recovery_epoch"].as_u64().unwrap_or(1);
    let view = state.project()?;
    recovery.authorized = true;
    recovery.authorized_until_ms = Some(crate::runtime::now_ms() + 600_000);
    recovery.state = Some(state.clone());
    Ok(json!({"state":state,"view":view}))
}
fn check_recovery(recovery: &Recovery, params: &Value) -> Result<()> {
    ensure!(
        text(params, "challenge_id")? == recovery.challenge.challenge_id
            && crate::runtime::now_ms()
                < recovery
                    .authorized_until_ms
                    .unwrap_or(recovery.challenge.expires_at_ms),
        "recovery challenge expired or mismatched"
    );
    Ok(())
}
fn authorized_recovery<'a>(session: &'a mut Session, params: &Value) -> Result<&'a mut Recovery> {
    let recovery = session
        .recovery
        .as_mut()
        .context("recovery challenge is required")?;
    check_recovery(recovery, params)?;
    ensure!(recovery.authorized, "prove root ownership before recovery");
    Ok(recovery)
}
fn bind_client_identity(input: &mut Value, subject: &str) -> Result<()> {
    if let Some(client) = input.get("client_id") {
        ensure!(
            client.as_str() == Some(subject),
            "client_id differs from authenticated client"
        );
    }
    if matches!(
        input["method"].as_str(),
        Some("message" | "presence" | "notifications" | "ack")
    ) {
        input
            .as_object_mut()
            .context("RPC input must be an object")?
            .insert("client_id".to_owned(), Value::String(subject.to_owned()));
    }
    Ok(())
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

fn signing(domain: &str, payload: Vec<u8>) -> Value {
    json!({"domain":domain,"payload":String::from_utf8(payload).expect("canonical JSON is UTF-8")})
}
fn text<'a>(value: &'a Value, key: &str) -> Result<&'a str> {
    let text = value[key]
        .as_str()
        .with_context(|| format!("{key} is required"))?;
    ensure!(
        !text.is_empty() && text.len() <= 2048 && !text.chars().any(char::is_control),
        "invalid {key}"
    );
    Ok(text)
}
fn from_field<T: serde::de::DeserializeOwned>(value: &Value, key: &str) -> Result<T> {
    serde_json::from_value(
        value
            .get(key)
            .cloned()
            .with_context(|| format!("{key} is required"))?,
    )
    .with_context(|| format!("invalid {key}"))
}
fn request_id(params: &Value) -> Result<String> {
    Ok(match params.get("request_id") {
        Some(_) => text(params, "request_id")?.to_owned(),
        None => uuid::Uuid::new_v4().to_string(),
    })
}
fn sort_roles(roles: &mut [crate::model::NodeRole]) -> Result<()> {
    roles.sort_by_key(|role| serde_json::to_string(role).unwrap_or_default());
    ensure!(
        !roles.windows(2).any(|r| r[0] == r[1]),
        "duplicate node roles"
    );
    Ok(())
}
fn random_hex() -> String {
    let mut bytes = [0u8; 32];
    OsRng.fill_bytes(&mut bytes);
    hex::encode(bytes)
}
fn secure<T>(result: std::result::Result<T, String>) -> Result<T> {
    result.map_err(anyhow::Error::msg)
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
    async fn local_browser_access_requires_a_launcher_and_preserves_membership_permissions()
    -> Result<()> {
        use tokio_tungstenite::tungstenite::client::IntoClientRequest;

        async fn connect_local(
            origin: &str,
            target: &str,
            cookie: &str,
            identity: &LocalIdentity,
        ) -> Result<Client> {
            let endpoint = origin.replace("http://", "ws://") + "/api/local/ws";
            let mut request = endpoint.into_client_request()?;
            request.headers_mut().insert("cookie", cookie.parse()?);
            let (socket, _) = tokio_tungstenite::connect_async(request).await?;
            Client::from_socket(NetworkSocket::Client(Box::new(socket)), identity, target).await
        }

        let dir =
            std::env::temp_dir().join(format!("vibeface-local-browser-{}", uuid::Uuid::new_v4()));
        crate::config::private_dir(&dir)?;
        let reservation = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let listen = reservation.local_addr()?;
        drop(reservation);
        let configuration = crate::config::Config::local(listen, vec![], false);
        configuration.save(&dir)?;
        crate::config::private_write(
            &dir.join("identity.json"),
            &serde_json::to_vec(&LocalIdentity::generate())?,
        )?;
        crate::config::private_write(&dir.join("data-epoch.json"), br#"{"data_epoch":5}"#)?;
        let node = Arc::new(Runtime::open(&dir).await?);
        let root = LocalIdentity::generate();
        let manager = LocalIdentity::generate();
        let browser = LocalIdentity::generate();
        node.install_membership(NetworkState::new(Genesis::create(
            &root,
            vec![
                Member::node(&node.identity, "local device", node.config.roles.clone())?,
                Member::client(&manager, "management browser", true)?,
            ],
        )?)?)
        .await?;
        let serving = node.clone();
        let server = tokio::spawn(async move { crate::api::serve(serving).await });
        let result: Result<()> = async {
            tokio::time::timeout(Duration::from_secs(5), async {
                while !dir.join("local-access.json").exists() { tokio::time::sleep(Duration::from_millis(10)).await; }
            }).await?;
            let launcher: crate::local_access::Launcher = serde_json::from_slice(&std::fs::read(dir.join("local-access.json"))?)?;
            let http = reqwest::Client::builder().timeout(Duration::from_secs(5)).build()?;
            assert_eq!(http.get(format!("http://{listen}/api/local/info")).header("Forwarded", "for=127.0.0.1;host=localhost").send().await?.status(), 404);
            let info: Value = http.get(format!("{}/api/local/info", launcher.origin)).send().await?.error_for_status()?.json().await?;
            assert_eq!(info["has_session"], false);
            let denied = http.post(format!("{}/api/local/launch", launcher.origin)).header("Origin", &launcher.origin).header("X-Forwarded-For", "127.0.0.1").json(&json!({})).send().await?;
            assert_eq!(denied.status(), 401);
            assert!(!denied.headers().contains_key("set-cookie"));
            assert_eq!(http.get(format!("{}/api/local/ws", launcher.origin)).header("Connection", "Upgrade").header("Upgrade", "websocket").header("Sec-WebSocket-Version", "13").header("Sec-WebSocket-Key", "dGhlIHNhbXBsZSBub25jZQ==").send().await?.status(), 401);

            let mut cookies = vec![];
            for identity in [&browser, &manager] {
                let launch: Value = http.post(format!("{}/api/local/launch", launcher.origin)).bearer_auth(&launcher.capability).json(&json!({})).send().await?.error_for_status()?.json().await?;
                let input = json!({"token":launch["token"],"peer_id":launcher.peer_id,"subject_id":identity.peer_id()?,"public_key":identity.public_key_hex()?});
                let exchanged = http.post(format!("{}/api/local/exchange", launcher.origin)).header("Origin", &launcher.origin).json(&input).send().await?.error_for_status()?;
                let cookie = exchanged.headers().get("set-cookie").context("session cookie missing")?.to_str()?;
                assert!(cookie.contains("HttpOnly") && cookie.contains("SameSite=Strict"));
                cookies.push(cookie.split(';').next().context("session cookie missing")?.to_owned());
                assert_eq!(http.post(format!("{}/api/local/exchange", launcher.origin)).header("Origin", &launcher.origin).json(&input).send().await?.status(), 401);
            }
            let mut local = connect_local(&launcher.origin, &launcher.peer_id, &cookies[0], &browser).await?;
            let auth = local.call("authenticate", json!({})).await?;
            assert_eq!(auth["principal"]["source"], "local_session");
            assert_eq!(auth["principal"]["can_manage"], false);
            assert!(!auth["view"]["members"].as_array().context("members missing")?.iter().any(|member| member["subject_id"] == browser.peer_id().unwrap()));
            assert!(local.call("prepare_invite", json!({"kind":"client"})).await.is_err());
            let conversation = uuid::Uuid::new_v4().to_string();
            local.call("rpc", json!({"method":"create_conversation","conversation_id":conversation,"title":"Local browser"})).await?;
            let conversations = local.call("rpc", json!({"method":"conversations"})).await?;
            assert!(conversations["conversations"].as_array().context("conversations missing")?.iter().any(|item| item["conversation_id"] == conversation && item["title"] == "Local browser"));
            let mut wrong_key = connect_local(&launcher.origin, &launcher.peer_id, &cookies[0], &LocalIdentity::generate()).await?;
            assert!(wrong_key.call("authenticate", json!({})).await.is_err());
            let mut signed = connect_local(&launcher.origin, &launcher.peer_id, &cookies[1], &manager).await?;
            let auth = signed.call("authenticate", json!({})).await?;
            assert_eq!(auth["principal"]["source"], "membership");
            assert_eq!(auth["principal"]["can_manage"], true);
            local.call("local_access", json!({"enabled":false})).await?;
            assert!(!crate::config::Config::load(&dir)?.local_auto_access);
            assert!(local.call("rpc", json!({"method":"conversations"})).await.is_err());
            assert_eq!(signed.call("authenticate", json!({})).await?["principal"]["can_manage"], true);
            signed.call("local_access", json!({"enabled":true})).await?;
            assert_eq!(http.get(format!("{}/api/local/ws", launcher.origin)).header("Cookie", &cookies[0]).header("Connection", "Upgrade").header("Upgrade", "websocket").header("Sec-WebSocket-Version", "13").header("Sec-WebSocket-Key", "dGhlIHNhbXBsZSBub25jZQ==").send().await?.status(), 401);
            Ok(())
        }.await;
        server.abort();
        let _ = server.await;
        drop(node);
        std::fs::remove_dir_all(&dir)?;
        result
    }

    #[tokio::test]
    async fn client_route_is_explicit_and_pins_the_destination() -> Result<()> {
        use tokio_tungstenite::tungstenite::Message as NativeMessage;
        for relay in [false, true] {
            for correct_target in [false, true] {
                let destination = LocalIdentity::generate();
                let target = if correct_target {
                    destination.peer_id()?
                } else {
                    LocalIdentity::generate().peer_id()?
                };
                let expected_target = target.clone();
                let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
                let url = format!("ws://{}/api/network/ws", listener.local_addr()?);
                let server = tokio::spawn(async move {
                    let (stream, _) = listener.accept().await?;
                    let mut socket = tokio_tungstenite::accept_async(stream).await?;
                    socket
                        .send(NativeMessage::Text(
                            json!({"type":"welcome","nonce":"ab".repeat(32)})
                                .to_string()
                                .into(),
                        ))
                        .await?;
                    let control: Value = serde_json::from_str(
                        socket.next().await.context("missing route")??.to_text()?,
                    )?;
                    assert_eq!(
                        control,
                        if relay {
                            json!({"type":"dial","target":expected_target,"service":"client"})
                        } else {
                            json!({"type":"direct","service":"client"})
                        }
                    );
                    socket
                        .send(NativeMessage::Text(
                            json!({"type":"ready"}).to_string().into(),
                        ))
                        .await?;
                    let mut handshake = secure(Handshake::new(
                        false,
                        &destination.secret_key_hex,
                        None,
                        b"vibeface:client:v1",
                    ))?;
                    let first = socket.next().await.context("missing first handshake")??;
                    assert!(secure(handshake.read(&first.into_data()))?.is_empty());
                    socket
                        .send(NativeMessage::Binary(secure(handshake.write(&[]))?.into()))
                        .await?;
                    if correct_target {
                        let third = socket.next().await.context("missing final handshake")??;
                        assert!(secure(handshake.read(&third.into_data()))?.is_empty());
                        secure(handshake.finish())?;
                    }
                    Ok::<_, anyhow::Error>(())
                });
                let client =
                    Client::connect(&url, &LocalIdentity::generate(), &target, relay).await;
                assert_eq!(client.is_ok(), correct_target);
                server.await??;
            }
        }
        Ok(())
    }

    #[test]
    fn recovery_keeps_old_node_keys_and_covers_the_new_local_node() -> Result<()> {
        let root = LocalIdentity::generate();
        let admin = Member::client(&LocalIdentity::generate(), "owner", true)?;
        let old = Member::node(
            &LocalIdentity::generate(),
            "old node",
            vec![crate::model::NodeRole::Replica],
        )?;
        let new = Member::node(
            &LocalIdentity::generate(),
            "new node",
            vec![crate::model::NodeRole::Replica],
        )?;
        let state = NetworkState::new(Genesis::create(&root, vec![admin.clone(), old.clone()])?)?;
        let members = vec![admin, new.clone()];
        let history = recovery_history(&state, &members)?;
        assert!(
            history
                .iter()
                .any(|node| node.subject_id == old.subject_id && node.public_key == old.public_key)
        );
        assert!(
            history
                .iter()
                .any(|node| node.subject_id == new.subject_id && node.public_key == new.public_key)
        );
        assert!(
            history
                .windows(2)
                .all(|nodes| nodes[0].subject_id < nodes[1].subject_id)
        );
        assert!(
            !members
                .iter()
                .any(|member| member.subject_id == old.subject_id)
        );
        Ok(())
    }

    #[test]
    fn rpc_client_identity_is_bound_to_the_noise_principal() {
        let mut input = json!({"method":"notifications"});
        bind_client_identity(&mut input, "actual-client").unwrap();
        assert_eq!(input["client_id"], "actual-client");
        assert!(
            bind_client_identity(
                &mut json!({"method":"ack","client_id":"another-client"}),
                "actual-client"
            )
            .is_err()
        );
        assert!(bind_client_identity(&mut json!({"method":"operate","request":{"action":{"response":{"client_id":"business-data"}}}}), "actual-client").is_ok());
        assert!(
            bind_client_identity(
                &mut json!({"method":"message","client_id":"actual-client"}),
                "actual-client"
            )
            .is_ok()
        );
    }

    #[test]
    fn channel_requests_require_bounded_ids_and_object_params() {
        let mut request = Request {
            id: "valid-request".into(),
            method: "info".into(),
            params: json!({}),
        };
        assert!(validate_request(&request).is_ok());
        request.params = json!("untyped payload");
        assert!(validate_request(&request).is_err());
        request.params = json!({});
        request.id = "\n".into();
        assert!(validate_request(&request).is_err());
    }
}
