//! Local SDK authority and native Node-to-Node policy imports.

use super::*;
use crate::signature_verification::SignatureVerification;

#[derive(Clone)]
pub(super) struct NodeTaskPolicySourceScope {
    pub(super) facts: RequestFacts,
    pub(super) context: prepare::PrepareContext,
    pub(super) request: NodeExecutionPolicyReadRequest,
    parent: Option<VerifiedAuthorization>,
    service: Option<ServiceId>,
    service_grant: Option<ObjectHash>,
    current_binding: Option<SignedBinding>,
    lease_expires_at_ms: u64,
}

/// Produced only after a current local reader or SDK delegation has been checked.
pub(crate) struct VerifiedNodeTaskPolicyAuthority {
    state: VerifiedPolicySet,
    heads: Vec<PolicyStamp>,
}
impl VerifiedNodeTaskPolicyAuthority {
    pub(crate) fn state(&self) -> &VerifiedPolicySet {
        &self.state
    }
    pub(crate) fn heads(&self) -> &[PolicyStamp] {
        &self.heads
    }
}

fn finish_usage(usage: &mut ExecutionProofUsage) -> Result<()> {
    if let Some(counts) = SignatureVerification::current_counts() {
        usage.record_actual_signature_counts(counts);
    }
    usage.enforce_limits()
}

fn source_project(
    primary: &VerifiedPolicySet,
    project: &ResourceRef,
    audience: &Audience,
    local: &PublicKey,
) -> Result<()> {
    let record = primary.resource(&project.space, &project.resource).ok_or(
        AuthorizationError::InvalidProof("native source Project is missing"),
    )?;
    require(
        record.payload.kind == ResourceKind::Project
            && record.payload.host == ServiceId::from_key(local)
            && audience.service == record.payload.host
            && audience.device == DeviceId::from_key(local),
        "native policy source belongs to another Project service",
    )
}

fn check_sdk_read_context(
    facts: &RequestFacts,
    context: &prepare::PrepareContext,
    local: &PublicKey,
    selection: &RemoteExecutionPolicySelection,
) -> Result<()> {
    let sdk = facts
        .context
        .sdk_session
        .as_ref()
        .ok_or(AuthorizationError::InvalidProof(
            "native policy source has no current SDK session",
        ))?;
    let mut read = facts.context.clone();
    read.sdk_session = None;
    require(
        sdk.target == DeviceId::from_key(local)
            && !sdk.session_id.is_empty()
            && read == context.context
            && context.context.workspace_resource.as_ref() == Some(&facts.resource.resource)
            && context.context.task.is_some()
            && context.context.conversation.is_some()
            && context.context.executor_kind.is_some()
            && context.execution_target_public_key.as_ref() == Some(&selection.target_public_key)
            && context
                .context
                .execution_session
                .as_ref()
                .is_some_and(|session| {
                    session.target == DeviceId::from_key(&selection.target_public_key)
                        && !session.session_id.is_empty()
                })
            && context
                .binding_generation
                .is_some_and(|generation| generation > 0),
        "native policy source changed its SDK context, Task or executor",
    )
}

fn check_binding(
    trust: &VerifiedTaskExecutionTrust,
    context: &prepare::PrepareContext,
    binding: Option<&SignedBinding>,
    local: &PublicKey,
    action: Action,
) -> Result<()> {
    let Some(binding) = binding else {
        return require(
            action == Action::ExecutionOpen
                && context.binding_generation == Some(1)
                && trust.binding_generation() == 1,
            "only an initial execution open can read policy without a current binding",
        );
    };
    task_binding::verify_binding(binding, local.as_str()).map_err(|_| {
        AuthorizationError::InvalidProof("native source binding is not signed by B")
    })?;
    let value = &binding.binding;
    let actual = &context.context;
    require(
        value.network_id
            == trust
                .original_intent()
                .payload
                .source_selection
                .resource
                .space
                .as_str()
            && actual
                .task
                .as_ref()
                .is_some_and(|task| task.as_str() == value.task_ref)
            && actual.conversation.as_deref() == Some(value.conversation_id.as_str()),
        "native source binding belongs to another Project, Task or conversation",
    )?;
    if action == Action::ExecutionRebind {
        return require(
            trust.original_binding() == Some(binding)
                && value.generation.checked_add(1) == context.binding_generation,
            "native rebind policy read changed its approved prior binding",
        );
    }
    let selection = trust.selection();
    let fingerprint = node_fingerprint(selection.target_public_key.as_str())
        .map_err(|_| AuthorizationError::InvalidProof("native binding has an invalid target"))?;
    require(
        context.binding_generation == Some(value.generation)
            && value.target_node == fingerprint
            && actual.execution_session.as_ref().is_some_and(|session| {
                session.target == DeviceId::from_key(&selection.target_public_key)
                    && session.session_id == value.session_id
            })
            && actual.executor_kind.as_deref() == value.open_spec["executor"].as_str()
            && context.canonical_cwd.as_deref() == value.open_spec["cwd"].as_str(),
        "native source binding changed its generation, session, target or open context",
    )
}

fn service_usage(
    primary: &VerifiedPolicySet,
    parent: &VerifiedAuthorization,
    service_grant: &ObjectHash,
    facts: &RequestFacts,
    local: &PublicKey,
    now: u64,
) -> Result<(ExecutionProofUsage, Vec<PolicyStamp>)> {
    let mut budget = Budget::default();
    let mut dependencies = BTreeMap::new();
    let record = primary
        .resource(&facts.resource.space, &facts.resource.resource)
        .ok_or(AuthorizationError::PolicyConflict)?;
    budget.verify(record)?;
    let device = primary.find_device(&DeviceId::from_key(local))?;
    primary.validate_certificate(
        device,
        now,
        &mut budget,
        &mut dependencies,
        &mut BTreeSet::new(),
    )?;
    if let Some(session) = &facts.context.execution_session {
        let target = primary.find_device(&session.target)?;
        primary.validate_certificate(
            target,
            now,
            &mut budget,
            &mut dependencies,
            &mut BTreeSet::new(),
        )?;
    }
    let chain = primary.grant_chain(Some(service_grant))?;
    primary.verify_grant_path(&chain, now, &mut budget, &mut dependencies, true)?;
    primary.expand_authorities(now, &mut budget, &mut dependencies)?;
    for head in parent.dependencies() {
        if let Some(previous) = dependencies.insert(head.domain.clone(), head.clone()) {
            require(
                previous == *head,
                "native source has conflicting parent and service heads",
            )?;
        }
    }
    let mut usage = ExecutionProofUsage::default();
    usage.record_authorization(parent)?;
    usage.record_budget(&budget)?;
    usage.record_chain_depth(chain.len());
    Ok((usage, dependencies.into_values().collect()))
}

/// The Store checks the persisted SDK lease/origin and ready Task/binding in this transaction.
pub fn verify_node_service_policy_source(
    primary: &VerifiedPolicySet,
    primary_pins: &[PolicyPin],
    trust: &VerifiedTaskExecutionTrust,
    parent: &VerifiedAuthorization,
    service: &ServiceId,
    service_grant: &ObjectHash,
    child_facts: &RequestFacts,
    selection: &RemoteExecutionPolicySelection,
    read_context: &prepare::PrepareContext,
    current_binding: Option<&SignedBinding>,
    lease_expires_at: u64,
    identity: &Identity,
    purpose: RemoteExecutionPolicyPurpose,
    now: u64,
    expires: u64,
) -> Result<(VerifiedTaskPolicySource, NodeExecutionPolicyReadRequest)> {
    let _signatures = ScopedSignatureVerification::new();
    let local = PublicKey::of(identity);
    require(
        purpose != RemoteExecutionPolicyPurpose::History,
        "History requires a current read source and its original artifact",
    )?;
    read_window(now, expires, now)?;
    require(
        now < lease_expires_at && expires <= lease_expires_at,
        "native policy source exceeds its current SDK lease",
    )?;
    source_project(
        primary,
        &child_facts.resource,
        &child_facts.audience,
        &local,
    )?;
    check_sdk_read_context(child_facts, read_context, &local, selection)?;
    check_binding(
        trust,
        read_context,
        current_binding,
        &local,
        child_facts.action,
    )?;
    let delegated =
        primary.verify_service_delegation(parent, service, service_grant, child_facts, now)?;
    require(
        service == &ServiceId::from_key(&local)
            && delegated.service() == service
            && delegated.device() == &DeviceId::from_key(&local)
            && expires <= delegated.expires_at_ms(),
        "native policy source is not this Node's current service delegation",
    )?;
    let expected_action = match purpose {
        RemoteExecutionPolicyPurpose::Execution => child_facts.action,
        RemoteExecutionPolicyPurpose::ObservationRead | RemoteExecutionPolicyPurpose::Resolver => {
            require(
                matches!(
                    child_facts.action,
                    Action::OperationStatusRead
                        | Action::OperationResolve
                        | Action::OperationReconcile
                ),
                "readonly C policy requires a current Project read source",
            )?;
            Action::OperationResolve
        }
        RemoteExecutionPolicyPurpose::History => unreachable!(),
    };
    let request = NodeExecutionPolicyReadRequest::Current(Signed::sign(
        NodeExecutionPolicyReadIntent {
            version: AUTH_VERSION,
            source_project: child_facts.resource.clone(),
            selection: selection.clone(),
            context: read_context.clone(),
            expected_action,
            purpose,
            execution_pins: trust.pins().to_vec(),
            nonce: Nonce::random()?,
            issued_at_ms: now,
            expires_at_ms: expires,
        },
        identity,
    )?);
    check_task_read_scope(trust, &TaskExecutionPolicyRequest::Node(request.clone()))?;
    let (mut usage, heads) =
        service_usage(primary, parent, service_grant, child_facts, &local, now)?;
    let authority = VerifiedNodeTaskPolicyAuthority {
        state: primary.clone(),
        heads,
    };
    let snapshot = policy_import::native_node_execution_source_snapshot(&authority, primary_pins)?;
    if let NodeExecutionPolicyReadRequest::Current(value) = &request {
        usage.record_signed(value)?;
    }
    usage.record_signed(trust.original_intent())?;
    usage.record_signed(trust.approval())?;
    if let Some(binding) = current_binding {
        usage.record_binding(binding)?;
    }
    finish_usage(&mut usage)?;
    let source = VerifiedTaskPolicySource {
        snapshot,
        primary_pins: primary_pins.to_vec(),
        source_scope: TaskPolicySourceScope::Node(NodeTaskPolicySourceScope {
            facts: child_facts.clone(),
            context: read_context.clone(),
            request: request.clone(),
            parent: Some(parent.clone()),
            service: Some(service.clone()),
            service_grant: Some(service_grant.clone()),
            current_binding: current_binding.cloned(),
            lease_expires_at_ms: lease_expires_at,
        }),
        verified_at_ms: now,
        expires_at_ms: expires.min(delegated.expires_at_ms()).min(lease_expires_at),
        usage,
        retained: false,
    };
    Ok((source, request))
}

/// Current read authority permits retrieving old artifacts after their writer is revoked.
pub fn verify_node_history_policy_source(
    primary: &VerifiedPolicySet,
    primary_pins: &[PolicyPin],
    trust: &VerifiedTaskExecutionTrust,
    current_reader: &VerifiedAuthorization,
    reference: &remote_execution_history::RemoteExecutionHistoryReference,
    original_binding: Option<&SignedBinding>,
    identity: &Identity,
    now: u64,
    expires: u64,
) -> Result<(VerifiedTaskPolicySource, NodeExecutionPolicyReadRequest)> {
    let _signatures = ScopedSignatureVerification::new();
    read_window(now, expires, now)?;
    primary.check_current(current_reader, now)?;
    let local = PublicKey::of(identity);
    let facts = &current_reader.signed_invocation().payload;
    source_project(primary, &facts.resource, &facts.audience, &local)?;
    remote_execution_history::validate_remote_execution_history_reference(reference)?;
    let context = reference.context()?;
    require(
        matches!(
            facts.action,
            Action::OperationStatusRead | Action::OperationResolve | Action::OperationReconcile
        ) && facts.resource == reference.source.payload.project
            && reference.source.issuer == local
            && reference.old_submission.issuer == local
            && facts.context.task == context.context.task
            && facts.context.conversation == context.context.conversation
            && expires <= current_reader.expires_at_ms(),
        "native History source has no current read permission for its original Project and Task",
    )?;
    if let Some(binding) = original_binding {
        task_binding::verify_binding(binding, local.as_str()).map_err(|_| {
            AuthorizationError::InvalidProof("History source has another binding signer")
        })?;
        require(
            task_binding::binding_hash(&binding.binding)
                .map_err(|_| AuthorizationError::InvalidProof("invalid History binding"))?
                == reference.source.payload.binding_hash
                && binding.binding.generation == reference.source.payload.binding_generation
                && reference
                    .expected_binding_hash
                    .as_ref()
                    .is_none_or(|hash| ObjectHash::body(binding).ok().as_ref() == Some(hash)),
            "History source changed its original durable binding",
        )?;
    }
    let request = NodeExecutionPolicyReadRequest::History(Signed::sign(
        NodeExecutionHistoryPolicyReadIntent {
            version: AUTH_VERSION,
            source_project: facts.resource.clone(),
            expected_action: facts.action,
            reference: reference.clone(),
            execution_pins: trust.pins().to_vec(),
            nonce: Nonce::random()?,
            issued_at_ms: now,
            expires_at_ms: expires,
        },
        identity,
    )?);
    check_task_read_scope(trust, &TaskExecutionPolicyRequest::Node(request.clone()))?;
    let authority = VerifiedNodeTaskPolicyAuthority {
        state: primary.clone(),
        heads: current_reader.dependencies().to_vec(),
    };
    let snapshot = policy_import::native_node_execution_source_snapshot(&authority, primary_pins)?;
    let mut usage = ExecutionProofUsage::default();
    usage.record_authorization(current_reader)?;
    usage.record_signed(trust.original_intent())?;
    usage.record_signed(trust.approval())?;
    remote_execution_history::record_history_reference_metadata(reference, &mut usage)?;
    if let NodeExecutionPolicyReadRequest::History(value) = &request {
        usage.record_signed(value)?;
    }
    if let Some(binding) = original_binding {
        usage.record_binding(binding)?;
    }
    finish_usage(&mut usage)?;
    let source = VerifiedTaskPolicySource {
        snapshot,
        primary_pins: primary_pins.to_vec(),
        source_scope: TaskPolicySourceScope::Node(NodeTaskPolicySourceScope {
            facts: RequestFacts {
                resource: facts.resource.clone(),
                action: facts.action,
                audience: facts.audience.clone(),
                operation_id: facts.operation_id.clone(),
                body_hash: facts.body_hash.clone(),
                context: facts.context.clone(),
            },
            context,
            request: request.clone(),
            parent: Some(current_reader.clone()),
            service: None,
            service_grant: None,
            current_binding: original_binding.cloned(),
            lease_expires_at_ms: current_reader.expires_at_ms(),
        }),
        verified_at_ms: now,
        expires_at_ms: expires.min(current_reader.expires_at_ms()),
        usage,
        retained: false,
    };
    Ok((source, request))
}
