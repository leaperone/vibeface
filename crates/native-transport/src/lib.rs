//! Outbound transport for native Vibeface clients.

use iroh::{
    Endpoint, RelayMode,
    endpoint::{BindError, PortmapperConfig, presets},
};

/// Bind a dial-only LAN endpoint on the caller's runtime.
pub async fn bind_lan_endpoint() -> Result<Endpoint, BindError> {
    Endpoint::builder(presets::Minimal)
        .relay_mode(RelayMode::Disabled)
        .portmapper_config(PortmapperConfig::Disabled)
        .bind()
        .await
}
