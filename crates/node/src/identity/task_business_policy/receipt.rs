use super::{source::source_originals_in, *};
use crate::{crypto::LocalIdentity, store::Store};
use anyhow::{Result, ensure};
use rusqlite::Connection;
use vibeface_secure::{
    Identity,
    authorization::{
        FreshnessReceipt, MAX_PROOF_BYTES, PublicKey,
        execution_handoff::ExecutionProofUsage,
        node_execution_policy::{ActualPolicyNoisePeer, VerifiedTaskCurrentPolicy},
        task_business_policy::{
            TaskBusinessAuthorization,
            receipt::{
                TaskBusinessChallenge, TaskBusinessReceiptRead,
                VerifiedTaskBusinessReceiptAuthority, authorize_task_business_challenge,
                verify_task_business_receipt_read,
            },
        },
    },
    signature_verification::{ScopedSignatureVerification, SignatureVerification},
};

fn sign_current_receipts<T: Serialize>(
    connection: &Connection,
    primary: &super::super::journal::PolicyState,
    current: &VerifiedTaskCurrentPolicy,
    authority: &VerifiedTaskBusinessReceiptAuthority,
    identity: &Identity,
    input: &T,
    mut usage: ExecutionProofUsage,
    now: u64,
) -> Result<Vec<Signed<FreshnessReceipt>>> {
    super::super::client_policy_read::check_heads_in(
        connection,
        primary,
        authority.primary_heads(),
    )?;
    super::super::foreign_execution_policy::check_task_policy_heads_in(connection, current)?;
    super::source::current_task(connection, authority.scope(), &mut usage)?;
    let receipts = authority.sign_receipts(identity, now)?;
    ensure!(
        serde_json::to_vec(input)?.len() + serde_json::to_vec(&receipts)?.len() <= MAX_PROOF_BYTES,
        "Task business receipt input and response exceed 64 KiB"
    );
    usage.merge(authority.proof_usage());
    for receipt in &receipts {
        usage.record_signed(receipt)?;
    }
    if let Some(counts) = SignatureVerification::current_counts() {
        ensure!(
            counts.actual_verifications <= 32,
            "Task business receipts exceeded their crypto budget"
        );
        usage.record_actual_signature_counts(counts);
    }
    usage.enforce_limits()?;
    Ok(receipts)
}

impl Store {
    pub(crate) fn task_business_challenge_receipts(
        &self,
        identity: &LocalIdentity,
        current: &VerifiedTaskCurrentPolicy,
        authorization: TaskBusinessAuthorization<'_>,
        wire: &TaskBusinessWire,
        source_header_hash: &ObjectHash,
        challenge: &TaskBusinessChallenge,
        now: u64,
    ) -> Result<Vec<Signed<FreshnessReceipt>>> {
        let _signatures = ScopedSignatureVerification::new();
        let signer = super::super::client_policy_read::signer(identity)?;
        let local_b = PublicKey::of(&signer);
        let tx = self.connection.unchecked_transaction()?;
        let (header, original) = source_originals_in(&tx, source_header_hash, &wire.hash()?)?;
        ensure!(
            original == *wire && header.issuer == local_b,
            "Task business challenge changed its original source wire"
        );
        header.verify_signature()?;
        let authority = authorize_task_business_challenge(
            self.policy.verified(),
            current,
            authorization,
            wire,
            challenge,
            &local_b,
            now,
        )?;
        let mut usage = ExecutionProofUsage::default();
        usage.record_signed(&header)?;
        let receipts = sign_current_receipts(
            &tx,
            &self.policy,
            current,
            &authority,
            &signer,
            challenge,
            usage,
            now,
        )?;
        tx.commit()?;
        Ok(receipts)
    }

    pub(crate) fn read_task_business_receipts(
        &self,
        identity: &LocalIdentity,
        current: &VerifiedTaskCurrentPolicy,
        authorization: TaskBusinessAuthorization<'_>,
        actual_c_noise: &ActualPolicyNoisePeer,
        request: &Signed<TaskBusinessReceiptRead>,
        now: u64,
    ) -> Result<Vec<Signed<FreshnessReceipt>>> {
        let _signatures = ScopedSignatureVerification::new();
        let signer = super::super::client_policy_read::signer(identity)?;
        let local_b = PublicKey::of(&signer);
        let tx = self.connection.unchecked_transaction()?;
        let (header, wire) = source_originals_in(
            &tx,
            &request.payload.source_header_hash,
            &request.payload.business_wire_hash,
        )?;
        let authority = verify_task_business_receipt_read(
            self.policy.verified(),
            current,
            authorization,
            &header,
            &wire,
            actual_c_noise,
            request,
            &local_b,
            now,
        )?;
        let receipts = sign_current_receipts(
            &tx,
            &self.policy,
            current,
            &authority,
            &signer,
            request,
            ExecutionProofUsage::default(),
            now,
        )?;
        tx.commit()?;
        Ok(receipts)
    }
}
