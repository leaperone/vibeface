use super::{
    bounded_recovery_context::{completed_recovery_read_in, recovery_read_reference_in},
    journal,
    policy::project_policy_in,
};
use crate::{crypto::LocalIdentity, store::Store};
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use vibeface_secure::{
    Identity,
    authorization::{
        AUTH_VERSION, JournalEntry, Nonce, ObjectHash, PublicKey, Signature, Signed,
        bootstrap::recovery::{
            context::RecoveryContextRequest,
            stream::{
                self, RecoveryCompletionReceipt, RecoveryCompletionStatusRequest,
                RecoveryProofUsage, RecoveryReadRequest, StreamedRecoveryCommitRequest,
                StreamedRecoveryNodeRequest,
            },
        },
    },
    signature_verification::ScopedSignatureVerification,
};

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ApprovedRecoveryPossession {
    request: StreamedRecoveryNodeRequest,
    proof: Signature,
}

fn metadata(connection: &Connection, key: &str) -> Result<Option<String>> {
    let encoded: Option<(usize, Option<String>)> = connection
        .query_row(
            "SELECT length(CAST(value AS BLOB)),CASE WHEN length(CAST(value AS BLOB))<=65536 THEN value END FROM identity_meta WHERE key=?",
            [key],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    let Some((length, encoded)) = encoded else {
        return Ok(None);
    };
    ensure!(length <= 65_536, "stored recovery record is too large");
    Ok(Some(encoded.context("stored recovery record is missing")?))
}

fn possession_key(snapshot_id: &Nonce) -> String {
    format!("recovery-possession:{snapshot_id}")
}

fn completion_key(snapshot_id: &Nonce) -> String {
    format!("recovery-completed:{snapshot_id}")
}

fn bounded(value: Value) -> Result<Value> {
    ensure!(
        serde_json::to_vec(&value)?.len() <= 65_536,
        "recovery carrier exceeds 64 KiB"
    );
    Ok(value)
}

fn carrier_bytes(request: &Value, response: &Value) -> Result<usize> {
    Ok(serde_json::to_vec(request)?
        .len()
        .max(serde_json::to_vec(response)?.len()))
}

pub(crate) fn recovery_completion_in(
    connection: &Connection,
    local: &PublicKey,
    principal: &str,
    request: &RecoveryCompletionStatusRequest,
) -> Result<Option<Signed<RecoveryCompletionReceipt>>> {
    let Some(encoded) = metadata(connection, &completion_key(&request.context.snapshot_id))? else {
        return Ok(None);
    };
    let receipt: Signed<RecoveryCompletionReceipt> = serde_json::from_str(&encoded)?;
    ensure!(
        receipt.issuer == *local
            && receipt.payload.version == AUTH_VERSION
            && receipt.payload.context == request.context
            && receipt.payload.input_hash == request.input_hash
            && receipt.payload.new_device_public_key.as_str() == principal
            && receipt.payload.source_node_public_key == *local,
        "recovery completion differs from the original request or Noise device"
    );
    receipt.verify_signature()?;
    Ok(Some(receipt))
}

impl Store {
    pub(crate) fn recovery_context(
        &mut self,
        identity: &LocalIdentity,
        principal: &str,
        params: &Value,
        now_ms: u64,
    ) -> Result<Value> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct BeginRequest {
            request: Signed<RecoveryContextRequest>,
        }
        bounded(params.clone())?;
        let request: BeginRequest = serde_json::from_value(params.clone())?;
        self.begin_recovery_read(
            identity,
            principal,
            RecoveryReadRequest::Account(request.request),
            now_ms,
        )
    }

    pub(crate) fn recovery_status(
        &mut self,
        identity: &LocalIdentity,
        principal: &str,
        params: &Value,
        _now_ms: u64,
    ) -> Result<Value> {
        let _verification = ScopedSignatureVerification::new();
        bounded(params.clone())?;
        let request: RecoveryCompletionStatusRequest = serde_json::from_value(params.clone())?;
        let local = PublicKey::parse(&identity.public_key_hex()?)?;
        let tx = self.connection.transaction()?;
        if let Some(receipt) = recovery_completion_in(&tx, &local, principal, &request)? {
            let response = bounded(json!({"state":"committed","receipt":receipt}))?;
            let mut usage = RecoveryProofUsage::new(carrier_bytes(params, &response)?)?;
            usage.record_completion(&receipt)?;
            usage.finish()?;
            return Ok(response);
        }
        let context = recovery_read_reference_in(&tx, identity, principal, &request.context)?;
        ensure!(
            matches!(&context.request, RecoveryReadRequest::Account(_)),
            "recovery status belongs to another purpose"
        );
        let response = json!({"state":"not_committed"});
        let mut usage = RecoveryProofUsage::new(carrier_bytes(params, &response)?)?;
        context.record_reference_usage(&mut usage)?;
        usage.finish()?;
        Ok(response)
    }

    pub(crate) fn recovery_node_possession(
        &mut self,
        identity: &LocalIdentity,
        principal: &str,
        params: &Value,
        now_ms: u64,
    ) -> Result<Value> {
        let _verification = ScopedSignatureVerification::new();
        bounded(params.clone())?;
        let request: StreamedRecoveryNodeRequest = serde_json::from_value(params.clone())?;
        let signer =
            Identity::from_secret_hex(&identity.secret_key_hex).map_err(anyhow::Error::msg)?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let context = completed_recovery_read_in(
            &tx,
            &self.policy,
            identity,
            principal,
            &request.context,
            now_ms,
        )?;
        ensure!(
            matches!(&context.request, RecoveryReadRequest::Account(_)),
            "recovery possession belongs to another purpose"
        );
        let proof = stream::sign_recovery_node_possession_in_state(
            &request,
            &context.request,
            &context.snapshot,
            &context.witness,
            self.policy.verified(),
            &signer,
            &context.snapshot.payload.claim.payload.claim_nonce,
            now_ms,
        )?;
        let key = possession_key(&request.context.snapshot_id);
        let response = bounded(json!({"proof":proof}))?;
        let mut usage = RecoveryProofUsage::new(carrier_bytes(params, &response)?)?;
        context.record_current_usage(self.policy.verified(), &mut usage)?;
        usage.record_node_request(&request, self.policy.verified())?;
        usage.finish()?;
        if let Some(previous) = metadata(&tx, &key)? {
            let previous: ApprovedRecoveryPossession = serde_json::from_str(&previous)?;
            ensure!(
                previous.request == request && previous.proof == proof,
                "recovery snapshot was already bound to another proposal"
            );
        } else {
            let encoded = serde_json::to_string(&ApprovedRecoveryPossession {
                request,
                proof: proof.clone(),
            })?;
            ensure!(
                encoded.len() <= 65_536,
                "stored recovery possession is too large"
            );
            tx.execute(
                "INSERT INTO identity_meta(key,value) VALUES(?,?)",
                params![key, encoded],
            )?;
        }
        tx.commit()?;
        Ok(response)
    }

    pub(crate) fn recover_account(
        &mut self,
        identity: &LocalIdentity,
        principal: &str,
        params: &Value,
        now_ms: u64,
    ) -> Result<Value> {
        let _verification = ScopedSignatureVerification::new();
        bounded(params.clone())?;
        let input: StreamedRecoveryCommitRequest = serde_json::from_value(params.clone())?;
        let input_hash = ObjectHash::body(&input)?;
        let status = RecoveryCompletionStatusRequest {
            context: input.context.clone(),
            input_hash: input_hash.clone(),
        };
        let signer =
            Identity::from_secret_hex(&identity.secret_key_hex).map_err(anyhow::Error::msg)?;
        let local = PublicKey::of(&signer);
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let Some(receipt) = recovery_completion_in(&tx, &local, principal, &status)? {
            let response = bounded(json!({"receipt":receipt}))?;
            let mut usage = RecoveryProofUsage::new(carrier_bytes(params, &response)?)?;
            usage.record_completion(&receipt)?;
            usage.finish()?;
            return Ok(response);
        }
        let context = completed_recovery_read_in(
            &tx,
            &self.policy,
            identity,
            principal,
            &input.context,
            now_ms,
        )?;
        ensure!(
            matches!(&context.request, RecoveryReadRequest::Account(_)),
            "recovery commit belongs to another purpose"
        );
        ensure!(
            input.new_device_public_key.as_str() == principal
                && input.new_device_public_key == *context.request.new_device_public_key()
                && input.claim_nonce == context.snapshot.payload.claim.payload.claim_nonce,
            "recovery commit changed its original client or claim"
        );
        let approved: ApprovedRecoveryPossession = serde_json::from_str(
            &metadata(&tx, &possession_key(&input.context.snapshot_id))?
                .context("recovery Node possession is not approved")?,
        )?;
        ensure!(
            approved.request.context == input.context,
            "recovery commit changed its approved context"
        );
        ensure!(
            input.journal_delta.iter().any(|entry| matches!(
                entry,
                JournalEntry::Device(certificate)
                    if certificate.payload.public_key == local
                        && certificate.payload.possession.as_ref() == Some(&approved.proof)
            )),
            "recovery commit changed its approved Node possession"
        );
        let verified = stream::verify_recovery_stream_commit_in_state(
            &input,
            &approved.request,
            &context.request,
            self.policy.verified(),
            now_ms,
        )?;
        ensure!(
            verified.input_hash() == &input_hash,
            "recovery commit changed its exact input hash"
        );
        let update = self
            .policy
            .prepare_network_update(&input.journal_delta, now_ms)?;
        let after = update.verified_after();
        ensure!(
            verified.committed_heads().len() == 2
                && verified
                    .committed_heads()
                    .iter()
                    .all(|head| after.head(&head.domain) == Some(head)),
            "recovery commit differs from its verified policy delta"
        );
        let receipt = Signed::sign(
            RecoveryCompletionReceipt {
                version: AUTH_VERSION,
                context: input.context.clone(),
                request_hash: input.request_hash.clone(),
                input_hash,
                committed_account_head: after
                    .head(&input.expected_account_head.domain)
                    .cloned()
                    .context("recovered account policy is missing")?,
                committed_space_head: after
                    .head(&input.expected_space_head.domain)
                    .cloned()
                    .context("recovered personal policy is missing")?,
                grant_leaf: input.grant_leaf.clone(),
                new_device_public_key: input.new_device_public_key.clone(),
                source_node_public_key: local,
                accepted_at_ms: now_ms,
            },
            &signer,
        )?;
        let response = bounded(json!({"receipt":receipt}))?;
        let mut usage = RecoveryProofUsage::new(carrier_bytes(params, &response)?)?;
        context.record_current_usage(self.policy.verified(), &mut usage)?;
        usage.record_node_request(&approved.request, self.policy.verified())?;
        usage.record_commit(&input, self.policy.verified())?;
        usage.record_completion(&receipt)?;
        usage.finish()?;
        let state = journal::commit_in(&tx, &update)?;
        project_policy_in(&tx, &state)?;
        tx.execute(
            "INSERT INTO identity_meta(key,value) VALUES(?,?)",
            params![
                completion_key(&input.context.snapshot_id),
                serde_json::to_string(&receipt)?
            ],
        )?;
        tx.commit()?;
        self.policy = state;
        Ok(response)
    }
}
