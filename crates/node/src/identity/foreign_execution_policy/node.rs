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
    remote_execution_policy::{
        RemoteExecutionPolicyPage, RemoteExecutionPolicyPageRequest, RemoteExecutionPolicyWitness,
    },
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
    Ok(raw)
}

fn current_trust_in(
    connection: &Connection,
    primary: &super::super::journal::PolicyState,
    source: &PublicKey,
    raw: &SdkPolicySourceProvenance,
    selection: &RemoteExecutionPolicySelection,
    context: &PrepareContext,
    usage: &mut ExecutionProofUsage,
) -> Result<VerifiedTaskExecutionTrust> {
    let reference = if let Some(binding) = raw.current_binding.as_ref() {
        binding_association_in(connection, primary, binding, usage)?.approval_ref
    } else {
        let task = context
            .context
            .task
            .as_ref()
            .context("SDK policy source has no Task")?;
        let prefix = task_approval_prefix(&raw.child_facts.resource, task)?;
        let mut statement =
            connection.prepare("SELECT value FROM identity_meta WHERE key GLOB ? ORDER BY key")?;
        let references = statement
            .query_map([format!("{prefix}*")], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut selected = None;
        for value in references {
            ensure!(
                value.len() <= MAX_PROOF_BYTES,
                "SDK Task approval index is too large"
            );
            let reference: ObjectHash = serde_json::from_str(&value)?;
            let original: Signed<RemoteExecutionPolicyReadIntent> =
                read_in(connection, &key("approved-intent", &reference))?
                    .context("SDK Task approval original is missing")?;
            if original.payload.selection == *selection && original.payload.context == *context {
                selected = Some(reference);
                break;
            }
        }
        selected.context("NeedPolicySync: initial SDK Task choice has no independent approval")?
    };
    let trust = load_task_execution_trust_in(connection, source, &reference)?;
    usage.record_signed(trust.original_intent())?;
    usage.record_signed(trust.approval())?;
    Ok(trust)
}

impl Store {
    pub(crate) fn prepare_node_task_policy_read(
        &mut self,
        identity: &LocalIdentity,
        parent: &VerifiedAuthorization,
        service: &ServiceId,
        service_grant: &ObjectHash,
        facts: &RequestFacts,
        selection: &RemoteExecutionPolicySelection,
        context: &PrepareContext,
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
        )?;
        let verified = verify_sdk_source_provenance(
            self.policy.verified(),
            parent,
            &raw,
            service,
            service_grant,
            facts,
            raw.current_binding.as_ref(),
            now,
        )?;
        let mut usage = verified.proof_usage().clone();
        let trust = current_trust_in(
            &tx,
            &self.policy,
            &local,
            &raw,
            selection,
            context,
            &mut usage,
        )?;
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
            raw.current_binding.as_ref(),
            &raw,
            &signer,
            purpose,
            now,
            expires.min(verified.expires_at_ms()),
        )?;
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
        usage.enforce_limits()?;
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
        let task_key = provenance::TaskExecutionPolicyProvenance::task_key(&policy)?;
        tx.commit()?;
        self.policy
            .task_execution_policy
            .insert_current(task_key, policy_ref.clone(), policy);
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
