use super::{
    CALL_TIMEOUT, Client, GATEWAY_FORWARD_METHOD, LocalIdentity, NetworkHub, Request, Service,
    UniqueJsonKeys, receive_value, require_node_policy_carrier, secure, send_frame,
};
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::Value;
use vibeface_secure::authorization::{
    PublicKey, Signed,
    gateway_forward::{
        GatewayForwardHeader, GatewayOriginal, GatewayOutcome, GatewayReplyHandle,
        GatewayReplyHeader, verify_gateway_reply,
    },
};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub(crate) struct GatewayForwardReply {
    pub forward: Signed<GatewayForwardHeader>,
    pub reply: Signed<GatewayReplyHeader>,
    pub outcome: GatewayOutcome,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct GatewayForwardRequest {
    pub header: Signed<GatewayForwardHeader>,
    pub original: GatewayOriginal,
}

pub(crate) struct GatewayForwardClient {
    client: Client,
    gateway: PublicKey,
    target: PublicKey,
    failed: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct GatewayWireReply {
    id: String,
    result: Value,
    error: Option<String>,
}

impl GatewayForwardClient {
    pub(crate) async fn connect(
        network: &NetworkHub,
        identity: &LocalIdentity,
        target: &PublicKey,
    ) -> Result<Self> {
        Ok(Self {
            client: connect(network, identity, target).await?,
            gateway: PublicKey::parse(&identity.public_key_hex()?)?,
            target: target.clone(),
            failed: false,
        })
    }

    pub(crate) async fn forward(
        &mut self,
        header: &Signed<GatewayForwardHeader>,
        original: &GatewayOriginal,
    ) -> Result<GatewayForwardReply> {
        ensure!(
            !self.failed,
            "gateway connection must be reopened after an unknown outcome"
        );
        ensure!(
            header.issuer == self.gateway
                && header.payload.gateway_key == self.gateway
                && header.payload.target_key == self.target,
            "gateway forward differs from its actual Noise endpoints"
        );
        let handle = GatewayReplyHandle::new(
            original.clone(),
            header.payload.source.clone(),
            header.payload.route_approval_hash.clone(),
            self.gateway.clone(),
            self.target.clone(),
            &header.payload.client_key,
            crate::runtime::now_ms() as u64,
        )?;
        self.failed = true;
        let result: GatewayForwardReply = exchange(
            &mut self.client,
            GATEWAY_FORWARD_METHOD,
            &GatewayForwardRequest {
                header: header.clone(),
                original: original.clone(),
            },
        )
        .await?;
        ensure!(
            result.forward == *header,
            "gateway response changed its exact forward header"
        );
        verify_gateway_reply(
            &handle,
            &result.forward,
            &result.reply,
            &result.outcome,
            crate::runtime::now_ms() as u64,
        )?;
        self.failed = false;
        Ok(result)
    }
}

async fn connect(
    network: &NetworkHub,
    identity: &LocalIdentity,
    target: &PublicKey,
) -> Result<Client> {
    let target_node_id = crate::crypto::fingerprint(target.as_str())?;
    let socket = network.dial(&target_node_id, Service::Client).await?;
    let (client, peer) =
        Client::from_socket_with_policy_peer(socket, identity, &target_node_id).await?;
    ensure!(peer.public_key() == target, "gateway Noise target changed");
    Ok(client)
}

async fn exchange<T: DeserializeOwned>(
    client: &mut Client,
    method: &str,
    params: &impl Serialize,
) -> Result<T> {
    let id = uuid::Uuid::new_v4().to_string();
    let wire = serde_json::to_vec(&Request {
        id: id.clone(),
        method: method.to_owned(),
        params: serde_json::to_value(params)?,
        invocation: None,
    })?;
    require_node_policy_carrier(wire.len(), 0)?;
    tokio::time::timeout(CALL_TIMEOUT, async {
        for record in secure(client.channel.seal_message(&wire))? {
            send_frame(&mut client.socket, record).await?;
        }
        let bytes = receive_value(&mut client.socket, &mut client.channel).await?;
        require_node_policy_carrier(wire.len(), bytes.len())?;
        decode_reply(&bytes, &id)
    })
    .await
    .context("gateway response timed out; outcome may be unknown, reconcile before retrying")?
}

fn decode_reply<T: DeserializeOwned>(bytes: &[u8], id: &str) -> Result<T> {
    serde_json::from_slice::<UniqueJsonKeys>(bytes)?;
    let reply: GatewayWireReply = serde_json::from_slice(bytes)?;
    ensure!(reply.id == id, "gateway response changed its request ID");
    if let Some(error) = reply.error {
        ensure!(
            reply.result.is_null(),
            "gateway response contains both result and error"
        );
        bail!("{error}");
    }
    Ok(serde_json::from_value(reply.result)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn gateway_replies_bind_id_and_reject_ambiguous_envelopes() -> Result<()> {
        assert_eq!(
            decode_reply::<Value>(br#"{"id":"request","result":{"ok":true}}"#, "request")?,
            json!({"ok":true})
        );
        assert!(decode_reply::<Value>(br#"{"id":"other","result":{}}"#, "request").is_err());
        assert!(
            decode_reply::<Value>(br#"{"id":"request","result":{},"unknown":true}"#, "request")
                .is_err()
        );
        assert!(
            decode_reply::<Value>(
                br#"{"id":"request","result":{},"error":"denied"}"#,
                "request"
            )
            .is_err()
        );
        assert!(
            decode_reply::<Value>(
                br#"{"id":"request","result":{"nested":{"x":1,"x":2}}}"#,
                "request"
            )
            .is_err()
        );
        assert!(
            decode_reply::<Value>(br#"{"id":"request","result":{},"result":{}}"#, "request")
                .is_err()
        );
        assert!(decode_reply::<Value>(br#"{"id":"request"}"#, "request").is_err());
        assert_eq!(
            decode_reply::<Value>(br#"{"id":"request","result":null}"#, "request")?,
            Value::Null
        );
        Ok(())
    }
}
