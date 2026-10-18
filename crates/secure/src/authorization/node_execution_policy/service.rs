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
        primary.verify_sdk_policy_read_source(parent, service, service_grant, child_facts, now)?;
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
    let mut usage = delegated.proof_usage().clone();
    let authority = VerifiedNodeTaskPolicyAuthority {
        state: primary.clone(),
        heads: delegated.heads().to_vec(),
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

#[derive(Clone)]
pub struct NodeTaskExecutionPolicyImport {
    trust: VerifiedTaskExecutionTrust,
    source: VerifiedTaskPolicySource,
    request: NodeExecutionPolicyReadRequest,
    header: Signed<remote_execution_policy::RemoteExecutionPolicySnapshot>,
    import: policy_import::BoundedPolicyImport,
    previous_page_hash: ObjectHash,
    next_cursor: Option<ObjectHash>,
    last_page: Option<Signed<remote_execution_policy::RemoteExecutionPolicyPage>>,
    page_count: u64,
    entry_count: u64,
    complete: bool,
    usage: ExecutionProofUsage,
    page_envelope_hashes: Vec<ObjectHash>,
    peer: ActualPolicyNoisePeer,
}

fn carrier<T: Serialize>(value: &T) -> Result<()> {
    require(
        canonical(value)?.len() <= MAX_PROOF_BYTES,
        "native Node policy carrier exceeds 64 KiB",
    )
}

fn check_request(request: &NodeExecutionPolicyReadRequest, now: u64) -> Result<()> {
    carrier(request)?;
    read_window(request.issued_at_ms(), request.expires_at_ms(), now)?;
    validate_pins(request.execution_pins())?;
    match request {
        NodeExecutionPolicyReadRequest::Current(value) => {
            value.verify_signature()?;
            require(
                value.payload.version == AUTH_VERSION
                    && value.payload.purpose != RemoteExecutionPolicyPurpose::History,
                "native current policy request changed its version or purpose",
            )
        }
        NodeExecutionPolicyReadRequest::History(value) => {
            value.verify_signature()?;
            require(
                value.payload.version == AUTH_VERSION,
                "native History policy request changed its version",
            )?;
            remote_execution_history::validate_remote_execution_history_reference(
                &value.payload.reference,
            )
        }
    }
}

/// `peer` comes from the completed connection to C; a signed transcript cannot enter here.
pub fn begin_node_task_policy_import(
    trust: &VerifiedTaskExecutionTrust,
    source: &VerifiedTaskPolicySource,
    request: &NodeExecutionPolicyReadRequest,
    header: &Signed<remote_execution_policy::RemoteExecutionPolicySnapshot>,
    verified_floors: &[PolicyPin],
    peer: &ActualPolicyNoisePeer,
    now: u64,
) -> Result<NodeTaskExecutionPolicyImport> {
    let _signatures = ScopedSignatureVerification::new();
    let same_source = match &source.source_scope {
        TaskPolicySourceScope::Node(scope) => scope.request == *request,
        TaskPolicySourceScope::Account(_) => false,
    };
    require(
        same_source && source.verified_at_ms <= now && now < source.expires_at_ms,
        "native Node policy import changed its verified local source",
    )?;
    check_request(request, now)?;
    let typed = TaskExecutionPolicyRequest::Node(request.clone());
    check_task_read_scope(trust, &typed)?;
    let expected = &trust.selection().target_public_key;
    require(
        peer.public_key() == expected,
        "native Node policy import reached another actual Noise peer",
    )?;
    carrier(header)?;
    header.verify_signature()?;
    read_window(
        header.payload.issued_at_ms,
        header.payload.expires_at_ms,
        now,
    )?;
    let history = request.purpose() == RemoteExecutionPolicyPurpose::History;
    require(
        history == header.payload.history.is_some()
            && &header.issuer == expected
            && &header.payload.source_node_public_key == expected
            && header.payload.version == AUTH_VERSION
            && header.payload.request_hash == ObjectHash::body(request)?
            && header.payload.saved_pins_hash == ObjectHash::body(&request.execution_pins())?
            && header.payload.issued_at_ms >= request.issued_at_ms()
            && header.payload.expires_at_ms <= request.expires_at_ms()
            && !header.payload.target_heads.is_empty()
            && header.payload.target_heads.len() <= MAX_POLICY_DOMAINS
            && header
                .payload
                .target_heads
                .windows(2)
                .all(|pair| pair[0].domain < pair[1].domain),
        "native Node policy snapshot changed its source, signed request or purpose",
    )?;
    let effective = effective_task_pins(trust, source, &typed, verified_floors)?;
    let selected: Vec<_> = effective
        .into_iter()
        .filter(|pin| {
            header
                .payload
                .target_heads
                .iter()
                .any(|head| head.domain == pin.domain)
        })
        .collect();
    require(
        selected.len() == header.payload.target_heads.len(),
        "native Node policy snapshot introduced an unapproved dependency domain",
    )?;
    let original_minima = request
        .execution_pins()
        .iter()
        .filter(|pin| selected.iter().any(|saved| saved.domain == pin.domain))
        .filter_map(|pin| pin.minimum_head.clone())
        .collect();
    let import = policy_import::BoundedPolicyImport::new(selected, original_minima)?;
    let mut usage = source.usage.clone();
    match request {
        NodeExecutionPolicyReadRequest::Current(value) => usage.record_signed(value)?,
        NodeExecutionPolicyReadRequest::History(value) => usage.record_signed(value)?,
    }
    usage.record_metadata(request)?;
    usage.record_signed(header)?;
    finish_usage(&mut usage)?;
    Ok(NodeTaskExecutionPolicyImport {
        trust: trust.clone(),
        source: source.clone(),
        request: request.clone(),
        header: header.clone(),
        import,
        previous_page_hash: header.hash()?,
        next_cursor: None,
        last_page: None,
        page_count: 0,
        entry_count: 0,
        complete: false,
        usage,
        page_envelope_hashes: Vec::new(),
        peer: peer.clone(),
    })
}

struct CompletedNodePolicy {
    trust: VerifiedTaskExecutionTrust,
    source: VerifiedTaskPolicySource,
    request: TaskExecutionPolicyRequest,
    header: Signed<remote_execution_policy::RemoteExecutionPolicySnapshot>,
    witness: Signed<remote_execution_policy::RemoteExecutionPolicyWitness>,
    foreign: policy_import::VerifiedPolicySnapshot,
    usage: ExecutionProofUsage,
    page_envelope_hashes: Vec<ObjectHash>,
    origin: PolicyCarrierOrigin,
}

impl NodeTaskExecutionPolicyImport {
    pub fn page_request(&self) -> remote_execution_policy::RemoteExecutionPolicyPageRequest {
        remote_execution_policy::RemoteExecutionPolicyPageRequest {
            snapshot_id: self.header.payload.snapshot_id.clone(),
            request_hash: self.header.payload.request_hash.clone(),
            cursor: self.next_cursor.clone(),
        }
    }
    pub fn proof_usage(&self) -> &ExecutionProofUsage {
        &self.usage
    }

    pub fn append_page(
        &mut self,
        page: &Signed<remote_execution_policy::RemoteExecutionPolicyPage>,
        now: u64,
    ) -> Result<()> {
        let _signatures = ScopedSignatureVerification::new();
        carrier(page)?;
        read_window(
            self.header.payload.issued_at_ms,
            self.header.payload.expires_at_ms,
            now,
        )?;
        page.verify_signature()?;
        require(
            page.issuer == self.header.issuer
                && page.payload.version == AUTH_VERSION
                && page.payload.request_hash == self.header.payload.request_hash
                && page.payload.snapshot_id == self.header.payload.snapshot_id
                && page.payload.target_heads_hash
                    == ObjectHash::body(&self.header.payload.target_heads)?,
            "native Node policy page changed its source or snapshot",
        )?;
        let mut usage = ExecutionProofUsage::default();
        usage.record_signed(page)?;
        if self.last_page.as_ref() == Some(page) {
            finish_usage(&mut usage)?;
            self.usage = usage;
            return Ok(());
        }
        require(
            !self.complete
                && page.payload.page_index == self.page_count
                && page.payload.request_cursor == self.next_cursor
                && page.payload.previous_page_hash == self.previous_page_hash
                && page.payload.complete == page.payload.next_cursor.is_none()
                && (page.payload.entry.is_some() || page.payload.complete),
            "native Node policy page is missing, duplicated or out of order",
        )?;
        let mut staged = self.import.clone();
        if let Some(entry) = &page.payload.entry {
            usage.merge(&policy_proof::policy_entry_usage(staged.state(), entry)?);
            usage.enforce_limits()?;
            staged.apply_entry(entry.clone())?;
        }
        finish_usage(&mut usage)?;
        let page_hash = page.hash()?;
        let envelope_hash = ObjectHash::body(page)?;
        self.import = staged;
        self.previous_page_hash = page_hash;
        self.next_cursor = page.payload.next_cursor.clone();
        self.page_count += 1;
        self.entry_count += u64::from(page.payload.entry.is_some());
        self.complete = page.payload.complete;
        self.last_page = Some(page.clone());
        self.page_envelope_hashes.push(envelope_hash);
        self.usage = usage;
        Ok(())
    }

    fn finish_policy(
        self,
        witness: &Signed<remote_execution_policy::RemoteExecutionPolicyWitness>,
        now: u64,
        history: bool,
    ) -> Result<CompletedNodePolicy> {
        let _signatures = ScopedSignatureVerification::new();
        check_request(&self.request, now)?;
        require(
            history == (self.request.purpose() == RemoteExecutionPolicyPurpose::History)
                && history == self.header.payload.history.is_some(),
            "native Node policy completion changed its purpose",
        )?;
        carrier(witness)?;
        witness.verify_signature()?;
        read_window(
            self.header.payload.issued_at_ms,
            self.header.payload.expires_at_ms,
            now,
        )?;
        read_window(
            witness.payload.issued_at_ms,
            witness.payload.expires_at_ms,
            now,
        )?;
        require(
            self.complete
                && witness.issuer == self.header.issuer
                && witness.payload.version == AUTH_VERSION
                && witness.payload.source_node_public_key == self.header.issuer
                && witness.payload.request_hash == self.header.payload.request_hash
                && witness.payload.snapshot_id == self.header.payload.snapshot_id
                && witness.payload.target_heads == self.header.payload.target_heads
                && witness.payload.snapshot_hash == self.header.hash()?
                && witness.payload.final_page_hash == self.previous_page_hash
                && witness.payload.page_count == self.page_count
                && witness.payload.entry_count == self.entry_count
                && witness.payload.issued_at_ms >= self.header.payload.issued_at_ms
                && witness.payload.expires_at_ms <= self.header.payload.expires_at_ms
                && self.source.verified_at_ms <= now
                && now < self.source.expires_at_ms
                && self.peer.public_key() == &self.header.issuer,
            "native Node policy witness changed its completed stream or actual peer",
        )?;
        let snapshot = self.import.finish(&self.header.payload.target_heads)?;
        let verified = verify_node_execution_policy_read(
            snapshot.state(),
            &self.request,
            &self.header.issuer,
            self.request.issuer(),
            self.header.payload.history.as_ref(),
            now,
        )?;
        require(
            verified.target_heads() == self.header.payload.target_heads
                && snapshot.journal().iter().all(|entry| {
                    policy_import::entry_hash(entry).is_ok_and(|hash| verified.allows_hash(&hash))
                }),
            "native Node policy stream contains objects outside its necessary reference closure",
        )?;
        let advanced: Vec<_> = snapshot
            .pins()
            .iter()
            .cloned()
            .map(|mut pin| {
                pin.minimum_head = snapshot.state().head(&pin.domain).cloned();
                pin
            })
            .collect();
        let foreign = snapshot.with_independent_pins(&advanced)?;
        let mut usage = verified.proof_usage().clone();
        usage.merge(&self.source.usage);
        usage.record_signed(&self.header)?;
        usage.record_signed(witness)?;
        usage.record_signed(self.trust.approval())?;
        finish_usage(&mut usage)?;
        let request = TaskExecutionPolicyRequest::Node(self.request);
        check_task_read_scope(&self.trust, &request)?;
        Ok(CompletedNodePolicy {
            trust: self.trust,
            source: self.source,
            request,
            header: self.header,
            witness: witness.clone(),
            foreign,
            usage,
            page_envelope_hashes: self.page_envelope_hashes,
            origin: PolicyCarrierOrigin::DirectNoise(self.peer),
        })
    }

    pub fn finish_current(
        self,
        witness: &Signed<remote_execution_policy::RemoteExecutionPolicyWitness>,
        now: u64,
    ) -> Result<VerifiedTaskCurrentPolicy> {
        let finished = self.finish_policy(witness, now, false)?;
        let combined =
            policy_import::merge_same_head_snapshots(&finished.source.snapshot, &finished.foreign)?;
        let (primary_heads, foreign_heads) = combined.heads().iter().cloned().partition(|head| {
            finished
                .source
                .primary_pins
                .iter()
                .any(|pin| pin.domain == head.domain)
        });
        let policy = VerifiedTaskCurrentPolicy {
            selection: finished.request.selection()?,
            context: finished.request.context()?,
            source: finished.source,
            trust: finished.trust,
            request: finished.request,
            snapshot: finished.header,
            witness: finished.witness,
            combined,
            primary_heads,
            foreign_heads,
            usage: finished.usage,
            page_envelope_hashes: finished.page_envelope_hashes,
            origin: finished.origin,
        };
        policy.check_window(now)?;
        Ok(policy)
    }

    pub fn finish_history(
        self,
        witness: &Signed<remote_execution_policy::RemoteExecutionPolicyWitness>,
        now: u64,
    ) -> Result<VerifiedTaskHistoryPolicy> {
        let finished = self.finish_policy(witness, now, true)?;
        Ok(VerifiedTaskHistoryPolicy {
            source: finished.source,
            trust: finished.trust,
            request: finished.request,
            snapshot: finished.header,
            witness: finished.witness,
            foreign: finished.foreign,
            usage: finished.usage,
            page_envelope_hashes: finished.page_envelope_hashes,
            origin: finished.origin,
        })
    }
}
