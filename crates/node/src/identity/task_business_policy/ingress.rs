use super::{storage::*, *};
use crate::{crypto::LocalIdentity, store::Store};
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, TransactionBehavior};
use vibeface_secure::{
    authorization::{
        MAX_PROOF_BYTES, PublicKey,
        execution_handoff::ExecutionProofUsage,
        node_execution_policy::ActualPolicyNoisePeer,
        task_business_policy::{
            TaskBusinessPolicyImport, TaskBusinessScopedView, begin_task_business_policy_import,
            task_business_primary_heads,
        },
    },
    signature_verification::{ScopedSignatureVerification, SignatureVerification},
};

pub(crate) struct TaskBusinessPolicyImportContext {
    importer: TaskBusinessPolicyImport,
    local_c: PublicKey,
    finished: bool,
}

#[derive(Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
struct CompletedImport {
    header_hash: ObjectHash,
    witness: Signed<TaskBusinessPolicyWitness>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ImportProgress {
    header_hash: ObjectHash,
    next_page_index: u64,
    ready_for_finish: bool,
}

fn require_header(
    connection: &rusqlite::Connection,
    header: &Signed<TaskBusinessPolicyHeader>,
) -> Result<ObjectHash> {
    let reference = header.hash()?;
    let saved: Signed<TaskBusinessPolicyHeader> =
        read(connection, &key("import-header", &reference))?
            .ok_or_else(|| anyhow::anyhow!("Task business import lost its original header"))?;
    ensure!(
        saved == *header,
        "Task business import changed its original header"
    );
    Ok(reference)
}

fn response(
    request: &TaskBusinessRequest,
    response: TaskBusinessResponse,
    mut usage: ExecutionProofUsage,
) -> Result<Value> {
    ensure!(
        serde_json::to_vec(request)?.len() + serde_json::to_vec(&response)?.len()
            <= MAX_PROOF_BYTES,
        "Task business policy input and response exceed 64 KiB"
    );
    usage.record_metadata(&response)?;
    if let Some(counts) = SignatureVerification::current_counts() {
        ensure!(
            counts.actual_verifications <= 32,
            "Task business policy exceeded its crypto budget"
        );
        usage.record_actual_signature_counts(counts);
    }
    usage.enforce_limits()?;
    Ok(serde_json::to_value(response)?)
}

fn check_current(
    connection: &rusqlite::Connection,
    policy: &super::super::journal::PolicyState,
    header: &Signed<TaskBusinessPolicyHeader>,
    local_c: &PublicKey,
    now: u64,
) -> Result<ExecutionProofUsage> {
    let (heads, usage) =
        task_business_primary_heads(policy.verified(), &header.payload.scope, local_c, now)?;
    super::super::client_policy_read::check_heads_in(connection, policy, &heads)?;
    Ok(usage)
}

pub(super) fn completed_witness_in(
    connection: &Connection,
    header: &Signed<TaskBusinessPolicyHeader>,
) -> Result<Signed<TaskBusinessPolicyWitness>> {
    let reference = require_header(connection, header)?;
    let complete: CompletedImport = read(connection, &key("import-complete", &reference))?
        .context("NeedPolicySync: Task business source has no durable completion")?;
    ensure!(
        complete.header_hash == reference && complete.witness.payload.header_hash == reference,
        "Task business source completion changed its original header"
    );
    let progress: ImportProgress = read(connection, &key("import-progress", &reference))?
        .context("NeedPolicySync: Task business source lost its durable final page")?;
    ensure!(
        progress.header_hash == reference
            && progress.ready_for_finish
            && progress.next_page_index == complete.witness.payload.page_count,
        "Task business source completion changed its original page cursor"
    );
    Ok(complete.witness)
}

pub(super) fn check_import_originals_in(
    connection: &Connection,
    header: &Signed<TaskBusinessPolicyHeader>,
    witness: &Signed<TaskBusinessPolicyWitness>,
    page_hashes: &[ObjectHash],
) -> Result<()> {
    let saved = completed_witness_in(connection, header)?;
    ensure!(
        saved == *witness && page_hashes.len() as u64 == witness.payload.page_count,
        "Task business source completion differs from its verified originals"
    );
    let reference = header.hash()?;
    for (index, expected) in page_hashes.iter().enumerate() {
        let page: Signed<TaskBusinessPolicyPage> = read(
            connection,
            &format!("{}:{index}", key("import-page", &reference)),
        )?
        .context("NeedPolicySync: Task business source original page is missing")?;
        ensure!(
            ObjectHash::body(&page)? == *expected,
            "Task business source original signed page changed"
        );
    }
    Ok(())
}

pub(crate) fn check_task_business_policy_view_in(
    connection: &Connection,
    primary: &super::super::journal::PolicyState,
    view: &TaskBusinessScopedView,
    wire: &TaskBusinessWire,
    now: u64,
) -> Result<()> {
    let _signatures = ScopedSignatureVerification::new();
    view.verify_wire(wire, now)?;
    let source = view.source();
    let local_c = &view.scope().target_public_key;
    let (heads, mut usage) =
        task_business_primary_heads(primary.verified(), view.scope(), local_c, now)?;
    ensure!(
        view.primary_heads() == heads.as_slice(),
        "NeedPolicySync: Task business view changed its current C policy heads"
    );
    super::super::client_policy_read::check_heads_in(connection, primary, &heads)?;
    source.verify_current_grant(primary.verified(), local_c, now)?;
    check_import_originals_in(
        connection,
        source.header(),
        source.witness(),
        source.original_page_envelope_hashes(),
    )?;
    source.check_saved_floors(&floors(connection, source.pins())?)?;
    usage.merge(view.proof_usage());
    if let Some(counts) = SignatureVerification::current_counts() {
        ensure!(
            counts.actual_verifications <= 32,
            "Task business source view exceeded its crypto budget"
        );
        usage.record_actual_signature_counts(counts);
    }
    usage.enforce_limits()?;
    Ok(())
}

impl Store {
    pub(crate) fn task_business_policy_view(
        &self,
        identity: &LocalIdentity,
        session: &TaskBusinessPolicySession,
        wire: &TaskBusinessWire,
        now: u64,
    ) -> Result<TaskBusinessScopedView> {
        let _signatures = ScopedSignatureVerification::new();
        let local_c = PublicKey::parse(&identity.public_key_hex()?)?;
        let tx = self.connection.unchecked_transaction()?;
        let view = session
            .source
            .scoped_view(self.policy.verified(), &local_c, wire, now)?;
        check_task_business_policy_view_in(&tx, &self.policy, &view, wire, now)?;
        tx.commit()?;
        Ok(view)
    }

    pub(crate) fn begin_task_business_import(
        &mut self,
        identity: &LocalIdentity,
        peer: &ActualPolicyNoisePeer,
        header: &Signed<TaskBusinessPolicyHeader>,
        now: u64,
    ) -> Result<(TaskBusinessPolicyImportContext, Value)> {
        let _signatures = ScopedSignatureVerification::new();
        let local_c = PublicKey::parse(&identity.public_key_hex()?)?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut usage = check_current(&tx, &self.policy, header, &local_c, now)?;
        // These roots come only from the already accepted local grant. The signed
        // source header supplies a stream binding, never an independent root.
        let approved = &self
            .policy
            .verified()
            .grant(&header.payload.scope.execution_grant)
            .ok_or_else(|| anyhow::anyhow!("missing current Task business grant"))?
            .payload
            .execution_source
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("current grant has no source policy"))?
            .pins;
        let selected_pins: Vec<_> = approved
            .iter()
            .filter(|pin| {
                header
                    .payload
                    .source_heads
                    .iter()
                    .any(|head| head.domain == pin.domain)
            })
            .cloned()
            .collect();
        let saved = floors(&tx, &selected_pins)?;
        let importer = begin_task_business_policy_import(
            self.policy.verified(),
            peer,
            header,
            &local_c,
            &saved,
            now,
        )?;
        usage.merge(importer.proof_usage());
        let context_hash = header.hash()?;
        let result = response(
            &TaskBusinessRequest::SourceBegin {
                header: header.clone(),
            },
            TaskBusinessResponse::SourceBegun {
                context_hash: context_hash.clone(),
            },
            usage,
        )?;
        immutable(&tx, &key("import-header", &context_hash), header)?;
        tx.commit()?;
        Ok((
            TaskBusinessPolicyImportContext {
                importer,
                local_c,
                finished: false,
            },
            result,
        ))
    }

    pub(crate) fn append_task_business_policy(
        &mut self,
        context: &mut TaskBusinessPolicyImportContext,
        page: &Signed<TaskBusinessPolicyPage>,
        now: u64,
    ) -> Result<Value> {
        let _signatures = ScopedSignatureVerification::new();
        ensure!(
            !context.finished,
            "Task business import is already complete"
        );
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut usage = check_current(
            &tx,
            &self.policy,
            context.importer.header(),
            &context.local_c,
            now,
        )?;
        require_header(&tx, context.importer.header())?;
        let mut staged = context.importer.clone();
        staged.append_page(page, now)?;
        usage.merge(staged.proof_usage());
        let context_hash = staged.header().hash()?;
        let result = response(
            &TaskBusinessRequest::SourcePage { page: page.clone() },
            TaskBusinessResponse::SourcePageAccepted {
                context_hash: context_hash.clone(),
                next_page_index: staged.next_page_index(),
                complete: page.payload.complete,
            },
            usage,
        )?;
        immutable(
            &tx,
            &format!(
                "{}:{}",
                key("import-page", &context_hash),
                page.payload.page_index
            ),
            page,
        )?;
        let progress_key = key("import-progress", &context_hash);
        let saved: Option<ImportProgress> = read(&tx, &progress_key)?;
        if let Some(saved) = &saved {
            ensure!(
                saved.header_hash == context_hash,
                "Task business progress changed its original header"
            );
        }
        if saved
            .as_ref()
            .is_none_or(|saved| saved.next_page_index < staged.next_page_index())
        {
            tx.execute("INSERT INTO identity_meta(key,value) VALUES(?,?) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
                rusqlite::params![progress_key,encoded(&ImportProgress {header_hash:context_hash,
                    next_page_index:staged.next_page_index(),ready_for_finish:page.payload.complete})?])?;
        }
        tx.commit()?;
        context.importer = staged;
        Ok(result)
    }

    pub(crate) fn finish_task_business_policy(
        &mut self,
        context: &mut TaskBusinessPolicyImportContext,
        witness: &Signed<TaskBusinessPolicyWitness>,
        now: u64,
    ) -> Result<(TaskBusinessPolicySession, Value)> {
        let _signatures = ScopedSignatureVerification::new();
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut usage = check_current(
            &tx,
            &self.policy,
            context.importer.header(),
            &context.local_c,
            now,
        )?;
        require_header(&tx, context.importer.header())?;
        let source = context.importer.clone().finish(witness, now)?;
        source.check_saved_floors(&floors(&tx, source.pins())?)?;
        usage.merge(source.proof_usage());
        let context_hash = source.header().hash()?;
        let progress: ImportProgress = read(&tx, &key("import-progress", &context_hash))?
            .ok_or_else(|| anyhow::anyhow!("Task business source has no durable final page"))?;
        ensure!(
            progress.header_hash == context_hash
                && progress.ready_for_finish
                && progress.next_page_index == witness.payload.page_count,
            "Task business completion differs from its durable page cursor"
        );
        let result = response(
            &TaskBusinessRequest::SourceFinish {
                witness: witness.clone(),
            },
            TaskBusinessResponse::SourceFinished {
                context_hash: context_hash.clone(),
            },
            usage,
        )?;
        immutable(
            &tx,
            &key("import-complete", &context_hash),
            &CompletedImport {
                header_hash: context_hash,
                witness: witness.clone(),
            },
        )?;
        advance_floors(&tx, &source)?;
        tx.commit()?;
        context.finished = true;
        Ok((TaskBusinessPolicySession { source }, result))
    }
}
