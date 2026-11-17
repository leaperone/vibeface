//! The complete, private Core inspection contract. A successful decode is not a running Node.
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Deserializer, Serialize};
use std::{
    net::SocketAddr,
    path::{Component, Path, PathBuf},
};
use vibeface_bridge::platform::ipc::Endpoint;

/// Unlike serde's default Option decoding, absence is an error; null remains meaningful.
pub(crate) fn required_nullable<'de, D, T>(
    deserializer: D,
) -> std::result::Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer)
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum NodeStatus {
    Running,
    Stopped,
    Unavailable,
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WriteState {
    Aligning,
    ReadOnly,
    Writable,
    Forked,
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PeerPath {
    Direct,
    Relay,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct NodeIdentity {
    pub data_dir: PathBuf,
    pub device_id: String,
}

pub fn validate_path(path: &Path) -> Result<()> {
    let text = path.to_str().context("Node path is not Unicode")?;
    ensure!(
        path.is_absolute()
            && text.len() <= 4096
            && !text.chars().any(char::is_control)
            && !path
                .components()
                .any(|p| matches!(p, Component::ParentDir | Component::CurDir))
            && path.components().collect::<PathBuf>().as_os_str() == path.as_os_str(),
        "Node path must be an absolute normalized path"
    );
    Ok(())
}
pub fn validate_device_id(id: &str) -> Result<()> {
    ensure!(
        id.len() == 64
            && id
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
        "invalid Node identity"
    );
    Ok(())
}
pub(crate) fn validate_version(value: &str) -> Result<()> {
    ensure!(
        !value.is_empty() && value.len() <= 160 && !value.chars().any(char::is_control),
        "invalid program version"
    );
    Ok(())
}
impl NodeIdentity {
    pub fn validate(&self) -> Result<()> {
        validate_path(&self.data_dir)?;
        validate_device_id(&self.device_id)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MeshStatus {
    pub enabled: bool,
    pub network: String,
    #[serde(deserialize_with = "required_nullable")]
    pub records: Option<usize>,
    #[serde(deserialize_with = "required_nullable")]
    pub neighbors: Option<usize>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PeerStatus {
    pub device_id: String,
    pub name: String,
    #[serde(deserialize_with = "required_nullable")]
    pub path: Option<PeerPath>,
    #[serde(deserialize_with = "required_nullable")]
    pub reachable: Option<bool>,
    #[serde(deserialize_with = "required_nullable")]
    pub synced_ms: Option<i64>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Inspection {
    pub status: NodeStatus,
    pub device_id: String,
    pub data_dir: PathBuf,
    pub maintenance_socket: PathBuf,
    pub listen: SocketAddr,
    pub local_listen: SocketAddr,
    pub public_url: String,
    pub web_enabled: bool,
    pub http_enabled: bool,
    #[serde(deserialize_with = "required_nullable")]
    pub version: Option<String>,
    #[serde(deserialize_with = "required_nullable")]
    pub write_state: Option<WriteState>,
    pub mesh: MeshStatus,
    #[serde(deserialize_with = "required_nullable")]
    pub peers: Option<Vec<PeerStatus>>,
    pub core_version: String,
    #[serde(deserialize_with = "required_nullable")]
    pub error: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BindingError {
    PathInvalid,
    DirectoryMismatch,
    DeviceMismatch,
}
impl std::fmt::Display for BindingError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::PathInvalid => "invalid Node identity path",
            Self::DirectoryMismatch => "Core inspected a different Node directory",
            Self::DeviceMismatch => "Node identity changed",
        })
    }
}
impl std::error::Error for BindingError {}

impl Inspection {
    pub fn from_value(value: serde_json::Value) -> Result<Self> {
        let inspected: Self = serde_json::from_value(value).context("invalid Core inspection")?;
        inspected.validate()?;
        Ok(inspected)
    }
    pub fn identity(&self) -> NodeIdentity {
        NodeIdentity {
            data_dir: self.data_dir.clone(),
            device_id: self.device_id.clone(),
        }
    }
    pub fn validate(&self) -> Result<()> {
        validate(self)
    }
    pub fn require_identity(
        &self,
        expected: &NodeIdentity,
    ) -> std::result::Result<(), BindingError> {
        require_binding(self, expected)
    }
}
pub fn require_binding(
    value: &Inspection,
    expected: &NodeIdentity,
) -> std::result::Result<(), BindingError> {
    expected.validate().map_err(|_| BindingError::PathInvalid)?;
    value
        .identity()
        .validate()
        .map_err(|_| BindingError::PathInvalid)?;
    if value.data_dir != expected.data_dir {
        return Err(BindingError::DirectoryMismatch);
    }
    if value.device_id != expected.device_id {
        return Err(BindingError::DeviceMismatch);
    }
    Ok(())
}
pub fn validate(value: &Inspection) -> Result<()> {
    value.identity().validate()?;
    Endpoint::from_path(&value.maintenance_socket)?;
    validate_version(&value.core_version)?;
    let url = reqwest::Url::parse(&value.public_url)?;
    ensure!(
        matches!(url.scheme(), "http" | "https")
            && url.host_str().is_some()
            && url.username().is_empty()
            && url.password().is_none()
            && url.query().is_none()
            && url.fragment().is_none()
            && url.path() == "/",
        "public_url must be an HTTP(S) origin"
    );
    ensure!(
        value.mesh.records.is_some() == value.mesh.neighbors.is_some(),
        "incomplete mesh health"
    );
    match value.status {
        NodeStatus::Running => {
            ensure!(
                value.version.as_deref() == Some(value.core_version.as_str())
                    && value.error.is_none()
                    && value.peers.is_some(),
                "inconsistent running inspection"
            );
        }
        NodeStatus::Stopped | NodeStatus::Unavailable => {
            ensure!(
                value.version.is_none()
                    && value.write_state.is_none()
                    && value.peers.is_none()
                    && !value.http_enabled
                    && value.mesh.records.is_none()
                    && value.mesh.neighbors.is_none()
                    && value.error.as_ref().is_some_and(|e| !e.is_empty()),
                "inconsistent non-running inspection"
            );
        }
    }
    for peer in value.peers.iter().flatten() {
        validate_device_id(&peer.device_id)?;
    }
    Ok(())
}

/// Exactly one child request. Never initializes data or retries a failed identity binding.
pub async fn inspect(core: &Path, data_dir: &Path) -> Result<Inspection> {
    ensure!(
        data_dir.is_absolute(),
        "Node data directory must be absolute"
    );
    ensure!(
        !std::fs::symlink_metadata(data_dir)?
            .file_type()
            .is_symlink(),
        "Node data directory must not be a symlink"
    );
    let data_dir =
        std::fs::canonicalize(data_dir).context("initialized Node directory is required")?;
    validate_path(&data_dir)?;
    let result = crate::request(
        core,
        &crate::ControlRequest::Inspect {
            data_dir: data_dir.clone(),
        },
    )
    .await?;
    let inspected = Inspection::from_value(result)?;
    if inspected.data_dir != data_dir {
        return Err(BindingError::DirectoryMismatch.into());
    }
    Ok(inspected)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::{fs, os::unix::fs::PermissionsExt};

    const STOPPED: &str = r#"{"ok":true,"result":{"status":"stopped","device_id":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","data_dir":DATA,"maintenance_socket":SOCKET,"listen":"127.0.0.1:4790","local_listen":"127.0.0.1:4791","public_url":"http://127.0.0.1:4790","web_enabled":true,"http_enabled":false,"version":null,"write_state":null,"mesh":{"enabled":false,"network":"vibeface","records":null,"neighbors":null},"peers":null,"core_version":"0.0.1-fixture","error":"maintenance socket is missing"}}"#;

    struct Fixture {
        root: PathBuf,
        data: PathBuf,
        core: PathBuf,
    }
    impl Fixture {
        fn new() -> Result<Self> {
            let root = std::env::temp_dir().join(format!("vf-inspect-{}", uuid::Uuid::new_v4()));
            fs::create_dir(&root)?;
            fs::set_permissions(&root, fs::Permissions::from_mode(0o700))?;
            let data = root.join("node");
            fs::create_dir(&data)?;
            let core = root.join("core");
            fs::write(
                &core,
                r#"#!/bin/sh
set -eu
[ "$#" -eq 1 ] && [ "$1" = --control ] || exit 91
fixture_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
cat >> "$fixture_dir/requests"
printf '\n' >> "$fixture_dir/calls"
cat "$fixture_dir/reply"
exit "$(cat "$fixture_dir/exit-code")"
"#,
            )?;
            fs::set_permissions(&core, fs::Permissions::from_mode(0o700))?;
            Ok(Self {
                root,
                data: fs::canonicalize(data)?,
                core,
            })
        }
        fn literal(&self) -> Result<String> {
            let endpoint = Endpoint::in_private_dir(&self.root, "fixture.sock")?;
            Ok(STOPPED
                .replace("DATA", &serde_json::to_string(&self.data)?)
                .replace("SOCKET", &serde_json::to_string(endpoint.path())?))
        }
        async fn run(&self, reply: &str, code: u8) -> Result<Inspection> {
            fs::write(self.root.join("reply"), reply)?;
            fs::write(self.root.join("exit-code"), code.to_string())?;
            let result = inspect(&self.core, &self.data).await;
            assert_eq!(
                fs::read_to_string(self.root.join("calls"))?,
                "\n",
                "one request only"
            );
            assert_eq!(
                fs::read_to_string(self.root.join("requests"))?,
                format!(
                    "{{\"command\":\"inspect\",\"data_dir\":{}}}\n",
                    serde_json::to_string(&self.data)?
                )
            );
            assert!(!self.root.join("mutation").exists());
            result
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    #[tokio::test]
    async fn child_inspection_is_strict_bound_and_never_retried() -> Result<()> {
        let fixture = Fixture::new()?;
        let stopped = fixture.run(&fixture.literal()?, 0).await?;
        assert_eq!(stopped.status, NodeStatus::Stopped);
        assert_eq!(stopped.device_id, "a".repeat(64));
        assert_eq!(stopped.core_version, "0.0.1-fixture");
        assert!(stopped.version.is_none());
        assert_eq!(
            stopped.require_identity(&NodeIdentity {
                data_dir: fixture.data.clone(),
                device_id: "b".repeat(64)
            }),
            Err(BindingError::DeviceMismatch)
        );
        assert!(!fixture.root.join("mutation").exists());
        let running = |wire: String| {
            wire.replace("\"status\":\"stopped\"", "\"status\":\"running\"")
                .replace("\"version\":null", "\"version\":\"0.0.1-fixture\"")
                .replace("\"peers\":null", "\"peers\":[]")
                .replace(
                    "\"error\":\"maintenance socket is missing\"",
                    "\"error\":null",
                )
        };
        let fixture = Fixture::new()?;
        let observed = fixture.run(&running(fixture.literal()?), 0).await?;
        assert_eq!(observed.status, NodeStatus::Running);
        assert_eq!(observed.version.as_deref(), Some("0.0.1-fixture"));
        assert!(observed.peers.as_ref().unwrap().is_empty());
        assert_eq!(observed.listen, "127.0.0.1:4790".parse::<SocketAddr>()?);
        assert_eq!(
            observed.local_listen,
            "127.0.0.1:4791".parse::<SocketAddr>()?
        );
        assert_eq!(observed.public_url, "http://127.0.0.1:4790");
        assert!(observed.web_enabled);
        assert!(!observed.http_enabled);
        assert!(observed.write_state.is_none());
        assert!(!observed.mesh.enabled);
        assert_eq!(observed.mesh.network, "vibeface");
        assert!(observed.mesh.records.is_none() && observed.mesh.neighbors.is_none());
        assert!(observed.error.is_none());
        // Each malformed literal is sent by a new real child, including missing nullable keys.
        for (from, to, is_running) in [
            ("\"write_state\":null,", "", false),
            (",\"error\":\"maintenance socket is missing\"", "", false),
            (",\"neighbors\":null", "", false),
            ("\"status\":", "\"unknown\":true,\"status\":", false),
            (
                "\"peers\":[]",
                "\"peers\":[{\"device_id\":\"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\",\"name\":\"peer\",\"path\":null,\"reachable\":null}]",
                true,
            ),
            (
                "\"peers\":[]",
                "\"peers\":[{\"device_id\":\"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\",\"name\":\"peer\",\"path\":\"unknown\",\"reachable\":null,\"synced_ms\":null}]",
                true,
            ),
            ("\"stopped\"", "\"idle\"", false),
            ("\"version\":null", "\"version\":\"unexpected\"", false),
            ("\"peers\":null", "\"peers\":[]", false),
            ("\"http_enabled\":false", "\"http_enabled\":true", false),
            (
                "\"error\":\"maintenance socket is missing\"",
                "\"error\":null",
                false,
            ),
            ("\"records\":null,", "", false),
            (
                "\"network\":\"vibeface\"",
                "\"network\":\"vibeface\",\"unknown\":true",
                false,
            ),
            ("\"version\":\"0.0.1-fixture\"", "\"version\":null", true),
            ("\"version\":\"0.0.1-fixture\",", "", true),
            (
                "\"version\":\"0.0.1-fixture\"",
                "\"version\":\"other\"",
                true,
            ),
            ("\"peers\":[]", "\"peers\":null", true),
            ("\"peers\":[],", "", true),
            ("\"error\":null", "\"error\":\"failure\"", true),
            ("\"write_state\":null", "\"write_state\":\"unknown\"", true),
        ] {
            let fixture = Fixture::new()?;
            let wire = if is_running {
                running(fixture.literal()?)
            } else {
                fixture.literal()?
            };
            assert!(
                fixture.run(&wire.replace(from, to), 0).await.is_err(),
                "{from} -> {to}"
            );
        }
        for reply in ["{broken", r#"{"ok":true,"result":null}"#, r#"{"ok":true}"#] {
            let fixture = Fixture::new()?;
            assert!(fixture.run(reply, 0).await.is_err());
        }
        let fixture = Fixture::new()?;
        assert!(fixture.run(&fixture.literal()?, 7).await.is_err());
        let fixture = Fixture::new()?;
        let wire = fixture.literal()?.replace(
            &serde_json::to_string(&fixture.data)?,
            &serde_json::to_string(&fixture.root)?,
        );
        let error = fixture.run(&wire, 0).await.unwrap_err();
        assert_eq!(
            error.downcast_ref::<BindingError>(),
            Some(&BindingError::DirectoryMismatch)
        );
        Ok(())
    }
}
