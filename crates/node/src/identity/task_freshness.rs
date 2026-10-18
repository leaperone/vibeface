use super::{
    admission::check_manager_tool_in, journal::PolicyState,
    task_execution_context::TaskScopedAuthorization,
};
use crate::{crypto::LocalIdentity, store::Store};
use anyhow::{Context, Result, ensure};
use rusqlite::params;
use vibeface_secure::{
    authorization::{
        AUTH_VERSION, DeviceId, FreshnessChallenge, Nonce, PublicKey, RequestFacts, ServiceId,
        Signed, TransportEvidence,
        node_execution_policy::{
            NodeExecutionFreshnessRead, NodeExecutionPolicyReadIntent, VerifiedTaskExecutionTrust,
        },
    },
    signature_verification::{ScopedSignatureVerification, SignatureVerification},
};

pub(crate) struct TaskFreshnessRead {
    pub(crate) trust: VerifiedTaskExecutionTrust,
    pub(crate) request: Signed<NodeExecutionFreshnessRead>,
}

impl Store {
    pub(crate) fn task_execution_freshness_read(
        &self,
        identity: &LocalIdentity,
        scoped: &TaskScopedAuthorization,
        transport: &TransportEvidence,
        challenge: &FreshnessChallenge,
        now: u64,
    ) -> Result<TaskFreshnessRead> {
        let signatures = ScopedSignatureVerification::new();
        let task = scoped
            .task_policy()
            .context("Node freshness requires a completed Task policy")?;
        let signer = super::client_policy_read::signer(identity)?;
        let local = PublicKey::of(&signer);
        let invocation = &scoped.signed_invocation().payload;
        ensure!(
            task.request().source_node() == &local
                && invocation.audience.device == DeviceId::from_key(&local)
                && invocation.audience.service == ServiceId::from_key(&local),
            "Node freshness source differs from this Project host"
        );
        let facts = RequestFacts {
            resource: invocation.resource.clone(),
            action: invocation.action,
            audience: invocation.audience.clone(),
            operation_id: invocation.operation_id.clone(),
            body_hash: invocation.body_hash.clone(),
            context: invocation.context.clone(),
        };
        let tx = self.connection.unchecked_transaction()?;
        let primary = PolicyState::reload_verified(&tx, &self.policy)?;
        scoped.check_current_in(&tx, primary.verified(), &facts, now)?;
        check_manager_tool_in(&tx, &primary, scoped.authorization(), transport, now)?;
        let encoded: String = tx.query_row(
            "SELECT challenge FROM authorization_challenges WHERE space_id=? AND nonce=?",
            params![facts.resource.space.as_str(), challenge.nonce.as_str()],
            |row| row.get(0),
        )?;
        ensure!(
            serde_json::from_str::<FreshnessChallenge>(&encoded)? == *challenge
                && *challenge
                    == FreshnessChallenge::new(
                        scoped.authorization(),
                        challenge.stage,
                        challenge.nonce.clone(),
                        challenge.issued_at_ms,
                        challenge.expires_at_ms,
                    )?
                && challenge.issued_at_ms <= now
                && now < challenge.expires_at_ms,
            "Node freshness changed its original issued challenge"
        );
        let mut context = task.context().clone();
        context.context.sdk_session = None;
        let expires_at_ms = challenge.expires_at_ms.min(scoped.expires_at_ms());
        let read = NodeExecutionPolicyReadIntent {
            version: AUTH_VERSION,
            source_project: facts.resource,
            selection: task.selection().clone(),
            context,
            expected_action: invocation.action,
            purpose: task.request().purpose(),
            source_provenance: None,
            execution_pins: task.trust().pins().to_vec(),
            nonce: Nonce::random()?,
            issued_at_ms: now,
            expires_at_ms,
        };
        let request = Signed::sign(
            NodeExecutionFreshnessRead {
                version: AUTH_VERSION,
                read,
                challenge: challenge.clone(),
            },
            &signer,
        )?;
        let mut usage = task.proof_usage().clone();
        usage.record_authorization(scoped.authorization())?;
        usage.record_signed(&request)?;
        usage.record_actual_signature_counts(signatures.counts());
        if let Some(counts) = SignatureVerification::current_counts() {
            usage.record_actual_signature_counts(counts);
        }
        usage.enforce_limits()?;
        tx.commit()?;
        Ok(TaskFreshnessRead {
            trust: task.trust().clone(),
            request,
        })
    }
}
