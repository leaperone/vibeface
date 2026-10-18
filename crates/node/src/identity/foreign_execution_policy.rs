use super::{
    client_policy_read::check_heads_in, remote_execution_policy::checked_source_task_binding_in,
};
use crate::{binding::SignedBinding, crypto::LocalIdentity, store::Store};
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::{Value, json};
use vibeface_secure::{
    Identity,
    authorization::{
        Action, MAX_PROOF_BYTES, ObjectHash, PublicKey, Signed,
        execution_handoff::ExecutionProofUsage,
        node_execution_policy::{
            TaskExecutionTrustApproval, VerifiedTaskExecutionTrust,
            approve_task_execution_policy as approve_native, task_execution_policy_choice_key,
            verify_task_execution_trust,
        },
        remote_execution_policy::{
            RemoteExecutionPolicyPurpose, RemoteExecutionPolicyReadIntent,
            verify_remote_execution_policy_delegation_source,
        },
    },
    signature_verification::{ScopedSignatureVerification, SignatureVerification},
};

const PREFIX: &str = "task-execution-policy-v2";

mod ingress;
pub(crate) use ingress::ForeignExecutionPolicyCache;
mod node;
pub(crate) use node::PendingNodeTaskRead;
mod provenance;
pub(crate) use provenance::{
    TaskExecutionPolicyProvenance, bind_task_execution_policy_in, check_task_policy_heads_in,
    current_task_execution_policy_in, current_task_prepare_policy_in,
    original_task_execution_policy_in, rehydrate_task_execution_policy_in,
    task_policy_candidates_in, task_policy_for_binding_in, validate_task_execution_policy_links_in,
};

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ApprovalNonce {
    intent_envelope_hash: ObjectHash,
    approval_ref: ObjectHash,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct AdmissionAssociation {
    policy_ref: ObjectHash,
    approval_ref: ObjectHash,
    invocation_hash: ObjectHash,
    authorization_id: String,
    body_hash: ObjectHash,
    binding_hash: String,
    admitted_at_ms: u64,
}

fn binding_association_key(binding: &SignedBinding) -> Result<String> {
    Ok(format!(
        "{PREFIX}:binding-admission:{}",
        crate::binding::binding_hash(&binding.binding)?
    ))
}

fn retained_binding_in(
    connection: &Connection,
    space: &str,
    hash: &str,
    local: &PublicKey,
) -> Result<SignedBinding> {
    let value: String = connection.query_row(
        "SELECT value FROM local_values WHERE space_id=? AND key=? AND length(CAST(value AS BLOB))<=?",
        params![space,format!("task-binding-record:{hash}"),MAX_PROOF_BYTES], |row| row.get(0),
    )?;
    let binding: SignedBinding = serde_json::from_str(&value)?;
    crate::binding::verify_binding(&binding, local.as_str())?;
    ensure!(
        crate::binding::binding_hash(&binding.binding)? == hash
            && binding.binding.network_id == space,
        "foreign policy retained binding changed its original hash or space"
    );
    Ok(binding)
}

fn binding_association_in(
    connection: &Connection,
    primary: &super::journal::PolicyState,
    binding: &SignedBinding,
    usage: &mut ExecutionProofUsage,
) -> Result<AdmissionAssociation> {
    let reference: ObjectHash = read_in(connection, &binding_association_key(binding)?)?
        .context("NeedPolicySync: current binding has no immutable foreign policy association")?;
    let association: AdmissionAssociation = read_in(connection, &key("admission", &reference))?
        .context("NeedPolicySync: foreign policy binding admission is missing")?;
    ensure!(
        association.invocation_hash == reference
            && association.binding_hash == crate::binding::binding_hash(&binding.binding)?,
        "foreign policy binding association changed its original admission"
    );
    let proof = super::admission::load_admission_in(
        connection,
        &binding.binding.network_id,
        &association.authorization_id,
    )?;
    ensure!(
        proof.invocation.hash()? == association.invocation_hash
            && ObjectHash::body(&proof.source_body)? == association.body_hash
            && proof.admitted_at_ms == association.admitted_at_ms
            && matches!(
                proof.source_body["method"].as_str(),
                Some("bind_execution" | "rebind_execution")
            ),
        "foreign policy association differs from its original accepted binding admission"
    );
    let source = provenance::association_source(primary, &association)?;
    let original =
        provenance::original_task_execution_policy_in(connection, primary, &source, &proof)?
            .context("foreign binding association has no original verified admission")?;
    ensure!(
        original.trust().approval().hash()? == association.approval_ref,
        "foreign binding association changed its actual approved roots or writer"
    );
    usage.merge(original.proof_usage());
    Ok(association)
}

fn read_binding_in(
    connection: &Connection,
    identity: &LocalIdentity,
    intent: &Signed<RemoteExecutionPolicyReadIntent>,
    usage: &mut ExecutionProofUsage,
) -> Result<Option<SignedBinding>> {
    let local = PublicKey::parse(&identity.public_key_hex()?)?;
    if intent.payload.purpose == RemoteExecutionPolicyPurpose::History {
        let reference = intent
            .payload
            .history
            .as_ref()
            .context("History has no original artifact")?;
        let source = &reference.source.payload;
        let binding = retained_binding_in(
            connection,
            source.project.space.as_str(),
            &source.binding_hash,
            &local,
        )?;
        ensure!(
            binding.binding.task_ref == source.task.as_str()
                && binding.binding.generation == source.binding_generation
                && binding.binding.session_id == source.execution_session.session_id
                && binding.binding.target_node
                    == crate::crypto::fingerprint(reference.target_public_key().as_str())?,
            "foreign History binding changed its original Task, generation, target or session"
        );
        usage.record_binding(&binding)?;
        Ok(Some(binding))
    } else {
        checked_source_task_binding_in(connection, intent, &local, usage)
    }
}

fn check_read_trust(
    intent: &Signed<RemoteExecutionPolicyReadIntent>,
    trust: &VerifiedTaskExecutionTrust,
) -> Result<()> {
    let current = &intent.payload;
    let old = &trust.original_intent().payload;
    ensure!(
        current.source_selection.resource == old.source_selection.resource
            && current.source_selection.audience == old.source_selection.audience
            && current.context.context.task == old.context.context.task
            && current.context.context.conversation == old.context.context.conversation
            && current.context.context.workspace_resource == old.context.context.workspace_resource
            && current.context.context.execution_session == old.context.context.execution_session
            && current.context.context.executor_kind == old.context.context.executor_kind
            && current.context.binding_generation == old.context.binding_generation
            && current.selection.execution_resource == old.selection.execution_resource
            && current.selection.target_public_key == old.selection.target_public_key,
        "foreign policy read escaped its immutable Task selection"
    );
    for approved in trust.pins() {
        ensure!(
            current
                .execution_pins
                .iter()
                .any(|pin| pin.domain == approved.domain
                    && pin.genesis_root == approved.genesis_root),
            "foreign policy read changed its independently approved genesis"
        );
    }
    if current.purpose == RemoteExecutionPolicyPurpose::Execution {
        ensure!(
            current.selection == old.selection
                && current.context.canonical_cwd == old.context.canonical_cwd,
            "foreign policy writer changed its grant or cwd without rebind"
        );
    }
    Ok(())
}

fn trust_for_read_in(
    connection: &Connection,
    primary: &super::journal::PolicyState,
    identity: &LocalIdentity,
    intent: &Signed<RemoteExecutionPolicyReadIntent>,
    supplied: Option<&ObjectHash>,
    usage: &mut ExecutionProofUsage,
) -> Result<VerifiedTaskExecutionTrust> {
    let local = PublicKey::parse(&identity.public_key_hex()?)?;
    let binding = read_binding_in(connection, identity, intent, usage)?;
    let candidate = intent.payload.purpose == RemoteExecutionPolicyPurpose::Execution
        && ((binding.is_none() && intent.payload.expected_action == Action::ExecutionOpen)
            || (binding.is_some() && intent.payload.expected_action == Action::ExecutionRebind));
    let reference = if candidate {
        let choice = task_execution_policy_choice_key(intent)?;
        let reference: ObjectHash = read_in(connection, &key("choice", &choice))?
            .context("NeedPolicySync: foreign Task choice has not been approved")?;
        reference
    } else {
        binding_association_in(
            connection,
            primary,
            binding
                .as_ref()
                .context("foreign read has no original binding")?,
            usage,
        )?
        .approval_ref
    };
    ensure!(
        supplied.is_none_or(|supplied| *supplied == reference),
        "foreign policy read supplied another Task approval"
    );
    let trust = load_task_execution_trust_in(connection, &local, &reference)?;
    if candidate {
        check_candidate_anchor(&trust, intent, binding.as_ref())?;
    } else {
        check_read_trust(intent, &trust)?;
    }
    usage.record_signed(trust.original_intent())?;
    usage.record_signed(trust.approval())?;
    Ok(trust)
}

fn key(kind: &str, reference: &ObjectHash) -> String {
    format!("{PREFIX}:{kind}:{reference}")
}

fn task_approval_prefix(
    project: &vibeface_secure::authorization::ResourceRef,
    task: &vibeface_secure::authorization::EventRef,
) -> Result<String> {
    Ok(format!(
        "{PREFIX}:task-approval:{}:",
        ObjectHash::body(&(project, task))?
    ))
}

fn value_in(connection: &Connection, key: &str) -> Result<Option<String>> {
    let bytes: Option<usize> = connection
        .query_row(
            "SELECT length(CAST(value AS BLOB)) FROM identity_meta WHERE key=?",
            [key],
            |row| row.get(0),
        )
        .optional()?;
    ensure!(
        bytes.is_none_or(|bytes| bytes <= MAX_PROOF_BYTES),
        "stored foreign execution policy carrier exceeds 64 KiB"
    );
    Ok(connection
        .query_row(
            "SELECT value FROM identity_meta WHERE key=?",
            [key],
            |row| row.get(0),
        )
        .optional()?)
}

fn read_in<T: DeserializeOwned>(connection: &Connection, key: &str) -> Result<Option<T>> {
    value_in(connection, key)?
        .map(|value| serde_json::from_str(&value).map_err(anyhow::Error::from))
        .transpose()
}

fn save_exact_in<T: Serialize>(connection: &Connection, key: &str, value: &T) -> Result<()> {
    let encoded = serde_json::to_string(value)?;
    ensure!(
        encoded.len() <= MAX_PROOF_BYTES,
        "foreign execution policy carrier exceeds 64 KiB"
    );
    if let Some(saved) = value_in(connection, key)? {
        ensure!(
            saved == encoded,
            "foreign execution policy retry changed its original carrier"
        );
    } else {
        connection.execute(
            "INSERT INTO identity_meta(key,value) VALUES(?,?)",
            params![key, encoded],
        )?;
    }
    Ok(())
}

pub(crate) fn load_task_execution_trust_in(
    connection: &Connection,
    source_node: &PublicKey,
    approval_ref: &ObjectHash,
) -> Result<VerifiedTaskExecutionTrust> {
    let approval: Signed<TaskExecutionTrustApproval> =
        read_in(connection, &key("approval", approval_ref))?
            .context("task execution policy approval is missing")?;
    ensure!(
        approval.hash()? == *approval_ref,
        "task execution policy approval reference changed"
    );
    let intent: Signed<RemoteExecutionPolicyReadIntent> =
        read_in(connection, &key("approved-intent", approval_ref))?
            .context("task execution policy original intent is missing")?;
    Ok(verify_task_execution_trust(
        &intent,
        &approval,
        source_node,
    )?)
}

fn check_candidate_anchor(
    trust: &VerifiedTaskExecutionTrust,
    intent: &Signed<RemoteExecutionPolicyReadIntent>,
    current: Option<&SignedBinding>,
) -> Result<()> {
    ensure!(
        task_execution_policy_choice_key(intent)? == *trust.choice_key(),
        "task execution policy changed its approved choice or genesis roots"
    );
    match current {
        None => ensure!(
            intent.payload.expected_action == Action::ExecutionOpen
                && trust.original_binding().is_none()
                && intent.payload.context.binding_generation == Some(1),
            "unbound policy approval is not its original first open"
        ),
        Some(current) => ensure!(
            intent.payload.expected_action == Action::ExecutionRebind
                && trust.original_binding() == Some(current)
                && current.binding.generation.checked_add(1)
                    == intent.payload.context.binding_generation,
            "bound policy choice requires its exact candidate anchor or admission association"
        ),
    }
    Ok(())
}

impl Store {
    pub(crate) fn foreign_execution_policy_selection(
        &mut self,
        identity: &LocalIdentity,
        principal: &str,
        params: &Value,
        now: u64,
    ) -> Result<Value> {
        let _signatures = ScopedSignatureVerification::new();
        let intent: Signed<RemoteExecutionPolicyReadIntent> =
            serde_json::from_value(params.clone())?;
        let local = PublicKey::parse(&identity.public_key_hex()?)?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Deferred)?;
        let source = verify_remote_execution_policy_delegation_source(
            self.policy.verified(),
            &intent,
            &local,
            &PublicKey::parse(principal)?,
            now,
        )?;
        check_heads_in(&tx, &self.policy, source.source_heads())?;
        self.policy.accepted_journal_snapshot(&tx)?;
        let mut usage = source.proof_usage().clone();
        if intent.payload.purpose == RemoteExecutionPolicyPurpose::History {
            usage.merge(
                &super::execution_dispatch::retained_execution_history_reference_in(
                    &tx,
                    identity,
                    intent
                        .payload
                        .history
                        .as_ref()
                        .context("History has no original artifact")?,
                )?,
            );
        }
        let binding = read_binding_in(&tx, identity, &intent, &mut usage)?;
        let output = if let Some(binding) = &binding {
            let association = binding_association_in(&tx, &self.policy, binding, &mut usage)?;
            let trust = load_task_execution_trust_in(&tx, &local, &association.approval_ref)?;
            usage.record_signed(trust.original_intent())?;
            usage.record_signed(trust.approval())?;
            json!({"binding":binding,"approval_ref":association.approval_ref,"policy_ref":association.policy_ref,
                "selection":trust.selection(),"approved_pins":trust.pins()})
        } else {
            ensure!(
                intent.payload.purpose == RemoteExecutionPolicyPurpose::Execution
                    && intent.payload.expected_action == Action::ExecutionOpen
                    && intent.payload.context.binding_generation == Some(1),
                "only first execution open may select an unbound Task"
            );
            json!({"binding":null,"approval_ref":null,"policy_ref":null,"selection":null,"approved_pins":[]})
        };
        let output = ingress::response(params, output, usage)?;
        tx.commit()?;
        Ok(output)
    }

    pub(crate) fn foreign_execution_policy_approve(
        &mut self,
        identity: &LocalIdentity,
        principal: &str,
        params: &Value,
        now: u64,
    ) -> Result<Value> {
        let intent: Signed<RemoteExecutionPolicyReadIntent> =
            serde_json::from_value(params.clone())?;
        let response_size = serde_json::to_vec(&json!({
            "approval_ref":intent.hash()?,
            "choice_ref":task_execution_policy_choice_key(&intent)?,
        }))?
        .len();
        ensure!(
            serde_json::to_vec(params)?.len() + response_size <= MAX_PROOF_BYTES,
            "task policy approval input and output exceed 64 KiB"
        );
        let trust = self.approve_task_execution_policy(identity, principal, &intent, now)?;
        Ok(json!({"approval_ref":trust.approval().hash()?,"choice_ref":trust.choice_key()}))
    }

    pub(crate) fn approve_task_execution_policy(
        &mut self,
        identity: &LocalIdentity,
        principal: &str,
        intent: &Signed<RemoteExecutionPolicyReadIntent>,
        now: u64,
    ) -> Result<VerifiedTaskExecutionTrust> {
        let _signatures = ScopedSignatureVerification::new();
        ensure!(
            intent.payload.purpose == RemoteExecutionPolicyPurpose::Execution
                && matches!(
                    intent.payload.expected_action,
                    Action::ExecutionOpen | Action::ExecutionRebind
                ),
            "only a current Task execution selection may approve foreign roots"
        );
        let signer =
            Identity::from_secret_hex(&identity.secret_key_hex).map_err(anyhow::Error::msg)?;
        let local = PublicKey::of(&signer);
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let source = verify_remote_execution_policy_delegation_source(
            self.policy.verified(),
            intent,
            &local,
            &PublicKey::parse(principal)?,
            now,
        )?;
        check_heads_in(&tx, &self.policy, source.source_heads())?;
        self.policy.accepted_journal_snapshot(&tx)?;
        let mut usage = source.proof_usage().clone();
        let current = checked_source_task_binding_in(&tx, intent, &local, &mut usage)?;
        let choice = task_execution_policy_choice_key(intent)?;
        let nonce_key = format!("{PREFIX}:nonce:{}:{}", intent.issuer, intent.payload.nonce);
        let envelope_hash = ObjectHash::body(intent)?;
        let saved_nonce: Option<ApprovalNonce> = read_in(&tx, &nonce_key)?;
        let saved_choice: Option<ObjectHash> = read_in(&tx, &key("choice", &choice))?;
        let saved = if let Some(nonce) = saved_nonce {
            ensure!(
                nonce.intent_envelope_hash == envelope_hash,
                "task policy approval nonce was reused with another signed intent"
            );
            ensure!(
                saved_choice.as_ref() == Some(&nonce.approval_ref),
                "task policy approval nonce changed its approved choice"
            );
            Some(nonce.approval_ref)
        } else {
            saved_choice
        };
        let trust = if let Some(reference) = saved {
            let trust = load_task_execution_trust_in(&tx, &local, &reference)?;
            check_candidate_anchor(&trust, intent, current.as_ref())?;
            trust
        } else {
            let trust = approve_native(&source, current.as_ref(), &signer, now)?;
            check_candidate_anchor(&trust, intent, current.as_ref())?;
            trust
        };
        let approval_ref = trust.approval().hash()?;
        usage.record_signed(trust.original_intent())?;
        usage.record_signed(trust.approval())?;
        if let Some(binding) = trust.original_binding() {
            usage.record_binding(binding)?;
        }
        let output = json!({"approval_ref":approval_ref,"choice_ref":trust.choice_key()});
        ensure!(
            serde_json::to_vec(intent)?.len() + serde_json::to_vec(&output)?.len()
                <= MAX_PROOF_BYTES,
            "task policy approval input and output exceed 64 KiB"
        );
        usage.record_metadata(&output)?;
        if let Some(counts) = SignatureVerification::current_counts() {
            ensure!(
                counts.actual_verifications <= 32,
                "task policy approval crypto budget exceeded"
            );
            usage.record_actual_signature_counts(counts);
        }
        usage.enforce_limits()?;
        save_exact_in(
            &tx,
            &key("approved-intent", &approval_ref),
            trust.original_intent(),
        )?;
        save_exact_in(&tx, &key("approval", &approval_ref), trust.approval())?;
        save_exact_in(&tx, &key("choice", &choice), &approval_ref)?;
        let scope_task = intent
            .payload
            .context
            .context
            .task
            .as_ref()
            .context("Task approval has no Task")?;
        save_exact_in(
            &tx,
            &format!(
                "{}{}",
                task_approval_prefix(&intent.payload.source_selection.resource, scope_task)?,
                approval_ref
            ),
            &approval_ref,
        )?;
        save_exact_in(&tx, &key("reader-intent", &envelope_hash), intent)?;
        save_exact_in(
            &tx,
            &nonce_key,
            &ApprovalNonce {
                intent_envelope_hash: envelope_hash,
                approval_ref,
            },
        )?;
        tx.commit()?;
        Ok(trust)
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::identity::{
        client_policy_read::verified_client_policy_for_test,
        test_support::{TestNetwork, now},
    };
    use vibeface_secure::authorization::{
        AUTH_VERSION, Actor, Audience, CheckStage, DeviceId, EventRef, Grant, GrantConstraints,
        Grantee, Invocation, InvocationContext, JournalEntry, Nonce, PolicyDomain, RequestFacts,
        ResourceId, ResourceKind, ResourceRecord, ResourceRef, ResourceScope, ServiceId,
        SessionRef, TransportEvidence, VerifiedClientAuthorization,
        client_policy_import::AuthorizationSelection,
        prepare::{
            PrepareContext, PrepareIntent, VerifiedRemoteExecution, verify_remote_execution,
        },
        remote_execution_policy::{
            RemoteExecutionPolicyPage, RemoteExecutionPolicyReadRequest,
            begin_remote_execution_policy_import, build_remote_execution_policy_read_intent,
        },
    };

    pub(crate) struct UnseededFixture {
        pub(crate) project: TestNetwork,
        pub(crate) executor: TestNetwork,
        pub(crate) store: Store,
        pub(crate) target: Store,
        pub(crate) source: VerifiedClientAuthorization,
        pub(crate) intent: Signed<RemoteExecutionPolicyReadIntent>,
    }

    pub(crate) fn unseeded_fixture() -> Result<UnseededFixture> {
        let project = TestNetwork::new()?;
        let executor = TestNetwork::new()?;
        let mut store = project.at(&project.directory.join("foreign-approval.sqlite3"))?;
        project.conversation(&mut store, "foreign-conversation")?;
        let task = project.task(&mut store, "foreign-conversation", "foreign-task")?;
        let target_key = PublicKey::parse(&executor.node.public_key_hex()?)?;
        let context = PrepareContext {
            context: InvocationContext {
                conversation: Some("foreign-conversation".into()),
                task: Some(EventRef::parse(&task.hash)?),
                execution_session: Some(SessionRef {
                    target: DeviceId::from_key(&target_key),
                    session_id: "foreign-session".into(),
                }),
                executor_kind: Some("codex".into()),
                workspace_resource: Some(project.resource.clone()),
                sdk_session: None,
            },
            binding_generation: Some(1),
            canonical_cwd: Some(project.project.to_string_lossy().into_owned()),
            execution_target_public_key: Some(target_key),
        };
        unseeded_fixture_with_context(project, executor, store, context, false)
    }

    pub(crate) fn unseeded_fixture_with_context(
        project: TestNetwork,
        executor: TestNetwork,
        mut store: Store,
        context: PrepareContext,
        delegate_execution_grant: bool,
    ) -> Result<UnseededFixture> {
        let local = PublicKey::parse(&project.node.public_key_hex()?)?;
        let audience = Audience {
            service: ServiceId::from_key(&local),
            device: DeviceId::from_key(&local),
        };
        let target_key = PublicKey::parse(&executor.node.public_key_hex()?)?;
        let resource = ResourceRef {
            space: executor.space.clone(),
            resource: ResourceId::from_bytes([234; 32]),
        };
        let mut target = executor.memory()?;
        let head = target
            .policy
            .verified()
            .head(&PolicyDomain::Space(executor.space.clone()))
            .context("executor policy is missing")?
            .clone();
        let execution_resource = Signed::sign(
            ResourceRecord {
                version: AUTH_VERSION,
                space: executor.space.clone(),
                resource: resource.resource.clone(),
                kind: ResourceKind::ExecutionService,
                host: ServiceId::from_key(&target_key),
                issued_under: head.clone(),
                nonce: Nonce::random()?,
            },
            &executor.root,
        )?;
        let grant = Signed::sign(
            Grant {
                execution_source: None,
                version: AUTH_VERSION,
                owner_space: executor.space.clone(),
                scope: ResourceScope::Exact(resource.clone()),
                grantee: Grantee::Actor(if delegate_execution_grant {
                    Actor::Account(executor.account.clone())
                } else {
                    Actor::Service(audience.service.clone())
                }),
                actions: if delegate_execution_grant {
                    vec![
                        Action::ExecutionOpen,
                        Action::ExecutionSend,
                        Action::ExecutionSubmit,
                        Action::ExecutionResolve,
                        Action::OperationStatusRead,
                        Action::OperationResolve,
                    ]
                } else {
                    vec![
                        Action::ExecutionOpen,
                        Action::ExecutionSend,
                        Action::ExecutionSubmit,
                        Action::OperationStatusRead,
                    ]
                },
                audience: Audience {
                    service: ServiceId::from_key(&target_key),
                    device: DeviceId::from_key(&target_key),
                },
                constraints: GrantConstraints {
                    context: InvocationContext {
                        conversation: context.context.conversation.clone(),
                        task: context.context.task.clone(),
                        execution_session: context.context.execution_session.clone(),
                        executor_kind: context.context.executor_kind.clone(),
                        workspace_resource: Some(resource.resource.clone()),
                        sdk_session: None,
                    },
                    ..Default::default()
                },
                parent: None,
                remaining_delegations: u8::from(delegate_execution_grant),
                issued_under: head.clone(),
                not_before_ms: now(),
                expires_at_ms: now() + 1_000_000,
                nonce: Nonce::random()?,
            },
            &executor.root,
        )?;
        let mut space = target
            .policy
            .verified()
            .space(&executor.space)
            .context("executor space is missing")?
            .clone();
        space.previous = Some(head);
        space.sequence += 1;
        space.resources.push(execution_resource.hash()?);
        space.resources.sort();
        space.grants.push(grant.hash()?);
        space.grants.sort();
        target.install_policy_journal(
            &[
                JournalEntry::Resource(execution_resource),
                JournalEntry::Grant(grant.clone()),
                JournalEntry::Space(Signed::sign(space, &executor.root)?),
            ],
            now(),
        )?;
        let grant = if delegate_execution_grant {
            let head = target
                .policy
                .verified()
                .head(&PolicyDomain::Space(executor.space.clone()))
                .context("executor parent grant policy is missing")?
                .clone();
            let mut child = grant.payload.clone();
            child.grantee = Grantee::Actor(Actor::Service(audience.service.clone()));
            child.actions.retain(|action| {
                !matches!(action, Action::ExecutionResolve | Action::OperationResolve)
            });
            child.parent = Some(grant.hash()?);
            child.remaining_delegations = 0;
            child.issued_under = head.clone();
            child.nonce = Nonce::random()?;
            let child = Signed::sign(child, &executor.client)?;
            let mut space = target
                .policy
                .verified()
                .space(&executor.space)
                .context("executor parent grant space is missing")?
                .clone();
            space.previous = Some(head);
            space.sequence += 1;
            space.grants.push(child.hash()?);
            space.grants.sort();
            target.install_policy_journal(
                &[
                    JournalEntry::Grant(child.clone()),
                    JournalEntry::Space(Signed::sign(space, &executor.root)?),
                ],
                now(),
            )?;
            child
        } else {
            grant
        };
        let source = verified_client_policy_for_test(
            &mut store,
            &project.node,
            &project.client,
            &project.account,
            AuthorizationSelection {
                resource: ResourceRef {
                    space: project.space.clone(),
                    resource: project.resource.clone(),
                },
                audience,
                grant_leaf: project.bundle.grant_leaf.clone(),
            },
            &project.pins,
            now(),
        )?;
        let intent = build_remote_execution_policy_read_intent(
            &source,
            &json!({
                "selection":{
                    "execution_resource":resource,
                    "target_public_key":target_key,
                    "execution_grant":grant.hash()?,
                },
                "context":context,
                "expected_action":Action::ExecutionOpen,
                "purpose":RemoteExecutionPolicyPurpose::Execution,
                "execution_pins":executor.pins,
            })
            .to_string(),
            &project.client,
            now() as f64,
            (now() + 60_000) as f64,
        )
        .map_err(anyhow::Error::msg)?;
        Ok(UnseededFixture {
            project,
            executor,
            store,
            target,
            source,
            intent: serde_json::from_str(&intent)?,
        })
    }

    fn carrier<T>(action: impl FnOnce() -> Result<T>) -> Result<T> {
        let signatures = SignatureVerification::new();
        let result = signatures.run(action)?;
        ensure!(
            signatures.counts().actual_verifications <= 32,
            "foreign approval carrier exceeded its crypto budget"
        );
        Ok(result)
    }

    pub(crate) fn import_current_policy(
        fixture: &mut UnseededFixture,
    ) -> Result<VerifiedRemoteExecution> {
        import_current_policy_in(
            &fixture.project,
            &fixture.executor,
            &mut fixture.store,
            &mut fixture.target,
            &fixture.source,
            &fixture.intent,
            None,
        )
    }

    pub(crate) fn import_current_policy_in(
        project: &TestNetwork,
        executor: &TestNetwork,
        store: &mut Store,
        target: &mut Store,
        source: &VerifiedClientAuthorization,
        intent: &Signed<RemoteExecutionPolicyReadIntent>,
        approval: Option<&VerifiedTaskExecutionTrust>,
    ) -> Result<VerifiedRemoteExecution> {
        let principal = project.client.public_key_hex();
        let approval_ref = if let Some(approval) = approval {
            approval.approval().hash()?
        } else {
            let approved = carrier(|| {
                store.foreign_execution_policy_approve(
                    &project.node,
                    &principal,
                    &serde_json::to_value(intent)?,
                    now(),
                )
            })?;
            serde_json::from_value(approved["approval_ref"].clone())?
        };
        let delegated = carrier(|| {
            store.remote_execution_policy_delegate(
                &project.node,
                &principal,
                &serde_json::to_value(intent)?,
                now(),
            )
        })?;
        let request = RemoteExecutionPolicyReadRequest {
            intent: intent.clone(),
            delegation: serde_json::from_value(delegated["delegation"].clone())?,
        };
        let prepared = carrier(|| {
            target.remote_execution_policy_prepare(
                &executor.node,
                &principal,
                &serde_json::to_value(&request)?,
                now(),
            )
        })?;
        let mut client_import = carrier(|| {
            begin_remote_execution_policy_import(
                source,
                &serde_json::to_string(&request)?,
                &prepared["snapshot"].to_string(),
                &serde_json::to_string(&executor.pins)?,
                &executor.node.public_key_hex()?,
                now() as f64,
            )
            .map_err(anyhow::Error::msg)
        })?;
        let begun = carrier(|| {
            store.foreign_execution_policy_begin(
                &project.node,
                &principal,
                &json!({"request":request,"snapshot":prepared["snapshot"],
                    "approval_ref":approval_ref}),
                now(),
            )
        })?;
        let import_ref: ObjectHash = serde_json::from_value(begun["import_ref"].clone())?;
        let mut pages = 0;
        loop {
            ensure!(
                pages < 128,
                "foreign cold-start policy stream did not finish"
            );
            let params = serde_json::from_str(
                &client_import
                    .page_request_json()
                    .map_err(anyhow::Error::msg)?,
            )?;
            let response = carrier(|| {
                target.remote_execution_policy_page(&executor.node, &principal, &params, now())
            })?;
            let page: Signed<RemoteExecutionPolicyPage> =
                serde_json::from_value(response["page"].clone())?;
            carrier(|| {
                store.foreign_execution_policy_page(
                    &project.node,
                    &principal,
                    &json!({"import_ref":import_ref,"page":page}),
                    now(),
                )
            })?;
            carrier(|| {
                client_import
                    .append_page(&serde_json::to_string(&page)?, now() as f64)
                    .map_err(anyhow::Error::msg)
            })?;
            pages += 1;
            if page.payload.complete {
                break;
            }
        }
        let params = serde_json::from_str(
            &client_import
                .page_request_json()
                .map_err(anyhow::Error::msg)?,
        )?;
        let finished = carrier(|| {
            target.remote_execution_policy_finish(&executor.node, &principal, &params, now())
        })?;
        carrier(|| {
            store.foreign_execution_policy_finish(
                &project.node,
                &principal,
                &json!({"import_ref":import_ref,"witness":finished["witness"]}),
                now(),
            )
        })?;
        let policy = carrier(|| {
            client_import
                .finish(&finished["witness"].to_string(), now() as f64)
                .map_err(anyhow::Error::msg)
        })?;
        carrier(|| {
            verify_remote_execution(source, &policy, now() as f64).map_err(anyhow::Error::msg)
        })
    }

    #[test]
    fn cold_original_policy_cannot_authorize_a_new_current_request() -> Result<()> {
        let mut fixture = unseeded_fixture()?;
        let remote = import_current_policy(&mut fixture)?;
        let context = &fixture.intent.payload.context;
        let local = PublicKey::parse(&fixture.project.node.public_key_hex()?)?;
        let audience = fixture.intent.payload.source_selection.audience.clone();
        let body = json!({
            "method":"bind_execution",
            "space_id":fixture.project.space,
            "resource_id":fixture.project.resource,
            "authorization_id":"foreign-original-bind",
            "execution_resource":fixture.intent.payload.selection.execution_resource,
            "target_public_key":fixture.intent.payload.selection.target_public_key,
            "execution_grant":fixture.intent.payload.selection.execution_grant,
            "conversation_id":"foreign-conversation",
            "task_id":"foreign-task",
            "task_ref":context.context.task,
            "target_node":fixture.executor.node.node_id()?,
            "session_id":"foreign-session",
            "binding_generation":1,
            "executor":"codex",
            "cwd":fixture.project.project,
        });
        let sign = |body: &Value| -> Result<(Signed<PrepareIntent>, Signed<Invocation>)> {
            let operation_id = body["authorization_id"]
                .as_str()
                .context("authorization ID missing")?;
            let prepared = carrier(|| {
                remote
                    .prepare(
                        &fixture.source,
                        &body.to_string(),
                        &serde_json::to_string(context)?,
                        "execution_open",
                        operation_id,
                        local.as_str(),
                        &fixture.project.client,
                        now() as f64,
                    )
                    .map_err(anyhow::Error::msg)
            })?;
            let prepared: Signed<PrepareIntent> = serde_json::from_str(&prepared)?;
            let invocation = carrier(|| {
                remote
                    .sign(
                        &fixture.source,
                        &serde_json::to_string(&prepared.payload.request)?,
                        &body.to_string(),
                        "execution_open",
                        operation_id,
                        local.as_str(),
                        &fixture.project.client,
                        now() as f64,
                    )
                    .map_err(anyhow::Error::msg)
            })?;
            Ok((prepared, serde_json::from_str(&invocation)?))
        };
        let (prepared, invocation) = sign(&body)?;
        let current = carrier(|| {
            current_task_prepare_policy_in(
                &fixture.store.connection,
                &fixture.store.policy,
                &local,
                &prepared,
                &body,
                now(),
            )?
            .context("fresh prepare lost its completed foreign policy")
        })?;
        assert_eq!(current.request().source_node(), &local);
        let signed = &invocation.payload;
        let facts = RequestFacts {
            resource: signed.resource.clone(),
            action: signed.action,
            audience: signed.audience.clone(),
            operation_id: signed.operation_id.clone(),
            body_hash: signed.body_hash.clone(),
            context: signed.context.clone(),
        };
        let transport = TransportEvidence::DirectNoise {
            device: DeviceId::from_key(&PublicKey::of(&fixture.project.client)),
        };
        let scoped = carrier(|| {
            fixture.store.authorize_scoped_request_for_body(
                &fixture.project.node,
                &invocation,
                &facts,
                &transport,
                &body,
                now(),
            )
        })?;
        let dispatch = carrier(|| {
            fixture
                .store
                .execution_dispatch_facts_in_scope(&scoped, &body, &audience, now())
        })?;
        let challenge = carrier(|| {
            fixture
                .store
                .issue_scoped_freshness(&scoped, CheckStage::Admit, now())
        })?;
        let mut receipts = carrier(|| {
            fixture
                .store
                .sign_current_receipts(&challenge, &fixture.project.node, now())
        })?;
        receipts.extend(carrier(|| {
            fixture
                .target
                .sign_current_receipts(&challenge, &fixture.executor.node, now())
        })?);
        let binding = carrier(|| {
            fixture.store.bind_execution_dispatch(
                &fixture.project.node,
                &dispatch,
                &scoped,
                &crate::identity::policy::FreshnessProof {
                    challenge,
                    receipts,
                },
                &transport,
                &body,
                now(),
            )
        })?;
        assert_eq!(binding.binding.generation, 1);
        let proof = crate::identity::admission::load_admission_in(
            &fixture.store.connection,
            fixture.project.space.as_str(),
            "foreign-original-bind",
        )?;
        assert_eq!(proof.invocation, invocation);
        assert_eq!(proof.admitted_at_ms, now());
        assert!(
            fixture
                .store
                .policy
                .verified()
                .account(&fixture.executor.account)
                .is_none()
        );
        let request = crate::model::OperationRequest {
            operation_id: "foreign-new-open".into(),
            network_id: fixture.project.space.to_string(),
            conversation_id: "foreign-conversation".into(),
            task_id: Some("foreign-task".into()),
            target_node: fixture.executor.node.node_id()?,
            session_id: "foreign-session".into(),
            expected_revision: 0,
            basis_heads: vec![binding.binding.task_ref.clone()],
            action: binding.binding.open_spec.clone(),
            resources: vec![
                binding.binding.open_spec["cwd"]
                    .as_str()
                    .context("binding cwd missing")?
                    .into(),
            ],
            binding: Some(binding),
        };
        let next_body = json!({
            "method":"operate",
            "space_id":fixture.project.space,
            "resource_id":fixture.project.resource,
            "authorization_id":"foreign-new-open",
            "execution_resource":fixture.intent.payload.selection.execution_resource,
            "target_public_key":fixture.intent.payload.selection.target_public_key,
            "execution_grant":fixture.intent.payload.selection.execution_grant,
            "request":request,
        });
        let (_, warm_invocation) = sign(&next_body)?;
        let signed = &warm_invocation.payload;
        let next_facts = RequestFacts {
            resource: signed.resource.clone(),
            action: signed.action,
            audience: signed.audience.clone(),
            operation_id: signed.operation_id.clone(),
            body_hash: signed.body_hash.clone(),
            context: signed.context.clone(),
        };
        let warm_scope = carrier(|| {
            fixture.store.authorize_scoped_request_for_body(
                &fixture.project.node,
                &warm_invocation,
                &next_facts,
                &transport,
                &next_body,
                now(),
            )
        })?;
        let warm_dispatch = carrier(|| {
            fixture.store.execution_dispatch_facts_in_scope(
                &warm_scope,
                &next_body,
                &audience,
                now(),
            )
        })?;
        assert_eq!(warm_dispatch.facts().operation_id, "foreign-new-open");
        let path = fixture.project.directory.join("foreign-approval.sqlite3");
        drop(fixture.store);
        let reopened = Store::open(&path)?;
        let original = carrier(|| {
            original_task_execution_policy_in(
                &reopened.connection,
                &reopened.policy,
                &local,
                &proof,
            )?
            .context("cold start lost the original accepted foreign admission")
        })?;
        assert_eq!(original.signed_invocation_hash(), &invocation.hash()?);
        assert_eq!(original.admitted_at_ms(), now());

        let (next_prepare, next_invocation) = sign(&next_body)?;
        assert_ne!(next_invocation.payload.nonce, invocation.payload.nonce);
        assert_ne!(next_invocation.payload.nonce, warm_invocation.payload.nonce);
        assert_ne!(next_invocation.hash()?, invocation.hash()?);
        assert_ne!(next_invocation.hash()?, warm_invocation.hash()?);
        let prepare_error = carrier(|| {
            current_task_prepare_policy_in(
                &reopened.connection,
                &reopened.policy,
                &local,
                &next_prepare,
                &next_body,
                now(),
            )
        })
        .err()
        .context("cold original authorized a new prepare")?;
        assert!(
            prepare_error.to_string().contains("NeedPolicySync"),
            "{prepare_error:#}"
        );
        let invocation_error = carrier(|| {
            current_task_execution_policy_in(
                &reopened.connection,
                &reopened.policy,
                &fixture.project.node,
                &next_invocation,
                &next_body,
                now(),
            )
        })
        .err()
        .context("cold original authorized a new invocation")?;
        assert!(
            invocation_error.to_string().contains("NeedPolicySync"),
            "{invocation_error:#}"
        );
        Ok(())
    }

    #[test]
    fn task_approval_reopens_originals_without_installing_foreign_policy() -> Result<()> {
        let mut fixture = unseeded_fixture()?;
        let principal = fixture.project.client.public_key_hex();
        let local = PublicKey::parse(&fixture.project.node.public_key_hex()?)?;
        let before: (u64, u64, u64) = fixture.store.connection.query_row(
            "SELECT (SELECT count(*) FROM policy_pins),(SELECT count(*) FROM policy_journal),(SELECT count(*) FROM current_policy_heads)",
            [], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )?;
        let trust = carrier(|| {
            fixture.store.approve_task_execution_policy(
                &fixture.project.node,
                &principal,
                &fixture.intent,
                now(),
            )
        })?;
        let reference = trust.approval().hash()?;
        assert_eq!(trust.original_intent(), &fixture.intent);
        assert!(
            fixture
                .store
                .policy
                .verified()
                .account(&fixture.executor.account)
                .is_none()
        );
        let retry = carrier(|| {
            fixture.store.approve_task_execution_policy(
                &fixture.project.node,
                &principal,
                &fixture.intent,
                now() + 1,
            )
        })?;
        assert_eq!(retry.approval(), trust.approval());
        let mut next_reader = fixture.intent.payload.clone();
        next_reader.nonce = Nonce::random()?;
        let next_reader = Signed::sign(next_reader, &fixture.project.client)?;
        let reused = carrier(|| {
            fixture.store.approve_task_execution_policy(
                &fixture.project.node,
                &principal,
                &next_reader,
                now() + 1,
            )
        })?;
        assert_eq!(reused.approval(), trust.approval());
        assert_eq!(reused.original_intent(), &fixture.intent);
        let after: (u64, u64, u64) = fixture.store.connection.query_row(
            "SELECT (SELECT count(*) FROM policy_pins),(SELECT count(*) FROM policy_journal),(SELECT count(*) FROM current_policy_heads)",
            [], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )?;
        assert_eq!(after, before);
        let mut changed = fixture.intent.payload.clone();
        changed.selection.execution_grant = ObjectHash::body(&"another execution grant")?;
        let changed = Signed::sign(changed, &fixture.project.client)?;
        assert!(
            fixture
                .store
                .approve_task_execution_policy(
                    &fixture.project.node,
                    &principal,
                    &changed,
                    now() + 1,
                )
                .is_err()
        );
        assert!(
            fixture
                .store
                .approve_task_execution_policy(
                    &fixture.project.node,
                    &fixture.executor.client.public_key_hex(),
                    &fixture.intent,
                    now() + 1,
                )
                .is_err()
        );
        let path = fixture.project.directory.join("foreign-approval.sqlite3");
        drop(fixture.store);
        let mut reopened = Store::open(&path)?;
        let loaded =
            carrier(|| load_task_execution_trust_in(&reopened.connection, &local, &reference))?;
        assert_eq!(loaded.approval(), trust.approval());
        assert_eq!(loaded.original_intent(), &fixture.intent);
        fixture.project.revoke_client(&mut reopened)?;
        assert!(
            reopened
                .approve_task_execution_policy(
                    &fixture.project.node,
                    &principal,
                    &fixture.intent,
                    now() + 1,
                )
                .is_err()
        );
        let approval_key = key("approval", &reference);
        let original =
            value_in(&reopened.connection, &approval_key)?.context("saved approval is missing")?;
        let mut tampered: serde_json::Value = serde_json::from_str(&original)?;
        tampered["signature"] = json!("00".repeat(64));
        reopened.connection.execute(
            "UPDATE identity_meta SET value=? WHERE key=?",
            params![tampered.to_string(), &approval_key],
        )?;
        assert!(load_task_execution_trust_in(&reopened.connection, &local, &reference).is_err());
        Ok(())
    }
}
