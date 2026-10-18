//! Read-only qualification of raw client intent before business-data normalization.

use super::*;
use serde_json::Value;

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PrepareContext {
    pub context: InvocationContext,
    pub binding_generation: Option<u64>,
    pub canonical_cwd: Option<String>,
    pub execution_target_public_key: Option<PublicKey>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PrepareIntent {
    pub request: Invocation,
    pub binding_generation: Option<u64>,
    pub canonical_cwd: Option<String>,
    pub execution_target_public_key: Option<PublicKey>,
}
signed_object!(PrepareIntent, "prepare-intent-v2");

/// A remote executor selected from policy roots already trusted by the caller.
#[derive(Clone, Debug)]
pub struct VerifiedExecutionDestination {
    project: ResourceRef,
    project_audience: Audience,
    resource: ResourceRef,
    audience: Audience,
    public_key: PublicKey,
    context: InvocationContext,
    action: Action,
    grant_leaf: ObjectHash,
    heads: Vec<PolicyStamp>,
    initiator: Actor,
    initiating_grant: Option<ObjectHash>,
    budget: Budget,
    grant_depth: usize,
}
impl VerifiedExecutionDestination {
    pub fn resource(&self) -> &ResourceRef {
        &self.resource
    }
    pub fn audience(&self) -> &Audience {
        &self.audience
    }
    pub fn public_key(&self) -> &PublicKey {
        &self.public_key
    }
    pub fn grant_leaf(&self) -> &ObjectHash {
        &self.grant_leaf
    }
    pub fn heads(&self) -> &[PolicyStamp] {
        &self.heads
    }
    pub fn initiator(&self) -> &Actor {
        &self.initiator
    }
    pub fn initiating_grant(&self) -> Option<&ObjectHash> {
        self.initiating_grant.as_ref()
    }
    pub fn check_facts(&self, facts: &RequestFacts) -> Result<()> {
        let mut context = facts.context.clone();
        context.sdk_session = None;
        require(
            facts.resource == self.project
                && facts.audience == self.project_audience
                && facts.action == self.action
                && context == self.context,
            "remote destination differs from Project facts",
        )
    }
}

fn remote_actions(actions: &[Action], resolver: bool) -> bool {
    if resolver {
        return actions.len() == 2
            && actions.contains(&Action::ExecutionResolve)
            && actions.contains(&Action::OperationResolve);
    }
    actions.contains(&Action::ExecutionSubmit)
        && actions
            .iter()
            .any(|action| *action != Action::ExecutionSubmit)
        && actions.iter().all(|action| {
            matches!(
                action,
                Action::ExecutionSubmit
                    | Action::ExecutionOpen
                    | Action::ExecutionSend
                    | Action::ExecutionSteer
                    | Action::ExecutionInterrupt
                    | Action::ExecutionRespondApproval
                    | Action::OperationStatusRead
            )
        })
}

impl VerifiedPolicySet {
    pub fn verify_execution_destination(
        &self,
        project: &ResourceRef,
        project_audience: &Audience,
        resource: &ResourceRef,
        public_key: &PublicKey,
        grant_leaf: &ObjectHash,
        context: &InvocationContext,
        action: Action,
        now_ms: u64,
    ) -> Result<VerifiedExecutionDestination> {
        let execution_action = match action {
                Action::ExecutionRebind => Action::ExecutionOpen,
                Action::ExecutionOpen
                    | Action::ExecutionSend
                    | Action::ExecutionSteer
                    | Action::ExecutionInterrupt
                    | Action::ExecutionRespondApproval
                    | Action::OperationStatusRead
                    | Action::OperationResolve => action,
                _ => return Err(AuthorizationError::InvalidProof("unsupported remote execution action")),
        };
        let project_record = self
            .resource(&project.space, &project.resource)
            .ok_or(AuthorizationError::InvalidProof("missing Project resource"))?;
        let execution = self.resource(&resource.space, &resource.resource).ok_or(
            AuthorizationError::InvalidProof("missing ExecutionService resource"),
        )?;
        let host = self.verify_service_device(&project.space, &project_audience.device, now_ms)?;
        let target_device = DeviceId::from_key(public_key);
        let target = self.verify_service_device(&resource.space, &target_device, now_ms)?;
        let audience = Audience {
            service: execution.payload.host.clone(),
            device: target_device,
        };
        require(
            project_record.payload.kind == ResourceKind::Project
                && project_record.payload.host == project_audience.service
                && host.payload.subject == Actor::Service(project_audience.service.clone())
                && host.payload.uses.contains(&DeviceUse::ServiceInvocation)
                && execution.payload.kind == ResourceKind::ExecutionService
                && execution.payload.host != project_audience.service
                && target.payload.public_key == *public_key
                && target.payload.subject == Actor::Service(audience.service.clone())
                && context
                    .execution_session
                    .as_ref()
                    .is_some_and(|session| session.target == audience.device)
                && context.task.is_some()
                && context.conversation.is_some()
                && context.executor_kind.is_some()
                && context.workspace_resource.as_ref() == Some(&project.resource),
            "remote destination must be an independent registered execution service",
        )?;
        let leaf = self
            .grant(grant_leaf)
            .ok_or(AuthorizationError::InvalidProof(
                "missing remote execution grant",
            ))?;
        let mut execution_context = context.clone();
        execution_context.sdk_session = None;
        execution_context.workspace_resource = Some(resource.resource.clone());
        require(
            leaf.payload.scope == ResourceScope::Exact(resource.clone())
                && leaf.payload.grantee
                    == Grantee::Actor(Actor::Service(project_audience.service.clone()))
                && leaf.payload.remaining_delegations == 0
                && leaf.payload.audience == audience
                && !leaf.payload.constraints.require_account_actor
                && !leaf.payload.constraints.require_webauthn_uv
                && leaf.payload.actions.contains(&execution_action)
                && remote_actions(&leaf.payload.actions, action == Action::OperationResolve)
                && leaf.payload.expires_at_ms < u64::MAX
                && leaf.payload.constraints.context == execution_context,
            "remote execution grant must exactly bind this task and executor",
        )?;
        let mut budget = Budget::default();
        let mut dependencies = BTreeMap::new();
        budget.verify(project_record)?;
        budget.verify(execution)?;
        for certificate in [host, target] {
            self.validate_certificate(
                certificate,
                now_ms,
                &mut budget,
                &mut dependencies,
                &mut BTreeSet::new(),
            )?;
        }
        let path = self.grant_chain(Some(grant_leaf))?;
        let (initiating, _) =
            self.verify_grant_path(&path, now_ms, &mut budget, &mut dependencies, true)?;
        self.expand_authorities(now_ms, &mut budget, &mut dependencies)?;
        let (initiator, initiating_grant) = match initiating {
            Some((actor, hash)) => (actor, Some(hash)),
            None => (Actor::Service(project_audience.service.clone()), None),
        };
        let mut context = context.clone();
        context.sdk_session = None;
        Ok(VerifiedExecutionDestination {
            project: project.clone(),
            project_audience: project_audience.clone(),
            resource: resource.clone(),
            audience,
            public_key: public_key.clone(),
            context,
            action,
            grant_leaf: grant_leaf.clone(),
            heads: dependencies.into_values().collect(),
            initiator,
            initiating_grant,
            budget,
            grant_depth: path.len(),
        })
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct RemoteExecutionSelection {
    execution_resource: ResourceRef,
    target_public_key: PublicKey,
    execution_grant: ObjectHash,
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = inspectRemoteExecutionShare))]
pub fn inspect_remote_execution_share(share_json: &str) -> crate::Result<String> {
    (|| -> Result<String> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Share {
            version: u16,
            kind: String,
            authorization: ClientAuthorizationBundle,
            selection: RemoteExecutionSelection,
        }
        require(
            share_json.len() <= 1_048_576,
            "execution share is too large",
        )?;
        let share: Share = serde_json::from_str(share_json)
            .map_err(|_| AuthorizationError::InvalidProof("invalid execution share"))?;
        require(
            share.version == AUTH_VERSION
                && share.kind == "vibeface-remote-execution"
                && share.authorization.grant_leaf == share.selection.execution_grant,
            "share selection differs from its Grant leaf",
        )?;
        let mut selected = None;
        for entry in &share.authorization.journal {
            if let JournalEntry::Grant(grant) = entry {
                if grant.hash()? == share.selection.execution_grant {
                    require(selected.is_none(), "duplicate selected execution Grant")?;
                    selected = Some(grant);
                }
            }
        }
        let signed = selected.ok_or(AuthorizationError::InvalidProof(
            "selected execution Grant is missing",
        ))?;
        signed.verify_signature()?;
        let grant = &signed.payload;
        require(
            grant.scope == ResourceScope::Exact(share.selection.execution_resource.clone())
                && grant.audience.device == DeviceId::from_key(&share.selection.target_public_key)
                && grant.remaining_delegations == 0
                && grant.expires_at_ms < u64::MAX
                && (remote_actions(&grant.actions, false) || remote_actions(&grant.actions, true))
                && grant.constraints.context.workspace_resource.as_ref()
                    == Some(&share.selection.execution_resource.resource)
                && grant
                    .constraints
                    .context
                    .execution_session
                    .as_ref()
                    .is_some_and(|session| session.target == grant.audience.device),
            "share candidate is not an exact execution or resolution Grant",
        )?;
        serde_json::to_string(
            &serde_json::json!({"selection":share.selection,"context":grant.constraints.context,
            "actions":grant.actions,"expires_at_ms":grant.expires_at_ms,"trust":"unverified"}),
        )
        .map_err(|_| AuthorizationError::InvalidProof("share candidate encoding failed"))
    })()
    .map_err(|error| error.to_string())
}

#[derive(Clone, Debug)]
#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen)]
pub struct VerifiedDirectionalGrantSnapshot {
    share: String,
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen)]
impl VerifiedDirectionalGrantSnapshot {
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = shareJson))]
    pub fn share_json(&self) -> String {
        self.share.clone()
    }
}

/// Export an already committed grant using the owner's independently saved roots.
#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = verifyDirectionalGrantSnapshot))]
pub fn verify_directional_grant_snapshot(
    bundle_json: &str,
    pins_json: &str,
    now_ms: f64,
) -> crate::Result<VerifiedDirectionalGrantSnapshot> {
    (|| -> Result<VerifiedDirectionalGrantSnapshot> {
        let now=client_time(now_ms)?;
        require(bundle_json.len() <= 1_048_576 && pins_json.len() <= MAX_PROOF_BYTES,"directional grant proof is too large")?;
        let bundle:ClientAuthorizationBundle=serde_json::from_str(bundle_json)
            .map_err(|_|AuthorizationError::InvalidProof("invalid directional grant bundle"))?;
        let pins:Vec<PolicyPin>=serde_json::from_str(pins_json)
            .map_err(|_|AuthorizationError::InvalidProof("invalid saved directional grant pins"))?;
        let state=replay_policies(&pins,&bundle.journal)?;
        let signed=state.grant(&bundle.grant_leaf).ok_or(AuthorizationError::InvalidProof("directional grant is missing"))?;
        let leaf=&signed.payload;
        let ResourceScope::Exact(resource)=&leaf.scope else { return Err(AuthorizationError::InvalidProof("execution sharing requires an exact resource")); };
        let record=state.resource(&resource.space,&resource.resource).ok_or(AuthorizationError::InvalidProof("shared execution resource is missing"))?;
        let target=state.verify_service_device(&resource.space,&leaf.audience.device,now)?;
        require(record.payload.kind == ResourceKind::ExecutionService && record.payload.host == leaf.audience.service
            && target.payload.subject == Actor::Service(leaf.audience.service.clone())
            && matches!(&leaf.grantee,Grantee::Actor(Actor::Service(service)) if service != &leaf.audience.service)
            && leaf.remaining_delegations == 0 && leaf.expires_at_ms < u64::MAX
            && (remote_actions(&leaf.actions,false) || remote_actions(&leaf.actions,true))
            && !leaf.constraints.require_account_actor && !leaf.constraints.require_webauthn_uv
            && leaf.constraints.context.sdk_session.is_none() && leaf.constraints.context.task.is_some()
            && leaf.constraints.context.conversation.is_some() && leaf.constraints.context.executor_kind.is_some()
            && leaf.constraints.context.workspace_resource.as_ref() == Some(&resource.resource)
            && leaf.constraints.context.execution_session.as_ref().is_some_and(|session|session.target == leaf.audience.device),
            "execution share must be a finite, nondelegable grant to the selected Project service")?;
        let mut budget=Budget::default();let mut dependencies=BTreeMap::new();
        budget.verify(record)?;
        state.validate_certificate(target,now,&mut budget,&mut dependencies,&mut BTreeSet::new())?;
        let path=state.grant_chain(Some(&bundle.grant_leaf))?;
        state.verify_grant_path(&path,now,&mut budget,&mut dependencies,true)?;
        state.expand_authorities(now,&mut budget,&mut dependencies)?;
        let selection=RemoteExecutionSelection {execution_resource:resource.clone(),target_public_key:target.payload.public_key.clone(),execution_grant:bundle.grant_leaf.clone()};
        let share=serde_json::to_string(&serde_json::json!({"version":AUTH_VERSION,"kind":"vibeface-remote-execution","authorization":bundle,"selection":selection}))
            .map_err(|_|AuthorizationError::InvalidProof("execution share encoding failed"))?;
        Ok(VerifiedDirectionalGrantSnapshot {share})
    })().map_err(|error|error.to_string())
}

#[derive(Clone, Debug)]
#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen)]
pub struct VerifiedRemoteExecution {
    state: VerifiedPolicySet,
    source: VerifiedClientAuthorization,
    selection: RemoteExecutionSelection,
    context: PrepareContext,
    action: Action,
    heads: Vec<PolicyStamp>,
    verified_at_ms: u64,
    execution_pins: Vec<PolicyPin>,
}

fn client_time(now: f64) -> Result<u64> {
    require(
        now.is_finite() && now >= 0.0 && now.fract() == 0.0 && now <= 9_007_199_254_740_991.0,
        "invalid remote client clock",
    )?;
    Ok(now as u64)
}

/// Both pin sets come from independently saved trust, never from an unknown Node response.
#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = verifyRemoteExecution))]
pub fn verify_remote_execution(
    auth: &VerifiedClientAuthorization,
    project_bundle_json: &str,
    project_pins_json: &str,
    execution_bundle_json: &str,
    execution_pins_json: &str,
    selection_json: &str,
    context_json: &str,
    expected_action: &str,
    now_ms: f64,
) -> crate::Result<VerifiedRemoteExecution> {
    (|| -> Result<VerifiedRemoteExecution> {
        let _signatures = crate::signature_verification::ScopedSignatureVerification::new();
        let now = client_time(now_ms)?;
        require(
            project_bundle_json.len() <= 1_048_576
                && execution_bundle_json.len() <= 1_048_576
                && project_pins_json.len() <= MAX_PROOF_BYTES
                && execution_pins_json.len() <= MAX_PROOF_BYTES
                && selection_json.len() <= MAX_PROOF_BYTES
                && context_json.len() <= MAX_PROOF_BYTES,
            "remote execution proof is too large",
        )?;
        let project: ClientAuthorizationBundle = serde_json::from_str(project_bundle_json)
            .map_err(|_| AuthorizationError::InvalidProof("invalid Project policy bundle"))?;
        let execution: ClientAuthorizationBundle = serde_json::from_str(execution_bundle_json)
            .map_err(|_| AuthorizationError::InvalidProof("invalid execution policy bundle"))?;
        let project_pins: Vec<PolicyPin> = serde_json::from_str(project_pins_json)
            .map_err(|_| AuthorizationError::InvalidProof("invalid Project pins"))?;
        let execution_pins: Vec<PolicyPin> = serde_json::from_str(execution_pins_json)
            .map_err(|_| AuthorizationError::InvalidProof("invalid independent execution pins"))?;
        let (pins, tail) =
            client_policy_tail(auth, &project, &project_pins, &execution, &execution_pins)?;
        let state = replay_policy_tail(auth.policy_snapshot.clone(), &pins, &tail)?;
        let baseline = revalidate_client_policy(auth, state.clone(), &project, &pins, now)?;
        let selection: RemoteExecutionSelection = serde_json::from_str(selection_json)
            .map_err(|_| AuthorizationError::InvalidProof("invalid remote execution selection"))?;
        require(
            selection.execution_grant == execution.grant_leaf,
            "execution bundle has another grant",
        )?;
        let context: PrepareContext = serde_json::from_str(context_json)
            .map_err(|_| AuthorizationError::InvalidProof("invalid remote prepare context"))?;
        let action: Action = serde_json::from_value(Value::String(expected_action.into()))
            .map_err(|_| AuthorizationError::InvalidProof("unknown remote action"))?;
        let project = ResourceRef {
            space: auth.scope.space_id.clone(),
            resource: context.context.workspace_resource.clone().ok_or(
                AuthorizationError::InvalidProof("remote Project resource is missing"),
            )?,
        };
        require(
            scope_covers(&auth.grant_scope, &project)
                && baseline.actions.contains(&action)
                && context_tighter(&baseline.constraints.context, &context.context)
                && context.execution_target_public_key.as_ref()
                    == Some(&selection.target_public_key),
            "remote selection differs from the verified client scope",
        )?;
        if baseline.constraints.require_webauthn_uv {
            return Err(AuthorizationError::UserPresenceRequired);
        }
        let destination = state.verify_execution_destination(
            &project,
            &auth.audience,
            &selection.execution_resource,
            &selection.target_public_key,
            &selection.execution_grant,
            &context.context,
            action,
            now,
        )?;
        let target = state.find_device(&DeviceId::from_key(&selection.target_public_key))?;
        let mut budget = Budget::default();
        let mut dependencies = baseline
            .heads
            .iter()
            .cloned()
            .map(|head| (head.domain.clone(), head))
            .collect();
        state.validate_certificate(
            target,
            now,
            &mut budget,
            &mut dependencies,
            &mut BTreeSet::new(),
        )?;
        state.expand_authorities(now, &mut budget, &mut dependencies)?;
        let mut usage = execution_handoff::ExecutionProofUsage::default();
        usage.record_budget(&baseline.budget)?;
        usage.record_budget(&destination.budget)?;
        usage.record_chain_depth(destination.grant_depth);
        usage.record_chain_depth(state.grant_chain(Some(&baseline.grant_leaf))?.len());
        usage.record_budget(&budget)?;
        usage.record_metadata(&selection)?;
        usage.record_metadata(&context)?;
        usage.record_actual_signature_counts(_signatures.counts());
        usage.enforce_limits()?;
        let execution_pins = execution_pins
            .into_iter()
            .map(|mut pin| {
                pin.minimum_head = Some(state.head(&pin.domain).cloned().ok_or(
                    AuthorizationError::InvalidProof("independent execution policy is missing"),
                )?);
                Ok(pin)
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(VerifiedRemoteExecution {
            state,
            source: auth.clone(),
            selection,
            context,
            action,
            heads: dependencies.into_values().collect(),
            verified_at_ms: now,
            execution_pins,
        })
    })()
    .map_err(|error| error.to_string())
}

impl VerifiedRemoteExecution {
    fn checked_observation(
        &self,
        actual_json: &str,
        binding_json: &str,
        now_ms: u64,
    ) -> Result<execution_handoff::ExecutionHandoffResult> {
        require(
            actual_json.len() <= execution_handoff::MAX_EXECUTION_HANDOFF_BYTES
                && binding_json.len() <= MAX_PROOF_BYTES
                && now_ms >= self.verified_at_ms,
            "remote observation proof is outside its bounds",
        )?;
        let actual: execution_handoff::ExecutionHandoffResult =
            serde_json::from_str(actual_json)
                .map_err(|_| AuthorizationError::InvalidProof("invalid remote observation"))?;
        let binding: task_binding::SignedBinding = serde_json::from_str(binding_json)
            .map_err(|_| AuthorizationError::InvalidProof("invalid observation binding"))?;
        self.state
            .verify_project_execution_source(&actual.handoff.handoff, now_ms)?;
        execution_handoff::verify_execution_handoff_receipt_signature(
            &actual.handoff,
            &actual.receipt,
            &actual.result,
        )?;
        let event: events::Event =
            serde_json::from_str(&actual.handoff.handoff.project_dispatch.canonical_body)
                .map_err(|_| AuthorizationError::InvalidProof("invalid observation dispatch"))?;
        let events::EventCause::Invocation { admission } = &event.cause else {
            return Err(AuthorizationError::InvalidProof(
                "observation has no original admission",
            ));
        };
        admission.invocation.verify_signature()?;
        actual.handoff.invocation.verify_signature()?;
        require(
            actual.handoff.handoff.execution_resource == self.selection.execution_resource
                && actual.handoff.handoff.target_public_key == self.selection.target_public_key
                && actual.handoff.invocation.issuer == self.source.audience_public_key
                && actual.handoff.invocation.payload.action == Action::ExecutionSubmit
                && actual.handoff.invocation.payload.resource == self.selection.execution_resource
                && admission.invocation.payload.resource.space == self.source.scope.space_id
                && Some(&admission.invocation.payload.resource.resource)
                    == self.context.context.workspace_resource.as_ref()
                && scope_covers(
                    &self.source.grant_scope,
                    &admission.invocation.payload.resource,
                )
                && admission.invocation.payload.audience == self.source.audience
                && admission.invocation.payload.context.task == self.context.context.task
                && admission.invocation.payload.context.conversation
                    == self.context.context.conversation
                && admission.invocation.payload.context.execution_session
                    == self.context.context.execution_session
                && admission.invocation.payload.context.executor_kind
                    == self.context.context.executor_kind
                && admission.invocation.payload.body_hash
                    == ObjectHash::body(&admission.source_body)?
                && admission.source_body["request"]["binding"]
                    == serde_json::to_value(&binding).map_err(|_| {
                        AuthorizationError::InvalidProof("observation binding encoding failed")
                    })?
                && actual.receipt.payload.issued_at_ms <= now_ms
                && actual.receipt.payload.issued_at_ms
                    >= actual.handoff.invocation.payload.not_before_ms,
            "remote observation differs from the selected Project task",
        )?;
        Ok(actual)
    }

    fn checked_result(
        &self,
        actual_json: &str,
        expected_body_json: &str,
        expected_invocation_json: &str,
        binding_json: &str,
        now_ms: u64,
    ) -> Result<execution_handoff::ExecutionHandoffResult> {
        require(
            expected_body_json.len() <= MAX_PROOF_BYTES
                && expected_invocation_json.len() <= MAX_PROOF_BYTES,
            "original browser proof is outside its bounds",
        )?;
        let actual = self.checked_observation(actual_json, binding_json, now_ms)?;
        let body: Value = serde_json::from_str(expected_body_json)
            .map_err(|_| AuthorizationError::InvalidProof("invalid original browser body"))?;
        let invocation: Signed<Invocation> = serde_json::from_str(expected_invocation_json)
            .map_err(|_| AuthorizationError::InvalidProof("invalid original browser invocation"))?;
        let event: events::Event =
            serde_json::from_str(&actual.handoff.handoff.project_dispatch.canonical_body)
                .map_err(|_| AuthorizationError::InvalidProof("invalid result dispatch"))?;
        let events::EventCause::Invocation { admission } = &event.cause else {
            return Err(AuthorizationError::InvalidProof(
                "result dispatch has no admission",
            ));
        };
        require(
            admission.source_body == body
                && admission.invocation == invocation
                && invocation.payload.device == self.source.scope.device_id
                && invocation.issuer
                    == self
                        .state
                        .find_device(&self.source.scope.device_id)?
                        .payload
                        .public_key
                && invocation.payload.actor == Actor::Account(self.source.scope.account_id.clone()),
            "remote result differs from the original browser authorization",
        )?;
        Ok(actual)
    }

    fn signing_authorization(
        &self,
        auth: &VerifiedClientAuthorization,
        body: &Value,
        now: u64,
    ) -> Result<VerifiedClientAuthorization> {
        require(
            now >= self.verified_at_ms
                && auth.scope == self.source.scope
                && auth.heads == self.source.heads
                && auth.grant_leaf == self.source.grant_leaf
                && auth.audience == self.source.audience
                && auth.audience_public_key == self.source.audience_public_key,
            "remote handle belongs to a different or refreshed Project authorization",
        )?;
        let selected = remote_selection(body)?.ok_or(AuthorizationError::InvalidProof(
            "remote selection is missing",
        ))?;
        require(
            selected
                == (
                    self.selection.execution_resource.clone(),
                    self.selection.target_public_key.clone(),
                    self.selection.execution_grant.clone(),
                )
                && raw_resource(body)?.space == auth.scope.space_id,
            "request changed its independently verified execution destination",
        )?;
        let project = raw_resource(body)?;
        self.state.verify_execution_destination(
            &project,
            &auth.audience,
            &self.selection.execution_resource,
            &self.selection.target_public_key,
            &self.selection.execution_grant,
            &self.context.context,
            self.action,
            now,
        )?;
        let mut result = auth.clone();
        result.heads = self.heads.clone();
        Ok(result)
    }
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen)]
impl VerifiedRemoteExecution {
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = signObservationRequest))]
    pub fn sign_observation_request(
        &self,
        original_json: &str,
        authorization_id: &str,
        expected_gateway_public_key: &str,
        signer: &Identity,
        now_ms: f64,
    ) -> crate::Result<String> {
        let _signatures = crate::signature_verification::ScopedSignatureVerification::new();
        (|| -> Result<String> {
            let now = client_time(now_ms)?;
            require(
                original_json.len() <= execution_handoff::MAX_EXECUTION_HANDOFF_BYTES,
                "observation handoff is too large",
            )?;
            let original: execution_handoff::SignedExecutionHandoff =
                serde_json::from_str(original_json)
                    .map_err(|_| AuthorizationError::InvalidProof("invalid original handoff"))?;
            let target =
                execution_observation::ExecutionObservationTarget::from_original(&original)?;
            require(
                self.action == Action::OperationResolve
                    && now >= self.verified_at_ms
                    && DeviceId::from_key(&PublicKey::of(signer)) == self.source.scope.device_id
                    && target.source.project.space == self.source.scope.space_id
                    && self.source.resources.iter().any(|record| {
                        record.payload.space == target.source.project.space
                            && record.payload.resource == target.source.project.resource
                    })
                    && target.execution_resource == self.selection.execution_resource
                    && target.target_public_key == self.selection.target_public_key
                    && self.context.context == target.context(&target.source.project.resource)
                    && self.context.binding_generation == Some(target.source.binding_generation)
                    && self.context.execution_target_public_key.as_ref()
                        == Some(&target.target_public_key)
                    && original
                        .handoff
                        .source_attestation
                        .as_ref()
                        .is_some_and(|source| source.issuer == self.source.audience_public_key),
                "observation handle differs from its original operation or current signer",
            )?;
            let body = serde_json::json!({
                "method": "resolve_unknown",
                "space_id": target.source.project.space,
                "resource_id": target.source.project.resource,
                "execution_resource": self.selection.execution_resource,
                "target_public_key": self.selection.target_public_key,
                "execution_grant": self.selection.execution_grant,
            });
            self.signing_authorization(&self.source, &body, now)?;
            let expires =
                now.checked_add(INVOCATION_MAX_AGE_MS)
                    .ok_or(AuthorizationError::InvalidProof(
                        "observation expiry overflow",
                    ))?;
            let signed = self.state.build_forwarded_execution_observation_request(
                &original,
                signer,
                &self.source.grant_leaf,
                &self.selection.execution_grant,
                authorization_id,
                &PublicKey::parse(expected_gateway_public_key)?,
                now,
                expires,
            )?;
            serde_json::to_string(&signed).map_err(|_| {
                AuthorizationError::InvalidProof("observation request encoding failed")
            })
        })()
        .map_err(|error| error.to_string())
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = resolutionResultJson))]
    pub fn resolution_result_json(
        &self,
        actual_json: &str,
        body_json: &str,
        invocation_json: &str,
        now_ms: f64,
    ) -> crate::Result<String> {
        (|| -> Result<String> {
            use execution_handoff::*;
            let now = client_time(now_ms)?;
            require(
                self.action == Action::OperationResolve
                    && actual_json.len() <= MAX_EXECUTION_HANDOFF_BYTES
                    && body_json.len() <= MAX_PROOF_BYTES
                    && invocation_json.len() <= MAX_PROOF_BYTES,
                "invalid resolver result proof bounds",
            )?;
            let expected_body: Value = serde_json::from_str(body_json)
                .map_err(|_| AuthorizationError::InvalidProof("invalid saved resolver body"))?;
            let expected_invocation: Signed<Invocation> = serde_json::from_str(invocation_json)
                .map_err(|_| {
                    AuthorizationError::InvalidProof("invalid saved resolver invocation")
                })?;
            expected_invocation.verify_signature()?;
            let raw: Value = serde_json::from_str(actual_json).map_err(|_| {
                AuthorizationError::InvalidProof("invalid executor resolution result")
            })?;
            let unknown = raw.get("resolution").is_some();
            let (wire, freshness, decision_time, state, purpose, encoded) = if unknown {
                let result: ExecutionHandoffResolutionResult = serde_json::from_value(raw)
                    .map_err(|_| {
                        AuthorizationError::InvalidProof("invalid Unknown resolution result")
                    })?;
                let clock = result.receipt.payload.issued_at_ms;
                (
                    result.resolution.clone(),
                    result.freshness.clone(),
                    clock,
                    result.handoff_receipt.payload.state,
                    ExecutionResolutionPurpose::ResolveUnknown,
                    serde_json::to_value(result),
                )
            } else {
                let result: ExecutionHandoffTerminalReconciliationResult =
                    serde_json::from_value(raw).map_err(|_| {
                        AuthorizationError::InvalidProof("invalid terminal reconciliation result")
                    })?;
                let clock = result
                    .freshness
                    .receipts
                    .iter()
                    .map(|r| r.payload.issued_at_ms)
                    .max()
                    .unwrap_or(result.freshness.challenge.issued_at_ms);
                (
                    result.reconciliation.clone(),
                    result.freshness.clone(),
                    clock,
                    result
                        .reconciliation
                        .resolution
                        .observation
                        .receipt
                        .payload
                        .state,
                    ExecutionResolutionPurpose::ReconcileTerminal,
                    serde_json::to_value(result),
                )
            };
            let encoded = encoded
                .map_err(|_| AuthorizationError::InvalidProof("resolution encoding failed"))?;
            let admission = &wire.resolution.account_admission;
            let facts = execution_resolution_account_facts(&wire.resolution)?;
            require(
                decision_time <= now
                    && now >= self.verified_at_ms
                    && admission.source_body == expected_body
                    && admission.invocation == expected_invocation
                    && admission.invocation.payload.device == self.source.scope.device_id
                    && admission.invocation.issuer
                        == self
                            .state
                            .find_device(&self.source.scope.device_id)?
                            .payload
                            .public_key
                    && admission.invocation.payload.actor
                        == Actor::Account(self.source.scope.account_id.clone())
                    && facts.resource.space == self.source.scope.space_id
                    && Some(&facts.resource.resource)
                        == self.context.context.workspace_resource.as_ref()
                    && facts.audience == self.source.audience
                    && facts.context == self.context.context
                    && wire.invocation.issuer == self.source.audience_public_key
                    && wire.invocation.payload.grant_leaf == self.selection.execution_grant
                    && wire
                        .resolution
                        .observation
                        .handoff
                        .handoff
                        .execution_resource
                        == self.selection.execution_resource
                    && wire
                        .resolution
                        .observation
                        .handoff
                        .handoff
                        .target_public_key
                        == self.selection.target_public_key,
                "resolution result differs from the original browser decision",
            )?;
            let transport = admission.transport.clone().into_evidence();
            let request = self.state.verify_invocation(
                &admission.invocation,
                &facts,
                &transport,
                admission.admitted_at_ms,
            )?;
            let fresh = self.state.verify_freshness(
                &request,
                &admission.challenge,
                &admission.receipts,
                admission.admitted_at_ms,
            )?;
            let admitted =
                self.state
                    .admit_operation(&request, &fresh, admission.admitted_at_ms)?;
            let resolver = self.state.verify_operation_authorization(
                &admitted,
                &facts,
                &transport,
                decision_time,
            )?;
            if unknown {
                let result: ExecutionHandoffResolutionResult =
                    serde_json::from_value(encoded.clone()).map_err(|_| {
                        AuthorizationError::InvalidProof("invalid resolution encoding")
                    })?;
                let verified = self.state.verify_execution_resolution_ack(
                    &result,
                    &resolver,
                    decision_time,
                )?;
                self.state.verify_execution_resolution_freshness(
                    &verified,
                    &freshness,
                    decision_time,
                )?;
            } else {
                let result: ExecutionHandoffTerminalReconciliationResult =
                    serde_json::from_value(encoded.clone()).map_err(|_| {
                        AuthorizationError::InvalidProof("invalid terminal encoding")
                    })?;
                self.state.verify_execution_terminal_reconciliation(
                    &result,
                    &resolver,
                    decision_time,
                )?;
            }
            serde_json::to_string(
                &serde_json::json!({"state":state,"purpose":purpose,"actual":encoded}),
            )
            .map_err(|_| AuthorizationError::InvalidProof("verified resolution encoding failed"))
        })()
        .map_err(|error| error.to_string())
    }
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = observationJson))]
    pub fn observation_json(
        &self,
        actual_json: &str,
        binding_json: &str,
        now_ms: f64,
    ) -> crate::Result<String> {
        (|| -> Result<String> {
            require(
                self.source.actions.iter().any(|action| {
                    matches!(
                        action,
                        Action::OperationStatusRead
                            | Action::OperationReconcile
                            | Action::OperationResolve
                    )
                }),
                "client has no Project operation read permission",
            )?;
            let actual =
                self.checked_observation(actual_json, binding_json, client_time(now_ms)?)?;
            serde_json::to_string(&actual)
                .map_err(|_| AuthorizationError::InvalidProof("observation encoding failed"))
        })()
        .map_err(|error| error.to_string())
    }
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = resolutionBodyJson))]
    pub fn resolution_body_json(
        &self,
        actual_json: &str,
        authorization_id: &str,
        outcome: &str,
        evidence: &str,
        now_ms: f64,
    ) -> crate::Result<String> {
        (|| -> Result<String> {
            let now = client_time(now_ms)?;
            require(
                self.action == Action::OperationResolve
                    && actual_json.len() <= execution_handoff::MAX_EXECUTION_HANDOFF_BYTES,
                "resolution needs an independently verified resolver grant",
            )?;
            let actual: execution_handoff::ExecutionHandoffResult =
                serde_json::from_str(actual_json).map_err(|_| {
                    AuthorizationError::InvalidProof("invalid resolution observation")
                })?;
            self.state
                .verify_project_execution_source(&actual.handoff.handoff, now)?;
            let (body, context) = remote_resolution_intent(
                &actual,
                &self.selection,
                authorization_id,
                outcome,
                evidence,
                &self.source.audience_public_key,
                now,
            )?;
            require(
                context == self.context,
                "resolver handle differs from the original task binding",
            )?;
            self.signing_authorization(&self.source, &body, now)?;
            serde_json::to_string(&serde_json::json!({"body":body,"context":context}))
                .map_err(|_| AuthorizationError::InvalidProof("resolution intent encoding failed"))
        })()
        .map_err(|error| error.to_string())
    }
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = resultJson))]
    pub fn result_json(
        &self,
        actual_json: &str,
        expected_body_json: &str,
        expected_invocation_json: &str,
        binding_json: &str,
        now_ms: f64,
    ) -> crate::Result<String> {
        (|| -> Result<String> {
            let actual = self.checked_result(
                actual_json,
                expected_body_json,
                expected_invocation_json,
                binding_json,
                client_time(now_ms)?,
            )?;
            serde_json::to_string(&actual).map_err(|_| {
                AuthorizationError::InvalidProof("verified remote result encoding failed")
            })
        })()
        .map_err(|error| error.to_string())
    }
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = selectionJson))]
    pub fn selection_json(&self) -> crate::Result<String> {
        serde_json::to_string(&self.selection)
            .map_err(|_| "remote selection encoding failed".into())
    }
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = metadataJson))]
    pub fn metadata_json(&self) -> crate::Result<String> {
        let record = self
            .state
            .resource(
                &self.selection.execution_resource.space,
                &self.selection.execution_resource.resource,
            )
            .ok_or_else(|| "verified execution resource is missing".to_owned())?;
        serde_json::to_string(&serde_json::json!({"selection":self.selection,"context":self.context,
            "audience":Audience {service:record.payload.host.clone(),device:DeviceId::from_key(&self.selection.target_public_key)},
            "pins":self.execution_pins})).map_err(|_| "remote metadata encoding failed".into())
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = sessionJson))]
    pub fn session_json(
        &self,
        receipt_json: &str,
        binding_json: &str,
        now_ms: f64,
    ) -> crate::Result<String> {
        (|| -> Result<String> {
            let now = client_time(now_ms)?;
            require(
                now >= self.verified_at_ms
                    && receipt_json.len() <= MAX_PROOF_BYTES
                    && binding_json.len() <= MAX_PROOF_BYTES,
                "invalid remote session proof bounds",
            )?;
            let receipt: Signed<execution_handoff::ExecutionHandoffReceipt> =
                serde_json::from_str(receipt_json).map_err(|_| {
                    AuthorizationError::InvalidProof("invalid executor session receipt")
                })?;
            let binding: task_binding::SignedBinding =
                serde_json::from_str(binding_json).map_err(|_| {
                    AuthorizationError::InvalidProof("invalid executor session binding")
                })?;
            let project = ResourceRef {
                space: self.source.scope.space_id.clone(),
                resource: self.context.context.workspace_resource.clone().ok_or(
                    AuthorizationError::InvalidProof("remote Project context is missing"),
                )?,
            };
            self.state.verify_execution_destination(
                &project,
                &self.source.audience,
                &self.selection.execution_resource,
                &self.selection.target_public_key,
                &self.selection.execution_grant,
                &self.context.context,
                self.action,
                now,
            )?;
            let snapshot = execution_handoff::verify_execution_session_snapshot(
                &receipt,
                &project,
                &self.selection.execution_resource,
                &self.selection.target_public_key,
                &binding,
                &self.source.audience_public_key,
                now,
            )?;
            require(
                self.context.context.task.as_ref() == Some(&snapshot.task)
                    && self.context.context.conversation.as_deref()
                        == Some(binding.binding.conversation_id.as_str())
                    && self
                        .context
                        .context
                        .execution_session
                        .as_ref()
                        .is_some_and(|session| {
                            session.target == snapshot.target
                                && session.session_id == snapshot.session_id
                        }),
                "session receipt differs from the selected task",
            )?;
            let mut value = serde_json::to_value(snapshot).map_err(|_| {
                AuthorizationError::InvalidProof("session snapshot encoding failed")
            })?;
            value["target_node"] = Value::String(binding.binding.target_node.clone());
            value["execution_session_receipt"] = serde_json::to_value(&receipt)
                .map_err(|_| AuthorizationError::InvalidProof("session receipt encoding failed"))?;
            serde_json::to_string(&value)
                .map_err(|_| AuthorizationError::InvalidProof("session proof encoding failed"))
        })()
        .map_err(|error| error.to_string())
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = validateReplay))]
    pub fn validate_replay(
        &self,
        auth: &VerifiedClientAuthorization,
        body_json: &str,
        invocation_json: &str,
        expected_action: &str,
        operation_id: &str,
        gateway_public_key: &str,
        now_ms: f64,
    ) -> crate::Result<()> {
        (|| -> Result<()> {
            require(
                body_json.len() <= MAX_PROOF_BYTES && invocation_json.len() <= MAX_PROOF_BYTES,
                "remote replay proof is too large",
            )?;
            let now = client_time(now_ms)?;
            let body: Value = serde_json::from_str(body_json)
                .map_err(|_| AuthorizationError::InvalidProof("invalid remote replay body"))?;
            let invocation: Signed<Invocation> =
                serde_json::from_str(invocation_json).map_err(|_| {
                    AuthorizationError::InvalidProof("invalid remote replay invocation")
                })?;
            let qualified = self.signing_authorization(auth, &body, now)?;
            let i = &invocation.payload;
            let action: Action = serde_json::from_value(Value::String(expected_action.into()))
                .map_err(|_| AuthorizationError::InvalidProof("unknown remote replay action"))?;
            require(
                i.actor == Actor::Account(qualified.scope.account_id.clone())
                    && i.device == qualified.scope.device_id
                    && i.grant_leaf == qualified.grant_leaf
                    && i.expected_policies == qualified.heads
                    && i.action == action
                    && action == self.action
                    && i.operation_id == operation_id
                    && i.context == self.context.context,
                "remote replay changed its verified client authorization",
            )?;
            let intent = PrepareIntent {
                request: i.clone(),
                binding_generation: self.context.binding_generation,
                canonical_cwd: self.context.canonical_cwd.clone(),
                execution_target_public_key: self.context.execution_target_public_key.clone(),
            };
            check_raw_context(&intent, &body, &qualified.audience_public_key)?;
            let gateway = PublicKey::parse(gateway_public_key)?;
            let transport = if gateway == qualified.audience_public_key {
                TransportEvidence::DirectNoise {
                    device: qualified.scope.device_id.clone(),
                }
            } else {
                TransportEvidence::GatewayNoise {
                    device: qualified.scope.device_id.clone(),
                    gateway: DeviceId::from_key(&gateway),
                }
            };
            let facts = RequestFacts {
                resource: raw_resource(&body)?,
                action,
                audience: qualified.audience.clone(),
                operation_id: operation_id.into(),
                body_hash: ObjectHash::body(&body)?,
                context: self.context.context.clone(),
            };
            self.state
                .verify_invocation(&invocation, &facts, &transport, now)?;
            Ok(())
        })()
        .map_err(|error| error.to_string())
    }

    pub fn prepare(
        &self,
        auth: &VerifiedClientAuthorization,
        body_json: &str,
        context_json: &str,
        expected_action: &str,
        operation_id: &str,
        gateway_public_key: &str,
        identity: &Identity,
        now_ms: f64,
    ) -> crate::Result<String> {
        let now = client_time(now_ms).map_err(|error| error.to_string())?;
        let body: Value =
            serde_json::from_str(body_json).map_err(|_| "invalid remote request".to_owned())?;
        let context: PrepareContext =
            serde_json::from_str(context_json).map_err(|_| "invalid remote context".to_owned())?;
        require(
            context == self.context
                && serde_json::to_value(self.action)
                    .ok()
                    .and_then(|value| value.as_str().map(str::to_owned))
                    .as_deref()
                    == Some(expected_action),
            "remote context or action differs from its verified handle",
        )
        .map_err(|error| error.to_string())?;
        let qualified = self
            .signing_authorization(auth, &body, now)
            .map_err(|error| error.to_string())?;
        sign_client_prepare_intent(
            &qualified,
            body_json,
            context_json,
            expected_action,
            operation_id,
            gateway_public_key,
            identity,
            now_ms,
        )
    }
    pub fn sign(
        &self,
        auth: &VerifiedClientAuthorization,
        template_json: &str,
        body_json: &str,
        expected_action: &str,
        operation_id: &str,
        gateway_public_key: &str,
        identity: &Identity,
        now_ms: f64,
    ) -> crate::Result<String> {
        (|| -> Result<String> {
            let now = client_time(now_ms)?;
            let body: Value = serde_json::from_str(body_json)
                .map_err(|_| AuthorizationError::InvalidProof("invalid remote request"))?;
            let invocation: Invocation = serde_json::from_str(template_json)
                .map_err(|_| AuthorizationError::InvalidProof("invalid remote invocation"))?;
            let auth = self.signing_authorization(auth, &body, now)?;
            require(
                invocation.context == self.context.context && invocation.action == self.action,
                "remote invocation changed its verified context or action",
            )?;
            let intent = PrepareIntent {
                request: invocation.clone(),
                binding_generation: self.context.binding_generation,
                canonical_cwd: self.context.canonical_cwd.clone(),
                execution_target_public_key: self.context.execution_target_public_key.clone(),
            };
            check_raw_context(&intent, &body, &auth.audience_public_key)?;
            let gateway = PublicKey::parse(gateway_public_key)?;
            require(
                gateway != PublicKey::of(identity),
                "gateway cannot be client key",
            )?;
            let presenter = if gateway == auth.audience_public_key {
                DeviceId::from_key(&PublicKey::of(identity))
            } else {
                DeviceId::from_key(&gateway)
            };
            sign_client_invocation_inner(
                &auth,
                template_json,
                body_json,
                expected_action,
                operation_id,
                identity,
                now_ms,
                &presenter,
            )
        })()
        .map_err(|error| error.to_string())
    }
}

fn remote_selection(body: &Value) -> Result<Option<(ResourceRef, PublicKey, ObjectHash)>> {
    let fields = [
        body.get("execution_resource"),
        body.get("target_public_key"),
        body.get("execution_grant"),
    ];
    if fields.iter().all(|field| field.is_none()) {
        return Ok(None);
    }
    require(
        fields
            .iter()
            .all(|field| field.is_some_and(|value| !value.is_null())),
        "remote execution selection is incomplete",
    )?;
    require(
        matches!(
            body["method"].as_str(),
            Some("operate" | "bind_execution" | "rebind_execution" | "resolve_unknown")
        ),
        "remote execution selection is not valid for this method",
    )?;
    Ok(Some((
        serde_json::from_value(body["execution_resource"].clone())
            .map_err(|_| AuthorizationError::InvalidProof("invalid execution resource"))?,
        serde_json::from_value(body["target_public_key"].clone())
            .map_err(|_| AuthorizationError::InvalidProof("invalid execution public key"))?,
        serde_json::from_value(body["execution_grant"].clone())
            .map_err(|_| AuthorizationError::InvalidProof("invalid execution grant"))?,
    )))
}

fn remote_resolution_intent(
    actual: &execution_handoff::ExecutionHandoffResult,
    selection: &RemoteExecutionSelection,
    authorization_id: &str,
    outcome: &str,
    evidence: &str,
    project_key: &PublicKey,
    now_ms: u64,
) -> Result<(Value, PrepareContext)> {
    use execution_handoff::{ExecutionHandoffState, ExecutionResolutionPurpose};
    execution_handoff::verify_execution_handoff_receipt_signature(
        &actual.handoff,
        &actual.receipt,
        &actual.result,
    )?;
    let wire = &actual.handoff;
    let source =
        wire.handoff
            .source_attestation
            .as_ref()
            .ok_or(AuthorizationError::InvalidProof(
                "resolution has no Project attestation",
            ))?;
    source.verify_signature()?;
    require(
        source.issuer == *project_key
            && source.payload == wire.handoff.source_statement()?
            && wire.handoff.target_public_key == selection.target_public_key
            && wire.handoff.execution_resource == selection.execution_resource
            && actual.receipt.payload.issued_at_ms <= now_ms
            && !authorization_id.is_empty()
            && authorization_id.len() <= 512
            && matches!(outcome, "succeeded" | "failed" | "rejected")
            && evidence.trim().len() >= 12
            && evidence.len() <= 65_536,
        "resolution intent differs from the signed Project or executor",
    )?;
    let purpose = match actual.receipt.payload.state {
        ExecutionHandoffState::Unknown => ExecutionResolutionPurpose::ResolveUnknown,
        ExecutionHandoffState::Succeeded
        | ExecutionHandoffState::Failed
        | ExecutionHandoffState::Rejected => {
            require(
                serde_json::to_value(actual.receipt.payload.state)
                    .map_err(|_| AuthorizationError::InvalidProof("invalid observed state"))?
                    == outcome,
                "terminal reconciliation cannot change the executor outcome",
            )?;
            ExecutionResolutionPurpose::ReconcileTerminal
        }
        _ => {
            return Err(AuthorizationError::InvalidProof(
                "executor result is still in progress",
            ));
        }
    };
    let event: events::Event = serde_json::from_str(&wire.handoff.project_dispatch.canonical_body)
        .map_err(|_| AuthorizationError::InvalidProof("invalid original dispatch"))?;
    let events::EventCause::Invocation { admission } = &event.cause else {
        return Err(AuthorizationError::InvalidProof(
            "resolution has no original admission",
        ));
    };
    let request = &admission.source_body["request"];
    let binding: task_binding::SignedBinding =
        serde_json::from_value(request["binding"].clone())
            .map_err(|_| AuthorizationError::InvalidProof("invalid original resolution binding"))?;
    task_binding::verify_binding(&binding, project_key.as_str())
        .map_err(|_| AuthorizationError::InvalidProof("resolution binding signature mismatch"))?;
    require(
        authorization_id != admission.invocation.payload.operation_id,
        "resolution requires a new authorization ID",
    )?;
    let body = serde_json::json!({
        "method":"resolve_unknown", "space_id":event.network_id, "resource_id":event.resource_id,
        "authorization_id":authorization_id, "operation_id":admission.invocation.payload.operation_id,
        "conversation_id":event.scope_id, "task_id":request["task_id"], "task_ref":binding.binding.task_ref,
        "target_node":binding.binding.target_node, "session_id":binding.binding.session_id,
        "outcome":outcome, "evidence":evidence, "execution_resource":selection.execution_resource,
        "target_public_key":selection.target_public_key, "execution_grant":selection.execution_grant,
        "original_handoff_hash":wire.hash()?, "original_body_hash":admission.invocation.payload.body_hash,
        "unknown_receipt_hash":actual.receipt.hash()?, "resolution_purpose":purpose,
        "execution_observation":actual
    });
    let context = InvocationContext {
        conversation: Some(binding.binding.conversation_id.clone()),
        task: Some(EventRef::parse(&binding.binding.task_ref)?),
        sdk_session: None,
        execution_session: Some(SessionRef {
            target: DeviceId::from_key(&selection.target_public_key),
            session_id: binding.binding.session_id.clone(),
        }),
        executor_kind: binding.binding.open_spec["executor"]
            .as_str()
            .map(str::to_owned),
        workspace_resource: Some(event.resource_id),
    };
    Ok((
        body,
        PrepareContext {
            context,
            binding_generation: Some(binding.binding.generation),
            canonical_cwd: binding.binding.open_spec["cwd"].as_str().map(str::to_owned),
            execution_target_public_key: Some(selection.target_public_key.clone()),
        },
    ))
}

fn check_raw_resolution(intent: &PrepareIntent, body: &Value, key: &PublicKey) -> Result<bool> {
    if body["method"] != "resolve_unknown" || body.get("execution_resource").is_none() {
        return Ok(false);
    }
    let i = &intent.request;
    require(
        i.action == Action::OperationResolve
            && matches!(i.actor, Actor::Account(_))
            && i.context.sdk_session.is_none(),
        "remote resolution requires an Account intent",
    )?;
    let (resource, public_key, grant) = remote_selection(body)?.ok_or(
        AuthorizationError::InvalidProof("resolution selection is missing"),
    )?;
    let selection = RemoteExecutionSelection {
        execution_resource: resource,
        target_public_key: public_key,
        execution_grant: grant,
    };
    let actual: execution_handoff::ExecutionHandoffResult =
        serde_json::from_value(body["execution_observation"].clone())
            .map_err(|_| AuthorizationError::InvalidProof("resolution observation is missing"))?;
    let (expected, context) = remote_resolution_intent(
        &actual,
        &selection,
        &i.operation_id,
        body["outcome"].as_str().unwrap_or_default(),
        body["evidence"].as_str().unwrap_or_default(),
        key,
        i.not_before_ms,
    )?;
    require(
        *body == expected
            && intent.request.context == context.context
            && intent.binding_generation == context.binding_generation
            && intent.canonical_cwd == context.canonical_cwd
            && intent.execution_target_public_key == context.execution_target_public_key,
        "resolution prepare differs from its signed observation",
    )?;
    Ok(true)
}

#[derive(Clone, Debug)]
pub struct VerifiedPrepareAuthorization {
    resource: ResourceRef,
    raw_body: Value,
    action: Action,
    operation_id: String,
    audience: Audience,
    presenter: DeviceId,
    destination: Option<VerifiedExecutionDestination>,
}
impl VerifiedPrepareAuthorization {
    pub fn execution_destination(&self) -> Option<&VerifiedExecutionDestination> {
        self.destination.as_ref()
    }
    pub fn resource(&self) -> &ResourceRef {
        &self.resource
    }
    pub fn raw_body(&self) -> &Value {
        &self.raw_body
    }
    pub fn action(&self) -> Action {
        self.action
    }
    pub fn operation_id(&self) -> &str {
        &self.operation_id
    }
    pub fn audience(&self) -> &Audience {
        &self.audience
    }
    pub fn presenter(&self) -> &DeviceId {
        &self.presenter
    }
}

fn text(value: Option<&Value>) -> Result<Option<&str>> {
    value
        .map(|value| {
            value.as_str().ok_or(AuthorizationError::InvalidProof(
                "raw context value is not text",
            ))
        })
        .transpose()
}
fn match_text(
    expected: Option<&str>,
    values: &[Option<&str>],
    reason: &'static str,
) -> Result<bool> {
    for value in values.iter().flatten() {
        require(expected == Some(*value), reason)?;
    }
    Ok(values.iter().any(Option::is_some))
}
fn raw_resource(body: &Value) -> Result<ResourceRef> {
    require(body.is_object(), "raw request must be an object")?;
    Ok(ResourceRef {
        space: SpaceId::parse(
            text(body.get("space_id"))?
                .ok_or(AuthorizationError::InvalidProof("raw request has no space"))?,
        )?,
        resource: ResourceId::parse(text(body.get("resource_id"))?.ok_or(
            AuthorizationError::InvalidProof("raw request has no resource"),
        )?)?,
    })
}
fn execution_key<'a>(
    intent: &'a PrepareIntent,
    audience_key: &'a PublicKey,
) -> Result<&'a PublicKey> {
    let Some(session) = &intent.request.context.execution_session else {
        require(
            intent.execution_target_public_key.is_none(),
            "execution key has no selected session",
        )?;
        return Ok(audience_key);
    };
    if session.target == intent.request.audience.device {
        require(
            intent
                .execution_target_public_key
                .as_ref()
                .is_none_or(|key| key == audience_key),
            "execution key differs from audience pin",
        )?;
        return Ok(audience_key);
    }
    let key =
        intent
            .execution_target_public_key
            .as_ref()
            .ok_or(AuthorizationError::InvalidProof(
                "independently verified execution key is required",
            ))?;
    require(
        DeviceId::from_key(key) == session.target,
        "execution key differs from selected device",
    )?;
    Ok(key)
}
fn project_metadata_context(
    project: &ResourceRef,
    audience: &Audience,
    key: &PublicKey,
    source: &Signed<execution_handoff::ProjectExecutionSource>,
    binding: &task_binding::SignedBinding,
) -> Result<PrepareContext> {
    source.verify_signature()?;
    task_binding::verify_binding(binding, key.as_str())
        .map_err(|_| AuthorizationError::InvalidProof("invalid metadata task binding"))?;
    let b = &binding.binding;
    let statement = &source.payload;
    require(
        source.issuer == *key
            && statement.version == AUTH_VERSION
            && statement.project == *project
            && b.network_id == project.space.as_str()
            && b.authority_node
                == hex::encode(Sha256::digest(crate::decode32(key.as_str()).map_err(
                    |_| AuthorizationError::InvalidProof("invalid Project host key"),
                )?))
            && statement.binding_hash
                == task_binding::binding_hash(b).map_err(|_| {
                    AuthorizationError::InvalidProof("invalid metadata binding hash")
                })?
            && statement.binding_generation == b.generation
            && statement.task.as_str() == b.task_ref
            && statement.execution_session.session_id == b.session_id
            && statement.execution_session.target != audience.device,
        "metadata proof differs from its Project host or bound task",
    )?;
    Ok(PrepareContext {
        context: InvocationContext {
            conversation: Some(b.conversation_id.clone()),
            task: Some(statement.task.clone()),
            workspace_resource: Some(project.resource.clone()),
            ..Default::default()
        },
        binding_generation: Some(b.generation),
        canonical_cwd: None,
        execution_target_public_key: None,
    })
}

impl VerifiedPolicySet {
    pub fn verify_execution_resolution_request_facts(
        &self,
        body: &Value,
        audience: &Audience,
        now_ms: u64,
    ) -> Result<RequestFacts> {
        let (resource, key, grant) = remote_selection(body)?.ok_or(
            AuthorizationError::InvalidProof("resolution destination is missing"),
        )?;
        require(
            body["method"] == "resolve_unknown",
            "invalid remote resolution purpose",
        )?;
        let actual: execution_handoff::ExecutionHandoffResult =
            serde_json::from_value(body["execution_observation"].clone()).map_err(|_| {
                AuthorizationError::InvalidProof("resolution observation is missing")
            })?;
        self.verify_project_execution_source(&actual.handoff.handoff, now_ms)?;
        let project_key = &self.find_device(&audience.device)?.payload.public_key;
        let (expected, context) = remote_resolution_intent(
            &actual,
            &RemoteExecutionSelection {
                execution_resource: resource,
                target_public_key: key,
                execution_grant: grant,
            },
            body["authorization_id"].as_str().unwrap_or_default(),
            body["outcome"].as_str().unwrap_or_default(),
            body["evidence"].as_str().unwrap_or_default(),
            project_key,
            now_ms,
        )?;
        let project = raw_resource(body)?;
        require(
            expected == *body
                && actual
                    .handoff
                    .handoff
                    .source_attestation
                    .as_ref()
                    .is_some_and(|source| source.payload.project == project)
                && actual.handoff.invocation.payload.presenter == audience.device,
            "remote resolution differs from the original Project observation",
        )?;
        Ok(RequestFacts {
            resource: project,
            action: Action::OperationResolve,
            audience: audience.clone(),
            operation_id: body["authorization_id"].as_str().unwrap_or_default().into(),
            body_hash: ObjectHash::body(body)?,
            context: context.context,
        })
    }

    pub fn verify_execution_dispatch_metadata(
        &self,
        project: &ResourceRef,
        audience: &Audience,
        source: &Signed<execution_handoff::ProjectExecutionSource>,
        binding: &task_binding::SignedBinding,
        now_ms: u64,
    ) -> Result<PrepareContext> {
        let record = self.resource(&project.space, &project.resource).ok_or(
            AuthorizationError::InvalidProof("metadata Project is missing"),
        )?;
        let host = self.verify_service_device(&project.space, &audience.device, now_ms)?;
        require(
            record.payload.kind == ResourceKind::Project
                && record.payload.host == audience.service
                && host.payload.subject == Actor::Service(audience.service.clone()),
            "metadata targets another Project host",
        )?;
        project_metadata_context(project, audience, &host.payload.public_key, source, binding)
    }
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = verifyProjectExecutionMetadata))]
pub fn verify_project_execution_metadata(
    auth: &VerifiedClientAuthorization,
    source_json: &str,
    binding_json: &str,
) -> crate::Result<String> {
    (|| -> Result<String> {
        require(
            source_json.len() <= MAX_PROOF_BYTES && binding_json.len() <= MAX_PROOF_BYTES,
            "metadata evidence is too large",
        )?;
        let source: Signed<execution_handoff::ProjectExecutionSource> =
            serde_json::from_str(source_json)
                .map_err(|_| AuthorizationError::InvalidProof("invalid Project metadata source"))?;
        let binding: task_binding::SignedBinding = serde_json::from_str(binding_json)
            .map_err(|_| AuthorizationError::InvalidProof("invalid Project metadata binding"))?;
        require(
            source.payload.project.space == auth.scope.space_id
                && scope_covers(&auth.grant_scope, &source.payload.project)
                && (auth.actions.contains(&Action::OperationStatusRead)
                    || auth.actions.contains(&Action::OperationReconcile)),
            "client has no Project metadata read permission",
        )?;
        let context = project_metadata_context(
            &source.payload.project,
            &auth.audience,
            &auth.audience_public_key,
            &source,
            &binding,
        )?;
        serde_json::to_string(&context)
            .map_err(|_| AuthorizationError::InvalidProof("metadata context encoding failed"))
    })()
    .map_err(|error| error.to_string())
}

fn check_raw_metadata(intent: &PrepareIntent, body: &Value, key: &PublicKey) -> Result<bool> {
    let Some(source) = body.get("execution_source") else {
        return Ok(false);
    };
    let i = &intent.request;
    require(
        matches!(
            body["method"].as_str(),
            Some("operation" | "session" | "reconcile")
        ) && matches!(
            i.action,
            Action::OperationStatusRead | Action::OperationReconcile
        ) && matches!(i.actor, Actor::Account(_))
            && body.get("request").is_none()
            && [
                "execution_resource",
                "target_public_key",
                "execution_grant",
                "sdk_session",
                "parent_invocation",
            ]
            .iter()
            .all(|field| body.get(*field).is_none()),
        "invalid Project metadata read purpose",
    )?;
    let source: Signed<execution_handoff::ProjectExecutionSource> =
        serde_json::from_value(source.clone())
            .map_err(|_| AuthorizationError::InvalidProof("invalid metadata source"))?;
    let binding: task_binding::SignedBinding = serde_json::from_value(body["binding"].clone())
        .map_err(|_| AuthorizationError::InvalidProof("invalid metadata binding"))?;
    let expected = project_metadata_context(&i.resource, &i.audience, key, &source, &binding)?;
    require(
        intent.request.context == expected.context
            && intent.binding_generation == expected.binding_generation
            && intent.canonical_cwd.is_none()
            && intent.execution_target_public_key.is_none()
            && text(body.get("operation_id"))? == Some(source.payload.operation_id.as_str()),
        "metadata request differs from its durable Project source",
    )?;
    for (field, value) in [
        ("conversation_id", binding.binding.conversation_id.as_str()),
        ("task_id", binding.binding.task_id.as_str()),
        ("task_ref", binding.binding.task_ref.as_str()),
        ("target_node", binding.binding.target_node.as_str()),
        ("session_id", binding.binding.session_id.as_str()),
    ] {
        if let Some(actual) = text(body.get(field))? {
            require(
                actual == value,
                "metadata request changed its original operation context",
            )?;
        }
    }
    if body["method"] == "session" {
        require(
            body.get("target_node").is_some() && body.get("session_id").is_some(),
            "session metadata needs its original target and session",
        )?;
    }
    Ok(true)
}

fn raw_binding(value: Option<&Value>) -> Result<Option<task_binding::SignedBinding>> {
    value
        .filter(|value| !value.is_null())
        .map(|value| {
            serde_json::from_value(value.clone())
                .map_err(|_| AuthorizationError::InvalidProof("invalid raw task binding"))
        })
        .transpose()
}

fn check_raw_rebind(intent:&PrepareIntent,body:&Value,audience_key:&PublicKey)->Result<bool> {
    if body["method"]!="rebind_execution" {return Ok(false);}
    if remote_selection(body)?.is_none() {return Ok(false);}
    let i=&intent.request;
    let binding=raw_binding(body.get("binding"))?.ok_or(AuthorizationError::InvalidProof("rebind has no old binding"))?;
    let seal:task_binding::SignedSeal=serde_json::from_value(body["previous_seal"].clone())
        .map_err(|_|AuthorizationError::InvalidProof("invalid previous executor seal"))?;
    let old_key:PublicKey=serde_json::from_value(body["previous_target_public_key"].clone())
        .map_err(|_|AuthorizationError::InvalidProof("previous executor pin is missing"))?;
    task_binding::verify_binding(&binding,audience_key.as_str())
        .map_err(|_|AuthorizationError::InvalidProof("old binding has another Project authority"))?;
    task_binding::verify_seal(&seal,old_key.as_str())
        .map_err(|_|AuthorizationError::InvalidProof("old binding has no matching executor seal"))?;
    let (_,new_key,_)=remote_selection(body)?.ok_or(AuthorizationError::InvalidProof("rebind needs its independent new execution destination"))?;
    let generation=binding.binding.generation.checked_add(1)
        .ok_or(AuthorizationError::InvalidProof("binding generation overflow"))?;
    require(i.action==Action::ExecutionRebind && matches!(i.actor,Actor::Account(_))
        && i.context.sdk_session.is_none()
        && binding.binding.network_id==i.resource.space.as_str()
        && node_fingerprint(old_key.as_str()).map_err(|_|AuthorizationError::InvalidProof("invalid previous executor pin"))?==binding.binding.target_node
        && seal.seal.network_id==binding.binding.network_id
        && seal.seal.binding_hash==task_binding::binding_hash(&binding.binding).map_err(|_|AuthorizationError::InvalidProof("invalid old binding"))?
        && seal.seal.target_node==binding.binding.target_node
        && body["task_ref"].as_str()==Some(binding.binding.task_ref.as_str())
        && body["task_id"].as_str()==Some(binding.binding.task_id.as_str())
        && body["conversation_id"].as_str()==Some(binding.binding.conversation_id.as_str())
        && i.context.task.as_ref().map(EventRef::as_str)==Some(binding.binding.task_ref.as_str())
        && i.context.conversation.as_deref()==Some(binding.binding.conversation_id.as_str())
        && i.context.execution_session.as_ref().is_some_and(|session|
            session.target==DeviceId::from_key(&new_key) && body["session_id"].as_str()==Some(session.session_id.as_str()))
        && body["target_node"].as_str()==Some(node_fingerprint(new_key.as_str()).map_err(|_|AuthorizationError::InvalidProof("invalid new executor pin"))?.as_str())
        && i.context.executor_kind.as_deref()==body["executor"].as_str()
        && intent.binding_generation==Some(generation)
        && intent.execution_target_public_key.as_ref()==Some(&new_key)
        && intent.canonical_cwd.is_some()
        && intent.canonical_cwd.as_deref()==body["cwd"].as_str(),
        "rebind changed its sealed source or independently selected new target")?;
    Ok(true)
}

fn check_raw_context(intent: &PrepareIntent, body: &Value, audience_key: &PublicKey) -> Result<()> {
    if execution_binding_seal::check_client_seal_intent(intent, body, audience_key)? {
        return Ok(());
    }
    if check_raw_rebind(intent,body,audience_key)? {return Ok(());}
    if check_raw_resolution(intent, body, audience_key)? {
        return Ok(());
    }
    if check_raw_metadata(intent, body, audience_key)? {
        return Ok(());
    }
    let i = &intent.request;
    let context = &i.context;
    validate_context(context)?;
    require(
        !matches!(i.actor, Actor::Account(_)) || context.sdk_session.is_none(),
        "account prepare cannot claim SDK adapter session",
    )?;
    require(
        context
            .workspace_resource
            .as_ref()
            .is_none_or(|resource| resource == &i.resource.resource),
        "raw workspace belongs to another resource",
    )?;
    let request = body.get("request");
    if let Some(request) = request {
        require(
            request.is_object(),
            "raw operation request must be an object",
        )?;
        if let Some(space) = text(request.get("space_id"))? {
            require(
                space == i.resource.space.as_str(),
                "raw operation belongs to another space",
            )?;
        }
    }
    let top_binding = raw_binding(body.get("binding"))?;
    let nested_binding = raw_binding(request.and_then(|request| request.get("binding")))?;
    if let (Some(top), Some(nested)) = (&top_binding, &nested_binding) {
        require(top == nested, "raw request contains conflicting bindings")?;
    }
    let binding = top_binding.or(nested_binding);
    if let Some(binding) = &binding {
        // These fields constrain preparation; final admission authenticates the binding.
        task_binding::validate_binding(&binding.binding)
            .map_err(|_| AuthorizationError::InvalidProof("invalid raw task binding"))?;
        require(
            binding.binding.network_id == i.resource.space.as_str(),
            "raw binding belongs to another space",
        )?;
    }
    let task_ids = [
        text(body.get("task_id"))?,
        text(request.and_then(|request| request.get("task_id")))?,
        binding
            .as_ref()
            .map(|binding| binding.binding.task_id.as_str()),
    ];
    match_text(
        task_ids.iter().flatten().next().copied(),
        &task_ids,
        "raw request contains conflicting task IDs",
    )?;
    let conversation = [
        text(body.get("conversation_id"))?,
        text(request.and_then(|request| request.get("conversation_id")))?,
        binding
            .as_ref()
            .map(|binding| binding.binding.conversation_id.as_str()),
    ];
    let found = match_text(
        context.conversation.as_deref(),
        &conversation,
        "raw conversation differs from prepare context",
    )?;
    require(
        context.conversation.is_none() || found,
        "prepare conversation has no raw evidence",
    )?;

    let task = [
        text(body.get("task_ref"))?,
        binding
            .as_ref()
            .map(|binding| binding.binding.task_ref.as_str()),
    ];
    let expected_task = context.task.as_ref().map(|reference| reference.as_str());
    let found = match_text(
        expected_task,
        &task,
        "raw task reference differs from prepare context",
    )?;
    require(
        context.task.is_none() || found,
        "prepare task has no raw creation reference",
    )?;

    let raw_generation = body
        .get("binding_generation")
        .map(|value| {
            value.as_u64().ok_or(AuthorizationError::InvalidProof(
                "invalid raw binding generation",
            ))
        })
        .transpose()?;
    let generations = [
        raw_generation,
        binding.as_ref().map(|binding| binding.binding.generation),
    ];
    for generation in generations.iter().flatten() {
        require(
            *generation > 0 && intent.binding_generation == Some(*generation),
            "raw binding generation differs from prepare context",
        )?;
    }
    require(
        intent.binding_generation.is_none() || generations.iter().any(Option::is_some),
        "prepare generation has no raw binding evidence",
    )?;

    let sessions = [
        text(body.get("session_id"))?,
        text(request.and_then(|request| request.get("session_id")))?,
        binding
            .as_ref()
            .map(|binding| binding.binding.session_id.as_str()),
    ];
    let found = match_text(
        context
            .execution_session
            .as_ref()
            .map(|session| session.session_id.as_str()),
        &sessions,
        "raw execution session differs from prepare context",
    )?;
    if context.execution_session.is_some() {
        require(found, "prepare execution session has no raw session")?;
    }
    let remote = remote_selection(body)?;
    if let Some((_, key, _)) = &remote {
        require(
            intent.execution_target_public_key.as_ref() == Some(key)
                && context.execution_session.as_ref().is_some_and(|session| {
                    session.target == DeviceId::from_key(key) && session.target != i.audience.device
                }),
            "remote execution target differs from the explicit client pin",
        )?;
    }
    if body["method"].as_str() == Some("operate") && remote.is_none() {
        require(
            context
                .execution_session
                .as_ref()
                .is_some_and(|session| session.target == i.audience.device),
            "operation execution target differs from audience",
        )?;
    }
    let raw_target = hex::encode(Sha256::digest(
        crate::decode32(execution_key(intent, audience_key)?.as_str())
            .map_err(|_| AuthorizationError::InvalidProof("invalid audience key"))?,
    ));
    match_text(
        Some(&raw_target),
        &[
            text(body.get("target_node"))?,
            text(request.and_then(|request| request.get("target_node")))?,
            binding
                .as_ref()
                .map(|binding| binding.binding.target_node.as_str()),
        ],
        "raw execution target differs from audience",
    )?;

    let action = request.and_then(|request| request.get("action"));
    match_text(
        context.executor_kind.as_deref(),
        &[
            text(body.get("executor"))?,
            text(action.and_then(|action| action.get("executor")))?,
            binding
                .as_ref()
                .and_then(|binding| binding.binding.open_spec.get("executor"))
                .and_then(Value::as_str),
        ],
        "raw executor differs from prepare context",
    )?;
    if let Some(cwd) = &intent.canonical_cwd {
        require(
            !cwd.is_empty() && cwd.len() <= 4096,
            "invalid prepare working directory",
        )?;
    }
    match_text(
        intent.canonical_cwd.as_deref(),
        &[
            text(body.get("cwd"))?,
            text(action.and_then(|action| action.get("cwd")))?,
            binding
                .as_ref()
                .and_then(|binding| binding.binding.open_spec.get("cwd"))
                .and_then(Value::as_str),
        ],
        "raw working directory differs from prepare context",
    )?;
    Ok(())
}

/// The body and context come from user intent; the Gateway key comes from a pinned Noise route.
#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = signClientPrepareIntent))]
pub fn sign_client_prepare_intent(
    auth: &VerifiedClientAuthorization,
    raw_body_json: &str,
    raw_context_json: &str,
    expected_action: &str,
    operation_id: &str,
    expected_gateway_public_key: &str,
    identity: &Identity,
    now_ms: f64,
) -> crate::Result<String> {
    (|| -> Result<String> {
        require(
            raw_body_json.len() <= MAX_PROOF_BYTES
                && raw_context_json.len() <= MAX_PROOF_BYTES
                && now_ms.is_finite()
                && now_ms >= 0.0
                && now_ms.fract() == 0.0
                && now_ms <= 9_007_199_254_740_991.0,
            "invalid client prepare input",
        )?;
        let body: Value = serde_json::from_str(raw_body_json)
            .map_err(|_| AuthorizationError::InvalidProof("invalid raw request JSON"))?;
        let raw: PrepareContext = serde_json::from_str(raw_context_json)
            .map_err(|_| AuthorizationError::InvalidProof("invalid raw context JSON"))?;
        let action: Action = serde_json::from_value(Value::String(expected_action.to_owned()))
            .map_err(|_| AuthorizationError::InvalidProof("unknown expected prepare action"))?;
        let gateway = PublicKey::parse(expected_gateway_public_key)?;
        require(
            gateway != PublicKey::of(identity),
            "gateway cannot be client key",
        )?;
        let resource = raw_resource(&body)?;
        require(
            auth.scope.device_id == DeviceId::from_key(&PublicKey::of(identity))
                && auth.scope.space_id == resource.space
                && scope_covers(&auth.grant_scope, &resource)
                && auth.actions.contains(&action)
                && valid_operation_id(operation_id)
                && text(body.get("authorization_id"))? == Some(operation_id)
                && context_tighter(&auth.constraints.context, &raw.context),
            "prepare intent differs from verified client grant",
        )?;
        if auth.constraints.require_webauthn_uv {
            return Err(AuthorizationError::UserPresenceRequired);
        }
        let actor = Actor::Account(auth.scope.account_id.clone());
        let now = now_ms as u64;
        let intent = PrepareIntent {
            request: Invocation {
                version: AUTH_VERSION,
                actor: actor.clone(),
                device: auth.scope.device_id.clone(),
                initiator: actor,
                initiating_grant: None,
                resource,
                action,
                audience: auth.audience.clone(),
                presenter: if gateway == auth.audience_public_key {
                    auth.scope.device_id.clone()
                } else {
                    DeviceId::from_key(&gateway)
                },
                operation_id: operation_id.to_owned(),
                body_hash: ObjectHash::body(&body)?,
                grant_leaf: auth.grant_leaf.clone(),
                expected_policies: auth.heads.clone(),
                context: raw.context,
                nonce: Nonce::random()?,
                not_before_ms: now,
                expires_at_ms: now + INVOCATION_MAX_AGE_MS,
            },
            binding_generation: raw.binding_generation,
            canonical_cwd: raw.canonical_cwd,
            execution_target_public_key: raw.execution_target_public_key,
        };
        check_raw_context(&intent, &body, &auth.audience_public_key)?;
        serde_json::to_string(&Signed::sign(intent, identity)?)
            .map_err(|_| AuthorizationError::InvalidProof("prepare intent encoding failed"))
    })()
    .map_err(|error| error.to_string())
}

impl VerifiedPolicySet {
    /// expected_action is parsed from the raw method; local_audience comes from the local service pin.
    pub fn verify_prepare_intent(
        &self,
        signed: &Signed<PrepareIntent>,
        raw_body: &Value,
        expected_action: Action,
        local_audience: &Audience,
        transport: &TransportEvidence,
        now_ms: u64,
    ) -> Result<VerifiedPrepareAuthorization> {
        require(
            canonical(raw_body)?.len() <= MAX_PROOF_BYTES,
            "raw prepare body too large",
        )?;
        let mut budget = Budget::default();
        let hash = budget.verify(signed)?;
        let intent = &signed.payload;
        let i = &intent.request;
        require(
            i.resource == raw_resource(raw_body)?
                && i.action == expected_action
                && i.audience == *local_audience
                && text(raw_body.get("authorization_id"))? == Some(i.operation_id.as_str())
                && i.body_hash == ObjectHash::body(raw_body)?,
            "prepare does not match raw request",
        )?;
        let audience_key = &self.find_device(&i.audience.device)?.payload.public_key;
        check_raw_context(intent, raw_body, audience_key)?;
        if let Some(session) = &i.context.execution_session {
            let certificate = self.find_device(&session.target)?;
            require(
                certificate.payload.public_key == *execution_key(intent, audience_key)?,
                "execution key differs from policy certificate",
            )?;
        }
        let destination = remote_selection(raw_body)?
            .map(|(resource, key, grant)| {
                self.verify_execution_destination(
                    &i.resource,
                    local_audience,
                    &resource,
                    &key,
                    &grant,
                    &i.context,
                    i.action,
                    now_ms,
                )
            })
            .transpose()?;
        let facts = RequestFacts {
            resource: i.resource.clone(),
            action: expected_action,
            audience: local_audience.clone(),
            operation_id: i.operation_id.clone(),
            body_hash: ObjectHash::body(raw_body)?,
            context: i.context.clone(),
        };
        // The verified prepare signature is never exposed as an executable Invocation signature.
        let request = Signed {
            payload: i.clone(),
            issuer: signed.issuer.clone(),
            signature: signed.signature.clone(),
        };
        self.verify_signed_invocation_body(
            &request,
            hash,
            budget,
            &facts,
            transport,
            now_ms,
            AuthorizationPhase::Admission,
        )?;
        Ok(VerifiedPrepareAuthorization {
            resource: i.resource.clone(),
            raw_body: raw_body.clone(),
            action: i.action,
            operation_id: i.operation_id.clone(),
            audience: i.audience.clone(),
            presenter: i.presenter.clone(),
            destination,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::authorization::tests::{Fixture, NOW};
    use serde_json::json;

    fn client_material(
        f: &Fixture,
        leaf: Option<ObjectHash>,
    ) -> (
        VerifiedClientAuthorization,
        Identity,
        ClientAuthorizationBundle,
        Vec<PolicyPin>,
    ) {
        let user = Identity::from_secret_hex(&hex::encode([4; 32])).unwrap();
        let space = SpaceId::personal(&f.account);
        let leaf = leaf.unwrap_or_else(|| {
            f.state
                .grants
                .iter()
                .find(|(_, grant)| {
                    grant.payload.grantee == Grantee::Actor(Actor::Account(f.account.clone()))
                        && grant.payload.actions.contains(&Action::MessageAppend)
                })
                .unwrap()
                .0
                .clone()
        });
        let ah = f.state.account_history(&f.account).unwrap();
        let sh = f.state.space_history(&space).unwrap();
        let mut journal = Vec::new();
        for (index, document) in ah.iter().enumerate() {
            journal.push(if index == 0 {
                JournalEntry::AccountGenesis(document.clone())
            } else {
                JournalEntry::Account(document.clone())
            });
            let stamp = account_stamp(document).unwrap();
            journal.extend(
                f.state
                    .devices
                    .values()
                    .filter(|certificate| certificate.payload.issued_under == stamp)
                    .cloned()
                    .map(JournalEntry::Device),
            );
        }
        for (index, document) in sh.iter().enumerate() {
            journal.push(if index == 0 {
                JournalEntry::SpaceGenesis(document.clone())
            } else {
                JournalEntry::Space(document.clone())
            });
            let stamp = space_stamp(document).unwrap();
            journal.extend(
                f.state
                    .resources
                    .values()
                    .filter(|record| record.payload.issued_under == stamp)
                    .cloned()
                    .map(JournalEntry::Resource),
            );
            journal.extend(
                f.state
                    .grants
                    .values()
                    .filter(|grant| grant.payload.issued_under == stamp)
                    .cloned()
                    .map(JournalEntry::Grant),
            );
        }
        let bundle = ClientAuthorizationBundle {
            journal,
            grant_leaf: leaf,
        };
        let pins = vec![
            PolicyPin {
                domain: PolicyDomain::Account(f.account.clone()),
                genesis_root: PublicKey::of(&f.root),
                minimum_head: f
                    .state
                    .head(&PolicyDomain::Account(f.account.clone()))
                    .cloned(),
            },
            PolicyPin {
                domain: PolicyDomain::Space(space.clone()),
                genesis_root: PublicKey::of(&f.root),
                minimum_head: f.state.head(&PolicyDomain::Space(space.clone())).cloned(),
            },
        ];
        let auth = verify_client_authorization(
            &serde_json::to_string(&bundle).unwrap(),
            &serde_json::to_string(&pins).unwrap(),
            f.account.as_str(),
            space.as_str(),
            PublicKey::of(&user).as_str(),
            NOW as f64,
        )
        .unwrap();
        (auth, user, bundle, pins)
    }
    fn client(f: &Fixture, leaf: Option<ObjectHash>) -> (VerifiedClientAuthorization, Identity) {
        let (auth, user, _, _) = client_material(f, leaf);
        (auth, user)
    }
    fn body(auth: &VerifiedClientAuthorization) -> Value {
        let resource = &auth.resources[0].payload;
        json!({"space_id":resource.space,"resource_id":resource.resource,
            "authorization_id":"prepare_1","method":"message","conversation_id":"chat","text":"hello"})
    }
    fn context(auth: &VerifiedClientAuthorization) -> PrepareContext {
        PrepareContext {
            context: InvocationContext {
                conversation: Some("chat".into()),
                workspace_resource: Some(auth.resources[0].payload.resource.clone()),
                ..Default::default()
            },
            ..Default::default()
        }
    }
    fn sign(
        auth: &VerifiedClientAuthorization,
        body: &Value,
        context: &PrepareContext,
        user: &Identity,
        action: &str,
    ) -> crate::Result<Signed<PrepareIntent>> {
        let json = sign_client_prepare_intent(
            auth,
            &body.to_string(),
            &serde_json::to_string(context).unwrap(),
            action,
            "prepare_1",
            auth.audience_public_key.as_str(),
            user,
            NOW as f64,
        )?;
        Ok(serde_json::from_str(&json).unwrap())
    }
    fn check(
        f: &Fixture,
        signed: &Signed<PrepareIntent>,
        body: &Value,
        action: Action,
        now: u64,
    ) -> Result<VerifiedPrepareAuthorization> {
        f.state.verify_prepare_intent(
            signed,
            body,
            action,
            &signed.payload.request.audience,
            &TransportEvidence::DirectNoise {
                device: signed.payload.request.device.clone(),
            },
            now,
        )
    }

    #[test]
    fn remote_prepare_requires_an_exact_current_execution_grant() {
        let mut f = Fixture::new();
        let executor = Identity::from_secret_hex(&hex::encode([87; 32])).unwrap();
        let execution_key = PublicKey::of(&executor);
        let space = SpaceId::personal(&f.account);
        let audience = Audience {
            service: ServiceId::from_key(&PublicKey::of(&f.node)),
            device: DeviceId::from_key(&PublicKey::of(&f.node)),
        };
        let mut certificate = DeviceCertificate {
            version: AUTH_VERSION,
            owner: PolicyDomain::Account(f.account.clone()),
            subject: Actor::Service(ServiceId::from_key(&execution_key)),
            device: DeviceId::from_key(&execution_key),
            public_key: execution_key.clone(),
            uses: vec![DeviceUse::Noise, DeviceUse::ServiceInvocation],
            possession: None,
            enrollment_grant: None,
            issued_under: f
                .state
                .head(&PolicyDomain::Account(f.account.clone()))
                .unwrap()
                .clone(),
            nonce: Nonce::random().unwrap(),
            not_before_ms: 0,
            expires_at_ms: 1_000_000,
        };
        certificate.prove_possession(&executor).unwrap();
        let hash = f
            .state
            .install_device(Signed::sign(certificate, &f.root).unwrap(), NOW)
            .unwrap();
        let mut account = f.state.account(&f.account).unwrap().clone();
        account.previous = f
            .state
            .head(&PolicyDomain::Account(f.account.clone()))
            .cloned();
        account.sequence += 1;
        account.devices.push(hash);
        account.devices.sort();
        f.state
            .install_account(Signed::sign(account, &f.root).unwrap())
            .unwrap();
        let execution = ResourceRef {
            space: space.clone(),
            resource: ResourceId::from_bytes([88; 32]),
        };
        let stamp = f
            .state
            .head(&PolicyDomain::Space(space.clone()))
            .unwrap()
            .clone();
        let resource_hash = f
            .state
            .install_resource(
                Signed::sign(
                    ResourceRecord {
                        version: AUTH_VERSION,
                        space: space.clone(),
                        resource: execution.resource.clone(),
                        kind: ResourceKind::ExecutionService,
                        host: ServiceId::from_key(&execution_key),
                        issued_under: stamp.clone(),
                        nonce: Nonce::random().unwrap(),
                    },
                    &f.root,
                )
                .unwrap(),
            )
            .unwrap();
        let project = ResourceRef {
            space: space.clone(),
            resource: ResourceId::from_bytes([10; 32]),
        };
        let context = InvocationContext {
            conversation: Some("chat".into()),
            task: Some(EventRef::parse(&hex::encode([89; 32])).unwrap()),
            execution_session: Some(SessionRef {
                target: DeviceId::from_key(&execution_key),
                session_id: "remote-session".into(),
            }),
            executor_kind: Some("codex".into()),
            workspace_resource: Some(project.resource.clone()),
            sdk_session: None,
        };
        let mut execution_context = context.clone();
        execution_context.workspace_resource = Some(execution.resource.clone());
        let grant = Signed::sign(
            Grant {
                version: AUTH_VERSION,
                owner_space: space.clone(),
                scope: ResourceScope::Exact(execution.clone()),
                grantee: Grantee::Actor(Actor::Service(audience.service.clone())),
                actions: vec![Action::ExecutionSend, Action::ExecutionSubmit],
                audience: Audience {
                    service: ServiceId::from_key(&execution_key),
                    device: DeviceId::from_key(&execution_key),
                },
                constraints: GrantConstraints {
                    context: execution_context,
                    ..Default::default()
                },
                parent: None,
                remaining_delegations: 0,
                issued_under: stamp.clone(),
                not_before_ms: 0,
                expires_at_ms: 1_000_000,
                nonce: Nonce::random().unwrap(),
            },
            &f.root,
        )
        .unwrap();
        let leaf = f.state.install_grant(grant.clone(), NOW).unwrap();
        let mut document = f.state.space(&space).unwrap().clone();
        document.previous = Some(stamp);
        document.sequence += 1;
        document.resources.push(resource_hash);
        document.resources.sort();
        document.grants.push(leaf.clone());
        document.grants.sort();
        f.state
            .install_space(Signed::sign(document, &f.root).unwrap())
            .unwrap();
        let (auth, user, bundle, pins) = client_material(&f, None);
        let body = json!({"method":"operate","space_id":space,"resource_id":project.resource,
            "authorization_id":"prepare_1","conversation_id":"chat","task_ref":context.task,
            "execution_resource":execution,"target_public_key":execution_key,"execution_grant":leaf,
            "request":{"conversation_id":"chat","session_id":"remote-session",
                "target_node":hex::encode(Sha256::digest(crate::decode32(execution_key.as_str()).unwrap())),
                "action":{"type":"send","text":"one task"}}});
        let raw = PrepareContext {
            context: context.clone(),
            execution_target_public_key: Some(execution_key.clone()),
            ..Default::default()
        };
        let mut execution_bundle = bundle.clone();
        execution_bundle.grant_leaf = leaf.clone();
        let selection = json!({"execution_resource":execution,"target_public_key":execution_key,"execution_grant":leaf});
        let snapshot = verify_directional_grant_snapshot(
            &serde_json::to_string(&execution_bundle).unwrap(),
            &serde_json::to_string(&pins).unwrap(),
            NOW as f64,
        )
        .unwrap();
        let share: Value = serde_json::from_str(&snapshot.share_json()).unwrap();
        assert_eq!(share["kind"], "vibeface-remote-execution");
        assert_eq!(share["selection"], selection);
        assert!(share.get("pins").is_none());
        let handle = verify_remote_execution(
            &auth,
            &serde_json::to_string(&bundle).unwrap(),
            &serde_json::to_string(&pins).unwrap(),
            &serde_json::to_string(&execution_bundle).unwrap(),
            &serde_json::to_string(&pins).unwrap(),
            &selection.to_string(),
            &serde_json::to_string(&raw).unwrap(),
            "execution_send",
            NOW as f64,
        )
        .unwrap();
        let signed: Signed<PrepareIntent> = serde_json::from_str(
            &handle
                .prepare(
                    &auth,
                    &body.to_string(),
                    &serde_json::to_string(&raw).unwrap(),
                    "execution_send",
                    "prepare_1",
                    auth.audience_public_key().as_str(),
                    &user,
                    NOW as f64,
                )
                .unwrap(),
        )
        .unwrap();
        let final_wire = handle
            .sign(
                &auth,
                &serde_json::to_string(&signed.payload.request).unwrap(),
                &body.to_string(),
                "execution_send",
                "prepare_1",
                auth.audience_public_key().as_str(),
                &user,
                NOW as f64,
            )
            .unwrap();
        let final_invocation: Signed<Invocation> = serde_json::from_str(&final_wire).unwrap();
        let metadata: Value = serde_json::from_str(&handle.metadata_json().unwrap()).unwrap();
        assert_eq!(metadata["context"], serde_json::to_value(&raw).unwrap());
        assert_eq!(
            metadata["audience"]["device"],
            serde_json::to_value(DeviceId::from_key(&execution_key)).unwrap()
        );
        handle
            .validate_replay(
                &auth,
                &body.to_string(),
                &final_wire,
                "execution_send",
                "prepare_1",
                auth.audience_public_key().as_str(),
                NOW as f64,
            )
            .unwrap();
        let mut corrupt = final_invocation.clone();
        corrupt.payload.nonce = Nonce::random().unwrap();
        assert!(
            handle
                .validate_replay(
                    &auth,
                    &body.to_string(),
                    &serde_json::to_string(&corrupt).unwrap(),
                    "execution_send",
                    "prepare_1",
                    auth.audience_public_key().as_str(),
                    NOW as f64
                )
                .is_err()
        );
        assert!(
            f.state
                .verify_invocation(
                    &final_invocation,
                    &RequestFacts {
                        resource: project.clone(),
                        action: Action::ExecutionSend,
                        audience: audience.clone(),
                        operation_id: "prepare_1".into(),
                        body_hash: ObjectHash::body(&body).unwrap(),
                        context: context.clone()
                    },
                    &TransportEvidence::DirectNoise {
                        device: DeviceId::from_key(&PublicKey::of(&user))
                    },
                    NOW
                )
                .is_ok()
        );
        let mut changed_body = body.clone();
        changed_body["target_public_key"] = json!(PublicKey::of(&user));
        assert!(
            handle
                .prepare(
                    &auth,
                    &changed_body.to_string(),
                    &serde_json::to_string(&raw).unwrap(),
                    "execution_send",
                    "prepare_1",
                    auth.audience_public_key().as_str(),
                    &user,
                    NOW as f64
                )
                .is_err()
        );
        let verified = check(&f, &signed, &body, Action::ExecutionSend, NOW).unwrap();
        assert_eq!(
            verified.execution_destination().unwrap().grant_leaf(),
            &leaf
        );
        let mut ordinary = body.clone();
        for field in ["execution_resource", "target_public_key", "execution_grant"] {
            ordinary.as_object_mut().unwrap().remove(field);
        }
        assert!(sign(&auth, &ordinary, &raw, &user, "execution_send").is_err());
        let mut changed = context.clone();
        changed.task = Some(EventRef::parse(&hex::encode([90; 32])).unwrap());
        assert!(
            f.state
                .verify_execution_destination(
                    &project,
                    &audience,
                    &execution,
                    &execution_key,
                    &leaf,
                    &changed,
                    Action::ExecutionSend,
                    NOW
                )
                .is_err()
        );
        assert!(
            f.state
                .verify_execution_destination(
                    &project,
                    &audience,
                    &execution,
                    &PublicKey::of(&user),
                    &leaf,
                    &context,
                    Action::ExecutionSend,
                    NOW
                )
                .is_err()
        );
        let mut wide = grant.payload.clone();
        wide.scope = ResourceScope::WholeSpace(space.clone());
        let wide = f
            .state
            .install_grant(Signed::sign(wide, &f.root).unwrap(), NOW)
            .unwrap();
        assert!(
            f.state
                .verify_execution_destination(
                    &project,
                    &audience,
                    &execution,
                    &execution_key,
                    &wide,
                    &context,
                    Action::ExecutionSend,
                    NOW
                )
                .is_err()
        );
        let mut document = f.state.space(&space).unwrap().clone();
        document.previous = f.state.head(&PolicyDomain::Space(space.clone())).cloned();
        document.sequence += 1;
        document.grants.retain(|hash| hash != &leaf);
        f.state
            .install_space(Signed::sign(document, &f.root).unwrap())
            .unwrap();
        assert!(
            f.state
                .verify_execution_destination(
                    &project,
                    &audience,
                    &execution,
                    &execution_key,
                    &leaf,
                    &context,
                    Action::ExecutionSend,
                    NOW
                )
                .is_err()
        );
    }

    #[test]
    fn prepares_raw_intent_without_producing_executable_authorization() {
        let f = Fixture::new();
        let (auth, user) = client(&f, None);
        let body = body(&auth);
        let signed = sign(&auth, &body, &context(&auth), &user, "message_append").unwrap();
        let verified = check(&f, &signed, &body, Action::MessageAppend, NOW).unwrap();
        assert_eq!(verified.resource(), &signed.payload.request.resource);
        assert_eq!(verified.raw_body(), &body);
        assert_eq!(verified.action(), Action::MessageAppend);
        assert_eq!(verified.operation_id(), "prepare_1");
        assert_eq!(verified.audience(), auth.audience());
        assert_eq!(verified.presenter(), &auth.scope.device_id);
        assert!(
            serde_json::from_str::<Signed<Invocation>>(&serde_json::to_string(&signed).unwrap())
                .is_err()
        );
        let request = signed.payload.request.clone();
        let facts = RequestFacts {
            resource: request.resource.clone(),
            action: request.action,
            audience: request.audience.clone(),
            operation_id: request.operation_id.clone(),
            body_hash: request.body_hash.clone(),
            context: request.context.clone(),
        };
        let counterfeit = Signed {
            payload: request,
            issuer: signed.issuer.clone(),
            signature: signed.signature.clone(),
        };
        assert_eq!(
            f.state
                .verify_invocation(
                    &counterfeit,
                    &facts,
                    &TransportEvidence::DirectNoise {
                        device: auth.scope.device_id.clone()
                    },
                    NOW
                )
                .unwrap_err(),
            AuthorizationError::InvalidProof("invalid object signature")
        );
        let again = sign(&auth, &body, &context(&auth), &user, "message_append").unwrap();
        assert_ne!(signed.payload.request.nonce, again.payload.request.nonce);
    }

    #[test]
    fn verifies_actor_grant_transport_heads_expiry_and_original_body() {
        let f = Fixture::new();
        let (auth, user) = client(&f, None);
        let body = body(&auth);
        let signed = sign(&auth, &body, &context(&auth), &user, "message_append").unwrap();
        let mut changed_body = body.clone();
        changed_body["text"] = json!("changed during normalization");
        assert!(check(&f, &signed, &changed_body, Action::MessageAppend, NOW).is_err());
        assert_eq!(
            check(
                &f,
                &signed,
                &body,
                Action::MessageAppend,
                NOW + INVOCATION_MAX_AGE_MS
            )
            .unwrap_err(),
            AuthorizationError::Expired
        );
        assert!(check(&f, &signed, &body, Action::TaskAccept, NOW).is_err());
        let wrong_issuer = Signed::sign(signed.payload.clone(), &f.node).unwrap();
        assert!(check(&f, &wrong_issuer, &body, Action::MessageAppend, NOW).is_err());
        let transport = TransportEvidence::DirectNoise {
            device: DeviceId::from_key(&PublicKey::of(&f.node)),
        };
        assert!(
            f.state
                .verify_prepare_intent(
                    &signed,
                    &body,
                    Action::MessageAppend,
                    auth.audience(),
                    &transport,
                    NOW
                )
                .is_err()
        );
        let mut stale = signed.payload.clone();
        stale.request.expected_policies[0].head =
            ObjectHash::parse(&format!("h2_{}", "f".repeat(64))).unwrap();
        assert!(matches!(
            check(
                &f,
                &Signed::sign(stale, &user).unwrap(),
                &body,
                Action::MessageAppend,
                NOW
            ),
            Err(AuthorizationError::NeedPolicySync(_))
        ));
        let mut not_granted = signed.payload.clone();
        not_granted.request.action = Action::TaskCreate;
        let mut create_body = body.clone();
        create_body["method"] = json!("create_task");
        not_granted.request.body_hash = ObjectHash::body(&create_body).unwrap();
        assert!(
            check(
                &f,
                &Signed::sign(not_granted, &user).unwrap(),
                &create_body,
                Action::TaskCreate,
                NOW
            )
            .is_err()
        );
        let mut other_resource = signed.payload.clone();
        other_resource.request.resource.resource = ResourceId::from_bytes([90; 32]);
        other_resource.request.context.workspace_resource =
            Some(other_resource.request.resource.resource.clone());
        changed_body = body.clone();
        changed_body["resource_id"] = json!(other_resource.request.resource.resource);
        other_resource.request.body_hash = ObjectHash::body(&changed_body).unwrap();
        assert!(
            check(
                &f,
                &Signed::sign(other_resource, &user).unwrap(),
                &changed_body,
                Action::MessageAppend,
                NOW
            )
            .is_err()
        );
    }

    #[test]
    fn raw_context_must_match_client_intent_before_any_normalization() {
        let f = Fixture::new();
        let (auth, user) = client(&f, None);
        let body = body(&auth);
        let signed = sign(&auth, &body, &context(&auth), &user, "message_append").unwrap();
        let mut wrong = signed.payload.clone();
        wrong.request.context.conversation = Some("another-chat".into());
        assert!(
            check(
                &f,
                &Signed::sign(wrong, &user).unwrap(),
                &body,
                Action::MessageAppend,
                NOW
            )
            .is_err()
        );
        let mut wrong = context(&auth);
        wrong.context.sdk_session = Some(SessionRef {
            target: auth.audience.device.clone(),
            session_id: "adapter".into(),
        });
        assert!(sign(&auth, &body, &wrong, &user, "message_append").is_err());
        let mut wrong = context(&auth);
        wrong.context.task = Some(EventRef::parse(&"a".repeat(64)).unwrap());
        assert!(sign(&auth, &body, &wrong, &user, "message_append").is_err());
        let mut cwd_body = body.clone();
        cwd_body["cwd"] = json!("/selected");
        let mut cwd = context(&auth);
        cwd.canonical_cwd = Some("/selected".into());
        let proof = sign(&auth, &cwd_body, &cwd, &user, "message_append").unwrap();
        assert!(check(&f, &proof, &cwd_body, Action::MessageAppend, NOW).is_ok());
        cwd.canonical_cwd = Some("/expanded".into());
        assert!(sign(&auth, &cwd_body, &cwd, &user, "message_append").is_err());

        let mut execution = body.clone();
        execution["method"] = json!("operate");
        execution["request"] = json!({"conversation_id":"chat","session_id":"session-1","target_node":f.node.node_id(),"action":{"type":"send"}});
        let mut selected = context(&auth);
        selected.context.execution_session = Some(SessionRef {
            target: auth.audience.device.clone(),
            session_id: "session-1".into(),
        });
        let proof = sign(&auth, &execution, &selected, &user, "execution_send").unwrap();
        assert!(check(&f, &proof, &execution, Action::ExecutionSend, NOW).is_ok());
        let binding = task_binding::TaskBinding {
            network_id: auth.scope.space_id.to_string(),
            conversation_id: "chat".into(),
            task_ref: "a".repeat(64),
            task_id: "task".into(),
            authority_node: f.node.node_id(),
            generation: 1,
            target_node: f.node.node_id(),
            session_id: "session-1".into(),
            open_spec: json!({"type":"open","executor":"codex","cwd":"/selected"}),
            previous_binding_hash: None,
            previous_seal: None,
        };
        let binding = task_binding::SignedBinding {
            signature: f
                .node
                .sign(
                    task_binding::BINDING_DOMAIN,
                    &serde_json::to_vec(&binding).unwrap(),
                )
                .unwrap(),
            binding,
        };
        execution["binding"] = serde_json::to_value(&binding).unwrap();
        execution["executor"] = json!("codex");
        selected.context.task = Some(EventRef::parse(&"a".repeat(64)).unwrap());
        selected.context.executor_kind = Some("codex".into());
        selected.binding_generation = Some(1);
        selected.canonical_cwd = Some("/selected".into());
        let proof = sign(&auth, &execution, &selected, &user, "execution_send").unwrap();
        assert!(check(&f, &proof, &execution, Action::ExecutionSend, NOW).is_ok());
        let mut wrong_task = execution.clone();
        wrong_task["request"]["task_id"] = json!("other-task");
        assert!(sign(&auth, &wrong_task, &selected, &user, "execution_send").is_err());
        let mut forged = proof.payload.clone();
        forged.request.body_hash = ObjectHash::body(&wrong_task).unwrap();
        assert_eq!(
            check(
                &f,
                &Signed::sign(forged, &user).unwrap(),
                &wrong_task,
                Action::ExecutionSend,
                NOW
            )
            .unwrap_err(),
            AuthorizationError::InvalidProof("raw request contains conflicting task IDs")
        );
        let mut conflict = binding;
        conflict.binding.task_id = "other-task".into();
        execution["request"]["binding"] = serde_json::to_value(conflict).unwrap();
        assert!(sign(&auth, &execution, &selected, &user, "execution_send").is_err());
        execution["request"]
            .as_object_mut()
            .unwrap()
            .remove("binding");
        selected.execution_target_public_key = Some(PublicKey::of(&user));
        assert!(sign(&auth, &execution, &selected, &user, "execution_send").is_err());
        selected.execution_target_public_key = None;
        selected
            .context
            .execution_session
            .as_mut()
            .unwrap()
            .session_id = "other-session".into();
        assert!(sign(&auth, &execution, &selected, &user, "execution_send").is_err());
    }

    #[test]
    fn task_limited_grant_requires_matching_raw_creation_reference() {
        let mut f = Fixture::new();
        let (original, _) = client(&f, None);
        let task = EventRef::parse(&"a".repeat(64)).unwrap();
        let mut grant = f.state.grants[&original.grant_leaf].payload.clone();
        grant.constraints.context.task = Some(task.clone());
        grant.issued_under = f
            .state
            .head(&PolicyDomain::Space(original.scope.space_id.clone()))
            .unwrap()
            .clone();
        grant.nonce = Nonce::from_bytes([91; 32]);
        let leaf = f
            .state
            .install_grant(Signed::sign(grant, &f.root).unwrap(), NOW)
            .unwrap();
        let mut space = f.state.space(&original.scope.space_id).unwrap().clone();
        space.previous = f
            .state
            .head(&PolicyDomain::Space(space.space.clone()))
            .cloned();
        space.sequence += 1;
        space.grants.push(leaf.clone());
        space.grants.sort();
        f.state
            .install_space(Signed::sign(space, &f.root).unwrap())
            .unwrap();
        let (auth, user) = client(&f, Some(leaf));
        let mut raw = body(&auth);
        let mut selected = context(&auth);
        selected.context.task = Some(task);
        assert!(sign(&auth, &raw, &selected, &user, "message_append").is_err());
        raw["task_ref"] = json!("a".repeat(64));
        let proof = sign(&auth, &raw, &selected, &user, "message_append").unwrap();
        assert!(check(&f, &proof, &raw, Action::MessageAppend, NOW).is_ok());
        raw["task_ref"] = json!("b".repeat(64));
        assert!(sign(&auth, &raw, &selected, &user, "message_append").is_err());
    }
}
