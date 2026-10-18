use super::*;
use vibeface_secure::authorization::{
    RequestFacts, ServiceId, VerifiedAuthorization,
    node_execution_policy::{
        ActualPolicyNoisePeer, NodeExecutionPolicyReadRequest, SdkPolicySourceProvenance,
        TaskExecutionPolicyRequest, VerifiedTaskPolicySource, begin_node_task_policy_import,
        verify_node_service_policy_source, verify_sdk_source_provenance,
    },
    prepare::PrepareContext,
    remote_execution_policy::{RemoteExecutionPolicySelection, RemoteExecutionPolicySnapshot},
};
use vibeface_secure::authorization::{
    node_execution_policy::{
        NodeTaskExecutionPolicyImport, TaskExecutionPolicyImport, VerifiedTaskCurrentPolicy,
        VerifiedTaskHistoryPolicy, VerifiedTaskOriginalAdmissionPolicy,
    },
    remote_execution_policy::{RemoteExecutionPolicyPage, RemoteExecutionPolicyWitness},
};

pub(crate) struct PendingNodeTaskRead {
    source: VerifiedTaskPolicySource,
    trust: VerifiedTaskExecutionTrust,
    request: NodeExecutionPolicyReadRequest,
}
impl PendingNodeTaskRead {
    pub(crate) fn request(&self) -> &NodeExecutionPolicyReadRequest {
        &self.request
    }
    pub(crate) fn trust(&self) -> &VerifiedTaskExecutionTrust {
        &self.trust
    }
}

fn node_request_in(
    connection: &Connection,
    reference: &ObjectHash,
) -> Result<(ingress::ImportRecord, NodeExecutionPolicyReadRequest)> {
    let record: ingress::ImportRecord =
        read_in(connection, &key("import", reference))?.context("Node policy import is missing")?;
    let request: TaskExecutionPolicyRequest =
        read_in(connection, &key("request", &record.request_ref))?
            .context("Node policy request is missing")?;
    ensure!(
        request.request_hash()? == record.request_ref,
        "Node policy original request changed"
    );
    Ok((
        record,
        request
            .node_request()
            .cloned()
            .context("Node policy import is not a signed Node request")?,
    ))
}

fn current_node_source_in(
    connection: &Connection,
    primary: &super::super::journal::PolicyState,
    request: &NodeExecutionPolicyReadRequest,
    now: u64,
) -> Result<SdkPolicySourceProvenance> {
    ensure!(
        request.issued_at_ms() <= now && now < request.expires_at_ms(),
        "Node policy reader expired"
    );
    let digest = request
        .source_provenance()
        .context("Node policy has no signed SDK source provenance")?;
    let raw: SdkPolicySourceProvenance = read_in(connection, &key("sdk-source", digest))?
        .context("Node policy SDK source original is missing")?;
    ensure!(
        ObjectHash::body(&raw)? == *digest,
        "Node policy SDK source original changed"
    );
    let current = super::super::admission::sdk_policy_source_provenance_in(
        connection,
        primary,
        request.issuer(),
        &raw.session.service,
        &raw.service_grant,
        &raw.child_facts.request_facts(),
        now,
    )?;
    ensure!(
        current == raw,
        "NeedPolicySync: Node reader source, Manager lease or binding changed"
    );
    let context = request.context()?;
    let mut source_context = raw.child_facts.context.clone();
    source_context.sdk_session = None;
    ensure!(
        source_context == context.context && raw.child_facts.resource == *request.source_project(),
        "Node policy source changed its signed reader context"
    );
    request
        .work()
        .validate_context(&context, &request.selection()?.target_public_key)?;
    ensure!(
        matches!(request.work(), ExecutionWorkIntent::TaskSession { .. })
            || raw.current_binding.is_none(),
        "ordinary Node policy source carries a Task binding"
    );
    Ok(raw)
}

#[allow(clippy::too_many_arguments)]
fn current_trust_in(
    connection: &Connection,
    primary: &super::super::journal::PolicyState,
    source: &PublicKey,
    raw: &SdkPolicySourceProvenance,
    selection: &RemoteExecutionPolicySelection,
    context: &PrepareContext,
    work: &ExecutionWorkIntent,
    purpose: RemoteExecutionPolicyPurpose,
    usage: &mut ExecutionProofUsage,
) -> Result<VerifiedTaskExecutionTrust> {
    work.validate_context(context, &selection.target_public_key)?;
    let trust = if let Some(binding) = raw.current_binding.as_ref() {
        ensure!(
            matches!(work, ExecutionWorkIntent::TaskSession { .. }),
            "ordinary SDK work cannot carry a Task binding"
        );
        let reference = binding_association_in(connection, primary, binding, usage)
            .context("SDK Node policy source original binding association")?
            .approval_ref;
        load_task_execution_trust_in(connection, source, &reference)?
    } else {
        approved_execution_trust_in(
            connection,
            source,
            &raw.child_facts.resource,
            work,
            selection,
            context,
            purpose,
        )?
    };
    ensure!(
        trust.work().approves_intent(work),
        "SDK Node policy changed its approved execution work"
    );
    usage.record_signed(trust.original_intent())?;
    usage.record_signed(trust.approval())?;
    Ok(trust)
}

impl Store {
    pub(crate) fn bound_node_task_policy_selection(
        &self,
        identity: &LocalIdentity,
        binding: &SignedBinding,
    ) -> Result<(
        vibeface_secure::authorization::ResourceRef,
        RemoteExecutionPolicySelection,
        PrepareContext,
    )> {
        let _carrier = ScopedSignatureVerification::new();
        let local = PublicKey::parse(&identity.public_key_hex()?)?;
        let tx = self.connection.unchecked_transaction()?;
        let mut usage = ExecutionProofUsage::default();
        let association = binding_association_in(&tx, &self.policy, binding, &mut usage)?;
        let trust = load_task_execution_trust_in(&tx, &local, &association.approval_ref)?;
        let original = trust.original_intent();
        let current = checked_source_task_binding_in(&tx, original, &local, &mut usage)?
            .context("bound SDK selection has no current Task binding")?;
        let project = original.payload.source_selection.resource.clone();
        let selection = original.payload.selection.clone();
        let context = original.payload.context.clone();
        let scope = &context.context;
        let session = scope
            .execution_session
            .as_ref()
            .context("bound SDK selection has no execution session")?;
        ensure!(
            current == *binding
                && binding.binding.network_id == project.space.as_str()
                && scope.workspace_resource.as_ref() == Some(&project.resource)
                && scope.task.as_ref().map(|task| task.as_str())
                    == Some(binding.binding.task_ref.as_str())
                && scope.conversation.as_deref() == Some(binding.binding.conversation_id.as_str())
                && scope.sdk_session.is_none()
                && context.binding_generation == Some(binding.binding.generation)
                && session.session_id == binding.binding.session_id
                && session.target
                    == vibeface_secure::authorization::DeviceId::from_key(
                        &selection.target_public_key
                    )
                && binding.binding.target_node
                    == crate::crypto::fingerprint(selection.target_public_key.as_str())?
                && scope.executor_kind.as_deref() == binding.binding.open_spec["executor"].as_str()
                && context.canonical_cwd.as_deref() == binding.binding.open_spec["cwd"].as_str()
                && context.execution_target_public_key.as_ref()
                    == Some(&selection.target_public_key),
            "bound SDK selection differs from its immutable approved Task binding"
        );
        usage.record_signed(original)?;
        usage.record_signed(trust.approval())?;
        usage.enforce_limits()?;
        tx.commit()?;
        Ok((project, selection, context))
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn prepare_node_task_policy_read(
        &mut self,
        identity: &LocalIdentity,
        parent: &VerifiedAuthorization,
        service: &ServiceId,
        service_grant: &ObjectHash,
        facts: &RequestFacts,
        selection: &RemoteExecutionPolicySelection,
        context: &PrepareContext,
        work: &ExecutionWorkIntent,
        purpose: RemoteExecutionPolicyPurpose,
        now: u64,
        expires: u64,
    ) -> Result<PendingNodeTaskRead> {
        let _carrier = ScopedSignatureVerification::new();
        let signer = super::super::client_policy_read::signer(identity)?;
        let local = PublicKey::of(&signer);
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let raw = super::super::admission::sdk_policy_source_provenance_in(
            &tx,
            &self.policy,
            &local,
            service,
            service_grant,
            facts,
            now,
        )
        .with_context(|| {
            format!(
                "SDK Node policy source collection; actual={:?}",
                SignatureVerification::current_counts()
            )
        })?;
        let verified = verify_sdk_source_provenance(
            self.policy.verified(),
            parent,
            &raw,
            service,
            service_grant,
            facts,
            raw.current_binding.as_ref(),
            now,
        )
        .with_context(|| {
            format!(
                "SDK Node policy source verification; actual={:?}",
                SignatureVerification::current_counts()
            )
        })?;
        let mut usage = verified.proof_usage().clone();
        let trust = current_trust_in(
            &tx,
            &self.policy,
            &local,
            &raw,
            selection,
            context,
            work,
            purpose,
            &mut usage,
        )
        .with_context(|| {
            format!(
                "SDK Node policy approved roots; proof={:?}; actual={:?}",
                usage.counts(),
                SignatureVerification::current_counts()
            )
        })?;
        let pins: Vec<_> = self
            .policy
            .pins()
            .iter()
            .filter(|pin| {
                raw.source_heads
                    .iter()
                    .any(|head| head.domain == pin.domain)
            })
            .cloned()
            .collect();
        let (source, request) = verify_node_service_policy_source(
            self.policy.verified(),
            &pins,
            &trust,
            parent,
            service,
            service_grant,
            facts,
            selection,
            context,
            work,
            raw.current_binding.as_ref(),
            &raw,
            &signer,
            purpose,
            now,
            expires.min(verified.expires_at_ms()),
        )
        .with_context(|| {
            format!(
                "SDK Node policy native source factory; proof={:?}; actual={:?}",
                usage.counts(),
                SignatureVerification::current_counts()
            )
        })?;
        match &request {
            NodeExecutionPolicyReadRequest::Current(value) => usage.record_signed(value)?,
            NodeExecutionPolicyReadRequest::History(value) => usage.record_signed(value)?,
        }
        let digest = request
            .source_provenance()
            .context("SDK source factory omitted its signed original reference")?;
        ensure!(
            *digest == ObjectHash::body(&raw)?,
            "SDK source factory changed its source original"
        );
        usage.record_metadata(&("node-policy-source-request-v2", ObjectHash::body(&request)?))?;
        if let Some(counts) = SignatureVerification::current_counts() {
            usage.record_actual_signature_counts(counts);
        }
        usage.enforce_limits().with_context(|| {
            format!(
                "SDK Node policy request proof budget; proof={:?}",
                usage.counts()
            )
        })?;
        save_exact_in(&tx, &key("sdk-source", digest), &raw)?;
        let typed = TaskExecutionPolicyRequest::Node(request.clone());
        save_exact_in(&tx, &key("request", &typed.request_hash()?), &typed)?;
        tx.commit()?;
        Ok(PendingNodeTaskRead {
            source,
            trust,
            request,
        })
    }

    pub(crate) fn begin_node_task_policy_read(
        &mut self,
        pending: PendingNodeTaskRead,
        snapshot: &Signed<RemoteExecutionPolicySnapshot>,
        peer: &ActualPolicyNoisePeer,
        now: u64,
    ) -> Result<ObjectHash> {
        let _carrier = ScopedSignatureVerification::new();
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        current_node_source_in(&tx, &self.policy, &pending.request, now)?;
        let typed = TaskExecutionPolicyRequest::Node(pending.request.clone());
        let request_ref = typed.request_hash()?;
        let original: TaskExecutionPolicyRequest = read_in(&tx, &key("request", &request_ref))?
            .context("Node policy original request is missing")?;
        ensure!(
            original == typed,
            "Node policy pending source changed its signed request"
        );
        let floors = ingress::floors_in(&tx, pending.request.execution_pins())?;
        let importer = TaskPolicyImport::Node(Box::new(begin_node_task_policy_import(
            &pending.trust,
            &pending.source,
            &pending.request,
            snapshot,
            &floors,
            peer,
            now,
        )?));
        let approval_ref = pending.trust.approval().hash()?;
        let snapshot_ref = ObjectHash::body(snapshot)?;
        let reference = ObjectHash::body(&(
            "task-policy-import-v2",
            &approval_ref,
            &request_ref,
            &snapshot_ref,
        ))?;
        let record = ingress::ImportRecord {
            approval_ref,
            request_ref,
            snapshot_ref,
            page_count: 0,
            ready_for_finish: false,
        };
        let mut usage = importer.proof_usage().clone();
        usage.record_metadata(&("node-policy-import-v2", &reference))?;
        if let Some(counts) = SignatureVerification::current_counts() {
            usage.record_actual_signature_counts(counts);
        }
        usage.enforce_limits()?;
        save_exact_in(&tx, &key("snapshot", &record.snapshot_ref), snapshot)?;
        if let Some(saved) = read_in::<ingress::ImportRecord>(&tx, &key("import", &reference))? {
            ensure!(
                saved.approval_ref == record.approval_ref
                    && saved.request_ref == record.request_ref
                    && saved.snapshot_ref == record.snapshot_ref,
                "Node policy import changed its original source"
            );
        } else {
            save_exact_in(&tx, &key("import", &reference), &record)?;
        }
        tx.commit()?;
        self.foreign_execution_policy
            .pending
            .entry(reference.clone())
            .or_insert(ingress::CachedImport {
                importer,
                page_hashes: Vec::new(),
                ready_for_finish: false,
            });
        Ok(reference)
    }

    pub(crate) fn append_node_task_policy_page(
        &mut self,
        reference: &ObjectHash,
        page: &Signed<RemoteExecutionPolicyPage>,
        now: u64,
    ) -> Result<()> {
        let _carrier = ScopedSignatureVerification::new();
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let (saved, request) = node_request_in(&tx, reference)?;
        current_node_source_in(&tx, &self.policy, &request, now)?;
        let cached = self
            .foreign_execution_policy
            .pending
            .get(reference)
            .context("NeedPolicyResume: Node policy import must resume from begin")?;
        let index = page.payload.page_index;
        let hash = ObjectHash::body(page)?;
        if index < cached.page_hashes.len() as u64 {
            ensure!(
                cached.page_hashes.get(index as usize) == Some(&hash),
                "Node policy retry changed its signed page"
            );
            let mut usage = cached.importer.proof_usage().clone();
            usage.record_metadata(&("node-policy-page-retry-v2", reference, index))?;
            if let Some(counts) = SignatureVerification::current_counts() {
                usage.record_actual_signature_counts(counts);
            }
            usage.enforce_limits()?;
            return Ok(());
        }
        ensure!(
            index == cached.page_hashes.len() as u64,
            "Node policy page is out of order"
        );
        let mut importer = cached.importer.clone();
        importer.append_page(page, now)?;
        let mut usage = importer.proof_usage().clone();
        usage.record_metadata(&("node-policy-page-v2", reference, index))?;
        if let Some(counts) = SignatureVerification::current_counts() {
            usage.record_actual_signature_counts(counts);
        }
        usage.enforce_limits()?;
        save_exact_in(&tx, &ingress::page_key(reference, index), page)?;
        if index >= saved.page_count {
            ensure!(
                index == saved.page_count && !saved.ready_for_finish,
                "Node policy original cursor changed"
            );
            let mut next = saved.clone();
            next.page_count += 1;
            next.ready_for_finish = page.payload.complete;
            ensure!(
                tx.execute(
                    "UPDATE identity_meta SET value=? WHERE key=? AND value=?",
                    params![
                        serde_json::to_string(&next)?,
                        key("import", reference),
                        serde_json::to_string(&saved)?
                    ]
                )? == 1,
                "Node policy original cursor changed during append"
            );
        }
        tx.commit()?;
        let cached = self
            .foreign_execution_policy
            .pending
            .get_mut(reference)
            .context("Node policy pending source disappeared")?;
        cached.importer = importer;
        cached.page_hashes.push(hash);
        cached.ready_for_finish = page.payload.complete;
        Ok(())
    }

    pub(crate) fn finish_node_task_policy_read(
        &mut self,
        reference: &ObjectHash,
        witness: &Signed<RemoteExecutionPolicyWitness>,
        now: u64,
    ) -> Result<ObjectHash> {
        let _carrier = ScopedSignatureVerification::new();
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let (saved, request) = node_request_in(&tx, reference)?;
        current_node_source_in(&tx, &self.policy, &request, now)?;
        let cached = self
            .foreign_execution_policy
            .pending
            .get(reference)
            .context("NeedPolicyResume: Node policy import must resume from begin")?;
        ensure!(
            cached.ready_for_finish
                && saved.ready_for_finish
                && cached.page_hashes.len() as u64 == saved.page_count,
            "Node policy import has not consumed all original pages"
        );
        let policy = cached.importer.clone().finish_current(witness, now)?;
        let mut usage = policy.proof_usage().clone();
        let complete = ingress::CompletionRecord {
            import_ref: reference.clone(),
            witness_ref: ObjectHash::body(witness)?,
            completed_at_ms: now,
        };
        let policy_ref = ObjectHash::body(&(
            "task-policy-completion-v2",
            reference,
            &complete.witness_ref,
        ))?;
        usage.record_metadata(&("node-policy-completion-v2", &policy_ref))?;
        if let Some(counts) = SignatureVerification::current_counts() {
            usage.record_actual_signature_counts(counts);
        }
        usage.enforce_limits()?;
        if let Some(old) =
            read_in::<ingress::CompletionRecord>(&tx, &key("completion", &policy_ref))?
        {
            ensure!(
                old.import_ref == complete.import_ref && old.witness_ref == complete.witness_ref,
                "Node policy completion changed its original witness"
            );
        } else {
            save_exact_in(&tx, &key("completion", &policy_ref), &complete)?;
        }
        save_exact_in(&tx, &key("witness", &complete.witness_ref), witness)?;
        ingress::advance_floors_in(&tx, policy.pins(), policy.foreign_heads())?;
        save_exact_in(&tx, &key("completed-import", reference), &policy_ref)?;
        let work_key = provenance::TaskExecutionPolicyProvenance::work_key(&policy)?;
        tx.commit()?;
        self.policy
            .task_execution_policy
            .insert_current(work_key, policy_ref.clone(), policy);
        Ok(policy_ref)
    }
}

#[derive(Clone)]
pub(super) enum TaskPolicyImport {
    Account(Box<TaskExecutionPolicyImport>),
    Node(Box<NodeTaskExecutionPolicyImport>),
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
    pub(super) fn finish_current(
        self,
        witness: &Signed<RemoteExecutionPolicyWitness>,
        now: u64,
    ) -> Result<VerifiedTaskCurrentPolicy> {
        Ok(match self {
            Self::Account(value) => (*value).finish_current(witness, now)?,
            Self::Node(value) => (*value).finish_current(witness, now)?,
        })
    }
    pub(super) fn finish_history(
        self,
        witness: &Signed<RemoteExecutionPolicyWitness>,
        now: u64,
    ) -> Result<VerifiedTaskHistoryPolicy> {
        Ok(match self {
            Self::Account(value) => (*value).finish_history(witness, now)?,
            Self::Node(value) => (*value).finish_history(witness, now)?,
        })
    }
    pub(super) fn finish_original_admission(
        self,
        witness: &Signed<RemoteExecutionPolicyWitness>,
        proof: &crate::model::AdmissionProof,
    ) -> Result<VerifiedTaskOriginalAdmissionPolicy> {
        Ok(match self {
            Self::Account(value) => (*value).finish_original_admission(witness, proof)?,
            Self::Node(value) => (*value).finish_original_admission(witness, proof)?,
        })
    }
    pub(super) fn finish_retained_floors(
        self,
        witness: &Signed<RemoteExecutionPolicyWitness>,
        clock: u64,
    ) -> Result<Vec<vibeface_secure::authorization::PolicyPin>> {
        Ok(match self {
            Self::Account(value) => (*value).finish_retained_floors(witness, clock)?,
            Self::Node(value) => (*value).finish_retained_floors(witness, clock)?,
        })
    }
}
