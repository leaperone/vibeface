//! A finite policy read over a dedicated Noise channel; it creates no business session.

use super::client_policy_read::{check_heads_in, signer};
use crate::{crypto::LocalIdentity, runtime::Runtime, store::Store};
use anyhow::{Result, ensure};
use rusqlite::{Transaction, TransactionBehavior};
use serde::{Deserialize, Serialize};
use vibeface_secure::{
    authorization::{
        AUTH_VERSION, FreshnessReceipt, MAX_PROOF_BYTES, ObjectHash, PublicKey, Signed,
        node_execution_policy::{
            NodeExecutionFreshnessRead, NodeExecutionPolicyReadRequest,
            record_node_freshness_receipts, verify_node_execution_freshness_read,
        },
        remote_execution_policy::{
            RemoteExecutionPolicyPage, RemoteExecutionPolicyPageRequest,
            RemoteExecutionPolicySnapshot, RemoteExecutionPolicyWitness,
        },
    },
    signature_verification::ScopedSignatureVerification,
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
    Freshness(Signed<NodeExecutionFreshnessRead>),
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
    Receipts(Vec<Signed<FreshnessReceipt>>),
}

impl Store {
    fn node_execution_freshness_receipts(
        &self,
        identity: &LocalIdentity,
        actual_noise_public_key: &str,
        request: &Signed<NodeExecutionFreshnessRead>,
        now_ms: u64,
    ) -> Result<Vec<Signed<FreshnessReceipt>>> {
        let signatures = ScopedSignatureVerification::new();
        let signer = signer(identity)?;
        let local = PublicKey::of(&signer);
        let tx = Transaction::new_unchecked(&self.connection, TransactionBehavior::Deferred)?;
        let verified = verify_node_execution_freshness_read(
            self.policy.verified(),
            request,
            &local,
            &PublicKey::parse(actual_noise_public_key)?,
            now_ms,
        )?;
        check_heads_in(&tx, &self.policy, verified.target_heads())?;
        self.policy.accepted_journal_snapshot(&tx)?;
        let challenge = verified.challenge();
        let challenge_hash = challenge.hash()?;
        let mut currents: Vec<_> = challenge
            .policies
            .iter()
            .filter(|stamp| verified.allows_head(stamp))
            .cloned()
            .collect();
        currents.sort_by(|left, right| left.domain.cmp(&right.domain));
        let receipts = if currents.is_empty() {
            Vec::new()
        } else {
            let receipt = FreshnessReceipt {
                version: AUTH_VERSION,
                currents,
                challenge_hash,
                issued_at_ms: now_ms,
                expires_at_ms: verified.expires_at_ms(),
            };
            receipt.validate_currents()?;
            vec![Signed::sign(receipt, &signer)?]
        };
        let mut usage = record_node_freshness_receipts(&verified, &receipts, now_ms)?;
        let envelopes = receipts
            .iter()
            .map(ObjectHash::body)
            .collect::<std::result::Result<Vec<_>, _>>()?;
        usage.record_metadata(&("node-execution-freshness-receipts-v2", envelopes))?;
        usage.record_actual_signature_counts(signatures.counts());
        usage.enforce_limits()?;
        tx.commit()?;
        Ok(receipts)
    }

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
            NodeReadRequest::Freshness(request) => {
                NodeReadResponse::Receipts(self.node_execution_freshness_receipts(
                    identity,
                    actual_noise_public_key,
                    &request,
                    now_ms,
                )?)
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
