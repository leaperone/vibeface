//! The CLI as a management client of a Node.
//!
//! It holds a device key of its own, sealed with a passphrase, and is approved into the
//! personal network like a browser or a phone: an invite, the short code both sides compare, a
//! manager's signed grant. It then signs exactly what those clients sign. The Node never signs
//! governance for it.

use crate::connection::{Connection, Endpoint};
mod durable;
mod enrollment;
mod plan;
#[cfg(test)]
mod recovery_tests;
#[cfg(test)]
mod rotation_tests;
use anyhow::{Context, Result, bail, ensure};
pub use enrollment::{Enrollment, Progress};
pub use plan::{IdentityOperationState, IdentityPlan, IdentityPlanInfo, IdentityPlanKind};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};
use vibeface_bridge::platform::privatefs;
use vibeface_secure::{
    Identity,
    join::{
        ATTESTATION_DOMAIN, INVITE_DOMAIN, InviteRecord, Ticket, attestation_payload, invite_hash,
        invite_id, join_commit, short_code,
    },
    link::NodeLink,
    membership::{
        AuthorizationView, GovernanceAction, GovernanceEvent, IdentityKind, Member, NetworkState,
        Revocation, RootCheckpoint,
    },
    recovery_open, recovery_seal,
};
use zeroize::Zeroizing;

pub const PROLOGUE: &[u8] = b"vibeface:client:v1";
const KEY_FILE: &str = "client.json";
const PINS_FILE: &str = "approvals.json";
const MAX_FILE: u64 = 4 * 1024 * 1024;
const INVITE_LIFETIME_MS: i64 = 10 * 60 * 1000;
const RECOVERY_READ_DOMAIN: &str = "identity-recovery-read";

/// Where this client reaches its Node: a Gateway origin and the Node's device ID.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Entry {
    pub origin: String,
    pub node_id: String,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "transport", rename_all = "snake_case", deny_unknown_fields)]
enum RememberedEndpoint {
    Gateway {
        entry: Entry,
    },
    Lan {
        node_id: String,
        addresses: Vec<String>,
    },
    Preferred {
        entry: Entry,
        addresses: Vec<String>,
    },
}
impl RememberedEndpoint {
    fn endpoint(&self) -> Endpoint {
        match self {
            Self::Gateway { entry } => Endpoint::Gateway(entry.clone()),
            Self::Lan { node_id, addresses } => Endpoint::Lan {
                node_id: node_id.clone(),
                addresses: addresses.clone(),
            },
            Self::Preferred { entry, addresses } => Endpoint::Preferred {
                gateway: entry.clone(),
                addresses: addresses.clone(),
            },
        }
    }
    fn from_endpoint(endpoint: Endpoint) -> Result<Self> {
        endpoint.validate()?;
        Ok(match endpoint {
            Endpoint::Gateway(entry) => Self::Gateway { entry },
            Endpoint::Lan { node_id, addresses } => Self::Lan { node_id, addresses },
            Endpoint::Preferred { gateway, addresses } => Self::Preferred {
                entry: gateway,
                addresses,
            },
            Endpoint::Local(_) => anyhow::bail!(
                "a local session credential must not be remembered as network membership"
            ),
        })
    }
}

/// `client.json`: the sealed key and, once it is a member, which network and Node it uses.
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct KeyFile {
    version: u32,
    device_id: String,
    #[serde(default)]
    network_id: Option<String>,
    #[serde(default)]
    endpoint: Option<RememberedEndpoint>,
    envelope: Value,
}

/// The unlocked client key. The secret leaves memory only sealed, in `client.json`.
pub struct ClientKey {
    dir: PathBuf,
    pub(crate) identity: Identity,
    file: KeyFile,
}

impl ClientKey {
    pub fn exists(dir: &Path) -> bool {
        dir.join(KEY_FILE).exists()
    }

    /// A new key sealed with `passphrase`; refuses to replace one already in `dir`.
    pub fn create(dir: &Path, passphrase: &str) -> Result<Self> {
        private_dir(dir)?;
        let identity = Identity::generate()?;
        let secret = Zeroizing::new(identity.secret_hex());
        let payload = Zeroizing::new(format!(r#"{{"secret_key_hex":"{}"}}"#, secret.as_str()));
        let envelope = recovery_seal(payload.as_bytes(), passphrase)?;
        let file = KeyFile {
            version: 1,
            device_id: identity.device_id(),
            network_id: None,
            endpoint: None,
            envelope: serde_json::from_slice(&envelope)?,
        };
        let path = dir.join(KEY_FILE);
        durable::publish_new(&path, &serde_json::to_vec_pretty(&file)?).with_context(|| {
            format!(
                "{} already holds a client key or cannot publish it privately",
                path.display()
            )
        })?;
        Ok(Self {
            dir: dir.to_owned(),
            identity,
            file,
        })
    }

    pub fn open(dir: &Path, passphrase: &str) -> Result<Self> {
        let path = dir.join(KEY_FILE);
        privatefs::ensure_private(&path, "the client key")?;
        let file: KeyFile = serde_json::from_slice(&read_bounded(&path)?)
            .context("the client key file is damaged")?;
        ensure!(file.version == 1, "unsupported client key file");
        let payload = Zeroizing::new(
            recovery_open(&serde_json::to_vec(&file.envelope)?, passphrase)
                .context("wrong passphrase for the client key")?,
        );
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Secret {
            secret_key_hex: Zeroizing<String>,
        }
        let secret: Secret = serde_json::from_slice(&payload)?;
        let identity = Identity::from_secret_hex(&secret.secret_key_hex)?;
        ensure!(
            identity.device_id() == file.device_id,
            "the client key file is damaged"
        );
        Ok(Self {
            dir: dir.to_owned(),
            identity,
            file,
        })
    }

    pub fn device_id(&self) -> &str {
        &self.file.device_id
    }

    pub fn network_id(&self) -> Option<&str> {
        self.file.network_id.as_deref()
    }

    pub fn entry(&self) -> Option<&Entry> {
        match self.file.endpoint.as_ref()? {
            RememberedEndpoint::Gateway { entry } | RememberedEndpoint::Preferred { entry, .. } => {
                Some(entry)
            }
            RememberedEndpoint::Lan { .. } => None,
        }
    }
    pub fn remembered_endpoint(&self) -> Result<Endpoint> {
        let endpoint = self
            .file
            .endpoint
            .as_ref()
            .context("this client has no remembered network entry")?
            .endpoint();
        endpoint.validate()?;
        Ok(endpoint)
    }

    /// Records the network this key is a member of and the Node it reaches it through.
    pub fn remember(&mut self, network_id: &str, entry: Entry) -> Result<()> {
        self.remember_endpoint(network_id, Endpoint::Gateway(entry))
    }
    pub fn remember_endpoint(&mut self, network_id: &str, endpoint: Endpoint) -> Result<()> {
        vibeface_secure::membership::validate_hash(network_id)?;
        let endpoint = RememberedEndpoint::from_endpoint(endpoint)?;
        let mut file = self.file.clone();
        file.network_id = Some(network_id.to_owned());
        file.endpoint = Some(endpoint);
        privatefs::write_atomic(&self.dir.join(KEY_FILE), &serde_json::to_vec_pretty(&file)?)?;
        self.file = file;
        enrollment::clear_joined(self, network_id)
    }

    /// Opens a channel to the remembered Node, or to `entry` when given.
    pub async fn connect(&self, entry: Option<&Entry>) -> Result<Session<'_>> {
        let endpoint = match entry {
            Some(entry) => Endpoint::Gateway(entry.clone()),
            None => self.remembered_endpoint()?,
        };
        self.connect_endpoint(&endpoint).await
    }

    pub async fn connect_endpoint(&self, endpoint: &Endpoint) -> Result<Session<'_>> {
        let connection = Connection::open_endpoint(endpoint, &self.identity).await?;
        Ok(self.session(connection, endpoint.clone()))
    }
    pub(crate) fn session(&self, connection: Connection, entry: Endpoint) -> Session<'_> {
        Session {
            key: self,
            connection,
            entry,
        }
    }

    pub(crate) fn sign(&self, domain: &str, payload: &[u8]) -> Result<String> {
        Ok(self.identity.sign(domain, payload)?)
    }
}

/// A connected client: what it signs, it signs with its own key.
pub struct Session<'a> {
    key: &'a ClientKey,
    pub connection: Connection,
    pub entry: Endpoint,
}

/// The directory as this client checked it, at the Node's time.
pub struct Directory {
    pub state: NetworkState,
    pub view: AuthorizationView,
    pub now_ms: u64,
}

impl Session<'_> {
    pub async fn call(&mut self, method: &str, params: Value) -> Result<Value> {
        self.connection.call(method, params).await
    }

    fn network_id(&self) -> Result<&str> {
        self.key
            .network_id()
            .context("this client is not a member of a personal network yet")
    }

    /// Reads the directory and checks every signature in it here, not trusting the Node's view.
    pub async fn directory(&mut self) -> Result<Directory> {
        let mut result = self.call("directory", json!({})).await?;
        let state: NetworkState = serde_json::from_value(result["state"].take())?;
        ensure!(
            state.genesis.network_id == self.network_id()?,
            "the Node answered for another personal network"
        );
        let now_ms = result["now_ms"].as_u64().context("now_ms is missing")?;
        let view = state.project_at(now_ms)?;
        Ok(Directory {
            state,
            view,
            now_ms,
        })
    }

    async fn govern(&mut self, method: &str, action: GovernanceAction) -> Result<Value> {
        let directory = self.directory().await?;
        let event = GovernanceEvent::unsigned(&directory.view, self.key.device_id(), action)?
            .sign(&self.key.identity)?;
        self.call(method, json!({"event":event})).await
    }

    /// Renames a member or changes a client's management permission.
    pub async fn grant(
        &mut self,
        device_id: &str,
        name: Option<&str>,
        manage: Option<bool>,
    ) -> Result<Value> {
        let mut member = self.directory().await?.view.member(device_id)?;
        if let Some(name) = name {
            member.name = name.to_owned();
        }
        if let Some(manage) = manage {
            ensure!(
                member.kind == IdentityKind::Client || !manage,
                "a Node cannot manage the personal network"
            );
            member.manage = manage;
        }
        self.govern("grant", GovernanceAction::Grant { member })
            .await
    }

    /// Removes a device; a Node keeps what it wrote up to its last head this Node holds.
    pub async fn revoke(&mut self, device_id: &str) -> Result<Value> {
        let member = self.directory().await?.view.member(device_id)?;
        let mut cutoff = None;
        if member.kind == IdentityKind::Node {
            let nodes = self.call("rpc", json!({"method":"nodes"})).await?;
            let node = nodes["nodes"]
                .as_array()
                .into_iter()
                .flatten()
                .find(|node| node["device_id"] == device_id)
                .context("the Node does not list that device")?;
            let seq = node["max_seq"].as_u64().unwrap_or(0);
            if seq > 0 {
                cutoff = Some((
                    seq,
                    node["head_hash"]
                        .as_str()
                        .context("head_hash is missing")?
                        .to_owned(),
                ));
            }
        }
        let revocation = Revocation::new(
            device_id,
            cutoff.as_ref().map(|(seq, hash)| (*seq, hash.as_str())),
        )?;
        self.govern("revoke", GovernanceAction::Revoke(revocation))
            .await
    }

    /// Approves (`approve`) or stops (`veto`) the recovery waiting in this epoch.
    pub async fn settle_recovery(&mut self, approve: bool) -> Result<Value> {
        let directory = self.directory().await?;
        let checkpoint = directory
            .view
            .pending
            .as_ref()
            .context("no recovery is waiting")?
            .checkpoint
            .clone();
        let at_ms = directory.now_ms;
        if approve {
            self.govern(
                "approve_recovery",
                GovernanceAction::Approve { checkpoint, at_ms },
            )
            .await
        } else {
            self.govern("veto", GovernanceAction::Veto { checkpoint, at_ms })
                .await
        }
    }

    /// Registers a ten-minute invite at the Node and returns its link. The link is the only
    /// copy of the invite's secret.
    pub async fn invite(&mut self) -> Result<String> {
        let secret: [u8; 32] = rand::random();
        let mut record = InviteRecord {
            network_id: self.network_id()?.to_owned(),
            invite_hash: invite_hash(&secret),
            issuer: self.key.device_id().to_owned(),
            expires_at_ms: now_ms() + INVITE_LIFETIME_MS,
            signature: String::new(),
        };
        record.signature = self.key.sign(INVITE_DOMAIN, &record.signing_payload()?)?;
        let registered = self.call("invite", json!({"record":record})).await?;
        let entry = NodeLink::parse(registered["entry"].as_str().context("entry is missing")?)?;
        ensure!(
            self.entry.node_id() == entry.device_id(),
            "the invite names another Node than the one this client reached"
        );
        Ok(Ticket::new(entry, secret)?.link()?)
    }

    /// The joins waiting for a manager, with the short code of each this client pinned and
    /// whose joiner revealed a nonce matching its commitment.
    pub async fn requests(&mut self) -> Result<Vec<Value>> {
        let mut listed = self.call("join_status", json!({})).await?;
        let pins = Pins::load(&self.key.dir)?;
        let mut requests: Vec<Value> = serde_json::from_value(listed["requests"].take())?;
        for request in &mut requests {
            if let Ok(code) = pins.code(self.key.device_id(), request) {
                request["code"] = json!(code);
            }
        }
        Ok(requests)
    }

    async fn request(&mut self, join_id: &str) -> Result<Value> {
        self.requests()
            .await?
            .into_iter()
            .find(|request| request["join_id"] == join_id)
            .context("no such join request; list them with vibeface client requests")
    }

    /// First approval step: this client's nonce, pinned before it is sent so a retry sends
    /// the same one. Returns the request as the Node then holds it.
    pub async fn confirm_join(&mut self, join_id: &str) -> Result<Value> {
        let request = self.request(join_id).await?;
        let mut pins = Pins::load(&self.key.dir)?;
        let pin = pins.pin(join_id, &request)?;
        let nonce = pin.nonce.clone();
        pins.save(&self.key.dir)?;
        self.call("approve", json!({"join_id":join_id,"admin_nonce":nonce}))
            .await?;
        self.request(join_id).await
    }

    /// The short code of a join this client confirmed, once the joiner revealed its nonce.
    pub async fn join_code(&mut self, join_id: &str) -> Result<Option<String>> {
        let request = self.request(join_id).await?;
        if request["joiner_nonce"].is_null() {
            return Ok(None);
        }
        Pins::load(&self.key.dir)?
            .code(self.key.device_id(), &request)
            .map(Some)
    }

    /// Second step, after the person compared the codes: sign the grant for the name the Node
    /// gave the device.
    pub async fn grant_join(&mut self, join_id: &str, manage: bool) -> Result<Value> {
        let request = self.request(join_id).await?;
        let pins = Pins::load(&self.key.dir)?;
        pins.code(self.key.device_id(), &request)?;
        let kind: IdentityKind = serde_json::from_value(request["kind"].clone())?;
        let member = Member::new(
            text(&request, "device_id")?,
            kind,
            text(&request, "name")?,
            manage && kind == IdentityKind::Client,
        )?;
        let directory = self.directory().await?;
        let event = GovernanceEvent::unsigned(
            &directory.view,
            self.key.device_id(),
            GovernanceAction::Grant { member },
        )?
        .sign(&self.key.identity)?;
        let joined = self
            .call("approve", json!({"join_id":join_id,"event":event}))
            .await?;
        let mut pins = Pins::load(&self.key.dir)?;
        pins.0.remove(join_id);
        pins.save(&self.key.dir)?;
        Ok(joined)
    }

    pub async fn reject_join(&mut self, join_id: &str) -> Result<Value> {
        self.call("approve", json!({"join_id":join_id,"reject":true}))
            .await
    }

    /// A conversation write the user signs, as the browser does: `body` is the event body,
    /// its signature goes in `attestation`.
    pub fn attest(&self, scope: &str, body: &Value) -> Result<Value> {
        let payload = attestation_payload(self.network_id()?, scope, &body.to_string())?;
        Ok(
            json!({"device_id":self.key.device_id(),"signature":self.key.sign(ATTESTATION_DOMAIN, &payload)?}),
        )
    }

    /// The operation ID the browser derives for a memory change: its payload and signer.
    pub fn operation_id(&self, scope: &str, body: &Value) -> Result<String> {
        let mut signed = attestation_payload(self.network_id()?, scope, &body.to_string())?;
        signed.extend_from_slice(self.key.device_id().as_bytes());
        Ok(hex::encode(Sha256::digest(&signed)))
    }

    /// A `rpc` write signed like the browser signs it.
    pub async fn rpc(&mut self, method: &str, mut params: Value) -> Result<Value> {
        params["method"] = json!(method);
        self.call("rpc", params).await
    }
}

/// The approvals this client started: its nonce and what the joiner committed to, kept so the
/// codes it shows are checked against what it saw first.
#[derive(Default, Serialize, Deserialize)]
struct Pins(BTreeMap<String, Pin>);

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Pin {
    nonce: String,
    device_id: String,
    invite_id: String,
    commit: String,
}

impl Pins {
    fn load(dir: &Path) -> Result<Self> {
        let path = dir.join(PINS_FILE);
        if !path.exists() {
            return Ok(Self::default());
        }
        Ok(serde_json::from_slice(&read_bounded(&path)?)?)
    }

    fn save(&self, dir: &Path) -> Result<()> {
        privatefs::write_atomic(&dir.join(PINS_FILE), &serde_json::to_vec_pretty(self)?)
    }

    fn pin(&mut self, join_id: &str, request: &Value) -> Result<&Pin> {
        let pin = Pin {
            nonce: hex::encode(rand::random::<[u8; 32]>()),
            device_id: text(request, "device_id")?.to_owned(),
            invite_id: text(request, "invite_id")?.to_owned(),
            commit: text(request, "commit")?.to_owned(),
        };
        let pinned = self.0.entry(join_id.to_owned()).or_insert(pin);
        ensure!(
            request["device_id"] == pinned.device_id.as_str()
                && request["invite_id"] == pinned.invite_id.as_str()
                && request["commit"] == pinned.commit.as_str(),
            "this join request changed since it was first confirmed; reject it"
        );
        Ok(pinned)
    }

    fn code(&self, admin: &str, request: &Value) -> Result<String> {
        let join_id = text(request, "join_id")?;
        let pin = self
            .0
            .get(join_id)
            .context("confirm this join first: vibeface client approve")?;
        let joiner_nonce = text(request, "joiner_nonce")?;
        ensure!(
            request["admin_id"] == admin
                && request["admin_nonce"] == pin.nonce.as_str()
                && request["device_id"] == pin.device_id.as_str()
                && request["invite_id"] == pin.invite_id.as_str()
                && request["commit"] == pin.commit.as_str()
                && join_commit(&pin.device_id, joiner_nonce)? == pin.commit,
            "this join request does not match what was confirmed; reject it"
        );
        Ok(short_code(
            text(request, "network_id")?,
            &pin.invite_id,
            &pin.device_id,
            admin,
            &pin.nonce,
            joiner_nonce,
        )?)
    }
}

/// The recovery file a browser saves: the root key, sealed with the recovery password.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RecoveryFile {
    version: u32,
    network_id: String,
    envelope: Value,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RecoveryMaterial {
    version: u32,
    network_id: String,
    root_secret_key: Zeroizing<String>,
    genesis: vibeface_secure::membership::Genesis,
    checkpoints: Vec<RootCheckpoint>,
    events: Vec<GovernanceEvent>,
}

fn recovery_file_bytes(material: &RecoveryMaterial, password: &str) -> Result<Vec<u8>> {
    let payload = Zeroizing::new(serde_json::to_vec(material)?);
    let envelope = recovery_seal(&payload, password)?;
    Ok(serde_json::to_vec_pretty(&RecoveryFile {
        version: 1,
        network_id: material.network_id.clone(),
        envelope: serde_json::from_slice(&envelope)?,
    })?)
}
#[cfg(test)]
fn write_recovery_file(path: &Path, material: &RecoveryMaterial, password: &str) -> Result<()> {
    durable::publish_new(path, &recovery_file_bytes(material, password)?)
}

fn read_recovery_file(path: &Path, password: &str) -> Result<RecoveryMaterial> {
    let file: RecoveryFile =
        serde_json::from_slice(&read_bounded(path)?).context("not a recovery file")?;
    let payload = Zeroizing::new(
        recovery_open(&serde_json::to_vec(&file.envelope)?, password)
            .context("wrong recovery password")?,
    );
    let material: RecoveryMaterial = serde_json::from_slice(&payload)?;
    ensure!(
        material.version == 1 && material.network_id == file.network_id,
        "the recovery file is damaged"
    );
    Ok(material)
}

/// Creates a personal network on an empty Node: this client is its first manager and
/// `recovery_file` (written before the Node hears of it) holds the new recovery key.
pub async fn prepare_create(
    key: &ClientKey,
    entry: Entry,
    capability: &str,
    name: &str,
    node_name: Option<&str>,
    recovery_file: &Path,
    password: &str,
) -> Result<IdentityPlan> {
    ensure!(
        !IdentityPlan::exists(key),
        "an identity operation is pending; reopen it instead of generating another root"
    );
    let mut session = key.connect(Some(&entry)).await?;
    let info = session.call("info", json!({})).await?;
    ensure!(
        info["initialized"] == false,
        "this Node already belongs to a personal network"
    );
    ensure!(
        info["device_id"] == entry.node_id.as_str(),
        "the Node answered with another device ID"
    );
    let node_name = node_name.unwrap_or(info["name"].as_str().unwrap_or("Node"));
    let root = Identity::generate()?;
    let genesis = vibeface_secure::membership::Genesis::create(
        &root,
        Member::client(&session.key.identity, name, true)?,
        Member::new(&entry.node_id, IdentityKind::Node, node_name, false)?,
    )?;
    let material = RecoveryMaterial {
        version: 1,
        network_id: genesis.network_id.clone(),
        root_secret_key: Zeroizing::new(root.secret_hex()),
        genesis: genesis.clone(),
        checkpoints: vec![],
        events: vec![],
    };
    drop(session);
    IdentityPlan::prepare(
        key,
        entry,
        recovery_file,
        &material,
        password,
        json!({"genesis":genesis,"capability":capability}),
    )
}

pub struct RecoveryOptions<'a> {
    pub name: &'a str,
    pub capability: Option<&'a str>,
    /// Retained members without the new manager. None keeps every current member.
    pub members: Option<&'a [Member]>,
}

#[derive(Serialize)]
pub struct RecoveryReview {
    pub network_id: String,
    pub members: Vec<Member>,
    pub required_node: String,
    pub new_manager: Member,
}

struct RecoverySource {
    root: Identity,
    state: NetworkState,
    view: AuthorizationView,
    heads: BTreeMap<String, (u64, String)>,
    now: u64,
    fresh: bool,
    review: RecoveryReview,
}

/// Verify the current recovery candidates without preparing a checkpoint, backup, or plan.
pub async fn review_recovery(
    key: &ClientKey,
    entry: Entry,
    recovery_file: &Path,
    password: &str,
    options: RecoveryOptions<'_>,
) -> Result<RecoveryReview> {
    ensure!(
        IdentityPlan::inspect(key, password)?
            .is_none_or(|plan| plan.state == IdentityOperationState::Applied),
        "an identity operation is pending; review its saved selection instead"
    );
    ensure!(
        options.members.is_none(),
        "review lists candidates before selecting members"
    );
    Ok(
        recovery_source(key, &entry, recovery_file, password, options.name)
            .await?
            .review,
    )
}

/// Starts a recovery with `recovery_file`: selected current members stay, this client joins as a
/// manager, and `new_file` (written before the Node hears of it) takes over the recovery key.
/// It takes effect after the recovery wait unless a manager stops it. An empty Node needs its
/// one-time `capability`. With no selection, every current member stays. A selection excludes
/// this new manager, keeps the connected Node, and may change existing clients' management rights.
pub async fn prepare_recovery(
    key: &ClientKey,
    entry: Entry,
    recovery_file: &Path,
    password: &str,
    new_file: &Path,
    options: RecoveryOptions<'_>,
) -> Result<IdentityPlan> {
    let RecoveryOptions {
        name,
        capability,
        members: selected_members,
    } = options;
    ensure!(
        !IdentityPlan::exists(key),
        "an identity operation is pending; reopen it instead of generating another root"
    );
    let RecoverySource {
        root,
        state,
        view,
        heads,
        now,
        fresh,
        review,
    } = recovery_source(key, &entry, recovery_file, password, name).await?;
    let members = recovery_members(
        &review.members,
        selected_members,
        &entry.node_id,
        review.new_manager,
    )?;
    let capability = if fresh {
        Some(capability.context("an empty Node needs its capability")?)
    } else {
        None
    };
    let next = Identity::generate()?;
    let retired = view.retire(&members, &heads)?;
    let checkpoint =
        RootCheckpoint::recovery(&state, &root, &next.device_id(), members, retired, now)?;
    state.clone().recover_at(checkpoint.clone(), now)?;
    let mut checkpoints = state.checkpoints.clone();
    checkpoints.push(checkpoint);
    let backup = RecoveryMaterial {
        version: 1,
        network_id: review.network_id,
        root_secret_key: Zeroizing::new(next.secret_hex()),
        genesis: state.genesis.clone(),
        checkpoints: checkpoints.clone(),
        events: state.events.clone(),
    };
    let mut params =
        json!({"genesis":state.genesis,"checkpoints":checkpoints,"events":state.events});
    if let Some(capability) = capability {
        params["capability"] = json!(capability);
    }
    IdentityPlan::prepare(key, entry, new_file, &backup, password, params)
}

async fn recovery_source(
    key: &ClientKey,
    entry: &Entry,
    recovery_file: &Path,
    password: &str,
    name: &str,
) -> Result<RecoverySource> {
    let material = read_recovery_file(recovery_file, password)?;
    let root = Identity::from_secret_hex(&material.root_secret_key)?;
    let me = key.device_id().to_owned();
    let mut session = key.connect(Some(entry)).await?;
    let info = session.call("info", json!({})).await?;
    let fresh = info["initialized"] == false;
    let mut state = NetworkState {
        genesis: material.genesis.clone(),
        checkpoints: material.checkpoints.clone(),
        events: material.events.clone(),
    };
    let mut heads = BTreeMap::new();
    let mut now = now_ms() as u64;
    if !fresh {
        let expires = now_ms() + 60_000;
        let proof = format!(
            r#"{{"device_id":"{me}","expires_at_ms":{expires},"network_id":"{}"}}"#,
            material.network_id
        );
        let mut read = session
            .call(
                "recovery_state",
                json!({"expires_at_ms":expires,
                    "signature":root.sign(RECOVERY_READ_DOMAIN, proof.as_bytes())?,
                    "checkpoints":material.checkpoints,"events":material.events}),
            )
            .await?;
        state = serde_json::from_value(read["state"].take())?;
        heads = serde_json::from_value(read["heads"].take())?;
        now = read["now_ms"].as_u64().context("now_ms is missing")?;
    }
    ensure!(
        state.genesis.network_id == material.network_id,
        "the Node answered for another personal network"
    );
    let view = state.project_at(now)?;
    ensure!(
        view.recovery_key == root.device_id(),
        "a newer recovery file replaced this one"
    );
    let mut members: Vec<Member> = view
        .members
        .iter()
        .filter(|member| view.revocation(&member.device_id).is_none() && member.device_id != me)
        .cloned()
        .collect();
    if !members
        .iter()
        .any(|member| member.device_id == entry.node_id)
    {
        ensure!(fresh, "this Node is not in your personal network");
        members.push(Member::new(
            &entry.node_id,
            IdentityKind::Node,
            info["name"].as_str().unwrap_or("Node"),
            false,
        )?);
    }
    ensure!(
        members
            .iter()
            .any(|member| member.device_id == entry.node_id && member.kind == IdentityKind::Node),
        "recovery must retain the connected Node"
    );
    state.check_root(&root.device_id(), now, false)?;
    Ok(RecoverySource {
        root,
        state,
        view,
        heads,
        now,
        fresh,
        review: RecoveryReview {
            network_id: material.network_id,
            members,
            required_node: entry.node_id.clone(),
            new_manager: Member::client(&session.key.identity, name, true)?,
        },
    })
}

fn recovery_members(
    allowed: &[Member],
    selected: Option<&[Member]>,
    required_node: &str,
    manager: Member,
) -> Result<Vec<Member>> {
    let mut members = selected.unwrap_or(allowed).to_vec();
    let mut seen = std::collections::BTreeSet::new();
    for member in &members {
        let original = allowed
            .iter()
            .find(|original| original.device_id == member.device_id)
            .context("selected recovery member is not a current member")?;
        ensure!(
            original.kind == member.kind && original.name == member.name,
            "recovery selection cannot change a device's kind or name"
        );
        member.validate()?;
        ensure!(
            member.device_id != manager.device_id && seen.insert(&member.device_id),
            "recovery selection contains a duplicate device or the new manager"
        );
    }
    ensure!(
        members
            .iter()
            .any(|member| member.device_id == required_node && member.kind == IdentityKind::Node),
        "recovery must retain the connected Node"
    );
    members.push(manager);
    Ok(members)
}

/// Rotates the recovery key without changing members. The replacement backup is durable
/// before explicit plan submission, and the original request survives unknown outcomes.
pub async fn prepare_key_rotation(
    key: &ClientKey,
    entry: Entry,
    recovery_file: &Path,
    password: &str,
    new_file: &Path,
    new_password: &str,
) -> Result<IdentityPlan> {
    ensure!(
        !IdentityPlan::exists(key),
        "an identity operation is pending; reopen it instead of generating another root"
    );
    let material = read_recovery_file(recovery_file, password)?;
    ensure!(
        key.network_id() == Some(material.network_id.as_str()),
        "recovery file belongs to another personal network"
    );
    let root = Identity::from_secret_hex(&material.root_secret_key)?;
    let mut session = key.connect(Some(&entry)).await?;
    let directory = session.directory().await?;
    ensure!(
        directory.view.recovery_key == root.device_id(),
        "a newer recovery file replaced this one"
    );
    directory
        .state
        .check_root(&root.device_id(), directory.now_ms, true)?;
    let next = Identity::generate()?;
    let checkpoint =
        RootCheckpoint::rotation(&directory.state, &root, &next.device_id(), directory.now_ms)?;
    directory
        .state
        .clone()
        .recover_at(checkpoint.clone(), directory.now_ms)?;
    let mut checkpoints = directory.state.checkpoints;
    checkpoints.push(checkpoint);
    let backup = RecoveryMaterial {
        version: 1,
        network_id: material.network_id,
        root_secret_key: Zeroizing::new(next.secret_hex()),
        genesis: directory.state.genesis,
        checkpoints,
        events: directory.state.events,
    };
    let params =
        json!({"genesis":backup.genesis,"checkpoints":backup.checkpoints,"events":backup.events});
    drop(session);
    IdentityPlan::prepare(key, entry, new_file, &backup, new_password, params)
}

fn text<'v>(value: &'v Value, key: &str) -> Result<&'v str> {
    value[key]
        .as_str()
        .with_context(|| format!("{key} is missing"))
}

fn read_bounded(path: &Path) -> Result<Vec<u8>> {
    use std::io::Read as _;
    let mut bytes = Vec::new();
    std::fs::File::open(path)
        .with_context(|| format!("cannot read {}", path.display()))?
        .take(MAX_FILE + 1)
        .read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() as u64 <= MAX_FILE,
        "{} is too large",
        path.display()
    );
    Ok(bytes)
}

fn private_dir(path: &Path) -> Result<()> {
    if !path.exists() {
        std::fs::create_dir_all(path)?;
    }
    privatefs::restrict_dir(path)
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_millis() as i64)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!("vibeface-client-{label}-{}", uuid::Uuid::new_v4()))
    }

    #[test]
    fn the_client_key_is_sealed_and_opens_only_with_its_passphrase() -> Result<()> {
        let dir = temp_dir("key");
        let created = ClientKey::create(&dir, "correct horse battery")?;
        let stored = std::fs::read_to_string(dir.join(KEY_FILE))?;
        assert!(!stored.contains(&created.identity.secret_hex()));
        assert!(ClientKey::create(&dir, "correct horse battery").is_err());
        assert!(ClientKey::open(&dir, "wrong passphrase").is_err());
        let mut opened = ClientKey::open(&dir, "correct horse battery")?;
        assert_eq!(opened.device_id(), created.device_id());
        let entry = Entry {
            origin: "http://127.0.0.1:4790".into(),
            node_id: "aa".repeat(32),
        };
        opened.remember(&"bb".repeat(32), entry.clone())?;
        let reopened = ClientKey::open(&dir, "correct horse battery")?;
        assert_eq!(
            (reopened.network_id(), reopened.entry()),
            (Some("bb".repeat(32).as_str()), Some(&entry))
        );
        std::fs::remove_dir_all(dir)?;
        Ok(())
    }

    #[test]
    fn remembered_route_preserves_numeric_hints_and_exact_node_across_restarts() -> Result<()> {
        let dir = temp_dir("remembered-route");
        let mut key = ClientKey::create(&dir, "fixture passphrase")?;
        let node = Identity::generate()?.device_id();
        let network = Identity::generate()?.device_id();
        let entry = Entry {
            origin: "https://gateway.example".into(),
            node_id: node.clone(),
        };
        key.remember_endpoint(
            &network,
            Endpoint::Preferred {
                gateway: entry.clone(),
                addresses: vec!["127.0.0.1:4790".into(), "[::1]:4790".into()],
            },
        )?;
        drop(key);
        let mut key = ClientKey::open(&dir, "fixture passphrase")?;
        let Endpoint::Preferred { gateway, addresses } = key.remembered_endpoint()? else {
            bail!("preferred route was not retained")
        };
        assert_eq!(gateway, entry);
        assert_eq!(addresses, vec!["127.0.0.1:4790", "[::1]:4790"]);
        assert!(
            key.remember_endpoint(
                &network,
                Endpoint::Lan {
                    node_id: node.clone(),
                    addresses: vec!["malformed:4790".into()]
                }
            )
            .is_err()
        );
        assert!(matches!(
            ClientKey::open(&dir, "fixture passphrase")?.remembered_endpoint()?,
            Endpoint::Preferred { .. }
        ));
        key.remember_endpoint(
            &network,
            Endpoint::Lan {
                node_id: node.clone(),
                addresses: vec!["127.0.0.1:4790".into()],
            },
        )?;
        let reopened = ClientKey::open(&dir, "fixture passphrase")?;
        assert!(
            reopened.entry().is_none(),
            "direct-only entries never manufacture a Gateway origin"
        );
        assert_eq!(reopened.remembered_endpoint()?.node_id(), node);
        std::fs::remove_dir_all(dir)?;
        Ok(())
    }
    #[test]
    fn recovery_files_round_trip_and_never_overwrite() -> Result<()> {
        let dir = temp_dir("recovery");
        private_dir(&dir)?;
        let (root, client, node) = (
            Identity::generate()?,
            Identity::generate()?,
            Identity::generate()?,
        );
        let genesis = vibeface_secure::membership::Genesis::create(
            &root,
            Member::client(&client, "cli", true)?,
            Member::node(&node, "node")?,
        )?;
        let material = RecoveryMaterial {
            version: 1,
            network_id: genesis.network_id.clone(),
            root_secret_key: Zeroizing::new(root.secret_hex()),
            genesis,
            checkpoints: vec![],
            events: vec![],
        };
        let path = dir.join("recovery.json");
        write_recovery_file(&path, &material, "recovery password")?;
        assert!(write_recovery_file(&path, &material, "recovery password").is_err());
        assert!(read_recovery_file(&path, "not the password").is_err());
        let read = read_recovery_file(&path, "recovery password")?;
        assert_eq!(
            Identity::from_secret_hex(&read.root_secret_key)?.device_id(),
            root.device_id()
        );
        std::fs::remove_dir_all(dir)?;
        Ok(())
    }

    #[tokio::test]
    async fn recovery_lost_reply_is_read_back_once_and_refusal_does_not_enroll() -> Result<()> {
        use crate::connection::Refused;
        use futures_util::{SinkExt, StreamExt};
        use tokio_tungstenite::tungstenite::Message;
        use vibeface_secure::Handshake;

        for accepted in [true, false] {
            let dir = temp_dir("recover-reconcile");
            let mut key = ClientKey::create(&dir, "client passphrase")?;
            let root = Identity::generate()?;
            let node = Identity::generate()?;
            let genesis = vibeface_secure::membership::Genesis::create(
                &root,
                Member::client(&Identity::generate()?, "previous client", true)?,
                Member::node(&node, "node")?,
            )?;
            let network = genesis.network_id.clone();
            let recovery_file = dir.join("recovery.json");
            write_recovery_file(
                &recovery_file,
                &RecoveryMaterial {
                    version: 1,
                    network_id: network.clone(),
                    root_secret_key: Zeroizing::new(root.secret_hex()),
                    genesis,
                    checkpoints: vec![],
                    events: vec![],
                },
                "recovery passphrase",
            )?;
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
            let entry = Entry {
                origin: format!("http://{}", listener.local_addr()?),
                node_id: node.device_id(),
            };
            let server_network = network.clone();
            let serving = tokio::spawn(async move {
                let mut checkpoint = None;
                let mut writes = 0;
                for _ in 0..3 {
                    let (stream, _) = listener.accept().await?;
                    let mut socket = tokio_tungstenite::accept_async(stream).await?;
                    assert!(matches!(socket.next().await.unwrap()?, Message::Text(_)));
                    let mut handshake = Handshake::signed(false, &node, None, PROLOGUE)?;
                    let frame = socket.next().await.unwrap()?;
                    handshake.read(&frame.into_data())?;
                    socket
                        .send(Message::Binary(handshake.write(&[])?.into()))
                        .await?;
                    let frame = socket.next().await.unwrap()?;
                    handshake.read(&frame.into_data())?;
                    let mut channel = handshake.finish()?;
                    while let Some(frame) = socket.next().await {
                        let Some(plain) = channel.open_record(&frame?.into_data())? else {
                            continue;
                        };
                        let request: Value = serde_json::from_slice(&plain)?;
                        let result = match request["method"].as_str().unwrap() {
                            "info" => json!({"initialized":false}),
                            "recover" => {
                                writes += 1;
                                let state: NetworkState = serde_json::from_value(json!({
                                    "genesis":request["params"]["genesis"],
                                    "checkpoints":request["params"]["checkpoints"],
                                    "events":request["params"]["events"],
                                }))?;
                                state.project()?;
                                checkpoint = Some(state.checkpoints[0].hash()?);
                                if accepted {
                                    break;
                                }
                                let response =
                                    json!({"id":request["id"],"error":"recovery refused"});
                                for record in
                                    channel.seal_message(&serde_json::to_vec(&response)?)?
                                {
                                    socket.send(Message::Binary(record.into())).await?;
                                }
                                break;
                            }
                            "authenticate" => {
                                if accepted {
                                    json!({"network_id":server_network,"recovery":{"status":"waiting","checkpoint_hash":checkpoint}})
                                } else {
                                    json!({})
                                }
                            }
                            method => panic!("unexpected method {method}"),
                        };
                        let response = json!({"id":request["id"],"result":result});
                        for record in channel.seal_message(&serde_json::to_vec(&response)?)? {
                            socket.send(Message::Binary(record.into())).await?;
                        }
                        if matches!(request["method"].as_str(), Some("info" | "authenticate")) {
                            break;
                        }
                    }
                }
                Ok::<_, anyhow::Error>(writes)
            });
            let mut plan = prepare_recovery(
                &key,
                entry.clone(),
                &recovery_file,
                "recovery passphrase",
                &dir.join("next-recovery.json"),
                RecoveryOptions {
                    name: "recovered client",
                    capability: Some("fixture-capability"),
                    members: None,
                },
            )
            .await?;
            assert_eq!(plan.state(), IdentityOperationState::Prepared);
            assert!(key.network_id().is_none());
            let result = plan.submit(&mut key).await;
            assert_eq!(serving.await??, 1, "recover must never be resubmitted");
            let reopened = ClientKey::open(&dir, "client passphrase")?;
            if accepted {
                assert_eq!(result?["recovery"]["status"], "waiting");
                assert_eq!(plan.state(), IdentityOperationState::Recorded);
                assert_eq!(reopened.network_id(), Some(network.as_str()));
                assert_eq!(reopened.entry(), Some(&entry));
            } else {
                assert!(result.unwrap_err().is::<Refused>());
                assert_eq!(plan.state(), IdentityOperationState::Unknown);
                assert!(reopened.network_id().is_none() && reopened.entry().is_none());
            }
            drop(plan);
            std::fs::remove_dir_all(dir)?;
        }
        Ok(())
    }

    #[test]
    fn gateway_urls_become_client_websockets() -> Result<()> {
        assert_eq!(
            crate::connection::client_url("http://127.0.0.1:4790")?,
            "ws://127.0.0.1:4790/api/client"
        );
        assert_eq!(
            crate::connection::client_url("https://node.example/app?x=1")?,
            "wss://node.example/api/client"
        );
        assert!(crate::connection::client_url("ftp://node.example").is_err());
        Ok(())
    }
}
