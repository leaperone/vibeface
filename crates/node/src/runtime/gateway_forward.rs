use super::{Runtime, now_ms};
use crate::client::GatewayForwardClient;
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::Arc;
use vibeface_secure::authorization::{
    Nonce, ObjectHash, Signed,
    client_policy_import::{
        ClientPolicyAuthConfirmRequest, ClientPolicyAuthConfirmation, ClientPolicyPage,
        ClientPolicyReadRequest, ClientPolicySnapshot, ClientPolicyWitness,
    },
    gateway_forward::{GatewayOriginal, GatewayRouteApproval},
    node_execution_policy::ActualPolicyNoisePeer,
};

#[derive(Serialize, Deserialize)]
#[serde(tag = "stage", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum GatewaySourceRequest {
    Route {
        approval: Signed<GatewayRouteApproval>,
    },
    RouteStatus {
        approval: Signed<GatewayRouteApproval>,
    },
    Begin {
        route_id: Nonce,
        request: Signed<ClientPolicyReadRequest>,
        snapshot: Signed<ClientPolicySnapshot>,
    },
    Page {
        reference: ObjectHash,
        page: Signed<ClientPolicyPage>,
    },
    Finish {
        reference: ObjectHash,
        witness: Signed<ClientPolicyWitness>,
        confirm_request: Signed<ClientPolicyAuthConfirmRequest>,
        confirmation: Signed<ClientPolicyAuthConfirmation>,
    },
    Forward {
        reference: ObjectHash,
        original: GatewayOriginal,
    },
}

impl Runtime {
    pub(crate) async fn gateway_source_request(
        self: &Arc<Self>,
        actual: &ActualPolicyNoisePeer,
        request: GatewaySourceRequest,
    ) -> Result<Value> {
        match request {
            GatewaySourceRequest::Route { approval } => {
                let committed = self.store.lock().await.install_gateway_route(
                    &self.identity,
                    &approval,
                    now_ms() as u64,
                )?;
                Ok(serde_json::to_value(committed)?)
            }
            GatewaySourceRequest::RouteStatus { approval } => {
                let status = self.store.lock().await.gateway_route_status(
                    &self.identity,
                    actual.public_key(),
                    &approval,
                    now_ms() as u64,
                )?;
                Ok(serde_json::to_value(status)?)
            }
            GatewaySourceRequest::Begin {
                route_id,
                request,
                snapshot,
            } => {
                let progress = self.store.lock().await.begin_gateway_source(
                    &self.identity,
                    actual.public_key(),
                    &route_id,
                    request,
                    snapshot,
                    now_ms() as u64,
                )?;
                Ok(serde_json::to_value(progress)?)
            }
            GatewaySourceRequest::Page { reference, page } => {
                let progress = self.store.lock().await.append_gateway_source(
                    &self.identity,
                    actual.public_key(),
                    &reference,
                    page,
                    now_ms() as u64,
                )?;
                Ok(serde_json::to_value(progress)?)
            }
            GatewaySourceRequest::Finish {
                reference,
                witness,
                confirm_request,
                confirmation,
            } => {
                let progress = self.store.lock().await.finish_gateway_source(
                    &self.identity,
                    actual.public_key(),
                    &reference,
                    witness,
                    confirm_request,
                    confirmation,
                    now_ms() as u64,
                )?;
                Ok(serde_json::to_value(progress)?)
            }
            GatewaySourceRequest::Forward {
                reference,
                original,
            } => {
                let forward = self.store.lock().await.prepare_gateway_forward(
                    &self.identity,
                    actual.public_key(),
                    &reference,
                    &original,
                    now_ms() as u64,
                )?;
                let mut client = GatewayForwardClient::connect(
                    &self.network,
                    &self.identity,
                    &forward.target_public_key,
                )
                .await?;
                let current = self.store.lock().await.prepare_gateway_forward(
                    &self.identity,
                    actual.public_key(),
                    &reference,
                    &original,
                    now_ms() as u64,
                )?;
                ensure!(
                    current.target_public_key == forward.target_public_key,
                    "gateway target changed while connecting"
                );
                let reply = client.forward(&current.header, &original).await?;
                Ok(serde_json::to_value(reply)?)
            }
        }
    }
}
