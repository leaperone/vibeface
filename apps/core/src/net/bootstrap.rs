//! Public bootstrap: which Discovery a Node registers with by default. The official one is the
//! public node `config/public-bootstrap.json` pins by its device ID.
//!
//! Discovery only says where to connect. Membership and task authority stay with the personal
//! network's signed history and the end-to-end identities on top of the relay.
use anyhow::{Context as _, Result, ensure};
use serde::{Deserialize, Serialize};

const PUBLIC_DEFAULTS: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../config/public-bootstrap.json"
));

/// The official service as `config/public-bootstrap.json` states it.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PublicDefaults {
    pub issuer: String,
    pub udp_host: String,
    pub udp_port: u16,
}

impl PublicDefaults {
    pub fn load() -> Result<Self> {
        let defaults: Self =
            serde_json::from_str(PUBLIC_DEFAULTS).context("invalid public bootstrap defaults")?;
        vibeface_secure::validate_device_id(&defaults.issuer)?;
        ensure!(
            defaults.udp_port != 0 && !defaults.udp_host.is_empty(),
            "invalid public discovery address"
        );
        Ok(defaults)
    }

    /// The `vibeface://<issuer>@<host>:<port>` form the UDP discovery protocol takes.
    pub fn discovery_url(&self) -> String {
        let host = if self.udp_host.contains(':') {
            format!("[{}]", self.udp_host)
        } else {
            self.udp_host.clone()
        };
        format!("vibeface://{}@{host}:{}", self.issuer, self.udp_port)
    }
}

/// Where a Node bootstraps from. `Off` is a product choice, never a fallback.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "kebab-case", deny_unknown_fields)]
pub enum Bootstrap {
    /// No public infrastructure is contacted.
    #[default]
    Off,
    /// The official public node from `config/public-bootstrap.json`.
    Public,
    /// A self-hosted Discovery pinned by the device ID in its URL; it never falls back to the
    /// official one.
    Custom { discovery: String },
}

impl Bootstrap {
    pub fn is_off(&self) -> bool {
        matches!(self, Self::Off)
    }

    pub fn label(&self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Public => "public",
            Self::Custom { .. } => "custom",
        }
    }

    pub fn validate(&self) -> Result<()> {
        self.discovery().map(|_| ())
    }

    /// The Discovery this bootstrap registers with; `None` when off.
    pub fn discovery(&self) -> Result<Option<String>> {
        match self {
            Self::Off => Ok(None),
            Self::Public => Ok(Some(PublicDefaults::load()?.discovery_url())),
            Self::Custom { discovery } => {
                super::discovery::validate_server(discovery)?;
                Ok(Some(discovery.clone()))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{crypto::LocalIdentity, net::device_id};

    #[test]
    fn public_defaults_come_from_the_shared_file() -> Result<()> {
        let file: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../config/public-bootstrap.json"),
        )?)?;
        let defaults = PublicDefaults::load()?;
        assert_eq!(defaults.issuer, file["issuer"].as_str().unwrap());
        let discovery = Bootstrap::Public.discovery()?.unwrap();
        let server = super::super::discovery::Server::parse(&discovery)?;
        assert_eq!(device_id(&server.id), defaults.issuer);
        assert_eq!(server.host, file["udp_host"].as_str().unwrap());
        assert_eq!(u64::from(server.port), file["udp_port"].as_u64().unwrap());
        assert!(Bootstrap::Off.discovery()?.is_none());
        assert_eq!(
            serde_json::to_value(Bootstrap::default())?,
            serde_json::json!({"mode":"off"})
        );
        Ok(())
    }

    #[test]
    fn custom_sources_pin_the_discovery_identity() -> Result<()> {
        let id = LocalIdentity::generate().device_id()?;
        let discovery = format!("vibeface://{id}@disc.example:4792");
        let custom = Bootstrap::Custom {
            discovery: discovery.clone(),
        };
        assert_eq!(custom.discovery()?, Some(discovery.clone()));
        for bad in [
            "https://disc.example",
            "vibeface://not-a-device@disc.example:4792",
        ] {
            let bad = Bootstrap::Custom {
                discovery: bad.into(),
            };
            assert!(bad.validate().is_err(), "{bad:?}");
        }
        assert!(
            serde_json::from_value::<Bootstrap>(serde_json::json!({
                "mode": "custom",
                "discovery": discovery,
                "https": "https://disc.example/bootstrap",
            }))
            .is_err()
        );
        Ok(())
    }
}
