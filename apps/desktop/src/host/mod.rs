//! Native desktop ownership and OS integration. This module never links Core's Runtime.
mod lifecycle;
mod model;
mod service;
mod settings;
pub mod activation;
pub mod platform;
pub mod tray;

pub use model::{Discovery, Inspect, LocalNode, NodeIdentity, NodeSource, NodeStatus, OwnedNode, RuntimeState, UnavailableReason};
pub use service::{BackgroundAction, ManagedNodeStatus, RuntimeUpdate, ServiceState, UpdateAvailability};
pub use settings::{NodeMode, Preferences, Settings, Theme};

use anyhow::{Context, Result, bail, ensure};
use lifecycle::OwnedChild;
#[cfg(test)]
pub(crate) use lifecycle::OwnedChild as TestOwnedChild;
use serde::Serialize;
use std::{path::{Path, PathBuf}, time::Duration};
use vibeface_bridge::platform::privatefs;
use vibeface_control::ControlRequest;

pub(crate) fn prepare_profile(profile: &Path) -> Result<()> {
    model::safe_path(profile)?;
    if profile.exists() { private_directory(profile)?; }
    else { privatefs::create_dir(profile)?; }
    private_directory(profile)
}
fn private_directory(path: &Path) -> Result<()> {
    ensure!(std::fs::symlink_metadata(path)?.is_dir() && !std::fs::symlink_metadata(path)?.file_type().is_symlink(), "节点目录必须是普通目录。");
    privatefs::ensure_private(path, "节点或桌面目录")
}
fn initialized(path: &Path) -> Result<bool> {
    let config = path.join("config.json"); let identity = path.join("identity.json");
    let has_config = config.try_exists()?; let has_identity = identity.try_exists()?;
    ensure!(has_config == has_identity, "节点配置不完整，拒绝初始化或启动。");
    if !has_config { return Ok(false); }
    private_directory(path)?;
    privatefs::read_private_bounded(&config, 1024 * 1024, "节点配置")?;
    privatefs::read_private_bounded(&identity, 64 * 1024, "节点身份")?;
    Ok(true)
}
fn bundle_binary(root: &Path, name: &str) -> PathBuf { root.join(if cfg!(windows) { format!("{name}.exe") } else { name.into() }) }
fn executable(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|metadata| privatefs::is_plain_file(&metadata) && privatefs::is_executable(path))
}
#[derive(Clone, Debug, Serialize)]
pub struct HostSnapshot {
    pub selected: Option<NodeIdentity>,
    pub owned: Option<OwnedNode>,
    pub discovery: Discovery,
    pub background: Option<ManagedNodeStatus>,
    pub runtime: RuntimeState,
    pub node_mode: NodeMode,
    pub preferences: Preferences,
    pub last_owned_exit: Option<i32>,
}

/// Hold this on one background actor or behind an async mutex. No async operation belongs in
/// GPUI's render path. Selecting a Node never transfers process ownership.
pub struct DesktopHost {
    profile_dir: PathBuf,
    runtime_dir: PathBuf,
    attach_only: bool,
    settings: Settings,
    selected: Option<NodeIdentity>,
    owned: Option<OwnedChild>,
    discovery: Discovery,
    background: Option<ManagedNodeStatus>,
    last_owned_exit: Option<i32>,
}
impl DesktopHost {
    pub fn open(profile_dir: PathBuf, runtime_dir: PathBuf, attach_only: bool) -> Result<Self> {
        prepare_profile(&profile_dir)?; model::safe_path(&runtime_dir)?;
        let settings = Settings::load(&profile_dir)?;
        // Persisted selection is a hint. Only a fresh identity/health check publishes it.
        Ok(Self { profile_dir: std::fs::canonicalize(profile_dir)?, runtime_dir, attach_only, settings,
            selected: None, owned: None, discovery: Discovery::default(), background: None, last_owned_exit: None })
    }
    pub fn settings(&self) -> &Settings { &self.settings }
    pub fn client_storage_dir(&self) -> PathBuf { self.profile_dir.join("native-client") }
    pub fn snapshot(&self) -> HostSnapshot {
        let core_missing = !executable(&self.core()); let cli_missing = !executable(&self.cli());
        HostSnapshot { selected: self.selected.clone(), owned: self.owned.as_ref().map(OwnedChild::snapshot), discovery: self.discovery.clone(), background: self.background.clone(),
            runtime: if core_missing || cli_missing { RuntimeState::Missing { core_missing, cli_missing } } else { RuntimeState::Ready },
            node_mode: self.settings.node_mode, preferences: self.settings.preferences.clone(), last_owned_exit: self.last_owned_exit }
    }
    fn core(&self) -> PathBuf { bundle_binary(&self.runtime_dir, "vibeface-core") }
    fn cli(&self) -> PathBuf { bundle_binary(&self.runtime_dir, "vibeface") }
    async fn request(&self, request: &ControlRequest) -> Result<serde_json::Value> {
        let core = self.core(); ensure!(executable(&core), "安装包缺少可运行的 Core。");
        vibeface_control::request(&core, request).await
    }
    pub fn save_preferences(&mut self, preferences: Preferences) -> Result<()> {
        let mut settings = self.settings.clone(); settings.preferences = preferences;
        settings.save(&self.profile_dir)?; self.settings = settings; Ok(())
    }
    pub fn choose_mode(&mut self, mode: NodeMode) -> Result<()> {
        let mut settings = self.settings.clone(); settings.node_mode = mode;
        settings.save(&self.profile_dir)?; self.settings = settings; Ok(())
    }
    pub fn clear_selection(&mut self) -> Result<()> {
        let mut settings = self.settings.clone(); settings.selected_node = None; settings.remembered_data_dirs.clear();
        settings.save(&self.profile_dir)?; self.settings = settings; self.selected = None; Ok(())
    }
    pub async fn inspect(&self, path: &Path) -> Result<Inspect> {
        model::safe_path(path)?; ensure!(initialized(path)?, "节点尚未初始化。");
        let inspected: Inspect = serde_json::from_value(self.request(&ControlRequest::Inspect { data_dir: path.into() }).await?)?;
        inspected.validate()?;
        ensure!(std::fs::canonicalize(path)? == std::fs::canonicalize(&inspected.data_dir)?, "Core 返回了另一个节点目录。");
        Ok(inspected)
    }
    async fn verify(&self, expected: &NodeIdentity, status: Option<NodeStatus>) -> Result<Inspect> {
        expected.validate()?; let inspected = self.inspect(&expected.data_dir).await?;
        ensure!(inspected.device_id == expected.device_id, "节点身份已变化，请重新扫描。");
        if let Some(status) = status { ensure!(inspected.status == status, "节点运行状态已变化，请重新扫描。"); }
        Ok(inspected)
    }
    pub async fn discover(&mut self) -> Result<Discovery> {
        self.poll_owned()?;
        self.selected = None;
        let result: Discovery = serde_json::from_value(self.request(&ControlRequest::DiscoverLocalNodes {
            app_profile_dir: self.profile_dir.clone(), remembered_data_dirs: self.settings.remembered_data_dirs.clone(),
        }).await?)?;
        result.validate()?; self.discovery = result.clone();
        let remembered = self.settings.selected_node.clone();
        self.selected = if let Some(expected) = remembered { self.verify(&expected, Some(NodeStatus::Running)).await.ok().map(|node| node.identity()) } else { None };
        if self.selected.is_none() {
            let mut running = result.nodes.iter().filter(|node| node.status == NodeStatus::Running);
            if let Some(candidate) = running.next() { if running.next().is_none() { self.selected = self.verify(&candidate.identity(), Some(NodeStatus::Running)).await.ok().map(|node| node.identity()); } }
        }
        let mut settings = self.settings.clone(); settings.selected_node = self.selected.clone();
        if settings.node_mode == NodeMode::Undecided && !result.nodes.is_empty() { settings.node_mode = NodeMode::Controller; }
        settings.save(&self.profile_dir)?; self.settings = settings;
        Ok(result)
    }
    pub async fn remember_directory(&mut self, path: PathBuf) -> Result<()> {
        let inspected = self.inspect(&path).await?;
        let mut settings = self.settings.clone(); settings.remembered_data_dirs.retain(|path| path != &inspected.data_dir);
        settings.remembered_data_dirs.push(inspected.data_dir);
        if settings.remembered_data_dirs.len() > 16 { settings.remembered_data_dirs.remove(0); }
        if settings.node_mode == NodeMode::Undecided { settings.node_mode = NodeMode::Controller; }
        settings.save(&self.profile_dir)?; self.settings = settings; Ok(())
    }
    pub async fn select_node(&mut self, node: &NodeIdentity) -> Result<()> {
        let inspected = self.verify(node, Some(NodeStatus::Running)).await?;
        self.remember_directory(inspected.data_dir.clone()).await?;
        let mut settings = self.settings.clone(); settings.selected_node = Some(inspected.identity());
        settings.save(&self.profile_dir)?; self.settings = settings; self.selected = Some(inspected.identity()); Ok(())
    }
    pub async fn resolve_selected(&mut self) -> Result<Option<Inspect>> {
        let Some(expected) = self.selected.clone() else { return Ok(None); };
        match self.verify(&expected, Some(NodeStatus::Running)).await { Ok(inspected) => Ok(Some(inspected)), Err(error) => { self.selected = None; Err(error) } }
    }
    /// Private local credentials remain on the background actor and never enter snapshots.
    pub async fn local_credential(&self, expected: &NodeIdentity) -> Result<vibeface_control::LocalHttpCredential> {
        self.verify(expected, Some(NodeStatus::Running)).await?;
        let credential = vibeface_control::LocalHttpCredential::read(&expected.data_dir)?;
        ensure!(credential.node_id == expected.device_id, "本机会话身份不一致。"); Ok(credential)
    }
    pub fn poll_owned(&mut self) -> Result<bool> {
        if let Some(owned) = &mut self.owned { if let Some(exit) = owned.poll()? { self.last_owned_exit = exit.code(); self.owned = None; return Ok(true); } }
        Ok(false)
    }
    pub async fn start_owned(&mut self) -> Result<Inspect> {
        ensure!(!self.attach_only, "这次会话只连接，不启动本机节点。");
        let service = self.background(BackgroundAction::Status).await?;
        ensure!(service.state == ServiceState::NotInstalled, "本机节点已由后台服务管理，请使用后台节点操作。");
        let path = self.profile_dir.join("node"); self.prepare_owned_node(&path).await?;
        let before = self.inspect(&path).await?; self.start_verified(before.identity()).await
    }
    async fn prepare_owned_node(&self, path: &Path) -> Result<()> {
        ensure!(path == self.profile_dir.join("node"), "只能初始化 App 自有节点目录。");
        if path.exists() { private_directory(path)?; }
        if !initialized(path)? { self.request(&ControlRequest::Init { options: serde_json::json!({"data_dir":path}) }).await?; }
        Ok(())
    }
    pub async fn start_candidate(&mut self, node: &LocalNode) -> Result<Inspect> {
        ensure!(!self.attach_only, "这次会话只连接，不启动本机节点。");
        ensure!(self.discovery.nodes.iter().any(|candidate| candidate.id() == node.id() && candidate.status == NodeStatus::Stopped), "节点候选已过期，请重新扫描。");
        self.verify(&node.identity(), Some(NodeStatus::Stopped)).await?;
        let service = self.background(BackgroundAction::Status).await?;
        if node.sources.contains(&NodeSource::UserService) || service.identity().as_ref() == Some(&node.identity()) {
            ensure!(service.state == ServiceState::InstalledStopped && service.identity().as_ref() == Some(&node.identity()), "用户服务不是当前已验证的后台节点。");
            let started = self.background(BackgroundAction::Start).await?;
            ensure!(started.identity().as_ref() == Some(&node.identity()) && started.core_status == Some(NodeStatus::Running), "后台节点启动身份或健康未确认。");
            let inspected = self.verify(&node.identity(), Some(NodeStatus::Running)).await?; self.select_node(&inspected.identity()).await?; Ok(inspected)
        } else { self.start_verified(node.identity()).await }
    }
    async fn start_verified(&mut self, identity: NodeIdentity) -> Result<Inspect> {
        self.poll_owned()?; ensure!(self.owned.is_none(), "请先停止 App 已启动的节点。");
        ensure!(!identity.data_dir.join("update-pending.json").try_exists()?, "节点更新尚未完成，请先继续后台更新。");
        let before = self.verify(&identity, Some(NodeStatus::Stopped)).await?;
        self.owned = Some(OwnedChild::start(&self.core(), before.identity())?);
        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        loop {
            if self.poll_owned()? { bail!("App 自有节点在启动时退出。"); }
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if let Ok(Ok(inspected)) = tokio::time::timeout(remaining.min(Duration::from_secs(3)), self.verify(&identity, Some(NodeStatus::Running))).await { self.select_node(&inspected.identity()).await?; return Ok(inspected); }
            if tokio::time::Instant::now() >= deadline { self.stop_owned().await?; bail!("节点尚未确认就绪，已停止 App 自有进程。"); }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    }
    pub async fn stop_owned(&mut self) -> Result<()> {
        if let Some(owned) = &mut self.owned { owned.stop(Duration::from_secs(20)).await?; self.owned = None; }
        Ok(())
    }
    pub async fn background(&mut self, action: BackgroundAction) -> Result<ManagedNodeStatus> {
        let cli = self.cli(); ensure!(executable(&cli), "安装包缺少可运行的 CLI。");
        if action != BackgroundAction::Status { ensure!(!self.attach_only, "这次会话只连接，不更改后台服务。"); }
        let fresh = service::invoke(&cli, &self.profile_dir, BackgroundAction::Status, None).await?;
        let previous = self.background.replace(fresh.clone());
        if action == BackgroundAction::Status { return Ok(fresh); }
        let identity = if action == BackgroundAction::Install {
            ensure!(fresh.state == ServiceState::NotInstalled, "后台服务已存在或状态未确认。");
            self.stop_owned().await?;
            let path = self.profile_dir.join("node"); self.prepare_owned_node(&path).await?;
            let inspected = self.inspect(&path).await?;
            ensure!(inspected.status == NodeStatus::Stopped, "安装后台服务前必须确认 App 节点已停止。");
            inspected.identity()
        } else {
            ensure!(fresh.manageable(), "没有可管理的已验证后台节点。");
            let identity = fresh.identity().context("后台节点身份缺失。")?;
            if let Some(previous) = previous { ensure!(previous.identity().as_ref() == Some(&identity), "后台节点身份已变化，请核对后重新操作。"); }
            if fresh.runtime_update.recovery_required && action != BackgroundAction::Stop && action != BackgroundAction::Update { bail!("上次更新尚未完成，请先继续更新。"); }
            if action == BackgroundAction::Update {
                ensure!(fresh.runtime_update.recovery_required || fresh.runtime_update.availability == UpdateAvailability::NewerAvailable, "随包运行时没有可更新的版本。");
                if fresh.runtime_update.recovery_required { ensure!(fresh.service_running == Some(false) && fresh.core_status == Some(NodeStatus::Stopped), "继续更新前请先停止后台节点。"); }
            }
            identity
        };
        let result = service::invoke(&cli, &self.profile_dir, action, Some(&identity.device_id)).await;
        let result = match result { Ok(result) => result, Err(error) => { self.background = service::invoke(&cli, &self.profile_dir, BackgroundAction::Status, None).await.ok(); return Err(error); } };
        self.background = Some(result.clone());
        if action == BackgroundAction::Uninstall { ensure!(result.state == ServiceState::NotInstalled, "后台服务移除尚未确认。"); }
        else {
            ensure!(result.identity().as_ref() == Some(&identity), "后台服务操作后节点身份不一致。");
            if action == BackgroundAction::Update {
                // Recovery may complete even when the OS service registration is absent.
                // Preserve CLI partial state so the user can remove or reinstall explicitly.
                if fresh.runtime_update.recovery_required && result.state == ServiceState::Partial && !result.runtime_update.recovery_required { return Ok(result); }
                let update = &result.runtime_update;
                ensure!(!update.recovery_required && update.availability == UpdateAvailability::Current && update.installed_version.as_ref() == Some(&update.bundled_version), "后台更新版本尚未确认。");
                let expected_running = fresh.service_running == Some(true) && fresh.core_status == Some(NodeStatus::Running);
                ensure!(result.service_running == Some(expected_running) && result.core_status == Some(if expected_running { NodeStatus::Running } else { NodeStatus::Stopped }) && update.running_version.as_ref() == expected_running.then_some(&update.bundled_version), "后台更新运行状态尚未确认。");
            }
        }
        Ok(result)
    }
    /// Attached Nodes and installed user services deliberately have no quit operation here.
    pub async fn shutdown(&mut self) -> Result<()> { self.stop_owned().await }
}
