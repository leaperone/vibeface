use super::ingress::{CompletionRecord, ImportRecord};
use super::*;
use std::collections::BTreeMap;
use vibeface_secure::authorization::{
    Invocation, PolicyDomain, PolicyStamp, RequestFacts, VerifiedAuthorization,
    node_execution_policy::{
        PolicyCarrierOrigin, VerifiedTaskCurrentPolicy, VerifiedTaskHistoryPolicy,
        VerifiedTaskOriginalAdmissionPolicy, begin_task_policy_import,
        from_task_original_admission, task_policy_source_from_retained,
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
    let reference = policy_reference(policy)?;
    let (complete, imported) = completion_in(connection, &reference)?;
    let request: RemoteExecutionPolicyReadRequest =
        read_in(connection, &key("request", &imported.request_ref))?
            .context("foreign policy original request is missing")?;
    let snapshot: Signed<RemoteExecutionPolicySnapshot> =
        read_in(connection, &key("snapshot", &imported.snapshot_ref))?
            .context("foreign policy original snapshot is missing")?;
    let witness: Signed<RemoteExecutionPolicyWitness> =
        read_in(connection, &key("witness", &complete.witness_ref))?
            .context("foreign policy original witness is missing")?;
    ensure!(
        policy.request().account_request() == Some(&request)
            && policy.snapshot() == &snapshot
            && policy.witness() == &witness
            && imported.approval_ref == policy.trust().approval().hash()?,
        "foreign policy original carrier differs from its verified provenance"
    );
    ensure!(
        policy.original_page_envelope_hashes().len() as u64 == imported.page_count,
        "foreign policy page count differs from its verified provenance"
    );
    for (index, expected) in policy.original_page_envelope_hashes().iter().enumerate() {
        let page: Signed<
            vibeface_secure::authorization::remote_execution_policy::RemoteExecutionPolicyPage,
        > = read_in(
            connection,
            &ingress::page_key(&complete.import_ref, index as u64),
        )?
        .context("foreign policy original page is missing")?;
        ensure!(
            ObjectHash::body(&page)? == *expected,
            "foreign policy original signed page changed"
        );
    }
    let original: Signed<RemoteExecutionPolicyReadIntent> =
        read_in(connection, &key("approved-intent", &imported.approval_ref))?
            .context("foreign approval original intent is missing")?;
    let approval: Signed<TaskExecutionTrustApproval> =
        read_in(connection, &key("approval", &imported.approval_ref))?
            .context("foreign approval original signature is missing")?;
    ensure!(
        policy.trust().original_intent() == &original && policy.trust().approval() == &approval,
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
    for policy in primary.task_execution_policy.current.values() {
        let context = policy.context();
        if policy.request().source_node() != source
            || policy.request().source_project() != project
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
    for (reference, policy) in &primary.task_execution_policy.current {
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
        let reader = policy
            .request()
            .account_request()
            .context("unsupported Task policy reader")?;
        let mut usage = ExecutionProofUsage::default();
        let trust = trust_for_read_in(
            connection,
            primary,
            identity,
            &reader.intent,
            Some(&policy.trust().approval().hash()?),
            &mut usage,
        )?;
        ensure!(
            trust.approval() == policy.trust().approval(),
            "Task policy changed its original approval"
        );
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
    let binding = retained_binding_in(
        connection,
        proof.invocation.payload.resource.space.as_str(),
        &association.binding_hash,
        source,
    )?;
    ensure!(
        Some(binding.binding.task_ref.as_str())
            == proof
                .invocation
                .payload
                .context
                .task
                .as_ref()
                .map(|task| task.as_str())
            && Some(&binding.binding.session_id)
                == proof
                    .invocation
                    .payload
                    .context
                    .execution_session
                    .as_ref()
                    .map(|session| &session.session_id),
        "original Task policy binding differs from its original invocation"
    );
    Ok(Some(from_task_original_admission(policy.clone(), proof)?))
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
        binding_hash,
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
        let request: RemoteExecutionPolicyReadRequest =
            read_in(connection, &key("request", &imported.request_ref))?
                .context("foreign policy retained request is missing")?;
        let snapshot: Signed<RemoteExecutionPolicySnapshot> =
            read_in(connection, &key("snapshot", &imported.snapshot_ref))?
                .context("foreign policy retained snapshot is missing")?;
        let witness: Signed<RemoteExecutionPolicyWitness> =
            read_in(connection, &key("witness", &complete.witness_ref))?
                .context("foreign policy retained witness is missing")?;
        ensure!(
            ObjectHash::body(&request)? == imported.request_ref
                && ObjectHash::body(&snapshot)? == imported.snapshot_ref
                && ObjectHash::body(&witness)? == complete.witness_ref,
            "foreign policy retained envelope hash changed"
        );
        let historical = primary.historical(
            &request.intent.payload.source_heads,
            complete.completed_at_ms,
        )?;
        let primary_pins: Vec<_> = primary
            .pins()
            .iter()
            .filter(|pin| {
                request
                    .intent
                    .payload
                    .source_heads
                    .iter()
                    .any(|head| head.domain == pin.domain)
            })
            .map(|pin| vibeface_secure::authorization::PolicyPin {
                domain: pin.domain.clone(),
                genesis_root: pin.genesis_root.clone(),
                minimum_head: None,
            })
            .collect();
        let (trust, retained) = {
            let _carrier = ScopedSignatureVerification::new();
            let trust = load_task_execution_trust_in(connection, source, &imported.approval_ref)?;
            let retained = task_policy_source_from_retained(
                &trust,
                &historical,
                &primary_pins,
                &request,
                complete.completed_at_ms,
            )?;
            ensure_actual_budget()?;
            (trust, retained)
        };
        let mut importer = {
            let _carrier = ScopedSignatureVerification::new();
            let importer = begin_task_policy_import(
                &trust,
                &retained,
                &request,
                &snapshot,
                &[],
                PolicyCarrierOrigin::SignedTranscript,
                complete.completed_at_ms,
            )?;
            importer.proof_usage().enforce_limits()?;
            ensure_actual_budget()?;
            importer
        };
        for index in 0..imported.page_count {
            let _carrier = ScopedSignatureVerification::new();
            let page: Signed<
                vibeface_secure::authorization::remote_execution_policy::RemoteExecutionPolicyPage,
            > = read_in(connection, &ingress::page_key(&complete.import_ref, index))?
                .context("foreign policy retained page is missing")?;
            importer.append_page(&page, complete.completed_at_ms)?;
            importer.proof_usage().enforce_limits()?;
            ensure_actual_budget()?;
        }
        let _carrier = ScopedSignatureVerification::new();
        if request.intent.payload.purpose == RemoteExecutionPolicyPurpose::History {
            let policy = importer.finish_history(&witness, complete.completed_at_ms)?;
            policy.proof_usage().enforce_limits()?;
            ensure_actual_budget()?;
            restored.history.insert(reference, policy);
        } else {
            let policy = importer.finish_current(&witness, complete.completed_at_ms)?;
            policy.proof_usage().enforce_limits()?;
            ensure_actual_budget()?;
            check_originals_in(connection, &policy)?;
            restored.current.insert(reference, policy);
        }
    }
    primary
        .task_execution_policy
        .current
        .extend(restored.current);
    primary
        .task_execution_policy
        .history
        .extend(restored.history);
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

pub(crate) fn validate_task_execution_policy_links_in(
    connection: &Connection,
    primary: &super::super::journal::PolicyState,
) -> Result<()> {
    ensure!(
        SignatureVerification::current_counts().is_none(),
        "Task policy startup validation must run outside a network carrier"
    );
    let mut floors = BTreeMap::new();
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
        let policy = primary
            .task_execution_policy
            .current
            .get(&association.policy_ref)
            .context("accepted foreign policy has no verified local source provenance")?;
        let proof = super::super::admission::load_admission_in(
            connection,
            policy.request().source_project().space.as_str(),
            &association.authorization_id,
        )?;
        original_task_execution_policy_in(
            connection,
            primary,
            policy.request().source_node(),
            &proof,
        )?
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
