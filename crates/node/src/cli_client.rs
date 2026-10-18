use anyhow::{Context, Result, bail, ensure};
use clap::{Args, Subcommand};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    fs::{self, File, OpenOptions},
    io::{BufRead, BufReader, Write},
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
    process::Command,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use vibeface::{client::Client, config, crypto::LocalIdentity};
use vibeface_secure::{
    Identity,
    authorization::{
        AccountId, AccountRight, DeviceCertificate, ObjectHash, PolicyPin, Signed, SpaceId,
        VerifiedClientAuthorization, client_policy_import as policy,
        enrollment::{self, DeviceEnrollmentIntent, offer},
    },
    connection::{ConnectionRoute, EnrollmentReference},
};

#[derive(Args)]
pub struct ClientCommand {
    #[command(subcommand)]
    action: ClientAction,
}

#[derive(Subcommand)]
enum ClientAction {
    /// Enroll an independent CLI identity using an invitation from your management device.
    Enroll {
        #[command(flatten)]
        profile: ProfileOptions,
        #[arg(long, visible_alias = "invitation-file")]
        connection_file: PathBuf,
    },
}

#[derive(Args)]
pub struct InviteCommand {
    #[command(subcommand)]
    action: InviteAction,
}

#[derive(Subcommand)]
enum InviteAction {
    /// Create a signed invitation using this CLI's enrolled management identity.
    Create {
        #[command(flatten)]
        profile: ProfileOptions,
        #[arg(long)]
        data_dir: Option<PathBuf>,
        #[arg(long)]
        via: Option<String>,
        #[arg(long)]
        out: Option<PathBuf>,
        /// Explicitly allow the new device to manage other devices.
        #[arg(long)]
        manage_devices: bool,
    },
    /// List requests for invitations created by this CLI profile.
    Pending {
        #[command(flatten)]
        profile: ProfileOptions,
    },
    /// Review a request, approve it, and wait for its independent possession proof.
    Approve {
        request_id: String,
        #[command(flatten)]
        profile: ProfileOptions,
    },
}

#[derive(Args)]
struct ProfileOptions {
    #[arg(long, default_value = "owner")]
    profile: String,
    /// Store CLI profiles separately from node service data.
    #[arg(long)]
    profiles_dir: Option<PathBuf>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Trust {
    account: AccountId,
    space: SpaceId,
    pins: Vec<PolicyPin>,
    route: ConnectionRoute,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Join {
    reference: EnrollmentReference,
    offer: String,
    intent: DeviceEnrollmentIntent,
    request_id: ObjectHash,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "state", content = "data", rename_all = "snake_case")]
enum ProfileState {
    New,
    Joining(Join),
    Ready(Trust),
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ManagedOffer {
    reference: EnrollmentReference,
    offer: String,
    published: bool,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PendingApproval {
    request_id: ObjectHash,
    offer_hash: ObjectHash,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Profile {
    version: u8,
    identity: LocalIdentity,
    state: ProfileState,
    offers: Vec<ManagedOffer>,
    approved_requests: Vec<PendingApproval>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct EncryptedProfile {
    format: String,
    version: u8,
    public_key: String,
    envelope: Value,
}

struct ProfileStore {
    _lock: File,
    path: PathBuf,
    password: String,
    profile: Profile,
}

fn secure<T>(value: std::result::Result<T, String>) -> Result<T> {
    value.map_err(anyhow::Error::msg)
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn json_value(input: &str) -> Result<Value> {
    serde_json::from_str(input).context("invalid protocol JSON")
}

fn text<'a>(value: &'a Value, key: &str) -> Result<&'a str> {
    value
        .get(key)
        .and_then(Value::as_str)
        .context(format!("missing {key}"))
}

fn profile_directory(options: &ProfileOptions) -> Result<PathBuf> {
    ensure!(
        !options.profile.is_empty()
            && options.profile.len() <= 64
            && options
                .profile
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_')),
        "profile name must contain only letters, digits, - or _"
    );
    let root = match &options.profiles_dir {
        Some(path) => path.clone(),
        None => {
            let directory = match std::env::var_os("XDG_CONFIG_HOME") {
                Some(path) => PathBuf::from(path),
                None => PathBuf::from(
                    std::env::var_os("HOME").context("HOME is unavailable; set --profiles-dir")?,
                )
                .join(".config"),
            };
            ensure!(
                directory.is_absolute(),
                "CLI configuration directory must be absolute"
            );
            directory.join("vibeface/clients")
        }
    };
    for directory in [&root, &root.join(&options.profile)] {
        match fs::symlink_metadata(directory) {
            Ok(_) => config::verify_private_dir(directory)?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                config::private_dir(directory)?;
                config::verify_private_dir(directory)?;
            }
            Err(error) => return Err(error.into()),
        }
    }
    Ok(root.join(&options.profile))
}

fn private_file(path: &Path) -> Result<File> {
    let before = fs::symlink_metadata(path)?;
    ensure!(
        before.is_file()
            && !before.file_type().is_symlink()
            && before.uid() == config::effective_uid()
            && before.mode() & 0o077 == 0,
        "CLI profile must be a private regular file owned by the current user"
    );
    let file = File::open(path)?;
    let after = file.metadata()?;
    ensure!(
        before.ino() == after.ino() && before.dev() == after.dev(),
        "CLI profile changed while opening"
    );
    Ok(file)
}

fn encode_profile(profile: &Profile, password: &str) -> Result<Vec<u8>> {
    let mut cleartext = serde_json::to_vec(profile)?;
    let encrypted = secure(vibeface_secure::recovery_seal(&cleartext, password));
    cleartext.fill(0);
    let envelope = EncryptedProfile {
        format: "vibeface-cli-profile".into(),
        version: 1,
        public_key: profile.identity.public_key_hex()?,
        envelope: serde_json::from_slice(&encrypted?)?,
    };
    Ok(serde_json::to_vec(&envelope)?)
}

fn decode_profile(input: &[u8], password: &str) -> Result<Profile> {
    let envelope: EncryptedProfile = serde_json::from_slice(input)?;
    ensure!(
        envelope.format == "vibeface-cli-profile" && envelope.version == 1,
        "unsupported CLI profile format"
    );
    let mut cleartext = secure(vibeface_secure::recovery_open(
        &serde_json::to_vec(&envelope.envelope)?,
        password,
    ))?;
    let decoded = serde_json::from_slice::<Profile>(&cleartext);
    cleartext.fill(0);
    let profile = decoded?;
    ensure!(
        profile.version == 1 && profile.identity.public_key_hex()? == envelope.public_key,
        "CLI profile identity does not match its encrypted contents"
    );
    Ok(profile)
}

struct EchoGuard {
    state: String,
    flag: &'static str,
}
impl Drop for EchoGuard {
    fn drop(&mut self) {
        let _ = Command::new("stty")
            .args([self.flag, "/dev/tty", &self.state])
            .status();
    }
}

fn read_secret(prompt: &str) -> Result<String> {
    let mut tty = OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/tty")
        .context("an interactive terminal is required to unlock the CLI identity")?;
    #[cfg(target_os = "macos")]
    let flag = "-f";
    #[cfg(not(target_os = "macos"))]
    let flag = "-F";
    let state = Command::new("stty")
        .args([flag, "/dev/tty", "-g"])
        .output()?;
    ensure!(state.status.success(), "cannot read terminal echo state");
    let state = String::from_utf8(state.stdout)?.trim().to_owned();
    ensure!(
        Command::new("stty")
            .args([flag, "/dev/tty", "-echo"])
            .status()?
            .success(),
        "cannot hide terminal input"
    );
    let guard = EchoGuard { state, flag };
    write!(tty, "{prompt}")?;
    tty.flush()?;
    let mut input = String::new();
    BufReader::new(&tty).read_line(&mut input)?;
    drop(guard);
    writeln!(tty)?;
    Ok(input.trim_end_matches(['\r', '\n']).to_owned())
}

impl ProfileStore {
    fn open(options: &ProfileOptions, create: bool) -> Result<Self> {
        let directory = profile_directory(options)?;
        let lock_path = directory.join("profile.lock");
        match fs::symlink_metadata(&lock_path) {
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                super::create_private(&lock_path, b"")?
            }
            Err(error) => return Err(error.into()),
        }
        let lock = private_file(&lock_path)?;
        lock.try_lock()
            .context("another command is using this CLI profile")?;
        let path = directory.join("profile.json");
        let (profile, password) = match fs::symlink_metadata(&path) {
            Ok(_) => {
                let file = private_file(&path)?;
                ensure!(
                    file.metadata()?.len() <= 4 * 1024 * 1024,
                    "CLI profile is too large"
                );
                let password = read_secret("CLI profile passphrase: ")?;
                let mut encoded = Vec::new();
                std::io::Read::read_to_end(
                    &mut std::io::Read::take(file, 4 * 1024 * 1024 + 1),
                    &mut encoded,
                )?;
                ensure!(encoded.len() <= 4 * 1024 * 1024, "CLI profile is too large");
                (decode_profile(&encoded, &password)?, password)
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound && create => {
                let password = read_secret("New CLI profile passphrase: ")?;
                let repeated = read_secret("Repeat passphrase: ")?;
                ensure!(password == repeated, "passphrases differ");
                let profile = Profile {
                    version: 1,
                    identity: LocalIdentity::generate(),
                    state: ProfileState::New,
                    offers: Vec::new(),
                    approved_requests: Vec::new(),
                };
                super::create_private(&path, &encode_profile(&profile, &password)?)?;
                (profile, password)
            }
            Err(error) => {
                return Err(error)
                    .context("CLI profile is not enrolled; run vibeface client enroll");
            }
        };
        Ok(Self {
            _lock: lock,
            path,
            password,
            profile,
        })
    }

    fn save(&self) -> Result<()> {
        private_file(&self.path)?;
        config::private_write(&self.path, &encode_profile(&self.profile, &self.password)?)
    }

    fn identity(&self) -> Result<Identity> {
        secure(Identity::from_secret_hex(
            &self.profile.identity.secret_key_hex,
        ))
    }

    fn trust(&self) -> Result<Trust> {
        match &self.profile.state {
            ProfileState::Ready(trust) => Ok(trust.clone()),
            _ => bail!("CLI enrollment is incomplete; resume vibeface client enroll"),
        }
    }
}

async fn connect(route: &ConnectionRoute, identity: &LocalIdentity) -> Result<Client> {
    secure(route.validate())?;
    let mut client = Client::connect(
        &route.endpoint,
        identity,
        &secure(route.node_id())?,
        route.relay,
    )
    .await?;
    let info = client.call("info", json!({})).await?;
    ensure!(
        info["public_key"] == route.node_public_key.as_str(),
        "connected node has another public key"
    );
    Ok(client)
}

async fn call(client: &mut Client, method: &str, params: Value) -> Result<Value> {
    client
        .call("identity", json!({"method":method,"params":params}))
        .await
}

async fn authorization(
    client: &mut Client,
    identity: &Identity,
    trust: &Trust,
) -> Result<VerifiedClientAuthorization> {
    let pins = serde_json::to_string(&trust.pins)?;
    let key = trust.route.node_public_key.as_str();
    let deadline = now() + 120_000;
    let request = secure(policy::build_client_policy_read_request(
        trust.account.as_str(),
        trust.space.as_str(),
        identity,
        key,
        &pins,
        "null",
        now() as f64,
        deadline as f64,
    ))?;
    let result = call(client, "begin_client_policy_read", json_value(&request)?).await?;
    let mut page = result["selections"].clone();
    let mut cursor = None;
    let mut snapshot = None;
    let selection = loop {
        let checked = json_value(&secure(policy::verify_client_policy_selections(
            &pins,
            &request,
            &page.to_string(),
            identity,
            key,
            now() as f64,
        ))?)?;
        let snapshot_id = text(&checked, "snapshot_id")?.to_owned();
        if let Some(previous) = &snapshot {
            ensure!(
                *previous == snapshot_id,
                "selection pages changed their signed snapshot"
            );
        } else {
            snapshot = Some(snapshot_id);
        }
        if let Some(selection) = checked["selections"]
            .as_array()
            .and_then(|items| items.first())
        {
            break selection.to_string();
        }
        ensure!(
            !checked["next_cursor"].is_null(),
            "the CLI identity has no authorized project at this node"
        );
        let next = text(&checked, "next_cursor")?.to_owned();
        ensure!(
            cursor.as_ref() != Some(&next),
            "selection pages did not advance"
        );
        cursor = Some(next);
        page = call(client, "client_policy_selections", json!({"snapshot_id":checked["snapshot_id"],"request_hash":checked["request_hash"],"cursor":checked["next_cursor"]})).await?["selections"].clone();
    };
    let request = secure(policy::build_client_policy_read_request(
        trust.account.as_str(),
        trust.space.as_str(),
        identity,
        key,
        &pins,
        &selection,
        now() as f64,
        deadline as f64,
    ))?;
    let begun = call(client, "begin_client_policy_read", json_value(&request)?).await?;
    let mut import = secure(policy::begin_client_policy_import(
        &pins,
        &request,
        &begun["snapshot"].to_string(),
        identity,
        key,
        now() as f64,
    ))?;
    loop {
        let response = call(
            client,
            "client_policy_page",
            json_value(&secure(import.page_request_json())?)?,
        )
        .await?;
        secure(import.append_page(&response["page"].to_string(), now() as f64))?;
        if response["page"]["payload"]["complete"] == true {
            break;
        }
    }
    let mut params = json_value(&secure(import.page_request_json())?)?;
    params["cursor"] = Value::Null;
    let witnessed = call(client, "client_policy_witness", params).await?;
    let verified =
        secure(import.finish(&witnessed["witness"].to_string(), &selection, now() as f64))?;
    let request = secure(policy::build_client_policy_auth_confirm_request(
        &verified,
        identity,
        now() as f64,
        deadline as f64,
    ))?;
    let reply = call(client, "confirm_client_policy_auth", json_value(&request)?).await?;
    secure(policy::confirm_client_policy_authorization(
        &verified,
        &request,
        &reply["confirmation"].to_string(),
        key,
        now() as f64,
    ))
}

fn update_trust(trust: &mut Trust, authorization: &VerifiedClientAuthorization) {
    for pin in &mut trust.pins {
        if let Some(head) = authorization
            .heads()
            .iter()
            .find(|head| head.domain == pin.domain)
        {
            pin.minimum_head = Some(head.clone());
        }
    }
}

async fn read_offer(
    client: &mut Client,
    identity: &Identity,
    reference: &EnrollmentReference,
    encoded: &str,
) -> Result<offer::VerifiedEnrollmentOffer> {
    let descriptor = secure(offer::verify_device_enrollment_offer(encoded, now() as f64))?;
    ensure!(
        secure(descriptor.offer_hash())? == reference.offer_hash.as_str()
            && descriptor.account_id() == reference.account_id.as_str()
            && descriptor.expires_at_ms() as u64 == reference.expires_at_ms
            && secure(descriptor.entry_json())?
                == serde_json::to_string(&secure(reference.route())?)?,
        "invitation differs from its signed account, target, route or expiry"
    );
    let request = secure(offer::build_enrollment_offer_policy_read_request(
        &descriptor,
        identity,
        now() as f64,
        reference.expires_at_ms.min(now() + 120_000) as f64,
    ))?;
    let begun = call(client, "enrollment_policy_read_begin", json!({"credential":{"purpose":"browser_device","credential":json_value(encoded)?},"request":json_value(&request)?})).await?;
    let mut import = secure(offer::begin_enrollment_offer_import(
        &descriptor,
        &request,
        &begun["snapshot"].to_string(),
        identity,
        reference.node_public_key.as_str(),
        now() as f64,
    ))?;
    loop {
        let response = call(
            client,
            "enrollment_policy_read_page",
            json_value(&secure(import.page_request_json())?)?,
        )
        .await?;
        secure(import.append_page(&response["page"].to_string(), now() as f64))?;
        if response["page"]["payload"]["complete"] == true {
            break;
        }
    }
    let mut params = json_value(&secure(import.page_request_json())?)?;
    params["cursor"] = Value::Null;
    let witnessed = call(client, "enrollment_policy_read_witness", params).await?;
    secure(import.finish(&witnessed["witness"].to_string(), now() as f64))
}

impl ClientCommand {
    pub async fn run(self) -> Result<()> {
        match self.action {
            ClientAction::Enroll {
                profile,
                connection_file,
            } => enroll(profile, connection_file).await,
        }
    }
}

async fn enroll(options: ProfileOptions, invitation_path: PathBuf) -> Result<()> {
    let bytes = super::read_bounded(&invitation_path, 64 * 1024)?;
    let input: EnrollmentReference = serde_json::from_slice(&bytes).context(
        "use a device invitation from an authorized manager, not a public connection card",
    )?;
    let mut store = ProfileStore::open(&options, true)?;
    let identity = store.identity()?;
    if let ProfileState::Ready(trust) = &store.profile.state {
        ensure!(
            input.account_id == trust.account
                && input.node_public_key == trust.route.node_public_key,
            "invitation belongs to another enrolled profile target"
        );
        println!("CLI profile is already enrolled.");
        return Ok(());
    }
    let reference = match &store.profile.state {
        ProfileState::Joining(join) => {
            ensure!(
                input == join.reference,
                "resume with the original invitation file"
            );
            input
        }
        _ => secure(EnrollmentReference::from_json(
            std::str::from_utf8(&bytes)?,
            now(),
        ))
        .context(
            "use a device invitation from an authorized manager, not a public connection card",
        )?,
    };
    let route = secure(reference.route())?;
    let mut client = connect(&route, &store.profile.identity).await?;
    let resuming = matches!(store.profile.state, ProfileState::Joining(_));
    let join = match &store.profile.state {
        ProfileState::Joining(join) => join.clone(),
        _ => {
            let response = call(
                &mut client,
                "enrollment_offer_get",
                json!({"account_id":reference.account_id,"offer_hash":reference.offer_hash}),
            )
            .await?;
            let encoded = response["offer"].to_string();
            let verified = read_offer(&mut client, &identity, &reference, &encoded).await?;
            let intent: DeviceEnrollmentIntent =
                serde_json::from_str(&secure(offer::create_device_enrollment_intent_from_offer(
                    &verified,
                    &identity,
                    now() as f64,
                ))?)?;
            let request_id = offer::enrollment_request_hash(&reference.offer_hash, &intent)?;
            let join = Join {
                reference: reference.clone(),
                offer: encoded,
                intent,
                request_id,
            };
            store.profile.state = ProfileState::Joining(join.clone());
            store.save()?;
            join
        }
    };
    let lookup = json!({"account_id":reference.account_id,"offer_hash":reference.offer_hash,"request_id":join.request_id});
    let mut status = if resuming {
        call(&mut client, "enrollment_status", lookup.clone())
            .await
            .context("could not reconcile the original enrollment; no new request was sent")?
    } else {
        json!({"status":"not_found"})
    };
    if status["status"] == "not_found" {
        status = call(&mut client, "enrollment_request", json!({"account_id":reference.account_id,"offer_hash":reference.offer_hash,"new_device_public_key":identity.public_key_hex(),"intent":join.intent})).await?;
    }
    println!("CLI device: {}", identity.node_id());
    println!("Request: {}", join.request_id.as_str());
    println!("Confirm this device in your existing management client.");
    loop {
        ensure!(
            status["request_id"] == join.request_id.as_str()
                && status["offer_hash"] == reference.offer_hash.as_str()
                && status["new_device_public_key"] == identity.public_key_hex()
                && status["unsigned_intent"] == serde_json::to_value(&join.intent)?,
            "node returned another enrollment request"
        );
        match text(&status, "status")? {
            "completed" => {
                let descriptor = secure(offer::verify_device_enrollment_offer(
                    &join.offer,
                    join.intent.certificate_not_before_ms as f64,
                ))?;
                let mut trust = Trust {
                    account: reference.account_id.clone(),
                    space: SpaceId::personal(&reference.account_id),
                    pins: serde_json::from_str(&secure(descriptor.pins_json())?)?,
                    route,
                };
                let auth = authorization(&mut client, &identity, &trust).await?;
                update_trust(&mut trust, &auth);
                store.profile.state = ProfileState::Ready(trust);
                store.save()?;
                println!("CLI profile enrolled and current authorization verified.");
                return Ok(());
            }
            "approved" => {
                let verified = read_offer(&mut client, &identity, &reference, &join.offer).await?;
                let qualified = secure(offer::verify_account_enrollment_from_offer(
                    &status["intent"].to_string(),
                    &verified,
                    now() as f64,
                ))?;
                ensure!(
                    status["intent"]["payload"] == serde_json::to_value(&join.intent)?,
                    "approval changed the original intent"
                );
                let certificate = secure(enrollment::sign_device_enrollment_possession(
                    &qualified,
                    &identity,
                    text(&status, "nonce")?,
                    now() as f64,
                ))?;
                call(&mut client, "enrollment_possession", json!({"account_id":reference.account_id,"offer_hash":reference.offer_hash,"request_id":join.request_id,"certificate":json_value(&certificate)?})).await?;
            }
            "requested" | "possessed" => {}
            _ => bail!("unknown enrollment status"),
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
        status = call(&mut client, "enrollment_status", json!({"account_id":reference.account_id,"offer_hash":reference.offer_hash,"request_id":join.request_id})).await?;
    }
}

impl InviteCommand {
    pub async fn run(self) -> Result<()> {
        match self.action {
            InviteAction::Create {
                profile,
                data_dir,
                via,
                out,
                manage_devices,
            } => create_invitation(profile, data_dir, via, out, manage_devices).await,
            InviteAction::Pending { profile } => pending_invitations(profile).await,
            InviteAction::Approve {
                profile,
                request_id,
            } => approve_invitation(profile, &request_id).await,
        }
    }
}

async fn manager(
    options: &ProfileOptions,
) -> Result<(ProfileStore, Client, Identity, VerifiedClientAuthorization)> {
    let mut store = ProfileStore::open(options, false)?;
    let mut trust = store.trust()?;
    let identity = store.identity()?;
    let mut client = connect(&trust.route, &store.profile.identity).await?;
    let authorization = authorization(&mut client, &identity, &trust).await?;
    ensure!(
        authorization
            .account_enrollment_rights()
            .contains(&AccountRight::EnrollDevice),
        "this CLI identity has no device enrollment permission"
    );
    update_trust(&mut trust, &authorization);
    store.profile.state = ProfileState::Ready(trust);
    store.save()?;
    Ok((store, client, identity, authorization))
}

async fn create_invitation(
    options: ProfileOptions,
    data_dir: Option<PathBuf>,
    via: Option<String>,
    out: Option<PathBuf>,
    manage_devices: bool,
) -> Result<()> {
    let (mut store, mut client, identity, authorization) = manager(&options).await?;
    let trust = store.trust()?;
    if let Some(directory) = data_dir {
        config::verify_data_format(&directory)?;
        ensure!(
            config::identity(&directory)?.public_key_hex()? == trust.route.node_public_key.as_str(),
            "selected node data belongs to another node"
        );
    }
    let route = match via {
        Some(endpoint) => secure(ConnectionRoute::new(
            &endpoint,
            trust.route.node_public_key.as_str(),
            true,
        ))?,
        None => trust.route.clone(),
    };
    if route != trust.route {
        connect(&route, &store.profile.identity).await?;
    }
    let rights = if manage_devices {
        authorization
            .account_enrollment_rights()
            .iter()
            .filter(|right| {
                matches!(
                    right,
                    AccountRight::EnrollDevice | AccountRight::RevokeDevice
                )
            })
            .cloned()
            .collect::<Vec<_>>()
    } else {
        Vec::new()
    };
    store.profile.offers.retain(|offer| {
        offer.reference.expires_at_ms > now()
            || store
                .profile
                .approved_requests
                .iter()
                .any(|approval| approval.offer_hash == offer.reference.offer_hash)
    });
    let reusable = store
        .profile
        .offers
        .iter()
        .position(|offer| !offer.published && offer.reference.expires_at_ms > now());
    let index = if let Some(index) = reusable {
        ensure!(
            secure(store.profile.offers[index].reference.route())? == route
                && serde_json::from_str::<Signed<offer::EnrollmentOffer>>(
                    &store.profile.offers[index].offer
                )?
                .payload
                .requested_rights
                    == rights,
            "an earlier invitation is awaiting reconciliation; resume its original route"
        );
        index
    } else {
        ensure!(
            store
                .profile
                .offers
                .iter()
                .filter(|offer| offer.reference.expires_at_ms > now())
                .count()
                < 32,
            "this profile has 32 active invitations; inspect pending requests before creating more"
        );
        let expiry = authorization
            .account_enrollment_expires_at_ms()
            .context("management certificate has no valid expiry")?;
        let encoded = secure(offer::create_device_enrollment_offer(
            &identity,
            trust.account.as_str(),
            &route.endpoint,
            route.node_public_key.as_str(),
            route.relay,
            &serde_json::to_string(&rights)?,
            expiry as f64,
            &authorization,
            now() as f64,
        ))?;
        let signed: Signed<offer::EnrollmentOffer> = serde_json::from_str(&encoded)?;
        let reference = secure(EnrollmentReference::from_offer(&signed))?;
        store.profile.offers.push(ManagedOffer {
            reference,
            offer: encoded,
            published: false,
        });
        store.save()?;
        store.profile.offers.len() - 1
    };
    let saved = store.profile.offers[index].clone();
    let reply = call(
        &mut client,
        "enrollment_offer_get",
        json!({"account_id":saved.reference.account_id,"offer_hash":saved.reference.offer_hash}),
    )
    .await?;
    if reply["status"] == "not_found" {
        let published = call(&mut client, "enrollment_offer_publish", json!({"offer":json_value(&saved.offer)?})).await
            .context("invitation publication is unconfirmed; rerun create to reconcile the original invitation")?;
        ensure!(
            published["offer_hash"] == saved.reference.offer_hash.as_str()
                && published["reference"] == serde_json::to_value(&saved.reference)?,
            "published invitation differs from the signed request"
        );
    } else {
        ensure!(
            reply["offer"] == json_value(&saved.offer)?,
            "existing invitation differs from the original signed request"
        );
    }
    store.profile.offers[index].published = true;
    store.save()?;
    let encoded = serde_json::to_vec(&saved.reference)?;
    if let Some(path) = out {
        let path = if path.is_absolute() {
            path
        } else {
            std::env::current_dir()?.join(path)
        };
        match fs::symlink_metadata(&path) {
            Ok(metadata) => {
                ensure!(
                    !metadata.file_type().is_symlink()
                        && metadata.is_file()
                        && super::read_bounded(&path, 64 * 1024)? == encoded,
                    "output already exists with different contents"
                );
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                super::create_private(&path, &encoded)?
            }
            Err(error) => return Err(error.into()),
        }
        println!("Invitation file: {}", path.display());
    } else {
        println!("{}", String::from_utf8(encoded)?);
    }
    eprintln!("Node: {}", secure(route.node_id())?);
    eprintln!(
        "Route: {} ({})",
        route.endpoint,
        if route.relay { "relay" } else { "direct" }
    );
    eprintln!("The new device still requires your explicit approval.");
    Ok(())
}

async fn pending_invitations(options: ProfileOptions) -> Result<()> {
    let (store, mut client, _, _) = manager(&options).await?;
    for offer in &store.profile.offers {
        if !offer.published || offer.reference.expires_at_ms <= now() {
            continue;
        }
        let response = call(&mut client, "enrollment_pending", json!({"account_id":offer.reference.account_id,"offer_hash":offer.reference.offer_hash})).await?;
        for request in response["requests"]
            .as_array()
            .context("node returned an invalid request list")?
        {
            println!(
                "{} {} {}",
                text(request, "request_id")?,
                text(request, "status")?,
                text(request, "new_device_public_key")?
            );
        }
    }
    Ok(())
}

fn confirm_request(request: &Value, reference: &EnrollmentReference) -> Result<()> {
    let mut tty = OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/tty")
        .context("an interactive terminal is required to approve a device")?;
    writeln!(
        tty,
        "Account: {}\nNode: {}\nRequest: {}\nNew device public key: {}\nPermissions: {}\nExpires: {}",
        reference.account_id.as_str(),
        reference.node_public_key.as_str(),
        text(request, "request_id")?,
        text(request, "new_device_public_key")?,
        request["unsigned_intent"]["requested_rights"],
        reference.expires_at_ms
    )?;
    write!(tty, "Approve this exact device? Type yes: ")?;
    tty.flush()?;
    let mut answer = String::new();
    BufReader::new(&tty).read_line(&mut answer)?;
    ensure!(
        answer.trim() == "yes",
        "approval cancelled; no authorization was submitted"
    );
    Ok(())
}

async fn approve_invitation(options: ProfileOptions, request_id: &str) -> Result<()> {
    let id = ObjectHash::parse(request_id)?;
    let (mut store, mut client, identity, mut auth) = manager(&options).await?;
    let mut found = None;
    for saved in &store.profile.offers {
        if !saved.published {
            continue;
        }
        let request = call(&mut client, "enrollment_status", json!({"account_id":saved.reference.account_id,"offer_hash":saved.reference.offer_hash,"request_id":id})).await?;
        if request["status"] != "not_found" {
            found = Some((saved.clone(), request));
            break;
        }
    }
    let (saved, mut request) =
        found.context("request does not belong to an invitation issued by this CLI identity")?;
    loop {
        let intent: DeviceEnrollmentIntent =
            serde_json::from_value(request["unsigned_intent"].clone())?;
        ensure!(
            request["request_id"] == id.as_str()
                && request["offer_hash"] == saved.reference.offer_hash.as_str()
                && offer::enrollment_request_hash(&saved.reference.offer_hash, &intent)? == id
                && request["new_device_public_key"] == intent.new_device_public_key.as_str(),
            "node changed the original enrollment request"
        );
        let state = text(&request, "status")?;
        if state == "completed" {
            store
                .profile
                .approved_requests
                .retain(|approval| approval.request_id != id);
            store.save()?;
            println!(
                "Enrollment is already recorded as completed for {}.",
                id.as_str()
            );
            return Ok(());
        }
        let descriptor = secure(offer::verify_device_enrollment_offer(
            &saved.offer,
            now() as f64,
        ))?;
        secure(offer::verify_device_enrollment_intent_for_manager(
            &auth,
            &descriptor,
            saved.reference.offer_hash.as_str(),
            &serde_json::to_string(&intent)?,
            intent.new_device_public_key.as_str(),
            now() as f64,
        ))?;
        if !store
            .profile
            .approved_requests
            .iter()
            .any(|approval| approval.request_id == id)
        {
            confirm_request(&request, &saved.reference)?;
            store.profile.approved_requests.push(PendingApproval {
                request_id: id.clone(),
                offer_hash: saved.reference.offer_hash.clone(),
            });
            store.save()?;
        }
        match state {
            "requested" => {
                let signed = secure(enrollment::sign_device_enrollment_intent(
                    &serde_json::to_string(&intent)?,
                    &identity,
                    &auth,
                    now() as f64,
                ))?;
                call(&mut client, "enrollment_approve", json!({"account_id":saved.reference.account_id,"offer_hash":saved.reference.offer_hash,"request_id":id,"intent":json_value(&signed)?})).await
                    .context("approval result is unknown; resume this same request to reconcile")?;
                println!(
                    "Approved. Waiting for the new device to prove possession of its own key."
                );
            }
            "approved" => {}
            "possessed" => {
                let signed_intent = request["intent"].to_string();
                let qualified = secure(enrollment::verify_account_enrollment_json(
                    &signed_intent,
                    &auth,
                    now() as f64,
                ))?;
                let certificate: DeviceCertificate =
                    serde_json::from_value(request["certificate"].clone())?;
                ensure!(
                    certificate.public_key == intent.new_device_public_key,
                    "possession certificate belongs to another device"
                );
                let prepared = secure(enrollment::prepare_account_device_enrollment(
                    &qualified,
                    &serde_json::to_string(&certificate)?,
                    &identity,
                    &auth,
                    now() as f64,
                ))?;
                let reply = call(&mut client, "account_device_commit", json!({"request":json_value(&secure(prepared.request_json())?)?,"request_id":id})).await
                    .context("certificate commit is unconfirmed; resume the original request before any new enrollment")?;
                let completed = secure(enrollment::finish_account_device_change(
                    &prepared,
                    &reply["committed"].to_string(),
                    now() as f64,
                ))?;
                let mut trust = store.trust()?;
                trust.pins = serde_json::from_str(&secure(completed.pins_json())?)?;
                store.profile.state = ProfileState::Ready(trust);
                store
                    .profile
                    .approved_requests
                    .retain(|approval| approval.request_id != id);
                store.save()?;
                println!("Device enrolled: {}", intent.new_device_public_key.as_str());
                return Ok(());
            }
            _ => bail!("unknown enrollment status"),
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
        request = call(&mut client, "enrollment_status", json!({"account_id":saved.reference.account_id,"offer_hash":saved.reference.offer_hash,"request_id":id})).await?;
        auth = authorization(&mut client, &identity, &store.trust()?).await?;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cli_profile_encrypts_an_independent_identity_and_rejects_tampering() {
        let profile = Profile {
            version: 1,
            identity: LocalIdentity::generate(),
            state: ProfileState::New,
            offers: vec![],
            approved_requests: vec![],
        };
        let password = "independent client test password";
        let encrypted = encode_profile(&profile, password).unwrap();
        assert!(
            !std::str::from_utf8(&encrypted)
                .unwrap()
                .contains(&profile.identity.secret_key_hex)
        );
        assert_eq!(
            decode_profile(&encrypted, password)
                .unwrap()
                .identity
                .public_key_hex()
                .unwrap(),
            profile.identity.public_key_hex().unwrap()
        );
        assert!(decode_profile(&encrypted, "wrong password").is_err());
        let mut value: Value = serde_json::from_slice(&encrypted).unwrap();
        value["public_key"] = json!("00".repeat(32));
        assert!(decode_profile(&serde_json::to_vec(&value).unwrap(), password).is_err());
    }

    #[test]
    fn cli_profile_names_cannot_escape_the_profile_directory() {
        for name in ["../other", "a/b", "", ".", "profile name"] {
            assert!(
                profile_directory(&ProfileOptions {
                    profile: name.into(),
                    profiles_dir: Some(PathBuf::from("/unused"))
                })
                .is_err()
            );
        }
    }
}
