use super::{ingress::*, storage::*, *};
use crate::{crypto::LocalIdentity, store::Store};
use anyhow::{Context, Result, ensure};
use rusqlite::Connection;
use vibeface_secure::{
    authorization::{
        MAX_POLICY_DOMAINS, PolicyPin, PolicyStamp, PublicKey,
        execution_handoff::ExecutionProofUsage,
        remote_execution_history::RemoteExecutionHistoryAcceptance,
        task_business_policy::{
            TaskBusinessScopedView,
            original::{
                TaskBusinessOriginalPolicyImport, VerifiedTaskBusinessOriginalSource,
                begin_task_business_original_import,
            },
            original_operation::VerifiedTaskBusinessOperationCurrent,
        },
    },
    signature_verification::{ScopedSignatureVerification, SignatureVerification},
};

#[derive(Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
struct AcceptedSourceAssociation {
    wire_hash: ObjectHash,
    acceptance_envelope_hash: ObjectHash,
    header_hash: ObjectHash,
    header_envelope_hash: ObjectHash,
    witness_envelope_hash: ObjectHash,
    page_envelope_hashes_hash: ObjectHash,
    accepted_at_ms: u64,
    primary_heads: Vec<PolicyStamp>,
    primary_pins: Vec<PolicyPin>,
}

pub(crate) struct TaskBusinessOriginalRecovery {
    association: AcceptedSourceAssociation,
    importer: TaskBusinessOriginalPolicyImport,
    finished: bool,
}

fn association_in(
    connection: &Connection,
    wire_hash: &ObjectHash,
) -> Result<AcceptedSourceAssociation> {
    let association: AcceptedSourceAssociation =
        read(connection, &key("accepted-source", wire_hash))?
            .context("NeedPolicySync: accepted Task business has no original source association")?;
    ensure!(
        association.wire_hash == *wire_hash,
        "accepted Task business source association changed its original wire"
    );
    Ok(association)
}

fn check_primary_in(
    connection: &Connection,
    primary: &super::super::journal::PolicyState,
    association: &AcceptedSourceAssociation,
) -> Result<()> {
    ensure!(
        !association.primary_heads.is_empty()
            && association.primary_heads.len() <= MAX_POLICY_DOMAINS
            && association
                .primary_heads
                .windows(2)
                .all(|pair| pair[0].domain < pair[1].domain)
            && association.primary_heads.len() == association.primary_pins.len()
            && association
                .primary_heads
                .iter()
                .zip(&association.primary_pins)
                .all(|(head, pin)| head.domain == pin.domain && pin.minimum_head.is_none()),
        "accepted Task business source changed its original executor domains"
    );
    let mut current = Vec::new();
    for pin in &association.primary_pins {
        let accepted = primary
            .pins()
            .iter()
            .find(|accepted| accepted.domain == pin.domain)
            .context("accepted Task business executor genesis is no longer pinned")?;
        ensure!(
            accepted.genesis_root == pin.genesis_root,
            "accepted Task business executor genesis changed"
        );
        current.push(
            primary
                .verified()
                .head(&pin.domain)
                .context("accepted Task business executor domain is missing")?
                .clone(),
        );
    }
    super::super::client_policy_read::check_heads_in(connection, primary, &current)
}

fn originals_in(
    connection: &Connection,
    association: &AcceptedSourceAssociation,
) -> Result<(
    Signed<TaskBusinessPolicyHeader>,
    Signed<TaskBusinessPolicyWitness>,
)> {
    let header: Signed<TaskBusinessPolicyHeader> =
        read(connection, &key("import-header", &association.header_hash))?
            .context("NeedPolicySync: accepted Task business source header is missing")?;
    ensure!(
        header.hash()? == association.header_hash
            && ObjectHash::body(&header)? == association.header_envelope_hash
            && header.payload.scope.business_wire_hash == association.wire_hash,
        "accepted Task business source header changed its original envelope"
    );
    let witness = completed_witness_in(connection, &header)?;
    ensure!(
        ObjectHash::body(&witness)? == association.witness_envelope_hash,
        "accepted Task business source witness changed its original envelope"
    );
    Ok((header, witness))
}

fn finish_usage(mut usage: ExecutionProofUsage) -> Result<()> {
    if let Some(counts) = SignatureVerification::current_counts() {
        ensure!(
            counts.actual_verifications <= 32,
            "original Task business source exceeded its crypto budget"
        );
        usage.record_actual_signature_counts(counts);
    }
    usage.enforce_limits()?;
    Ok(())
}

pub(crate) fn bind_task_business_original_in(
    connection: &Connection,
    primary: &super::super::journal::PolicyState,
    view: &TaskBusinessScopedView,
    wire: &TaskBusinessWire,
    acceptance: &RemoteExecutionHistoryAcceptance,
    now: u64,
) -> Result<()> {
    let _signatures = ScopedSignatureVerification::new();
    check_task_business_policy_view_in(connection, primary, view, wire, now)?;
    let source = view.source();
    let original = begin_task_business_original_import(
        primary.verified(),
        wire,
        acceptance,
        source.header(),
        &view.scope().target_public_key,
        now,
    )?;
    let primary_pins = view
        .primary_heads()
        .iter()
        .map(|head| {
            let pin = primary
                .pins()
                .iter()
                .find(|pin| pin.domain == head.domain)
                .context("accepted Task business executor has no independent genesis")?;
            Ok(PolicyPin {
                domain: pin.domain.clone(),
                genesis_root: pin.genesis_root.clone(),
                minimum_head: None,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    let association = AcceptedSourceAssociation {
        wire_hash: wire.hash()?,
        acceptance_envelope_hash: ObjectHash::body(acceptance)?,
        header_hash: source.header().hash()?,
        header_envelope_hash: ObjectHash::body(source.header())?,
        witness_envelope_hash: ObjectHash::body(source.witness())?,
        page_envelope_hashes_hash: ObjectHash::body(&(
            "task-business-original-page-envelopes-v2",
            source.original_page_envelope_hashes(),
        ))?,
        accepted_at_ms: original.accepted_at_ms(),
        primary_heads: view.primary_heads().to_vec(),
        primary_pins,
    };
    let mut usage = view.proof_usage().clone();
    usage.merge(original.proof_usage());
    finish_usage(usage)?;
    immutable(
        connection,
        &key("accepted-source", &association.wire_hash),
        &association,
    )
}

fn check_source_association_in(
    connection: &Connection,
    primary: &super::super::journal::PolicyState,
    source: &VerifiedTaskBusinessOriginalSource,
    wire: &TaskBusinessWire,
    acceptance: &RemoteExecutionHistoryAcceptance,
) -> Result<()> {
    let association = association_in(connection, &wire.hash()?)?;
    ensure!(
        source.wire() == wire
            && source.wire_hash() == &association.wire_hash
            && ObjectHash::body(acceptance)? == association.acceptance_envelope_hash
            && source.acceptance_envelope_hash()? == association.acceptance_envelope_hash
            && source.header().hash()? == association.header_hash
            && source.header_envelope_hash()? == association.header_envelope_hash
            && source.witness_envelope_hash()? == association.witness_envelope_hash
            && ObjectHash::body(&(
                "task-business-original-page-envelopes-v2",
                source.original_page_envelope_hashes(),
            ))? == association.page_envelope_hashes_hash
            && source.accepted_at_ms() == association.accepted_at_ms
            && source.primary_heads() == association.primary_heads.as_slice(),
        "original Task business source differs from its durable acceptance association"
    );
    check_primary_in(connection, primary, &association)?;
    originals_in(connection, &association)?;
    Ok(())
}

pub(crate) fn check_task_business_original_source_in(
    connection: &Connection,
    primary: &super::super::journal::PolicyState,
    source: &VerifiedTaskBusinessOriginalSource,
    wire: &TaskBusinessWire,
    acceptance: &RemoteExecutionHistoryAcceptance,
) -> Result<()> {
    let _signatures = ScopedSignatureVerification::new();
    check_source_association_in(connection, primary, source, wire, acceptance)?;
    check_import_originals_in(
        connection,
        source.header(),
        source.witness(),
        source.original_page_envelope_hashes(),
    )?;
    finish_usage(source.proof_usage().clone())
}

pub(super) fn check_operation_originals_in(
    connection: &Connection,
    primary: &super::super::journal::PolicyState,
    operation: &VerifiedTaskBusinessOperationCurrent,
) -> Result<()> {
    let association = association_in(connection, operation.wire_hash())?;
    ensure!(
        operation.wire().hash()? == association.wire_hash
            && operation.acceptance_envelope_hash()? == association.acceptance_envelope_hash
            && operation.original_header_envelope_hash()? == association.header_envelope_hash
            && operation.original_witness_envelope_hash()? == association.witness_envelope_hash
            && ObjectHash::body(&(
                "task-business-original-page-envelopes-v2",
                operation.original_page_envelope_hashes(),
            ))? == association.page_envelope_hashes_hash
            && operation.accepted_at_ms() == association.accepted_at_ms,
        "Task operation changed its durable original acceptance or source"
    );
    check_primary_in(connection, primary, &association)?;
    let (header, witness) = originals_in(connection, &association)?;
    check_import_originals_in(
        connection,
        &header,
        &witness,
        operation.original_page_envelope_hashes(),
    )
}

impl Store {
    pub(crate) fn begin_task_business_original_recovery(
        &self,
        identity: &LocalIdentity,
        wire: &TaskBusinessWire,
        acceptance: &RemoteExecutionHistoryAcceptance,
        now: u64,
    ) -> Result<TaskBusinessOriginalRecovery> {
        let _signatures = ScopedSignatureVerification::new();
        let local_c = PublicKey::parse(&identity.public_key_hex()?)?;
        let tx = self.connection.unchecked_transaction()?;
        let association = association_in(&tx, &wire.hash()?)?;
        ensure!(
            ObjectHash::body(acceptance)? == association.acceptance_envelope_hash,
            "Task business recovery changed its original executor acceptance"
        );
        check_primary_in(&tx, &self.policy, &association)?;
        let (header, _) = originals_in(&tx, &association)?;
        let importer = begin_task_business_original_import(
            self.policy.verified(),
            wire,
            acceptance,
            &header,
            &local_c,
            now,
        )?;
        ensure!(
            importer.accepted_at_ms() == association.accepted_at_ms,
            "Task business recovery changed its signed acceptance clock"
        );
        finish_usage(importer.proof_usage().clone())?;
        tx.commit()?;
        Ok(TaskBusinessOriginalRecovery {
            association,
            importer,
            finished: false,
        })
    }

    pub(crate) fn advance_task_business_original_recovery(
        &self,
        context: &mut TaskBusinessOriginalRecovery,
    ) -> Result<Option<VerifiedTaskBusinessOriginalSource>> {
        let _signatures = ScopedSignatureVerification::new();
        ensure!(
            !context.finished,
            "Task business original recovery is already complete"
        );
        let tx = self.connection.unchecked_transaction()?;
        let association = association_in(&tx, &context.association.wire_hash)?;
        ensure!(
            association == context.association,
            "Task business recovery changed its original source association"
        );
        check_primary_in(&tx, &self.policy, &association)?;
        let (header, witness) = originals_in(&tx, &association)?;
        ensure!(
            context.importer.header() == &header,
            "Task business recovery changed its original signed header"
        );
        let mut staged = context.importer.clone();
        let index = staged.next_page_index();
        if index < witness.payload.page_count {
            let page: Signed<TaskBusinessPolicyPage> = read(
                &tx,
                &format!("{}:{index}", key("import-page", &association.header_hash)),
            )?
            .context("NeedPolicySync: Task business recovery original page is missing")?;
            staged.append_page(&page)?;
            finish_usage(staged.proof_usage().clone())?;
            tx.commit()?;
            context.importer = staged;
            return Ok(None);
        }
        ensure!(
            index == witness.payload.page_count,
            "Task business recovery advanced beyond its original witness"
        );
        let source = staged.finish_original(&witness)?;
        check_source_association_in(
            &tx,
            &self.policy,
            &source,
            source.wire(),
            source.acceptance(),
        )?;
        finish_usage(source.proof_usage().clone())?;
        tx.commit()?;
        context.finished = true;
        Ok(Some(source))
    }
}
