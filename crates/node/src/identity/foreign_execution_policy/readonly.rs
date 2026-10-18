use super::*;
use vibeface_secure::authorization::{
    Actor, DeviceId, Invocation, RequestFacts, node_execution_policy::VerifiedTaskCurrentPolicy,
    prepare::PrepareContext, remote_execution_policy::RemoteExecutionPolicySelection,
};

pub(crate) fn current_task_readonly_policy_in(
    connection: &Connection,
    primary: &super::super::journal::PolicyState,
    source: &PublicKey,
    binding: &SignedBinding,
    selection: &RemoteExecutionPolicySelection,
    purpose: RemoteExecutionPolicyPurpose,
    actual: &Invocation,
    reader: &PublicKey,
    context: &PrepareContext,
    now: u64,
) -> Result<VerifiedTaskCurrentPolicy> {
    ensure!(
        matches!(
            purpose,
            RemoteExecutionPolicyPurpose::ObservationRead | RemoteExecutionPolicyPurpose::Resolver
        ) && actual.action == Action::OperationResolve
            && matches!(actual.actor, Actor::Account(_))
            && actual.context.sdk_session.is_none()
            && actual.context == context.context
            && actual.device == DeviceId::from_key(reader)
            && actual.audience.device == DeviceId::from_key(source)
            && actual.audience.service
                == vibeface_secure::authorization::ServiceId::from_key(source)
            && actual.not_before_ms <= now
            && now < actual.expires_at_ms,
        "readonly Task policy requires this current Account request and reader"
    );
    let mut usage = ExecutionProofUsage::default();
    let association = binding_association_in(connection, primary, binding, &mut usage)?;
    let trust = load_task_execution_trust_in(connection, source, &association.approval_ref)?;
    let original = trust.original_intent();
    let current = checked_source_task_binding_in(connection, original, source, &mut usage)?
        .context("readonly Task policy has no current binding")?;
    let session = context
        .context
        .execution_session
        .as_ref()
        .context("readonly Task policy has no execution session")?;
    ensure!(
        current == *binding
            && actual.resource == original.payload.source_selection.resource
            && context.binding_generation == Some(binding.binding.generation)
            && context.context.task.as_ref().map(|task| task.as_str())
                == Some(binding.binding.task_ref.as_str())
            && context.context.conversation.as_deref()
                == Some(binding.binding.conversation_id.as_str())
            && context.context.workspace_resource.as_ref() == Some(&actual.resource.resource)
            && context.context.executor_kind.as_deref()
                == binding.binding.open_spec["executor"].as_str()
            && session.session_id == binding.binding.session_id
            && session.target == DeviceId::from_key(&selection.target_public_key)
            && binding.binding.target_node
                == crate::crypto::fingerprint(selection.target_public_key.as_str())?
            && context.execution_target_public_key.as_ref() == Some(&selection.target_public_key)
            && selection.execution_resource == original.payload.selection.execution_resource
            && selection.target_public_key == original.payload.selection.target_public_key
            && match purpose {
                RemoteExecutionPolicyPurpose::ObservationRead => context.canonical_cwd.is_none(),
                RemoteExecutionPolicyPurpose::Resolver =>
                    context.canonical_cwd.is_none()
                        || context.canonical_cwd.as_deref()
                            == binding.binding.open_spec["cwd"].as_str(),
                _ => false,
            },
        "readonly Task policy differs from its actual approved binding or context"
    );
    let task = context
        .context
        .task
        .as_ref()
        .context("readonly Task policy has no Task")?;
    let facts = RequestFacts {
        resource: actual.resource.clone(),
        action: actual.action,
        audience: actual.audience.clone(),
        operation_id: actual.operation_id.clone(),
        body_hash: actual.body_hash.clone(),
        context: actual.context.clone(),
    };
    let mut selected: Option<VerifiedTaskCurrentPolicy> = None;
    for policy in primary
        .task_execution_policy
        .current_for_task(&actual.resource, task)?
    {
        let Some(request) = policy.request().account_request() else {
            continue;
        };
        let intent = &request.intent.payload;
        if policy.request().purpose() != purpose
            || policy.request().source_node() != source
            || policy.trust().approval().hash()? != association.approval_ref
            || policy.selection() != selection
            || policy.context() != context
            || intent.expected_action != actual.action
            || intent.source_selection.resource != actual.resource
            || intent.source_selection.audience != actual.audience
            || intent.source_selection.grant_leaf != actual.grant_leaf
            || actual.actor != Actor::Account(intent.source_account.clone())
            || request.intent.issuer != *reader
            || intent.source_device_public_key != *reader
            || now < intent.issued_at_ms
            || now < request.delegation.payload.issued_at_ms
            || now < policy.snapshot().payload.issued_at_ms
            || now < policy.witness().payload.issued_at_ms
            || now >= policy.expires_at_ms()
        {
            continue;
        }
        if check_task_policy_heads_in(connection, policy).is_err() {
            continue;
        }
        provenance::check_current_anchor_in(connection, primary, source, policy, &mut usage, now)?;
        if purpose == RemoteExecutionPolicyPurpose::Resolver {
            policy.check_scope(&facts, selection, context, now)?;
        }
        if selected.as_ref().is_none_or(|prior| {
            policy.snapshot().payload.issued_at_ms > prior.snapshot().payload.issued_at_ms
        }) {
            selected = Some(policy.clone());
        }
    }
    let selected = selected.context(
        "NeedPolicySync: Task has no current dedicated readonly policy for this reader and purpose",
    )?;
    usage.merge(selected.proof_usage());
    usage.enforce_limits()?;
    Ok(selected)
}
