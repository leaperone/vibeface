use super::{
    admission::check_policy_heads_in,
    foreign_execution_policy::{
        bind_task_execution_policy_in, check_task_policy_heads_in,
        current_task_execution_policy_in, original_task_execution_policy_in,
    },
    journal::PolicyState,
    policy::authorize_invocation_for_body,
};
use crate::{binding::SignedBinding, crypto::LocalIdentity, model::AdmissionProof, store::Store};
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, params};
use serde_json::Value;
use std::borrow::Cow;
use vibeface_secure::authorization::{
    CheckStage, FreshnessChallenge, Invocation, Nonce, RequestFacts, Signed, TransportEvidence,
    VerifiedAuthorization, VerifiedPolicySet,
    node_execution_policy::{VerifiedTaskCurrentPolicy, VerifiedTaskOriginalAdmissionPolicy},
};

#[derive(Clone)]
pub(crate) struct TaskScopedAuthorization {
    authorization: VerifiedAuthorization,
    task_policy: Option<VerifiedTaskCurrentPolicy>,
}

impl TaskScopedAuthorization {
    #[cfg(test)]
    pub(crate) fn from_primary(authorization: VerifiedAuthorization) -> Self {
        Self {
            authorization,
            task_policy: None,
        }
    }

    pub(crate) fn authorization(&self) -> &VerifiedAuthorization {
        &self.authorization
    }

    pub(crate) fn task_policy(&self) -> Option<&VerifiedTaskCurrentPolicy> {
        self.task_policy.as_ref()
    }

    pub(super) fn state<'a>(&'a self, primary: &'a VerifiedPolicySet) -> &'a VerifiedPolicySet {
        self.task_policy
            .as_ref()
            .map_or(primary, |task| task.state())
    }

    pub(crate) fn expires_at_ms(&self) -> u64 {
        self.task_policy
            .as_ref()
            .map_or(self.authorization.expires_at_ms(), |task| {
                task.expires_at_ms().min(self.authorization.expires_at_ms())
            })
    }

    pub(crate) fn check_current_in(
        &self,
        connection: &Connection,
        primary: &VerifiedPolicySet,
        facts: &RequestFacts,
        now_ms: u64,
    ) -> Result<()> {
        let Some(task) = &self.task_policy else {
            return check_policy_heads_in(connection, primary, &self.authorization, now_ms);
        };
        task.check_scope(facts, task.selection(), task.context(), now_ms)?;
        check_task_policy_heads_in(connection, task)?;
        task.state().check_current(&self.authorization, now_ms)?;
        ensure!(
            now_ms < self.expires_at_ms(),
            "task execution authorization expired"
        );
        Ok(())
    }

    pub(crate) fn bind_in(
        &self,
        connection: &Connection,
        body: &Value,
        binding: Option<&SignedBinding>,
        admitted_at_ms: u64,
    ) -> Result<()> {
        if let Some(task) = &self.task_policy {
            bind_task_execution_policy_in(
                connection,
                task,
                &self.authorization,
                body,
                binding,
                admitted_at_ms,
            )?;
        }
        Ok(())
    }
}

pub(crate) struct OriginalAdmissionView<'a> {
    primary: Cow<'a, VerifiedPolicySet>,
    task_policy: Option<VerifiedTaskOriginalAdmissionPolicy>,
}

impl<'a> OriginalAdmissionView<'a> {
    pub(crate) fn load(
        connection: &Connection,
        policy: &'a PolicyState,
        proof: &AdmissionProof,
    ) -> Result<Self> {
        let key = super::event_authorization::audience_key(policy.verified(), proof)?;
        let task_policy = original_task_execution_policy_in(connection, policy, &key, proof)?;
        let heads = task_policy
            .as_ref()
            .map_or(proof.challenge.policies.as_slice(), |task| {
                task.primary_heads()
            });
        let primary = if policy.current_heads_at(heads, proof.admitted_at_ms)? {
            Cow::Borrowed(policy.verified())
        } else {
            Cow::Owned(policy.historical(heads, proof.admitted_at_ms)?)
        };
        if let Some(task) = &task_policy {
            ensure!(
                task.signed_invocation_hash() == &proof.invocation.hash()?
                    && task.admitted_at_ms() == proof.admitted_at_ms
                    && heads
                        .iter()
                        .all(|head| primary.head(&head.domain) == Some(head)),
                "original task policy differs from its accepted admission"
            );
        }
        Ok(Self {
            primary,
            task_policy,
        })
    }

    pub(crate) fn state(&self) -> &VerifiedPolicySet {
        self.task_policy
            .as_ref()
            .map_or(self.primary.as_ref(), |task| task.state())
    }

    pub(crate) fn primary_state(&self) -> &VerifiedPolicySet {
        self.primary.as_ref()
    }

    pub(crate) fn task_policy(&self) -> Option<&VerifiedTaskOriginalAdmissionPolicy> {
        self.task_policy.as_ref()
    }
}

impl Store {
    pub(crate) fn authorize_scoped_request_for_body(
        &self,
        identity: &LocalIdentity,
        invocation: &Signed<Invocation>,
        facts: &RequestFacts,
        transport: &TransportEvidence,
        body: &Value,
        now_ms: u64,
    ) -> Result<TaskScopedAuthorization> {
        let snapshot = self.connection.unchecked_transaction()?;
        let current = PolicyState::reload_verified(&snapshot, &self.policy)?;
        let task_policy = current_task_execution_policy_in(
            &snapshot, &current, identity, invocation, body, now_ms,
        )?;
        let state = task_policy
            .as_ref()
            .map_or(current.verified(), |task| task.state());
        let authorization =
            authorize_invocation_for_body(state, invocation, facts, transport, body, now_ms)?;
        let scoped = TaskScopedAuthorization {
            authorization,
            task_policy,
        };
        scoped.check_current_in(&snapshot, current.verified(), facts, now_ms)?;
        snapshot.commit()?;
        Ok(scoped)
    }

    pub(crate) fn issue_scoped_freshness(
        &mut self,
        scoped: &TaskScopedAuthorization,
        stage: CheckStage,
        now_ms: u64,
    ) -> Result<FreshnessChallenge> {
        let snapshot = self.connection.unchecked_transaction()?;
        let current = PolicyState::reload_verified(&snapshot, &self.policy)?;
        let auth = scoped.authorization();
        let invocation = &auth.signed_invocation().payload;
        let facts = RequestFacts {
            resource: invocation.resource.clone(),
            action: invocation.action,
            audience: invocation.audience.clone(),
            operation_id: invocation.operation_id.clone(),
            body_hash: invocation.body_hash.clone(),
            context: invocation.context.clone(),
        };
        scoped.check_current_in(&snapshot, current.verified(), &facts, now_ms)?;
        let lifetime = if stage == CheckStage::RemoteRead {
            vibeface_secure::authorization::REMOTE_READ_MAX_AGE_MS
        } else {
            vibeface_secure::authorization::EXECUTION_LEASE_MS
        };
        let expires_at_ms = now_ms
            .checked_add(lifetime)
            .context("challenge expiry overflow")?
            .min(scoped.expires_at_ms());
        let challenge =
            FreshnessChallenge::new(auth, stage, Nonce::random()?, now_ms, expires_at_ms)?;
        let encoded = serde_json::to_string(&challenge)?;
        let stage = serde_json::to_string(&stage)?;
        snapshot.execute(
            "INSERT INTO authorization_challenges(space_id,nonce,invocation_hash,stage,challenge,expires_at_ms) VALUES(?,?,?,?,?,?)",
            params![auth.resource().space.as_str(), challenge.nonce.as_str(), auth.invocation_hash().as_str(), stage.trim_matches('"'), encoded, expires_at_ms],
        )?;
        snapshot.commit()?;
        Ok(challenge)
    }
}
