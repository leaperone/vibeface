//! The CLI as a management client of a Node.
//!
//! It holds a device key of its own, sealed with a passphrase, and is approved into the
//! personal network like a browser or a phone: an invite, the short code both sides compare, a
//! manager's signed grant. It then signs exactly what those clients sign. The Node never signs
//! governance for it.

use crate::maintenance::Refused;
use anyhow::{Context, Result, bail, ensure};
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    time::Duration,
};
use tokio_tungstenite::tungstenite::Message;
use vibeface_bridge::platform::privatefs;
use vibeface_secure::{
    Channel, Handshake, Identity,
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
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
const CALL_TIMEOUT: Duration = Duration::from_secs(60);
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

/// `client.json`: the sealed key and, once it is a member, which network and Node it uses.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct KeyFile {
    version: u32,
    device_id: String,
    #[serde(default)]
    network_id: Option<String>,
    #[serde(default)]
    entry: Option<Entry>,
    envelope: Value,
}

/// The unlocked client key. The secret leaves memory only sealed, in `client.json`.
pub struct ClientKey {
    dir: PathBuf,
    identity: Identity,
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
            entry: None,
            envelope: serde_json::from_slice(&envelope)?,
        };
        let path = dir.join(KEY_FILE);
        let mut handle = privatefs::create_new(&path).with_context(|| {
            format!(
                "{} already holds a client key; it is used as is",
                path.display()
            )
        })?;
        std::io::Write::write_all(&mut handle, &serde_json::to_vec_pretty(&file)?)?;
        handle.sync_all()?;
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
        self.file.entry.as_ref()
    }

    /// Records the network this key is a member of and the Node it reaches it through.
    pub fn remember(&mut self, network_id: &str, entry: Entry) -> Result<()> {
        self.file.network_id = Some(network_id.to_owned());
        self.file.entry = Some(entry);
        privatefs::write_atomic(
            &self.dir.join(KEY_FILE),
            &serde_json::to_vec_pretty(&self.file)?,
        )
    }

    /// Opens a channel to the remembered Node, or to `entry` when given.
    pub async fn connect(&self, entry: Option<&Entry>) -> Result<Session<'_>> {
        let entry = entry
            .or(self.file.entry.as_ref())
            .context("this client is not enrolled yet; run vibeface client enroll")?
            .clone();
        let connection = Connection::open(&entry.origin, &self.identity, &entry.node_id).await?;
        Ok(Session {
            key: self,
            connection,
            entry,
        })
    }

    fn sign(&self, domain: &str, payload: &[u8]) -> Result<String> {
        Ok(self.identity.sign(domain, payload)?)
    }
}

/// One Noise channel to a Node, as the browser opens it on `/api/client`.
pub struct Connection {
    socket: tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
    channel: Channel,
    next: u64,
}

impl Connection {
    /// Connects through the Gateway at `origin` and pins `target`'s device key.
    pub async fn open(origin: &str, identity: &Identity, target: &str) -> Result<Self> {
        let (mut socket, _) = tokio::time::timeout(
            HANDSHAKE_TIMEOUT,
            tokio_tungstenite::connect_async(client_url(origin)?),
        )
        .await
        .context("Gateway connection timed out")??;
        let connect = json!({"type":"connect","target":target}).to_string();
        socket.send(Message::Text(connect.into())).await?;
        let mut handshake = Handshake::signed(true, identity, Some(target.to_owned()), PROLOGUE)?;
        socket
            .send(Message::Binary(handshake.write(&[])?.into()))
            .await?;
        loop {
            let frame = tokio::time::timeout(HANDSHAKE_TIMEOUT, socket.next())
                .await
                .context("client handshake timed out")?
                .context("the Gateway closed the connection")??;
            match frame {
                Message::Binary(bytes) => {
                    ensure!(
                        handshake.read(&bytes)?.is_empty(),
                        "unexpected client handshake payload"
                    );
                    break;
                }
                // A busy Gateway keeps the session in line; each notice restarts the wait.
                Message::Text(text) => {
                    let notice: Value = serde_json::from_str(&text)?;
                    if notice["type"] != "queued" {
                        bail!(
                            "the Gateway refused: {}",
                            notice["error"].as_str().unwrap_or("unknown reason")
                        );
                    }
                }
                Message::Close(_) => bail!("the Gateway closed the connection"),
                _ => {}
            }
        }
        socket
            .send(Message::Binary(handshake.write(&[])?.into()))
            .await?;
        ensure!(
            handshake.remote_id().as_deref() == Some(target),
            "the Node answered with another device key"
        );
        Ok(Self {
            socket,
            channel: handshake.finish()?,
            next: 0,
        })
    }

    /// One request. An error the Node answered with is `Refused`; any other leaves the outcome
    /// of a write unknown.
    pub async fn call(&mut self, method: &str, params: Value) -> Result<Value> {
        self.next += 1;
        let id = format!("cli-{}", self.next);
        let request = serde_json::to_vec(&json!({"id":id,"method":method,"params":params}))?;
        for record in self.channel.seal_message(&request)? {
            self.socket.send(Message::Binary(record.into())).await?;
        }
        tokio::time::timeout(CALL_TIMEOUT, async {
            loop {
                let frame = self
                    .socket
                    .next()
                    .await
                    .context("the Node closed the channel")??;
                let Message::Binary(bytes) = frame else {
                    ensure!(
                        !matches!(frame, Message::Close(_)),
                        "the Node closed the channel"
                    );
                    continue;
                };
                let Some(plain) = self.channel.open_record(&bytes)? else {
                    continue;
                };
                let mut response: Value = serde_json::from_slice(&plain)?;
                if response["id"].as_str() != Some(id.as_str()) {
                    continue;
                }
                if let Some(error) = response["error"].as_str() {
                    return Err(Refused(error.to_owned()).into());
                }
                return Ok(response["result"].take());
            }
        })
        .await
        .context("the Node did not answer in time; the outcome is unknown")?
    }
}

/// `http(s)://host` → `ws(s)://host/api/client`.
fn client_url(origin: &str) -> Result<String> {
    let mut url = reqwest::Url::parse(origin).context("the Gateway URL is invalid")?;
    let scheme = match url.scheme() {
        "http" => "ws",
        "https" => "wss",
        _ => bail!("the Gateway URL must start with http:// or https://"),
    };
    url.set_scheme(scheme)
        .map_err(|()| anyhow::anyhow!("the Gateway URL is invalid"))?;
    url.set_path("/api/client");
    url.set_query(None);
    Ok(url.to_string())
}

/// A connected client: what it signs, it signs with its own key.
pub struct Session<'a> {
    key: &'a ClientKey,
    pub connection: Connection,
    pub entry: Entry,
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
            self.entry.node_id == entry.device_id(),
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

/// The joiner's side of an invite, from `join` to the grant.
pub struct Enrollment<'a> {
    session: Session<'a>,
    pub join_id: String,
    nonce: Zeroizing<String>,
    /// What the first confirming answer named; a later one must agree.
    admin: Option<(String, String, String)>,
}

/// How far an enrollment got.
pub enum Progress {
    Waiting,
    /// Compare this code with the one the approving device shows.
    Code(String),
    Joined,
}

impl<'a> Enrollment<'a> {
    /// Sends `join` for `link` through `origin`, whose Gateway reaches the invite's Node.
    pub async fn start(key: &'a ClientKey, origin: &str, link: &str, name: &str) -> Result<Self> {
        let ticket = Ticket::from_link(link)?;
        let entry = Entry {
            origin: origin.to_owned(),
            node_id: ticket.entry_id(),
        };
        let mut session = key.connect(Some(&entry)).await?;
        let authenticated = session.call("authenticate", json!({})).await?;
        ensure!(
            authenticated["revoked"] != true,
            "this client key was removed from the network; delete its key file and enroll again"
        );
        ensure!(
            authenticated["principal"].is_null(),
            "this client is already a member"
        );
        let nonce = Zeroizing::new(hex::encode(rand::random::<[u8; 32]>()));
        let request = session
            .call(
                "join",
                json!({"secret":hex::encode(ticket.secret),"public_key":key.device_id(),
                    "kind":"client","name":name,"commit":join_commit(key.device_id(), &nonce)?}),
            )
            .await?;
        ensure!(
            request["invite_id"] == invite_id(&ticket.invite_hash())?,
            "the Node answered for another invite"
        );
        Ok(Self {
            join_id: text(&request, "join_id")?.to_owned(),
            session,
            nonce,
            admin: None,
        })
    }

    /// One look at the join; reveals this side's nonce once a manager confirmed it.
    pub async fn poll(&mut self) -> Result<Progress> {
        let mut status = self
            .session
            .call("join_status", json!({"join_id":self.join_id}))
            .await?;
        match status["status"].as_str() {
            Some("pending") => Ok(Progress::Waiting),
            Some("confirming") => {
                let admin = (
                    text(&status, "network_id")?.to_owned(),
                    text(&status, "admin_id")?.to_owned(),
                    text(&status, "admin_nonce")?.to_owned(),
                );
                let pinned = self.admin.get_or_insert_with(|| admin.clone());
                ensure!(
                    *pinned == admin,
                    "the approving device changed mid-join; start again"
                );
                if status["joiner_nonce"].is_null() {
                    status = self
                        .session
                        .call(
                            "join_status",
                            json!({"join_id":self.join_id,"reveal":self.nonce.as_str()}),
                        )
                        .await?;
                }
                Ok(Progress::Code(short_code(
                    &admin.0,
                    text(&status, "invite_id")?,
                    self.session.key.device_id(),
                    &admin.1,
                    &admin.2,
                    &self.nonce,
                )?))
            }
            Some("joined") => {
                let state: NetworkState = serde_json::from_value(status["state"].take())?;
                let network = text(&status, "network_id")?;
                ensure!(
                    state.genesis.network_id == network
                        && self
                            .admin
                            .as_ref()
                            .is_none_or(|(pinned, ..)| pinned == network),
                    "the Node answered for another personal network"
                );
                state.project()?.client(self.session.key.device_id())?;
                Ok(Progress::Joined)
            }
            other => bail!(
                "the join ended: {} {}",
                other.unwrap_or("unknown"),
                status["reason"].as_str().unwrap_or_default()
            ),
        }
    }

    /// The network joined and the Node it was joined through.
    pub fn finish(self) -> Result<(String, Entry)> {
        let network = self
            .admin
            .map(|(network, ..)| network)
            .context("the join finished without a confirmation")?;
        Ok((network, self.session.entry))
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

fn write_recovery_file(path: &Path, material: &RecoveryMaterial, password: &str) -> Result<()> {
    let payload = Zeroizing::new(serde_json::to_vec(material)?);
    let envelope = recovery_seal(&payload, password)?;
    let file = RecoveryFile {
        version: 1,
        network_id: material.network_id.clone(),
        envelope: serde_json::from_slice(&envelope)?,
    };
    let mut handle = privatefs::create_new(path)
        .with_context(|| format!("{} already exists; choose a new file", path.display()))?;
    std::io::Write::write_all(&mut handle, &serde_json::to_vec_pretty(&file)?)?;
    handle.sync_all()?;
    Ok(())
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
pub async fn create_network(
    key: &mut ClientKey,
    entry: Entry,
    capability: &str,
    name: &str,
    node_name: Option<&str>,
    recovery_file: &Path,
    password: &str,
) -> Result<Value> {
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
    write_recovery_file(
        recovery_file,
        &RecoveryMaterial {
            version: 1,
            network_id: genesis.network_id.clone(),
            root_secret_key: Zeroizing::new(root.secret_hex()),
            genesis: genesis.clone(),
            checkpoints: vec![],
            events: vec![],
        },
        password,
    )?;
    let network = genesis.network_id.clone();
    let reply = session
        .call(
            "recover",
            json!({"genesis":genesis,"capability":capability}),
        )
        .await;
    drop(session);
    let created = match reply {
        Ok(created) => created,
        Err(error) if error.is::<Refused>() => return Err(error),
        // The outcome is unknown: the Node may still be installing the network or have lost the
        // reply. Ask it for a while before failing, never sending `recover` again.
        Err(error) => {
            let mut created = None;
            for _ in 0..12 {
                tokio::time::sleep(Duration::from_secs(5)).await;
                let reply = async {
                    key.connect(Some(&entry))
                        .await?
                        .call("authenticate", json!({}))
                        .await
                }
                .await;
                match reply {
                    Ok(reply) if reply["network_id"] == network.as_str() => {
                        created = Some(reply);
                        break;
                    }
                    Ok(reply) if reply["initialized"] == true => break,
                    _ => {}
                }
            }
            created.ok_or(error)?
        }
    };
    ensure!(
        created["principal"]["can_manage"] == true,
        "the Node did not make this client a manager"
    );
    key.remember(&network, entry)?;
    Ok(created)
}

/// Starts a recovery with `recovery_file`: every current member stays, this client joins as a
/// manager, and `new_file` (written before the Node hears of it) takes over the recovery key.
/// It takes effect after the recovery wait unless a manager stops it. An empty Node needs its
/// one-time `capability`.
pub async fn recover(
    key: &mut ClientKey,
    entry: Entry,
    recovery_file: &Path,
    password: &str,
    new_file: &Path,
    name: &str,
    capability: Option<&str>,
) -> Result<Value> {
    let material = read_recovery_file(recovery_file, password)?;
    let root = Identity::from_secret_hex(&material.root_secret_key)?;
    let me = key.device_id().to_owned();
    let mut session = key.connect(Some(&entry)).await?;
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
    members.push(Member::client(&session.key.identity, name, true)?);
    state.check_root(&root.device_id(), now, false)?;
    let next = Identity::generate()?;
    let retired = view.retire(&members, &heads)?;
    let checkpoint =
        RootCheckpoint::recovery(&state, &root, &next.device_id(), members, retired, now)?;
    state.clone().recover_at(checkpoint.clone(), now)?;
    let mut checkpoints = state.checkpoints.clone();
    checkpoints.push(checkpoint);
    write_recovery_file(
        new_file,
        &RecoveryMaterial {
            version: 1,
            network_id: material.network_id.clone(),
            root_secret_key: Zeroizing::new(next.secret_hex()),
            genesis: state.genesis.clone(),
            checkpoints: checkpoints.clone(),
            events: state.events.clone(),
        },
        password,
    )?;
    let mut params =
        json!({"genesis":state.genesis,"checkpoints":checkpoints,"events":state.events});
    if fresh {
        params["capability"] = json!(capability.context("an empty Node needs its capability")?);
    }
    let result = session.call("recover", params).await?;
    drop(session);
    key.remember(&material.network_id, entry)?;
    Ok(result)
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

    #[test]
    fn gateway_urls_become_client_websockets() -> Result<()> {
        assert_eq!(
            client_url("http://127.0.0.1:4790")?,
            "ws://127.0.0.1:4790/api/client"
        );
        assert_eq!(
            client_url("https://node.example/app?x=1")?,
            "wss://node.example/api/client"
        );
        assert!(client_url("ftp://node.example").is_err());
        Ok(())
    }
}
