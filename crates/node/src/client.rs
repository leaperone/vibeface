//! Client-owned keys authenticate the encrypted channel; current membership authorizes each request.

use crate::{
    crypto::{self, LocalIdentity},
    identity::{
        self, AuthorizationView, Genesis, GovernanceAction, GovernanceEvent, IdentityKind, Member,
        NetworkState, RootCheckpoint,
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
    target_node_id: String,
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
}
struct Session {
    subject_id: String,
    public_key: String,
    subscribed: bool,
    recovery: Option<Recovery>,
}

pub struct Client {
    socket: NetworkSocket,
    channel: Channel,
}
impl Client {
    pub async fn connect(url: &str, identity: &LocalIdentity, target: &str) -> Result<Self> {
        let endpoint = client_endpoint(url)?;
        if let Ok(socket) = crate::network::connect_direct(&endpoint, Service::Client).await {
            if let Ok(client) = Self::from_socket(socket, identity, target).await {
                return Ok(client);
            }
        }
        let socket = crate::network::connect_relay(&endpoint, target, Service::Client).await?;
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

pub async fn serve(node: Arc<Runtime>, mut socket: NetworkSocket) -> Result<()> {
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
    let mut session = Session {
        subject_id,
        public_key,
        subscribed: false,
        recovery: None,
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
                            let result = dispatch(&node, &mut session, &request.method, request.params).await;
                            let response = match result { Ok(result) => Response { id: request.id, result, error: None }, Err(error) => Response { id: request.id, result: Value::Null, error: Some(error.to_string()) } };
                            send_value(&mut socket, &mut channel, &response).await?;
                        } else { partial_started.get_or_insert_with(Instant::now); }
                    }
                    Message::Ping(_) | Message::Pong(_) => {},
                    _ => bail!("client channel accepts only encrypted binary frames"),
                }
            }
            event = events.recv(), if session.subscribed => {
                let principal = current_view(&node, &session).await?.principal(&session.subject_id)?;
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
                if session.subscribed { current_view(&node, &session).await?.principal(&session.subject_id)?; }
                tokio::time::timeout(HANDSHAKE_TIMEOUT, socket.send(Message::Ping(Vec::new().into()))).await.context("client heartbeat send timed out")??;
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
            json!({"initialized":node.has_membership().await?,"node_id":node.identity.node_id()?,"public_key":node.identity.public_key_hex()?,"roles":node.config.roles,"version":env!("CARGO_PKG_VERSION")}),
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
            let view = current_view(node, session).await?;
            view.authorize_client(&session.subject_id)?;
            Ok(json!({"state":node.membership_state().await?,"view":view}))
        }
        "subscribe" => {
            current_view(node, session)
                .await?
                .authorize_client(&session.subject_id)?;
            session.subscribed = true;
            Ok(json!({"subscribed":true}))
        }
        "rpc" => {
            let principal = current_view(node, session)
                .await?
                .principal(&session.subject_id)?;
            let mut params = params;
            bind_client_identity(&mut params, &principal.subject_id)?;
            node.rpc(
                Access {
                    network_id: principal.network_id,
                    human: true,
                    conversation_id: None,
                },
                params,
            )
            .await
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
                node.identities.bootstrap_network(
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
            let mut roles = params
                .get("node_roles")
                .map(|v| serde_json::from_value(v.clone()))
                .transpose()?
                .unwrap_or_default();
            sort_roles(&mut roles)?;
            let payload = node.identities.create_invite(InviteRequest {
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
            node.identities.commit_invite(invite.clone())?;
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
            let review = node.identities.pair_request(request)?;
            let signing = signing("identity-pair-review", review.signing_bytes()?);
            Ok(json!({"request_id":request_id,"review":review,"signing":signing}))
        }
        "pair_accept" => {
            node.membership_state().await?;
            let request_id = text(&params, "request_id")?;
            let review = node.identities.lookup_pair(request_id)?;
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
                .identities
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
                node.identities.pending_pairs(&session.subject_id)?,
            )?)
        }
        "prepare_pair" => {
            let view = manager_view(node, session).await?;
            let pair = node
                .identities
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
                .identities
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
            node.identities.confirm_pair(PairConfirm {
                review: pair.review,
                inviter_signature: text(&params, "inviter_signature")?.to_owned(),
                new_subject_signature: pair.new_subject_signature,
            })?;
            let state = node.identities.store_signed_grant(event)?;
            node.install_membership(state).await?;
            authentication(node, session).await
        }
        "pair_status" => {
            node.membership_state().await?;
            let request_id = text(&params, "request_id")?;
            let review = match node.identities.lookup_pair(request_id) {
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
            node.install_membership(state).await?;
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
            let checkpoint = RootCheckpoint {
                version: identity::IDENTITY_VERSION,
                network_id: state.genesis.network_id.clone(),
                epoch: state
                    .epoch()
                    .checked_add(1)
                    .context("recovery epoch overflow")?,
                members,
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
                node.identities.bootstrap_recovered_network(
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
    let view = node.membership_state().await?.project()?;
    let member = view.authorize_client(&session.subject_id)?;
    ensure!(
        member.public_key == session.public_key,
        "client identity key mismatch"
    );
    Ok(view)
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
    let state = node.membership_state().await?;
    let view = state.project()?;
    let member = view.authorize_client(&session.subject_id)?;
    ensure!(
        member.public_key == session.public_key,
        "client identity key mismatch"
    );
    Ok(json!({"principal":view.principal(&session.subject_id)?,"state":state,"view":view}))
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
    let local = view.authorize_node(&node.identity.node_id()?, None);
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
async fn recovery_challenge(
    node: &Runtime,
    session: &mut Session,
    params: &Value,
) -> Result<Value> {
    let network_id = text(params, "network_id")?.to_owned();
    let owner_public_key = text(params, "owner_public_key")?.to_owned();
    ensure!(
        crypto::fingerprint(&owner_public_key)? == network_id,
        "recovery root fingerprint mismatch"
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
        target_node_id: node.identity.node_id()?,
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
        node.identities
            .verify_bootstrap_capability(text(params, "capability")?)?;
        from_field(params, "state")?
    };
    ensure!(
        state.genesis == recovery.genesis,
        "recovery state belongs to another genesis"
    );
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
