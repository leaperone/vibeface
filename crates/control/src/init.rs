//! Shared final request for initializing a Node. No CLI or Core dependencies live here.

use crate::provider::Provider;
use serde::{Deserialize, Serialize};
use std::{
    net::{IpAddr, Ipv4Addr, SocketAddr},
    num::{NonZeroU32, NonZeroUsize},
    path::PathBuf,
};

pub const DEFAULT_LISTEN: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 4790);
pub const DEFAULT_LOCAL_LISTEN: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 4791);

fn default_listen() -> SocketAddr {
    DEFAULT_LISTEN
}

fn default_local_listen() -> SocketAddr {
    DEFAULT_LOCAL_LISTEN
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InitOptions {
    pub data_dir: PathBuf,
    #[serde(default = "default_listen")]
    pub listen: SocketAddr,
    #[serde(default = "default_local_listen")]
    pub local_listen: SocketAddr,
    pub public_url: Option<String>,
    #[serde(default)]
    pub tls: bool,
    #[serde(default)]
    pub no_web: bool,
    #[serde(default)]
    pub allowed_origins: Vec<String>,
    #[serde(default)]
    pub roots: Vec<PathBuf>,
    #[serde(default)]
    pub no_local_auto_access: bool,
    #[serde(default)]
    pub allow_insecure_lan: bool,
    #[serde(default)]
    pub relays: Vec<String>,
    /// `(relay URL, token file)`: the token this Node presents to one of `relays`.
    #[serde(default)]
    pub relay_tokens: Vec<(String, PathBuf)>,
    pub relay_listen: Option<SocketAddr>,
    pub relay_url: Option<String>,
    /// File with the token this Node's relay asks of devices outside its personal network.
    pub relay_operator_token: Option<PathBuf>,
    #[serde(default)]
    pub relay_open: bool,
    pub relay_rate: Option<NonZeroU32>,
    pub relay_max_clients: Option<NonZeroUsize>,
    pub relay_max_clients_per_ip: Option<NonZeroUsize>,
    #[serde(default)]
    pub forward_any: bool,
    pub forward_sessions: Option<usize>,
    pub forward_queue: Option<usize>,
    pub forward_rate: Option<NonZeroU32>,
    pub forward_per_ip: Option<NonZeroUsize>,
    #[serde(default)]
    pub trusted_proxies: Vec<IpAddr>,
    pub manager: Option<Provider>,
    pub workspace: Option<PathBuf>,
    pub model: Option<String>,
    #[serde(default)]
    pub bins: Vec<(Provider, PathBuf)>,
    pub codex_socket: Option<PathBuf>,
    #[serde(default)]
    pub no_mesh: bool,
    pub mesh_network: Option<String>,
    /// Replaces the default bootstrap node.
    #[serde(default)]
    pub bootstrap: Vec<String>,
}

impl InitOptions {
    pub fn new(data_dir: PathBuf) -> Self {
        Self {
            data_dir,
            listen: default_listen(),
            local_listen: default_local_listen(),
            public_url: None,
            tls: false,
            no_web: false,
            allowed_origins: Vec::new(),
            roots: Vec::new(),
            no_local_auto_access: false,
            allow_insecure_lan: false,
            relays: Vec::new(),
            relay_tokens: Vec::new(),
            relay_listen: None,
            relay_url: None,
            relay_operator_token: None,
            relay_open: false,
            relay_rate: None,
            relay_max_clients: None,
            relay_max_clients_per_ip: None,
            forward_any: false,
            forward_sessions: None,
            forward_queue: None,
            forward_rate: None,
            forward_per_ip: None,
            trusted_proxies: Vec::new(),
            manager: None,
            workspace: None,
            model: None,
            bins: Vec::new(),
            codex_socket: None,
            no_mesh: false,
            mesh_network: None,
            bootstrap: Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ControlRequest;
    use serde_json::json;

    #[test]
    fn init_defaults_have_the_existing_flat_wire_contract() -> anyhow::Result<()> {
        let request: ControlRequest = serde_json::from_value(json!({
            "command": "init", "options": {"data_dir": "/controlled/node"}
        }))?;
        assert_eq!(
            serde_json::to_value(request)?,
            json!({
                "command": "init", "options": {
                    "data_dir": "/controlled/node",
                    "listen": "127.0.0.1:4790", "local_listen": "127.0.0.1:4791",
                    "public_url": null, "tls": false, "no_web": false,
                    "allowed_origins": [], "roots": [], "no_local_auto_access": false,
                    "allow_insecure_lan": false, "relays": [], "relay_tokens": [],
                    "relay_listen": null, "relay_url": null, "relay_operator_token": null,
                    "relay_open": false, "relay_rate": null, "relay_max_clients": null,
                    "relay_max_clients_per_ip": null, "forward_any": false,
                    "forward_sessions": null, "forward_queue": null, "forward_rate": null,
                    "forward_per_ip": null, "trusted_proxies": [], "manager": null,
                    "workspace": null, "model": null, "bins": [], "codex_socket": null,
                    "no_mesh": false, "mesh_network": null, "bootstrap": []
                }
            })
        );
        let omitted: InitOptions = serde_json::from_value(json!({"data_dir": "/controlled/node"}))?;
        assert_eq!(omitted, InitOptions::new(PathBuf::from("/controlled/node")));
        Ok(())
    }

    #[test]
    fn init_flat_wire_rejects_missing_unknown_and_invalid_values() {
        for options in [
            json!({}),
            json!({"listen": "127.0.0.1:4790"}),
            json!({"data_dir": "/node", "typo": true}),
            json!({"data_dir": "/node", "settings": {}}),
            json!({"data_dir": "/node", "listen": "invalid"}),
            json!({"data_dir": "/node", "no_mesh": "true"}),
            json!({"data_dir": "/node", "manager": "unknown"}),
            json!({"data_dir": "/node", "manager": {"provider": "codex"}}),
            json!({"data_dir": "/node", "bins": [["unknown", "/agent"]]}),
        ] {
            assert!(
                serde_json::from_value::<ControlRequest>(json!({
                    "command": "init", "options": options
                }))
                .is_err(),
                "accepted {options}"
            );
        }
        for field in [
            "relay_rate",
            "relay_max_clients",
            "relay_max_clients_per_ip",
            "forward_rate",
            "forward_per_ip",
        ] {
            for invalid in [json!(0), json!(-1), json!("1")] {
                let mut options = json!({"data_dir": "/node"});
                options[field] = invalid;
                assert!(
                    serde_json::from_value::<ControlRequest>(json!({
                        "command": "init", "options": options
                    }))
                    .is_err()
                );
            }
        }
    }
}
