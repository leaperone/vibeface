use super::{original::check_operation_originals_in, storage::*, *};
use crate::{crypto::LocalIdentity, store::Store};
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, TransactionBehavior};
use vibeface_secure::{
    authorization::{
        AuthorizationError, CheckStage, EXECUTION_LEASE_MS, FreshnessReceipt, MAX_PROOF_BYTES,
        Nonce, PolicyPin, PolicyStamp, PublicKey,
        execution_handoff::{
            ExecutionHandoffChallenge, ExecutionHandoffFreshness, ExecutionProofUsage,
            SignedExecutionHandoff,
        },
        remote_execution_history::RemoteExecutionHistoryAcceptance,
        task_business_policy::{
            original::VerifiedTaskBusinessOriginalSource,
            original_operation::{
                TaskBusinessOperationCheck, VerifiedTaskBusinessOperationCurrent,
                begin_task_business_operation_current,
            },
        },
    },
    signature_verification::{ScopedSignatureVerification, SignatureVerification},
};

pub(crate) struct TaskBusinessOperationContext {
    original: VerifiedTaskBusinessOriginalSource,
    check: TaskBusinessOperationCheck,
    challenge: ExecutionHandoffChallenge,
    local_receipts: Vec<Signed<FreshnessReceipt>>,
    finished: bool,
}

impl TaskBusinessOperationContext {
    pub(crate) fn challenge(&self) -> &ExecutionHandoffChallenge {
        &self.challenge
    }

    pub(crate) fn local_receipts(&self) -> &[Signed<FreshnessReceipt>] {
        &self.local_receipts
    }
}

fn current_floors_in(
    connection: &Connection,
    primary: &super::super::journal::PolicyState,
    wire: &SignedExecutionHandoff,
    foreign_heads: &[PolicyStamp],
) -> Result<Vec<PolicyPin>> {
    let grant = primary
        .verified()
        .grant(&wire.invocation.payload.grant_leaf)
        .context("Task operation has no current exact executor grant")?;
    let source = grant
        .payload
        .execution_source
        .as_ref()
        .context("Task operation current executor grant has no approved source")?;
    let pins: Vec<_> = source
        .pins
        .iter()
        .filter(|pin| foreign_heads.iter().any(|head| head.domain == pin.domain))
        .cloned()
        .collect();
    floors(connection, &pins)
}

fn finish_usage(mut usage: ExecutionProofUsage) -> Result<ExecutionProofUsage> {
    if let Some(counts) = SignatureVerification::current_counts() {
        ensure!(
            counts.actual_verifications <= 32,
            "Task operation exceeded its crypto budget"
        );
        usage.record_actual_signature_counts(counts);
    }
    usage.enforce_limits()?;
    Ok(usage)
}

pub(crate) fn check_task_business_operation_current_in(
    connection: &Connection,
    primary: &super::super::journal::PolicyState,
    operation: &VerifiedTaskBusinessOperationCurrent,
    now: u64,
) -> Result<ExecutionProofUsage> {
    let _signatures = ScopedSignatureVerification::new();
    ensure!(
        operation.checked_at_ms() <= now && now < operation.deadline_ms(),
        "Task operation current capability expired or changed its checked clock"
    );
    check_operation_originals_in(connection, primary, operation)?;
    let saved = current_floors_in(
        connection,
        primary,
        operation.wire(),
        operation.foreign_heads(),
    )?;
    let usage = operation.check_current(
        primary.verified(),
        &operation.wire().handoff.target_public_key,
        &saved,
        now,
    )?;
    super::super::client_policy_read::check_heads_in(
        connection,
        primary,
        operation.primary_heads(),
    )?;
    finish_usage(usage)
}

impl Store {
    pub(crate) fn begin_task_business_operation_check(
        &mut self,
        identity: &LocalIdentity,
        original: &VerifiedTaskBusinessOriginalSource,
        wire: &TaskBusinessWire,
        acceptance: &RemoteExecutionHistoryAcceptance,
        stage: CheckStage,
        now: u64,
    ) -> Result<TaskBusinessOperationContext> {
        let _signatures = ScopedSignatureVerification::new();
        ensure!(
            matches!(stage, CheckStage::Start | CheckStage::Renew),
            "Task operation permits only Start or Renew"
        );
        let TaskBusinessWire::Submit(submission) = wire else {
            anyhow::bail!("Task operation requires its original accepted Submit")
        };
        let local_c = PublicKey::parse(&identity.public_key_hex()?)?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        super::check_task_business_original_source_in(
            &tx,
            &self.policy,
            original,
            wire,
            acceptance,
        )?;
        let saved = current_floors_in(&tx, &self.policy, submission, original.foreign_heads())?;
        let check = begin_task_business_operation_current(
            self.policy.verified(),
            original,
            &local_c,
            &saved,
            now,
        )?;
        super::super::client_policy_read::check_heads_in(&tx, &self.policy, check.primary_heads())?;
        let challenge = check.issue_challenge(
            stage,
            Nonce::random()?,
            now,
            now.checked_add(EXECUTION_LEASE_MS)
                .context("Task operation challenge expiry overflow")?
                .min(check.expires_at_ms()),
        )?;
        let signer = super::super::client_policy_read::signer(identity)?;
        let local_receipts = check.sign_receipts(&signer, &challenge, now)?;
        ensure!(
            serde_json::to_vec(&challenge)?.len() + serde_json::to_vec(&local_receipts)?.len()
                <= MAX_PROOF_BYTES,
            "Task operation challenge and local receipts exceed 64 KiB"
        );
        super::super::execution_handoff::persist_execution_handoff_operation_challenge_in(
            &tx, submission, &challenge,
        )?;
        let mut usage = check.proof_usage().clone();
        usage.record_metadata(&challenge)?;
        for receipt in &local_receipts {
            usage.record_signed(receipt)?;
        }
        finish_usage(usage)?;
        tx.commit()?;
        Ok(TaskBusinessOperationContext {
            original: original.clone(),
            check,
            challenge,
            local_receipts,
            finished: false,
        })
    }

    pub(crate) fn finish_task_business_operation_check(
        &self,
        identity: &LocalIdentity,
        context: &mut TaskBusinessOperationContext,
        proof: &ExecutionHandoffFreshness,
        now: u64,
    ) -> Result<VerifiedTaskBusinessOperationCurrent> {
        let _signatures = ScopedSignatureVerification::new();
        ensure!(
            !context.finished && proof.challenge == context.challenge,
            "Task operation proof changed or completed its persisted challenge"
        );
        let local_c = PublicKey::parse(&identity.public_key_hex()?)?;
        let tx = self.connection.unchecked_transaction()?;
        super::check_task_business_original_source_in(
            &tx,
            &self.policy,
            &context.original,
            context.original.wire(),
            context.original.acceptance(),
        )?;
        let saved = current_floors_in(
            &tx,
            &self.policy,
            context.check.wire(),
            context.check.foreign_heads(),
        )?;
        let current = begin_task_business_operation_current(
            self.policy.verified(),
            &context.original,
            &local_c,
            &saved,
            now,
        )?;
        if current.dependencies() != context.check.dependencies()
            || current.primary_heads() != context.check.primary_heads()
        {
            return Err(AuthorizationError::NeedPolicySync(current.dependencies().to_vec()).into());
        }
        let operation = current.finish(proof, now)?;
        check_task_business_operation_current_in(&tx, &self.policy, &operation, now)?;
        super::super::execution_handoff::check_execution_handoff_operation_challenge_in(
            &tx,
            operation.wire(),
            operation.freshness(),
            false,
        )?;
        tx.commit()?;
        context.finished = true;
        Ok(operation)
    }
}
