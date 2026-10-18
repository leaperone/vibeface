use super::*;
use vibeface_secure::authorization::{
    node_execution_policy::{
        NodeTaskExecutionPolicyImport, TaskExecutionPolicyImport, VerifiedTaskCurrentPolicy,
        VerifiedTaskHistoryPolicy, VerifiedTaskOriginalAdmissionPolicy,
    },
    remote_execution_policy::{
        RemoteExecutionPolicyPage, RemoteExecutionPolicyPageRequest, RemoteExecutionPolicyWitness,
    },
};

#[derive(Clone)]
pub(super) enum TaskPolicyImport {
    Account(TaskExecutionPolicyImport),
    Node(NodeTaskExecutionPolicyImport),
}

impl TaskPolicyImport {
    pub(super) fn proof_usage(&self) -> &ExecutionProofUsage {
        match self {
            Self::Account(value) => value.proof_usage(),
            Self::Node(value) => value.proof_usage(),
        }
    }
    pub(super) fn append_page(
        &mut self,
        page: &Signed<RemoteExecutionPolicyPage>,
        now: u64,
    ) -> Result<()> {
        match self {
            Self::Account(value) => value.append_page(page, now)?,
            Self::Node(value) => value.append_page(page, now)?,
        };
        Ok(())
    }
    pub(super) fn page_request(&self) -> Result<RemoteExecutionPolicyPageRequest> {
        Ok(match self {
            Self::Account(value) => value.page_request()?,
            Self::Node(value) => value.page_request(),
        })
    }
    pub(super) fn finish_current(
        self,
        witness: &Signed<RemoteExecutionPolicyWitness>,
        now: u64,
    ) -> Result<VerifiedTaskCurrentPolicy> {
        Ok(match self {
            Self::Account(value) => value.finish_current(witness, now)?,
            Self::Node(value) => value.finish_current(witness, now)?,
        })
    }
    pub(super) fn finish_history(
        self,
        witness: &Signed<RemoteExecutionPolicyWitness>,
        now: u64,
    ) -> Result<VerifiedTaskHistoryPolicy> {
        Ok(match self {
            Self::Account(value) => value.finish_history(witness, now)?,
            Self::Node(value) => value.finish_history(witness, now)?,
        })
    }
    pub(super) fn finish_original_admission(
        self,
        witness: &Signed<RemoteExecutionPolicyWitness>,
        proof: &crate::model::AdmissionProof,
    ) -> Result<VerifiedTaskOriginalAdmissionPolicy> {
        Ok(match self {
            Self::Account(value) => value.finish_original_admission(witness, proof)?,
            Self::Node(value) => value.finish_original_admission(witness, proof)?,
        })
    }
    pub(super) fn finish_retained_floors(
        self,
        witness: &Signed<RemoteExecutionPolicyWitness>,
        clock: u64,
    ) -> Result<Vec<vibeface_secure::authorization::PolicyPin>> {
        Ok(match self {
            Self::Account(value) => value.finish_retained_floors(witness, clock)?,
            Self::Node(value) => value.finish_retained_floors(witness, clock)?,
        })
    }
}
