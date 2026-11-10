use super::{
    Runtime,
    codex_server::Link,
    provider_adapter::{
        PendingDisconnectClassification, ScrubbedStderrTail, TerminationEvidence,
        classify_pending_disconnect, safe_model_text,
    },
};
use crate::config::ManagerSettings;
use crate::providers::{
    self, AppServer, AppServerMode, Authentication, CatalogCoverage, EffortOptions, LaunchCheck,
    ModelCatalog, ModelChoice, ProbeCode, ProbeIssue, ProbeStage, Provider, ProviderProbe,
    ProviderProbeReadiness,
};
use serde_json::Value;
use std::{collections::BTreeMap, path::PathBuf, process::Stdio, time::Duration};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::{
    process::{Child, ChildStderr},
    task::JoinHandle,
};
use vibeface_bridge::agent::{CursorAuthentication, CursorCatalog, CursorObservation};
use vibeface_bridge::platform::tree;

const OUTPUT_LIMIT: u64 = 64 * 1024;
const PROBE_LIMIT: Duration = Duration::from_secs(5);

#[derive(Clone, PartialEq)]
struct ProbeInput {
    bin: PathBuf,
    env: BTreeMap<String, String>,
    authentication: Option<String>,
    server: Option<AppServer>,
    epoch: u64,
    manager: Option<ManagerSettings>,
}

#[derive(serde::Serialize)]
enum LaunchIdentity<'a> {
    Process {
        bin: PathBuf,
        env: &'a BTreeMap<String, String>,
        authentication: Option<String>,
    },
    System {
        socket: Option<&'a std::path::Path>,
    },
}

pub(super) fn generation_for(
    identity: &crate::crypto::LocalIdentity,
    provider: Provider,
    local: &providers::Local,
) -> anyhow::Result<String> {
    let server = &local.switches.codex_app_server;
    let env = local.launch_env(provider);
    let launch = if provider.on_app_server() && server.mode == AppServerMode::System {
        LaunchIdentity::System {
            socket: server.socket.as_deref(),
        }
    } else {
        LaunchIdentity::Process {
            bin: local.bin(provider),
            env: &env,
            authentication: (provider == Provider::Codex)
                .then(|| local.codex_external_auth().map(|auth| auth.fingerprint()))
                .flatten(),
        }
    };
    identity.provider_fingerprint(&serde_json::to_vec(&(provider, launch))?)
}

pub(super) struct CachedProviderProbe {
    input: ProbeInput,
    probe: ProviderProbe,
}

struct ClaudeLogin {
    input: ProbeInput,
    observation: Result<Option<Authentication>, ProbeIssue>,
}

impl Runtime {
    pub(crate) async fn prepare_claude_login(
        &self,
        bin: &std::path::Path,
        env: BTreeMap<String, String>,
    ) -> anyhow::Result<BTreeMap<String, String>> {
        let prepared = self
            .prepare_claude_probe(ProbeInput {
                bin: bin.into(),
                env,
                authentication: None,
                server: None,
                epoch: 0,
                manager: None,
            })
            .await?;
        anyhow::ensure!(
            prepared.observation != Ok(Some(Authentication::SignInRequired)),
            "Claude Code requires sign-in; refresh the local CLI login or sign in for this Node"
        );
        Ok(prepared.input.env)
    }

    async fn prepare_claude_probe(&self, mut input: ProbeInput) -> anyhow::Result<ClaudeLogin> {
        let roots = providers::ProviderRoots::new(&self.data_dir);
        roots.clear_claude_borrow()?;
        let mut observation = claude_authentication(&input, &self.data_dir).await;
        if observation == Ok(Some(Authentication::SignInRequired)) {
            let mut local = providers::Local {
                switches: Default::default(),
                bins: BTreeMap::from([(Provider::Claude, input.bin.clone())]),
                env: Default::default(),
            };
            local.env.apply(
                Provider::Claude,
                input
                    .env
                    .iter()
                    .map(|(name, value)| (name.clone(), Some(value.clone())))
                    .collect(),
            )?;
            roots.prepare_login(Provider::Claude, &local, &self.host)?;
            anyhow::ensure!(
                !roots.imported_login_expired(Provider::Claude, &local),
                "the borrowed Claude CLI login has expired; refresh the local CLI login or sign in for this Node"
            );
            let env = roots.borrowed_claude_environment(&local)?;
            if env != input.env {
                input.env = env;
                observation = claude_authentication(&input, &self.data_dir).await;
            }
        }
        Ok(ClaudeLogin { input, observation })
    }

    fn probe_input(&self, provider: Provider, local: &providers::Local) -> ProbeInput {
        ProbeInput {
            bin: local.bin(provider),
            env: if provider == Provider::Claude {
                providers::ProviderRoots::new(&self.data_dir)
                    .borrowed_claude_environment(local)
                    .unwrap_or_else(|_| local.launch_env(provider))
            } else {
                local.launch_env(provider)
            },
            authentication: (provider == Provider::Codex)
                .then(|| local.codex_external_auth().map(|auth| auth.fingerprint()))
                .flatten(),
            server: provider
                .on_app_server()
                .then(|| local.switches.codex_app_server.clone()),
            epoch: if provider.on_app_server() {
                self.codex.epoch()
            } else {
                0
            },
            manager: self
                .manager()
                .filter(|manager| manager.provider == provider),
        }
    }

    fn cached_probe(&self, provider: Provider, local: &providers::Local) -> ProviderProbe {
        let input = self.probe_input(provider, local);
        let mut cache = self
            .provider_probes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let entry = cache
            .entry(provider)
            .or_insert_with(|| CachedProviderProbe {
                input: input.clone(),
                probe: unchecked(provider),
            });
        if entry.input != input {
            *entry = CachedProviderProbe {
                input,
                probe: unchecked(provider),
            };
        }
        entry.probe.clone()
    }

    pub(crate) fn provider_generation(
        &self,
        provider: Provider,
        local: &providers::Local,
    ) -> anyhow::Result<String> {
        generation_for(&self.identity, provider, local)
    }

    pub(crate) async fn provider_probe_snapshot(&self, provider: Provider) -> ProviderProbe {
        let local = self.providers();
        let mut probe = self.cached_probe(provider, &local);
        if providers::ProviderRoots::new(&self.data_dir).imported_login_expired(provider, &local) {
            probe.authentication = Authentication::SignInRequired;
        }
        if let Some(issue) = prerequisites(provider, &local) {
            probe.launch = LaunchCheck::Failed { issue };
            return probe;
        }
        if self
            .pending_provider_launches
            .load(std::sync::atomic::Ordering::SeqCst)
            || provider.on_app_server()
                && self.codex.configuration_pending(
                    &local.codex_target(
                        self.manager()
                            .is_some_and(|manager| manager.provider.on_app_server()),
                    ),
                )
        {
            if !matches!(probe.launch, LaunchCheck::Failed { .. }) {
                probe.launch = LaunchCheck::Starting;
            }
            return probe;
        }
        if provider.on_app_server() {
            probe.launch = match self.codex.link().await {
                Link::Connected { .. } => LaunchCheck::Launchable,
                Link::Starting => LaunchCheck::Starting,
                Link::Off => LaunchCheck::Unchecked,
                Link::NotFound { .. }
                | Link::HandshakeFailed { .. }
                | Link::LaunchFailed { .. } => LaunchCheck::Failed {
                    issue: ProbeIssue::new(ProbeStage::Launch, ProbeCode::LaunchFailed),
                },
            };
        }
        probe
    }

    pub(crate) async fn provider_readiness(
        &self,
        provider: Provider,
        model: &str,
    ) -> ProviderProbeReadiness {
        self.provider_probe_snapshot(provider)
            .await
            .readiness(model)
    }

    pub(crate) async fn manager_readiness(&self) -> Option<ProviderProbeReadiness> {
        let manager = self.manager()?;
        if !manager.workspace.is_dir() {
            return Some(ProviderProbeReadiness::NeedsSetup {
                issue: ProbeIssue::new(ProbeStage::Launch, ProbeCode::ConfigInvalid),
            });
        }
        Some(
            self.provider_readiness(manager.provider, &manager.model)
                .await,
        )
    }

    pub(crate) async fn provider_catalog(&self, provider: Provider) -> ModelCatalog {
        self.provider_probe_snapshot(provider).await.catalog
    }

    pub(crate) async fn probe_provider(&self, provider: Provider) -> ProviderProbe {
        let _refresh = self.provider_probe_locks[&provider].lock().await;
        if provider == Provider::Codex && self.refresh_provider_login(provider).await.is_err() {
            tracing::warn!(
                event = "providers.local_login_unavailable",
                "could not refresh the imported Codex access token"
            );
        }
        let local = self.providers();
        let original_input = self.probe_input(provider, &local);
        let mut probe = self.provider_probe_snapshot(provider).await;
        let claude_observation = if provider == Provider::Claude {
            let mut input = original_input.clone();
            input.env = local.launch_env(provider);
            match self.prepare_claude_probe(input).await {
                Ok(prepared) => Some(prepared.observation),
                Err(_) => {
                    tracing::warn!(
                        event = "providers.local_login_unavailable",
                        "could not reuse the local Claude CLI access token"
                    );
                    None
                }
            }
        } else {
            None
        };
        let input = self.probe_input(provider, &local);
        if input != original_input {
            let mut cache = self
                .provider_probes
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(entry) = cache.get_mut(&provider)
                && entry.input == original_input
                && entry.probe.config_id == probe.config_id
            {
                *entry = CachedProviderProbe {
                    input: input.clone(),
                    probe: unchecked(provider),
                };
                probe = entry.probe.clone();
            }
        }
        probe.authentication = Authentication::Unverified;
        probe.catalog = ModelCatalog::default();
        if !provider.on_app_server()
            && prerequisites(provider, &local).is_none()
            && matches!(probe.launch, LaunchCheck::Failed { .. })
        {
            probe.launch = LaunchCheck::Unchecked;
        }
        if providers::ProviderRoots::new(&self.data_dir).imported_login_expired(provider, &local) {
            probe.authentication = Authentication::SignInRequired;
        } else if provider.on_app_server() && matches!(probe.launch, LaunchCheck::Launchable) {
            let (authentication, catalog) = self.codex.provider_observation().await;
            probe.authentication = authentication;
            probe.catalog = catalog;
        } else if matches!(
            probe.launch,
            LaunchCheck::Unchecked | LaunchCheck::Launchable
        ) {
            probe.launch = LaunchCheck::Unchecked;
            match provider {
                Provider::Claude => match match claude_observation {
                    Some(observation) => observation,
                    None => claude_authentication(&input, &self.data_dir).await,
                } {
                    Ok(Some(authentication)) => {
                        probe.launch = LaunchCheck::Launchable;
                        probe.authentication = authentication;
                    }
                    Err(issue) => probe.launch = LaunchCheck::Failed { issue },
                    Ok(None) => {}
                },
                Provider::Pi => match pi_catalog(&input, &self.data_dir).await {
                    Ok(Some(catalog)) => {
                        probe.launch = LaunchCheck::Launchable;
                        probe.catalog = catalog;
                    }
                    Err(issue) => probe.launch = LaunchCheck::Failed { issue },
                    Ok(None) => {}
                },
                Provider::Cursor => {
                    let env = input.env.clone();
                    if let Some(bridge) = self.cursor.get() {
                        let workspace = input
                            .manager
                            .as_ref()
                            .map(|manager| manager.workspace.clone())
                            .unwrap_or_else(|| self.data_dir.clone());
                        match bridge
                            .provider_observation(
                                &input.bin,
                                &workspace,
                                &self.data_dir.join("cursor/provider-probe"),
                                &env,
                            )
                            .await
                        {
                            Ok(observation) => {
                                if observation.launchable {
                                    probe.launch = LaunchCheck::Launchable;
                                }
                                probe.authentication = cursor_authentication(&observation);
                                probe.catalog = cursor_catalog(&observation);
                            }
                            Err(_) => {
                                probe.launch = LaunchCheck::Failed {
                                    issue: ProbeIssue::new(
                                        ProbeStage::Launch,
                                        ProbeCode::LaunchFailed,
                                    ),
                                };
                            }
                        }
                    }
                }
                _ => {}
            }
        }
        probe.checked_at_ms = Some(super::now_ms());
        if self.probe_input(provider, &self.providers()) == input {
            let mut cache = self
                .provider_probes
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(entry) = cache.get_mut(&provider)
                && entry.input == input
                && entry.probe.config_id == probe.config_id
            {
                entry.probe = probe.clone();
                return probe;
            }
        }
        self.provider_probe_snapshot(provider).await
    }
}

fn cursor_authentication(observation: &CursorObservation) -> Authentication {
    match observation.authentication {
        CursorAuthentication::Authenticated => Authentication::Authenticated,
        CursorAuthentication::SignInRequired => Authentication::SignInRequired,
        CursorAuthentication::Unknown => Authentication::Unverified,
    }
}

fn cursor_catalog(observation: &CursorObservation) -> ModelCatalog {
    let CursorCatalog::Available { models } = &observation.catalog else {
        return ModelCatalog::default();
    };
    let models = models
        .iter()
        .filter(|model| safe_model_text(&model.id) && safe_model_text(&model.display_name))
        .map(|model| ModelChoice {
            id: model.id.clone(),
            name: model.display_name.clone(),
            default: false,
            effort: EffortOptions::Unknown,
        })
        .collect();
    ModelCatalog::Available {
        models,
        coverage: CatalogCoverage::Partial,
    }
}

/// Names a probe's process tree in this account's job object namespace.
const PROBE_TREE_KEY: &str = "provider-probe";

struct ProbeChild {
    child: Child,
    tree: tree::Tree,
}

fn stderr_task(stderr: Option<ChildStderr>) -> Option<JoinHandle<ScrubbedStderrTail>> {
    stderr.map(|stderr| tokio::spawn(ScrubbedStderrTail::read(stderr)))
}

async fn finish_stderr(task: Option<JoinHandle<ScrubbedStderrTail>>) -> ScrubbedStderrTail {
    let Some(task) = task else {
        return ScrubbedStderrTail::default();
    };
    tokio::time::timeout(Duration::from_millis(250), task)
        .await
        .ok()
        .and_then(Result::ok)
        .unwrap_or_default()
}

fn issue_with_evidence(
    stage: ProbeStage,
    code: ProbeCode,
    tail: &ScrubbedStderrTail,
    pending: bool,
    termination: Option<&TerminationEvidence>,
) -> ProbeIssue {
    let mut issue = ProbeIssue::new(stage, code);
    issue.safe_error = tail
        .safe_error()
        .or_else(|| {
            matches!(
                classify_pending_disconnect(pending, termination),
                PendingDisconnectClassification::Unknown
            )
            .then_some("provider_disconnected")
        })
        .map(str::to_owned);
    issue
}

impl Drop for ProbeChild {
    fn drop(&mut self) {
        // A cancelled probe must also stop helpers the CLI started.
        self.tree.kill();
    }
}

fn spawn_probe(
    input: &ProbeInput,
    fallback: &std::path::Path,
    args: &[&str],
) -> Result<ProbeChild, ProbeIssue> {
    let mut command = std::process::Command::new(&input.bin);
    #[cfg(test)]
    for name in [
        "ANTHROPIC_API_KEY",
        "ANTHROPIC_AUTH_TOKEN",
        "CLAUDE_CODE_OAUTH_TOKEN",
    ] {
        command.env_remove(name);
    }
    for (name, _) in std::env::vars_os() {
        let text = name.to_string_lossy();
        if text.starts_with("VIBEFACE_")
            || matches!(text.as_ref(), "CLAUDECODE" | "CLAUDE_CODE_ENTRYPOINT")
        {
            command.env_remove(name);
        }
    }
    command
        .args(args)
        .envs(&input.env)
        .current_dir(
            input
                .manager
                .as_ref()
                .map_or(fallback, |manager| manager.workspace.as_path()),
        )
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let (tree, child) = tree::Tree::spawn(command, PROBE_TREE_KEY, true).map_err(|error| {
        let missing = error
            .downcast_ref::<std::io::Error>()
            .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound);
        if missing {
            ProbeIssue::new(ProbeStage::Install, ProbeCode::NotInstalled)
        } else {
            ProbeIssue::new(ProbeStage::Launch, ProbeCode::LaunchFailed)
        }
    })?;
    Ok(ProbeChild { child, tree })
}

async fn claude_authentication(
    input: &ProbeInput,
    fallback: &std::path::Path,
) -> Result<Option<Authentication>, ProbeIssue> {
    let mut process = spawn_probe(input, fallback, &["auth", "status", "--json"])?;
    process.child.stdin.take();
    let stderr = stderr_task(process.child.stderr.take());
    let Some(stdout) = process.child.stdout.take() else {
        drop(process);
        let _ = finish_stderr(stderr).await;
        return Ok(None);
    };
    let mut stdout = stdout.take(OUTPUT_LIMIT + 1);
    let mut bytes = Vec::new();
    let status = tokio::time::timeout(PROBE_LIMIT, async {
        stdout.read_to_end(&mut bytes).await.ok()?;
        if bytes.len() > OUTPUT_LIMIT as usize {
            return None;
        }
        process.child.wait().await.ok()
    })
    .await
    .ok()
    .flatten();
    let termination = status
        .as_ref()
        .map(|status| TerminationEvidence::from(*status));
    let pending = status.is_none() || bytes.is_empty();
    drop(process);
    let tail = finish_stderr(stderr).await;
    if let Some(authentication) = status
        .as_ref()
        .and_then(|status| claude_authentication_response(&bytes, status.success()))
    {
        return Ok(Some(authentication));
    }
    if !tail.is_empty()
        || matches!(
            classify_pending_disconnect(pending, termination.as_ref()),
            PendingDisconnectClassification::Unknown
        )
    {
        return Err(issue_with_evidence(
            ProbeStage::Authentication,
            ProbeCode::AuthenticationUnverified,
            &tail,
            pending,
            termination.as_ref(),
        ));
    }
    Ok(None)
}

fn claude_authentication_response(bytes: &[u8], success: bool) -> Option<Authentication> {
    let status: Value = serde_json::from_slice(bytes).ok()?;
    match status["loggedIn"].as_bool() {
        Some(true) if success => Some(Authentication::Authenticated),
        Some(false) => Some(Authentication::SignInRequired),
        _ => None,
    }
}

async fn pi_catalog(
    input: &ProbeInput,
    fallback: &std::path::Path,
) -> Result<Option<ModelCatalog>, ProbeIssue> {
    let mut process = spawn_probe(
        input,
        fallback,
        &[
            "--mode",
            "rpc",
            "--no-session",
            "--no-tools",
            "--no-extensions",
        ],
    )?;
    let stderr = stderr_task(process.child.stderr.take());
    let (Some(mut stdin), Some(stdout)) = (process.child.stdin.take(), process.child.stdout.take())
    else {
        drop(process);
        let _ = finish_stderr(stderr).await;
        return Ok(None);
    };
    let stdout = stdout.take(OUTPUT_LIMIT + 1);
    let mut stdout = BufReader::new(stdout);
    let mut pending = false;
    let catalog = tokio::time::timeout(PROBE_LIMIT, async {
        stdin
            .write_all(b"{\"id\":\"provider-probe\",\"type\":\"get_available_models\"}\n")
            .await
            .ok()?;
        stdin.flush().await.ok()?;
        pending = true;
        let mut total = 0;
        loop {
            let mut line = String::new();
            let read = stdout.read_line(&mut line).await.ok()?;
            total += read;
            if read == 0 || total > OUTPUT_LIMIT as usize {
                return None;
            }
            let Ok(response) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            if response["type"] == "response" && response["id"] == "provider-probe" {
                if response["success"] != true {
                    return None;
                }
                return pi_model_catalog(&response["data"]);
            }
        }
    })
    .await
    .ok()
    .flatten();
    let termination = process
        .child
        .try_wait()
        .ok()
        .flatten()
        .map(TerminationEvidence::from);
    drop(process);
    let tail = finish_stderr(stderr).await;
    if catalog.is_some() {
        return Ok(catalog);
    }
    if !tail.is_empty()
        || matches!(
            classify_pending_disconnect(pending, termination.as_ref()),
            PendingDisconnectClassification::Unknown
        )
    {
        return Err(issue_with_evidence(
            ProbeStage::ModelList,
            ProbeCode::CatalogUnavailable,
            &tail,
            pending,
            termination.as_ref(),
        ));
    }
    Ok(None)
}

fn pi_model_catalog(data: &Value) -> Option<ModelCatalog> {
    let models = data["models"]
        .as_array()?
        .iter()
        .filter_map(|model| {
            let provider = model["provider"]
                .as_str()
                .filter(|value| safe_model_text(value))?;
            let id = model["id"]
                .as_str()
                .filter(|value| safe_model_text(value))?;
            let id = format!("{provider}/{id}");
            if !safe_model_text(&id) {
                return None;
            }
            Some(ModelChoice {
                name: model["name"]
                    .as_str()
                    .filter(|value| safe_model_text(value))
                    .unwrap_or(&id)
                    .into(),
                id,
                default: false,
                effort: if model["reasoning"] == false {
                    EffortOptions::ProviderControlled
                } else {
                    EffortOptions::Unknown
                },
            })
        })
        .collect();
    Some(ModelCatalog::Available {
        models,
        coverage: CatalogCoverage::Complete,
    })
}

fn unchecked(provider: Provider) -> ProviderProbe {
    ProviderProbe {
        provider,
        config_id: uuid::Uuid::new_v4().simple().to_string(),
        checked_at_ms: None,
        launch: LaunchCheck::Unchecked,
        authentication: Authentication::Unverified,
        catalog: ModelCatalog::default(),
        version: None,
    }
}

fn prerequisites(provider: Provider, local: &providers::Local) -> Option<ProbeIssue> {
    if !local.launches(provider) {
        return None;
    }
    let bin = local.bin(provider);
    let env = local.launch_env(provider);
    let path = env
        .get("PATH")
        .map(std::ffi::OsString::from)
        .or_else(|| crate::discovery::Host::current().path);
    if !crate::discovery::runnable(&bin, path.as_ref()) {
        return Some(ProbeIssue::new(
            ProbeStage::Install,
            ProbeCode::NotInstalled,
        ));
    }
    let Ok(required) = provider.key_variables(&env) else {
        return Some(ProbeIssue::new(
            ProbeStage::Launch,
            ProbeCode::ConfigInvalid,
        ));
    };
    if required.iter().any(|name| {
        !env.contains_key(name) && std::env::var_os(name).is_none_or(|value| value.is_empty())
    }) {
        return Some(ProbeIssue::new(
            ProbeStage::Authentication,
            ProbeCode::MissingCredential,
        ));
    }
    None
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::runtime::test_support::initialize_node;
    use serde_json::json;
    use std::os::unix::fs::PermissionsExt;
    use vibeface_bridge::agent::{CursorAuthentication, CursorModel};

    async fn cursor_probe_node(
        env: BTreeMap<String, String>,
    ) -> anyhow::Result<(std::sync::Arc<Runtime>, PathBuf)> {
        let dir = std::env::temp_dir().join(format!(
            "vibeface-cursor-provider-probe-{}",
            uuid::Uuid::new_v4()
        ));
        crate::config::private_dir(&dir)?;
        let dir = dir.canonicalize()?;
        let mut configuration = crate::config::Config::local("127.0.0.1:0".parse()?, vec![], false);
        configuration.providers.execution = [Provider::Cursor].into();
        configuration.bins.insert(
            Provider::Cursor,
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("../../crates/bridge/tests/fixtures/fake-cursor-agent"),
        );
        initialize_node(
            &dir,
            &configuration,
            &crate::crypto::LocalIdentity::generate(),
        )?;
        let mut variables = providers::ProviderEnv::default();
        variables.apply(
            Provider::Cursor,
            env.into_iter()
                .map(|(name, value)| (name, Some(value)))
                .collect(),
        )?;
        let mut configuration = crate::config::Config::load(&dir)?;
        configuration.provider_env = variables;
        configuration.save(&dir)?;
        let node = std::sync::Arc::new(Runtime::open(&dir).await?);
        node.tools_ready("http://127.0.0.1:9".into())?;
        Ok((node, dir))
    }

    #[tokio::test]
    async fn cursor_probe_confirms_metadata_and_keeps_unknown_models_attemptable()
    -> anyhow::Result<()> {
        let (node, dir) = cursor_probe_node(BTreeMap::new()).await?;
        let probe = node.probe_provider(Provider::Cursor).await;
        assert_eq!(probe.launch, LaunchCheck::Launchable);
        assert_eq!(probe.authentication, Authentication::Authenticated);
        assert_eq!(probe.readiness("composer-2"), ProviderProbeReadiness::Ready);
        for model in ["auto", "custom-model"] {
            let readiness = probe.readiness(model);
            assert!(matches!(
                readiness,
                ProviderProbeReadiness::Unverified { .. }
            ));
            assert!(readiness.can_attempt());
        }
        node.cursor.get().unwrap().stop().await;
        drop(node);
        std::fs::remove_dir_all(dir)?;
        Ok(())
    }

    #[tokio::test]
    async fn cursor_probe_does_not_confirm_launch_when_bridge_exits_before_handshake()
    -> anyhow::Result<()> {
        let (node, dir) = cursor_probe_node(BTreeMap::from([(
            "FAKE_BRIDGE_EXIT_BEFORE_READY".into(),
            "1".into(),
        )]))
        .await?;
        let probe = node.probe_provider(Provider::Cursor).await;
        assert_ne!(probe.launch, LaunchCheck::Launchable);
        assert_eq!(probe.authentication, Authentication::Unverified);
        assert_eq!(probe.catalog, ModelCatalog::default());
        node.cursor.get().unwrap().stop().await;
        drop(node);
        std::fs::remove_dir_all(dir)?;
        Ok(())
    }

    #[tokio::test]
    async fn cursor_probe_cannot_overwrite_a_replaced_configuration() -> anyhow::Result<()> {
        let (node, dir) = cursor_probe_node(BTreeMap::new()).await?;
        let ready = dir.join("me-ready");
        let release = dir.join("me-release");
        let provider = Provider::Cursor;
        let mut original = node.providers();
        original.env.apply(
            provider,
            BTreeMap::from([
                ("FAKE_BRIDGE_ME".into(), Some("200".into())),
                (
                    "FAKE_BRIDGE_ME_READY".into(),
                    Some(ready.display().to_string()),
                ),
                (
                    "FAKE_BRIDGE_ME_RELEASE".into(),
                    Some(release.display().to_string()),
                ),
            ]),
        )?;
        *node.providers.lock().unwrap() = original.clone();
        let first = node.provider_probe_snapshot(provider).await;
        let result = async {
            let pending = node.probe_provider(provider);
            tokio::pin!(pending);
            tokio::select! {
                _ = async {
                    while !ready.exists() {
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                } => {},
                early = &mut pending => anyhow::bail!("Cursor probe completed before the fixture barrier: {early:?}"),
            }
            let save = node.config_save.lock().await;
            for key in ["fixture-new-key", "fixture-api-key"] {
                node.write_providers(
                    |target| {
                        target.local.env.apply(
                            provider,
                            BTreeMap::from([("FAKE_CONFIG_ID".into(), Some(key.into()))]),
                        )
                    },
                    None,
                )
                .await?;
            }
            drop(save);
            let current = node.provider_probe_snapshot(provider).await;
            std::fs::write(release, b"ready")?;
            let returned = pending.await;
            Ok::<_, anyhow::Error>((current, returned))
        }
        .await;
        node.cursor.get().unwrap().stop().await;
        drop(node);
        let (current, returned) = result?;
        std::fs::remove_dir_all(dir)?;
        assert_ne!(first.config_id, current.config_id);
        assert_eq!(returned, current);
        assert_eq!(returned.authentication, Authentication::Unverified);
        assert_eq!(returned.checked_at_ms, None);
        Ok(())
    }

    #[test]
    fn cursor_metadata_maps_to_authenticated_partial_catalog_without_inventing_auto() {
        let observation = CursorObservation {
            launchable: true,
            authentication: CursorAuthentication::Authenticated,
            catalog: CursorCatalog::Available {
                models: vec![CursorModel {
                    id: "composer-2".into(),
                    display_name: "Composer 2".into(),
                }],
            },
        };
        assert_eq!(
            cursor_authentication(&observation),
            Authentication::Authenticated
        );
        let ModelCatalog::Available { models, coverage } = cursor_catalog(&observation) else {
            panic!("catalog should remain available")
        };
        assert_eq!(coverage, CatalogCoverage::Partial);
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].id, "composer-2");
        assert!(!models.iter().any(|model| model.id == "auto"));
    }

    #[test]
    fn cursor_metadata_unknowns_do_not_become_auth_failures() {
        let observation = CursorObservation {
            launchable: true,
            authentication: CursorAuthentication::Unknown,
            catalog: CursorCatalog::Unavailable,
        };
        assert_eq!(
            cursor_authentication(&observation),
            Authentication::Unverified
        );
        assert_eq!(cursor_catalog(&observation), ModelCatalog::default());
    }

    #[tokio::test]
    async fn runtime_generation_follows_effective_launch_and_not_probe_epochs() -> anyhow::Result<()>
    {
        let (node, _, dir) = super::super::triage::tests::node().await?;
        let provider = Provider::Codex;
        let mut local = node.providers();
        local.switches.codex_app_server = AppServer {
            mode: AppServerMode::Spawn,
            socket: Some("/unused/socket-a".into()),
        };
        local.bins.insert(provider, "/fixture/codex".into());
        local.env.apply(
            provider,
            BTreeMap::from([
                ("CODEX_HOME".into(), Some("/fixture/home".into())),
                ("CPA_API_KEY".into(), Some("fixture-secret-a".into())),
            ]),
        )?;
        let generation = node.provider_generation(provider, &local)?;
        let first = node.cached_probe(provider, &local);
        node.provider_probes.lock().unwrap().clear();
        let restarted = node.cached_probe(provider, &local);
        assert_ne!(first.config_id, restarted.config_id);
        assert_eq!(node.provider_generation(provider, &local)?, generation);
        node.provider_probes
            .lock()
            .unwrap()
            .get_mut(&provider)
            .unwrap()
            .input
            .epoch = u64::MAX;
        let reconnected = node.cached_probe(provider, &local);
        assert_ne!(restarted.config_id, reconnected.config_id);
        assert_eq!(node.provider_generation(provider, &local)?, generation);

        let mut changed = local.clone();
        changed
            .bins
            .insert(provider, "/fixture/another-codex".into());
        assert_ne!(node.provider_generation(provider, &changed)?, generation);
        for (name, value) in [
            ("CODEX_HOME", "/fixture/other-home"),
            ("CPA_API_KEY", "fixture-secret-b"),
        ] {
            let mut changed = local.clone();
            changed.env.apply(
                provider,
                BTreeMap::from([(name.into(), Some(value.into()))]),
            )?;
            assert_ne!(node.provider_generation(provider, &changed)?, generation);
        }
        local.switches.codex_app_server.socket = Some("/unused/socket-b".into());
        local.env.apply(
            Provider::Claude,
            BTreeMap::from([("ANTHROPIC_API_KEY".into(), Some("unrelated-secret".into()))]),
        )?;
        assert_eq!(node.provider_generation(provider, &local)?, generation);

        let bin = dir.join("nvm/bin");
        std::fs::create_dir_all(&bin)?;
        let mut nvm = local.clone();
        nvm.bins.insert(provider, bin.join("codex"));
        nvm.env.apply(
            provider,
            BTreeMap::from([("PATH".into(), Some(dir.join("empty").display().to_string()))]),
        )?;
        let before_node = node.provider_generation(provider, &nvm)?;
        for name in ["codex", "node"] {
            std::fs::write(bin.join(name), "#!/bin/sh\nexit 0\n")?;
            std::fs::set_permissions(bin.join(name), std::fs::Permissions::from_mode(0o700))?;
        }
        let with_node = node.provider_generation(provider, &nvm)?;
        assert_ne!(before_node, with_node);
        assert_eq!(
            node.probe_input(provider, &nvm).env,
            nvm.launch_env(provider)
        );
        let effective_path = nvm.launch_env(provider)["PATH"].clone();
        nvm.env.apply(
            provider,
            BTreeMap::from([("PATH".into(), Some(effective_path))]),
        )?;
        assert_eq!(node.provider_generation(provider, &nvm)?, with_node);
        nvm.bins.remove(&provider);
        assert!(prerequisites(provider, &nvm).is_none());
        let status = node
            .provider_status(provider, &nvm, &node.settings_snapshot())
            .await;
        assert_eq!(status["installed"], true);

        local.switches.codex_app_server.mode = AppServerMode::System;
        let system = node.provider_generation(provider, &local)?;
        assert_ne!(system, generation);
        local.bins.insert(provider, "/another/unused-bin".into());
        local.env.apply(
            provider,
            BTreeMap::from([("CPA_API_KEY".into(), Some("unused-secret".into()))]),
        )?;
        assert_eq!(node.provider_generation(provider, &local)?, system);
        local.switches.codex_app_server.socket = Some("/fixture/other-system".into());
        assert_ne!(node.provider_generation(provider, &local)?, system);

        use std::os::unix::ffi::OsStringExt;
        local.switches.codex_app_server.mode = AppServerMode::Spawn;
        local
            .bins
            .insert(provider, std::ffi::OsString::from_vec(vec![0xff]).into());
        assert!(node.provider_generation(provider, &local).is_err());
        drop(node);
        std::fs::remove_dir_all(dir)?;
        Ok(())
    }

    #[tokio::test]
    async fn a_probe_from_the_first_configuration_cannot_overwrite_its_replacement()
    -> anyhow::Result<()> {
        let (node, _, dir) = super::super::triage::tests::node().await?;
        let (fixture_dir, input) = fixture(
            r#"
[ "$*" = "auth status --json" ] || exit 9
touch "$PROBE_READY"
while [ ! -f "$PROBE_RELEASE" ]; do sleep 0.01; done
printf '%s\n' '{"loggedIn":false}'
exit 1
"#,
        )?;
        let ready = fixture_dir.join("ready");
        let release = fixture_dir.join("release");
        let provider = Provider::Claude;
        let mut original = node.providers();
        original.bins.insert(provider, input.bin);
        original.switches.execution.insert(provider);
        original.env.apply(
            provider,
            BTreeMap::from([
                ("PROBE_READY".into(), Some(ready.display().to_string())),
                ("PROBE_RELEASE".into(), Some(release.display().to_string())),
                ("ANTHROPIC_API_KEY".into(), Some("fixture-secret-a".into())),
            ]),
        )?;
        *node.providers.lock().unwrap() = original.clone();
        let generation = node.provider_generation(provider, &original)?;
        let first = node.provider_probe_snapshot(provider).await;
        let running = node.clone();
        let pending = tokio::spawn(async move { running.probe_provider(provider).await });
        tokio::time::timeout(Duration::from_secs(3), async {
            while !ready.exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await?;
        let mut changed = original.clone();
        changed.env.apply(
            provider,
            BTreeMap::from([("ANTHROPIC_API_KEY".into(), Some("fixture-secret-b".into()))]),
        )?;
        *node.providers.lock().unwrap() = changed.clone();
        let between = node.provider_probe_snapshot(provider).await;
        assert_ne!(node.provider_generation(provider, &changed)?, generation);
        *node.providers.lock().unwrap() = original.clone();
        let current = node.provider_probe_snapshot(provider).await;
        assert_ne!(first.config_id, between.config_id);
        assert_ne!(first.config_id, current.config_id);
        assert_eq!(node.provider_generation(provider, &original)?, generation);
        std::fs::write(&release, b"ready")?;
        let returned = pending.await?;
        assert_eq!(returned.config_id, current.config_id);
        assert_eq!(returned.authentication, Authentication::Unverified);
        assert_eq!(returned.checked_at_ms, None);
        let saved = node.provider_probe_snapshot(provider).await;
        assert_eq!(saved, returned);
        drop(node);
        std::fs::remove_dir_all(dir)?;
        std::fs::remove_dir_all(fixture_dir)?;
        Ok(())
    }

    fn fixture(script: &str) -> anyhow::Result<(PathBuf, ProbeInput)> {
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .join(".artifacts/provider-probe-tests")
            .join(uuid::Uuid::new_v4().to_string());
        crate::config::private_dir(&dir)?;
        let bin = dir.join("provider");
        std::fs::write(&bin, format!("#!/bin/sh\n{script}\n"))?;
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o700))?;
        Ok((
            dir,
            ProbeInput {
                bin,
                env: BTreeMap::new(),
                authentication: None,
                server: None,
                epoch: 0,
                manager: None,
            },
        ))
    }

    #[test]
    fn only_a_readable_claude_status_can_confirm_signed_out() {
        assert_eq!(
            claude_authentication_response(
                br#"{"loggedIn":false,"email":"private@example.invalid"}"#,
                false
            ),
            Some(Authentication::SignInRequired)
        );
        assert_eq!(
            claude_authentication_response(br#"{"loggedIn":true}"#, true),
            Some(Authentication::Authenticated)
        );
        assert_eq!(
            claude_authentication_response(br#"{"loggedIn":true}"#, false),
            None
        );
        assert_eq!(
            claude_authentication_response(br#"{"email":"private@example.invalid"}"#, true),
            None
        );
        assert_eq!(claude_authentication_response(b"not json", false), None);
    }

    #[tokio::test]
    async fn claude_borrowing_yields_to_private_keychain_login_before_each_launch()
    -> anyhow::Result<()> {
        let (node, _, dir) = super::super::triage::tests::node().await?;
        let mut node = std::sync::Arc::try_unwrap(node)
            .map_err(|_| anyhow::anyhow!("fixture runtime is shared"))?;
        let (fixture_dir, input) = fixture(
            r#"
[ "$*" = "auth status --json" ] || exit 9
if [ -n "$CLAUDE_CODE_OAUTH_TOKEN" ]; then
  printf '%s\n' borrowed >> "$PROBE_LOGIN_CALLS"
else
  printf '%s\n' private >> "$PROBE_LOGIN_CALLS"
fi
if [ -f "$PROBE_PRIVATE_LOGIN" ]; then
  [ -z "$CLAUDE_CODE_OAUTH_TOKEN" ] || exit 8
  printf '%s\n' '{"loggedIn":true}'
elif [ -f "$PROBE_UNKNOWN_LOGIN" ]; then
  printf '%s\n' '{unreadable'
  exit 2
elif [ "$CLAUDE_CODE_OAUTH_TOKEN" = 'synthetic-borrowed-access' ]; then
  printf '%s\n' '{"loggedIn":true}'
else
  printf '%s\n' '{"loggedIn":false}'
  exit 1
fi
"#,
        )?;
        let private_login = fixture_dir.join("private-keychain-login");
        let unknown_login = fixture_dir.join("unknown-private-login");
        let calls = fixture_dir.join("calls");
        node.host.home = Some(fixture_dir.join("person"));
        let source = fixture_dir.join("person/.claude/.credentials.json");
        crate::config::private_dir(source.parent().unwrap())?;
        crate::config::private_write(
            &source,
            &serde_json::to_vec(&json!({"claudeAiOauth":{
                "accessToken":"synthetic-borrowed-access",
                "refreshToken":"synthetic-source-refresh",
                "expiresAt":vibeface_protocol::now_ms() + 3_600_000,
            }}))?,
        )?;
        let mut local = node.providers();
        local.bins.insert(Provider::Claude, input.bin.clone());
        local.switches.execution.insert(Provider::Claude);
        local.env.apply(
            Provider::Claude,
            BTreeMap::from([
                (
                    "PROBE_PRIVATE_LOGIN".into(),
                    Some(private_login.display().to_string()),
                ),
                (
                    "PROBE_UNKNOWN_LOGIN".into(),
                    Some(unknown_login.display().to_string()),
                ),
                (
                    "PROBE_LOGIN_CALLS".into(),
                    Some(calls.display().to_string()),
                ),
            ]),
        )?;
        *node.providers.lock().unwrap() = local.clone();
        let env = local.launch_env(Provider::Claude);
        let borrowed = node.prepare_claude_login(&input.bin, env.clone()).await?;
        assert_eq!(
            borrowed["CLAUDE_CODE_OAUTH_TOKEN"],
            "synthetic-borrowed-access"
        );
        assert_eq!(std::fs::read_to_string(&calls)?, "private\nborrowed\n");
        let roots = providers::ProviderRoots::new(&dir);
        let private_file = roots.claude_config.join(".credentials.json");
        assert!(
            !private_file.exists(),
            "borrowing never creates a CLI credential"
        );
        assert_eq!(
            roots.authentication_source(Provider::Claude, &local),
            providers::AuthenticationSource::ImportedCliLogin
        );

        std::fs::write(&private_login, b"private-keychain-fixture")?;
        let private = node.prepare_claude_login(&input.bin, env.clone()).await?;
        assert!(!private.contains_key("CLAUDE_CODE_OAUTH_TOKEN"));
        let observed = node.probe_provider(Provider::Claude).await;
        assert_eq!(observed.authentication, Authentication::Authenticated);
        assert_eq!(
            roots.authentication_source(Provider::Claude, &local),
            providers::AuthenticationSource::PrivateLogin
        );
        assert!(!private_file.exists());

        std::fs::remove_file(&private_login)?;
        std::fs::write(&unknown_login, b"unknown-private-status")?;
        let unknown = node.prepare_claude_login(&input.bin, env.clone()).await?;
        assert!(!unknown.contains_key("CLAUDE_CODE_OAUTH_TOKEN"));
        assert_eq!(
            roots.authentication_source(Provider::Claude, &local),
            providers::AuthenticationSource::PrivateLogin
        );

        std::fs::remove_file(&unknown_login)?;
        crate::config::private_write(&private_file, b"synthetic-private-login")?;
        assert!(node.prepare_claude_login(&input.bin, env).await.is_err());
        assert_eq!(std::fs::read(&private_file)?, b"synthetic-private-login");
        drop(node);
        std::fs::remove_dir_all(dir)?;
        std::fs::remove_dir_all(fixture_dir)?;
        Ok(())
    }

    #[tokio::test]
    async fn claude_probe_uses_only_the_safe_status_command_and_accepts_signed_out_exit()
    -> anyhow::Result<()> {
        let (dir, input) = fixture(
            r#"
[ "$*" = "auth status --json" ] || exit 9
printf '%s\n' '{"loggedIn":false,"email":"private@example.invalid","accessToken":"fixture-secret"}'
exit 1
"#,
        )?;
        assert_eq!(
            claude_authentication(&input, &dir)
                .await
                .map_err(|_| anyhow::anyhow!("fixture failed to spawn"))?,
            Some(Authentication::SignInRequired)
        );
        std::fs::remove_dir_all(dir)?;
        Ok(())
    }

    #[tokio::test]
    async fn claude_probe_keeps_only_a_safe_stderr_category() -> anyhow::Result<()> {
        let (dir, input) = fixture(
            r#"
printf '%s\n' 'Authorization: Bearer fixture-secret at /private/provider' >&2
exit 2
"#,
        )?;
        let issue = claude_authentication(&input, &dir).await.unwrap_err();
        assert_eq!(issue.safe_error.as_deref(), Some("authentication_failed"));
        let serialized = serde_json::to_string(&issue)?;
        assert!(!serialized.contains("fixture-secret"));
        assert!(!serialized.contains("/private/provider"));
        std::fs::remove_dir_all(dir)?;
        Ok(())
    }

    #[tokio::test]
    async fn pi_catalog_does_not_open_a_session_or_send_a_prompt() -> anyhow::Result<()> {
        let (dir, input) = fixture(
            r#"
[ "$*" = "--mode rpc --no-session --no-tools --no-extensions" ] || exit 9
IFS= read -r request
[ "$request" = '{"id":"provider-probe","type":"get_available_models"}' ] || exit 9
printf '%s\n' '{"type":"response","id":"provider-probe","success":true,"data":{"models":[{"provider":"fixture","id":"model","name":"Fixture model","reasoning":true}]}}'
sleep 60
"#,
        )?;
        let catalog = pi_catalog(&input, &dir)
            .await
            .map_err(|_| anyhow::anyhow!("fixture failed to spawn"))?
            .expect("controlled RPC answered");
        assert_eq!(
            catalog,
            ModelCatalog::Available {
                models: vec![ModelChoice {
                    id: "fixture/model".into(),
                    name: "Fixture model".into(),
                    default: false,
                    effort: EffortOptions::Unknown
                }],
                coverage: CatalogCoverage::Complete
            }
        );
        std::fs::remove_dir_all(dir)?;
        Ok(())
    }

    #[test]
    fn pi_models_keep_manual_ids_and_do_not_invent_effort_choices() {
        let catalog = pi_model_catalog(&json!({"models":[{"provider":"fixture","id":"plain","reasoning":false},{"provider":"fixture","id":"bad\nmodel"}]})).unwrap();
        let ModelCatalog::Available { models, coverage } = catalog else {
            panic!("missing catalog")
        };
        assert_eq!(coverage, CatalogCoverage::Complete);
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].id, "fixture/plain");
        assert_eq!(models[0].effort, EffortOptions::ProviderControlled);
        assert!(pi_model_catalog(&json!({})).is_none());
    }
}
