use super::{Runtime, codex_server::Link};
use crate::config::ManagerSettings;
use crate::providers::{
    self, AppServer, AppServerMode, Authentication, CatalogCoverage, EffortOptions, LaunchCheck,
    ModelCatalog, ModelChoice, ProbeCode, ProbeIssue, ProbeStage, Provider, ProviderProbe,
    ProviderProbeReadiness,
};
use serde_json::Value;
use std::{collections::BTreeMap, path::PathBuf, process::Stdio, time::Duration};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};

const OUTPUT_LIMIT: u64 = 64 * 1024;
const PROBE_LIMIT: Duration = Duration::from_secs(5);

#[derive(Clone, PartialEq)]
struct ProbeInput {
    bin: PathBuf,
    env: BTreeMap<String, String>,
    server: Option<AppServer>,
    epoch: u64,
    manager: Option<ManagerSettings>,
}

#[derive(serde::Serialize)]
enum LaunchIdentity<'a> {
    Process {
        bin: PathBuf,
        env: &'a BTreeMap<String, String>,
    },
    System {
        socket: Option<&'a std::path::Path>,
    },
}

pub(super) struct CachedProviderProbe {
    input: ProbeInput,
    probe: ProviderProbe,
}

impl Runtime {
    fn probe_input(&self, provider: Provider, local: &providers::Local) -> ProbeInput {
        ProbeInput {
            bin: local.bin(provider),
            env: local.env.get(provider).clone(),
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
        let server = &local.switches.codex_app_server;
        let launch = if provider.on_app_server() && server.mode == AppServerMode::System {
            LaunchIdentity::System {
                socket: server.socket.as_deref(),
            }
        } else {
            LaunchIdentity::Process {
                bin: local.bin(provider),
                env: local.env.get(provider),
            }
        };
        self.identity
            .provider_fingerprint(&serde_json::to_vec(&(provider, launch))?)
    }

    pub(crate) async fn provider_probe_snapshot(&self, provider: Provider) -> ProviderProbe {
        let local = self.providers();
        let mut probe = self.cached_probe(provider, &local);
        if let Some(issue) = prerequisites(provider, &local) {
            probe.launch = LaunchCheck::Failed { issue };
            return probe;
        }
        if self
            .pending_provider_launches
            .load(std::sync::atomic::Ordering::SeqCst)
            && !matches!(probe.launch, LaunchCheck::Failed { .. })
        {
            probe.launch = LaunchCheck::Starting;
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
        let local = self.providers();
        let input = self.probe_input(provider, &local);
        let mut probe = self.provider_probe_snapshot(provider).await;
        probe.authentication = Authentication::Unverified;
        probe.catalog = ModelCatalog::default();
        if !provider.on_app_server()
            && prerequisites(provider, &local).is_none()
            && matches!(probe.launch, LaunchCheck::Failed { .. })
        {
            probe.launch = LaunchCheck::Unchecked;
        }
        if provider.on_app_server() && matches!(probe.launch, LaunchCheck::Launchable) {
            let (authentication, catalog) = self.codex.provider_observation().await;
            probe.authentication = authentication;
            probe.catalog = catalog;
        } else if matches!(
            probe.launch,
            LaunchCheck::Unchecked | LaunchCheck::Launchable
        ) {
            probe.launch = LaunchCheck::Unchecked;
            match provider {
                Provider::Claude => match claude_authentication(&input, &self.data_dir).await {
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

struct ProbeChild {
    child: Child,
    group: i32,
}

impl Drop for ProbeChild {
    fn drop(&mut self) {
        // A cancelled probe must also stop helpers the CLI started.
        unsafe {
            libc::kill(-self.group, libc::SIGKILL);
        }
    }
}

fn spawn_probe(
    input: &ProbeInput,
    fallback: &std::path::Path,
    args: &[&str],
) -> Result<ProbeChild, ProbeIssue> {
    let mut command = Command::new(&input.bin);
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
        .stderr(Stdio::null())
        .process_group(0)
        .kill_on_drop(true);
    let child = command.spawn().map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            ProbeIssue::new(ProbeStage::Install, ProbeCode::NotInstalled)
        } else {
            ProbeIssue::new(ProbeStage::Launch, ProbeCode::LaunchFailed)
        }
    })?;
    let group = child
        .id()
        .and_then(|id| i32::try_from(id).ok())
        .filter(|id| *id > 1)
        .ok_or_else(|| ProbeIssue::new(ProbeStage::Launch, ProbeCode::LaunchFailed))?;
    Ok(ProbeChild { child, group })
}

async fn claude_authentication(
    input: &ProbeInput,
    fallback: &std::path::Path,
) -> Result<Option<Authentication>, ProbeIssue> {
    let mut process = spawn_probe(input, fallback, &["auth", "status", "--json"])?;
    process.child.stdin.take();
    let Some(stdout) = process.child.stdout.take() else {
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
    Ok(status.and_then(|status| claude_authentication_response(&bytes, status.success())))
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
    let (Some(mut stdin), Some(stdout)) = (process.child.stdin.take(), process.child.stdout.take())
    else {
        return Ok(None);
    };
    let stdout = stdout.take(OUTPUT_LIMIT + 1);
    let mut stdout = BufReader::new(stdout);
    Ok(tokio::time::timeout(PROBE_LIMIT, async {
        stdin
            .write_all(b"{\"id\":\"provider-probe\",\"type\":\"get_available_models\"}\n")
            .await
            .ok()?;
        stdin.flush().await.ok()?;
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
    .flatten())
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

fn safe_model_text(value: &str) -> bool {
    !value.trim().is_empty()
        && value.len() <= 256
        && !value.chars().any(char::is_control)
        && !crate::model::looks_secret(value)
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
    if !crate::discovery::runnable(&bin, std::env::var_os("PATH").as_ref()) {
        return Some(ProbeIssue::new(
            ProbeStage::Install,
            ProbeCode::NotInstalled,
        ));
    }
    let configured = local.env.get(provider);
    let Ok(required) = provider.key_variables(configured) else {
        return Some(ProbeIssue::new(
            ProbeStage::Launch,
            ProbeCode::ConfigInvalid,
        ));
    };
    if required.iter().any(|name| {
        !configured.contains_key(name)
            && std::env::var_os(name).is_none_or(|value| value.is_empty())
    }) {
        return Some(ProbeIssue::new(
            ProbeStage::Authentication,
            ProbeCode::MissingCredential,
        ));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::os::unix::fs::PermissionsExt;

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
