use anyhow::{Context, Result, ensure};
use futures_util::{StreamExt, stream::BoxStream};
use iroh::{
    EndpointId,
    address_lookup::{AddressLookup, Error, Item},
};
use iroh_dns::{endpoint_info::EndpointInfo, pkarr::SignedPacket};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DiscoveryConfig {
    #[serde(default)]
    pub publish: bool,
    #[serde(default)]
    pub resolve: bool,
    #[serde(default)]
    pub urls: Vec<String>,
    #[serde(default)]
    pub publish_direct: bool,
}
impl DiscoveryConfig {
    pub fn validate(&self) -> Result<()> {
        ensure!(self.urls.len() <= 4, "too many discovery endpoints");
        ensure!(
            !(self.publish || self.resolve) || !self.urls.is_empty(),
            "discovery requires an explicit endpoint"
        );
        for value in &self.urls {
            let url = reqwest::Url::parse(value)?;
            let loopback = url.host_str().is_some_and(|host| {
                host.trim_matches(['[', ']'])
                    .parse::<std::net::IpAddr>()
                    .is_ok_and(|ip| ip.is_loopback())
            });
            ensure!(
                (url.scheme() == "https" || url.scheme() == "http" && loopback)
                    && url.username().is_empty()
                    && url.password().is_none()
                    && url.host_str().is_some()
                    && url.query().is_none()
                    && url.fragment().is_none(),
                "discovery requires HTTPS or literal loopback HTTP without credentials"
            );
        }
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub struct FreshResolver {
    url: reqwest::Url,
    client: reqwest::Client,
    slots: Arc<tokio::sync::Semaphore>,
}
impl FreshResolver {
    pub fn new(url: reqwest::Url) -> Result<Self> {
        Ok(Self {
            url,
            client: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .timeout(std::time::Duration::from_secs(10))
                .build()?,
            slots: Arc::new(tokio::sync::Semaphore::new(16)),
        })
    }
    async fn lookup(&self, endpoint: EndpointId) -> Result<Item> {
        let _slot = self
            .slots
            .try_acquire()
            .context("discovery concurrency limit reached")?;
        let mut url = self.url.clone();
        url.path_segments_mut()
            .map_err(|_| anyhow::anyhow!("invalid discovery URL"))?
            .pop_if_empty()
            .push(&endpoint.to_z32());
        let response = self.client.get(url).send().await?.error_for_status()?;
        let limit = SignedPacket::MAX_BYTES - 32;
        ensure!(
            response.content_length().is_none_or(|n| n <= limit as u64),
            "discovery packet too large"
        );
        let mut body = Vec::new();
        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk?;
            ensure!(
                body.len() + chunk.len() <= limit,
                "discovery packet too large"
            );
            body.extend_from_slice(&chunk);
        }
        let packet = SignedPacket::from_relay_payload(&endpoint, &body)?;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_micros() as u64;
        check_timestamp(packet.timestamp().as_micros(), now)?;
        let info = EndpointInfo::from_pkarr_signed_packet(&packet)?;
        Ok(Item::new(info, "vibeface-pkarr", None))
    }
}
fn check_timestamp(timestamp: u64, now: u64) -> Result<()> {
    ensure!(
        timestamp <= now.saturating_add(120_000_000)
            && timestamp >= now.saturating_sub(900_000_000),
        "discovery packet is stale or dated in the future"
    );
    Ok(())
}
impl AddressLookup for FreshResolver {
    fn resolve(
        &self,
        endpoint: EndpointId,
    ) -> Option<BoxStream<'static, std::result::Result<Item, Error>>> {
        let resolver = self.clone();
        Some(Box::pin(futures_util::stream::once(async move {
            resolver.lookup(endpoint).await.map_err(|error| {
                Error::from_err("vibeface-pkarr", std::io::Error::other(error.to_string()))
            })
        })))
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn signed_timestamp_has_separate_freshness_and_future_limits() {
        let now = 2_000_000_000;
        assert!(check_timestamp(now, now).is_ok());
        assert!(check_timestamp(now - 900_000_000, now).is_ok());
        assert!(check_timestamp(now - 900_000_001, now).is_err());
        assert!(check_timestamp(now + 120_000_001, now).is_err());
        let mut config = DiscoveryConfig {
            resolve: true,
            urls: vec!["http://127.0.0.1:8787/pkarr".into()],
            ..Default::default()
        };
        assert!(config.validate().is_ok());
        config.urls = vec!["http://remote.example/pkarr".into()];
        assert!(config.validate().is_err());
    }
}
