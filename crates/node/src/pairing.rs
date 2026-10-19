//! Resumable enrollment of this Node's own identity into one cluster.

use crate::{
    client::Client,
    config::{self, Config},
    crypto::digest,
    identity::{
        ClusterState, IDENTITY_VERSION, IdentityKind, Member,
        local::{InviteEnvelope, PairRequest, PairReview},
    },
    runtime::Runtime,
};
use anyhow::{Context, Result, bail, ensure};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{fs, io::Read, path::PathBuf, sync::Arc};
use tokio::sync::Mutex;

#[derive(Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Invitation {
    url: String,
    peer_id: String,
    invite: InviteSecret,
}

#[derive(Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct InviteSecret {
    invite: InviteEnvelope,
    capability: String,
}

impl Invitation {
    pub fn parse(input: &str) -> Result<Self> {
        let input = input.trim();
        ensure!(input.len() <= 192 * 1024, "invitation is too large");
        if input.starts_with('{') {
            return Self::from_bytes(input.as_bytes());
        }
        let bytes = URL_SAFE_NO_PAD
            .decode(input)
            .context("invalid base64url invitation")?;
        Self::from_bytes(&bytes)
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<Self> {
        ensure!(bytes.len() <= 128 * 1024, "invitation is too large");
        let mut invitation: Self =
            serde_json::from_slice(bytes).context("invalid Node invitation")?;
        ensure!(
            invitation.invite.invite.payload.version == IDENTITY_VERSION,
            "unsupported invitation version"
        );
        ensure!(
            invitation.peer_id.len() == 64 && hex::decode(&invitation.peer_id).is_ok(),
            "invitation requires a pinned Node identity"
        );
        ensure!(
            invitation.invite.invite.payload.new_kind == IdentityKind::Node,
            "invitation must authorize a Node"
        );
        ensure!(
            digest(invitation.invite.capability.as_bytes())
                == invitation.invite.invite.payload.capability_hash,
            "invitation capability mismatch"
        );
        invitation.url = network_url(&invitation.url)?;
        Ok(invitation)
    }
}

pub fn network_url(value: &str) -> Result<String> {
    ensure!(value.len() <= 2048, "network URL is too long");
    let mut url = reqwest::Url::parse(value).context("invalid network URL")?;
    let network_endpoint = matches!(url.scheme(), "ws" | "wss");
    ensure!(
        url.host_str().is_some()
            && url.username().is_empty()
            && url.password().is_none()
            && url.query().is_none()
            && url.fragment().is_none()
            && (network_endpoint || matches!(url.path(), "/" | "/api/network/ws")),
        "network URL must identify the Node WebSocket endpoint without credentials"
    );
    let scheme = match url.scheme() {
        "https" | "wss" => "wss",
        "http" | "ws" => "ws",
        _ => bail!("unsupported network URL scheme"),
    };
    url.set_scheme(scheme)
        .map_err(|_| anyhow::anyhow!("invalid network URL scheme"))?;
    if !network_endpoint {
        url.set_path("/api/network/ws");
    }
    Ok(url.to_string())
}

#[derive(Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum Phase {
    #[default]
    Created,
    Requesting,
    Reviewing,
    Accepting,
    Accepted,
    Expired,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct PendingPair {
    url: String,
    peer_id: String,
    invite_id: String,
    request_id: String,
    name: String,
    #[serde(default)]
    request_hash: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    invitation: Option<Invitation>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    review: Option<PairReview>,
    #[serde(default)]
    phase: Phase,
}

#[derive(Debug, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum JoinState {
    Idle,
    Review {
        request_id: String,
        review: PairReview,
        review_hash: String,
    },
    Pending {
        request_id: String,
        review: PairReview,
        review_hash: String,
    },
    Joined {
        cluster_id: String,
        peer_id: String,
    },
    Uncertain {
        request_id: String,
        message: &'static str,
    },
}

pub struct NodePairing {
    node: Arc<Runtime>,
    pending_path: PathBuf,
    serial: Mutex<()>,
}

impl NodePairing {
    pub fn new(node: Arc<Runtime>) -> Self {
        let pending_path = node.data_dir.join("pairing.pending.json");
        Self {
            node,
            pending_path,
            serial: Mutex::new(()),
        }
    }

    fn load(&self) -> Result<Option<PendingPair>> {
        let file = match fs::File::open(&self.pending_path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        let mut bytes = Vec::new();
        file.take(256 * 1024 + 1).read_to_end(&mut bytes)?;
        ensure!(
            bytes.len() <= 256 * 1024,
            "pending pairing receipt is too large"
        );
        Ok(Some(
            serde_json::from_slice(&bytes).context("invalid pending pairing receipt")?,
        ))
    }

    fn save(&self, pending: &PendingPair) -> Result<()> {
        config::private_write(&self.pending_path, &serde_json::to_vec(pending)?)
    }

    fn member(&self, pending: &PendingPair) -> Result<Member> {
        let invitation = pending
            .invitation
            .as_ref()
            .context("resume pairing with its original invitation")?;
        let payload = &invitation.invite.invite.payload;
        ensure!(
            !payload.manage
                && payload
                    .node_roles
                    .iter()
                    .all(|role| self.node.config.roles.contains(role)),
            "invitation exceeds this Node's configured capabilities"
        );
        Member::node(
            &self.node.identity,
            &pending.name,
            payload.node_roles.clone(),
        )
    }

    pub async fn request(&self, invitation: Invitation, name: &str) -> Result<JoinState> {
        let _serial = self.serial.lock().await;
        let mut previous = self.load()?;
        if let Some(pending) = previous.as_mut() {
            let same = pending.url == invitation.url
                && pending.peer_id == invitation.peer_id
                && pending.invite_id == invitation.invite.invite.payload.invite_id
                && pending.name == name;
            if !same
                && (pending.phase == Phase::Expired
                    || (pending.phase == Phase::Created && pending.request_hash.is_none()))
            {
                previous = None;
            } else if !same
                && pending.invitation.as_ref().is_some_and(|invitation| {
                    invitation.invite.invite.payload.expires_at_ms <= crate::runtime::now_ms()
                })
            {
                self.reconcile(pending, false).await?;
                if pending.phase == Phase::Expired {
                    previous = None;
                }
            }
        }
        let mut pending = if let Some(mut pending) = previous {
            ensure!(
                pending.url == invitation.url
                    && pending.peer_id == invitation.peer_id
                    && pending.invite_id == invitation.invite.invite.payload.invite_id
                    && pending.name == name,
                "finish the pending pairing with its original invitation and device name"
            );
            if let Some(previous) = &pending.invitation {
                ensure!(previous == &invitation, "pending invitation changed");
            }
            pending.invitation = Some(invitation);
            pending
        } else {
            ensure!(
                !self.node.has_membership().await?,
                "this Node already belongs to a cluster; existing membership was preserved"
            );
            ensure!(
                invitation.invite.invite.payload.expires_at_ms > crate::runtime::now_ms(),
                "invitation expired"
            );
            PendingPair {
                url: invitation.url.clone(),
                peer_id: invitation.peer_id.clone(),
                invite_id: invitation.invite.invite.payload.invite_id.clone(),
                request_id: uuid::Uuid::new_v4().to_string(),
                name: name.into(),
                request_hash: None,
                invitation: Some(invitation),
                review: None,
                phase: Phase::Created,
            }
        };
        self.member(&pending)?;
        self.save(&pending)?;
        self.reconcile(&mut pending, true).await
    }

    pub async fn status(&self) -> Result<JoinState> {
        let _serial = self.serial.lock().await;
        match self.load()? {
            Some(mut pending) => self.reconcile(&mut pending, false).await,
            None if self.node.has_membership().await? => {
                let state = self.node.membership_state().await?;
                state
                    .project()?
                    .authorize_node(&self.node.identity.peer_id()?, None)?;
                Ok(JoinState::Joined {
                    cluster_id: state.genesis.cluster_id,
                    peer_id: self.node.identity.peer_id()?,
                })
            }
            None => Ok(JoinState::Idle),
        }
    }

    pub async fn accept(&self, request_id: &str, review_hash: &str) -> Result<JoinState> {
        let _serial = self.serial.lock().await;
        let Some(mut pending) = self.load()? else {
            bail!("no pending Node pairing");
        };
        ensure!(pending.request_id == request_id, "pairing request changed");
        let review = pending
            .review
            .as_ref()
            .context("pairing review is missing")?;
        ensure!(
            review.hash()? == review_hash,
            "pairing review changed; check it before accepting"
        );
        let current = self.reconcile(&mut pending, false).await?;
        if !matches!(current, JoinState::Review { .. }) {
            return Ok(current);
        }
        let review = pending
            .review
            .as_ref()
            .context("pairing review is missing")?;
        ensure!(
            review.hash()? == review_hash,
            "pairing review changed; check it before accepting"
        );
        ensure!(
            review.expires_at_ms > crate::runtime::now_ms(),
            "pair review expired"
        );
        let signature = self
            .node
            .identity
            .sign_payload("identity-pair-review", &review.signing_bytes()?)?;
        let mut client = match self.connect(&pending).await {
            Ok(client) => client,
            Err(_) => {
                return Ok(Self::uncertain(
                    &pending,
                    "无法连接邀请节点。请检查连接后刷新状态。",
                ));
            }
        };
        pending.phase = Phase::Accepting;
        self.save(&pending)?;
        if client
            .call(
                "pair_accept",
                json!({"request_id":pending.request_id,"signature":signature}),
            )
            .await
            .is_err()
        {
            return Ok(Self::uncertain(
                &pending,
                "加入确认的结果未知。请刷新状态核对，不要重新提交。",
            ));
        }
        pending.phase = Phase::Accepted;
        self.save(&pending)?;
        self.review_state(&pending, true)
    }

    async fn connect(&self, pending: &PendingPair) -> Result<Client> {
        Client::connect(&pending.url, &self.node.identity, &pending.peer_id).await
    }

    fn uncertain(pending: &PendingPair, message: &'static str) -> JoinState {
        JoinState::Uncertain {
            request_id: pending.request_id.clone(),
            message,
        }
    }

    fn review_state(&self, pending: &PendingPair, accepted: bool) -> Result<JoinState> {
        let review = pending
            .review
            .clone()
            .context("pairing review is missing")?;
        let review_hash = review.hash()?;
        let request_id = pending.request_id.clone();
        Ok(if accepted {
            JoinState::Pending {
                request_id,
                review,
                review_hash,
            }
        } else {
            JoinState::Review {
                request_id,
                review,
                review_hash,
            }
        })
    }

    fn check_review(&self, pending: &PendingPair, review: &PairReview) -> Result<()> {
        let invitation = pending
            .invitation
            .as_ref()
            .context("resume pairing with its original invitation")?;
        verify_review(
            review,
            &invitation.invite.invite,
            &self.member(pending)?,
            pending
                .request_hash
                .as_deref()
                .context("pending request hash is missing")?,
        )?;
        ensure!(
            pending
                .review
                .as_ref()
                .is_none_or(|previous| previous == review),
            "pairing review changed; original pending receipt was preserved"
        );
        Ok(())
    }

    async fn reconcile(&self, pending: &mut PendingPair, submit: bool) -> Result<JoinState> {
        let Some(invitation) = pending.invitation.clone() else {
            return Ok(Self::uncertain(
                pending,
                "请使用原邀请和节点名称恢复这次配对。",
            ));
        };
        let member = self.member(pending)?;
        if self.node.has_membership().await? {
            return self
                .finish(pending, self.node.membership_state().await?)
                .await;
        }
        let mut client = match self.connect(pending).await {
            Ok(client) => client,
            Err(_) => {
                return Ok(Self::uncertain(
                    pending,
                    "无法连接邀请节点。请检查连接后刷新状态。",
                ));
            }
        };
        let status = match client
            .call("pair_status", json!({"request_id":pending.request_id}))
            .await
        {
            Ok(status) => status,
            Err(_) => {
                return Ok(Self::uncertain(
                    pending,
                    "尚未取得配对状态。请检查邀请节点后刷新。",
                ));
            }
        };
        match status
            .get("status")
            .and_then(Value::as_str)
            .context("invalid pairing status")?
        {
            "granted" => {
                if !matches!(pending.phase, Phase::Accepting | Phase::Accepted) {
                    let review = match status.get("review") {
                        Some(review) => serde_json::from_value(review.clone())?,
                        None => pending
                            .review
                            .clone()
                            .context("granted pairing review is missing")?,
                    };
                    self.check_review(pending, &review)?;
                    pending.review = Some(review);
                    pending.phase = Phase::Reviewing;
                    self.save(pending)?;
                    return self.review_state(pending, false);
                }
                let state: ClusterState = serde_json::from_value(
                    status
                        .get("state")
                        .context("paired cluster state is missing")?
                        .clone(),
                )?;
                self.finish(pending, state).await
            }
            "pending" => {
                let review: PairReview = serde_json::from_value(
                    status
                        .get("review")
                        .context("pending review missing")?
                        .clone(),
                )?;
                self.check_review(pending, &review)?;
                pending.review = Some(review);
                let accepted = status.get("accepted").and_then(Value::as_bool);
                if (accepted == Some(true)
                    && matches!(pending.phase, Phase::Accepting | Phase::Accepted))
                    || (accepted.is_none() && pending.phase == Phase::Accepted)
                {
                    pending.phase = Phase::Accepted;
                } else if accepted == Some(false) || pending.phase != Phase::Accepting {
                    pending.phase = Phase::Reviewing;
                } else {
                    self.save(pending)?;
                    return Ok(Self::uncertain(
                        pending,
                        "邀请节点未提供确认状态。请升级邀请节点后刷新，保留当前配对。",
                    ));
                }
                self.save(pending)?;
                self.review_state(pending, pending.phase == Phase::Accepted)
            }
            "expired" => {
                let review: PairReview = serde_json::from_value(
                    status
                        .get("review")
                        .context("expired review missing")?
                        .clone(),
                )?;
                self.check_review(pending, &review)?;
                ensure!(
                    review.expires_at_ms <= crate::runtime::now_ms(),
                    "pairing is not expired"
                );
                pending.review = Some(review);
                pending.phase = Phase::Expired;
                self.save(pending)?;
                Ok(Self::uncertain(
                    pending,
                    "邀请已过期。请生成新邀请后重新提交。",
                ))
            }
            "not_found"
                if invitation.invite.invite.payload.expires_at_ms <= crate::runtime::now_ms() =>
            {
                pending.phase = Phase::Expired;
                self.save(pending)?;
                Ok(Self::uncertain(
                    pending,
                    "邀请已过期。请生成新邀请后重新提交。",
                ))
            }
            "not_found"
                if submit
                    && pending.phase != Phase::Accepted
                    && pending.phase != Phase::Accepting =>
            {
                let prepared = client.call("prepare_pair_request", json!({"invite":invitation.invite.invite,"capability":invitation.invite.capability,"name":pending.name,"request_id":pending.request_id})).await;
                let prepared = match prepared {
                    Ok(prepared) => prepared,
                    Err(_) => {
                        return Ok(Self::uncertain(
                            pending,
                            "无法准备配对请求。请检查邀请是否有效后重试原邀请。",
                        ));
                    }
                };
                let mut request: PairRequest = serde_json::from_value(
                    prepared
                        .get("request")
                        .context("pairing request template missing")?
                        .clone(),
                )?;
                ensure!(
                    request.request_id == pending.request_id
                        && request.invite_id == pending.invite_id
                        && request.capability == invitation.invite.capability
                        && request.member == member,
                    "pairing request does not match this device and invitation"
                );
                let signing_bytes = request.signing_bytes()?;
                let request_hash = digest(&signing_bytes);
                ensure!(
                    pending
                        .request_hash
                        .as_ref()
                        .is_none_or(|previous| previous == &request_hash),
                    "pairing request template changed; original pending receipt was preserved"
                );
                pending.request_hash = Some(request_hash);
                pending.phase = Phase::Requesting;
                self.save(pending)?;
                request.proof = self
                    .node
                    .identity
                    .sign_payload("identity-pair", &signing_bytes)?;
                let result = match client
                    .call("pair_request", json!({"request":request}))
                    .await
                {
                    Ok(result) => result,
                    Err(_) => {
                        return Ok(Self::uncertain(
                            pending,
                            "配对申请的结果未知。请刷新状态核对，不要重新提交。",
                        ));
                    }
                };
                let review: PairReview = serde_json::from_value(
                    result
                        .get("review")
                        .context("pairing review missing")?
                        .clone(),
                )?;
                self.check_review(pending, &review)?;
                pending.review = Some(review);
                pending.phase = Phase::Reviewing;
                self.save(pending)?;
                self.review_state(pending, false)
            }
            "not_found" => Ok(Self::uncertain(
                pending,
                "邀请节点尚无这次配对记录。请保留原邀请，检查后再提交原申请。",
            )),
            _ => bail!("unsupported pairing status; pending receipt was preserved"),
        }
    }

    async fn finish(&self, pending: &PendingPair, state: ClusterState) -> Result<JoinState> {
        let invitation = pending
            .invitation
            .as_ref()
            .context("pending invitation is missing")?;
        let view = state.project()?;
        ensure!(
            view.cluster_id == invitation.invite.invite.payload.cluster_id,
            "paired cluster does not match the invitation; existing membership was preserved"
        );
        let member = self.member(pending)?;
        ensure!(
            view.authorize_node(&self.node.identity.peer_id()?, None)? == member,
            "paired cluster grants unexpected local permissions"
        );
        let _config = self.node.config_save.lock().await;
        let dir = self
            .pending_path
            .parent()
            .context("Node data directory is missing")?;
        let mut configuration = Config::load(dir)?;
        let changed = !configuration.network.bootstrap_urls.contains(&pending.url);
        if changed {
            configuration
                .network
                .bootstrap_urls
                .push(pending.url.clone());
            configuration.validate()?;
        }
        configuration.network.validate()?;
        self.node.network.validate_bootstrap(&pending.url).await?;
        let installed = self.node.install_membership(state).await?;
        ensure!(
            installed.authorize_node(&self.node.identity.peer_id()?, None)? == member,
            "installed cluster no longer grants the requested local membership"
        );
        if changed {
            configuration.save(dir)?;
        }
        self.node.network.add_bootstrap(&pending.url).await?;
        fs::remove_file(&self.pending_path)?;
        fs::File::open(dir)?.sync_all()?;
        Ok(JoinState::Joined {
            cluster_id: view.cluster_id,
            peer_id: self.node.identity.peer_id()?,
        })
    }
}

fn verify_review(
    review: &PairReview,
    invite: &InviteEnvelope,
    member: &Member,
    request_hash: &str,
) -> Result<()> {
    let payload = &invite.payload;
    ensure!(
        review.version == payload.version
            && review.invite_id == payload.invite_id
            && review.cluster_id == payload.cluster_id
            && review.inviter_subject_id == payload.inviter_subject_id
            && review.epoch == payload.epoch
            && review.expires_at_ms == payload.expires_at_ms
            && review.proposal_hash == request_hash
            && review.new_member == *member,
        "pairing review does not match the invitation and local device"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        crypto::LocalIdentity,
        identity::{Genesis, GovernanceEvent, NodeRole, local::InviteRequest},
        network::{NetworkConfig, NetworkHub, Service},
    };
    use axum::{
        Router,
        extract::{ConnectInfo, State, WebSocketUpgrade},
        routing::get,
    };
    use std::{net::SocketAddr, path::Path};
    use tokio::{net::TcpListener, task::JoinSet};

    fn directory() -> Result<PathBuf> {
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../.artifacts")
            .join(format!("node-join-{}", uuid::Uuid::new_v4()));
        config::private_dir(&dir)?;
        Ok(dir)
    }

    async fn open_node(dir: &Path, listen: SocketAddr) -> Result<Arc<Runtime>> {
        config::private_dir(dir)?;
        Config::local(listen, vec![], false).save(dir)?;
        config::private_write(&dir.join("data-epoch.json"), br#"{"data_epoch":6}"#)?;
        config::private_write(
            &dir.join("identity.json"),
            &serde_json::to_vec(&LocalIdentity::generate())?,
        )?;
        Ok(Arc::new(Runtime::open(dir).await?))
    }

    async fn host(
        node: Arc<Runtime>,
        listener: TcpListener,
        tasks: &mut JoinSet<()>,
    ) -> Result<String> {
        let address = listener.local_addr()?;
        let url = format!("ws://{address}/api/network/ws");
        let (hub, mut incoming) = NetworkHub::new(
            node.identity.clone(),
            NetworkConfig {
                advertise_urls: vec![url.clone()],
                relay_enabled: true,
                ..NetworkConfig::default()
            },
        );
        tasks.spawn(async move {
            let mut clients = JoinSet::new();
            while let Some(incoming) = incoming.recv().await {
                let node = node.clone();
                clients.spawn(async move {
                    match incoming.service {
                        Service::Client => {
                            let _ = crate::client::serve(node, incoming.socket).await;
                        }
                        Service::Peer => {
                            let _ = node.hub.clone().accept_transport(incoming.socket).await;
                        }
                    }
                });
            }
        });
        let app = Router::new()
            .route(
                "/api/network/ws",
                get(
                    |State(hub): State<Arc<NetworkHub>>,
                     ConnectInfo(peer): ConnectInfo<SocketAddr>,
                     ws: WebSocketUpgrade| async move {
                        let admission = hub.admit(peer.ip()).unwrap();
                        ws.on_upgrade(move |socket| async move {
                            let _ = hub.accept(socket, admission).await;
                        })
                    },
                ),
            )
            .with_state(hub);
        tasks.spawn(async move {
            axum::serve(
                listener,
                app.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .await
            .unwrap();
        });
        Ok(url)
    }

    async fn invitation(node: &Runtime, admin: &LocalIdentity, url: &str) -> Result<Invitation> {
        let store = node.store.lock().await;
        let view = store.public_state()?.project()?;
        let capability = "f1".repeat(32);
        let payload = store.create_invite(InviteRequest {
            request_id: uuid::Uuid::new_v4().to_string(),
            cluster_id: view.cluster_id,
            inviter_subject_id: admin.peer_id()?,
            epoch: view.epoch,
            capability: capability.clone(),
            new_kind: IdentityKind::Node,
            node_roles: vec![NodeRole::Relay, NodeRole::Replica],
            manage: false,
            expires_at_ms: None,
        })?;
        let inviter_signature = admin.sign_payload("identity-invite", &payload.signing_bytes()?)?;
        let invite = InviteEnvelope {
            payload,
            inviter_signature,
        };
        store.commit_invite(invite.clone())?;
        Ok(Invitation {
            url: url.into(),
            peer_id: node.identity.peer_id()?,
            invite: InviteSecret { invite, capability },
        })
    }

    #[test]
    fn invitation_and_review_bind_exact_node_identity_permissions_and_request() -> Result<()> {
        let node = LocalIdentity::generate();
        let member = Member::node(&node, "Build Node", vec![NodeRole::Relay])?;
        let payload = crate::identity::local::InvitePayload {
            version: IDENTITY_VERSION,
            invite_id: "invite".into(),
            request_id: "create-invite".into(),
            cluster_id: "ab".repeat(32),
            inviter_subject_id: "cd".repeat(32),
            capability_hash: digest(b"fixture capability"),
            new_kind: IdentityKind::Node,
            node_roles: vec![NodeRole::Relay],
            manage: false,
            epoch: 1,
            expires_at_ms: 42,
            nonce: "nonce".into(),
        };
        let invite = InviteEnvelope {
            payload,
            inviter_signature: String::new(),
        };
        let request = PairRequest {
            request_id: "request".into(),
            invite_id: "invite".into(),
            capability: "fixture capability".into(),
            member: member.clone(),
            nonce: "request nonce".into(),
            proof: String::new(),
        };
        let wire = json!({"url":"http://localhost:4790", "peer_id":node.peer_id()?,
            "invite":{"invite":invite,"capability":"fixture capability"}});
        let encoded = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&wire)?);
        let parsed = Invitation::parse(&encoded)?;
        assert_eq!(parsed.url, "ws://localhost:4790/api/network/ws");
        assert_eq!(
            Invitation::parse(&wire.to_string())?.peer_id,
            node.peer_id()?
        );
        for relay in [false, true] {
            let mut old = wire.clone();
            old["relay"] = json!(relay);
            assert!(Invitation::parse(&old.to_string()).is_err());
        }
        let mut old = wire.clone();
        let fields = old.as_object_mut().unwrap();
        fields.remove("peer_id");
        fields.insert("node_id".into(), json!(node.peer_id()?));
        assert!(Invitation::parse(&old.to_string()).is_err());
        let mut old = wire;
        old["invite"]["invite"]["payload"]["version"] = json!(1);
        assert!(Invitation::parse(&old.to_string()).is_err());
        let request_hash = digest(&request.signing_bytes()?);
        let mut review = PairReview {
            version: IDENTITY_VERSION,
            invite_id: "invite".into(),
            cluster_id: "ab".repeat(32),
            inviter_subject_id: "cd".repeat(32),
            new_member: member.clone(),
            proposal_hash: request_hash.clone(),
            parents: vec![],
            epoch: 1,
            expires_at_ms: 42,
        };
        verify_review(&review, &invite, &member, &request_hash)?;
        review.proposal_hash = invite.payload.hash()?;
        assert!(verify_review(&review, &invite, &member, &request_hash).is_err());
        review.proposal_hash = request_hash.clone();
        review.new_member.name = "Another Node".into();
        assert!(verify_review(&review, &invite, &member, &request_hash).is_err());
        let wire = json!({"url":"http://localhost:4790", "peer_id":"device",
            "invite_id":"invite", "request_id":"request", "name":"device", "request_hash":null});
        let pending: PendingPair = serde_json::from_value(wire.clone())?;
        assert_eq!(pending.peer_id, "device");
        assert!(pending.invitation.is_none() && pending.phase == Phase::Created);
        let mut old = wire;
        let fields = old.as_object_mut().unwrap();
        fields.remove("peer_id");
        fields.insert("node_id".into(), json!("device"));
        assert!(serde_json::from_value::<PendingPair>(old).is_err());
        Ok(())
    }

    #[tokio::test]
    async fn node_join_reconciles_lost_responses_restarts_and_waits_for_manager_approval()
    -> Result<()> {
        let dir = directory()?;
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let inviter = open_node(&dir.join("inviter"), listener.local_addr()?).await?;
        let root = LocalIdentity::generate();
        let admin = LocalIdentity::generate();
        let state = ClusterState::new(Genesis::create(
            &root,
            vec![
                Member::client(&admin, "manager", true)?,
                Member::node(
                    &inviter.identity,
                    "existing Node",
                    vec![NodeRole::Gateway, NodeRole::Relay, NodeRole::Replica],
                )?,
            ],
        )?)?;
        inviter.install_membership(state.clone()).await?;
        let mut tasks = JoinSet::new();
        let url = host(inviter.clone(), listener, &mut tasks).await?;
        let joiner_dir = dir.join("joiner");
        let joiner = open_node(&joiner_dir, "127.0.0.1:4790".parse()?).await?;
        let pairing = NodePairing::new(joiner.clone());
        let invitation = invitation(&inviter, &admin, &url).await?;
        let result = async {
            assert!(matches!(pairing.status().await?, JoinState::Idle));
            let initial = pairing.request(invitation.clone(), "Mac mini").await?;
            let JoinState::Review { request_id, review, review_hash } = initial else { bail!("expected exact Node review"); };
            assert_eq!(review.new_member.subject_id, joiner.identity.peer_id()?);
            assert_eq!(review.new_member.kind, IdentityKind::Node);
            assert_eq!(review.new_member.node_roles, invitation.invite.invite.payload.node_roles);
            let receipt = fs::read(&pairing.pending_path)?;
            for nested in [false, true] {
                let mut old_receipt: Value = serde_json::from_slice(&receipt)?;
                if nested { old_receipt["invitation"]["relay"] = json!(false); }
                else { old_receipt["relay"] = json!(true); }
                config::private_write(&pairing.pending_path, &serde_json::to_vec(&old_receipt)?)?;
                assert!(pairing.status().await.is_err());
                assert!(pairing.request(invitation.clone(), "Mac mini").await.is_err());
            }
            config::private_write(&pairing.pending_path, &receipt)?;
            assert!(matches!(pairing.request(invitation.clone(), "Mac mini").await?, JoinState::Review { request_id: recovered, .. } if recovered == request_id));
            let saved: Value = serde_json::from_slice(&fs::read(&pairing.pending_path)?)?;
            assert!(saved.get("relay").is_none() && saved["invitation"].get("relay").is_none());
            assert!(!inviter.store.lock().await.pair_accepted(&request_id)?);
            assert!(!joiner.has_membership().await?);
            assert!(pairing.accept(&request_id, &"0".repeat(64)).await.is_err());
            let counter = rusqlite::Connection::open(inviter.data_dir.join("node.sqlite3"))?;
            counter.execute_batch("CREATE TABLE acceptance_writes(count INTEGER NOT NULL); INSERT INTO acceptance_writes VALUES(0); CREATE TRIGGER count_acceptance AFTER UPDATE OF new_subject_signature ON identity_pair_requests BEGIN UPDATE acceptance_writes SET count=count+1; END;")?;
            let mut pending = pairing.load()?.unwrap();
            pending.review = None;
            pending.phase = Phase::Requesting;
            pairing.save(&pending)?;
            assert!(matches!(pairing.status().await?, JoinState::Review { request_id: recovered, .. } if recovered == request_id));
            pending = pairing.load()?.unwrap();
            pending.phase = Phase::Accepting;
            pairing.save(&pending)?;
            assert!(matches!(pairing.status().await?, JoinState::Review { .. }));
            assert_eq!(counter.query_row::<i64, _, _>("SELECT count FROM acceptance_writes", [], |row| row.get(0))?, 0);
            assert!(matches!(pairing.accept(&request_id, &review_hash).await?, JoinState::Pending { .. }));
            pending = pairing.load()?.unwrap();
            pending.phase = Phase::Accepting;
            pairing.save(&pending)?;
            assert!(matches!(pairing.status().await?, JoinState::Pending { .. }));
            assert!(matches!(pairing.accept(&request_id, &review_hash).await?, JoinState::Pending { .. }));
            assert_eq!(counter.query_row::<i64, _, _>("SELECT count FROM acceptance_writes", [], |row| row.get(0))?, 1);
            assert!(!joiner.has_membership().await?);
            pending = pairing.load()?.unwrap();
            pending.url = "ws://127.0.0.1:0/api/network/ws".into();
            pairing.save(&pending)?;
            assert!(matches!(pairing.status().await?, JoinState::Uncertain { request_id: recovered, .. } if recovered == request_id));
            assert_eq!(pairing.load()?.unwrap().request_hash, pending.request_hash);
            assert_eq!(counter.query_row::<i64, _, _>("SELECT count FROM acceptance_writes", [], |row| row.get(0))?, 1);
            pending.url = url.clone();
            pairing.save(&pending)?;
            let mut manager = Client::connect(&url, &admin, &inviter.identity.peer_id()?).await?;
            let prepared = manager.call("prepare_pair", json!({"request_id":request_id})).await?;
            let mut event: GovernanceEvent = serde_json::from_value(prepared["event"].clone())?;
            event.signature = admin.sign_payload("identity-governance", &event.signing_bytes()?)?;
            let inviter_signature = admin.sign_payload("identity-pair-review", &review.signing_bytes()?)?;
            manager.call("pair_confirm", json!({"request_id":request_id,"event":event,"inviter_signature":inviter_signature})).await?;
            pending = pairing.load()?.unwrap();
            pending.phase = Phase::Reviewing;
            pairing.save(&pending)?;
            assert!(matches!(pairing.status().await?, JoinState::Review { .. }));
            assert!(!joiner.has_membership().await?);
            assert!(matches!(pairing.accept(&request_id, &review_hash).await?, JoinState::Pending { .. }));
            assert_eq!(counter.query_row::<i64, _, _>("SELECT count FROM acceptance_writes", [], |row| row.get(0))?, 1);
            let mut configuration = Config::load(&joiner_dir)?;
            configuration.roots = vec![joiner_dir.canonicalize()?];
            configuration.save(&joiner_dir)?;
            drop(counter);
            Ok::<_, anyhow::Error>((request_id, review_hash))
        }.await;
        drop(pairing);
        drop(joiner);
        let result = match result {
            Ok((request_id, review_hash)) => async {
                let joiner = Arc::new(Runtime::open(&joiner_dir).await?);
                let pairing = NodePairing::new(joiner.clone());
                let joined = pairing.status().await?;
                assert!(matches!(joined, JoinState::Joined { ref cluster_id, .. } if cluster_id == &state.genesis.cluster_id));
                assert!(!pairing.pending_path.exists());
                assert_eq!(Config::load(&joiner_dir)?.network.bootstrap_urls, vec![url]);
                assert_eq!(Config::load(&joiner_dir)?.roots, vec![joiner_dir.canonicalize()?]);
                assert!(matches!(pairing.status().await?, JoinState::Joined { .. }));
                assert!(pairing.request(invitation, "Mac mini").await.is_err());
                assert!(pairing.accept(&request_id, &review_hash).await.is_err());
                Ok(())
            }.await,
            Err(error) => Err(error),
        };
        tasks.abort_all();
        while tasks.join_next().await.is_some() {}
        drop(inviter);
        fs::remove_dir_all(dir)?;
        result
    }

    #[tokio::test]
    async fn node_join_validates_bootstrap_capacity_before_installing_membership() -> Result<()> {
        let dir = directory()?;
        let inviter = open_node(&dir.join("inviter"), "127.0.0.1:4791".parse()?).await?;
        let admin = LocalIdentity::generate();
        let state = ClusterState::new(Genesis::create(
            &LocalIdentity::generate(),
            vec![
                Member::client(&admin, "manager", true)?,
                Member::node(
                    &inviter.identity,
                    "existing Node",
                    inviter.config.roles.clone(),
                )?,
            ],
        )?)?;
        inviter.install_membership(state.clone()).await?;
        for (count, startup_override) in [(7, false), (8, false), (7, true), (8, true)] {
            let node_dir = dir.join(format!("joiner-{count}-{startup_override}"));
            let node = open_node(&node_dir, "127.0.0.1:4790".parse()?).await?;
            let urls: Vec<String> = (0..count)
                .map(|index| format!("ws://127.0.0.1:{}/api/network/ws", 5900 + index))
                .collect();
            if !startup_override {
                let mut configuration = Config::load(&node_dir)?;
                configuration.network.bootstrap_urls = urls.clone();
                configuration.save(&node_dir)?;
            }
            drop(node);
            let node = Arc::new(if startup_override {
                Runtime::open_with_bootstrap_urls(&node_dir, &urls).await?
            } else {
                Runtime::open(&node_dir).await?
            });
            assert_eq!(node.config.network.bootstrap_urls.len(), count);
            assert_eq!(
                Config::load(&node_dir)?.network.bootstrap_urls.len(),
                if startup_override { 0 } else { count }
            );
            let capability = node.store.lock().await.bootstrap_capability()?;
            let pairing = NodePairing::new(node.clone());
            let invitation =
                invitation(&inviter, &admin, "ws://127.0.0.1:9/api/network/ws").await?;
            let mut granted = state.clone();
            granted.append(GovernanceEvent::grant(
                &admin,
                &granted.project()?,
                Member::node(
                    &node.identity,
                    "Mac mini",
                    invitation.invite.invite.payload.node_roles.clone(),
                )?,
            )?)?;
            let pending = PendingPair {
                url: invitation.url.clone(),
                peer_id: invitation.peer_id.clone(),
                invite_id: invitation.invite.invite.payload.invite_id.clone(),
                request_id: "accepted-request".into(),
                name: "Mac mini".into(),
                request_hash: None,
                invitation: Some(invitation),
                review: None,
                phase: Phase::Accepted,
            };
            pairing.save(&pending)?;
            let config_before = fs::read(node_dir.join("config.json"))?;
            let receipt_before = fs::read(&pairing.pending_path)?;
            let result = pairing.finish(&pending, granted).await;
            let saved_count;
            if count == 8 {
                assert_eq!(result.unwrap_err().to_string(), "too many network URLs");
                assert!(!node.has_membership().await?);
                node.store
                    .lock()
                    .await
                    .verify_bootstrap_capability(&capability.capability)?;
                assert_eq!(fs::read(node_dir.join("config.json"))?, config_before);
                assert_eq!(fs::read(&pairing.pending_path)?, receipt_before);
                saved_count = if startup_override { 0 } else { 8 };
            } else {
                assert!(matches!(result?, JoinState::Joined { .. }));
                assert!(node.has_membership().await?);
                assert!(!pairing.pending_path.exists());
                let saved = Config::load(&node_dir)?;
                saved_count = if startup_override { 1 } else { 8 };
                assert_eq!(saved.network.bootstrap_urls.len(), saved_count);
                assert_eq!(saved.network.bootstrap_urls.last(), Some(&pending.url));
                node.network.validate_bootstrap(&pending.url).await?;
            }
            assert!(
                node.network
                    .add_bootstrap("ws://127.0.0.1:10/api/network/ws")
                    .await
                    .is_err()
            );
            drop(pairing);
            drop(node);
            let reopened = Runtime::open(&node_dir).await?;
            assert_eq!(reopened.has_membership().await?, count == 7);
            assert_eq!(
                Config::load(&node_dir)?.network.bootstrap_urls.len(),
                saved_count
            );
            drop(reopened);
        }
        drop(inviter);
        fs::remove_dir_all(dir)?;
        Ok(())
    }

    #[tokio::test]
    async fn pending_invitation_cannot_replace_an_installed_cluster() -> Result<()> {
        let dir = directory()?;
        let node = open_node(&dir, "127.0.0.1:4790".parse()?).await?;
        let admin = LocalIdentity::generate();
        let state = ClusterState::new(Genesis::create(
            &LocalIdentity::generate(),
            vec![
                Member::client(&admin, "manager", true)?,
                Member::node(&node.identity, "Mac mini", node.config.roles.clone())?,
            ],
        )?)?;
        node.install_membership(state.clone()).await?;
        let pairing = NodePairing::new(node.clone());
        let mut invitation = invitation(&node, &admin, "ws://127.0.0.1:9/api/network/ws").await?;
        assert!(
            pairing
                .request(invitation.clone(), "Mac mini")
                .await
                .is_err()
        );
        assert!(!pairing.pending_path.exists());
        invitation.invite.invite.payload.cluster_id = "11".repeat(32);
        let pending = PendingPair {
            url: invitation.url.clone(),
            peer_id: invitation.peer_id.clone(),
            invite_id: invitation.invite.invite.payload.invite_id.clone(),
            request_id: "pending".into(),
            name: "Mac mini".into(),
            request_hash: None,
            invitation: Some(invitation),
            review: None,
            phase: Phase::Accepting,
        };
        pairing.save(&pending)?;
        assert!(pairing.status().await.is_err());
        assert!(pairing.pending_path.exists());
        assert_eq!(node.membership_state().await?, state);
        drop(pairing);
        drop(node);
        fs::remove_dir_all(dir)?;
        Ok(())
    }
}
