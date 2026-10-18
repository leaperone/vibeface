use super::{storage::*, *};
use crate::{binding::SignedBinding, crypto::LocalIdentity, store::Store};
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use std::collections::BTreeSet;
use vibeface_secure::{
    authorization::{
        MAX_PROOF_BYTES, PolicyDomain, PublicKey,
        execution_handoff::{
            ExecutionProofUsage, MAX_EXECUTION_HANDOFF_BYTES, SignedExecutionHandoff,
            SignedExecutionHandoffResolution,
        },
        node_execution_policy::VerifiedTaskCurrentPolicy,
        task_business_policy::{
            TaskBusinessAuthorization, TaskBusinessPolicyExport, TaskBusinessPurpose,
            TaskBusinessScope, build_task_business_policy_export,
        },
    },
    signature_verification::{ScopedSignatureVerification, SignatureVerification},
};

pub(crate) struct TaskBusinessPolicySourceContext {
    exporter: TaskBusinessPolicyExport,
    current: VerifiedTaskCurrentPolicy,
    snapshot: super::super::journal::AcceptedJournalSnapshot,
    cursor: Option<super::super::journal::AcceptedJournalCursor>,
    journal_complete: bool,
    outbox: OutboxAnchor,
}

pub(crate) struct TaskBusinessSourceChunk {
    pub(crate) page: Option<Signed<TaskBusinessPolicyPage>>,
    pub(crate) witness: Option<Signed<TaskBusinessPolicyWitness>>,
}

impl TaskBusinessPolicySourceContext {
    pub(crate) fn export(&self) -> &TaskBusinessPolicyExport {
        &self.exporter
    }

    pub(crate) fn header(&self) -> &Signed<TaskBusinessPolicyHeader> {
        self.exporter.header()
    }
    pub(crate) fn target_public_key(&self) -> &PublicKey {
        self.exporter.target_public_key()
    }
}

struct OutboxAnchor {
    reference: OutboxReference,
    wire_json: String,
}

#[derive(Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct OutboxReference {
    space: String,
    key: String,
    nested: bool,
}

fn local_value(connection: &Connection, space: &str, key: &str, nested: bool) -> Result<String> {
    let field = if nested {
        "json_extract(value,'$.wire')"
    } else {
        "value"
    };
    let query = format!(
        "SELECT CASE WHEN length(CAST({field} AS BLOB))<=? THEN {field} ELSE NULL END FROM local_values WHERE space_id=? AND key=?"
    );
    let value: Option<String> = connection.query_row(
        &query,
        params![MAX_EXECUTION_HANDOFF_BYTES, space, key],
        |row| row.get(0),
    )?;
    value.context("Task business outbox carrier exceeds its bound")
}

fn retained_outbox(connection: &Connection, wire: &TaskBusinessWire) -> Result<OutboxAnchor> {
    let scope = wire.scope()?;
    let space = scope.source_project.space.to_string();
    let (key, nested, wire_json) = match wire {
        TaskBusinessWire::Submit(value) => {
            let name = format!(
                "project-execution-handoff:{}",
                value.invocation.payload.operation_id
            );
            let saved = local_value(connection, &space, &name, false)?;
            ensure!(
                serde_json::from_str::<SignedExecutionHandoff>(&saved)? == *value,
                "Task business submission differs from its immutable Project outbox"
            );
            (name, false, saved)
        }
        TaskBusinessWire::Resolve(value) => {
            let name = format!(
                "project-execution-resolution:{}",
                value
                    .resolution
                    .account_admission
                    .invocation
                    .payload
                    .operation_id
            );
            let saved = local_value(connection, &space, &name, true)?;
            ensure!(
                serde_json::from_str::<SignedExecutionHandoffResolution>(&saved)? == *value,
                "Task business resolution differs from its immutable Project outbox"
            );
            (name, true, saved)
        }
        TaskBusinessWire::Observe(value) => {
            let target = &value.request.payload.intent.target;
            let original = local_value(
                connection,
                &space,
                &format!("project-execution-handoff:{}", target.source.operation_id),
                false,
            )?;
            let original: SignedExecutionHandoff = serde_json::from_str(&original)?;
            ensure!(
                original.hash()? == target.handoff_hash,
                "Task business observation changed its durable original submission"
            );
            let name = format!("task-business-observation:{}", value.request.hash()?);
            let wire_json = serde_json::to_string(value)?;
            ensure!(
                wire_json.len() <= MAX_PROOF_BYTES,
                "Task business observation exceeds its carrier bound"
            );
            let prior: Option<String> = connection
                .query_row(
                    "SELECT value FROM local_values WHERE space_id=? AND key=?",
                    params![space, name],
                    |row| row.get(0),
                )
                .optional()?;
            if let Some(prior) = prior {
                ensure!(
                    prior == wire_json,
                    "Task business observation already has another immutable wire"
                );
            } else {
                connection.execute(
                    "INSERT INTO local_values(space_id,key,value) VALUES(?,?,?)",
                    params![space, name, wire_json],
                )?;
            }
            (name, false, wire_json)
        }
    };
    Ok(OutboxAnchor {
        reference: OutboxReference { space, key, nested },
        wire_json,
    })
}

fn check_outbox(connection: &Connection, anchor: &OutboxAnchor) -> Result<()> {
    let field = if anchor.reference.nested {
        "json_extract(value,'$.wire')"
    } else {
        "value"
    };
    let query = format!(
        "SELECT EXISTS(SELECT 1 FROM local_values WHERE space_id=? AND key=? AND {field}=?)"
    );
    let same: bool = connection.query_row(
        &query,
        params![
            anchor.reference.space,
            anchor.reference.key,
            anchor.wire_json
        ],
        |row| row.get(0),
    )?;
    ensure!(
        same,
        "Task business immutable outbox changed during source export"
    );
    Ok(())
}

pub(super) fn source_originals_in(
    connection: &Connection,
    reference: &ObjectHash,
    wire_hash: &ObjectHash,
) -> Result<(Signed<TaskBusinessPolicyHeader>, TaskBusinessWire)> {
    let header: Signed<TaskBusinessPolicyHeader> =
        read(connection, &key("source-header", reference))?
            .context("Task business source original header is missing")?;
    ensure!(
        header.hash()? == *reference && header.payload.scope.business_wire_hash == *wire_hash,
        "Task business source changed its original header or wire hash"
    );
    let anchor: OutboxReference = read(connection, &key("source-outbox", reference))?
        .context("Task business source original outbox reference is missing")?;
    ensure!(
        anchor.space == header.payload.scope.source_project.space.as_str(),
        "Task business source original outbox belongs to another space"
    );
    let original = local_value(connection, &anchor.space, &anchor.key, anchor.nested)?;
    let wire = match header.payload.scope.purpose {
        TaskBusinessPurpose::Submit => TaskBusinessWire::Submit(serde_json::from_str(&original)?),
        TaskBusinessPurpose::Observe => TaskBusinessWire::Observe(serde_json::from_str(&original)?),
        TaskBusinessPurpose::ResolveUnknown | TaskBusinessPurpose::ReconcileTerminal => {
            TaskBusinessWire::Resolve(serde_json::from_str(&original)?)
        }
    };
    ensure!(
        wire.scope()? == header.payload.scope,
        "Task business source original outbox changed its complete wire"
    );
    Ok((header, wire))
}

fn current_task(
    connection: &Connection,
    scope: &TaskBusinessScope,
    usage: &mut ExecutionProofUsage,
) -> Result<()> {
    let space = scope.source_project.space.as_str();
    let current: String = connection.query_row("SELECT value FROM local_values WHERE space_id=? AND key=? AND length(CAST(value AS BLOB))<=?",
        params![space,format!("task-binding-current:{}",scope.task_ref),MAX_PROOF_BYTES],|row|row.get(0))?;
    let current: String = serde_json::from_str(&current)?;
    let binding: String = connection.query_row("SELECT value FROM local_values WHERE space_id=? AND key=? AND length(CAST(value AS BLOB))<=?",
        params![space,format!("task-binding-record:{current}"),MAX_PROOF_BYTES],|row|row.get(0))?;
    let binding: SignedBinding = serde_json::from_str(&binding)?;
    crate::binding::verify_binding(&binding, scope.source_node_public_key.as_str())?;
    let value = &binding.binding;
    ensure!(
        crate::binding::binding_hash(value)? == current
            && value.network_id == space
            && value.task_ref == scope.task_ref.as_str()
            && value.generation == scope.binding_generation
            && value.session_id == scope.execution_session.session_id
            && value.target_node == crate::crypto::fingerprint(scope.target_public_key.as_str())?,
        "Task business source changed its current Task binding"
    );
    let ready: bool = connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM events WHERE space_id=?1 AND hash=?2 AND status='ready' AND resource_id=?3 AND scope_id=?4
          AND json_extract(envelope,'$.hash')=?2
          AND json_extract(json_extract(envelope,'$.canonical_body'),'$.space_id')=?1
          AND json_extract(json_extract(envelope,'$.canonical_body'),'$.resource_id')=?3
          AND json_extract(json_extract(envelope,'$.canonical_body'),'$.scope_id')=?4
          AND json_extract(json_extract(envelope,'$.canonical_body'),'$.body.type')='task_created'
          AND json_extract(json_extract(envelope,'$.canonical_body'),'$.body.task_id')=?5)",
        params![space,scope.task_ref.as_str(),scope.source_project.resource.as_str(),value.conversation_id,value.task_id],|row|row.get(0))?;
    ensure!(
        ready,
        "Task business source Task is not the retained ready Project task"
    );
    usage.record_binding(&binding)?;
    Ok(())
}

fn usage(mut usage: ExecutionProofUsage, request: &TaskBusinessRequest) -> Result<()> {
    let response = match request {
        TaskBusinessRequest::SourceBegin { header } => TaskBusinessResponse::SourceBegun {
            context_hash: header.hash()?,
        },
        TaskBusinessRequest::SourcePage { page } => TaskBusinessResponse::SourcePageAccepted {
            context_hash: page.payload.header_hash.clone(),
            next_page_index: page
                .payload
                .page_index
                .checked_add(1)
                .context("Task business page index overflow")?,
            complete: page.payload.complete,
        },
        TaskBusinessRequest::SourceFinish { witness } => TaskBusinessResponse::SourceFinished {
            context_hash: witness.payload.header_hash.clone(),
        },
        TaskBusinessRequest::Business(_) => {
            anyhow::bail!("Task business source cannot export a business request")
        }
    };
    ensure!(
        serde_json::to_vec(request)?.len() + serde_json::to_vec(&response)?.len()
            <= MAX_PROOF_BYTES,
        "Task business source input and response exceed 64 KiB"
    );
    usage.record_metadata(&response)?;
    if let Some(counts) = SignatureVerification::current_counts() {
        ensure!(
            counts.actual_verifications <= 32,
            "Task business source exceeded its crypto budget"
        );
        usage.record_actual_signature_counts(counts);
    }
    usage.enforce_limits()?;
    Ok(())
}

impl Store {
    pub(crate) fn task_business_source_originals(
        &self,
        reference: &ObjectHash,
        wire_hash: &ObjectHash,
    ) -> Result<(Signed<TaskBusinessPolicyHeader>, TaskBusinessWire)> {
        let tx = self.connection.unchecked_transaction()?;
        let originals = source_originals_in(&tx, reference, wire_hash)?;
        tx.commit()?;
        Ok(originals)
    }

    pub(crate) fn begin_task_business_source(
        &mut self,
        identity: &LocalIdentity,
        current: &VerifiedTaskCurrentPolicy,
        authorization: TaskBusinessAuthorization<'_>,
        wire: &TaskBusinessWire,
        now: u64,
    ) -> Result<TaskBusinessPolicySourceContext> {
        let _signatures = ScopedSignatureVerification::new();
        let signer = super::super::client_policy_read::signer(identity)?;
        let exporter =
            build_task_business_policy_export(current, authorization, wire, &signer, now)?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        super::super::client_policy_read::check_heads_in(
            &tx,
            &self.policy,
            exporter.source_heads(),
        )?;
        super::super::foreign_execution_policy::check_task_policy_heads_in(&tx, current)?;
        let mut proof = exporter.proof_usage().clone();
        current_task(&tx, exporter.scope(), &mut proof)?;
        let outbox = retained_outbox(&tx, wire)?;
        let snapshot = self.policy.accepted_journal_snapshot(&tx)?;
        usage(
            proof,
            &TaskBusinessRequest::SourceBegin {
                header: exporter.header().clone(),
            },
        )?;
        immutable(
            &tx,
            &key("source-header", &exporter.header().hash()?),
            exporter.header(),
        )?;
        immutable(
            &tx,
            &key("source-outbox", &exporter.header().hash()?),
            &outbox.reference,
        )?;
        tx.commit()?;
        Ok(TaskBusinessPolicySourceContext {
            exporter,
            current: current.clone(),
            snapshot,
            cursor: None,
            journal_complete: false,
            outbox,
        })
    }

    pub(crate) fn next_task_business_source(
        &mut self,
        identity: &LocalIdentity,
        context: &mut TaskBusinessPolicySourceContext,
        now: u64,
    ) -> Result<TaskBusinessSourceChunk> {
        let _signatures = ScopedSignatureVerification::new();
        let signer = super::super::client_policy_read::signer(identity)?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        super::super::client_policy_read::check_heads_in(
            &tx,
            &self.policy,
            context.exporter.source_heads(),
        )?;
        super::super::foreign_execution_policy::check_task_policy_heads_in(&tx, &context.current)?;
        check_outbox(&tx, &context.outbox)?;
        let mut proof = ExecutionProofUsage::default();
        current_task(&tx, context.exporter.scope(), &mut proof)?;
        let mut staged = context.exporter.clone();
        let mut next_cursor = context.cursor.clone();
        let mut complete = context.journal_complete;
        let (page, witness) = if complete {
            let (page, witness) = staged.finish_pages(&signer, now)?;
            (Some(page), Some(witness))
        } else {
            let domains: BTreeSet<PolicyDomain> = staged
                .source_heads()
                .iter()
                .map(|head| head.domain.clone())
                .collect();
            let scanned = self.policy.accepted_journal_page(
                &tx,
                &context.snapshot,
                context.cursor.as_ref(),
                &domains,
            )?;
            ensure!(
                scanned.scanned <= 32,
                "Task business source scan exceeded its row bound"
            );
            next_cursor = Some(scanned.next_cursor);
            complete = scanned.complete;
            let page = if let Some(entry) = scanned.entry {
                if staged.allows_entry(&entry.entry)? {
                    Some(staged.append_entry_page(&signer, entry.entry, now)?)
                } else {
                    None
                }
            } else {
                None
            };
            (page, None)
        };
        proof.merge(staged.proof_usage());
        if let Some(page) = &page {
            usage(
                proof.clone(),
                &TaskBusinessRequest::SourcePage { page: page.clone() },
            )?;
        } else {
            if let Some(counts) = SignatureVerification::current_counts() {
                ensure!(
                    counts.actual_verifications <= 32,
                    "Task business source scan exceeded its crypto budget"
                );
                proof.record_actual_signature_counts(counts);
            }
            proof.enforce_limits()?;
        }
        if let Some(witness) = &witness {
            usage(
                proof,
                &TaskBusinessRequest::SourceFinish {
                    witness: witness.clone(),
                },
            )?;
        }
        tx.commit()?;
        context.exporter = staged;
        context.cursor = next_cursor;
        context.journal_complete = complete;
        Ok(TaskBusinessSourceChunk { page, witness })
    }
}
