//! A finite policy read over a dedicated Noise channel; it creates no business session.

use crate::{crypto::LocalIdentity, runtime::Runtime, store::Store};
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use vibeface_secure::authorization::{
    MAX_PROOF_BYTES, PublicKey, Signed,
    node_execution_policy::NodeExecutionPolicyReadRequest,
    remote_execution_policy::{
        RemoteExecutionPolicyPage, RemoteExecutionPolicyPageRequest, RemoteExecutionPolicySnapshot,
        RemoteExecutionPolicyWitness,
    },
};

#[derive(Serialize, Deserialize)]
#[serde(
    tag = "stage",
    content = "request",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub(crate) enum NodeReadRequest {
    Prepare(NodeExecutionPolicyReadRequest),
    Page(RemoteExecutionPolicyPageRequest),
    Finish(RemoteExecutionPolicyPageRequest),
}

#[derive(Serialize, Deserialize)]
#[serde(
    tag = "stage",
    content = "response",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub(crate) enum NodeReadResponse {
    Snapshot(Signed<RemoteExecutionPolicySnapshot>),
    Page(Signed<RemoteExecutionPolicyPage>),
    Witness(Signed<RemoteExecutionPolicyWitness>),
}

impl Store {
    fn read_node_execution_policy(
        &mut self,
        identity: &LocalIdentity,
        actual_noise_public_key: &str,
        request: NodeReadRequest,
        now_ms: u64,
    ) -> Result<NodeReadResponse> {
        // This key is taken only from the completed local Noise handshake. It is
        // never a claimed presenter, a forwarded field or a PeerHub identity.
        PublicKey::parse(actual_noise_public_key)?;
        let request_bytes = serde_json::to_vec(&request)?.len();
        ensure!(
            request_bytes <= MAX_PROOF_BYTES,
            "Node policy read exceeds 64 KiB"
        );
        let response = match request {
            NodeReadRequest::Prepare(request) => {
                let value = self.node_execution_policy_prepare(
                    identity,
                    actual_noise_public_key,
                    request,
                    now_ms,
                )?;
                NodeReadResponse::Snapshot(serde_json::from_value(value["snapshot"].clone())?)
            }
            NodeReadRequest::Page(request) => {
                let value = self.node_execution_policy_page(
                    identity,
                    actual_noise_public_key,
                    request,
                    now_ms,
                )?;
                NodeReadResponse::Page(serde_json::from_value(value["page"].clone())?)
            }
            NodeReadRequest::Finish(request) => {
                let value = self.node_execution_policy_finish(
                    identity,
                    actual_noise_public_key,
                    request,
                    now_ms,
                )?;
                NodeReadResponse::Witness(serde_json::from_value(value["witness"].clone())?)
            }
        };
        ensure!(
            request_bytes + serde_json::to_vec(&response)?.len() <= MAX_PROOF_BYTES,
            "Node policy read input and output exceed 64 KiB"
        );
        Ok(response)
    }
}

impl Runtime {
    pub(crate) async fn node_execution_policy_read(
        &self,
        actual_noise_public_key: &str,
        request: NodeReadRequest,
    ) -> Result<NodeReadResponse> {
        self.store.lock().await.read_node_execution_policy(
            &self.identity,
            actual_noise_public_key,
            request,
            crate::runtime::now_ms() as u64,
        )
    }
}
