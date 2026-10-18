//! Purpose-bound policy reads for a separately pinned remote executor.

use super::remote_execution_history::{
    RemoteExecutionHistoryAcceptance, RemoteExecutionHistoryReference,
};
use super::{
    client_policy_import::{
        AuthorizationSelection, ClientPolicyContextReference,
        record_confirmed_client_policy_metadata, require_confirmed_client_policy,
    },
    execution_handoff::ExecutionProofUsage,
    policy_import::{BoundedPolicyImport, VerifiedPolicySnapshot},
    prepare::PrepareContext,
    *,
};
use crate::signature_verification::{ScopedSignatureVerification, SignatureVerification};
use serde_json::Value;

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RemoteExecutionPolicyPurpose {
    Execution,
    ObservationRead,
    Resolver,
    History,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RemoteExecutionPolicySelection {
    pub execution_resource: ResourceRef,
    pub target_public_key: PublicKey,
    pub execution_grant: ObjectHash,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RemoteExecutionPolicyReadIntent {
    pub version: u16,
    pub source_context: ClientPolicyContextReference,
    pub source_selection: AuthorizationSelection,
    pub source_account: AccountId,
    pub source_device_public_key: PublicKey,
    pub source_heads: Vec<PolicyStamp>,
    pub selection: RemoteExecutionPolicySelection,
    pub context: PrepareContext,
    pub expected_action: Action,
    pub purpose: RemoteExecutionPolicyPurpose,
    pub execution_pins: Vec<PolicyPin>,
    pub nonce: Nonce,
    pub issued_at_ms: u64,
    pub expires_at_ms: u64,
    pub history: Option<RemoteExecutionHistoryReference>,
}

macro_rules! policy_object {
    ($name:ident, $domain:literal) => {
        impl sealed::Sealed for $name {}
        impl SignedObject for $name {
            const DOMAIN: &'static str = $domain;
        }
    };
}
policy_object!(
    RemoteExecutionPolicyReadIntent,
    "remote-execution-policy-intent-v2"
);

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RemoteExecutionPolicyReadDelegation {
    pub version: u16,
    pub intent_hash: ObjectHash,
    pub source_node_public_key: PublicKey,
    pub source_device_public_key: PublicKey,
    pub source_project: ResourceRef,
    pub task_ref: EventRef,
    pub execution_session: SessionRef,
    pub issued_at_ms: u64,
    pub expires_at_ms: u64,
}
policy_object!(
    RemoteExecutionPolicyReadDelegation,
    "remote-execution-policy-delegation-v2"
);

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RemoteExecutionPolicyReadRequest {
    pub intent: Signed<RemoteExecutionPolicyReadIntent>,
    pub delegation: Signed<RemoteExecutionPolicyReadDelegation>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct RemoteExecutionPolicySnapshot {
    pub version: u16,
    pub request_hash: ObjectHash,
    pub snapshot_id: Nonce,
    pub saved_pins_hash: ObjectHash,
    pub target_heads: Vec<PolicyStamp>,
    pub source_node_public_key: PublicKey,
    pub issued_at_ms: u64,
    pub expires_at_ms: u64,
    pub history: Option<RemoteExecutionHistoryAcceptance>,
}
policy_object!(
    RemoteExecutionPolicySnapshot,
    "remote-execution-policy-snapshot-v2"
);

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RemoteExecutionPolicyPageRequest {
    pub snapshot_id: Nonce,
    pub request_hash: ObjectHash,
    pub cursor: Option<ObjectHash>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RemoteExecutionPolicyPage {
    pub version: u16,
    pub request_hash: ObjectHash,
    pub snapshot_id: Nonce,
    pub target_heads_hash: ObjectHash,
    pub page_index: u64,
    pub request_cursor: Option<ObjectHash>,
    pub previous_page_hash: ObjectHash,
    pub entry: Option<JournalEntry>,
    pub next_cursor: Option<ObjectHash>,
    pub complete: bool,
}
policy_object!(RemoteExecutionPolicyPage, "remote-execution-policy-page-v2");

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RemoteExecutionPolicyWitness {
    pub version: u16,
    pub request_hash: ObjectHash,
    pub snapshot_id: Nonce,
    pub target_heads: Vec<PolicyStamp>,
    pub final_page_hash: ObjectHash,
    pub page_count: u64,
    pub entry_count: u64,
    pub snapshot_hash: ObjectHash,
    pub source_node_public_key: PublicKey,
    pub issued_at_ms: u64,
    pub expires_at_ms: u64,
}
policy_object!(
    RemoteExecutionPolicyWitness,
    "remote-execution-policy-witness-v2"
);

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct IntentInput {
    selection: RemoteExecutionPolicySelection,
    context: PrepareContext,
    expected_action: Action,
    purpose: RemoteExecutionPolicyPurpose,
    execution_pins: Vec<PolicyPin>,
}

fn encode<T: Serialize>(value: &T) -> crate::Result<String> {
    serde_json::to_string(value).map_err(|_| "remote execution policy encoding failed".into())
}

fn carrier<T: Serialize>(value: &T) -> Result<()> {
    require(
        canonical(value)?.len() <= MAX_PROOF_BYTES,
        "remote execution policy carrier is too large",
    )
}

fn parse<T: for<'de> Deserialize<'de>>(json: &str) -> Result<T> {
    require(
        json.len() <= MAX_PROOF_BYTES,
        "remote execution policy carrier is too large",
    )?;
    let value = serde_json::from_str(json)
        .map_err(|_| AuthorizationError::InvalidProof("invalid remote execution policy JSON"))?;
    Ok(value)
}

fn window(issued: u64, expires: u64, now: u64) -> Result<()> {
    validate_window(issued, expires, now)?;
    require(
        expires
            .checked_sub(issued)
            .is_some_and(|age| age <= INVOCATION_MAX_AGE_MS),
        "remote execution policy read exceeds its time limit",
    )
}

fn pins(pins: &[PolicyPin]) -> Result<()> {
    require(
        !pins.is_empty()
            && pins.len() <= MAX_POLICY_DOMAINS
            && pins.windows(2).all(|pair| pair[0].domain < pair[1].domain)
            && pins.iter().all(|pin| {
                pin.minimum_head
                    .as_ref()
                    .is_none_or(|head| head.domain == pin.domain)
            }),
        "execution policy requires exact independently saved pins",
    )
}

fn purpose_action(purpose: RemoteExecutionPolicyPurpose, action: Action) -> Result<Action> {
    match purpose {
        RemoteExecutionPolicyPurpose::History => {
            require(
                matches!(
                    action,
                    Action::OperationStatusRead
                        | Action::OperationResolve
                        | Action::OperationReconcile
                ),
                "history requires current Project read permission",
            )?;
            Ok(action)
        }
        RemoteExecutionPolicyPurpose::ObservationRead | RemoteExecutionPolicyPurpose::Resolver => {
            require(
                action == Action::OperationResolve,
                "resolver policy has another action",
            )?;
            Ok(action)
        }
        RemoteExecutionPolicyPurpose::Execution => match action {
            Action::ExecutionRebind => Ok(Action::ExecutionOpen),
            Action::ExecutionOpen
            | Action::ExecutionSend
            | Action::ExecutionSteer
            | Action::ExecutionInterrupt
            | Action::ExecutionRespondApproval
            | Action::OperationStatusRead => Ok(action),
            _ => Err(AuthorizationError::InvalidProof(
                "execution policy has another purpose or action",
            )),
        },
    }
}

fn finish_usage(usage: &mut ExecutionProofUsage) -> Result<()> {
    if let Some(counts) = SignatureVerification::current_counts() {
        usage.record_actual_signature_counts(counts);
    }
    usage.enforce_limits()
}

fn source_context(source: &VerifiedClientAuthorization) -> Result<ClientPolicyContextReference> {
    let context: Value =
        serde_json::from_str(&source.client_policy_context_json().map_err(|_| {
            AuthorizationError::InvalidProof("source has no confirmed client context")
        })?)
        .map_err(|_| AuthorizationError::InvalidProof("invalid confirmed source context"))?;
    serde_json::from_value(context["client_policy_context"].clone())
        .map_err(|_| AuthorizationError::InvalidProof("invalid confirmed source reference"))
}

pub(crate) fn revalidate_source(
    source: &VerifiedClientAuthorization,
    now: u64,
) -> Result<VerifiedClientAuthorization> {
    let snapshot = require_confirmed_client_policy(source)?;
    require(
        source.resources.len() == 1 && source.resources[0].payload.kind == ResourceKind::Project,
        "remote execution requires a confirmed selected Project",
    )?;
    let resource = ResourceRef {
        space: source.resources[0].payload.space.clone(),
        resource: source.resources[0].payload.resource.clone(),
    };
    let key = snapshot
        .state()
        .find_device(&source.scope.device_id)?
        .payload
        .public_key
        .clone();
    let bundle = ClientAuthorizationBundle {
        journal: Vec::new(),
        grant_leaf: source.grant_leaf.clone(),
    };
    let mut current = verify_client_policy_state_selected(
        snapshot.state().clone(),
        &bundle,
        snapshot.pins(),
        source.scope.account_id.clone(),
        source.scope.space_id.clone(),
        key,
        Some(&resource),
        now,
    )?;
    let source_node = client_policy_import::confirmed_client_policy_source_node(source)?;
    client_policy_import::bind_client_policy_source_node(&mut current, source_node, now)?;
    require(
        current.scope == source.scope
            && current.heads == source.heads
            && current.audience == source.audience
            && current.audience_public_key == source.audience_public_key,
        "remote source requires explicit bounded synchronization",
    )?;
    current.imported_snapshot = Some(snapshot.clone());
    current.client_policy_context = source.client_policy_context.clone();
    Ok(current)
}

fn check_source_intent(
    source: &VerifiedClientAuthorization,
    intent: &Signed<RemoteExecutionPolicyReadIntent>,
    now: u64,
) -> Result<VerifiedClientAuthorization> {
    let current = revalidate_source(source, now)?;
    require(
        intent.payload.source_context == source_context(source)?
            && intent.payload.source_heads == current.heads,
        "remote policy intent changed its confirmed source context",
    )?;
    check_source_claims(&current, intent, now)?;
    Ok(current)
}

fn check_source_claims(
    current: &VerifiedClientAuthorization,
    intent: &Signed<RemoteExecutionPolicyReadIntent>,
    now: u64,
) -> Result<()> {
    carrier(intent)?;
    intent.verify_signature()?;
    let value = &intent.payload;
    window(value.issued_at_ms, value.expires_at_ms, now)?;
    pins(&value.execution_pins)?;
    purpose_action(value.purpose, value.expected_action)?;
    match (&value.history, value.purpose) {
        (Some(reference), RemoteExecutionPolicyPurpose::History) => require(
            reference.source.payload.project == value.source_selection.resource
                && reference.source.issuer == current.audience_public_key
                && reference.old_submission.payload.resource == value.selection.execution_resource
                && reference.old_submission.payload.grant_leaf == value.selection.execution_grant
                && reference.target_public_key == value.selection.target_public_key
                && reference.context()? == value.context,
            "history intent changed its original artifact selection or context",
        )?,
        (None, purpose) if purpose != RemoteExecutionPolicyPurpose::History => {}
        _ => {
            return Err(AuthorizationError::InvalidProof(
                "remote policy purpose and history differ",
            ));
        }
    }
    let project = ResourceRef {
        space: current.resources[0].payload.space.clone(),
        resource: current.resources[0].payload.resource.clone(),
    };
    require(
        value.version == AUTH_VERSION
            && value.source_selection
                == (AuthorizationSelection {
                    resource: project.clone(),
                    audience: current.audience.clone(),
                    grant_leaf: current.grant_leaf.clone(),
                })
            && value.source_account == current.scope.account_id
            && value.source_device_public_key == intent.issuer
            && DeviceId::from_key(&intent.issuer) == current.scope.device_id
            && !value.source_heads.is_empty()
            && value.source_heads.len() <= MAX_POLICY_DOMAINS
            && value
                .source_heads
                .windows(2)
                .all(|pair| pair[0].domain < pair[1].domain)
            && current
                .heads
                .iter()
                .all(|required| value.source_heads.contains(required))
            && current.actions.contains(&value.expected_action)
            && context_tighter(&current.constraints.context, &value.context.context)
            && value.context.context.workspace_resource.as_ref() == Some(&project.resource)
            && value.context.context.sdk_session.is_none()
            && value.context.context.task.is_some()
            && value.context.context.conversation.is_some()
            && value.context.context.executor_kind.is_some()
            && value
                .context
                .context
                .execution_session
                .as_ref()
                .is_some_and(|session| {
                    session.target == DeviceId::from_key(&value.selection.target_public_key)
                })
            && value.context.execution_target_public_key.as_ref()
                == Some(&value.selection.target_public_key),
        "remote policy intent changed its confirmed source or exact task",
    )?;
    if current.constraints.require_webauthn_uv {
        return Err(AuthorizationError::UserPresenceRequired);
    }
    Ok(())
}

#[cfg_attr(target_arch="wasm32",wasm_bindgen::prelude::wasm_bindgen(js_name=buildRemoteExecutionPolicyReadIntent))]
pub fn build_remote_execution_policy_read_intent(
    source: &VerifiedClientAuthorization,
    input_json: &str,
    signer: &Identity,
    now_ms: f64,
    expires_at_ms: f64,
) -> crate::Result<String> {
    let _scope = ScopedSignatureVerification::new();
    (|| -> Result<String> {
        let now = bootstrap::timestamp(now_ms)?;
        let expires = bootstrap::timestamp(expires_at_ms)?;
        window(now, expires, now)?;
        let current = revalidate_source(source, now)?;
        let input: IntentInput = parse(input_json)?;
        let intent = Signed::sign(
            RemoteExecutionPolicyReadIntent {
                version: AUTH_VERSION,
                source_context: source_context(source)?,
                source_selection: AuthorizationSelection {
                    resource: ResourceRef {
                        space: current.resources[0].payload.space.clone(),
                        resource: current.resources[0].payload.resource.clone(),
                    },
                    audience: current.audience.clone(),
                    grant_leaf: current.grant_leaf.clone(),
                },
                source_account: current.scope.account_id.clone(),
                source_device_public_key: PublicKey::of(signer),
                source_heads: current.heads.clone(),
                selection: input.selection,
                context: input.context,
                expected_action: input.expected_action,
                purpose: input.purpose,
                execution_pins: input.execution_pins,
                nonce: Nonce::random()?,
                issued_at_ms: now,
                expires_at_ms: expires,
                history: None,
            },
            signer,
        )?;
        check_source_intent(source, &intent, now)?;
        let mut usage = ExecutionProofUsage::default();
        record_confirmed_client_policy_metadata(source, &mut usage)?;
        usage.record_budget(&current.budget)?;
        usage.record_signed(&intent)?;
        finish_usage(&mut usage)?;
        encode(&intent)
            .map_err(|_| AuthorizationError::InvalidProof("remote intent encoding failed"))
    })()
    .map_err(|error| error.to_string())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct HistoryIntentInput {
    actual_json: String,
    body_json: String,
    invocation_json: String,
    binding_json: Option<String>,
}

#[cfg_attr(target_arch="wasm32",wasm_bindgen::prelude::wasm_bindgen(js_name=buildRemoteExecutionHistoryPolicyReadIntent))]
pub fn build_remote_execution_history_policy_read_intent(
    source: &VerifiedClientAuthorization,
    input_json: &str,
    independent_execution_pins_json: &str,
    signer: &Identity,
    now_ms: f64,
    expires_at_ms: f64,
) -> crate::Result<String> {
    let _scope = ScopedSignatureVerification::new();
    (|| -> Result<String> {
        require(
            input_json.len() <= execution_handoff::MAX_EXECUTION_HANDOFF_BYTES,
            "historical artifact input is too large",
        )?;
        let input: HistoryIntentInput = serde_json::from_str(input_json)
            .map_err(|_| AuthorizationError::InvalidProof("invalid historical artifact input"))?;
        let reference = super::remote_execution_history::derive_reference(
            &input.actual_json,
            &input.body_json,
            &input.invocation_json,
            input.binding_json.as_deref().unwrap_or(""),
        )?;
        let now = bootstrap::timestamp(now_ms)?;
        let expires = bootstrap::timestamp(expires_at_ms)?;
        window(now, expires, now)?;
        let current = revalidate_source(source, now)?;
        let action = [
            Action::OperationStatusRead,
            Action::OperationResolve,
            Action::OperationReconcile,
        ]
        .into_iter()
        .find(|action| current.actions.contains(action))
        .ok_or(AuthorizationError::InvalidProof(
            "history requires current Project read permission",
        ))?;
        let execution_pins: Vec<PolicyPin> = parse(independent_execution_pins_json)?;
        pins(&execution_pins)?;
        let context = reference.context()?;
        let selection = RemoteExecutionPolicySelection {
            execution_resource: reference.old_submission.payload.resource.clone(),
            target_public_key: reference.target_public_key.clone(),
            execution_grant: reference.old_submission.payload.grant_leaf.clone(),
        };
        let intent = Signed::sign(
            RemoteExecutionPolicyReadIntent {
                version: AUTH_VERSION,
                source_context: source_context(source)?,
                source_selection: AuthorizationSelection {
                    resource: reference.source.payload.project.clone(),
                    audience: current.audience.clone(),
                    grant_leaf: current.grant_leaf.clone(),
                },
                source_account: current.scope.account_id.clone(),
                source_device_public_key: PublicKey::of(signer),
                source_heads: current.heads.clone(),
                selection,
                context,
                expected_action: action,
                purpose: RemoteExecutionPolicyPurpose::History,
                execution_pins,
                nonce: Nonce::random()?,
                issued_at_ms: now,
                expires_at_ms: expires,
                history: Some(reference),
            },
            signer,
        )?;
        check_source_intent(source, &intent, now)?;
        let mut usage = ExecutionProofUsage::default();
        record_confirmed_client_policy_metadata(source, &mut usage)?;
        usage.record_budget(&current.budget)?;
        usage.record_signed(&intent)?;
        let reference = intent
            .payload
            .history
            .as_ref()
            .ok_or(AuthorizationError::PolicyConflict)?;
        super::remote_execution_history::record_history_reference_metadata(reference, &mut usage)?;
        finish_usage(&mut usage)?;
        encode(&intent)
            .map_err(|_| AuthorizationError::InvalidProof("history intent encoding failed"))
    })()
    .map_err(|error| error.to_string())
}

#[derive(Clone)]
pub struct VerifiedRemoteExecutionPolicySourceIntent {
    intent: Signed<RemoteExecutionPolicyReadIntent>,
    current: VerifiedClientAuthorization,
    source_node_public_key: PublicKey,
    source_heads: Vec<PolicyStamp>,
    verified_at_ms: u64,
    usage: ExecutionProofUsage,
}
impl VerifiedRemoteExecutionPolicySourceIntent {
    pub(super) fn authorization(&self) -> &VerifiedClientAuthorization {
        &self.current
    }
    pub fn intent(&self) -> &Signed<RemoteExecutionPolicyReadIntent> {
        &self.intent
    }
    pub fn proof_usage(&self) -> &ExecutionProofUsage {
        &self.usage
    }
    pub fn source_heads(&self) -> &[PolicyStamp] {
        &self.source_heads
    }
    pub fn source_node_public_key(&self) -> &PublicKey {
        &self.source_node_public_key
    }
    pub(super) fn verified_at_ms(&self) -> u64 {
        self.verified_at_ms
    }
}

pub fn verify_remote_execution_policy_delegation_source(
    state: &VerifiedPolicySet,
    intent: &Signed<RemoteExecutionPolicyReadIntent>,
    local_node: &PublicKey,
    noise_client: &PublicKey,
    now: u64,
) -> Result<VerifiedRemoteExecutionPolicySourceIntent> {
    let _scope = ScopedSignatureVerification::new();
    require(
        &intent.issuer == noise_client,
        "source policy reader differs from its actual Noise client",
    )?;
    let claim = &intent.payload;
    let bundle = ClientAuthorizationBundle {
        journal: Vec::new(),
        grant_leaf: claim.source_selection.grant_leaf.clone(),
    };
    let current = verify_client_policy_state_selected(
        state.clone(),
        &bundle,
        &[],
        claim.source_account.clone(),
        claim.source_selection.resource.space.clone(),
        intent.issuer.clone(),
        Some(&claim.source_selection.resource),
        now,
    )?;
    require(
        current.resources.len() == 1
            && current.resources[0].payload.kind == ResourceKind::Project
            && &current.audience_public_key == local_node,
        "policy delegation must reach the real current Project service",
    )?;
    let client = state.find_device(&current.scope.device_id)?;
    require(
        client.payload.owner == PolicyDomain::Account(current.scope.account_id.clone())
            && client.payload.subject == Actor::Account(current.scope.account_id.clone())
            && client.payload.public_key == *noise_client
            && client.payload.uses.contains(&DeviceUse::Noise)
            && client.payload.uses.contains(&DeviceUse::AccountInvocation),
        "policy delegation requires this account's current Noise client",
    )?;
    check_source_claims(&current, intent, now)?;
    let mut usage = ExecutionProofUsage::default();
    usage.record_signed(intent)?;
    usage.record_budget(&current.budget)?;
    if let Some(reference) = &intent.payload.history {
        super::remote_execution_history::validate_remote_execution_history_reference(reference)?;
        super::remote_execution_history::record_history_reference_metadata(reference, &mut usage)?;
    }
    finish_usage(&mut usage)?;
    Ok(VerifiedRemoteExecutionPolicySourceIntent {
        intent: intent.clone(),
        source_node_public_key: current.audience_public_key.clone(),
        source_heads: current.heads.clone(),
        current,
        verified_at_ms: now,
        usage,
    })
}

/// The Node must check its current Task/binding in the same Store boundary before signing.
pub fn sign_remote_execution_policy_read_delegation(
    verified: &VerifiedRemoteExecutionPolicySourceIntent,
    identity: &Identity,
    now: u64,
) -> Result<Signed<RemoteExecutionPolicyReadDelegation>> {
    let intent = &verified.intent.payload;
    window(intent.issued_at_ms, intent.expires_at_ms, now)?;
    require(
        now >= verified.verified_at_ms
            && PublicKey::of(identity) == verified.source_node_public_key,
        "only the current source Project service can delegate this policy read",
    )?;
    let delegation = Signed::sign(
        RemoteExecutionPolicyReadDelegation {
            version: AUTH_VERSION,
            intent_hash: verified.intent.hash()?,
            source_node_public_key: verified.source_node_public_key.clone(),
            source_device_public_key: verified.intent.issuer.clone(),
            source_project: intent.source_selection.resource.clone(),
            task_ref: intent
                .context
                .context
                .task
                .clone()
                .ok_or(AuthorizationError::InvalidProof("policy read has no Task"))?,
            execution_session: intent.context.context.execution_session.clone().ok_or(
                AuthorizationError::InvalidProof("policy read has no session"),
            )?,
            issued_at_ms: now,
            expires_at_ms: intent.expires_at_ms,
        },
        identity,
    )?;
    carrier(&delegation)?;
    Ok(delegation)
}

fn check_request(request: &RemoteExecutionPolicyReadRequest, now: u64) -> Result<()> {
    carrier(request)?;
    request.intent.verify_signature()?;
    request.delegation.verify_signature()?;
    let intent = &request.intent.payload;
    let delegation = &request.delegation.payload;
    window(intent.issued_at_ms, intent.expires_at_ms, now)?;
    window(delegation.issued_at_ms, delegation.expires_at_ms, now)?;
    pins(&intent.execution_pins)?;
    purpose_action(intent.purpose, intent.expected_action)?;
    require(
        (intent.purpose == RemoteExecutionPolicyPurpose::History) == intent.history.is_some(),
        "remote policy purpose changed its historical artifact",
    )?;
    require(
        intent.version == AUTH_VERSION
            && delegation.version == AUTH_VERSION
            && intent.source_device_public_key == request.intent.issuer
            && delegation.intent_hash == request.intent.hash()?
            && delegation.source_node_public_key == request.delegation.issuer
            && delegation.source_device_public_key == request.intent.issuer
            && delegation.source_project == intent.source_selection.resource
            && intent.source_selection.audience.service
                == ServiceId::from_key(&request.delegation.issuer)
            && intent.source_selection.audience.device
                == DeviceId::from_key(&request.delegation.issuer)
            && intent.context.context.task.as_ref() == Some(&delegation.task_ref)
            && intent.context.context.execution_session.as_ref()
                == Some(&delegation.execution_session)
            && intent.context.context.workspace_resource.as_ref()
                == Some(&delegation.source_project.resource)
            && intent.context.context.sdk_session.is_none()
            && intent.context.execution_target_public_key.as_ref()
                == Some(&intent.selection.target_public_key)
            && delegation.execution_session.target
                == DeviceId::from_key(&intent.selection.target_public_key)
            && delegation.issued_at_ms >= intent.issued_at_ms
            && delegation.expires_at_ms <= intent.expires_at_ms,
        "execution policy delegation changed its original client, source, Task or selection",
    )
}

#[derive(Clone)]
pub struct VerifiedRemoteExecutionPolicyRead {
    request: RemoteExecutionPolicyReadRequest,
    heads: Vec<PolicyStamp>,
    usage: ExecutionProofUsage,
    allowed_hashes: BTreeSet<ObjectHash>,
}
impl VerifiedRemoteExecutionPolicyRead {
    pub fn request(&self) -> &RemoteExecutionPolicyReadRequest {
        &self.request
    }
    pub fn target_heads(&self) -> &[PolicyStamp] {
        &self.heads
    }
    pub fn proof_usage(&self) -> &ExecutionProofUsage {
        &self.usage
    }
    pub fn domains(&self) -> Vec<PolicyDomain> {
        self.heads.iter().map(|head| head.domain.clone()).collect()
    }
    pub fn allows_hash(&self, hash: &ObjectHash) -> bool {
        self.allowed_hashes.contains(hash)
    }
    pub fn allows_entry(&self, entry: &JournalEntry) -> Result<bool> {
        Ok(self.allows_hash(&policy_import::entry_hash(entry)?))
    }
}

pub(super) fn reference_closure(
    state: &VerifiedPolicySet,
    initial: &[PolicyStamp],
    independent: &[PolicyPin],
) -> Result<(Vec<PolicyStamp>, BTreeSet<ObjectHash>)> {
    let mut domains: BTreeSet<_> = initial.iter().map(|head| head.domain.clone()).collect();
    let mut visited = BTreeSet::new();
    let mut hashes = BTreeSet::new();
    let mut pending = BTreeSet::new();
    loop {
        let next = domains
            .iter()
            .find(|domain| !visited.contains(*domain))
            .cloned();
        if let Some(domain) = next {
            let head = state
                .head(&domain)
                .ok_or(AuthorizationError::PolicyConflict)?;
            let pin = independent
                .iter()
                .find(|pin| pin.domain == domain)
                .ok_or_else(|| AuthorizationError::NeedPolicySync(vec![head.clone()]))?;
            let genesis = match &domain {
                PolicyDomain::Account(account) => state
                    .account(account)
                    .map(|value| value.genesis_root.clone()),
                PolicyDomain::Space(space) => state.space(space).map(|value| match &value.root {
                    SpaceRoot::Personal { genesis_root, .. } | SpaceRoot::Team { genesis_root } => {
                        genesis_root.clone()
                    }
                }),
            }
            .ok_or(AuthorizationError::PolicyConflict)?;
            require(
                genesis == pin.genesis_root,
                "execution policy dependency has another independent genesis",
            )?;
            if let Some(minimum) = &pin.minimum_head {
                if !state.roots.contains_key(minimum) {
                    return Err(AuthorizationError::NeedPolicySync(vec![minimum.clone()]));
                }
            }
            require(
                state.roots.contains_key(head),
                "execution policy dependency has no accepted head",
            )?;
            let mut documents = Vec::new();
            match &domain {
                PolicyDomain::Account(account) => {
                    for document in state
                        .account_history(account)
                        .ok_or(AuthorizationError::PolicyConflict)?
                    {
                        let hash = document.hash()?;
                        hashes.insert(hash.clone());
                        documents.push(hash);
                        pending.extend(document.payload.devices.iter().cloned());
                        pending.extend(document.payload.authenticators.iter().cloned());
                        if let Some(proof) = &document.payload.device_enrollment {
                            domains.extend(
                                proof
                                    .intent
                                    .payload
                                    .expected_heads
                                    .iter()
                                    .map(|head| head.domain.clone()),
                            );
                            domains.extend(
                                proof
                                    .assertion
                                    .pending
                                    .envelope()
                                    .expected_heads
                                    .iter()
                                    .map(|head| head.domain.clone()),
                            );
                            pending.insert(proof.assertion.record.hash()?);
                        }
                        if let Some(approval) = &document.payload.authenticator_approval {
                            domains.extend(
                                approval
                                    .intent
                                    .payload
                                    .expected_heads
                                    .iter()
                                    .map(|head| head.domain.clone()),
                            );
                            domains.extend(
                                approval
                                    .assertion
                                    .pending
                                    .envelope()
                                    .expected_heads
                                    .iter()
                                    .map(|head| head.domain.clone()),
                            );
                            pending.insert(approval.assertion.record.hash()?);
                        }
                    }
                }
                PolicyDomain::Space(space) => {
                    for document in state
                        .space_history(space)
                        .ok_or(AuthorizationError::PolicyConflict)?
                    {
                        let hash = document.hash()?;
                        hashes.insert(hash.clone());
                        documents.push(hash);
                        pending.extend(document.payload.devices.iter().cloned());
                        pending.extend(document.payload.resources.iter().cloned());
                        pending.extend(document.payload.memberships.iter().cloned());
                        pending.extend(document.payload.grants.iter().cloned());
                        pending.extend(document.payload.governance_grant.iter().cloned());
                        if let SpaceRoot::Personal { owner, .. } = &document.payload.root {
                            domains.insert(PolicyDomain::Account(owner.clone()));
                        }
                    }
                }
            }
            for hash in documents {
                if let Some(commit) = state.commits.get(&hash) {
                    domains.extend(
                        commit
                            .payload
                            .dependencies
                            .iter()
                            .map(|head| head.domain.clone()),
                    );
                }
            }
            visited.insert(domain);
        } else if let Some(hash) = pending.pop_first() {
            if !hashes.insert(hash.clone()) {
                continue;
            }
            if let Some(value) = state.devices.get(&hash) {
                domains.insert(value.payload.owner.clone());
                domains.insert(value.payload.issued_under.domain.clone());
                pending.extend(value.payload.enrollment_grant.iter().cloned());
            } else if let Some(value) = state.resources.get(&hash) {
                domains.insert(value.payload.issued_under.domain.clone());
            } else if let Some(value) = state.authenticators.get(&hash) {
                domains.insert(value.payload.issued_under.domain.clone());
            } else if let Some(value) = state.memberships.get(&hash) {
                domains.insert(value.payload.issued_under.domain.clone());
            } else if let Some(value) = state.grants.get(&hash) {
                domains.insert(value.payload.issued_under.domain.clone());
                pending.extend(value.payload.parent.iter().cloned());
                domains.insert(match &value.payload.scope {
                    ResourceScope::WholeSpace(space) => PolicyDomain::Space(space.clone()),
                    ResourceScope::Exact(resource) => PolicyDomain::Space(resource.space.clone()),
                });
                if let Grantee::MembersOf(selector) = &value.payload.grantee {
                    domains.insert(PolicyDomain::Space(selector.space.clone()));
                }
            } else {
                return Err(AuthorizationError::InvalidProof(
                    "execution policy is missing a referenced accepted object",
                ));
            }
        } else {
            break;
        }
        require(
            domains.len() <= MAX_POLICY_DOMAINS,
            "execution policy reference closure has too many domains",
        )?;
    }
    let heads = domains
        .iter()
        .map(|domain| {
            state
                .head(domain)
                .cloned()
                .ok_or(AuthorizationError::PolicyConflict)
        })
        .collect::<Result<Vec<_>>>()?;
    Ok((heads, hashes))
}

pub fn verify_remote_execution_policy_read(
    state: &VerifiedPolicySet,
    request: &RemoteExecutionPolicyReadRequest,
    local_node: &PublicKey,
    noise_client: &PublicKey,
    now: u64,
) -> Result<VerifiedRemoteExecutionPolicyRead> {
    let _scope = ScopedSignatureVerification::new();
    check_request(request, now)?;
    let intent = &request.intent.payload;
    require(
        intent.purpose != RemoteExecutionPolicyPurpose::History && intent.history.is_none(),
        "history requires its independent read verifier",
    )?;
    let selection = &intent.selection;
    require(
        &request.intent.issuer == noise_client && &selection.target_public_key == local_node,
        "execution policy request reached another Noise client or target",
    )?;
    let record = state
        .resource(
            &selection.execution_resource.space,
            &selection.execution_resource.resource,
        )
        .ok_or(AuthorizationError::InvalidProof(
            "selected ExecutionService is missing",
        ))?;
    let target = state.verify_service_device(
        &selection.execution_resource.space,
        &DeviceId::from_key(local_node),
        now,
    )?;
    let leaf = state
        .grant(&selection.execution_grant)
        .ok_or(AuthorizationError::InvalidProof(
            "selected execution grant is missing",
        ))?;
    let expected_action = purpose_action(intent.purpose, intent.expected_action)?;
    let mut context = intent.context.context.clone();
    context.workspace_resource = Some(selection.execution_resource.resource.clone());
    require(
        record.payload.kind == ResourceKind::ExecutionService
            && record.payload.host == ServiceId::from_key(local_node)
            && target.payload.public_key == *local_node
            && target.payload.subject == Actor::Service(record.payload.host.clone())
            && leaf.payload.scope == ResourceScope::Exact(selection.execution_resource.clone())
            && leaf.payload.grantee
                == Grantee::Actor(Actor::Service(ServiceId::from_key(
                    &request.delegation.issuer,
                )))
            && leaf.payload.audience
                == (Audience {
                    service: record.payload.host.clone(),
                    device: DeviceId::from_key(local_node),
                })
            && leaf.payload.remaining_delegations == 0
            && leaf.payload.expires_at_ms < u64::MAX
            && !leaf.payload.constraints.require_account_actor
            && !leaf.payload.constraints.require_webauthn_uv
            && leaf.payload.constraints.context == context
            && context.task.is_some()
            && context.conversation.is_some()
            && context.executor_kind.is_some()
            && leaf.payload.actions.contains(&expected_action)
            && prepare::remote_actions(
                &leaf.payload.actions,
                intent.purpose != RemoteExecutionPolicyPurpose::Execution,
            ),
        "execution policy read exceeds the current exact Service grant or purpose",
    )?;
    let mut budget = Budget::default();
    let mut dependencies = BTreeMap::new();
    budget.verify(record)?;
    state.validate_certificate(
        target,
        now,
        &mut budget,
        &mut dependencies,
        &mut BTreeSet::new(),
    )?;
    let chain = state.grant_chain(Some(&selection.execution_grant))?;
    state.verify_grant_path(&chain, now, &mut budget, &mut dependencies, true)?;
    state.expand_authorities(now, &mut budget, &mut dependencies)?;
    let dependencies: Vec<_> = dependencies.into_values().collect();
    let (heads, allowed_hashes) = reference_closure(state, &dependencies, &intent.execution_pins)?;
    require(
        !heads.is_empty()
            && heads.iter().all(|head| {
                intent
                    .execution_pins
                    .iter()
                    .any(|pin| pin.domain == head.domain)
            }),
        "execution policy read needs independently pinned dependency domains",
    )?;
    let mut usage = ExecutionProofUsage::default();
    usage.record_signed(&request.intent)?;
    usage.record_signed(&request.delegation)?;
    usage.record_budget(&budget)?;
    usage.record_chain_depth(chain.len());
    finish_usage(&mut usage)?;
    Ok(VerifiedRemoteExecutionPolicyRead {
        request: request.clone(),
        heads,
        usage,
        allowed_hashes,
    })
}

pub fn verify_remote_execution_history_policy_read(
    state: &VerifiedPolicySet,
    request: &RemoteExecutionPolicyReadRequest,
    acceptance: &RemoteExecutionHistoryAcceptance,
    local_node: &PublicKey,
    noise_client: &PublicKey,
    now: u64,
) -> Result<VerifiedRemoteExecutionPolicyRead> {
    let _scope = ScopedSignatureVerification::new();
    check_request(request, now)?;
    let intent = &request.intent.payload;
    let reference = intent
        .history
        .as_ref()
        .ok_or(AuthorizationError::InvalidProof(
            "history policy requires its original artifact reference",
        ))?;
    require(
        intent.purpose == RemoteExecutionPolicyPurpose::History
            && &request.intent.issuer == noise_client
            && &intent.selection.target_public_key == local_node
            && reference.source.issuer == request.delegation.issuer
            && reference.source.payload.project == intent.source_selection.resource
            && reference.old_submission.issuer == request.delegation.issuer
            && reference.old_submission.payload.resource == intent.selection.execution_resource
            && reference.old_submission.payload.grant_leaf == intent.selection.execution_grant
            && reference.context()? == intent.context,
        "history policy changed its original artifact, reader or source service",
    )?;
    let verified = super::remote_execution_history::verify_remote_execution_history_policy(
        state,
        reference,
        acceptance,
        &intent.execution_pins,
        local_node,
        now,
    )?;
    let mut usage = verified.proof_usage().clone();
    usage.record_signed(&request.intent)?;
    usage.record_signed(&request.delegation)?;
    finish_usage(&mut usage)?;
    Ok(VerifiedRemoteExecutionPolicyRead {
        request: request.clone(),
        heads: verified.target_heads().to_vec(),
        usage,
        allowed_hashes: verified.allowed_hashes().clone(),
    })
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen)]
pub struct RemoteExecutionPolicyImport {
    request: RemoteExecutionPolicyReadRequest,
    header: Signed<RemoteExecutionPolicySnapshot>,
    import: BoundedPolicyImport,
    previous_page_hash: ObjectHash,
    next_cursor: Option<ObjectHash>,
    last_page: Option<Signed<RemoteExecutionPolicyPage>>,
    page_count: u64,
    entry_count: u64,
    complete: bool,
    usage: ExecutionProofUsage,
}

#[derive(Clone)]
#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen)]
pub struct VerifiedRemoteExecutionPolicy {
    request: RemoteExecutionPolicyReadRequest,
    header: Signed<RemoteExecutionPolicySnapshot>,
    witness: Signed<RemoteExecutionPolicyWitness>,
    snapshot: VerifiedPolicySnapshot,
    usage: ExecutionProofUsage,
}

impl VerifiedRemoteExecutionPolicy {
    pub(super) fn native_request(&self) -> &RemoteExecutionPolicyReadRequest {
        &self.request
    }
    pub(super) fn native_header(&self) -> &Signed<RemoteExecutionPolicySnapshot> {
        &self.header
    }
    pub(super) fn native_witness(&self) -> &Signed<RemoteExecutionPolicyWitness> {
        &self.witness
    }
    pub(crate) fn snapshot(&self) -> &VerifiedPolicySnapshot {
        &self.snapshot
    }
    pub(crate) fn intent(&self) -> &RemoteExecutionPolicyReadIntent {
        &self.request.intent.payload
    }
    pub(crate) fn expires_at_ms(&self) -> u64 {
        self.request
            .intent
            .payload
            .expires_at_ms
            .min(self.request.delegation.payload.expires_at_ms)
            .min(self.header.payload.expires_at_ms)
            .min(self.witness.payload.expires_at_ms)
    }
    pub(crate) fn revalidate(
        &self,
        source: &VerifiedClientAuthorization,
        now: u64,
    ) -> Result<VerifiedClientAuthorization> {
        let current = check_source_intent(source, &self.request.intent, now)?;
        require(
            self.request.delegation.issuer == source.audience_public_key,
            "execution policy delegation belongs to another confirmed source",
        )?;
        window(
            self.header.payload.issued_at_ms,
            self.header.payload.expires_at_ms,
            now,
        )?;
        window(
            self.witness.payload.issued_at_ms,
            self.witness.payload.expires_at_ms,
            now,
        )?;
        window(
            self.request.delegation.payload.issued_at_ms,
            self.request.delegation.payload.expires_at_ms,
            now,
        )?;
        Ok(current)
    }
    pub(crate) fn record_metadata(&self, usage: &mut ExecutionProofUsage) -> Result<()> {
        usage.record_signed(&self.request.intent)?;
        usage.record_verified_object(&self.request.delegation)?;
        usage.record_verified_object(&self.header)?;
        usage.record_verified_object(&self.witness)?;
        Ok(())
    }
}

#[cfg_attr(target_arch="wasm32",wasm_bindgen::prelude::wasm_bindgen(js_name=beginRemoteExecutionPolicyImport))]
pub fn begin_remote_execution_policy_import(
    source: &VerifiedClientAuthorization,
    request_json: &str,
    snapshot_json: &str,
    independent_execution_pins_json: &str,
    expected_c_noise_key: &str,
    now_ms: f64,
) -> crate::Result<RemoteExecutionPolicyImport> {
    begin_import_inner(
        source,
        request_json,
        snapshot_json,
        independent_execution_pins_json,
        expected_c_noise_key,
        now_ms,
        false,
    )
}

fn begin_import_inner(
    source: &VerifiedClientAuthorization,
    request_json: &str,
    snapshot_json: &str,
    independent_execution_pins_json: &str,
    expected_c_noise_key: &str,
    now_ms: f64,
    history: bool,
) -> crate::Result<RemoteExecutionPolicyImport> {
    let _scope = ScopedSignatureVerification::new();
    (|| -> Result<RemoteExecutionPolicyImport> {
        let now = bootstrap::timestamp(now_ms)?;
        let request: RemoteExecutionPolicyReadRequest = parse(request_json)?;
        let header: Signed<RemoteExecutionPolicySnapshot> = parse(snapshot_json)?;
        let independent: Vec<PolicyPin> = parse(independent_execution_pins_json)?;
        let current = check_source_intent(source, &request.intent, now)?;
        let expected = PublicKey::parse(expected_c_noise_key)?;
        require(
            independent == request.intent.payload.execution_pins
                && request.delegation.issuer == current.audience_public_key,
            "execution policy snapshot changed its source, independent pins or exact request",
        )?;
        let mut usage = ExecutionProofUsage::default();
        record_confirmed_client_policy_metadata(source, &mut usage)?;
        usage.record_budget(&current.budget)?;
        begin_native_execution_policy_import(
            &request,
            &header,
            &independent,
            &expected,
            &usage,
            now,
            history,
        )
    })()
    .map_err(|error| error.to_string())
}

/// The caller must independently verify the current local source and its saved trust.
pub(super) fn begin_native_execution_policy_import(
    request: &RemoteExecutionPolicyReadRequest,
    header: &Signed<RemoteExecutionPolicySnapshot>,
    effective_independent_pins: &[PolicyPin],
    expected_c_key: &PublicKey,
    source_usage: &ExecutionProofUsage,
    now: u64,
    history: bool,
) -> Result<RemoteExecutionPolicyImport> {
    let _scope = ScopedSignatureVerification::new();
    require(
        history == (request.intent.payload.purpose == RemoteExecutionPolicyPurpose::History)
            && history == header.payload.history.is_some(),
        "policy import changed its independent historical purpose",
    )?;
    check_request(request, now)?;
    pins(effective_independent_pins)?;
    carrier(header)?;
    header.verify_signature()?;
    window(header.payload.issued_at_ms, header.payload.expires_at_ms, now)?;
    let original_pins = &request.intent.payload.execution_pins;
    require(
        effective_independent_pins.len() == original_pins.len()
            && effective_independent_pins
                .iter()
                .zip(original_pins)
                .all(|(effective, original)| {
                    effective.domain == original.domain
                        && effective.genesis_root == original.genesis_root
                        && original.minimum_head.as_ref().is_none_or(|minimum| {
                            effective.minimum_head.as_ref().is_some_and(|current| {
                                (current.authority_epoch, current.sequence)
                                    > (minimum.authority_epoch, minimum.sequence)
                                    || current == minimum
                            })
                        })
                })
            && &header.issuer == expected_c_key
            && &header.payload.source_node_public_key == expected_c_key
            && expected_c_key == &request.intent.payload.selection.target_public_key
            && header.payload.version == AUTH_VERSION
            && header.payload.request_hash == ObjectHash::body(request)?
            && header.payload.saved_pins_hash == ObjectHash::body(original_pins)?
            && header.payload.issued_at_ms >= request.delegation.payload.issued_at_ms
            && header.payload.expires_at_ms <= request.delegation.payload.expires_at_ms
            && !header.payload.target_heads.is_empty()
            && header.payload.target_heads.len() <= MAX_POLICY_DOMAINS
            && header
                .payload
                .target_heads
                .windows(2)
                .all(|pair| pair[0].domain < pair[1].domain),
        "execution policy snapshot changed its source, independent pins or exact request",
    )?;
    let selected: Vec<_> = effective_independent_pins
        .iter()
        .filter(|pin| {
            header
                .payload
                .target_heads
                .iter()
                .any(|head| head.domain == pin.domain)
        })
        .cloned()
        .collect();
    require(
        selected.len() == header.payload.target_heads.len(),
        "execution policy snapshot introduced an independently untrusted domain",
    )?;
    let required = original_pins
        .iter()
        .filter(|pin| selected.iter().any(|saved| saved.domain == pin.domain))
        .filter_map(|pin| pin.minimum_head.clone())
        .collect();
    let import = BoundedPolicyImport::new(selected, required)?;
    let mut usage = source_usage.clone();
    usage.record_signed(&request.intent)?;
    usage.record_signed(&request.delegation)?;
    usage.record_signed(header)?;
    finish_usage(&mut usage)?;
    Ok(RemoteExecutionPolicyImport {
        request: request.clone(),
        previous_page_hash: header.hash()?,
        header: header.clone(),
        import,
        next_cursor: None,
        last_page: None,
        page_count: 0,
        entry_count: 0,
        complete: false,
        usage,
    })
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen)]
impl RemoteExecutionPolicyImport {
    #[cfg_attr(target_arch="wasm32",wasm_bindgen::prelude::wasm_bindgen(js_name=pageRequestJson))]
    pub fn page_request_json(&self) -> crate::Result<String> {
        encode(&RemoteExecutionPolicyPageRequest {
            snapshot_id: self.header.payload.snapshot_id.clone(),
            request_hash: self.header.payload.request_hash.clone(),
            cursor: self.next_cursor.clone(),
        })
    }
    #[cfg_attr(target_arch="wasm32",wasm_bindgen::prelude::wasm_bindgen(js_name=proofUsageJson))]
    pub fn proof_usage_json(&self) -> crate::Result<String> {
        encode(
            &serde_json::json!({"last_carrier":self.usage.counts(),"page_count":self.page_count,"entry_count":self.entry_count}),
        )
    }
    #[cfg_attr(target_arch="wasm32",wasm_bindgen::prelude::wasm_bindgen(js_name=appendPage))]
    pub fn append_page(&mut self, page_json: &str, now_ms: f64) -> crate::Result<()> {
        let _scope = ScopedSignatureVerification::new();
        (|| -> Result<()> {
            let now = bootstrap::timestamp(now_ms)?;
            let page: Signed<RemoteExecutionPolicyPage> = parse(page_json)?;
            self.append_native_page(&page, now)
        })()
        .map_err(|error| error.to_string())
    }
    pub fn finish(
        self,
        witness_json: &str,
        now_ms: f64,
    ) -> crate::Result<VerifiedRemoteExecutionPolicy> {
        self.finish_inner(witness_json, now_ms, false)
    }
}

impl RemoteExecutionPolicyImport {
    pub(super) fn append_native_page(
        &mut self,
        page: &Signed<RemoteExecutionPolicyPage>,
        now: u64,
    ) -> Result<()> {
        let _scope = ScopedSignatureVerification::new();
        carrier(page)?;
        window(
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
            "execution policy page changed its source or snapshot",
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
                && page.payload.complete == page.payload.next_cursor.is_none(),
            "execution policy page is missing, duplicated or out of order",
        )?;
        let mut staged = self.import.clone();
        if let Some(entry) = &page.payload.entry {
            usage.merge(&policy_proof::policy_entry_usage(staged.state(), entry)?);
            usage.enforce_limits()?;
            staged.apply_entry(entry.clone())?;
        }
        finish_usage(&mut usage)?;
        self.import = staged;
        self.previous_page_hash = page.hash()?;
        self.next_cursor = page.payload.next_cursor.clone();
        self.page_count += 1;
        self.entry_count += u64::from(page.payload.entry.is_some());
        self.complete = page.payload.complete;
        self.last_page = Some(page.clone());
        self.usage = usage;
        Ok(())
    }

    fn finish_inner(
        self,
        witness_json: &str,
        now_ms: f64,
        history: bool,
    ) -> crate::Result<VerifiedRemoteExecutionPolicy> {
        let _scope = ScopedSignatureVerification::new();
        (|| -> Result<VerifiedRemoteExecutionPolicy> {
            let now = bootstrap::timestamp(now_ms)?;
            let witness: Signed<RemoteExecutionPolicyWitness> = parse(witness_json)?;
            self.finish_native(&witness, now, history)
        })()
        .map_err(|error| error.to_string())
    }

    pub(super) fn finish_native(
        self,
        witness: &Signed<RemoteExecutionPolicyWitness>,
        now: u64,
        history: bool,
    ) -> Result<VerifiedRemoteExecutionPolicy> {
        let _scope = ScopedSignatureVerification::new();
        carrier(witness)?;
        require(
            history
                == (self.request.intent.payload.purpose
                    == RemoteExecutionPolicyPurpose::History)
                && history == self.header.payload.history.is_some(),
            "policy completion changed its independent historical purpose",
        )?;
        witness.verify_signature()?;
        window(
            witness.payload.issued_at_ms,
            witness.payload.expires_at_ms,
            now,
        )?;
        window(
            self.header.payload.issued_at_ms,
            self.header.payload.expires_at_ms,
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
                && witness.payload.expires_at_ms <= self.header.payload.expires_at_ms,
            "execution policy witness changed its completed stream",
        )?;
        let snapshot = self.import.finish(&self.header.payload.target_heads)?;
        let verified = if let Some(acceptance) = &self.header.payload.history {
            verify_remote_execution_history_policy_read(
                snapshot.state(),
                &self.request,
                acceptance,
                &self.header.issuer,
                &self.request.intent.issuer,
                now,
            )?
        } else {
            verify_remote_execution_policy_read(
                snapshot.state(),
                &self.request,
                &self.header.issuer,
                &self.request.intent.issuer,
                now,
            )?
        };
        require(
            verified.target_heads() == self.header.payload.target_heads
                && snapshot.journal().iter().all(|entry| {
                    policy_import::entry_hash(entry)
                        .is_ok_and(|hash| verified.allows_hash(&hash))
                }),
            "execution policy stream contains objects outside its necessary reference closure",
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
        let snapshot = snapshot.with_independent_pins(&advanced)?;
        let mut usage = ExecutionProofUsage::default();
        usage.record_signed(witness)?;
        usage.merge(verified.proof_usage());
        finish_usage(&mut usage)?;
        Ok(VerifiedRemoteExecutionPolicy {
            request: self.request,
            header: self.header,
            witness: witness.clone(),
            snapshot,
            usage,
        })
    }
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen)]
impl VerifiedRemoteExecutionPolicy {
    #[cfg_attr(target_arch="wasm32",wasm_bindgen::prelude::wasm_bindgen(js_name=revalidatePins))]
    pub fn revalidate_pins(
        &self,
        latest_independent_pins_json: &str,
        now_ms: f64,
    ) -> crate::Result<VerifiedRemoteExecutionPolicy> {
        let _scope = ScopedSignatureVerification::new();
        (|| -> Result<VerifiedRemoteExecutionPolicy> {
            let now = bootstrap::timestamp(now_ms)?;
            window(self.header.payload.issued_at_ms, self.expires_at_ms(), now)?;
            let incoming: Vec<PolicyPin> = parse(latest_independent_pins_json)?;
            pins(&incoming)?;
            let selected: Vec<_> = incoming
                .into_iter()
                .filter(|pin| {
                    self.snapshot
                        .pins()
                        .iter()
                        .any(|saved| saved.domain == pin.domain)
                })
                .collect();
            let mut result = self.clone();
            result.snapshot = self.snapshot.with_independent_pins(&selected)?;
            Ok(result)
        })()
        .map_err(|error| error.to_string())
    }
    #[cfg_attr(target_arch="wasm32",wasm_bindgen::prelude::wasm_bindgen(js_name=metadataJson))]
    pub fn metadata_json(&self) -> crate::Result<String> {
        let intent = self.intent();
        encode(
            &serde_json::json!({"selection":intent.selection,"context":intent.context,
            "expected_action":intent.expected_action,"purpose":intent.purpose,"pins":self.snapshot.pins(),
            "heads":self.snapshot.heads(),"expires_at_ms":self.expires_at_ms()}),
        )
    }
    #[cfg_attr(target_arch="wasm32",wasm_bindgen::prelude::wasm_bindgen(js_name=proofUsageJson))]
    pub fn proof_usage_json(&self) -> crate::Result<String> {
        encode(&self.usage.counts())
    }
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen)]
pub struct RemoteExecutionHistoryPolicyImport {
    inner: RemoteExecutionPolicyImport,
}

#[derive(Clone)]
#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen)]
pub struct VerifiedRemoteExecutionHistoryPolicy {
    inner: VerifiedRemoteExecutionPolicy,
}

#[cfg_attr(target_arch="wasm32",wasm_bindgen::prelude::wasm_bindgen(js_name=beginRemoteExecutionHistoryPolicyImport))]
pub fn begin_remote_execution_history_policy_import(
    source: &VerifiedClientAuthorization,
    request_json: &str,
    snapshot_json: &str,
    independent_execution_pins_json: &str,
    expected_c_noise_key: &str,
    now_ms: f64,
) -> crate::Result<RemoteExecutionHistoryPolicyImport> {
    Ok(RemoteExecutionHistoryPolicyImport {
        inner: begin_import_inner(
            source,
            request_json,
            snapshot_json,
            independent_execution_pins_json,
            expected_c_noise_key,
            now_ms,
            true,
        )?,
    })
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen)]
impl RemoteExecutionHistoryPolicyImport {
    #[cfg_attr(target_arch="wasm32",wasm_bindgen::prelude::wasm_bindgen(js_name=pageRequestJson))]
    pub fn page_request_json(&self) -> crate::Result<String> {
        self.inner.page_request_json()
    }
    #[cfg_attr(target_arch="wasm32",wasm_bindgen::prelude::wasm_bindgen(js_name=appendPage))]
    pub fn append_page(&mut self, page_json: &str, now_ms: f64) -> crate::Result<()> {
        self.inner.append_page(page_json, now_ms)
    }
    #[cfg_attr(target_arch="wasm32",wasm_bindgen::prelude::wasm_bindgen(js_name=proofUsageJson))]
    pub fn proof_usage_json(&self) -> crate::Result<String> {
        self.inner.proof_usage_json()
    }
    pub fn finish(
        self,
        witness_json: &str,
        now_ms: f64,
    ) -> crate::Result<VerifiedRemoteExecutionHistoryPolicy> {
        Ok(VerifiedRemoteExecutionHistoryPolicy {
            inner: self.inner.finish_inner(witness_json, now_ms, true)?,
        })
    }
}

impl VerifiedRemoteExecutionHistoryPolicy {
    pub(crate) fn snapshot(&self) -> &VerifiedPolicySnapshot {
        self.inner.snapshot()
    }
    pub(crate) fn reference(&self) -> Result<&RemoteExecutionHistoryReference> {
        self.inner
            .intent()
            .history
            .as_ref()
            .ok_or(AuthorizationError::InvalidProof(
                "historical policy has no original artifact reference",
            ))
    }
    pub(crate) fn acceptance(&self) -> Result<&RemoteExecutionHistoryAcceptance> {
        self.inner
            .header
            .payload
            .history
            .as_ref()
            .ok_or(AuthorizationError::InvalidProof(
                "historical policy has no original signature proof",
            ))
    }
    pub(crate) fn revalidate(
        &self,
        source: &VerifiedClientAuthorization,
        now: u64,
    ) -> Result<VerifiedClientAuthorization> {
        require(
            self.inner.intent().purpose == RemoteExecutionPolicyPurpose::History,
            "history requires its independent read purpose",
        )?;
        self.inner.revalidate(source, now)
    }
    pub(crate) fn record_metadata(&self, usage: &mut ExecutionProofUsage) -> Result<()> {
        self.inner.record_metadata(usage)
    }
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen)]
impl VerifiedRemoteExecutionHistoryPolicy {
    #[cfg_attr(target_arch="wasm32",wasm_bindgen::prelude::wasm_bindgen(js_name=revalidatePins))]
    pub fn revalidate_pins(
        &self,
        latest_independent_pins_json: &str,
        now_ms: f64,
    ) -> crate::Result<VerifiedRemoteExecutionHistoryPolicy> {
        Ok(Self {
            inner: self
                .inner
                .revalidate_pins(latest_independent_pins_json, now_ms)?,
        })
    }
    #[cfg_attr(target_arch="wasm32",wasm_bindgen::prelude::wasm_bindgen(js_name=metadataJson))]
    pub fn metadata_json(&self) -> crate::Result<String> {
        let reference = self.reference().map_err(|error| error.to_string())?;
        encode(
            &serde_json::json!({"purpose":"history","reference":reference,
            "pins":self.inner.snapshot.pins(),"heads":self.inner.snapshot.heads(),
            "expires_at_ms":self.inner.expires_at_ms()}),
        )
    }
    #[cfg_attr(target_arch="wasm32",wasm_bindgen::prelude::wasm_bindgen(js_name=proofUsageJson))]
    pub fn proof_usage_json(&self) -> crate::Result<String> {
        self.inner.proof_usage_json()
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::authorization::client_policy_import::*;
    use serde_json::json;

    fn wire<T: Serialize>(value: &T) -> String {
        serde_json::to_string(value).unwrap()
    }

    #[test]
    fn bounded_history_import_verifies_original_result_after_current_writer_revocation() {
        let now = 10_020;
        let mut fixture = execution_observation::tests::retained_history_fixture();
        let mut document = fixture
            .state
            .space(&fixture.actual.handoff.handoff.execution_resource.space)
            .unwrap()
            .clone();
        document.previous = fixture
            .state
            .head(&PolicyDomain::Space(document.space.clone()))
            .cloned();
        document.sequence += 1;
        document
            .grants
            .retain(|hash| hash != &fixture.actual.handoff.invocation.payload.grant_leaf);
        let revoked = Signed::sign(document, &fixture.root).unwrap();
        fixture.state.install_space(revoked.clone()).unwrap();
        fixture
            .execution_bundle
            .journal
            .push(JournalEntry::Space(revoked));
        for pin in &mut fixture.pins {
            pin.minimum_head = fixture.state.head(&pin.domain).cloned();
        }
        let mut independent: BTreeMap<_, _> = require_confirmed_client_policy(&fixture.source)
            .unwrap()
            .pins()
            .iter()
            .cloned()
            .map(|pin| (pin.domain.clone(), pin))
            .collect();
        for pin in &fixture.pins {
            if let Some(saved) = independent.insert(pin.domain.clone(), pin.clone()) {
                assert_eq!(saved.genesis_root, pin.genesis_root);
            }
        }
        let independent: Vec<_> = independent.into_values().collect();
        let input = json!({"actual_json":wire(&fixture.actual),"body_json":wire(&fixture.body),
            "invocation_json":wire(&fixture.invocation),"binding_json":wire(&fixture.binding)});
        let intent_json = build_remote_execution_history_policy_read_intent(
            &fixture.source,
            &wire(&input),
            &wire(&independent),
            &fixture.client,
            now as f64,
            (now + 1_000) as f64,
        )
        .unwrap();
        let intent: Signed<RemoteExecutionPolicyReadIntent> =
            serde_json::from_str(&intent_json).unwrap();
        assert_eq!(
            intent.payload.purpose,
            RemoteExecutionPolicyPurpose::History
        );
        let gate = verify_remote_execution_policy_delegation_source(
            require_confirmed_client_policy(&fixture.source)
                .unwrap()
                .state(),
            &intent,
            &PublicKey::of(&fixture.project_node),
            &PublicKey::of(&fixture.client),
            now,
        )
        .unwrap();
        let delegation =
            sign_remote_execution_policy_read_delegation(&gate, &fixture.project_node, now)
                .unwrap();
        let request = RemoteExecutionPolicyReadRequest { intent, delegation };
        let verified = verify_remote_execution_history_policy_read(
            &fixture.state,
            &request,
            &fixture.acceptance,
            &PublicKey::of(&fixture.executor),
            &PublicKey::of(&fixture.client),
            now,
        )
        .unwrap();
        assert!(
            verify_remote_execution_policy_read(
                &fixture.state,
                &request,
                &PublicKey::of(&fixture.executor),
                &PublicKey::of(&fixture.client),
                now
            )
            .is_err()
        );
        let header = Signed::sign(
            RemoteExecutionPolicySnapshot {
                version: AUTH_VERSION,
                request_hash: ObjectHash::body(&request).unwrap(),
                snapshot_id: Nonce::random().unwrap(),
                saved_pins_hash: ObjectHash::body(&independent).unwrap(),
                target_heads: verified.target_heads().to_vec(),
                source_node_public_key: PublicKey::of(&fixture.executor),
                issued_at_ms: now,
                expires_at_ms: now + 1_000,
                history: Some(fixture.acceptance.clone()),
            },
            &fixture.executor,
        )
        .unwrap();
        assert!(
            begin_remote_execution_policy_import(
                &fixture.source,
                &wire(&request),
                &wire(&header),
                &wire(&independent),
                PublicKey::of(&fixture.executor).as_str(),
                now as f64
            )
            .is_err()
        );
        let mut import = begin_remote_execution_history_policy_import(
            &fixture.source,
            &wire(&request),
            &wire(&header),
            &wire(&independent),
            PublicKey::of(&fixture.executor).as_str(),
            now as f64,
        )
        .unwrap();
        let entries: Vec<_> = fixture
            .execution_bundle
            .journal
            .iter()
            .filter(|entry| verified.allows_entry(entry).unwrap())
            .cloned()
            .collect();
        assert!(!entries.is_empty());
        let mut previous = header.hash().unwrap();
        for (index, entry) in entries.iter().enumerate() {
            let page_request: RemoteExecutionPolicyPageRequest =
                serde_json::from_str(&import.page_request_json().unwrap()).unwrap();
            let next_cursor =
                (index + 1 < entries.len()).then(|| policy_import::entry_hash(entry).unwrap());
            let page = Signed::sign(
                RemoteExecutionPolicyPage {
                    version: AUTH_VERSION,
                    request_hash: header.payload.request_hash.clone(),
                    snapshot_id: header.payload.snapshot_id.clone(),
                    target_heads_hash: ObjectHash::body(&header.payload.target_heads).unwrap(),
                    page_index: index as u64,
                    request_cursor: page_request.cursor,
                    previous_page_hash: previous,
                    entry: Some(entry.clone()),
                    complete: next_cursor.is_none(),
                    next_cursor,
                },
                &fixture.executor,
            )
            .unwrap();
            import.append_page(&wire(&page), now as f64).unwrap();
            previous = page.hash().unwrap();
        }
        let witness = Signed::sign(
            RemoteExecutionPolicyWitness {
                version: AUTH_VERSION,
                request_hash: header.payload.request_hash.clone(),
                snapshot_id: header.payload.snapshot_id.clone(),
                target_heads: header.payload.target_heads.clone(),
                final_page_hash: previous,
                page_count: entries.len() as u64,
                entry_count: entries.len() as u64,
                snapshot_hash: header.hash().unwrap(),
                source_node_public_key: PublicKey::of(&fixture.executor),
                issued_at_ms: now,
                expires_at_ms: now + 1_000,
            },
            &fixture.executor,
        )
        .unwrap();
        let history = import.finish(&wire(&witness), now as f64).unwrap();
        let result = remote_resolution_history::verify_remote_execution_result_history(
            &fixture.source,
            &history,
            &wire(&fixture.actual),
            &wire(&fixture.body),
            &wire(&fixture.invocation),
            &wire(&fixture.binding),
            now as f64,
        )
        .unwrap();
        let result: Value = serde_json::from_str(&result).unwrap();
        assert_eq!(result["historical"], true);
        assert_eq!(result["state"], "succeeded");
        assert_eq!(
            result["actual"],
            serde_json::to_value(&fixture.actual).unwrap()
        );
        let merged = policy_import::merge_same_head_snapshots(
            require_confirmed_client_policy(&fixture.source).unwrap(), history.snapshot(),
        ).unwrap();
        let event: events::Event = serde_json::from_str(
            &fixture.actual.handoff.handoff.project_dispatch.canonical_body,
        ).unwrap();
        let events::EventCause::Invocation { mut admission } = event.cause else {
            panic!("fixture has no original admission");
        };
        let original = &admission.invocation.payload;
        let facts = RequestFacts {
            resource: original.resource.clone(), action: original.action,
            audience: original.audience.clone(), operation_id: original.operation_id.clone(),
            body_hash: original.body_hash.clone(), context: original.context.clone(),
        };
        let execution_domain = PolicyDomain::Space(
            fixture.actual.handoff.handoff.execution_resource.space.clone(),
        );
        assert!(original.expected_policies.iter().any(|head| head.domain == execution_domain));
        let mut missing = original.clone();
        missing.expected_policies.retain(|head| head.domain != execution_domain);
        admission.invocation = Signed::sign(missing, &fixture.client).unwrap();
        let AuthorizationError::NeedPolicySync(heads) = remote_resolution_history::admitted(
            merged.state(), &admission, &facts,
        ).unwrap_err() else {
            panic!("missing signed execution policy must require explicit synchronization");
        };
        assert_eq!(heads, admission.challenge.policies);
        assert!(heads.iter().any(|head| head.domain == execution_domain));
        let mut wrong = fixture.body.clone();
        wrong["authorization_id"] = json!("another-history");
        assert!(
            remote_resolution_history::verify_remote_execution_result_history(
                &fixture.source,
                &history,
                &wire(&fixture.actual),
                &wire(&wrong),
                &wire(&fixture.invocation),
                &wire(&fixture.binding),
                now as f64
            )
            .is_err()
        );
    }

    fn domain(entry: &JournalEntry) -> PolicyDomain {
        match entry {
            JournalEntry::AccountGenesis(value) | JournalEntry::Account(value) => {
                PolicyDomain::Account(value.payload.account.clone())
            }
            JournalEntry::AccountCommit(value) => {
                PolicyDomain::Account(value.proposal.payload.account.clone())
            }
            JournalEntry::SpaceGenesis(value) | JournalEntry::Space(value) => {
                PolicyDomain::Space(value.payload.space.clone())
            }
            JournalEntry::SpaceCommit(value) => {
                PolicyDomain::Space(value.proposal.payload.space.clone())
            }
            JournalEntry::Device(value) => value.payload.owner.clone(),
            JournalEntry::Resource(value) => PolicyDomain::Space(value.payload.space.clone()),
            JournalEntry::Authenticator(value) => {
                PolicyDomain::Account(value.payload.account.clone())
            }
            JournalEntry::Membership(value) => PolicyDomain::Space(value.payload.space.clone()),
            JournalEntry::Grant(value) => PolicyDomain::Space(value.payload.owner_space.clone()),
        }
    }

    pub(crate) fn confirmed_source(
        bundle: &ClientAuthorizationBundle,
        pins: &[PolicyPin],
        client: &Identity,
        noise_source: &Identity,
        project: &ResourceRef,
        now: u64,
    ) -> crate::Result<VerifiedClientAuthorization> {
        let state = replay_policies(pins, &bundle.journal).map_err(|error| error.to_string())?;
        let account = match &state
            .find_device(&DeviceId::from_key(&PublicKey::of(client)))
            .map_err(|error| error.to_string())?
            .payload
            .subject
        {
            Actor::Account(account) => account.clone(),
            _ => return Err("fixture client must be an Account".into()),
        };
        let selection = AuthorizationSelection {
            resource: project.clone(),
            audience: state
                .grant(&bundle.grant_leaf)
                .unwrap()
                .payload
                .audience
                .clone(),
            grant_leaf: bundle.grant_leaf.clone(),
        };
        let request_json = build_client_policy_read_request(
            account.as_str(),
            project.space.as_str(),
            client,
            PublicKey::of(noise_source).as_str(),
            &wire(&pins),
            &wire(&Some(&selection)),
            now as f64,
            (now + 1_000) as f64,
        )?;
        let request: Signed<ClientPolicyReadRequest> = serde_json::from_str(&request_json).unwrap();
        let verified = verify_client_policy_read(
            &state,
            &request,
            &PublicKey::of(noise_source),
            &PublicKey::of(client),
            now,
        )
        .map_err(|error| error.to_string())?;
        let header = Signed::sign(
            ClientPolicySnapshot {
                version: AUTH_VERSION,
                request_hash: request.hash().unwrap(),
                snapshot_id: Nonce::random().unwrap(),
                saved_pins_hash: client_policy_saved_pins_hash(pins).unwrap(),
                target_heads: verified.heads().to_vec(),
                selection: selection.clone(),
                source_node_public_key: PublicKey::of(noise_source),
                issued_at_ms: now,
                expires_at_ms: now + 1_000,
            },
            noise_source,
        )
        .unwrap();
        let mut import = begin_client_policy_import(
            &wire(&pins),
            &request_json,
            &wire(&header),
            client,
            PublicKey::of(noise_source).as_str(),
            now as f64,
        )?;
        let entries: Vec<_> = bundle
            .journal
            .iter()
            .filter(|entry| verified.domains().contains(&domain(entry)))
            .cloned()
            .collect();
        assert!(!entries.is_empty());
        let mut previous = header.hash().unwrap();
        for (index, entry) in entries.iter().enumerate() {
            let request: ClientPolicyPageRequest =
                serde_json::from_str(&import.page_request_json()?).unwrap();
            let next =
                (index + 1 < entries.len()).then(|| policy_import::entry_hash(entry).unwrap());
            let page = Signed::sign(
                ClientPolicyPage {
                    version: AUTH_VERSION,
                    request_hash: header.payload.request_hash.clone(),
                    snapshot_id: header.payload.snapshot_id.clone(),
                    target_heads_hash: client_policy_target_heads_hash(verified.heads()).unwrap(),
                    page_index: index as u64,
                    request_cursor: request.cursor,
                    previous_page_hash: previous,
                    entry: Some(entry.clone()),
                    complete: next.is_none(),
                    next_cursor: next,
                },
                noise_source,
            )
            .unwrap();
            import.append_page(&wire(&page), now as f64)?;
            previous = page.hash().unwrap();
        }
        let witness = Signed::sign(
            ClientPolicyWitness {
                version: AUTH_VERSION,
                request_hash: header.payload.request_hash.clone(),
                snapshot_id: header.payload.snapshot_id.clone(),
                target_heads: verified.heads().to_vec(),
                final_page_hash: previous,
                page_count: entries.len() as u64,
                entry_count: entries.len() as u64,
                snapshot_hash: header.hash().unwrap(),
                source_node_public_key: PublicKey::of(noise_source),
                issued_at_ms: now,
                expires_at_ms: now + 1_000,
            },
            noise_source,
        )
        .unwrap();
        let authorization = import.finish(&wire(&witness), &wire(&selection), now as f64)?;
        let confirm_json = build_client_policy_auth_confirm_request(
            &authorization,
            client,
            now as f64,
            (now + 1_000) as f64,
        )?;
        let confirm: Signed<ClientPolicyAuthConfirmRequest> =
            serde_json::from_str(&confirm_json).unwrap();
        verify_client_policy_auth_confirm(
            &state,
            &request,
            &header,
            &witness,
            &confirm,
            &PublicKey::of(noise_source),
            &PublicKey::of(client),
            now,
        )
        .map_err(|error| error.to_string())?;
        let receipt = Signed::sign(
            ClientPolicyAuthConfirmation {
                version: AUTH_VERSION,
                context: confirm.payload.context.clone(),
                confirmation_request_hash: confirm.hash().unwrap(),
                selection,
                target_heads: verified.heads().to_vec(),
                device_public_key: PublicKey::of(client),
                source_node_public_key: PublicKey::of(noise_source),
                accepted_at_ms: now,
            },
            noise_source,
        )
        .unwrap();
        confirm_client_policy_authorization(
            &authorization,
            &confirm_json,
            &wire(&receipt),
            PublicKey::of(noise_source).as_str(),
            now as f64,
        )
    }

    pub(crate) struct RemotePolicyStream {
        pub(crate) request: RemoteExecutionPolicyReadRequest,
        pub(crate) header: Signed<RemoteExecutionPolicySnapshot>,
        pub(crate) pages: Vec<Signed<RemoteExecutionPolicyPage>>,
        pub(crate) witness: Signed<RemoteExecutionPolicyWitness>,
    }

    impl RemotePolicyStream {
        pub(crate) fn begin(
            &self,
            source: &VerifiedClientAuthorization,
            now: u64,
        ) -> crate::Result<RemoteExecutionPolicyImport> {
            begin_remote_execution_policy_import(
                source,
                &wire(&self.request),
                &wire(&self.header),
                &wire(&self.request.intent.payload.execution_pins),
                self.header.issuer.as_str(),
                now as f64,
            )
        }

        pub(crate) fn import(
            &self,
            source: &VerifiedClientAuthorization,
            now: u64,
        ) -> crate::Result<VerifiedRemoteExecutionPolicy> {
            let mut import = self.begin(source, now)?;
            for page in &self.pages {
                import.append_page(&wire(page), now as f64)?;
            }
            import.finish(&wire(&self.witness), now as f64)
        }
    }

    pub(crate) fn remote_policy_stream(
        source: &VerifiedClientAuthorization,
        client: &Identity,
        project_node: &Identity,
        execution_bundle: &ClientAuthorizationBundle,
        execution_pins: &[PolicyPin],
        execution_node: &Identity,
        selection: &RemoteExecutionPolicySelection,
        context: &PrepareContext,
        action: Action,
        purpose: RemoteExecutionPolicyPurpose,
        now: u64,
    ) -> crate::Result<RemotePolicyStream> {
        let intent_json = build_remote_execution_policy_read_intent(
            source,
            &wire(
                &json!({"selection": selection, "context": context, "expected_action": action,
                "purpose": purpose, "execution_pins": execution_pins}),
            ),
            client,
            now as f64,
            (now + 1_000) as f64,
        )?;
        let intent: Signed<RemoteExecutionPolicyReadIntent> =
            serde_json::from_str(&intent_json).unwrap();
        let source_gate = verify_remote_execution_policy_delegation_source(
            require_confirmed_client_policy(source).unwrap().state(),
            &intent,
            &PublicKey::of(project_node),
            &PublicKey::of(client),
            now,
        )
        .map_err(|error| error.to_string())?;
        let delegation =
            sign_remote_execution_policy_read_delegation(&source_gate, project_node, now)
                .map_err(|error| error.to_string())?;
        let request = RemoteExecutionPolicyReadRequest { intent, delegation };
        let execution_state = replay_policies(execution_pins, &execution_bundle.journal)
            .map_err(|error| error.to_string())?;
        let verified = verify_remote_execution_policy_read(
            &execution_state,
            &request,
            &PublicKey::of(execution_node),
            &PublicKey::of(client),
            now,
        )
        .map_err(|error| error.to_string())?;
        let header = Signed::sign(
            RemoteExecutionPolicySnapshot {
                version: AUTH_VERSION,
                request_hash: ObjectHash::body(&request).unwrap(),
                snapshot_id: Nonce::random().unwrap(),
                saved_pins_hash: ObjectHash::body(&execution_pins).unwrap(),
                target_heads: verified.target_heads().to_vec(),
                source_node_public_key: PublicKey::of(execution_node),
                issued_at_ms: now,
                expires_at_ms: now + 1_000,
                history: None,
            },
            execution_node,
        )
        .unwrap();
        let entries: Vec<_> = execution_bundle
            .journal
            .iter()
            .filter(|entry| verified.allows_entry(entry).unwrap())
            .cloned()
            .collect();
        assert!(!entries.is_empty());
        let mut previous = header.hash().unwrap();
        let mut cursor = None;
        let mut pages = Vec::new();
        for (index, entry) in entries.iter().enumerate() {
            let next =
                (index + 1 < entries.len()).then(|| policy_import::entry_hash(entry).unwrap());
            let page = Signed::sign(
                RemoteExecutionPolicyPage {
                    version: AUTH_VERSION,
                    request_hash: header.payload.request_hash.clone(),
                    snapshot_id: header.payload.snapshot_id.clone(),
                    target_heads_hash: ObjectHash::body(&verified.target_heads()).unwrap(),
                    page_index: index as u64,
                    request_cursor: cursor,
                    previous_page_hash: previous,
                    entry: Some(entry.clone()),
                    complete: next.is_none(),
                    next_cursor: next.clone(),
                },
                execution_node,
            )
            .unwrap();
            previous = page.hash().unwrap();
            cursor = next;
            pages.push(page);
        }
        let witness = Signed::sign(
            RemoteExecutionPolicyWitness {
                version: AUTH_VERSION,
                request_hash: header.payload.request_hash.clone(),
                snapshot_id: header.payload.snapshot_id.clone(),
                target_heads: verified.target_heads().to_vec(),
                final_page_hash: previous,
                page_count: pages.len() as u64,
                entry_count: entries.len() as u64,
                snapshot_hash: header.hash().unwrap(),
                source_node_public_key: PublicKey::of(execution_node),
                issued_at_ms: now,
                expires_at_ms: now + 1_000,
            },
            execution_node,
        )
        .unwrap();
        Ok(RemotePolicyStream {
            request,
            header,
            pages,
            witness,
        })
    }
}
