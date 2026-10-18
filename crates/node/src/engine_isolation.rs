//! Resource-scoped Docker execution. The runtime owns authorization and durable leases.
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    process::Stdio,
    sync::Arc,
    time::Duration,
};
use tokio::{
    process::Command,
    sync::{Mutex, mpsc},
};
use vibeface_bridge::codex::CodexClient;
use vibeface_secure::authorization::{ResourceId, SpaceId};

const CONTAINER_HOME: &str = "/home/vibeface";
const COMMAND_TIMEOUT: Duration = Duration::from_secs(15);
const WATCHDOG: &str = "import ctypes,os,signal,time\nif ctypes.CDLL(None,use_errno=True).prctl(4,0,0,0,0)!=0: os._exit(78)\nsignal.signal(signal.SIGTERM,lambda *_:os._exit(0))\nwhile True:\n try: n=int(open('/run/vibeface-control/deadline').read())\n except Exception: os._exit(78)\n if n<=0 or time.time_ns()//1000000>=n: os._exit(77)\n time.sleep(0.1)";

#[derive(Clone, Debug)]
pub struct ExecutionContext {
    pub space_id: SpaceId,
    pub operation_id: String,
    pub resource_id: ResourceId,
    pub shared: bool,
    pub lease_expires_at_ms: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionIsolationConfig {
    pub image: String,
    pub docker_bin: PathBuf,
    #[serde(default)]
    pub egress: Option<ExecutionEgressConfig>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionEgressConfig {
    pub gateway_container: String,
    pub gateway_port: u16,
    pub codex_model: String,
}

impl ExecutionIsolationConfig {
    pub fn validate(&self) -> Result<()> {
        let digest = self.image.rsplit_once("sha256:").map(|(_, digest)| digest);
        ensure!(
            (self.image.starts_with("sha256:") || self.image.contains("@sha256:"))
                && digest.is_some_and(|digest| digest.len() == 64
                    && digest
                        .bytes()
                        .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())),
            "execution image must be pinned to a sha256 digest"
        );
        ensure!(
            self.docker_bin.is_absolute() && self.docker_bin.is_file(),
            "Docker executable must be an existing absolute file"
        );
        if let Some(egress) = &self.egress {
            vibeface_protocol::validate_id(&egress.gateway_container)
                .map_err(anyhow::Error::msg)?;
            ensure!(
                egress.gateway_port != 0,
                "model gateway port must be positive"
            );
            ensure!(
                !egress.codex_model.trim().is_empty()
                    && egress.codex_model.len() <= 128
                    && !egress.codex_model.contains(['\n', '\r']),
                "invalid isolated Codex model"
            );
        }
        Ok(())
    }
}

#[derive(Clone)]
pub(super) struct DockerIsolation {
    config: ExecutionIsolationConfig,
    data_dir: PathBuf,
    scopes: Arc<Mutex<BTreeMap<String, IsolatedScope>>>,
    project_read_only: bool,
    command_scope: bool,
}

struct IsolatedScope {
    handle: IsolatedProcess,
    operations: BTreeMap<String, u64>,
    codex: Option<CodexClient>,
    stopped: bool,
}

pub(super) struct StoppedScope {
    node: String,
    key: String,
    name: String,
    id: Option<String>,
    network_id: Option<String>,
}

#[derive(Clone)]
pub(crate) struct IsolatedProcess {
    config: ExecutionIsolationConfig,
    name: String,
    id: String,
    control: PathBuf,
    gateway_url: Option<String>,
}

impl DockerIsolation {
    pub fn new(config: ExecutionIsolationConfig, data_dir: PathBuf) -> Self {
        Self {
            config,
            data_dir,
            scopes: Arc::new(Mutex::new(BTreeMap::new())),
            project_read_only: false,
            command_scope: false,
        }
    }

    fn scope_key(context: &ExecutionContext) -> String {
        hash(&format!("{}\0{}", context.space_id, context.resource_id))
    }

    pub fn manager(config: ExecutionIsolationConfig, data_dir: PathBuf) -> Self {
        Self {
            project_read_only: true,
            ..Self::new(config, data_dir)
        }
    }

    pub fn command(config: ExecutionIsolationConfig, data_dir: PathBuf) -> Self {
        Self {
            command_scope: true,
            ..Self::new(config, data_dir)
        }
    }

    fn key(&self, context: &ExecutionContext) -> String {
        if self.command_scope {
            hash(&format!(
                "command\0{}\0{}\0{}",
                context.space_id, context.resource_id, context.operation_id
            ))
        } else if self.project_read_only {
            hash(&format!(
                "manager\0{}\0{}\0{}",
                context.space_id, context.resource_id, context.operation_id
            ))
        } else {
            Self::scope_key(context)
        }
    }

    pub async fn prepare(
        &self,
        context: &ExecutionContext,
        project: &Path,
    ) -> Result<IsolatedProcess> {
        self.config.validate()?;
        validate_lease(context.lease_expires_at_ms)?;
        if self.project_read_only && context.operation_id.starts_with("manager:") {
            let (trigger, attempt) = context.operation_id["manager:".len()..]
                .split_once(':')
                .context("manager isolation ID has no attempt")?;
            ensure!(
                context.operation_id.len() <= 128,
                "manager isolation ID is too long"
            );
            vibeface_protocol::validate_id(trigger).map_err(anyhow::Error::msg)?;
            vibeface_protocol::validate_id(attempt).map_err(anyhow::Error::msg)?;
        } else {
            vibeface_protocol::validate_id(&context.operation_id).map_err(anyhow::Error::msg)?;
        }
        let data_dir = self
            .data_dir
            .canonicalize()
            .context("canonical Node data directory")?;
        crate::config::verify_private_dir(&data_dir)?;
        let project = project
            .canonicalize()
            .context("canonical execution project")?;
        ensure!(
            project.is_dir()
                && project.parent().is_some()
                && !data_dir.starts_with(&project)
                && !project.starts_with(&data_dir),
            "project and private Node data directories must be separate"
        );
        let uid = crate::config::effective_uid();
        ensure!(uid != 0, "shared execution requires a non-root Node user");
        let key = self.key(context);
        let node = hash(
            data_dir
                .to_str()
                .context("Node data directory must be UTF-8")?,
        );
        let name = format!("vibeface-{}-{}", &node[..16], &key[..32]);
        let mut scopes = self.scopes.lock().await;
        if let Some(scope) = scopes.get_mut(&key) {
            ensure!(
                !scope.stopped,
                "isolated resource was stopped; reconcile its operations before replacing the container"
            );
            self.verify_container(&scope.handle.id, &node, &key, &project, uid)
                .await?;
            ensure!(
                scope
                    .operations
                    .values()
                    .all(|deadline| *deadline > vibeface_protocol::now_ms()),
                "execution lease already expired; do not revive an operation"
            );
            scope
                .operations
                .insert(context.operation_id.clone(), context.lease_expires_at_ms);
            write_deadline(scope)?;
            return Ok(scope.handle.clone());
        }
        ensure!(
            self.inspect(&name).await?.is_none(),
            "existing isolated container requires reconciliation; do not restart or extend unknown operations"
        );
        let execution_dir = private_child(&data_dir, "execution")?;
        let scope_dir = private_child(&execution_dir, &key)?;
        let home = private_child(&scope_dir, "home")?;
        private_child(&home, ".codex")?;
        let control = private_child(&scope_dir, "control")?;
        crate::config::private_write(
            &control.join("operations.json"),
            &serde_json::to_vec(&BTreeMap::from([(
                context.operation_id.clone(),
                context.lease_expires_at_ms,
            )]))?,
        )?;
        crate::config::private_write(
            &control.join("deadline"),
            context.lease_expires_at_ms.to_string().as_bytes(),
        )?;
        for credential in [
            ".codex/auth.json",
            ".claude/.credentials.json",
            ".cursor/auth.json",
        ] {
            ensure!(
                !home.join(credential).try_exists()?,
                "shared Provider credentials must remain in the trusted model gateway"
            );
        }
        let (network, gateway_url) = self.egress_network(&node, &key, true).await?;
        match self.inspect(&name).await? {
            Some(_) => {
                self.verify_container(&name, &node, &key, &project, uid)
                    .await?
            }
            None => {
                let mut command = self.docker();
                command.args([
                    "run",
                    "--detach",
                    "--name",
                    &name,
                    "--read-only",
                    "--cap-drop",
                    "ALL",
                    "--security-opt",
                    "no-new-privileges",
                    "--pids-limit",
                    "128",
                    "--user",
                    &format!("{uid}:{}", effective_gid()),
                    "--network",
                    &network,
                    "--workdir",
                    "/project",
                    "--label",
                    &format!("vibeface.node={node}"),
                    "--label",
                    &format!("vibeface.scope={key}"),
                    "--label",
                    &format!("vibeface.project-read-only={}", self.project_read_only),
                    "--label",
                    &format!(
                        "vibeface.project={}",
                        hash(project.to_str().context("project path must be UTF-8")?)
                    ),
                    "--mount",
                    &format!(
                        "{}{}",
                        bind_mount(&project, "/project")?,
                        if self.project_read_only {
                            ",readonly"
                        } else {
                            ""
                        }
                    ),
                    "--mount",
                    &bind_mount(&home, CONTAINER_HOME)?,
                    "--mount",
                    &format!(
                        "{},readonly",
                        bind_mount(&control, "/run/vibeface-control")?
                    ),
                    "--tmpfs",
                    "/tmp:rw,nosuid,nodev,noexec,size=67108864",
                    "--entrypoint",
                    "/usr/bin/env",
                    &self.config.image,
                    "-i",
                    "PATH=/usr/local/bin:/usr/bin:/bin",
                    "HOME=/home/vibeface",
                    "/usr/bin/python3",
                    "-I",
                    "-S",
                    "-c",
                    WATCHDOG,
                ]);
                checked_output(command, "create isolated resource container").await?;
                self.verify_container(&name, &node, &key, &project, uid)
                    .await?;
            }
        }
        let id = self
            .inspect(&name)
            .await?
            .context("new isolated container disappeared")?["Id"]
            .as_str()
            .context("isolated container ID missing")?
            .to_owned();
        let handle = IsolatedProcess {
            config: self.config.clone(),
            name,
            id,
            control,
            gateway_url,
        };
        scopes.insert(
            key,
            IsolatedScope {
                handle: handle.clone(),
                operations: BTreeMap::from([(
                    context.operation_id.clone(),
                    context.lease_expires_at_ms,
                )]),
                codex: None,
                stopped: false,
            },
        );
        Ok(handle)
    }

    pub async fn codex(
        &self,
        context: &ExecutionContext,
        prepared: &IsolatedProcess,
        events: mpsc::UnboundedSender<Value>,
    ) -> Result<CodexClient> {
        let mut scopes = self.scopes.lock().await;
        let scope = scopes
            .get_mut(&self.key(context))
            .context("isolated resource is not running")?;
        ensure!(
            !scope.stopped
                && scope.handle.id == prepared.id
                && scope.operations.contains_key(&context.operation_id),
            "isolated operation generation is no longer active"
        );
        if scope.codex.is_none() {
            let mut command = scope.handle.command("codex");
            command.args(["app-server", "--listen", "stdio://"]);
            if let Some(egress) = &self.config.egress {
                command.args([
                    "-c",
                    &format!("model={:?}", egress.codex_model),
                    "-c",
                    "model_provider=\"vibeface_gateway\"",
                    "-c",
                    "model_providers.vibeface_gateway.name=\"Vibeface model gateway\"",
                    "-c",
                    &format!(
                        "model_providers.vibeface_gateway.base_url={:?}",
                        format!("{}v1", scope.handle.gateway_url.as_ref().unwrap())
                    ),
                    "-c",
                    "model_providers.vibeface_gateway.env_key=\"OPENAI_API_KEY\"",
                    "-c",
                    "model_providers.vibeface_gateway.wire_api=\"responses\"",
                ]);
            }
            scope.codex = Some(CodexClient::start_process(command, events));
        }
        Ok(scope.codex.as_ref().unwrap().clone())
    }

    pub async fn existing_codex(
        &self,
        context: &ExecutionContext,
    ) -> Result<(CodexClient, String)> {
        let scopes = self.scopes.lock().await;
        let scope = scopes
            .get(&self.key(context))
            .context("isolated resource is not connected; reconcile its original container")?;
        ensure!(
            !scope.stopped && scope.operations.contains_key(&context.operation_id),
            "isolated operation generation is no longer active"
        );
        let codex = scope
            .codex
            .clone()
            .context("isolated Codex is not connected")?;
        Ok((codex, scope.handle.id.clone()))
    }

    pub async fn complete(&self, context: &ExecutionContext) -> Result<()> {
        let key = self.key(context);
        let (name, id, expired) = {
            let mut scopes = self.scopes.lock().await;
            let Some(scope) = scopes.get_mut(&key) else {
                return Ok(());
            };
            if !scope.operations.contains_key(&context.operation_id) {
                return Ok(());
            }
            let expired = scope
                .operations
                .values()
                .any(|deadline| *deadline <= vibeface_protocol::now_ms());
            if scope.operations.len() > 1 && !scope.stopped && !expired {
                scope.operations.remove(&context.operation_id);
                return write_deadline(scope);
            }
            scope.stopped = true;
            (scope.handle.name.clone(), scope.handle.id.clone(), expired)
        };
        self.cancel(context).await?;
        let mut command = self.docker();
        command.args(["rm", &id]);
        checked_output(command, "remove completed isolated container").await?;
        if let Some(egress) = &self.config.egress {
            let mut command = self.docker();
            command.args(["network", "disconnect", &name, &egress.gateway_container]);
            checked_output(command, "disconnect completed model network").await?;
            let mut command = self.docker();
            command.args(["network", "rm", &name]);
            checked_output(command, "remove completed model network").await?;
        }
        self.scopes.lock().await.remove(&key);
        ensure!(
            !expired,
            "execution lease expired before cleanup; preserve affected operations as unknown"
        );
        Ok(())
    }

    pub async fn renew(&self, context: &ExecutionContext, expires_at_ms: u64) -> Result<()> {
        validate_lease(expires_at_ms)?;
        let mut scopes = self.scopes.lock().await;
        let scope = scopes
            .get_mut(&self.key(context))
            .context("isolated operation is not running")?;
        ensure!(!scope.stopped, "isolated resource was stopped");
        ensure!(
            scope
                .operations
                .values()
                .all(|deadline| *deadline > vibeface_protocol::now_ms()),
            "execution lease already expired; do not revive a resource"
        );
        let deadline = scope
            .operations
            .get_mut(&context.operation_id)
            .context("isolated operation has no active lease")?;
        ensure!(
            *deadline > vibeface_protocol::now_ms(),
            "execution lease already expired; do not revive an operation"
        );
        *deadline = expires_at_ms;
        write_deadline(scope)
    }

    pub async fn affected(&self, context: &ExecutionContext) -> Vec<String> {
        let mut affected = BTreeSet::from([context.operation_id.clone()]);
        if let Some(scope) = self.scopes.lock().await.get(&self.key(context)) {
            if scope.operations.contains_key(&context.operation_id) {
                affected.extend(scope.operations.keys().cloned());
            }
        }
        affected.into_iter().collect()
    }

    pub async fn cancel(&self, context: &ExecutionContext) -> Result<Vec<String>> {
        let mut scopes = self.scopes.lock().await;
        Ok(self.stop(context, &mut scopes).await?.1)
    }

    pub(super) async fn fence(
        &self,
        context: &ExecutionContext,
    ) -> Result<(StoppedScope, Vec<String>)> {
        let mut scopes = self.scopes.lock().await;
        let (mut stopped, affected) = self.stop(context, &mut scopes).await?;
        if self.config.egress.is_some() {
            stopped.network_id = self
                .inspect_network(&stopped.name)
                .await?
                .map(|network| {
                    ensure!(
                        network["Labels"]["vibeface.node"] == stopped.node
                            && network["Labels"]["vibeface.scope"] == stopped.key,
                        "refuse to fence an unrelated model network"
                    );
                    Ok(network["Id"]
                        .as_str()
                        .context("isolated model network ID missing")?
                        .to_owned())
                })
                .transpose()?;
        }
        Ok((stopped, affected))
    }

    async fn stop(
        &self,
        context: &ExecutionContext,
        scopes: &mut BTreeMap<String, IsolatedScope>,
    ) -> Result<(StoppedScope, Vec<String>)> {
        let key = self.key(context);
        let node = hash(
            self.data_dir
                .canonicalize()?
                .to_str()
                .context("Node data directory must be UTF-8")?,
        );
        let name = format!("vibeface-{}-{}", &node[..16], &key[..32]);
        let mut affected = BTreeSet::from([context.operation_id.clone()]);
        let id = if let Some(scope) = scopes.get_mut(&key) {
            ensure!(
                scope.operations.contains_key(&context.operation_id),
                "stale operation cannot cancel a newer execution generation"
            );
            scope.stopped = true;
            affected.extend(scope.operations.keys().cloned());
            scope.codex = None;
            let _ = crate::config::private_write(
                &scope.handle.control.join("deadline"),
                vibeface_protocol::now_ms().to_string().as_bytes(),
            );
            Some(scope.handle.id.clone())
        } else {
            if let Some(container) = self.inspect(&name).await? {
                let control = self
                    .data_dir
                    .canonicalize()?
                    .join("execution")
                    .join(&key)
                    .join("control");
                crate::config::verify_private_dir(&control)?;
                let operations: BTreeMap<String, u64> =
                    serde_json::from_slice(&std::fs::read(control.join("operations.json"))?)?;
                ensure!(
                    operations.contains_key(&context.operation_id),
                    "stale operation cannot cancel a different persisted generation"
                );
                affected.extend(operations.into_keys());
                Some(
                    container["Id"]
                        .as_str()
                        .context("isolated container ID missing")?
                        .to_owned(),
                )
            } else {
                None
            }
        };
        if let Some(id) = &id {
            if let Some(container) = self.inspect(id).await? {
                ensure!(
                    container["Config"]["Labels"]["vibeface.node"] == node
                        && container["Config"]["Labels"]["vibeface.scope"] == key,
                    "refuse to stop an unrelated container"
                );
                if container["State"]["Running"] == true {
                    let mut command = self.docker();
                    command.args(["stop", "--time", "2", id]);
                    checked_output(command, "stop isolated resource container").await?;
                }
                let stopped = self
                    .inspect(id)
                    .await?
                    .context("container disappeared before stop could be confirmed")?;
                ensure!(
                    stopped["State"]["Running"] == false && stopped["State"]["Pid"] == 0,
                    "isolated execution stop could not be confirmed"
                );
            }
        }
        Ok((
            StoppedScope {
                node,
                key,
                name,
                id,
                network_id: None,
            },
            affected.into_iter().collect(),
        ))
    }

    pub(super) async fn complete_fenced(&self, stopped: &StoppedScope) -> Result<()> {
        let mut scopes = self.scopes.lock().await;
        if let Some(scope) = scopes.get(&stopped.key) {
            ensure!(
                scope.stopped && stopped.id.as_deref() == Some(scope.handle.id.as_str()),
                "stopped execution cannot clean a different scope generation"
            );
        }
        if let Some(container) = self.inspect(&stopped.name).await? {
            ensure!(
                container["Id"].as_str() == stopped.id.as_deref(),
                "stopped execution container name belongs to a different generation"
            );
        }
        if let Some(id) = &stopped.id {
            if let Some(container) = self.inspect(id).await? {
                ensure!(
                    container["Config"]["Labels"]["vibeface.node"] == stopped.node
                        && container["Config"]["Labels"]["vibeface.scope"] == stopped.key
                        && container["State"]["Running"] == false
                        && container["State"]["Pid"] == 0,
                    "fenced execution is no longer confirmed stopped"
                );
                let mut command = self.docker();
                command.args(["rm", id]);
                checked_output(command, "remove fenced isolated container").await?;
                ensure!(
                    self.inspect(id).await?.is_none(),
                    "fenced container still exists after cleanup"
                );
            }
        }
        if let Some(egress) = &self.config.egress {
            if let Some(network) = self.inspect_network(&stopped.name).await? {
                ensure!(
                    network["Id"].as_str() == stopped.network_id.as_deref(),
                    "stopped execution cannot clean a different model network generation"
                );
            }
            if let Some(id) = &stopped.network_id {
                if let Some(network) = self.inspect_network(id).await? {
                    ensure!(
                        network["Labels"]["vibeface.node"] == stopped.node
                            && network["Labels"]["vibeface.scope"] == stopped.key,
                        "refuse to clean an unrelated model network"
                    );
                    let gateway = self.inspect(&egress.gateway_container).await?;
                    let gateway_id = gateway
                        .as_ref()
                        .map(|gateway| gateway["Id"].as_str().context("model gateway ID missing"))
                        .transpose()?;
                    let peers = network["Containers"]
                        .as_object()
                        .context("model network peers unavailable")?;
                    ensure!(
                        peers.keys().all(|peer| Some(peer.as_str()) == gateway_id),
                        "fenced model network has another execution endpoint"
                    );
                    if let Some(gateway_id) = gateway_id {
                        if peers.contains_key(gateway_id) {
                            let mut command = self.docker();
                            command.args(["network", "disconnect", id, &egress.gateway_container]);
                            checked_output(command, "disconnect fenced model network").await?;
                        }
                    }
                    let mut command = self.docker();
                    command.args(["network", "rm", id]);
                    checked_output(command, "remove fenced model network").await?;
                    ensure!(
                        self.inspect_network(id).await?.is_none(),
                        "fenced model network still exists after cleanup"
                    );
                }
            }
        }
        scopes.remove(&stopped.key);
        Ok(())
    }

    fn docker(&self) -> Command {
        Command::new(&self.config.docker_bin)
    }

    async fn inspect(&self, name: &str) -> Result<Option<Value>> {
        let mut command = self.docker();
        command.args(["container", "inspect", name]);
        let output = timed_output(command)
            .await
            .context("inspect isolated resource container")?;
        if output.status.success() {
            let values: Vec<Value> = serde_json::from_slice(&output.stdout)?;
            ensure!(values.len() == 1, "invalid container inspect response");
            return Ok(values.into_iter().next());
        }
        let error = String::from_utf8_lossy(&output.stderr).to_ascii_lowercase();
        ensure!(
            error.contains("no such container") || error.contains("no such object"),
            "Docker could not inspect isolated execution"
        );
        Ok(None)
    }

    async fn inspect_network(&self, name: &str) -> Result<Option<Value>> {
        let mut command = self.docker();
        command.args(["network", "inspect", name]);
        let output = timed_output(command)
            .await
            .context("inspect isolated model network")?;
        if output.status.success() {
            let values: Vec<Value> = serde_json::from_slice(&output.stdout)?;
            ensure!(values.len() == 1, "invalid network inspect response");
            return Ok(values.into_iter().next());
        }
        let error = String::from_utf8_lossy(&output.stderr).to_ascii_lowercase();
        ensure!(
            error.contains("no such network")
                || (error.contains("network ") && error.contains(" not found")),
            "Docker could not inspect isolated model network"
        );
        Ok(None)
    }

    async fn verify_container(
        &self,
        name: &str,
        node: &str,
        key: &str,
        project: &Path,
        uid: u32,
    ) -> Result<()> {
        let container = self
            .inspect(name)
            .await?
            .context("isolated container is missing")?;
        ensure!(
            container["State"]["Running"] == true,
            "isolated container is stopped; do not restart unknown operations"
        );
        ensure!(
            container["Config"]["Labels"]["vibeface.node"] == node
                && container["Config"]["Labels"]["vibeface.scope"] == key
                && container["Config"]["Labels"]["vibeface.project-read-only"]
                    == self.project_read_only.to_string()
                && container["Config"]["Labels"]["vibeface.project"]
                    == hash(project.to_str().context("project path must be UTF-8")?),
            "isolated container belongs to a different Node, space or project"
        );
        let host = &container["HostConfig"];
        let (network, _) = self.egress_network(node, key, false).await?;
        ensure!(
            container["Config"]["Image"] == self.config.image
                && container["Config"]["User"] == format!("{uid}:{}", effective_gid())
                && host["ReadonlyRootfs"] == true
                && host["Privileged"] == false
                && host["NetworkMode"] == network
                && host["CapDrop"]
                    .as_array()
                    .is_some_and(|caps| caps.iter().any(|cap| cap == "ALL"))
                && host["CapAdd"].as_array().is_none_or(Vec::is_empty)
                && host["SecurityOpt"]
                    .as_array()
                    .is_some_and(|options| options.len() == 1
                        && options.iter().any(|option| option == "no-new-privileges"
                            || option == "no-new-privileges:true")),
            "isolated container security configuration changed"
        );
        ensure!(
            container["Config"]["Entrypoint"] == serde_json::json!(["/usr/bin/env"])
                && container["Config"]["Cmd"]
                    == serde_json::json!([
                        "-i",
                        "PATH=/usr/local/bin:/usr/bin:/bin",
                        "HOME=/home/vibeface",
                        "/usr/bin/python3",
                        "-I",
                        "-S",
                        "-c",
                        WATCHDOG
                    ])
                && host["RestartPolicy"]["Name"] == "no",
            "isolated container watchdog or restart policy changed"
        );
        let mounts = container["Mounts"]
            .as_array()
            .context("container mounts unavailable")?;
        let home = self
            .data_dir
            .canonicalize()?
            .join("execution")
            .join(key)
            .join("home");
        ensure!(
            mounts.len() == 3
                && mounts.iter().all(|mount| mount["Type"] == "bind"
                    && ((mount["Destination"] == "/project"
                        && mount["Source"] == project.to_string_lossy().as_ref()
                        && mount["RW"] == !self.project_read_only)
                        || (mount["Destination"] == CONTAINER_HOME
                            && mount["Source"] == home.to_string_lossy().as_ref())
                        || (mount["Destination"] == "/run/vibeface-control"
                            && mount["Source"]
                                == home
                                    .parent()
                                    .unwrap()
                                    .join("control")
                                    .to_string_lossy()
                                    .as_ref()
                            && mount["RW"] == false))),
            "isolated container has unexpected mounts"
        );
        let networks = container["NetworkSettings"]["Networks"]
            .as_object()
            .context("container network connections unavailable")?;
        ensure!(
            networks.len() == 1 && networks.contains_key(&network),
            "isolated container has an unexpected network connection"
        );
        ensure!(
            host["PidMode"].as_str() == Some("")
                && host["IpcMode"] == "private"
                && host["Devices"].as_array().is_none_or(Vec::is_empty)
                && host["DeviceRequests"].as_array().is_none_or(Vec::is_empty),
            "isolated container shares host process or device access"
        );
        Ok(())
    }

    async fn egress_network(
        &self,
        node: &str,
        key: &str,
        create: bool,
    ) -> Result<(String, Option<String>)> {
        let Some(egress) = &self.config.egress else {
            return Ok(("none".into(), None));
        };
        let name = format!("vibeface-{}-{}", &node[..16], &key[..32]);
        let mut command = self.docker();
        command.args(["network", "inspect", &name]);
        let output = timed_output(command).await?;
        if !output.status.success() {
            ensure!(
                create
                    && String::from_utf8_lossy(&output.stderr)
                        .to_ascii_lowercase()
                        .contains("not found"),
                "isolated model network is unavailable"
            );
            let mut command = self.docker();
            command.args([
                "network",
                "create",
                "--internal",
                "--driver",
                "bridge",
                "--opt",
                "com.docker.network.bridge.gateway_mode_ipv4=isolated",
                "--label",
                &format!("vibeface.node={node}"),
                "--label",
                &format!("vibeface.scope={key}"),
                &name,
            ]);
            checked_output(command, "create isolated model network").await?;
        }
        let gateway = self
            .inspect(&egress.gateway_container)
            .await?
            .context("model gateway is missing")?;
        ensure!(
            gateway["State"]["Running"] == true
                && gateway["HostConfig"]["Privileged"] == false
                && gateway["HostConfig"]["CapDrop"]
                    .as_array()
                    .is_some_and(|caps| caps.iter().any(|cap| cap == "ALL"))
                && gateway["HostConfig"]["CapAdd"]
                    .as_array()
                    .is_none_or(Vec::is_empty)
                && gateway["HostConfig"]["PidMode"].as_str() == Some("")
                && gateway["HostConfig"]["NetworkMode"] != "host",
            "model gateway must be running without host namespaces or Linux capabilities"
        );
        if gateway["NetworkSettings"]["Networks"][&name].is_null() {
            ensure!(create, "model gateway is disconnected from this scope");
            let mut command = self.docker();
            command.args(["network", "connect", &name, &egress.gateway_container]);
            checked_output(command, "connect the trusted model gateway").await?;
        }
        let mut command = self.docker();
        command.args(["network", "inspect", &name]);
        let networks: Vec<Value> = serde_json::from_slice(
            &checked_output(command, "inspect isolated model network").await?,
        )?;
        let network = networks.first().context("model network is missing")?;
        ensure!(
            network["Internal"] == true
                && network["Driver"] == "bridge"
                && network["EnableIPv6"] == false
                && network["Options"]["com.docker.network.bridge.gateway_mode_ipv4"] == "isolated"
                && network["Labels"]["vibeface.node"] == node
                && network["Labels"]["vibeface.scope"] == key,
            "model egress network must belong to this isolated scope"
        );
        let gateway = self
            .inspect(&egress.gateway_container)
            .await?
            .context("model gateway disappeared")?;
        let address = gateway["NetworkSettings"]["Networks"][&name]["IPAddress"]
            .as_str()
            .context("model gateway has no scope address")?;
        ensure!(
            address.parse::<std::net::Ipv4Addr>().is_ok(),
            "invalid model gateway address"
        );
        for (id, peer) in network["Containers"]
            .as_object()
            .context("model network peers unavailable")?
        {
            if gateway["Id"] == *id {
                continue;
            }
            let peer_name = peer["Name"]
                .as_str()
                .context("model network peer name unavailable")?;
            let peer = self
                .inspect(peer_name)
                .await?
                .context("model network peer disappeared")?;
            ensure!(
                peer["Config"]["Labels"]["vibeface.node"] == node
                    && peer["Config"]["Labels"]["vibeface.scope"] == key
                    && peer_name == name,
                "model network contains a different execution scope"
            );
        }
        Ok((
            name,
            Some(format!("http://{address}:{}/", egress.gateway_port)),
        ))
    }
}

impl IsolatedProcess {
    pub fn container_id(&self) -> &str {
        &self.id
    }

    pub fn command(&self, executable: &str) -> Command {
        let mut command = Command::new(&self.config.docker_bin);
        command.args([
            "exec",
            "-i",
            "--workdir",
            "/project",
            &self.id,
            "/usr/bin/env",
            "-i",
            "PATH=/usr/local/bin:/usr/bin:/bin",
            "HOME=/home/vibeface",
            "CODEX_HOME=/home/vibeface/.codex",
            "CLAUDE_CONFIG_DIR=/home/vibeface/.claude",
            "XDG_CONFIG_HOME=/home/vibeface/.config",
            "XDG_CACHE_HOME=/home/vibeface/.cache",
            "XDG_STATE_HOME=/home/vibeface/.local/state",
            "TMPDIR=/tmp",
        ]);
        if let Some(base_url) = &self.gateway_url {
            command.arg(format!("OPENAI_BASE_URL={base_url}v1"));
            command.arg("OPENAI_API_KEY=vibeface-public-gateway-dummy");
            command.arg(format!("ANTHROPIC_BASE_URL={base_url}anthropic"));
            command.arg("ANTHROPIC_API_KEY=vibeface-public-gateway-dummy");
        }
        command.arg(executable);
        command
    }
}

fn hash(value: &str) -> String {
    hex::encode(Sha256::digest(value.as_bytes()))
}

fn validate_lease(expires_at_ms: u64) -> Result<()> {
    let now = vibeface_protocol::now_ms();
    ensure!(
        expires_at_ms > now
            && expires_at_ms
                <= now.saturating_add(vibeface_secure::authorization::EXECUTION_LEASE_MS),
        "execution lease must be current and bounded"
    );
    Ok(())
}

fn write_deadline(scope: &IsolatedScope) -> Result<()> {
    let deadline = scope
        .operations
        .values()
        .min()
        .copied()
        .context("isolated resource has no active operation")?;
    crate::config::private_write(
        &scope.handle.control.join("operations.json"),
        &serde_json::to_vec(&scope.operations)?,
    )?;
    crate::config::private_write(
        &scope.handle.control.join("deadline"),
        deadline.to_string().as_bytes(),
    )
}

fn bind_mount(source: &Path, target: &str) -> Result<String> {
    let source = source.to_str().context("container mount must be UTF-8")?;
    ensure!(
        !source.contains([',', '\n', '\r']),
        "container mount contains an unsupported character"
    );
    Ok(format!("type=bind,src={source},dst={target}"))
}

fn private_child(parent: &Path, name: &str) -> Result<PathBuf> {
    use std::os::unix::fs::DirBuilderExt;
    let path = parent.join(name);
    match std::fs::DirBuilder::new().mode(0o700).create(&path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error.into()),
    }
    crate::config::verify_private_dir(&path)?;
    Ok(path)
}

fn effective_gid() -> u32 {
    unsafe extern "C" {
        fn getegid() -> u32;
    }
    unsafe { getegid() }
}

async fn timed_output(mut command: Command) -> Result<std::process::Output> {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    tokio::time::timeout(COMMAND_TIMEOUT, command.output())
        .await
        .context("Docker command timed out; execution state is unknown")?
        .context("start Docker command")
}

async fn checked_output(command: Command, action: &str) -> Result<Vec<u8>> {
    let output = timed_output(command).await?;
    if !output.status.success() {
        bail!("{action} failed; inspect the isolated resource before retrying");
    }
    ensure!(
        output.stdout.len() <= 1_048_576,
        "Docker response exceeded its limit"
    );
    Ok(output.stdout)
}

#[cfg(test)]
mod tests {
    use super::super::{Engine, EngineAction, ExecutorConfig, ExecutorKind, ExecutorSession};

    async fn cleanup_test_container(docker: &Path, name: &str) -> Result<()> {
        let mut command = Command::new(docker);
        command.args(["rm", "--force", name]);
        let output = timed_output(command).await?;
        ensure!(
            output.status.success()
                || String::from_utf8_lossy(&output.stderr)
                    .to_ascii_lowercase()
                    .contains("no such container"),
            "test container cleanup failed"
        );
        Ok(())
    }
    use super::*;

    #[test]
    fn isolation_configuration_rejects_mutable_images_and_host_networks() {
        let mut config = ExecutionIsolationConfig {
            image: "node:latest".into(),
            docker_bin: "/bin/sh".into(),
            egress: None,
        };
        assert!(config.validate().is_err());
        config.image = format!("sha256:{}", "a".repeat(64));
        assert!(config.validate().is_ok());
        config.egress = Some(ExecutionEgressConfig {
            gateway_container: "models".into(),
            gateway_port: 0,
            codex_model: "test-model".into(),
        });
        assert!(config.validate().is_err());
        config.egress.as_mut().unwrap().gateway_port = 8080;
        assert!(config.validate().is_ok());
        assert!(serde_json::from_value::<ExecutionEgressConfig>(serde_json::json!({"gateway_container":"models","gateway_port":8080,"codex_model":"test-model","network":"host"})).is_err());
    }

    #[test]
    fn operation_payload_cannot_choose_a_mount_or_container() {
        let context = ExecutionContext {
            space_id: SpaceId::parse(&format!("spc2_{}", "1".repeat(64))).unwrap(),
            resource_id: ResourceId::parse(&format!("res2_{}", "2".repeat(64))).unwrap(),
            operation_id: "operation-a".into(),
            shared: true,
            lease_expires_at_ms: vibeface_protocol::now_ms() + 30_000,
        };
        let key = DockerIsolation::scope_key(&context);
        let mut other = context.clone();
        other.operation_id = "operation-b".into();
        assert_eq!(DockerIsolation::scope_key(&other), key);
        other.resource_id = ResourceId::parse(&format!("res2_{}", "3".repeat(64))).unwrap();
        assert_ne!(DockerIsolation::scope_key(&other), key);
        assert!(bind_mount(Path::new("/project,src=/"), "/project").is_err());
    }

    #[tokio::test]
    async fn confirmed_fencing_cleans_only_its_stopped_container_and_network_generation()
    -> Result<()> {
        use std::os::unix::fs::PermissionsExt;
        let root = std::env::temp_dir().join(format!("vfc-{}", uuid::Uuid::new_v4()));
        let data = root.join("node");
        let control = root.join("control");
        crate::config::private_dir(&data)?;
        crate::config::private_dir(&control)?;
        let docker = root.join("docker");
        std::fs::write(
            &docker,
            format!(
                r##"#!/bin/sh
fixture='{}'
case "$1 $2" in
  'container inspect')
    if [ "$3" = models ]; then cat "$fixture/gateway.json"; exit; fi
    if [ -f "$fixture/container.json" ]; then cat "$fixture/container.json"; exit; fi
    printf 'No such container\n' >&2; exit 1;;
  'rm old-container')
    if [ -f "$fixture/fail-container" ]; then exit 1; fi
    if [ ! -f "$fixture/keep-container" ]; then rm "$fixture/container.json"; fi;;
  'network inspect')
    if [ -f "$fixture/network.json" ]; then cat "$fixture/network.json"; exit; fi
    printf 'No such network\n' >&2; exit 1;;
  'network disconnect')
    [ "$3" = old-network ] && [ "$4" = models ] || exit 2
    cp "$fixture/detached-network.json" "$fixture/network.json";;
  'network rm')
    [ "$3" = old-network ] || exit 2
    if [ -f "$fixture/fail-network" ]; then exit 1; fi
    rm "$fixture/network.json";;
  *) exit 2;;
esac
"##,
                root.display()
            ),
        )?;
        std::fs::set_permissions(&docker, std::fs::Permissions::from_mode(0o700))?;
        let settings = ExecutionIsolationConfig {
            image: format!("sha256:{}", "a".repeat(64)),
            docker_bin: docker,
            egress: Some(ExecutionEgressConfig {
                gateway_container: "models".into(),
                gateway_port: 8080,
                codex_model: "fixture".into(),
            }),
        };
        let (events, _) = mpsc::unbounded_channel();
        let engine = Engine::new(
            ExecutorConfig {
                execution_isolation: Some(settings.clone()),
                data_dir: Some(data.clone()),
                ..ExecutorConfig::default()
            },
            events,
        );
        let context = ExecutionContext {
            space_id: SpaceId::parse(&format!("spc2_{}", "1".repeat(64)))?,
            resource_id: ResourceId::from_bytes([2; 32]),
            operation_id: "expired-operation".into(),
            shared: true,
            lease_expires_at_ms: 0,
        };
        let isolation = engine.isolation.as_ref().unwrap();
        let node = hash(data.canonicalize()?.to_str().unwrap());
        let key = isolation.key(&context);
        let name = format!("vibeface-{}-{}", &node[..16], &key[..32]);
        let labels = serde_json::json!({"vibeface.node":node,"vibeface.scope":key});
        let container = serde_json::json!([{"Id":"old-container", "Config":{"Labels":labels}, "State":{"Running":false,"Pid":0}}]);
        let network = serde_json::json!({"Id":"old-network","Labels":labels,"Containers":{"gateway-id":{"Name":"models"}}});
        std::fs::write(root.join("container.json"), serde_json::to_vec(&container)?)?;
        std::fs::write(
            root.join("network.json"),
            serde_json::to_vec(&serde_json::json!([network]))?,
        )?;
        std::fs::write(
            root.join("detached-network.json"),
            serde_json::to_vec(
                &serde_json::json!([{"Id":"old-network","Labels":labels,"Containers":{}}]),
            )?,
        )?;
        std::fs::write(root.join("gateway.json"), br#"[{"Id":"gateway-id"}]"#)?;
        let handle = IsolatedProcess {
            config: settings,
            name,
            id: "old-container".into(),
            control,
            gateway_url: None,
        };
        isolation.scopes.lock().await.insert(
            key.clone(),
            IsolatedScope {
                handle: handle.clone(),
                operations: BTreeMap::from([
                    (context.operation_id.clone(), 0),
                    ("also-stopped".into(), 0),
                ]),
                codex: None,
                stopped: false,
            },
        );
        let (stopped, affected) = engine.fence_operation(&context).await?;
        assert_eq!(affected, vec!["also-stopped", "expired-operation"]);
        std::fs::write(root.join("fail-container"), b"")?;
        assert!(engine.complete_fenced_operation(&stopped).await.is_err());
        assert!(root.join("container.json").exists());
        std::fs::remove_file(root.join("fail-container"))?;
        std::fs::write(root.join("keep-container"), b"")?;
        assert!(engine.complete_fenced_operation(&stopped).await.is_err());
        assert!(isolation.scopes.lock().await.contains_key(&key));
        std::fs::remove_file(root.join("keep-container"))?;
        std::fs::write(root.join("fail-network"), b"")?;
        assert!(engine.complete_fenced_operation(&stopped).await.is_err());
        assert!(!root.join("container.json").exists());
        assert!(root.join("network.json").exists());
        assert!(isolation.scopes.lock().await.contains_key(&key));
        std::fs::remove_file(root.join("fail-network"))?;
        engine.complete_fenced_operation(&stopped).await?;
        engine.complete_fenced_operation(&stopped).await?;
        assert!(!root.join("network.json").exists());
        assert!(!isolation.scopes.lock().await.contains_key(&key));
        assert!(engine.fenced_operations.lock().await.contains(&(
            context.space_id.clone(),
            context.resource_id.clone(),
            "also-stopped".into()
        )));

        let mut new_handle = handle;
        new_handle.id = "new-container".into();
        isolation.scopes.lock().await.insert(
            key.clone(),
            IsolatedScope {
                handle: new_handle,
                operations: BTreeMap::from([("new-operation".into(), 0)]),
                codex: None,
                stopped: false,
            },
        );
        assert!(engine.complete_fenced_operation(&stopped).await.is_err());
        assert_eq!(
            isolation.scopes.lock().await[&key].handle.id,
            "new-container"
        );
        isolation.scopes.lock().await.remove(&key);
        let mut new_container = container;
        new_container[0]["Id"] = "new-container".into();
        std::fs::write(
            root.join("container.json"),
            serde_json::to_vec(&new_container)?,
        )?;
        assert!(engine.complete_fenced_operation(&stopped).await.is_err());
        assert!(root.join("container.json").exists());
        std::fs::remove_file(root.join("container.json"))?;
        std::fs::write(root.join("network.json"), br#"[{"Id":"new-network"}]"#)?;
        assert!(engine.complete_fenced_operation(&stopped).await.is_err());
        assert!(root.join("network.json").exists());
        std::fs::remove_dir_all(root)?;
        Ok(())
    }

    #[tokio::test]
    #[ignore = "requires the pinned isolation fixture image and a local Docker daemon"]
    async fn production_engine_isolates_files_environment_and_cancels_the_container() -> Result<()>
    {
        let root = PathBuf::from(std::env::var("VIBEFACE_ISOLATION_TEST_ROOT")?)
            .join(uuid::Uuid::new_v4().to_string());
        let data = root.join("node");
        let project = root.join("project");
        let other = root.join("other-project");
        crate::config::private_dir(&data)?;
        std::fs::create_dir(&project)?;
        std::fs::create_dir(&other)?;
        std::fs::write(project.join("visible"), "visible")?;
        std::fs::write(other.join("private"), "other project")?;
        let secret = root.join("host-private");
        std::fs::write(&secret, "host-only fixture")?;
        std::os::unix::fs::symlink(&secret, project.join("escape"))?;
        let docker_bin = PathBuf::from(std::env::var("VIBEFACE_ISOLATION_TEST_DOCKER")?);
        let config = ExecutionIsolationConfig {
            image: std::env::var("VIBEFACE_ISOLATION_TEST_IMAGE")?,
            docker_bin: docker_bin.clone(),
            egress: None,
        };
        let (events, _received) = mpsc::unbounded_channel();
        let engine = Engine::new(
            ExecutorConfig {
                execution_isolation: Some(config),
                data_dir: Some(data.clone()),
                timeout_secs: 30,
                ..ExecutorConfig::default()
            },
            events,
        );
        let context = ExecutionContext {
            space_id: SpaceId::parse(&format!("spc2_{}", "4".repeat(64)))?,
            resource_id: ResourceId::parse(&format!("res2_{}", "5".repeat(64)))?,
            operation_id: "inspect-operation".into(),
            shared: true,
            lease_expires_at_ms: vibeface_protocol::now_ms() + 30_000,
        };
        let session = ExecutorSession {
            executor: ExecutorKind::ClaudeCode,
            cwd: project,
            native_session_id: None,
        };
        let node = hash(data.canonicalize()?.to_str().unwrap());
        let key = DockerIsolation::scope_key(&context);
        let name = format!("vibeface-{}-{}", &node[..16], &key[..32]);
        let outcome = async {
            let stale = engine.isolation.as_ref().unwrap().prepare(&context, &session.cwd).await?;
            let result = engine.execute_authorized(&context, &session, EngineAction::Send { text: serde_json::json!({"secret":secret,"other_project":other.join("private"),"background":true}).to_string() }, |_| async { Ok(()) }).await?;
            let proof = result.result["result"].as_object().context("isolation proof missing")?;
            ensure!(proof.len() == 9 && proof.values().all(|value| value == true), "production Engine isolation proof failed: {proof:?}");
            let mut waiting = context.clone();
            waiting.operation_id = "waiting-operation".into();
            let running_engine = engine.clone();
            let running_context = waiting.clone();
            let running_session = session.clone();
            let task = tokio::spawn(async move { running_engine.execute_authorized(&running_context, &running_session, EngineAction::Send { text: "{\"wait\":true}".into() }, |_| async { Ok(()) }).await });
            tokio::time::timeout(Duration::from_secs(10), async {
                while !session.cwd.join("started").exists() { tokio::time::sleep(Duration::from_millis(20)).await; }
            }).await?;
            let mut old_command = stale.command("node");
            old_command.args(["-e", "require('node:fs').writeFileSync('/project/stale-write','unsafe')"]);
            ensure!(checked_output(old_command, "attempt stale container generation").await.is_err(), "stale exec reached a newer container");
            ensure!(engine.cancel_operation(&context).await.is_err(), "stale cancel stopped a newer operation");
            ensure!(!session.cwd.join("stale-write").exists(), "stale command wrote into a new operation");
            let mut concurrent = waiting.clone();
            concurrent.operation_id = "same-resource-operation".into();
            engine.isolation.as_ref().unwrap().prepare(&concurrent, &session.cwd).await?;
            let affected = engine.cancel_operation(&waiting).await?;
            ensure!(affected == vec!["same-resource-operation", "waiting-operation"], "cancel did not report every active operation");
            let failure = task.await?.expect_err("stopped provider must not report success");
            ensure!(failure.uncertain, "interrupted execution must remain unknown");
            let rejected = engine.execute_authorized(&waiting, &session, EngineAction::Send { text: "{\"wait\":true}".into() }, |_| async { Ok(()) }).await.expect_err("a stopped resource must not restart an operation");
            ensure!(!rejected.uncertain, "rejected restart should not invoke a Provider");
            let mut inspect = Command::new(&docker_bin);
            inspect.args(["inspect", &name]);
            let containers: Vec<Value> = serde_json::from_slice(&checked_output(inspect, "verify stopped test container").await?)?;
            ensure!(containers[0]["State"]["Running"] == false && containers[0]["State"]["Pid"] == 0, "cancel did not stop the actual container");
            tokio::time::sleep(Duration::from_millis(3_200)).await;
            ensure!(!session.cwd.join("background-write").exists(), "completed Provider left a background process running");
            let (stopped, _) = engine.fence_operation(&waiting).await?;
            engine.complete_fenced_operation(&stopped).await?;
            let mut replacement = waiting.clone();
            replacement.operation_id = "replacement-operation".into();
            replacement.lease_expires_at_ms = vibeface_protocol::now_ms() + 30_000;
            let prepared = engine.isolation.as_ref().unwrap().prepare(&replacement, &session.cwd).await?;
            ensure!(prepared.id != stale.id, "replacement reused the stopped container generation");
            ensure!(engine.complete_fenced_operation(&stopped).await.is_err(), "old stopped token cleaned the replacement generation");
            let (replaced, _) = engine.fence_operation(&replacement).await?;
            engine.complete_fenced_operation(&replaced).await?;
            println!("isolation-proof={}", serde_json::json!({"file_and_environment":proof,"cancel_affected":affected,"cancel_confirmed":true,"interrupted_result_unknown":failure.uncertain,"stopped_operation_not_restarted":true,"stale_exec_rejected":true,"stale_cancel_rejected":true,"terminal_background_write_absent":true,"fenced_cleanup_allows_new_generation":true}));
            Ok::<_, anyhow::Error>(())
        }.await;
        let cleaned = cleanup_test_container(&docker_bin, &name).await;
        std::fs::remove_dir_all(&root)?;
        outcome?;
        cleaned?;
        Ok(())
    }

    #[tokio::test]
    #[ignore = "requires the pinned Linux Codex isolation image and a local Docker daemon"]
    async fn production_engine_opens_a_real_isolated_codex_session_without_a_model_turn()
    -> Result<()> {
        let root = PathBuf::from(std::env::var("VIBEFACE_ISOLATION_TEST_ROOT")?)
            .join(uuid::Uuid::new_v4().to_string());
        let data = root.join("node");
        let project = root.join("project");
        crate::config::private_dir(&data)?;
        std::fs::create_dir(&project)?;
        let docker_bin = PathBuf::from(std::env::var("VIBEFACE_ISOLATION_TEST_DOCKER")?);
        let config = ExecutionIsolationConfig {
            image: std::env::var("VIBEFACE_ISOLATION_TEST_IMAGE")?,
            docker_bin: docker_bin.clone(),
            egress: None,
        };
        let (events, _received) = mpsc::unbounded_channel();
        let engine = Engine::new(
            ExecutorConfig {
                execution_isolation: Some(config),
                data_dir: Some(data.clone()),
                timeout_secs: 30,
                ..ExecutorConfig::default()
            },
            events,
        );
        let context = ExecutionContext {
            space_id: SpaceId::parse(&format!("spc2_{}", "6".repeat(64)))?,
            resource_id: ResourceId::parse(&format!("res2_{}", "7".repeat(64)))?,
            operation_id: "codex-open-operation".into(),
            shared: true,
            lease_expires_at_ms: vibeface_protocol::now_ms() + 30_000,
        };
        let session = ExecutorSession {
            executor: ExecutorKind::Codex,
            cwd: project,
            native_session_id: None,
        };
        let node = hash(data.canonicalize()?.to_str().unwrap());
        let key = DockerIsolation::scope_key(&context);
        let name = format!("vibeface-{}-{}", &node[..16], &key[..32]);
        let outcome = async {
            let result = engine.execute_authorized(&context, &session, EngineAction::Open {}, |_| async { Ok(()) }).await?;
            ensure!(result.native_session_id.is_some(), "real Codex returned no native thread");
            ensure!(engine.isolation.as_ref().unwrap().inspect(&name).await?.is_none(), "completed Codex container was not removed");
            println!("isolated-codex-proof={}", serde_json::json!({"native_thread_created":true,"stdio_connected":true,"container_stop_confirmed":true,"model_turns":0}));
            Ok::<_, anyhow::Error>(())
        }.await;
        let cleaned = cleanup_test_container(&docker_bin, &name).await;
        std::fs::remove_dir_all(&root)?;
        outcome?;
        cleaned?;
        Ok(())
    }

    #[tokio::test]
    #[ignore = "requires the isolation attack fixture image and a local Docker daemon"]
    async fn production_watchdog_expires_after_controller_loss_despite_same_uid_attacks()
    -> Result<()> {
        let root = PathBuf::from(std::env::var("VIBEFACE_ISOLATION_TEST_ROOT")?)
            .join(uuid::Uuid::new_v4().to_string());
        let data = root.join("node");
        let project = root.join("project");
        crate::config::private_dir(&data)?;
        std::fs::create_dir(&project)?;
        std::fs::write(project.join("visible"), "visible")?;
        let docker_bin = PathBuf::from(std::env::var("VIBEFACE_ISOLATION_TEST_DOCKER")?);
        let config = ExecutionIsolationConfig {
            image: std::env::var("VIBEFACE_ISOLATION_TEST_IMAGE")?,
            docker_bin: docker_bin.clone(),
            egress: None,
        };
        let (events, _received) = mpsc::unbounded_channel();
        let engine = Engine::new(
            ExecutorConfig {
                execution_isolation: Some(config),
                data_dir: Some(data.clone()),
                timeout_secs: 30,
                ..ExecutorConfig::default()
            },
            events,
        );
        let context = ExecutionContext {
            space_id: SpaceId::parse(&format!("spc2_{}", "8".repeat(64)))?,
            resource_id: ResourceId::parse(&format!("res2_{}", "9".repeat(64)))?,
            operation_id: "watchdog-operation".into(),
            shared: true,
            lease_expires_at_ms: vibeface_protocol::now_ms() + 4_000,
        };
        let session = ExecutorSession {
            executor: ExecutorKind::ClaudeCode,
            cwd: project.clone(),
            native_session_id: None,
        };
        let node = hash(data.canonicalize()?.to_str().unwrap());
        let key = DockerIsolation::scope_key(&context);
        let name = format!("vibeface-{}-{}", &node[..16], &key[..32]);
        let running_engine = engine.clone();
        let running_context = context.clone();
        let task = tokio::spawn(async move {
            running_engine
                .execute_authorized(
                    &running_context,
                    &session,
                    EngineAction::Send {
                        text: "{\"wait\":true,\"attack\":true}".into(),
                    },
                    |_| async { Ok(()) },
                )
                .await
        });
        let outcome = async {
            tokio::time::timeout(Duration::from_secs(3), async { while !project.join("attack-proof.json").exists() { tokio::time::sleep(Duration::from_millis(20)).await; } }).await?;
            let proof: Value = serde_json::from_slice(&std::fs::read(project.join("attack-proof.json"))?)?;
            ensure!(proof.as_object().is_some_and(|proof| proof.len() == 6 && proof.values().all(|value| value == true)), "same-uid watchdog attack succeeded: {proof}");
            task.abort();
            tokio::time::timeout(Duration::from_secs(6), async {
                loop {
                    let state = engine.isolation.as_ref().unwrap().inspect(&name).await?.context("watchdog container missing")?;
                    if state["State"]["Running"] == false {
                        ensure!(state["State"]["ExitCode"] == 77 && state["State"]["Pid"] == 0, "watchdog did not enforce its deadline");
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
                Ok::<_, anyhow::Error>(())
            }).await??;
            ensure!(!project.join("late-write").exists(), "provider continued after its lease expired");
            let renewed = engine.renew_operation(&context, vibeface_protocol::now_ms() + 30_000).await;
            ensure!(renewed.is_err(), "expired operation was revived");
            println!("watchdog-proof={}", serde_json::json!({"same_uid_attacks":proof,"controller_future_aborted":true,"no_renewal_container_exited":true,"late_write_absent":true,"expired_renew_rejected":true}));
            Ok::<_, anyhow::Error>(())
        }.await;
        task.abort();
        let cleaned = cleanup_test_container(&docker_bin, &name).await;
        std::fs::remove_dir_all(root)?;
        outcome?;
        cleaned?;
        Ok(())
    }

    #[tokio::test]
    #[ignore = "requires the isolation fixture image and local Docker daemon"]
    async fn manager_and_executor_use_distinct_home_and_project_permissions() -> Result<()> {
        let root = PathBuf::from(std::env::var("VIBEFACE_ISOLATION_TEST_ROOT")?)
            .join(uuid::Uuid::new_v4().to_string());
        let data = root.join("node");
        let project = root.join("project");
        crate::config::private_dir(&data)?;
        std::fs::create_dir(&project)?;
        let settings = ExecutionIsolationConfig {
            image: std::env::var("VIBEFACE_ISOLATION_TEST_IMAGE")?,
            docker_bin: std::env::var("VIBEFACE_ISOLATION_TEST_DOCKER")?.into(),
            egress: None,
        };
        let executor = DockerIsolation::new(settings.clone(), data.clone());
        let manager = DockerIsolation::manager(settings, data);
        let context = ExecutionContext {
            space_id: SpaceId::parse(&format!("spc2_{}", "a".repeat(64)))?,
            resource_id: ResourceId::parse(&format!("res2_{}", "b".repeat(64)))?,
            operation_id: "distinct-manager-executor".into(),
            shared: true,
            lease_expires_at_ms: vibeface_protocol::now_ms() + 30_000,
        };
        let mut other_manager = context.clone();
        other_manager.operation_id = format!("manager:{}:{}", "a".repeat(64), uuid::Uuid::nil());
        let result = async {
            ensure!(executor.prepare(&other_manager, &project).await.is_err(), "ordinary execution accepted a Manager ID");
            let writable = executor.prepare(&context, &project).await?;
            let readonly = manager.prepare(&context, &project).await?;
            let other_readonly = manager.prepare(&other_manager, &project).await?;
            ensure!(writable.id != readonly.id && writable.name != readonly.name, "Manager reused executor generation");
            ensure!(readonly.id != other_readonly.id && readonly.name != other_readonly.name, "different AgentStarts reused a Manager generation");
            let mut write = writable.command("node");
            write.args(["-e", "require('node:fs').writeFileSync('/home/vibeface/executor-private','private');require('node:fs').writeFileSync('/project/executor-visible','visible')"]);
            checked_output(write, "write executor positive controls").await?;
            let mut check = readonly.command("node");
            check.args(["-e", "const fs=require('node:fs');if(fs.existsSync('/home/vibeface/executor-private')||fs.readFileSync('/project/executor-visible','utf8')!=='visible')process.exit(1);try{fs.writeFileSync('/project/manager-unsafe','unsafe');process.exit(2)}catch{};fs.writeFileSync('/home/vibeface/manager-private','private')"]);
            checked_output(check, "verify distinct Manager HOME and project read-only mount").await?;
            let mut reverse = writable.command("node");
            reverse.args(["-e", "if(require('node:fs').existsSync('/home/vibeface/manager-private'))process.exit(1)"]);
            checked_output(reverse, "verify executor cannot read Manager HOME").await?;
            let mut cross = other_readonly.command("node");
            cross.args(["-e", "const fs=require('node:fs');if(fs.existsSync('/home/vibeface/manager-private'))process.exit(1);fs.writeFileSync('/home/vibeface/other-manager-private','private')"]);
            checked_output(cross, "verify another AgentStart cannot read Manager HOME").await?;
            let mut cross_reverse = readonly.command("node");
            cross_reverse.args(["-e", "if(require('node:fs').existsSync('/home/vibeface/other-manager-private'))process.exit(1)"]);
            checked_output(cross_reverse, "verify Manager cannot read another AgentStart HOME").await?;
            println!("manager-executor-proof={}", serde_json::json!({"same_space_resource":true,"distinct_container_ids":true,"distinct_homes":true,"executor_project_write_positive":true,"manager_project_read_positive":true,"manager_project_write_denied":true,"different_agent_starts_isolated":true}));
            Ok::<_, anyhow::Error>(())
        }.await;
        let _ = executor.complete(&context).await;
        let _ = manager.complete(&context).await;
        let _ = manager.complete(&other_manager).await;
        std::fs::remove_dir_all(root)?;
        result
    }

    #[tokio::test]
    #[ignore = "requires the isolation fixture image and local Docker daemon"]
    async fn production_egress_rejects_host_other_scopes_and_extra_networks() -> Result<()> {
        use tokio::io::{AsyncBufReadExt, BufReader};
        let root = PathBuf::from(std::env::var("VIBEFACE_ISOLATION_TEST_ROOT")?)
            .join(uuid::Uuid::new_v4().to_string());
        let data = root.join("node");
        let project = root.join("project");
        crate::config::private_dir(&data)?;
        std::fs::create_dir(&project)?;
        let docker = PathBuf::from(std::env::var("VIBEFACE_ISOLATION_TEST_DOCKER")?);
        let image = std::env::var("VIBEFACE_ISOLATION_TEST_IMAGE")?;
        let gateway = format!("vibeface-gateway-test-{}", uuid::Uuid::new_v4());
        let mut command = Command::new(&docker);
        command.args(["run", "--detach", "--name", &gateway, "--cap-drop", "ALL", "--read-only", "--entrypoint", "node", &image, "-e", "require('node:http').createServer((q,s)=>{s.statusCode=q.url==='/v1/models'?200:403;s.end(q.url==='/v1/models'?'model-only':'denied')}).listen(8080,'0.0.0.0')"]);
        checked_output(command, "start fixture gateway").await?;
        let isolation = DockerIsolation::new(
            ExecutionIsolationConfig {
                image,
                docker_bin: docker.clone(),
                egress: Some(ExecutionEgressConfig {
                    gateway_container: gateway.clone(),
                    gateway_port: 8080,
                    codex_model: "fixture-model".into(),
                }),
            },
            data,
        );
        let context = ExecutionContext {
            space_id: SpaceId::parse(&format!("spc2_{}", "a".repeat(64)))?,
            resource_id: ResourceId::parse(&format!("res2_{}", "b".repeat(64)))?,
            operation_id: "egress-a".into(),
            shared: true,
            lease_expires_at_ms: vibeface_protocol::now_ms() + 30_000,
        };
        let mut other = context.clone();
        other.space_id = SpaceId::parse(&format!("spc2_{}", "c".repeat(64)))?;
        other.operation_id = "egress-b".into();
        let node = hash(isolation.data_dir.canonicalize()?.to_str().unwrap());
        let names = [
            DockerIsolation::scope_key(&context),
            DockerIsolation::scope_key(&other),
        ]
        .map(|key| format!("vibeface-{}-{}", &node[..16], &key[..32]));
        let outcome = async {
            let first = isolation.prepare(&context, &project).await?;
            let second = isolation.prepare(&other, &project).await?;
            let mut command = second.command("node");
            command.args(["-e", "require('node:http').createServer((q,s)=>s.end('scope-private')).listen(12345,'0.0.0.0',()=>console.log('ready'))"]);
            let mut listener = command.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null()).kill_on_drop(true).spawn()?;
            let mut line = String::new();
            tokio::time::timeout(Duration::from_secs(3), BufReader::new(listener.stdout.take().unwrap()).read_line(&mut line)).await??;
            ensure!(line.trim() == "ready", "other scope listener is not ready");
            let mut positive = second.command("node");
            positive.args(["-e", "fetch('http://127.0.0.1:12345').then(x=>x.text()).then(x=>{if(x!=='scope-private')process.exit(1)})"]);
            checked_output(positive, "verify other scope positive endpoint").await?;
            let info = isolation.inspect(&second.id).await?.unwrap();
            let other_ip = info["NetworkSettings"]["Networks"][&second.name]["IPAddress"].as_str().context("scope address missing")?;
            let host = tokio::net::TcpListener::bind("0.0.0.0:0").await?;
            let port = host.local_addr()?.port().to_string();
            let mut probe = first.command("node");
            probe.args(["-e", "const net=require('node:net');const get=async u=>{try{return await(await fetch(u,{signal:AbortSignal.timeout(400)})).text()}catch{return null}};const connect=(host,port)=>new Promise(r=>{const s=net.connect({host,port});s.setTimeout(400);s.on('connect',()=>{s.destroy();r(true)});s.on('error',()=>r(false));s.on('timeout',()=>{s.destroy();r(false)})});(async()=>{const yes=await get(process.argv[1]+'v1/models');const no=await get(process.argv[1]+'private-route');const peer=await connect(process.argv[2],12345);const host=await connect('host.docker.internal',Number(process.argv[3]));const privateNet=await connect('10.0.0.1',80);const publicNet=await connect('1.1.1.1',443);console.log(JSON.stringify({gateway_allowed:yes==='model-only',gateway_other_route_denied:no==='denied',other_scope_denied:!peer,host_denied:!host,private_network_denied:!privateNet,direct_public_network_denied:!publicNet}))})()", first.gateway_url.as_ref().unwrap(), other_ip, &port]);
            let proof: Value = serde_json::from_slice(&checked_output(probe, "probe isolated egress").await?)?;
            ensure!(proof.as_object().is_some_and(|p| p.len() == 6 && p.values().all(|v| v == true)), "egress isolation failed: {proof}");
            let mut drift = Command::new(&docker);
            drift.args(["network", "connect", "bridge", &first.id]);
            checked_output(drift, "add network drift fixture").await?;
            ensure!(isolation.prepare(&context, &project).await.is_err(), "extra network was accepted");
            println!("egress-proof={}", serde_json::json!({"connections":proof,"other_scope_listener_positive":true,"extra_network_rejected":true}));
            drop(listener);
            Ok::<_, anyhow::Error>(())
        }.await;
        for name in &names {
            let _ = cleanup_test_container(&docker, name).await;
            let mut command = Command::new(&docker);
            command.args(["network", "disconnect", name, &gateway]);
            let _ = timed_output(command).await;
            let mut command = Command::new(&docker);
            command.args(["network", "rm", name]);
            let _ = timed_output(command).await;
        }
        let cleaned = cleanup_test_container(&docker, &gateway).await;
        std::fs::remove_dir_all(root)?;
        outcome?;
        cleaned?;
        Ok(())
    }

    #[tokio::test]
    #[ignore = "requires the isolated Linux Codex image and a trusted live model gateway"]
    async fn production_engine_completes_a_real_codex_turn_through_the_model_gateway() -> Result<()>
    {
        let root = PathBuf::from(std::env::var("VIBEFACE_ISOLATION_TEST_ROOT")?)
            .join(uuid::Uuid::new_v4().to_string());
        let data = root.join("node");
        let project = root.join("project");
        crate::config::private_dir(&data)?;
        std::fs::create_dir(&project)?;
        let docker = PathBuf::from(std::env::var("VIBEFACE_ISOLATION_TEST_DOCKER")?);
        let gateway = std::env::var("VIBEFACE_ISOLATION_TEST_GATEWAY")?;
        let settings = ExecutionIsolationConfig {
            image: std::env::var("VIBEFACE_ISOLATION_TEST_IMAGE")?,
            docker_bin: docker.clone(),
            egress: Some(ExecutionEgressConfig {
                gateway_container: gateway.clone(),
                gateway_port: 8080,
                codex_model: "gpt-6.1-sol".into(),
            }),
        };
        let (events, mut received) = mpsc::unbounded_channel();
        let engine = Engine::new(
            ExecutorConfig {
                execution_isolation: Some(settings),
                data_dir: Some(data.clone()),
                timeout_secs: 25,
                ..ExecutorConfig::default()
            },
            events,
        );
        let context = ExecutionContext {
            space_id: SpaceId::parse(&format!("spc2_{}", "d".repeat(64)))?,
            resource_id: ResourceId::parse(&format!("res2_{}", "e".repeat(64)))?,
            operation_id: "real-model-operation".into(),
            shared: true,
            lease_expires_at_ms: vibeface_protocol::now_ms() + 30_000,
        };
        let session = ExecutorSession {
            executor: ExecutorKind::Codex,
            cwd: project,
            native_session_id: None,
        };
        let node = hash(data.canonicalize()?.to_str().unwrap());
        let key = DockerIsolation::scope_key(&context);
        let name = format!("vibeface-{}-{}", &node[..16], &key[..32]);
        let outcome = async {
            let result = engine.execute_authorized(&context, &session, EngineAction::Send { text: "Reply with exactly isolation-model-ok. Do not call any tools or commands. Do not read files.".into() }, |_| async { Ok(()) }).await?;
            let native = result.native_session_id.context("real turn has no native session")?;
            let turn = result.result["turn"]["id"].as_str().context("real turn has no ID")?.to_owned();
            let text = tokio::time::timeout(Duration::from_secs(25), async {
                loop {
                    let snapshot = engine.snapshot_authorized(&context).await?;
                    let state = &snapshot["threads"][&native];
                    if state["last_turn"]["id"] == turn {
                        match state["last_turn"]["status"].as_str() {
                            Some("completed") => break Ok::<_, anyhow::Error>(state["last_assistant"].as_str().context("real model returned no final text")?.to_owned()),
                            Some("failed" | "interrupted") => bail!("real model turn did not complete: {}", state["last_turn"]),
                            _ => {},
                        }
                    }
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
            }).await??;
            ensure!(text.trim() == "isolation-model-ok", "real model did not return the acceptance string");
            tokio::time::timeout(Duration::from_secs(2), async {
                loop {
                    let event = received.recv().await.context("real model event channel closed")?;
                    if event["event"]["method"] != "turn/completed" { continue; }
                    ensure!(event["execution_context"]["space_id"] == context.space_id.as_str() && event["execution_context"]["operation_id"] == context.operation_id && event["execution_context"]["container_id"].as_str().is_some_and(|id| id.len() == 64), "real Provider event lost its trusted execution context");
                    break Ok::<_, anyhow::Error>(());
                }
            }).await??;
            engine.complete_operation(&context).await?;
            println!("real-model-proof={}", serde_json::json!({"provider":"codex-cli 0.163.0","model":"gpt-6.1-sol","input":"fixed acceptance string, no tools","output":text.trim(),"model_turns":1,"trusted_scoped_completion":true,"container_stop_confirmed":true,"upstream_key_in_execution_container":false}));
            Ok::<_, anyhow::Error>(())
        }.await;
        let _ = cleanup_test_container(&docker, &name).await;
        let mut command = Command::new(&docker);
        command.args(["network", "disconnect", &name, &gateway]);
        let _ = timed_output(command).await;
        let mut command = Command::new(&docker);
        command.args(["network", "rm", &name]);
        let _ = timed_output(command).await;
        std::fs::remove_dir_all(root)?;
        outcome
    }
}
