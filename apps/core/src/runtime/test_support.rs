use super::*;
use crate::{
    identity::{Genesis, Member},
    work::{WorkRecord, WorkSpec, WorkState},
};

#[track_caller]
pub(super) fn assert_work_outcome(record: &WorkRecord, outcome: WorkOutcome) {
    assert_eq!(record.state, WorkState::Finished { outcome });
}

pub(super) fn provider_work(
    provider: Provider,
    target: &str,
    cwd: &Path,
    prompt: &str,
) -> WorkSpec {
    WorkSpec {
        work_id: "w-1".into(),
        conversation: "c".into(),
        trigger: "message:m1".into(),
        target: target.into(),
        dispatcher: target.into(),
        call_ref: "w-1".into(),
        kind: WorkKind::Agent(provider),
        model: None,
        resolved: ResolvedModel {
            provider,
            model: None,
        },
        cwd: cwd.display().to_string(),
        title: "t".into(),
        prompt: prompt.into(),
        continues: None,
        parent: None,
        thread: None,
    }
}

pub(crate) fn initialize_node(
    dir: &Path,
    configuration: &Config,
    identity: &LocalIdentity,
) -> Result<()> {
    config::private_dir(dir)?;
    configuration.save(dir)?;
    config::private_write(&dir.join("data-epoch.json"), &config::data_epoch_marker())?;
    config::private_write(&dir.join("identity.json"), &serde_json::to_vec(identity)?)
}

pub(super) async fn open_node(dir: &Path, configuration: &Config) -> Result<Arc<Runtime>> {
    initialize_node(dir, configuration, &LocalIdentity::generate())?;
    Ok(Arc::new(Runtime::open(dir).await?))
}

pub(crate) fn signed_main_input(
    admin: &LocalIdentity,
    network: &str,
    scope: &str,
    id: &str,
    text: &str,
    attachments: Vec<Attachment>,
    sequence: u64,
) -> Result<EventBody> {
    admin.attest(
        network,
        scope,
        EventBody::InputPosted {
            message_id: id.into(),
            target: crate::workspace::InputTarget::BotMain {
                bot_id: scope.into(),
            },
            intent: crate::workspace::InputIntent::Continue,
            sequence,
            expected_revision: None,
            text: text.into(),
            attachments,
            report_results: Some(true),
            attestation: None,
        },
    )
}

pub(crate) fn manager_settings(
    provider: Provider,
    workspace: PathBuf,
    model: String,
) -> config::ManagerSettings {
    config::ManagerSettings {
        provider,
        workspace,
        model,
        reasoning_effort: None,
        backup: Default::default(),
    }
}

pub(super) async fn join_network(node: &Runtime) -> Result<(LocalIdentity, String)> {
    let admin = LocalIdentity::generate();
    let state = NetworkState::new(Genesis::create(
        &LocalIdentity::generate(),
        Member::client(&admin, "admin", true)?,
        Member::node(&node.identity, "node")?,
    )?)?;
    let network = state.genesis.network_id.clone();
    node.install_membership(state).await?;
    Ok((admin, network))
}

pub(super) type HashPause = (
    tokio::sync::oneshot::Sender<()>,
    std::sync::mpsc::Receiver<()>,
);
pub(super) static HASH_PAUSES: std::sync::LazyLock<std::sync::Mutex<HashMap<PathBuf, HashPause>>> =
    std::sync::LazyLock::new(Default::default);

pub(in crate::runtime) fn pause_attachment_hash(path: &Path) {
    let pause = HASH_PAUSES
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .remove(path);
    if let Some((entered, resume)) = pause {
        let _ = entered.send(());
        let _ = resume.recv();
    }
}

pub(super) async fn bare_node() -> Result<(Arc<Runtime>, PathBuf)> {
    node_at(LocalIdentity::generate(), Default::default()).await
}

pub(super) async fn node_at(
    identity: LocalIdentity,
    network: crate::net::NetworkConfig,
) -> Result<(Arc<Runtime>, PathBuf)> {
    node_with(identity, network, |_| {}).await
}

pub(super) async fn node_with(
    identity: LocalIdentity,
    network: crate::net::NetworkConfig,
    configure: impl FnOnce(&mut Config),
) -> Result<(Arc<Runtime>, PathBuf)> {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../.artifacts")
        .join(format!("triage-{}", uuid::Uuid::new_v4()));
    config::private_dir(&dir)?;
    let mut configuration = Config::local("127.0.0.1:0".parse()?, vec![], false);
    // A unix socket path must stay short; nothing listens unless a test binds it.
    #[cfg(unix)]
    let socket = PathBuf::from(format!(
        "/tmp/vf-{}.sock",
        &uuid::Uuid::new_v4().simple().to_string()[..12]
    ));
    #[cfg(windows)]
    let socket = vibeface_bridge::platform::ipc::Endpoint::in_private_dir(&dir, "fixture.sock")?
        .path()
        .to_owned();
    configuration.providers.codex_app_server.mode = crate::providers::AppServerMode::System;
    configuration.providers.codex_app_server.socket = Some(socket);
    configuration.providers.execution = [crate::providers::Provider::Codex].into();
    configuration.network = network;
    configure(&mut configuration);
    initialize_node(&dir, &configuration, &identity)?;
    Ok((Arc::new(Runtime::open(&dir).await?), dir))
}

pub(crate) async fn node() -> Result<(Arc<Runtime>, LocalIdentity, PathBuf)> {
    let (node, dir) = bare_node().await?;
    let (admin, _) = join_network(&node).await?;
    Ok((node, admin, dir))
}

pub(crate) async fn bot(
    node: &Runtime,
    admin: &LocalIdentity,
    scope: &str,
    name: &str,
    threads: &[&str],
) -> Result<()> {
    instructed_bot(node, admin, scope, name, "", threads).await
}

pub(super) async fn instructed_bot(
    node: &Runtime,
    admin: &LocalIdentity,
    scope: &str,
    name: &str,
    instructions: &str,
    threads: &[&str],
) -> Result<()> {
    let network = node.network_id().await?;
    let local = node.identity.device_id()?;
    let owner = admin.attest(
        &network,
        scope,
        EventBody::ConversationOwner {
            scheduler_node: local.clone(),
            epoch: 1,
            attestation: None,
            cause: None,
        },
    )?;
    let owner = crate::model::FirstOwner {
        scheduler_node: local,
        attestation: owner.attestation().cloned().context("signed owner")?,
    };
    let bot = admin.attest(
        &network,
        scope,
        EventBody::BotCreated {
            bot_id: scope.into(),
            name: name.into(),
            instructions: instructions.into(),
            project: None,
            attestation: None,
            cause: None,
        },
    )?;
    let identity = node.identity.clone();
    let scope = scope.to_owned();
    let name = name.to_owned();
    let threads: Vec<String> = threads.iter().map(|thread| (*thread).to_owned()).collect();
    node.store
        .call("triage.test.instructed_bot", move |store| {
            let scope = scope.as_str();
            let name = name.as_str();
            store.create_conversation(&identity, &network, scope, name, owner, now_ms())?;
            store.append_event(&identity, &network, scope, bot, vec![], now_ms())?;
            for thread in &threads {
                let progress = EventBody::WorkProgress {
                    work_id: format!("w-{thread}"),
                    phase: crate::model::WorkPhase::Running,
                    summary: String::new(),
                    thread_id: Some(thread.clone()),
                    turn_id: None,
                };
                store.append_event(&identity, &network, scope, progress, vec![], now_ms())?;
            }
            Ok(())
        })
        .await
}

pub(super) fn named_socket(node: &Runtime) -> PathBuf {
    node.providers()
        .switches
        .codex_app_server
        .socket
        .expect("a named socket")
}

/// An App Server fixture: `answer` gives each request's reply, `{"result":…}` or `{"error":…}`.
pub(in crate::runtime) async fn scripted_app_server(
    node: &Runtime,
    mut answer: impl FnMut(&Value) -> Value + Send + 'static,
) -> Result<tokio::sync::mpsc::UnboundedReceiver<Value>> {
    use futures_util::{SinkExt as _, StreamExt as _};
    let mut listener = vibeface_bridge::platform::ipc::Listener::bind(
        &vibeface_bridge::platform::ipc::Endpoint::from_path(&named_socket(node))?,
    )
    .await?;
    let (sent, received) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move {
        let Ok(stream) = listener.accept().await else {
            return;
        };
        let Ok(mut server) = tokio_tungstenite::accept_async(stream).await else {
            return;
        };
        while let Some(Ok(message)) = server.next().await {
            let Ok(request) = serde_json::from_slice::<Value>(&message.into_data()) else {
                continue;
            };
            if request.get("id").is_some() {
                let mut reply = match request["method"].as_str() {
                    Some("initialize") => json!({"result":{"userAgent":"codex/fixture"}}),
                    _ => answer(&request),
                };
                reply["id"] = request["id"].clone();
                let _ = server.send(reply.to_string().into()).await;
            }
            let _ = sent.send(request);
        }
    });
    Ok(received)
}

pub(crate) async fn start_reply(
    node: &Runtime,
    admin: &LocalIdentity,
    id: &str,
) -> Result<(String, String)> {
    start_reply_in(node, admin, "c", id).await
}

pub(super) async fn start_reply_in(
    node: &Runtime,
    admin: &LocalIdentity,
    scope: &str,
    id: &str,
) -> Result<(String, String)> {
    let (trigger, attempt) = claim_reply(node, admin, scope, id, "boot").await?;
    {
        let conversation = (scope).to_owned();
        let trigger = (trigger).to_owned();
        let attempt_id = (attempt).to_owned();
        node.store
            .call("triage.test.mark_started", move |store| {
                store.mark_started(&conversation, &trigger, &attempt_id)
            })
            .await
    }?;
    Ok((trigger, attempt))
}

pub(in crate::runtime) async fn claim_reply(
    node: &Runtime,
    admin: &LocalIdentity,
    scope: &str,
    id: &str,
    boot: &str,
) -> Result<(String, String)> {
    let network = node.network_id().await?;
    let identity = node.identity.clone();
    let admin = admin.clone();
    let scope = scope.to_owned();
    let id = id.to_owned();
    let boot = boot.to_owned();
    let generation = node.provider_generation(Provider::Codex, &node.providers())?;
    node.store
        .call("triage.test.claim_reply", move |store| {
            let scope = scope.as_str();
            let id = id.as_str();
            let boot = boot.as_str();
            let sequence = match store.workspace_input(&network, id)? {
                Some(existing) => existing.input.route.sequence,
                None => store.messages(&network, scope)?.len() as u64 + 1,
            };
            store.post_input_fixture(
                &identity,
                &network,
                signed_main_input(&admin, &network, scope, id, id, vec![], sequence)?,
                now_ms(),
            )?;
            let claimed = store
                .claim_next_fixture(&identity, &network, scope, boot, "cfg")?
                .context("job")?;
            let trigger = claimed.job.trigger.clone();
            let attempt = claimed.job.state.attempt().context("attempt")?.id.clone();
            store.record_turn_runtime(
                scope,
                &trigger,
                &attempt,
                &crate::scheduling::TurnRuntime {
                    provider: Provider::Codex,
                    model: Some("fixture".into()),
                    reasoning_effort: None,
                    generation,
                    session_generation: claimed.session_generation,
                    delivery_id: attempt.clone(),
                    stage: vibeface_bridge::failure::ProviderStage::Validate,
                    execution: crate::scheduling::TurnExecution::NotStarted,
                    attempted_at_ms: now_ms(),
                    switched_from: None,
                    switch_reason: None,
                },
            )?;
            Ok((trigger, attempt))
        })
        .await
}

pub(in crate::runtime) fn pi_fixture_log(dir: &Path) -> Result<Vec<Value>> {
    let directory = dir.join("pi-log-records");
    let entries = match std::fs::read_dir(&directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(vec![]),
        Err(error) => {
            return Err(error).with_context(|| {
                format!("read Pi fixture record directory {}", directory.display())
            });
        }
    };
    let mut paths = Vec::new();
    for entry in entries {
        let path = entry
            .with_context(|| format!("list Pi fixture record directory {}", directory.display()))?
            .path();
        if path
            .extension()
            .is_some_and(|extension| extension == "json")
        {
            paths.push(path);
        }
    }
    paths.sort();
    paths
        .into_iter()
        .map(|path| {
            let bytes = std::fs::read(&path)
                .with_context(|| format!("read Pi fixture record {}", path.display()))?;
            let record: Value = serde_json::from_slice(&bytes).with_context(|| {
                format!(
                    "parse complete Pi fixture record {} ({} bytes)",
                    path.display(),
                    bytes.len()
                )
            })?;
            ensure!(
                record.is_object(),
                "Pi fixture record {} must be an object ({} bytes)",
                path.display(),
                bytes.len()
            );
            Ok(record)
        })
        .collect()
}
