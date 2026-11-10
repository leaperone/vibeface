//! Local identity locations. Display hints never grant network membership or authorize a Node.
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashSet,
    fs::File,
    io::ErrorKind,
    path::{Component, Path, PathBuf},
};
use vibeface_bridge::platform::privatefs;
use vibeface_client::Entry;

const REGISTRY_FILE: &str = "native-client-profiles.json";
const PROFILE_DIRECTORY: &str = "native-client-profiles";
const MAX_REGISTRY_BYTES: u64 = 64 * 1024;
pub const MAX_PROFILES: usize = 32;

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct ProfileId(String);

impl ProfileId {
    pub fn parse(value: &str) -> Result<Self> {
        let valid = value == "default"
            || (uuid::Uuid::parse_str(value)
                .is_ok_and(|id| id.get_version_num() == 4 && id.to_string() == value));
        ensure!(valid, "个人网络档案标识无效。");
        Ok(Self(value.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for ProfileId {
    type Error = anyhow::Error;

    fn try_from(value: String) -> Result<Self> {
        Self::parse(&value)
    }
}

impl From<ProfileId> for String {
    fn from(value: ProfileId) -> Self {
        value.0
    }
}

/// Untrusted labels for the picker. Use the unlocked ClientKey and verified directory to connect.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProfileHints {
    pub network: Option<String>,
    pub entry: Option<Entry>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PersonalProfile {
    pub id: ProfileId,
    pub label: String,
    pub hints: ProfileHints,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProfilePaths {
    pub client_dir: PathBuf,
    pub outbox_dir: PathBuf,
    pub draft_dir: PathBuf,
    /// IdentityPlan stores its sealed journal beside client.json.
    pub plan_dir: PathBuf,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RegistryData {
    version: u32,
    selected_id: ProfileId,
    profiles: Vec<PersonalProfile>,
}

/// Own on the backend actor. The held lock prevents two actors from replacing each other's registry.
pub struct ProfileRegistry {
    root: PathBuf,
    data: RegistryData,
    saved: Vec<u8>,
    _lock: File,
}

impl ProfileRegistry {
    /// Registers the existing default location without reading, generating or rewriting its key.
    /// An absent default directory remains absent until an explicit identity action creates it.
    pub fn open(default_client_dir: &Path) -> Result<Self> {
        safe_path(default_client_dir)?;
        ensure!(
            default_client_dir.file_name().is_some_and(|name| name == "native-client"),
            "默认身份目录无效。"
        );
        let parent = default_client_dir.parent().context("默认身份目录缺少上级目录。")?;
        private_directory(parent)?;
        let root = std::fs::canonicalize(parent)?;
        let path = root.join(REGISTRY_FILE);
        let lock_path = root.join("native-client-profiles.lock");
        if metadata_exists(&lock_path)? {
            private_file(&lock_path)?;
        }
        let lock = privatefs::open_or_create(&lock_path)?;
        private_file(&lock_path)?;
        lock.try_lock().context("个人网络档案已由另一客户端打开。")?;

        let (data, saved) = if metadata_exists(&path)? {
            let saved = privatefs::read_private_bounded(
                &path,
                MAX_REGISTRY_BYTES,
                "个人网络档案注册表",
            )?;
            // Hide parse details so arbitrary damaged bytes cannot enter logs or UI errors.
            let data = serde_json::from_slice(&saved)
                .map_err(|_| anyhow::anyhow!("个人网络档案注册表已损坏，原文件已保留。"))?;
            (data, saved)
        } else {
            let id = ProfileId::parse("default")?;
            let data = RegistryData {
                version: 1,
                selected_id: id.clone(),
                profiles: vec![PersonalProfile {
                    id,
                    label: "默认个人网络".into(),
                    hints: ProfileHints::default(),
                }],
            };
            validate_data(&data)?;
            validate_locations(&root, &data)?;
            let saved = serde_json::to_vec(&data)?;
            ensure!(!metadata_exists(&path)?, "个人网络档案注册表已在外部出现，请重新打开。");
            privatefs::write_atomic(&path, &saved)?;
            (data, saved)
        };
        validate_data(&data)?;
        validate_locations(&root, &data)?;
        Ok(Self { root, data, saved, _lock: lock })
    }

    pub fn profiles(&self) -> &[PersonalProfile] {
        &self.data.profiles
    }

    pub fn selected_id(&self) -> &ProfileId {
        &self.data.selected_id
    }

    pub fn selected(&self) -> &PersonalProfile {
        self.data.profiles.iter().find(|profile| profile.id == self.data.selected_id)
            .expect("validated registry selection")
    }

    pub fn paths(&self, id: &ProfileId) -> Result<ProfilePaths> {
        ensure!(self.data.profiles.iter().any(|profile| &profile.id == id), "个人网络档案不存在。");
        validate_locations(&self.root, &self.data)?;
        Ok(profile_paths(&self.root, id))
    }

    /// Keeps the exact existing OS service name, including its original path spelling, for default.
    pub fn credential_scope_from_base(&self, id: &ProfileId, existing_base: &str) -> Result<String> {
        self.paths(id)?;
        let digest = existing_base.strip_prefix("vibeface-native://")
            .context("个人网络档案凭证服务无效。")?;
        ensure!(digest.len() == 64 && digest.bytes().all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)), "个人网络档案凭证服务无效。");
        Ok(if id.as_str() == "default" { existing_base.to_owned() } else { format!("{existing_base}/{}", id.as_str()) })
    }

    /// Call only for an explicit create action. No key, enrollment, connection or Node is created.
    /// Selection stays unchanged until the caller has confirmed and closed its previous client.
    pub fn create(&mut self, label: &str) -> Result<ProfileId> {
        validate_label(label)?;
        ensure!(self.data.profiles.len() < MAX_PROFILES, "个人网络档案已达上限，请保留并核对现有档案。");
        self.check_saved()?;
        let parent = self.root.join(PROFILE_DIRECTORY);
        create_private_directory(&parent)?;
        let id = ProfileId::parse(&uuid::Uuid::new_v4().to_string())?;
        let paths = profile_paths(&self.root, &id);
        privatefs::create_dir_new(&paths.client_dir)?;
        privatefs::sync_dir(&parent)?;
        create_private_directory(&paths.outbox_dir)?;
        create_private_directory(&paths.draft_dir)?;
        let mut next = self.data.clone();
        next.profiles.push(PersonalProfile { id: id.clone(), label: label.into(), hints: ProfileHints::default() });
        // On failure retain the new directory. Never erase identity files or pending operations.
        self.save(next)?;
        Ok(id)
    }

    /// Metadata only. UI must confirm a switch, close the old client and fence asynchronous results.
    /// Unknown writes, draft leases and sealed plans remain at the original profile's paths.
    pub fn select(&mut self, id: &ProfileId) -> Result<()> {
        self.paths(id)?;
        let mut next = self.data.clone();
        next.selected_id = id.clone();
        self.save(next)
    }

    pub fn set_hints(&mut self, id: &ProfileId, hints: ProfileHints) -> Result<()> {
        let mut next = self.data.clone();
        let profile = next.profiles.iter_mut().find(|profile| &profile.id == id)
            .context("个人网络档案不存在。")?;
        profile.hints = hints;
        self.save(next)
    }

    fn check_saved(&self) -> Result<()> {
        private_directory(&self.root)?;
        validate_locations(&self.root, &self.data)?;
        let actual = privatefs::read_private_bounded(&self.root.join(REGISTRY_FILE), MAX_REGISTRY_BYTES, "个人网络档案注册表")?;
        ensure!(actual == self.saved, "个人网络档案注册表已在外部改变，原文件已保留，请重新打开。");
        Ok(())
    }

    fn save(&mut self, next: RegistryData) -> Result<()> {
        validate_data(&next)?;
        validate_locations(&self.root, &next)?;
        self.check_saved()?;
        let bytes = serde_json::to_vec(&next)?;
        ensure!(bytes.len() as u64 <= MAX_REGISTRY_BYTES, "个人网络档案注册表超出范围。");
        privatefs::write_atomic(&self.root.join(REGISTRY_FILE), &bytes)?;
        self.data = next;
        self.saved = bytes;
        Ok(())
    }
}

fn profile_paths(root: &Path, id: &ProfileId) -> ProfilePaths {
    let client_dir = if id.as_str() == "default" {
        root.join("native-client")
    } else {
        root.join(PROFILE_DIRECTORY).join(id.as_str())
    };
    ProfilePaths {
        outbox_dir: client_dir.join("outbox"),
        draft_dir: client_dir.join("input-drafts"),
        plan_dir: client_dir.clone(),
        client_dir,
    }
}

fn validate_data(data: &RegistryData) -> Result<()> {
    ensure!(data.version == 1, "个人网络档案注册表版本无效。");
    ensure!(!data.profiles.is_empty() && data.profiles.len() <= MAX_PROFILES, "个人网络档案数量无效。");
    let mut ids = HashSet::new();
    for profile in &data.profiles {
        ProfileId::parse(profile.id.as_str())?;
        ensure!(ids.insert(&profile.id), "个人网络档案标识重复。");
        validate_label(&profile.label)?;
        if let Some(network) = &profile.hints.network {
            validate_display(network, 128)?;
        }
        if let Some(entry) = &profile.hints.entry {
            validate_display(&entry.origin, 2048)?;
            validate_display(&entry.node_id, 128)?;
        }
    }
    ensure!(data.profiles.iter().any(|profile| profile.id.as_str() == "default"), "默认身份档案缺失，拒绝替换已有身份。");
    ensure!(ids.contains(&data.selected_id), "选中的个人网络档案不存在。");
    Ok(())
}

fn validate_label(value: &str) -> Result<()> {
    validate_display(value, 120)?;
    ensure!(value.trim() == value, "个人网络档案名称首尾不能有空白。");
    Ok(())
}

fn validate_display(value: &str, max: usize) -> Result<()> {
    ensure!(!value.trim().is_empty() && value.len() <= max && !value.chars().any(char::is_control), "个人网络档案显示信息无效。");
    Ok(())
}

fn validate_locations(root: &Path, data: &RegistryData) -> Result<()> {
    private_directory(root)?;
    let parent = root.join(PROFILE_DIRECTORY);
    if metadata_exists(&parent)? {
        private_directory(&parent)?;
    }
    for profile in &data.profiles {
        let paths = profile_paths(root, &profile.id);
        if !metadata_exists(&paths.client_dir)? {
            ensure!(profile.id.as_str() == "default", "个人网络档案目录缺失，拒绝生成替代身份。");
            continue;
        }
        private_directory(&paths.client_dir)?;
        for path in [&paths.outbox_dir, &paths.draft_dir] {
            if metadata_exists(path)? {
                private_directory(path)?;
            }
        }
        for name in ["client.json", "approvals.json", "identity-operation.json", "enrollment.json", "delivery-intents.json"] {
            let path = paths.client_dir.join(name);
            if metadata_exists(&path)? {
                private_file(&path)?;
            }
        }
    }
    Ok(())
}

fn safe_path(path: &Path) -> Result<()> {
    let text = path.to_str().context("个人网络档案路径不是有效的 Unicode。")?;
    ensure!(path.is_absolute() && text.len() <= 4096 && !text.chars().any(char::is_control)
        && !path.components().any(|part| matches!(part, Component::ParentDir)), "个人网络档案路径无效。");
    Ok(())
}

fn metadata_exists(path: &Path) -> Result<bool> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error.into()),
    }
}

fn private_directory(path: &Path) -> Result<()> {
    let metadata = std::fs::symlink_metadata(path)?;
    ensure!(metadata.is_dir() && !metadata.file_type().is_symlink(), "个人网络档案目录必须是普通目录。");
    privatefs::ensure_private(path, "个人网络档案目录")
}

fn private_file(path: &Path) -> Result<()> {
    ensure!(privatefs::is_plain_file(&std::fs::symlink_metadata(path)?), "个人网络档案文件必须是普通文件。");
    privatefs::ensure_private(path, "个人网络档案文件")
}

fn create_private_directory(path: &Path) -> Result<()> {
    if metadata_exists(path)? {
        return private_directory(path);
    }
    privatefs::create_dir_new(path)?;
    if let Some(parent) = path.parent() {
        privatefs::sync_dir(parent)?;
    }
    private_directory(path)
}
