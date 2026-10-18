use super::{
    event_authorization::project_dispatch_origin_in, execution_handoff::public_execution_result,
};
use crate::{crypto::LocalIdentity, model::*, store::Store};
use anyhow::{Context, Result, ensure};
use rusqlite::{OptionalExtension, Transaction, TransactionBehavior, params};
use serde_json::json;
use vibeface_secure::authorization::{
    AUTH_VERSION, DeviceId, EventRef, ObjectHash, PublicKey, ResourceRef, Signed, SpaceId,
    execution_handoff::{
        ExecutionSessionSnapshot, ProjectDispatchOrigin, ProjectExecutionReceipt,
        ProjectExecutionResult, derive_project_dispatch_work,
        verify_project_execution_receipt_signature,
    },
};

fn result_key(operation: &str) -> String {
    format!("project-execution-result:{operation}")
}

impl Store {
    pub(crate) fn local_project_dispatch_origin(
        &self,
        dispatch: &SignedEvent,
    ) -> Result<ProjectDispatchOrigin> {
        project_dispatch_origin_in(&self.connection, &self.policy, dispatch)
    }

    pub(crate) fn local_project_execution_result(
        &self,
        space: &str,
        operation: &str,
    ) -> Result<Option<ProjectExecutionResult>> {
        self.local_value(space, &result_key(operation))?
            .map(serde_json::from_value)
            .transpose()
            .map_err(Into::into)
    }

    pub(crate) fn seal_local_execution_result(
        &mut self,
        identity: &LocalIdentity,
        space: &str,
        operation_id: &str,
        now: u64,
    ) -> Result<ProjectExecutionResult> {
        let dispatch = self
            .operation_dispatch(space, operation_id)?
            .context("local execution has no admitted dispatch")?;
        let tx = Transaction::new_unchecked(&self.connection, TransactionBehavior::Immediate)?;
        let origin = project_dispatch_origin_in(&tx, &self.policy, &dispatch)?;
        let event: Event = serde_json::from_str(&dispatch.canonical_body)?;
        let EventCause::Invocation { admission } = &event.cause else {
            anyhow::bail!("local execution dispatch has no original admission");
        };
        let (request, state, raw): (String, String, String) = tx.query_row(
            "SELECT request,state,result FROM operations WHERE space_id=? AND operation_id=?",
            params![space, operation_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )?;
        let request: OperationRequest = serde_json::from_str(&request)?;
        let state: OperationState = serde_json::from_str(&state)?;
        ensure!(
            request.network_id == space
                && request.target_node == identity.node_id()?
                && request.operation_id == operation_id
                && event.network_id == space
                && matches!(&event.body, EventBody::DispatchRecorded {action,..} if *action == json!(request)),
            "local execution result differs from its admitted request"
        );
        ensure!(
            state.terminal() || matches!(state, OperationState::Running | OperationState::Unknown),
            "local execution result is not settled"
        );
        let target = PublicKey::parse(&identity.public_key_hex()?)?;
        let project = ResourceRef {
            space: SpaceId::parse(space)?,
            resource: event.resource_id.clone(),
        };
        let work = derive_project_dispatch_work(&event, &origin, &target)?;
        let session: Option<ExecutionSession> = request.session_id.as_deref().map(|id| {
            let encoded: Option<String> = tx.query_row(
                "SELECT record FROM execution_sessions WHERE space_id=? AND target_node=? AND session_id=?",
                params![space,request.target_node,id],|row|row.get(0),
            ).optional()?;
            encoded.map(|encoded|serde_json::from_str(&encoded)).transpose().map_err(anyhow::Error::from)
        }).transpose()?.flatten();
        let raw = if state == OperationState::Running {
            tx.query_row(
                "SELECT value FROM local_values WHERE space_id=? AND key=?",
                params![space, format!("turn:{operation_id}")],
                |row| row.get::<_, String>(0),
            )?
        } else {
            raw
        };
        let result = public_execution_result(
            serde_json::from_str(&raw)?,
            session
                .as_ref()
                .and_then(|session| session.native_session_id.as_deref()),
        );
        let previous: Option<String> = tx
            .query_row(
                "SELECT value FROM local_values WHERE space_id=? AND key=?",
                params![space, result_key(operation_id)],
                |row| row.get(0),
            )
            .optional()?;
        if let Some(previous) = previous {
            let previous: ProjectExecutionResult = serde_json::from_str(&previous)?;
            ensure!(
                previous.dispatch == dispatch,
                "local receipt changed its dispatch"
            );
            verify_project_execution_receipt_signature(
                &dispatch,
                &origin,
                &previous.receipt,
                &previous.result,
            )?;
            if previous.result == result && json!(previous.receipt.payload.state) == json!(state) {
                tx.commit()?;
                return Ok(previous);
            }
            ensure!(
                matches!(previous.receipt.payload.state,
                    vibeface_secure::authorization::execution_handoff::ExecutionHandoffState::Accepted
                    | vibeface_secure::authorization::execution_handoff::ExecutionHandoffState::Running
                    | vibeface_secure::authorization::execution_handoff::ExecutionHandoffState::Unknown),
                "a settled local execution result cannot change"
            );
        }
        let snapshot = session
            .map(|session| -> Result<_> {
                ensure!(
                    session.revision
                        >= request
                            .expected_revision
                            .checked_add(u64::from(state.terminal()))
                            .context("execution revision overflow")?,
                    "local execution session has not reached its result revision"
                );
                Ok(ExecutionSessionSnapshot {
                    original_project: project.clone(),
                    execution_resource: project.clone(),
                    work: work.clone(),
                    target: DeviceId::from_key(&target),
                    session_id: session.session_id,
                    revision: session.revision,
                    executor: session.executor,
                    cwd: session.cwd,
                    state: session.state,
                    expires_at_ms: now
                        .checked_add(vibeface_secure::authorization::EXECUTION_LEASE_MS)
                        .context("local execution snapshot expiry overflow")?,
                })
            })
            .transpose()?;
        let signer = vibeface_secure::Identity::from_secret_hex(&identity.secret_key_hex)
            .map_err(anyhow::Error::msg)?;
        let receipt = Signed::sign(
            ProjectExecutionReceipt {
                version: AUTH_VERSION,
                dispatch: EventRef::parse(&dispatch.hash)?,
                project,
                target_public_key: target,
                operation_id: operation_id.into(),
                original_invocation_hash: admission.invocation.hash()?,
                original_admission_hash: ObjectHash::body(admission)?,
                work,
                state: serde_json::from_value(json!(state))?,
                result_hash: ObjectHash::body(&result)?,
                session: snapshot,
                issued_at_ms: now,
            },
            &signer,
        )?;
        verify_project_execution_receipt_signature(&dispatch, &origin, &receipt, &result)?;
        let value = ProjectExecutionResult {
            dispatch,
            receipt,
            result,
        };
        tx.execute("INSERT INTO local_values(space_id,key,value) VALUES(?,?,?) ON CONFLICT(space_id,key) DO UPDATE SET value=excluded.value",
            params![space,result_key(operation_id),serde_json::to_string(&value)?])?;
        tx.commit()?;
        Ok(value)
    }
}
