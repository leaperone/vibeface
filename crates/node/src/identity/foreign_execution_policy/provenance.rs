use super::ingress::{CompletionRecord, ImportRecord};
use super::*;
use std::collections::{BTreeMap, BTreeSet};
use vibeface_secure::authorization::{
    Invocation, PolicyDomain, PolicyStamp, RequestFacts, VerifiedAuthorization,
    node_execution_policy::{
        PolicyCarrierOrigin, SdkPolicySourceProvenance, TaskExecutionPolicyRequest,
        VerifiedTaskCurrentPolicy, VerifiedTaskHistoryPolicy, VerifiedTaskOriginalAdmissionPolicy,
        begin_task_policy_import, from_task_original_admission, task_policy_source_from_retained,
    },
    remote_execution_policy::{
        RemoteExecutionPolicyReadRequest, RemoteExecutionPolicySnapshot,
        RemoteExecutionPolicyWitness,
    },
};

#[derive(Clone, Default)]
pub(crate) struct TaskExecutionPolicyProvenance {
    pub(super) current: BTreeMap<ObjectHash, VerifiedTaskCurrentPolicy>,
    pub(super) history: BTreeMap<ObjectHash, VerifiedTaskHistoryPolicy>,
    by_work: BTreeMap<ObjectHash, BTreeSet<ObjectHash>>,
    original: BTreeMap<ObjectHash, CachedOriginal>,
    retained_floors: BTreeMap<String, vibeface_secure::authorization::PolicyPin>,
}

impl TaskExecutionPolicyProvenance {
    pub(super) fn work_key(policy: &VerifiedTaskCurrentPolicy) -> Result<ObjectHash> {
        Ok(ObjectHash::body(&work_approval_prefix(
            policy.request().source_project(),
            &policy.work(),
        )?)?)
    }

    pub(super) fn insert_current(
        &mut self,
        work_key: ObjectHash,
        reference: ObjectHash,
        policy: VerifiedTaskCurrentPolicy,
    ) {
        self.by_work
            .entry(work_key)
            .or_default()
            .insert(reference.clone());
        self.current.insert(reference, policy);
    }

    fn current_for_key(&self, key: ObjectHash) -> Result<Vec<&VerifiedTaskCurrentPolicy>> {
        self.by_work
            .get(&key)
            .into_iter()
            .flatten()
            .map(|reference| {
                self.current
                    .get(reference)
                    .context("execution policy index lost its genuine current provenance")
            })
            .collect()
    }

    pub(crate) fn current_for_work(
        &self,
        project: &ResourceRef,
        work: &ExecutionWorkIntent,
    ) -> Result<Vec<&VerifiedTaskCurrentPolicy>> {
        self.current_for_key(ObjectHash::body(&work_approval_prefix(project, work)?)?)
    }

    pub(super) fn current_for_task(
        &self,
        project: &ResourceRef,
        task: &vibeface_secure::authorization::EventRef,
    ) -> Result<Vec<&VerifiedTaskCurrentPolicy>> {
        self.current_for_key(ObjectHash::body(&task_approval_prefix(project, task)?)?)
    }
}

#[derive(Clone)]
struct CachedOriginal {
    policy: VerifiedTaskOriginalAdmissionPolicy,
    proof_hash: ObjectHash,
    policy_ref: ObjectHash,
}

pub(super) fn association_source(
    primary: &super::super::journal::PolicyState,
    association: &AdmissionAssociation,
) -> Result<PublicKey> {
    if let Some(original) = primary
        .task_execution_policy
        .original
        .get(&association.invocation_hash)
    {
        ensure!(
            original.policy_ref == association.policy_ref,
            "original Task policy association changed its completion"
        );
        return Ok(original.policy.request().source_node().clone());
    }
    Ok(primary
        .task_execution_policy
        .current
        .get(&association.policy_ref)
        .context("NeedPolicySync: original Task policy provenance is missing")?
        .request()
        .source_node()
        .clone())
}

fn import_reference(policy: &VerifiedTaskCurrentPolicy) -> Result<ObjectHash> {
    Ok(ObjectHash::body(&(
        "task-policy-import-v2",
        policy.trust().approval().hash()?,
        policy.request().request_hash()?,
        ObjectHash::body(policy.snapshot())?,
    ))?)
}

fn policy_reference(policy: &VerifiedTaskCurrentPolicy) -> Result<ObjectHash> {
    Ok(ObjectHash::body(&(
        "task-policy-completion-v2",
        import_reference(policy)?,
        ObjectHash::body(policy.witness())?,
    ))?)
}

fn completion_in(
    connection: &Connection,
    reference: &ObjectHash,
) -> Result<(CompletionRecord, ImportRecord)> {
    let complete: CompletionRecord = read_in(connection, &key("completion", reference))?
        .context("NeedPolicySync: completed Task policy is missing")?;
    ensure!(
        ObjectHash::body(&(
            "task-policy-completion-v2",
            &complete.import_ref,
            &complete.witness_ref
        ))? == *reference,
        "foreign policy completion reference changed"
    );
    let imported: ImportRecord = read_in(connection, &key("import", &complete.import_ref))?
        .context("NeedPolicySync: Task policy import originals are missing")?;
    ensure!(
        ObjectHash::body(&(
            "task-policy-import-v2",
            &imported.approval_ref,
            &imported.request_ref,
            &imported.snapshot_ref
        ))? == complete.import_ref
            && imported.ready_for_finish,
        "foreign import reference changed"
    );
    Ok((complete, imported))
}

fn check_originals_in(connection: &Connection, policy: &VerifiedTaskCurrentPolicy) -> Result<()> {
    check_saved_carriers_in(
        connection,
        policy.request(),
        policy.snapshot(),
        policy.witness(),
        policy.trust(),
        policy.original_page_envelope_hashes(),
    )
}

fn check_saved_carriers_in(
    connection: &Connection,
    expected_request: &vibeface_secure::authorization::node_execution_policy::TaskExecutionPolicyRequest,
    expected_snapshot: &Signed<RemoteExecutionPolicySnapshot>,
    expected_witness: &Signed<RemoteExecutionPolicyWitness>,
    trust: &VerifiedTaskExecutionTrust,
    page_hashes: &[ObjectHash],
) -> Result<()> {
    let import_ref = ObjectHash::body(&(
        "task-policy-import-v2",
        trust.approval().hash()?,
        expected_request.request_hash()?,
        ObjectHash::body(expected_snapshot)?,
    ))?;
    let reference = ObjectHash::body(&(
        "task-policy-completion-v2",
        import_ref,
        ObjectHash::body(expected_witness)?,
    ))?;
    let (complete, imported) = completion_in(connection, &reference)?;
    let request: Box<TaskExecutionPolicyRequest> =
        read_in(connection, &key("request", &imported.request_ref))?
            .context("foreign policy original request is missing")?;
    let snapshot: Box<Signed<RemoteExecutionPolicySnapshot>> =
        read_in(connection, &key("snapshot", &imported.snapshot_ref))?
            .context("foreign policy original snapshot is missing")?;
    let witness: Box<Signed<RemoteExecutionPolicyWitness>> =
        read_in(connection, &key("witness", &complete.witness_ref))?
            .context("foreign policy original witness is missing")?;
    ensure!(
        expected_request == request.as_ref()
            && request.request_hash()? == imported.request_ref
            && expected_snapshot == snapshot.as_ref()
            && expected_witness == witness.as_ref()
            && imported.approval_ref == trust.approval().hash()?,
        "foreign policy original carrier differs from its verified provenance"
    );
    if let Some(node) = request.node_request() {
        let reference = node
            .source_provenance()
            .context("Node policy has no signed SDK source provenance")?;
        let raw: Box<SdkPolicySourceProvenance> =
            read_in(connection, &key("sdk-source", reference))?
                .context("Node policy original SDK source is missing")?;
        ensure!(
            ObjectHash::body(raw.as_ref())? == *reference,
            "Node policy original SDK source changed"
        );
    }
    ensure!(
        page_hashes.len() as u64 == imported.page_count,
        "foreign policy page count differs from its verified provenance"
    );
    for (index, expected) in page_hashes.iter().enumerate() {
        let page: Box<
            Signed<
                vibeface_secure::authorization::remote_execution_policy::RemoteExecutionPolicyPage,
            >,
        > = read_in(
            connection,
            &ingress::page_key(&complete.import_ref, index as u64),
        )?
        .context("foreign policy original page is missing")?;
        ensure!(
            ObjectHash::body(page.as_ref())? == *expected,
            "foreign policy original signed page changed"
        );
    }
    let original: Box<Signed<RemoteExecutionPolicyReadIntent>> =
        read_in(connection, &key("approved-intent", &imported.approval_ref))?
            .context("foreign approval original intent is missing")?;
    let approval: Box<Signed<TaskExecutionTrustApproval>> =
        read_in(connection, &key("approval", &imported.approval_ref))?
            .context("foreign approval original signature is missing")?;
    ensure!(
        trust.original_intent() == original.as_ref() && trust.approval() == approval.as_ref(),
        "foreign policy approval differs from its verified provenance"
    );
    Ok(())
}

pub(crate) fn check_task_policy_heads_in(
    connection: &Connection,
    policy: &VerifiedTaskCurrentPolicy,
) -> Result<()> {
    check_originals_in(connection, policy)?;
    for head in policy.primary_heads() {
        let (kind, id) = match &head.domain {
            PolicyDomain::Account(id) => ("account", id.as_str()),
            PolicyDomain::Space(id) => ("space", id.as_str()),
        };
        let current: Option<(u64,u64,String)> = connection.query_row(
            "SELECT authority_epoch,sequence,head_hash FROM current_policy_heads WHERE domain_kind=? AND domain_id=?",
            params![kind,id],|row|Ok((row.get(0)?,row.get(1)?,row.get(2)?)),
        ).optional()?;
        ensure!(
            current == Some((head.authority_epoch, head.sequence, head.head.to_string())),
            "NeedPolicySync: primary Task policy changed"
        );
        let root: Option<String> = connection
            .query_row(
                "SELECT genesis_root FROM policy_pins WHERE domain_kind=? AND domain_id=?",
                params![kind, id],
                |row| row.get(0),
            )
            .optional()?;
        let pin = policy
            .pins()
            .iter()
            .find(|pin| pin.domain == head.domain)
            .context("primary Task policy has no pin")?;
        ensure!(
            root.as_deref() == Some(pin.genesis_root.as_str()),
            "primary Task policy genesis changed"
        );
    }
    for floor in ingress::floors_in(connection, policy.pins())? {
        if policy
            .primary_heads()
            .iter()
            .any(|head| head.domain == floor.domain)
        {
            continue;
        }
        let head = policy
            .foreign_heads()
            .iter()
            .find(|head| head.domain == floor.domain)
            .context("foreign Task policy omitted a verified domain")?;
        if let Some(minimum) = floor.minimum_head {
            ensure!(
                (head.authority_epoch, head.sequence)
                    >= (minimum.authority_epoch, minimum.sequence)
                    && ((head.authority_epoch, head.sequence)
                        != (minimum.authority_epoch, minimum.sequence)
                        || *head == minimum),
                "NeedPolicySync: foreign Task policy is below the verified floor"
            );
        }
    }
    Ok(())
}

fn remote_body(body: &Value) -> bool {
    ["execution_resource", "target_public_key", "execution_grant"]
        .iter()
        .any(|field| body.get(*field).is_some())
}

pub(crate) fn task_policy_for_binding_in(
    connection: &Connection,
    primary: &super::super::journal::PolicyState,
    source: &PublicKey,
    binding: &SignedBinding,
    project: &vibeface_secure::authorization::ResourceRef,
    selection: &vibeface_secure::authorization::remote_execution_policy::RemoteExecutionPolicySelection,
    action: Action,
    now: u64,
) -> Result<VerifiedTaskCurrentPolicy> {
    let hash = crate::binding::binding_hash(&binding.binding)?;
    ensure!(
        binding.binding.network_id == project.space.as_str()
            && retained_binding_in(connection, project.space.as_str(), &hash, source)? == *binding,
        "Task policy lookup changed its exact persisted binding"
    );
    let current: String = connection.query_row(
        "SELECT value FROM local_values WHERE space_id=? AND key=?",
        params![
            project.space.as_str(),
            format!("task-binding-current:{}", binding.binding.task_ref)
        ],
        |row| row.get(0),
    )?;
    ensure!(
        serde_json::from_str::<String>(&current)? == hash,
        "Task policy lookup requires the actual current binding"
    );
    let encoded: String = connection.query_row("SELECT envelope FROM events WHERE space_id=? AND hash=? AND resource_id=? AND status='ready' AND length(CAST(envelope AS BLOB))<=?",
        params![project.space.as_str(),binding.binding.task_ref,project.resource.as_str(),crate::model::MAX_EVENT_BYTES*6+1024],|row|row.get(0))?;
    let task: crate::model::SignedEvent = serde_json::from_str(&encoded)?;
    let event = crate::crypto::verify_signed_event(project.space.as_str(), &task, source.as_str())?;
    ensure!(
        task.hash == binding.binding.task_ref
            && event.scope_id == binding.binding.conversation_id
            && matches!(&event.body,crate::model::EventBody::TaskCreated{task_id,..} if task_id==&binding.binding.task_id),
        "Task policy lookup changed its original ready Task"
    );
    super::super::event_authorization::validate_retained_event_dependencies_in(connection, &task)?;
    let mut usage = ExecutionProofUsage::default();
    usage.record_retained_event(&task)?;
    usage.record_binding(binding)?;
    let association = binding_association_in(connection, primary, binding, &mut usage)?;
    let mut selected: Option<VerifiedTaskCurrentPolicy> = None;
    let task_ref = vibeface_secure::authorization::EventRef::parse(&binding.binding.task_ref)?;
    for policy in primary
        .task_execution_policy
        .current_for_task(project, &task_ref)?
    {
        let context = policy.context();
        if policy.request().source_node() != source
            || policy.request().source_project() != project
            || !matches!(
                policy.request().purpose(),
                RemoteExecutionPolicyPurpose::Execution | RemoteExecutionPolicyPurpose::Resolver
            )
            || policy.request().expected_action() != action
            || policy.selection() != selection
            || policy.trust().approval().hash()? != association.approval_ref
            || context.binding_generation != Some(binding.binding.generation)
            || context.context.task.as_ref().map(|task| task.as_str())
                != Some(binding.binding.task_ref.as_str())
            || context.context.conversation.as_deref()
                != Some(binding.binding.conversation_id.as_str())
            || context
                .context
                .execution_session
                .as_ref()
                .map(|session| session.session_id.as_str())
                != Some(binding.binding.session_id.as_str())
            || context.context.executor_kind.as_deref()
                != binding.binding.open_spec["executor"].as_str()
            || context.canonical_cwd.as_deref() != binding.binding.open_spec["cwd"].as_str()
            || now >= policy.expires_at_ms()
            || now < policy.snapshot().payload.issued_at_ms
            || now < policy.witness().payload.issued_at_ms
        {
            continue;
        }
        if check_task_policy_heads_in(connection, policy).is_err() {
            continue;
        }
        if selected.as_ref().is_none_or(|prior| {
            policy.snapshot().payload.issued_at_ms > prior.snapshot().payload.issued_at_ms
        }) {
            selected = Some(policy.clone());
        }
    }
    let selected = selected.context(
        "NeedPolicySync: current binding has no completed foreign policy for this exact action",
    )?;
    usage.merge(selected.proof_usage());
    usage.enforce_limits()?;
    Ok(selected)
}

fn facts(invocation: &Signed<Invocation>, body: &Value) -> Result<RequestFacts> {
    let value = &invocation.payload;
    ensure!(
        value.body_hash == ObjectHash::body(body)?
            && crate::model::authorization_action(body)? == value.action,
        "foreign Task policy body differs from the original invocation"
    );
    Ok(RequestFacts {
        resource: value.resource.clone(),
        action: value.action,
        audience: value.audience.clone(),
        operation_id: value.operation_id.clone(),
        body_hash: value.body_hash.clone(),
        context: value.context.clone(),
    })
}

pub(crate) fn task_policy_candidates_in(
    connection: &Connection,
    primary: &super::super::journal::PolicyState,
    source: &PublicKey,
    project: &vibeface_secure::authorization::ResourceRef,
    task: &vibeface_secure::authorization::EventRef,
    conversation: &str,
    executor: &str,
    action: Action,
    binding: Option<&SignedBinding>,
    now: u64,
) -> Result<Vec<VerifiedTaskCurrentPolicy>> {
    if let Some(binding) = binding {
        ensure!(
            binding.binding.task_ref == task.as_str()
                && binding.binding.network_id == project.space.as_str()
                && binding.binding.conversation_id == conversation
                && binding.binding.open_spec["executor"].as_str() == Some(executor),
            "Task candidates changed the actual binding scope"
        );
        let mut usage = ExecutionProofUsage::default();
        let association = binding_association_in(connection, primary, binding, &mut usage)?;
        let original_source = association_source(primary, &association)?;
        ensure!(
            original_source == *source,
            "Task candidates changed the actual source Node"
        );
        let proof = super::super::admission::load_admission_in(
            connection,
            project.space.as_str(),
            &association.authorization_id,
        )?;
        let original = original_task_execution_policy_in(connection, primary, source, &proof)?
            .context("Task candidates have no original approved writer")?;
        let policy = task_policy_for_binding_in(
            connection,
            primary,
            source,
            binding,
            project,
            original.trust().selection(),
            action,
            now,
        )?;
        return Ok(vec![policy]);
    }
    let current: Option<String> = connection
        .query_row(
            "SELECT value FROM local_values WHERE space_id=? AND key=?",
            params![
                project.space.as_str(),
                format!("task-binding-current:{task}")
            ],
            |row| row.get(0),
        )
        .optional()?;
    ensure!(
        current.is_none(),
        "Task candidates omitted the actual current binding"
    );
    let mut candidates: BTreeMap<ObjectHash, VerifiedTaskCurrentPolicy> = BTreeMap::new();
    for policy in primary
        .task_execution_policy
        .current_for_task(project, task)?
    {
        let context = policy.context();
        if policy.request().source_node() != source
            || policy.request().source_project() != project
            || policy.request().purpose() != RemoteExecutionPolicyPurpose::Execution
            || policy.request().expected_action() != Action::ExecutionOpen
            || action != Action::ExecutionOpen
            || context.binding_generation != Some(1)
            || context.context.conversation.as_deref() != Some(conversation)
            || context.context.executor_kind.as_deref() != Some(executor)
            || now >= policy.expires_at_ms()
            || now < policy.snapshot().payload.issued_at_ms
            || now < policy.witness().payload.issued_at_ms
        {
            continue;
        }
        if check_task_policy_heads_in(connection, policy).is_err() {
            continue;
        }
        let mut usage = policy.proof_usage().clone();
        check_current_anchor_in(connection, primary, source, policy, &mut usage, now)?;
        usage.enforce_limits()?;
        let selection = ObjectHash::body(policy.selection())?;
        if candidates.get(&selection).is_none_or(|prior| {
            policy.snapshot().payload.issued_at_ms > prior.snapshot().payload.issued_at_ms
        }) {
            candidates.insert(selection, policy.clone());
        }
    }
    if candidates.is_empty() {
        let prefix = task_approval_prefix(project, task)?;
        let stored: Option<String> = connection
            .query_row(
                "SELECT value FROM identity_meta WHERE key GLOB ? ORDER BY key LIMIT 1",
                [format!("{prefix}*")],
                |row| row.get(0),
            )
            .optional()?;
        if let Some(encoded) = stored {
            ensure!(
                encoded.len() <= MAX_PROOF_BYTES,
                "Task approval lookup is too large"
            );
            let reference: ObjectHash = serde_json::from_str(&encoded)?;
            let trust = load_task_execution_trust_in(connection, source, &reference)?;
            let original = &trust.original_intent().payload;
            ensure!(
                original.source_selection.resource == *project
                    && original.context.context.task.as_ref() == Some(task)
                    && original.context.binding_generation == Some(1),
                "Task approval lookup changed its exact unbound scope"
            );
            anyhow::bail!(
                "NeedPolicySync: approved foreign Task has no fresh policy for this exact action"
            );
        }
    }
    Ok(candidates.into_values().collect())
}

pub(crate) fn execution_policy_candidates_in(
    connection: &Connection,
    primary: &super::super::journal::PolicyState,
    source: &PublicKey,
    project: &ResourceRef,
    work: &ExecutionWorkIntent,
    action: Action,
    now: u64,
) -> Result<Vec<VerifiedTaskCurrentPolicy>> {
    ensure!(
        !matches!(work, ExecutionWorkIntent::TaskSession { .. }),
        "Task target discovery requires its actual Task creation and binding"
    );
    let mut selected: BTreeMap<ObjectHash, VerifiedTaskCurrentPolicy> = BTreeMap::new();
    for policy in primary
        .task_execution_policy
        .current_for_work(project, work)?
    {
        if policy.request().source_node() != source
            || policy.request().source_project() != project
            || policy.request().purpose() != RemoteExecutionPolicyPurpose::Execution
            || policy.request().expected_action() != action
            || policy.work() != *work
            || now >= policy.expires_at_ms()
            || now < policy.snapshot().payload.issued_at_ms
            || now < policy.witness().payload.issued_at_ms
        {
            continue;
        }
        if check_task_policy_heads_in(connection, policy).is_err() {
            continue;
        }
        let mut usage = policy.proof_usage().clone();
        check_current_anchor_in(connection, primary, source, policy, &mut usage, now)?;
        usage.enforce_limits()?;
        let reference = ObjectHash::body(policy.selection())?;
        if selected.get(&reference).is_none_or(|previous| {
            policy.snapshot().payload.issued_at_ms > previous.snapshot().payload.issued_at_ms
        }) {
            selected.insert(reference, policy.clone());
        }
    }
    Ok(selected.into_values().collect())
}

pub(crate) fn current_task_prepare_policy_in(
    connection: &Connection,
    primary: &super::super::journal::PolicyState,
    source: &PublicKey,
    prepare: &Signed<vibeface_secure::authorization::prepare::PrepareIntent>,
    body: &Value,
    now: u64,
) -> Result<Option<VerifiedTaskCurrentPolicy>> {
    if !remote_body(body) {
        return Ok(None);
    }
    let mut selected: Option<VerifiedTaskCurrentPolicy> = None;
    for policy in primary.task_execution_policy.current.values() {
        if policy.request().source_node() != source
            || policy.request().source_project() != &prepare.payload.request.resource
        {
            continue;
        }
        if policy.check_prepare_scope(prepare, body, now).is_err() {
            continue;
        }
        if check_task_policy_heads_in(connection, policy).is_err() {
            continue;
        }
        let mut usage = policy.proof_usage().clone();
        check_current_anchor_in(connection, primary, source, policy, &mut usage, now)?;
        usage.record_signed(prepare)?;
        if let Some(counts) = SignatureVerification::current_counts() {
            usage.record_actual_signature_counts(counts);
        }
        usage.enforce_limits()?;
        if selected.as_ref().is_none_or(|prior| {
            (
                policy.snapshot().payload.issued_at_ms,
                policy.witness().payload.issued_at_ms,
            ) > (
                prior.snapshot().payload.issued_at_ms,
                prior.witness().payload.issued_at_ms,
            )
        }) {
            selected = Some(policy.clone());
        }
    }
    Ok(Some(selected.context(
        "NeedPolicySync: this exact prepare has no current completed foreign policy",
    )?))
}

pub(crate) fn check_local_original_source_in(
    connection: &Connection,
    primary: &super::super::journal::PolicyState,
    source: &PublicKey,
    original: &vibeface_secure::authorization::execution_handoff::SignedExecutionHandoff,
    usage: &mut ExecutionProofUsage,
) -> Result<vibeface_secure::authorization::execution_handoff::ProjectExecutionSource> {
    let statement = original.handoff.source_statement()?;
    let encoded: String = connection.query_row(
        "SELECT value FROM local_values WHERE space_id=? AND key=?",
        params![
            statement.project.space.as_str(),
            format!("project-execution-handoff:{}", statement.operation_id)
        ],
        |row| row.get(0),
    )?;
    ensure!(
        serde_json::from_str::<
            vibeface_secure::authorization::execution_handoff::SignedExecutionHandoff,
        >(&encoded)?
            == *original,
        "readonly original differs from the exact durable Project handoff"
    );
    let proof = super::super::admission::admission_for_operation_from(
        connection,
        statement.project.space.as_str(),
        &statement.operation_id,
    )?;
    let dispatch: crate::model::Event =
        serde_json::from_str(&original.handoff.project_dispatch.canonical_body)?;
    ensure!(
        matches!(&dispatch.cause, crate::model::EventCause::Invocation { admission } if admission == &proof),
        "readonly original differs from its exact accepted admission"
    );
    let historical = super::super::task_execution_context::OriginalAdmissionView::load(
        connection, primary, &proof,
    )?;
    let verified = historical
        .state()
        .verify_execution_observation_source(&original.handoff, proof.admitted_at_ms)?;
    ensure!(
        verified.capsule_hash() == &statement.capsule_hash
            && original
                .handoff
                .source_attestation
                .as_ref()
                .is_some_and(|signed| signed.issuer == *source),
        "readonly original changed its verified Project source"
    );
    if let Some(policy) = historical.task_policy() {
        usage.record_signed(policy.trust().original_intent())?;
        usage.record_signed(policy.trust().approval())?;
        usage.record_metadata(&(
            "execution-original-policy-reference-v1",
            policy.proof_hash(),
        ))?;
    }
    usage.record_signed(
        original
            .handoff
            .source_attestation
            .as_ref()
            .context("original source attestation is missing")?,
    )?;
    usage.record_metadata(&(
        "execution-original-reference-v1",
        original.hash()?,
        ObjectHash::body(&proof)?,
        &statement.work,
    ))?;
    Ok(statement)
}

fn check_conversation_original_in(
    connection: &Connection,
    primary: &super::super::journal::PolicyState,
    source: &PublicKey,
    project: &ResourceRef,
    work: &ExecutionWorkIntent,
    usage: &mut ExecutionProofUsage,
) -> Result<()> {
    let ExecutionWorkIntent::ConversationSession {
        open: ConversationOpenIntent::Existing { open },
        ..
    } = work
    else {
        return Ok(());
    };
    let encoded: String = connection.query_row(
        "SELECT result FROM operations WHERE space_id=? AND operation_id=? AND json_extract(request,'$.action.type')='open' AND json_extract(request,'$.task_id') IS NULL",
        params![project.space.as_str(), open.operation_id], |row| row.get(0),
    )?;
    let accepted: vibeface_secure::authorization::execution_handoff::ExecutionHandoffResult =
        serde_json::from_str(&encoded)?;
    vibeface_secure::authorization::execution_handoff::verify_execution_handoff_receipt_signature(
        &accepted.handoff,
        &accepted.receipt,
        &accepted.result,
    )?;
    let statement =
        check_local_original_source_in(connection, primary, source, &accepted.handoff, usage)?;
    ensure!(
        matches!(
            accepted.handoff.handoff.origin,
            vibeface_secure::authorization::execution_handoff::ExecutionOrigin::ConversationOpen
        ) && accepted.receipt.payload.state
            == vibeface_secure::authorization::execution_handoff::ExecutionHandoffState::Succeeded
            && statement.project == *project
            && statement.work.intent() == *work,
        "current conversation work changed its successful original Open"
    );
    usage.record_metadata(&("conversation-open-result-v1", ObjectHash::body(&accepted)?))?;
    Ok(())
}

pub(crate) fn check_current_anchor_in(
    connection: &Connection,
    primary: &super::super::journal::PolicyState,
    source: &PublicKey,
    policy: &VerifiedTaskCurrentPolicy,
    usage: &mut ExecutionProofUsage,
    now: u64,
) -> Result<()> {
    let work = policy.work();
    if !matches!(work, ExecutionWorkIntent::TaskSession { .. }) {
        let approved = approved_execution_trust_in(
            connection,
            source,
            policy.request().source_project(),
            &work,
            policy.selection(),
            policy.context(),
            policy.request().purpose(),
        )?;
        ensure!(
            approved.approval().hash()? == policy.trust().approval().hash()?,
            "current work changed its independent destination approval"
        );
        check_conversation_original_in(
            connection,
            primary,
            source,
            policy.request().source_project(),
            &work,
            usage,
        )?;
        usage.record_signed(approved.original_intent())?;
        usage.record_signed(approved.approval())?;
    }
    if let Some(node) = policy.request().node_request() {
        ensure!(
            node.issuer() == source,
            "Node policy current source changed"
        );
        let reference = node
            .source_provenance()
            .context("SDK Node read has no signed source provenance")?;
        let raw: SdkPolicySourceProvenance = read_in(connection, &key("sdk-source", reference))?
            .context("SDK Node read original source is missing")?;
        ensure!(
            ObjectHash::body(&raw)? == *reference,
            "SDK Node read original source changed"
        );
        let current = super::super::admission::sdk_policy_source_provenance_in(
            connection,
            primary,
            source,
            &raw.session.service,
            &raw.service_grant,
            &raw.child_facts.request_facts(),
            now,
        )?;
        ensure!(
            current == raw,
            "NeedPolicySync: SDK Node reader source, lease or binding changed"
        );
        if let Some(binding) = raw.current_binding.as_ref() {
            let association = binding_association_in(connection, primary, binding, usage)?;
            ensure!(
                association.approval_ref == policy.trust().approval().hash()?,
                "SDK Node reader changed its actual Task approval"
            );
        } else if matches!(work, ExecutionWorkIntent::TaskSession { .. }) {
            ensure!(
                node.expected_action() == Action::ExecutionOpen
                    && policy.context().binding_generation == Some(1)
                    && policy.trust().original_binding().is_none(),
                "SDK Node reader changed its initial Task choice"
            );
        }
        usage.record_metadata(&("sdk-source-reference-v2", reference))?;
        return Ok(());
    }
    let reader = policy
        .request()
        .account_request()
        .context("unsupported Task policy reader")?;
    if !matches!(work, ExecutionWorkIntent::TaskSession { .. }) {
        check_read_trust(&reader.intent, policy.trust())?;
        return Ok(());
    }
    let binding = checked_source_task_binding_in(connection, &reader.intent, source, usage)?;
    let candidate = reader.intent.payload.purpose == RemoteExecutionPolicyPurpose::Execution
        && ((binding.is_none() && reader.intent.payload.expected_action == Action::ExecutionOpen)
            || (binding.is_some()
                && reader.intent.payload.expected_action == Action::ExecutionRebind));
    if candidate {
        check_candidate_anchor(policy.trust(), &reader.intent, binding.as_ref())?;
    } else {
        let association = binding_association_in(
            connection,
            primary,
            binding.as_ref().context("current policy has no binding")?,
            usage,
        )?;
        ensure!(
            association.approval_ref == policy.trust().approval().hash()?,
            "current policy changed its actual binding approval"
        );
        check_read_trust(&reader.intent, policy.trust())?;
    }
    Ok(())
}

pub(crate) fn current_task_execution_policy_in(
    connection: &Connection,
    primary: &super::super::journal::PolicyState,
    identity: &LocalIdentity,
    invocation: &Signed<Invocation>,
    body: &Value,
    now: u64,
) -> Result<Option<VerifiedTaskCurrentPolicy>> {
    if !remote_body(body) {
        return Ok(None);
    }
    let local = PublicKey::parse(&identity.public_key_hex()?)?;
    let actual = facts(invocation, body)?;
    let mut selected = None;
    for policy in primary.task_execution_policy.current.values() {
        let reference = policy_reference(policy)?;
        if policy.request().source_node() != &local
            || policy.request().source_project() != &actual.resource
        {
            continue;
        }
        if policy
            .check_body_scope(invocation, &actual, body, now)
            .is_err()
        {
            continue;
        }
        if check_task_policy_heads_in(connection, policy).is_err() {
            continue;
        }
        let mut usage = policy.proof_usage().clone();
        check_current_anchor_in(connection, primary, &local, policy, &mut usage, now)?;
        if let Some(counts) = SignatureVerification::current_counts() {
            usage.record_actual_signature_counts(counts);
        }
        usage.enforce_limits()?;
        if selected
            .as_ref()
            .is_none_or(|(_, prior): &(ObjectHash, VerifiedTaskCurrentPolicy)| {
                (
                    policy.snapshot().payload.issued_at_ms,
                    policy.witness().payload.issued_at_ms,
                ) > (
                    prior.snapshot().payload.issued_at_ms,
                    prior.witness().payload.issued_at_ms,
                )
            })
        {
            selected = Some((reference.clone(), policy.clone()));
        }
    }
    Ok(Some(
        selected
            .context("NeedPolicySync: this exact Task has no current completed foreign policy")?
            .1,
    ))
}

pub(crate) fn original_task_execution_policy_in(
    connection: &Connection,
    primary: &super::super::journal::PolicyState,
    source: &PublicKey,
    proof: &crate::model::AdmissionProof,
) -> Result<Option<VerifiedTaskOriginalAdmissionPolicy>> {
    if !remote_body(&proof.source_body) {
        return Ok(None);
    }
    let reference = proof.invocation.hash()?;
    let association: AdmissionAssociation = read_in(connection, &key("admission", &reference))?
        .context("NeedPolicySync: original Task policy admission association is missing")?;
    ensure!(
        association.invocation_hash == reference
            && association.authorization_id == proof.invocation.payload.operation_id
            && association.body_hash == ObjectHash::body(&proof.source_body)?
            && association.admitted_at_ms == proof.admitted_at_ms,
        "original Task policy association changed its accepted admission"
    );
    let saved = super::super::admission::load_admission_in(
        connection,
        proof.invocation.payload.resource.space.as_str(),
        &association.authorization_id,
    )?;
    ensure!(
        saved == *proof,
        "original Task policy differs from its exact persisted admission"
    );
    if let Some(cached) = primary.task_execution_policy.original.get(&reference) {
        let original = &cached.policy;
        ensure!(
            cached.policy_ref == association.policy_ref
                && cached.proof_hash == ObjectHash::body(proof)?
                && original.proof_hash() == &cached.proof_hash
                && original.request().source_node() == source
                && original.trust().approval().hash()? == association.approval_ref,
            "original Task policy changed its exact accepted proof, source or completion"
        );
        check_saved_carriers_in(
            connection,
            original.request(),
            original.snapshot(),
            original.witness(),
            original.trust(),
            original.original_page_envelope_hashes(),
        )?;
        check_original_work_in(connection, source, proof, &association, original.request())?;
        return Ok(Some(original.clone()));
    }
    let policy = primary
        .task_execution_policy
        .current
        .get(&association.policy_ref)
        .context("NeedPolicySync: original foreign policy provenance must be rehydrated")?;
    ensure!(
        policy.request().source_node() == source
            && policy.trust().approval().hash()? == association.approval_ref,
        "original Task policy belongs to another source or approval"
    );
    check_originals_in(connection, policy)?;
    check_original_work_in(connection, source, proof, &association, policy.request())?;
    Ok(Some(from_task_original_admission(policy.clone(), proof)?))
}

fn check_original_work_in(
    connection: &Connection,
    source: &PublicKey,
    proof: &crate::model::AdmissionProof,
    association: &AdmissionAssociation,
    request: &TaskExecutionPolicyRequest,
) -> Result<()> {
    let work = request.work();
    ensure!(
        association.work == work,
        "original policy changed its admitted work"
    );
    work.validate_context(&request.context()?, &request.selection()?.target_public_key)?;
    match &work {
        ExecutionWorkIntent::TaskSession {
            task,
            binding_generation,
            execution_session,
        } => {
            let hash = association
                .binding_hash
                .as_deref()
                .context("original Task policy has no retained binding hash")?;
            let binding = retained_binding_in(
                connection,
                proof.invocation.payload.resource.space.as_str(),
                hash,
                source,
            )?;
            ensure!(
                binding.binding.task_ref == task.as_str()
                    && binding.binding.generation == *binding_generation
                    && binding.binding.session_id == execution_session.session_id
                    && proof.invocation.payload.context.task.as_ref() == Some(task)
                    && proof.invocation.payload.context.execution_session.as_ref()
                        == Some(execution_session),
                "original Task policy changed its retained binding"
            );
        }
        _ => {
            ensure!(
                association.binding_hash.is_none(),
                "non-Task admission retained a Task binding"
            );
            if proof.source_body["method"] == "operate" {
                work.validate_request(&proof.invocation.payload, &proof.source_body)?;
            }
        }
    }
    Ok(())
}

pub(crate) fn bind_task_execution_policy_in(
    connection: &Connection,
    policy: &VerifiedTaskCurrentPolicy,
    authorization: &VerifiedAuthorization,
    body: &Value,
    new_binding: Option<&SignedBinding>,
    admitted_at: u64,
) -> Result<()> {
    check_task_policy_heads_in(connection, policy)?;
    let invocation = authorization.signed_invocation();
    policy.check_body_scope(invocation, &facts(invocation, body)?, body, admitted_at)?;
    let saved = super::super::admission::load_admission_in(
        connection,
        authorization.resource().space.as_str(),
        authorization.operation_id(),
    )?;
    ensure!(
        saved.invocation == *invocation
            && saved.source_body == *body
            && saved.admitted_at_ms == admitted_at,
        "Task policy association differs from its actual accepted admission"
    );
    let work = policy.work();
    if !matches!(work, ExecutionWorkIntent::TaskSession { .. }) {
        ensure!(
            new_binding.is_none(),
            "non-Task policy cannot create a Task binding"
        );
        work.validate_context(policy.context(), &policy.selection().target_public_key)?;
        let association = AdmissionAssociation {
            policy_ref: policy_reference(policy)?,
            approval_ref: policy.trust().approval().hash()?,
            invocation_hash: invocation.hash()?,
            authorization_id: authorization.operation_id().to_owned(),
            body_hash: authorization.body_hash().clone(),
            work,
            binding_hash: None,
            admitted_at_ms: admitted_at,
        };
        return save_exact_in(
            connection,
            &key("admission", &association.invocation_hash),
            &association,
        );
    }
    let binding: SignedBinding = match new_binding {
        Some(binding) => {
            ensure!(
                matches!(
                    body["method"].as_str(),
                    Some("bind_execution" | "rebind_execution")
                ),
                "only bind or rebind may create a Task policy binding association"
            );
            binding.clone()
        }
        None => {
            ensure!(
                !matches!(
                    body["method"].as_str(),
                    Some("bind_execution" | "rebind_execution")
                ),
                "Task bind requires its persisted new binding"
            );
            if body["method"] == "resolve_unknown" {
                let encoded: String = connection.query_row(
                    "SELECT request FROM operations WHERE space_id=? AND operation_id=?",
                    params![
                        authorization.resource().space.as_str(),
                        body["operation_id"]
                            .as_str()
                            .context("resolution operation is missing")?
                    ],
                    |row| row.get(0),
                )?;
                let request: crate::model::OperationRequest = serde_json::from_str(&encoded)?;
                request
                    .binding
                    .context("resolution operation has no original binding")?
            } else {
                serde_json::from_value(body["request"]["binding"].clone())?
            }
        }
    };
    let local = policy.request().source_node();
    let binding_hash = crate::binding::binding_hash(&binding.binding)?;
    ensure!(
        retained_binding_in(
            connection,
            authorization.resource().space.as_str(),
            &binding_hash,
            local
        )? == binding,
        "Task policy association changed its original persisted binding"
    );
    let context = policy.context();
    ensure!(
        Some(binding.binding.generation) == context.binding_generation
            && Some(binding.binding.task_ref.as_str())
                == context.context.task.as_ref().map(|task| task.as_str())
            && Some(binding.binding.session_id.as_str())
                == context
                    .context
                    .execution_session
                    .as_ref()
                    .map(|session| session.session_id.as_str())
            && binding.binding.target_node
                == crate::crypto::fingerprint(policy.selection().target_public_key.as_str())?,
        "Task policy association changed its exact binding scope"
    );
    let current: String = connection.query_row(
        "SELECT value FROM local_values WHERE space_id=? AND key=?",
        params![
            authorization.resource().space.as_str(),
            format!("task-binding-current:{}", binding.binding.task_ref)
        ],
        |row| row.get(0),
    )?;
    ensure!(
        serde_json::from_str::<String>(&current)? == binding_hash,
        "Task policy binding changed during business admission"
    );
    if new_binding.is_some() {
        ensure!(
            body["task_id"].as_str() == Some(binding.binding.task_id.as_str())
                && body["conversation_id"].as_str()
                    == Some(binding.binding.conversation_id.as_str())
                && body["executor"] == binding.binding.open_spec["executor"]
                && body["cwd"] == binding.binding.open_spec["cwd"],
            "Task policy new binding changed its admitted body"
        );
        match policy.trust().original_binding() {
            None => ensure!(
                binding.binding.generation == 1
                    && binding.binding.previous_binding_hash.is_none()
                    && binding.binding.previous_seal.is_none(),
                "first Task policy changed its initial binding"
            ),
            Some(old) => {
                let old_hash = crate::binding::binding_hash(&old.binding)?;
                ensure!(
                    binding.binding.previous_binding_hash.as_deref() == Some(old_hash.as_str())
                        && binding.binding.previous_seal
                            == Some(serde_json::from_value(body["previous_seal"].clone())?)
                        && serde_json::from_value::<SignedBinding>(body["binding"].clone())?
                            == *old
                        && old.binding.generation.checked_add(1)
                            == Some(binding.binding.generation),
                    "Task policy rebind changed its actual previous binding or seal"
                );
            }
        }
    }
    let reference = policy_reference(policy)?;
    let association = AdmissionAssociation {
        policy_ref: reference,
        approval_ref: policy.trust().approval().hash()?,
        invocation_hash: invocation.hash()?,
        authorization_id: authorization.operation_id().to_owned(),
        body_hash: authorization.body_hash().clone(),
        work,
        binding_hash: Some(binding_hash),
        admitted_at_ms: admitted_at,
    };
    save_exact_in(
        connection,
        &key("admission", &association.invocation_hash),
        &association,
    )?;
    if new_binding.is_some() {
        save_exact_in(
            connection,
            &binding_association_key(&binding)?,
            &association.invocation_hash,
        )?;
    }
    Ok(())
}

pub(crate) fn rehydrate_task_execution_policy_in(
    connection: &Connection,
    primary: &mut super::super::journal::PolicyState,
    source: &PublicKey,
) -> Result<()> {
    ensure!(
        SignatureVerification::current_counts().is_none(),
        "Task policy rehydration must run outside a network carrier"
    );
    let mut statement =
        connection.prepare("SELECT key FROM identity_meta WHERE key LIKE ? ORDER BY key")?;
    let names = statement
        .query_map([format!("{PREFIX}:completion:%")], |row| {
            row.get::<_, String>(0)
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut restored = TaskExecutionPolicyProvenance::default();
    let mut association_statement =
        connection.prepare("SELECT key FROM identity_meta WHERE key LIKE ? ORDER BY key")?;
    let association_keys = association_statement
        .query_map([format!("{PREFIX}:admission:%")], |row| {
            row.get::<_, String>(0)
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let associations = association_keys
        .iter()
        .map(|name| {
            read_in::<AdmissionAssociation>(connection, name)?
                .context("foreign policy accepted association is missing")
        })
        .collect::<Result<Vec<_>>>()?;
    for name in names {
        let reference = ObjectHash::parse(
            name.strip_prefix(&format!("{PREFIX}:completion:"))
                .context("invalid foreign policy completion key")?,
        )?;
        let (complete, imported) = completion_in(connection, &reference)?;
        let approval: Signed<TaskExecutionTrustApproval> =
            read_in(connection, &key("approval", &imported.approval_ref))?
                .context("foreign policy retained approval is missing")?;
        if approval.issuer != *source {
            continue;
        }
        let request: TaskExecutionPolicyRequest =
            read_in(connection, &key("request", &imported.request_ref))?
                .context("foreign policy retained request is missing")?;
        let snapshot: Signed<RemoteExecutionPolicySnapshot> =
            read_in(connection, &key("snapshot", &imported.snapshot_ref))?
                .context("foreign policy retained snapshot is missing")?;
        let witness: Signed<RemoteExecutionPolicyWitness> =
            read_in(connection, &key("witness", &complete.witness_ref))?
                .context("foreign policy retained witness is missing")?;
        ensure!(
            request.request_hash()? == imported.request_ref
                && ObjectHash::body(&snapshot)? == imported.snapshot_ref
                && ObjectHash::body(&witness)? == complete.witness_ref,
            "foreign policy retained envelope hash changed"
        );
        let importer = rebuild_retained_import_in(
            connection,
            primary,
            source,
            &imported,
            &complete.import_ref,
            &request,
            &snapshot,
            complete.completed_at_ms,
        )?;
        {
            let _carrier = ScopedSignatureVerification::new();
            let floors = importer
                .clone()
                .finish_retained_floors(&witness, complete.completed_at_ms)?;
            ensure_actual_budget()?;
            for pin in floors {
                merge_verified_floor(&mut restored.retained_floors, pin)?;
            }
        }
        for association in associations
            .iter()
            .filter(|association| association.policy_ref == reference)
        {
            let proof = super::super::admission::load_admission_in(
                connection,
                request.source_project().space.as_str(),
                &association.authorization_id,
            )?;
            ensure!(
                proof.invocation.hash()? == association.invocation_hash
                    && ObjectHash::body(&proof.source_body)? == association.body_hash
                    && proof.admitted_at_ms == association.admitted_at_ms
                    && association.approval_ref == imported.approval_ref,
                "retained foreign policy association changed its accepted admission"
            );
            let original_importer = rebuild_retained_import_in(
                connection,
                primary,
                source,
                &imported,
                &complete.import_ref,
                &request,
                &snapshot,
                proof.admitted_at_ms,
            )?;
            let _carrier = ScopedSignatureVerification::new();
            let policy = original_importer.finish_original_admission(&witness, &proof)?;
            policy.proof_usage().enforce_limits()?;
            ensure_actual_budget()?;
            restored.original.insert(
                association.invocation_hash.clone(),
                CachedOriginal {
                    proof_hash: ObjectHash::body(&proof)?,
                    policy_ref: reference.clone(),
                    policy,
                },
            );
        }
    }
    primary
        .task_execution_policy
        .original
        .extend(restored.original);
    for pin in restored.retained_floors.into_values() {
        merge_verified_floor(&mut primary.task_execution_policy.retained_floors, pin)?;
    }
    Ok(())
}

fn ensure_actual_budget() -> Result<()> {
    if let Some(counts) = SignatureVerification::current_counts() {
        ensure!(
            counts.actual_verifications <= 32,
            "foreign retained carrier crypto budget exceeded"
        );
    }
    Ok(())
}

fn rebuild_retained_import_in(
    connection: &Connection,
    primary: &super::super::journal::PolicyState,
    source: &PublicKey,
    imported: &ImportRecord,
    import_ref: &ObjectHash,
    request: &TaskExecutionPolicyRequest,
    snapshot: &Signed<RemoteExecutionPolicySnapshot>,
    clock: u64,
) -> Result<super::node::TaskPolicyImport> {
    ensure!(
        SignatureVerification::current_counts().is_none(),
        "retained import rebuild must run outside a network carrier"
    );
    let raw_node = if let Some(node) = request.node_request() {
        let digest = node
            .source_provenance()
            .context("retained Node read has no signed SDK source provenance")?;
        let raw: SdkPolicySourceProvenance = read_in(connection, &key("sdk-source", digest))?
            .context("retained Node read original SDK source is missing")?;
        ensure!(
            ObjectHash::body(&raw)? == *digest,
            "retained Node read SDK source changed"
        );
        Some(raw)
    } else {
        None
    };
    let heads = match request {
        TaskExecutionPolicyRequest::Account(value) => &value.intent.payload.source_heads,
        TaskExecutionPolicyRequest::Node(_) => {
            &raw_node
                .as_ref()
                .context("Node source original is missing")?
                .source_heads
        }
    };
    let historical = primary.historical(heads, clock)?;
    let pins: Vec<_> = primary
        .pins()
        .iter()
        .filter(|pin| heads.iter().any(|head| head.domain == pin.domain))
        .map(|pin| vibeface_secure::authorization::PolicyPin {
            domain: pin.domain.clone(),
            genesis_root: pin.genesis_root.clone(),
            minimum_head: None,
        })
        .collect();
    let (trust, retained) = {
        let _carrier = ScopedSignatureVerification::new();
        let trust = load_task_execution_trust_in(connection, source, &imported.approval_ref)?;
        let retained=match request {
            TaskExecutionPolicyRequest::Account(value)=>task_policy_source_from_retained(&trust,&historical,&pins,value,clock)?,
            TaskExecutionPolicyRequest::Node(value)=>vibeface_secure::authorization::node_execution_policy::node_task_policy_source_from_retained(
                &historical,&pins,&trust,raw_node.as_ref().context("Node source original is missing")?,value,clock)?,
        };
        ensure_actual_budget()?;
        (trust, retained)
    };
    let mut importer = {
        let _carrier = ScopedSignatureVerification::new();
        let importer=match request {
            TaskExecutionPolicyRequest::Account(value)=>super::node::TaskPolicyImport::Account(Box::new(begin_task_policy_import(&trust,&retained,value,snapshot,&[],PolicyCarrierOrigin::SignedTranscript,clock)?)),
            TaskExecutionPolicyRequest::Node(value)=>super::node::TaskPolicyImport::Node(Box::new(vibeface_secure::authorization::node_execution_policy::begin_retained_node_task_policy_import(&trust,&retained,value,snapshot,clock)?)),
        };
        importer.proof_usage().enforce_limits()?;
        ensure_actual_budget()?;
        importer
    };
    for index in 0..imported.page_count {
        let _carrier = ScopedSignatureVerification::new();
        let page: Signed<
            vibeface_secure::authorization::remote_execution_policy::RemoteExecutionPolicyPage,
        > = read_in(connection, &ingress::page_key(import_ref, index))?
            .context("retained Task policy original page is missing")?;
        importer.append_page(&page, clock)?;
        importer.proof_usage().enforce_limits()?;
        ensure_actual_budget()?;
    }
    Ok(importer)
}

pub(crate) fn validate_task_execution_policy_links_in(
    connection: &Connection,
    primary: &super::super::journal::PolicyState,
) -> Result<()> {
    ensure!(
        SignatureVerification::current_counts().is_none(),
        "Task policy startup validation must run outside a network carrier"
    );
    let mut floors = primary.task_execution_policy.retained_floors.clone();
    for policy in primary.task_execution_policy.current.values() {
        gather_verified_floors(
            &mut floors,
            policy.pins(),
            policy.primary_heads(),
            policy.foreign_heads(),
        )?;
    }
    for policy in primary.task_execution_policy.history.values() {
        gather_verified_floors(
            &mut floors,
            policy.pins(),
            policy.primary_heads(),
            policy.foreign_heads(),
        )?;
    }
    for (name, expected) in floors {
        let saved: vibeface_secure::authorization::PolicyPin =
            read_in(connection, &name)?.context("verified foreign policy floor is missing")?;
        ensure!(
            saved == expected,
            "foreign policy floor differs from its verified original completions"
        );
    }
    let mut statement =
        connection.prepare("SELECT key FROM identity_meta WHERE key LIKE ? ORDER BY key")?;
    let names = statement
        .query_map([format!("{PREFIX}:admission:%")], |row| {
            row.get::<_, String>(0)
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for name in names {
        let _carrier = ScopedSignatureVerification::new();
        let association: AdmissionAssociation =
            read_in(connection, &name)?.context("foreign policy admission link is missing")?;
        ensure!(
            key("admission", &association.invocation_hash) == name,
            "foreign policy admission link changed its key"
        );
        let source = association_source(primary, &association)?;
        let space = if let Some(original) = primary
            .task_execution_policy
            .original
            .get(&association.invocation_hash)
        {
            original.policy.request().source_project().space.as_str()
        } else {
            primary
                .task_execution_policy
                .current
                .get(&association.policy_ref)
                .context("accepted foreign policy has no verified local source provenance")?
                .request()
                .source_project()
                .space
                .as_str()
        };
        let proof = super::super::admission::load_admission_in(
            connection,
            space,
            &association.authorization_id,
        )?;
        original_task_execution_policy_in(connection, primary, &source, &proof)?
            .context("accepted foreign policy link has no original admission")?;
        ensure_actual_budget()?;
    }
    Ok(())
}

fn gather_verified_floors(
    floors: &mut BTreeMap<String, vibeface_secure::authorization::PolicyPin>,
    pins: &[vibeface_secure::authorization::PolicyPin],
    primary: &[PolicyStamp],
    heads: &[PolicyStamp],
) -> Result<()> {
    for head in heads {
        if primary.iter().any(|primary| primary.domain == head.domain) {
            continue;
        }
        let original = pins
            .iter()
            .find(|pin| pin.domain == head.domain)
            .context("verified foreign head has no pin")?;
        let pin = vibeface_secure::authorization::PolicyPin {
            domain: head.domain.clone(),
            genesis_root: original.genesis_root.clone(),
            minimum_head: Some(head.clone()),
        };
        let name = ingress::floor_key(&pin)?;
        if let Some(prior) = floors.get(&name).and_then(|pin| pin.minimum_head.as_ref()) {
            if (prior.authority_epoch, prior.sequence) > (head.authority_epoch, head.sequence) {
                continue;
            }
            ensure!(
                (prior.authority_epoch, prior.sequence) != (head.authority_epoch, head.sequence)
                    || prior == head,
                "stored foreign completions conflict at the same verified floor"
            );
        }
        floors.insert(name, pin);
    }
    Ok(())
}

fn merge_verified_floor(
    floors: &mut BTreeMap<String, vibeface_secure::authorization::PolicyPin>,
    pin: vibeface_secure::authorization::PolicyPin,
) -> Result<()> {
    let head = pin
        .minimum_head
        .as_ref()
        .context("verified retained floor has no exact head")?;
    ensure!(
        head.domain == pin.domain,
        "verified retained floor belongs to another domain"
    );
    let name = ingress::floor_key(&pin)?;
    if let Some(prior) = floors.get(&name).and_then(|pin| pin.minimum_head.as_ref()) {
        if (prior.authority_epoch, prior.sequence) > (head.authority_epoch, head.sequence) {
            return Ok(());
        }
        ensure!(
            (prior.authority_epoch, prior.sequence) != (head.authority_epoch, head.sequence)
                || prior == head,
            "verified retained floors conflict at the same version"
        );
    }
    floors.insert(name, pin);
    Ok(())
}
