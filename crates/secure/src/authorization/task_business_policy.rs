//! A source policy stream bound to one immutable Task business wire.

pub mod receipt;

use super::{
    events::{Event, EventCause},
    execution_handoff::{
        ExecutionProofUsage, ExecutionResolutionPurpose, SignedExecutionHandoff,
        SignedExecutionHandoffResolution, VerifiedExecutionHandoff, VerifiedExecutionResolution,
    },
    execution_observation::{SignedExecutionObservation, VerifiedExecutionObservationCurrent},
    node_execution_policy::{ActualPolicyNoisePeer, VerifiedTaskCurrentPolicy},
    policy_import::{BoundedPolicyImport, VerifiedPolicySnapshot},
    *,
};
use crate::signature_verification::{ScopedSignatureVerification, SignatureVerification};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(
    tag = "type",
    content = "wire",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum TaskBusinessWire {
    Submit(SignedExecutionHandoff),
    Observe(SignedExecutionObservation),
    Resolve(SignedExecutionHandoffResolution),
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TaskBusinessPurpose {
    Submit,
    Observe,
    ResolveUnknown,
    ReconcileTerminal,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct TaskBusinessScope {
    pub purpose: TaskBusinessPurpose,
    pub business_wire_hash: ObjectHash,
    pub source_project: ResourceRef,
    pub source_node_public_key: PublicKey,
    pub task_ref: EventRef,
    pub execution_session: SessionRef,
    pub binding_generation: u64,
    pub execution_resource: ResourceRef,
    pub execution_grant: ObjectHash,
    pub target_public_key: PublicKey,
}

impl TaskBusinessWire {
    pub fn invocation(&self) -> &Signed<Invocation> {
        match self {
            Self::Submit(value) => &value.invocation,
            Self::Observe(value) => &value.invocation,
            Self::Resolve(value) => &value.invocation,
        }
    }

    pub fn hash(&self) -> Result<ObjectHash> {
        match self {
            Self::Submit(value) => value.hash(),
            Self::Observe(value) => value.hash(),
            Self::Resolve(value) => value.hash(),
        }
    }

    pub fn scope(&self) -> Result<TaskBusinessScope> {
        let (source, resource, target, purpose) = match self {
            Self::Submit(value) => (
                value.handoff.source_statement()?,
                value.handoff.execution_resource.clone(),
                value.handoff.target_public_key.clone(),
                TaskBusinessPurpose::Submit,
            ),
            Self::Observe(value) => {
                let target = &value.request.payload.intent.target;
                (
                    target.source.clone(),
                    target.execution_resource.clone(),
                    target.target_public_key.clone(),
                    TaskBusinessPurpose::Observe,
                )
            }
            Self::Resolve(value) => {
                let original = &value.resolution.observation.handoff.handoff;
                (
                    original.source_statement()?,
                    original.execution_resource.clone(),
                    original.target_public_key.clone(),
                    match value.resolution.purpose {
                        ExecutionResolutionPurpose::ResolveUnknown => {
                            TaskBusinessPurpose::ResolveUnknown
                        }
                        ExecutionResolutionPurpose::ReconcileTerminal => {
                            TaskBusinessPurpose::ReconcileTerminal
                        }
                    },
                )
            }
        };
        let invocation = self.invocation();
        let expected_action = if purpose == TaskBusinessPurpose::Submit {
            Action::ExecutionSubmit
        } else {
            Action::ExecutionResolve
        };
        require(
            source.binding_generation > 0
                && source.execution_session.target == DeviceId::from_key(&target)
                && invocation.payload.version == AUTH_VERSION
                && invocation.payload.action == expected_action
                && invocation.payload.resource == resource
                && invocation.payload.actor
                    == Actor::Service(ServiceId::from_key(&invocation.issuer))
                && invocation.payload.device == DeviceId::from_key(&invocation.issuer)
                && invocation.payload.presenter == DeviceId::from_key(&invocation.issuer)
                && invocation.payload.audience.service == ServiceId::from_key(&target)
                && invocation.payload.audience.device == DeviceId::from_key(&target)
                && invocation.payload.context.task.as_ref() == Some(&source.task)
                && invocation.payload.context.execution_session.as_ref()
                    == Some(&source.execution_session),
            "Task business wire changed its source, target or Task binding",
        )?;
        Ok(TaskBusinessScope {
            purpose,
            business_wire_hash: self.hash()?,
            source_project: source.project,
            source_node_public_key: invocation.issuer.clone(),
            task_ref: source.task,
            execution_session: source.execution_session,
            binding_generation: source.binding_generation,
            execution_resource: resource,
            execution_grant: invocation.payload.grant_leaf.clone(),
            target_public_key: target,
        })
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct TaskBusinessPolicyHeader {
    pub version: u16,
    pub scope: TaskBusinessScope,
    pub source_heads: Vec<PolicyStamp>,
    pub stream_id: Nonce,
    pub issued_at_ms: u64,
    pub expires_at_ms: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct TaskBusinessPolicyPage {
    pub version: u16,
    pub header_hash: ObjectHash,
    pub page_index: u64,
    pub request_cursor: Option<ObjectHash>,
    pub previous_page_hash: ObjectHash,
    pub entry: Option<JournalEntry>,
    pub next_cursor: Option<ObjectHash>,
    pub complete: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct TaskBusinessPolicyWitness {
    pub version: u16,
    pub header_hash: ObjectHash,
    pub final_page_hash: ObjectHash,
    pub page_count: u64,
    pub entry_count: u64,
    pub source_heads: Vec<PolicyStamp>,
    pub issued_at_ms: u64,
    pub expires_at_ms: u64,
}

macro_rules! task_business_object {
    ($name:ident, $domain:literal) => {
        impl sealed::Sealed for $name {}
        impl SignedObject for $name {
            const DOMAIN: &'static str = $domain;
        }
    };
}
task_business_object!(TaskBusinessPolicyHeader, "task-business-policy-header-v2");
task_business_object!(TaskBusinessPolicyPage, "task-business-policy-page-v2");
task_business_object!(TaskBusinessPolicyWitness, "task-business-policy-witness-v2");

fn carrier<T: Serialize>(value: &T) -> Result<()> {
    require(
        canonical(value)?.len() <= MAX_PROOF_BYTES,
        "Task business policy carrier is too large",
    )
}

fn window(issued: u64, expires: u64, now: u64) -> Result<()> {
    validate_window(issued, expires, now)?;
    require(
        expires
            .checked_sub(issued)
            .is_some_and(|age| age <= INVOCATION_MAX_AGE_MS),
        "Task business policy source exceeds its time limit",
    )
}

fn finish_usage(usage: &mut ExecutionProofUsage) -> Result<()> {
    usage.enforce_limits()?;
    if let Some(counts) = SignatureVerification::current_counts() {
        require(
            counts.actual_verifications <= MAX_PROOF_SIGNATURES,
            "Task business signature budget exceeded",
        )?;
        usage.record_actual_signature_counts(counts);
    }
    Ok(())
}

fn source_grant<'a>(
    primary: &'a VerifiedPolicySet,
    scope: &TaskBusinessScope,
    local_c: &PublicKey,
    now: u64,
    usage: &mut ExecutionProofUsage,
) -> Result<(&'a ExecutionSourcePolicy, u64)> {
    require(
        &scope.target_public_key == local_c
            && scope.execution_session.target == DeviceId::from_key(local_c)
            && scope.binding_generation > 0,
        "Task business policy targets another executor",
    )?;
    let grant = primary
        .grant(&scope.execution_grant)
        .ok_or(AuthorizationError::InvalidProof(
            "missing current Task business grant",
        ))?;
    let mut budget = Budget::default();
    let mut dependencies = BTreeMap::new();
    let path = primary.grant_chain(Some(&scope.execution_grant))?;
    primary.verify_grant_path(&path, now, &mut budget, &mut dependencies, true)?;
    let value = &grant.payload;
    let expected_action = if scope.purpose == TaskBusinessPurpose::Submit {
        Action::ExecutionSubmit
    } else {
        Action::ExecutionResolve
    };
    require(
        value.scope == ResourceScope::Exact(scope.execution_resource.clone())
            && value.grantee
                == Grantee::Actor(Actor::Service(ServiceId::from_key(
                    &scope.source_node_public_key,
                )))
            && value.audience.service == ServiceId::from_key(local_c)
            && value.audience.device == DeviceId::from_key(local_c)
            && value.actions.contains(&expected_action)
            && value.constraints.context.task.as_ref() == Some(&scope.task_ref)
            && value.constraints.context.execution_session.as_ref()
                == Some(&scope.execution_session)
            && value.remaining_delegations == 0
            && value.expires_at_ms < u64::MAX,
        "Task business source requires the exact current executor grant",
    )?;
    let source = value
        .execution_source
        .as_ref()
        .ok_or(AuthorizationError::InvalidProof(
            "executor grant has no approved source policy",
        ))?;
    require(
        source.project == scope.source_project
            && source.node_public_key == scope.source_node_public_key
            && !source.pins.is_empty()
            && source.pins.len() <= MAX_POLICY_DOMAINS
            && source
                .pins
                .windows(2)
                .all(|pair| pair[0].domain < pair[1].domain)
            && source.pins.iter().all(|pin| {
                pin.minimum_head
                    .as_ref()
                    .is_none_or(|head| head.domain == pin.domain)
            }),
        "Task business source changed its grant-approved project, key or policy roots",
    )?;
    usage.record_budget(&budget)?;
    usage.record_chain_depth(path.len());
    Ok((source, budget.expires_at_ms))
}

#[derive(Clone)]
pub(super) struct TaskBusinessLocalAuthority {
    state: VerifiedPolicySet,
    heads: Vec<PolicyStamp>,
    pins: Vec<PolicyPin>,
}

impl TaskBusinessLocalAuthority {
    pub(super) fn state(&self) -> &VerifiedPolicySet {
        &self.state
    }
    pub(super) fn heads(&self) -> &[PolicyStamp] {
        &self.heads
    }
    pub(super) fn pins(&self) -> &[PolicyPin] {
        &self.pins
    }
}

fn local_authority(
    primary: &VerifiedPolicySet,
    scope: &TaskBusinessScope,
) -> Result<TaskBusinessLocalAuthority> {
    let mut independent = Vec::new();
    for (account, state) in &primary.accounts {
        independent.push(PolicyPin {
            domain: PolicyDomain::Account(account.clone()),
            genesis_root: state
                .history
                .first()
                .ok_or(AuthorizationError::PolicyConflict)?
                .payload
                .genesis_root
                .clone(),
            minimum_head: Some(state.stamp.clone()),
        });
    }
    for (space, state) in &primary.spaces {
        let genesis_root = match &state
            .history
            .first()
            .ok_or(AuthorizationError::PolicyConflict)?
            .payload
            .root
        {
            SpaceRoot::Personal { genesis_root, .. } | SpaceRoot::Team { genesis_root } => {
                genesis_root.clone()
            }
        };
        independent.push(PolicyPin {
            domain: PolicyDomain::Space(space.clone()),
            genesis_root,
            minimum_head: Some(state.stamp.clone()),
        });
    }
    independent.sort_by(|left, right| left.domain.cmp(&right.domain));
    let domain = PolicyDomain::Space(scope.execution_resource.space.clone());
    let head = primary
        .head(&domain)
        .ok_or(AuthorizationError::PolicyConflict)?
        .clone();
    let (heads, _) = remote_execution_policy::reference_closure(primary, &[head], &independent)?;
    let pins = independent
        .into_iter()
        .filter(|pin| heads.iter().any(|head| head.domain == pin.domain))
        .collect();
    Ok(TaskBusinessLocalAuthority {
        state: primary.clone(),
        heads,
        pins,
    })
}

pub fn task_business_primary_heads(
    primary: &VerifiedPolicySet,
    scope: &TaskBusinessScope,
    local_c: &PublicKey,
    now: u64,
) -> Result<(Vec<PolicyStamp>, ExecutionProofUsage)> {
    let _signatures = ScopedSignatureVerification::new();
    let mut usage = ExecutionProofUsage::default();
    source_grant(primary, scope, local_c, now, &mut usage)?;
    let authority = local_authority(primary, scope)?;
    finish_usage(&mut usage)?;
    Ok((authority.heads, usage))
}

fn effective_pins(
    approved: &[PolicyPin],
    saved: &[PolicyPin],
) -> Result<(Vec<PolicyPin>, Vec<PolicyStamp>)> {
    require(
        saved.len() <= MAX_POLICY_DOMAINS
            && saved.windows(2).all(|pair| pair[0].domain < pair[1].domain),
        "invalid Task business saved policy floors",
    )?;
    let mut pins = approved.to_vec();
    let mut required = Vec::new();
    for old in saved {
        let pin = pins.iter_mut().find(|pin| pin.domain == old.domain).ok_or(
            AuthorizationError::InvalidProof("Task business saved floor has an unapproved domain"),
        )?;
        require(
            pin.genesis_root == old.genesis_root,
            "Task business saved floor changed its genesis",
        )?;
        if let Some(minimum) = &old.minimum_head {
            require(
                minimum.domain == old.domain,
                "Task business saved floor changed its domain",
            )?;
            if let Some(original) = &pin.minimum_head {
                required.push(original.clone());
                require(
                    (minimum.authority_epoch, minimum.sequence)
                        != (original.authority_epoch, original.sequence)
                        || minimum == original,
                    "Task business saved floor conflicts at the same policy position",
                )?;
            }
            let replace = pin.minimum_head.as_ref().is_none_or(|old| {
                (minimum.authority_epoch, minimum.sequence) >= (old.authority_epoch, old.sequence)
            });
            if replace {
                pin.minimum_head = Some(minimum.clone());
            } else {
                required.push(minimum.clone());
            }
        }
    }
    required.sort_by(|left, right| left.domain.cmp(&right.domain));
    // BoundedPolicyImport adds effective minima itself. Preserve older minima at finish.
    Ok((pins, required))
}

#[derive(Clone)]
pub struct TaskBusinessPolicyImport {
    header: Signed<TaskBusinessPolicyHeader>,
    local_authority: TaskBusinessLocalAuthority,
    import: BoundedPolicyImport,
    original_minima: Vec<PolicyStamp>,
    previous_page_hash: ObjectHash,
    next_cursor: Option<ObjectHash>,
    page_count: u64,
    entry_count: u64,
    complete: bool,
    last_page: Option<Signed<TaskBusinessPolicyPage>>,
    page_envelope_hashes: Vec<ObjectHash>,
    usage: ExecutionProofUsage,
}

pub fn begin_task_business_policy_import(
    primary: &VerifiedPolicySet,
    actual_peer: &ActualPolicyNoisePeer,
    header: &Signed<TaskBusinessPolicyHeader>,
    local_c: &PublicKey,
    saved_floors: &[PolicyPin],
    now: u64,
) -> Result<TaskBusinessPolicyImport> {
    let _signatures = ScopedSignatureVerification::new();
    carrier(header)?;
    window(
        header.payload.issued_at_ms,
        header.payload.expires_at_ms,
        now,
    )?;
    require(
        header.payload.version == AUTH_VERSION
            && &header.issuer == actual_peer.public_key()
            && header.issuer == header.payload.scope.source_node_public_key
            && !header.payload.source_heads.is_empty()
            && header.payload.source_heads.len() <= MAX_POLICY_DOMAINS
            && header
                .payload
                .source_heads
                .windows(2)
                .all(|pair| pair[0].domain < pair[1].domain),
        "Task business source header changed its actual Noise source or exact heads",
    )?;
    let mut usage = ExecutionProofUsage::default();
    let (source, deadline) =
        source_grant(primary, &header.payload.scope, local_c, now, &mut usage)?;
    require(
        header.payload.expires_at_ms <= deadline
            && header
                .payload
                .source_heads
                .iter()
                .all(|head| source.pins.iter().any(|pin| pin.domain == head.domain)),
        "Task business source introduced a domain outside its approved roots",
    )?;
    header.verify_signature()?;
    usage.record_signed(header)?;
    let (pins, original_minima) = effective_pins(&source.pins, saved_floors)?;
    let selected: Vec<_> = pins
        .into_iter()
        .filter(|pin| {
            header
                .payload
                .source_heads
                .iter()
                .any(|head| head.domain == pin.domain)
        })
        .collect();
    let required: Vec<_> = selected
        .iter()
        .filter_map(|pin| {
            source
                .pins
                .iter()
                .find(|old| old.domain == pin.domain)
                .and_then(|pin| pin.minimum_head.clone())
        })
        .collect();
    let import = BoundedPolicyImport::new(selected, required)?;
    let local_authority = local_authority(primary, &header.payload.scope)?;
    finish_usage(&mut usage)?;
    Ok(TaskBusinessPolicyImport {
        header: header.clone(),
        local_authority,
        import,
        original_minima,
        previous_page_hash: header.hash()?,
        next_cursor: None,
        page_count: 0,
        entry_count: 0,
        complete: false,
        last_page: None,
        page_envelope_hashes: Vec::new(),
        usage,
    })
}

impl TaskBusinessPolicyImport {
    pub fn header(&self) -> &Signed<TaskBusinessPolicyHeader> {
        &self.header
    }
    pub fn next_page_index(&self) -> u64 {
        self.page_count
    }
    pub fn next_cursor(&self) -> Option<&ObjectHash> {
        self.next_cursor.as_ref()
    }
    pub fn proof_usage(&self) -> &ExecutionProofUsage {
        &self.usage
    }
    pub fn primary_heads(&self) -> &[PolicyStamp] {
        self.local_authority.heads()
    }

    pub fn append_page(&mut self, page: &Signed<TaskBusinessPolicyPage>, now: u64) -> Result<()> {
        let _signatures = ScopedSignatureVerification::new();
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
                && page.payload.header_hash == self.header.hash()?,
            "Task business page changed its signed source header",
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
            "Task business policy page is missing, changed or out of order",
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
        self.page_envelope_hashes.push(ObjectHash::body(page)?);
        self.usage = usage;
        Ok(())
    }

    pub fn finish(
        self,
        witness: &Signed<TaskBusinessPolicyWitness>,
        now: u64,
    ) -> Result<VerifiedTaskBusinessSource> {
        let _signatures = ScopedSignatureVerification::new();
        carrier(witness)?;
        witness.verify_signature()?;
        window(
            self.header.payload.issued_at_ms,
            self.header.payload.expires_at_ms,
            now,
        )?;
        window(
            witness.payload.issued_at_ms,
            witness.payload.expires_at_ms,
            now,
        )?;
        require(
            self.complete
                && witness.issuer == self.header.issuer
                && witness.payload.version == AUTH_VERSION
                && witness.payload.header_hash == self.header.hash()?
                && witness.payload.final_page_hash == self.previous_page_hash
                && witness.payload.page_count == self.page_count
                && witness.payload.entry_count == self.entry_count
                && witness.payload.source_heads == self.header.payload.source_heads
                && witness.payload.issued_at_ms >= self.header.payload.issued_at_ms
                && witness.payload.expires_at_ms <= self.header.payload.expires_at_ms,
            "Task business source witness changed its completed stream",
        )?;
        let snapshot = self.import.finish(&self.header.payload.source_heads)?;
        require(
            self.original_minima
                .iter()
                .all(|head| snapshot.state().roots.contains_key(head)),
            "Task business source omitted an earlier saved floor",
        )?;
        let project = &self.header.payload.scope.source_project;
        let initial = snapshot
            .state()
            .head(&PolicyDomain::Space(project.space.clone()))
            .ok_or(AuthorizationError::PolicyConflict)?
            .clone();
        let (heads, allowed) = remote_execution_policy::reference_closure(
            snapshot.state(),
            &[initial],
            snapshot.pins(),
        )?;
        require(
            heads == self.header.payload.source_heads
                && snapshot.journal().iter().all(|entry| {
                    policy_import::entry_hash(entry).is_ok_and(|hash| allowed.contains(&hash))
                }),
            "Task business source contains objects outside its necessary closure",
        )?;
        let resource = snapshot
            .state()
            .resource(&project.space, &project.resource)
            .ok_or(AuthorizationError::InvalidProof(
                "missing Task business source Project",
            ))?;
        require(
            resource.payload.kind == ResourceKind::Project
                && resource.payload.host == ServiceId::from_key(&self.header.issuer),
            "Task business source key does not host its Project",
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
        usage.record_signed(&self.header)?;
        usage.record_signed(witness)?;
        finish_usage(&mut usage)?;
        Ok(VerifiedTaskBusinessSource {
            header: self.header,
            witness: witness.clone(),
            snapshot,
            local_authority: self.local_authority,
            page_envelope_hashes: self.page_envelope_hashes,
            usage,
        })
    }
}

#[derive(Clone)]
pub struct VerifiedTaskBusinessSource {
    header: Signed<TaskBusinessPolicyHeader>,
    witness: Signed<TaskBusinessPolicyWitness>,
    snapshot: VerifiedPolicySnapshot,
    local_authority: TaskBusinessLocalAuthority,
    page_envelope_hashes: Vec<ObjectHash>,
    usage: ExecutionProofUsage,
}

impl VerifiedTaskBusinessSource {
    pub fn header(&self) -> &Signed<TaskBusinessPolicyHeader> {
        &self.header
    }
    pub fn witness(&self) -> &Signed<TaskBusinessPolicyWitness> {
        &self.witness
    }
    pub fn scope(&self) -> &TaskBusinessScope {
        &self.header.payload.scope
    }
    pub fn wire_hash(&self) -> &ObjectHash {
        &self.scope().business_wire_hash
    }
    pub fn state(&self) -> &VerifiedPolicySet {
        self.snapshot.state()
    }
    pub fn pins(&self) -> &[PolicyPin] {
        self.snapshot.pins()
    }
    pub fn heads(&self) -> &[PolicyStamp] {
        self.snapshot.heads()
    }
    pub fn primary_heads(&self) -> &[PolicyStamp] {
        self.local_authority.heads()
    }
    pub fn proof_usage(&self) -> &ExecutionProofUsage {
        &self.usage
    }
    pub fn original_page_envelope_hashes(&self) -> &[ObjectHash] {
        &self.page_envelope_hashes
    }

    pub fn verify_wire(&self, wire: &TaskBusinessWire, now: u64) -> Result<()> {
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
        require(
            wire.scope()? == *self.scope(),
            "Task business source belongs to another immutable wire",
        )
    }

    pub fn check_saved_floors(&self, saved: &[PolicyPin]) -> Result<()> {
        effective_pins(self.pins(), saved)?;
        for pin in saved {
            if let Some(head) = &pin.minimum_head {
                require(
                    self.state().roots.contains_key(head),
                    "Task business source is behind a saved exact floor",
                )?;
            }
        }
        Ok(())
    }

    pub fn verify_current_grant(
        &self,
        primary: &VerifiedPolicySet,
        local_c: &PublicKey,
        now: u64,
    ) -> Result<()> {
        let _signatures = ScopedSignatureVerification::new();
        let mut usage = self.usage.clone();
        let (source, _) = source_grant(primary, self.scope(), local_c, now, &mut usage)?;
        let selected: Vec<_> = source
            .pins
            .iter()
            .filter(|pin| self.pins().iter().any(|saved| saved.domain == pin.domain))
            .cloned()
            .collect();
        self.snapshot.with_independent_pins(&selected)?;
        finish_usage(&mut usage)
    }

    pub fn scoped_view(
        &self,
        primary: &VerifiedPolicySet,
        local_c: &PublicKey,
        wire: &TaskBusinessWire,
        now: u64,
    ) -> Result<TaskBusinessScopedView> {
        let _signatures = ScopedSignatureVerification::new();
        self.verify_wire(wire, now)?;
        self.verify_current_grant(primary, local_c, now)?;
        let (_, mut usage) = task_business_primary_heads(primary, self.scope(), local_c, now)?;
        usage.merge(self.proof_usage());
        let authority = local_authority(primary, self.scope())?;
        let local = policy_import::native_task_business_primary_snapshot(&authority)?;
        let combined = policy_import::merge_same_head_snapshots(&local, &self.snapshot)?;
        finish_usage(&mut usage)?;
        Ok(TaskBusinessScopedView {
            source: self.clone(),
            combined,
            primary_heads: authority.heads,
            usage,
        })
    }
}

#[derive(Clone)]
pub struct TaskBusinessScopedView {
    source: VerifiedTaskBusinessSource,
    combined: VerifiedPolicySnapshot,
    primary_heads: Vec<PolicyStamp>,
    usage: ExecutionProofUsage,
}

impl TaskBusinessScopedView {
    pub fn state(&self) -> &VerifiedPolicySet {
        self.combined.state()
    }
    pub fn scope(&self) -> &TaskBusinessScope {
        self.source.scope()
    }
    pub fn primary_heads(&self) -> &[PolicyStamp] {
        &self.primary_heads
    }
    pub fn foreign_heads(&self) -> &[PolicyStamp] {
        self.source.heads()
    }
    pub fn proof_usage(&self) -> &ExecutionProofUsage {
        &self.usage
    }
    pub fn source(&self) -> &VerifiedTaskBusinessSource {
        &self.source
    }
    pub fn verify_wire(&self, wire: &TaskBusinessWire, now: u64) -> Result<()> {
        self.source.verify_wire(wire, now)
    }
}

#[derive(Clone, Copy)]
pub enum TaskBusinessAuthorization<'a> {
    Submit(&'a VerifiedExecutionHandoff),
    Observe(&'a VerifiedExecutionObservationCurrent),
    Resolve(&'a VerifiedExecutionResolution),
}

#[derive(Clone)]
pub struct TaskBusinessPolicyExport {
    header: Signed<TaskBusinessPolicyHeader>,
    source: VerifiedPolicySet,
    pins: Vec<PolicyPin>,
    usage: ExecutionProofUsage,
    authority_usage: ExecutionProofUsage,
    allowed_hashes: BTreeSet<ObjectHash>,
    sent_hashes: BTreeSet<ObjectHash>,
    previous_page_hash: ObjectHash,
    next_cursor: Option<ObjectHash>,
    page_count: u64,
    complete: bool,
}

pub fn build_task_business_policy_export(
    current: &VerifiedTaskCurrentPolicy,
    authorization: TaskBusinessAuthorization<'_>,
    wire: &TaskBusinessWire,
    identity: &Identity,
    now: u64,
) -> Result<TaskBusinessPolicyExport> {
    let _signatures = ScopedSignatureVerification::new();
    let scope = wire.scope()?;
    let (heads, deadline) = match (&authorization, wire) {
        (TaskBusinessAuthorization::Submit(verified), TaskBusinessWire::Submit(value)) => {
            require(
                verified.wire() == value,
                "Task business export changed its verified submission",
            )?;
            (verified.dependencies(), verified.expires_at_ms())
        }
        (TaskBusinessAuthorization::Observe(verified), TaskBusinessWire::Observe(value)) => {
            require(
                verified.hash() == &value.hash()?,
                "Task business export changed its verified observation",
            )?;
            (verified.heads(), verified.expires_at_ms())
        }
        (TaskBusinessAuthorization::Resolve(verified), TaskBusinessWire::Resolve(value)) => {
            require(
                verified.wire() == value,
                "Task business export changed its verified resolution",
            )?;
            (verified.dependencies(), verified.expires_at_ms())
        }
        _ => {
            return Err(AuthorizationError::InvalidProof(
                "Task business export changed its verified purpose",
            ));
        }
    };
    require(
        PublicKey::of(identity) == scope.source_node_public_key
            && now < deadline
            && current.request().source_project() == &scope.source_project
            && current.request().source_node() == &scope.source_node_public_key
            && current.selection().execution_resource == scope.execution_resource
            && current.selection().execution_grant == scope.execution_grant
            && current.selection().target_public_key == scope.target_public_key
            && current.context().context.task.as_ref() == Some(&scope.task_ref)
            && current.context().context.execution_session.as_ref()
                == Some(&scope.execution_session)
            && current.context().binding_generation == Some(scope.binding_generation)
            && heads
                .iter()
                .all(|head| current.state().head(&head.domain) == Some(head)),
        "Task business export changed its current policy, signer or Task binding",
    )?;
    check_source_policy_scope(current, wire, &scope, now)?;
    let mut usage = ExecutionProofUsage::default();
    let (approved, grant_deadline) = source_grant(
        current.state(),
        &scope,
        &scope.target_public_key,
        now,
        &mut usage,
    )?;
    let initial: Vec<_> = current
        .primary_heads()
        .iter()
        .filter(|head| approved.pins.iter().any(|pin| pin.domain == head.domain))
        .cloned()
        .collect();
    require(
        initial
            .iter()
            .any(|head| head.domain == PolicyDomain::Space(scope.source_project.space.clone())),
        "Task business export omitted its source Project domain",
    )?;
    let (source_heads, allowed_hashes) =
        remote_execution_policy::reference_closure(current.state(), &initial, &approved.pins)?;
    let pins = approved
        .pins
        .iter()
        .filter(|pin| source_heads.iter().any(|head| head.domain == pin.domain))
        .cloned()
        .collect();
    let expires = deadline
        .min(grant_deadline)
        .min(current.expires_at_ms())
        .min(
            now.checked_add(INVOCATION_MAX_AGE_MS)
                .ok_or(AuthorizationError::Expired)?,
        );
    let header = Signed::sign(
        TaskBusinessPolicyHeader {
            version: AUTH_VERSION,
            scope,
            source_heads,
            stream_id: Nonce::random()?,
            issued_at_ms: now,
            expires_at_ms: expires,
        },
        identity,
    )?;
    let authority_usage = usage.clone();
    usage.record_signed(&header)?;
    finish_usage(&mut usage)?;
    Ok(TaskBusinessPolicyExport {
        previous_page_hash: header.hash()?,
        header,
        source: current.state().clone(),
        pins,
        usage,
        authority_usage,
        allowed_hashes,
        sent_hashes: BTreeSet::new(),
        next_cursor: None,
        page_count: 0,
        complete: false,
    })
}

fn check_source_policy_scope(
    current: &VerifiedTaskCurrentPolicy,
    wire: &TaskBusinessWire,
    scope: &TaskBusinessScope,
    now: u64,
) -> Result<()> {
    require(
        current.request().source_project() == &scope.source_project
            && current.request().source_node() == &scope.source_node_public_key
            && current.selection().execution_resource == scope.execution_resource
            && current.selection().execution_grant == scope.execution_grant
            && current.selection().target_public_key == scope.target_public_key
            && current.context().context.task.as_ref() == Some(&scope.task_ref)
            && current.context().context.execution_session.as_ref()
                == Some(&scope.execution_session)
            && current.context().binding_generation == Some(scope.binding_generation),
        "Task business source policy changed its exact wire scope",
    )?;
    match wire {
        TaskBusinessWire::Submit(value) => {
            let event: Event = serde_json::from_str(&value.handoff.project_dispatch.canonical_body)
                .map_err(|_| AuthorizationError::InvalidProof("invalid source dispatch event"))?;
            let EventCause::Invocation { admission } = event.cause else {
                return Err(AuthorizationError::InvalidProof(
                    "Task business source has no original admission",
                ));
            };
            let request = &admission.invocation.payload;
            let facts = request_facts(request);
            current.check_body_scope(&admission.invocation, &facts, &admission.source_body, now)?;
        }
        TaskBusinessWire::Observe(value) => {
            let request = &value.request.payload.request;
            let context = prepare::PrepareContext {
                context: request.context.clone(),
                binding_generation: Some(scope.binding_generation),
                canonical_cwd: None,
                execution_target_public_key: Some(scope.target_public_key.clone()),
            };
            current.check_business_observation_scope(
                &request_facts(request),
                current.selection(),
                &context,
                now,
            )?;
        }
        TaskBusinessWire::Resolve(value) => {
            let admission = &value.resolution.account_admission;
            current.check_body_scope(
                &admission.invocation,
                &request_facts(&admission.invocation.payload),
                &admission.source_body,
                now,
            )?;
        }
    }
    Ok(())
}

fn request_facts(request: &Invocation) -> RequestFacts {
    RequestFacts {
        resource: request.resource.clone(),
        action: request.action,
        audience: request.audience.clone(),
        operation_id: request.operation_id.clone(),
        body_hash: request.body_hash.clone(),
        context: request.context.clone(),
    }
}

impl TaskBusinessPolicyExport {
    pub fn header(&self) -> &Signed<TaskBusinessPolicyHeader> {
        &self.header
    }
    pub fn scope(&self) -> &TaskBusinessScope {
        &self.header.payload.scope
    }
    pub fn target_public_key(&self) -> &PublicKey {
        &self.scope().target_public_key
    }
    pub fn source_heads(&self) -> &[PolicyStamp] {
        &self.header.payload.source_heads
    }
    pub fn pins(&self) -> &[PolicyPin] {
        &self.pins
    }
    pub fn proof_usage(&self) -> &ExecutionProofUsage {
        &self.usage
    }

    pub fn allows_entry(&self, entry: &JournalEntry) -> Result<bool> {
        let hash = policy_import::entry_hash(entry)?;
        if !self.allowed_hashes.contains(&hash) {
            return Ok(false);
        }
        let accepted = match entry {
            JournalEntry::AccountGenesis(value) | JournalEntry::Account(value) => self
                .source
                .account_history(&value.payload.account)
                .is_some_and(|values| values.contains(value)),
            JournalEntry::SpaceGenesis(value) | JournalEntry::Space(value) => self
                .source
                .space_history(&value.payload.space)
                .is_some_and(|values| values.contains(value)),
            JournalEntry::AccountCommit(value) => {
                self.source
                    .account_history(&value.proposal.payload.account)
                    .is_some_and(|values| values.contains(&value.proposal))
                    && self.source.commits.get(&hash) == Some(&value.commit)
            }
            JournalEntry::SpaceCommit(value) => {
                self.source
                    .space_history(&value.proposal.payload.space)
                    .is_some_and(|values| values.contains(&value.proposal))
                    && self.source.commits.get(&hash) == Some(&value.commit)
            }
            JournalEntry::Device(value) => self.source.devices.get(&hash) == Some(value),
            JournalEntry::Resource(value) => self.source.resources.get(&hash) == Some(value),
            JournalEntry::Authenticator(value) => {
                self.source.authenticators.get(&hash) == Some(value)
            }
            JournalEntry::Membership(value) => self.source.memberships.get(&hash) == Some(value),
            JournalEntry::Grant(value) => self.source.grants.get(&hash) == Some(value),
        };
        require(
            accepted,
            "Task business source entry differs from its genuine accepted state",
        )?;
        Ok(true)
    }

    pub fn append_entry_page(
        &mut self,
        identity: &Identity,
        entry: JournalEntry,
        now: u64,
    ) -> Result<Signed<TaskBusinessPolicyPage>> {
        require(
            self.allows_entry(&entry)?,
            "Task business source entry lies outside its necessary closure",
        )?;
        let hash = policy_import::entry_hash(&entry)?;
        require(
            !self.sent_hashes.contains(&hash),
            "Task business source repeated an entry",
        )?;
        let page = self.make_page(identity, Some(entry), Some(hash.clone()), now)?;
        self.sent_hashes.insert(hash);
        Ok(page)
    }

    pub fn finish_pages(
        &mut self,
        identity: &Identity,
        now: u64,
    ) -> Result<(
        Signed<TaskBusinessPolicyPage>,
        Signed<TaskBusinessPolicyWitness>,
    )> {
        require(
            self.sent_hashes == self.allowed_hashes,
            "Task business source omitted accepted closure entries",
        )?;
        let page = self.make_page(identity, None, None, now)?;
        let witness = Signed::sign(
            TaskBusinessPolicyWitness {
                version: AUTH_VERSION,
                header_hash: self.header.hash()?,
                final_page_hash: page.hash()?,
                page_count: self.page_count,
                entry_count: self.sent_hashes.len() as u64,
                source_heads: self.header.payload.source_heads.clone(),
                issued_at_ms: now,
                expires_at_ms: self.header.payload.expires_at_ms,
            },
            identity,
        )?;
        let mut usage = self.authority_usage.clone();
        usage.record_signed(&page)?;
        usage.record_signed(&witness)?;
        finish_usage(&mut usage)?;
        self.usage = usage;
        Ok((page, witness))
    }

    fn make_page(
        &mut self,
        identity: &Identity,
        entry: Option<JournalEntry>,
        next_cursor: Option<ObjectHash>,
        now: u64,
    ) -> Result<Signed<TaskBusinessPolicyPage>> {
        let _signatures = ScopedSignatureVerification::new();
        window(
            self.header.payload.issued_at_ms,
            self.header.payload.expires_at_ms,
            now,
        )?;
        require(
            !self.complete && PublicKey::of(identity) == self.header.issuer,
            "Task business exporter is complete or changed signer",
        )?;
        let mut usage = self.authority_usage.clone();
        if let Some(entry) = &entry {
            usage.merge(&policy_proof::policy_entry_payload_usage(entry)?);
        }
        let page = Signed::sign(
            TaskBusinessPolicyPage {
                version: AUTH_VERSION,
                header_hash: self.header.hash()?,
                page_index: self.page_count,
                request_cursor: self.next_cursor.clone(),
                previous_page_hash: self.previous_page_hash.clone(),
                entry,
                complete: next_cursor.is_none(),
                next_cursor: next_cursor.clone(),
            },
            identity,
        )?;
        carrier(&page)?;
        usage.record_signed(&page)?;
        finish_usage(&mut usage)?;
        self.previous_page_hash = page.hash()?;
        self.next_cursor = next_cursor;
        self.page_count += 1;
        self.complete = page.payload.complete;
        self.usage = usage;
        Ok(page)
    }
}

#[cfg(test)]
mod tests {
    use super::super::{
        client_policy_import::require_confirmed_client_policy,
        execution_observation::tests::{
            RetainedHistoryFixture, retained_history_fixture_with_execution_source,
        },
        node_execution_policy::{
            PolicyCarrierOrigin, approve_task_execution_policy, begin_task_policy_import,
            task_policy_source_from_current,
        },
        remote_execution_policy::{
            RemoteExecutionPolicyPurpose, RemoteExecutionPolicySelection,
            tests::{confirmed_source, remote_policy_stream},
            verify_remote_execution_policy_delegation_source,
        },
    };
    use super::*;

    fn current_policy(fixture: &RetainedHistoryFixture) -> VerifiedTaskCurrentPolicy {
        let now = fixture.invocation.payload.not_before_ms;
        let retained = require_confirmed_client_policy(&fixture.source).unwrap();
        let project = fixture
            .actual
            .handoff
            .handoff
            .source_statement()
            .unwrap()
            .project;
        let grant_leaf = retained
            .state()
            .grants
            .values()
            .find(|grant| {
                grant.payload.actions.contains(&Action::ExecutionOpen)
                    && grant.payload.grantee
                        == Grantee::Actor(Actor::Account(fixture.source.scope.account_id.clone()))
            })
            .unwrap()
            .hash()
            .unwrap();
        let source = confirmed_source(
            &ClientAuthorizationBundle {
                journal: retained.journal().to_vec(),
                grant_leaf,
            },
            retained.pins(),
            &fixture.client,
            &fixture.project_node,
            &project,
            now,
        )
        .unwrap();
        let source_policy = require_confirmed_client_policy(&source).unwrap();
        let leaf = fixture
            .state
            .grant(&fixture.actual.handoff.invocation.payload.grant_leaf)
            .unwrap();
        let ResourceScope::Exact(resource) = &leaf.payload.scope else {
            panic!("fixture grant is not exact")
        };
        let selection = RemoteExecutionPolicySelection {
            execution_resource: resource.clone(),
            target_public_key: PublicKey::of(&fixture.executor),
            execution_grant: leaf.hash().unwrap(),
        };
        let mut context = leaf.payload.constraints.context.clone();
        context.workspace_resource = Some(project.resource.clone());
        let context = prepare::PrepareContext {
            context,
            binding_generation: Some(1),
            canonical_cwd: Some("/tmp".into()),
            execution_target_public_key: Some(PublicKey::of(&fixture.executor)),
        };
        let stream = remote_policy_stream(
            &source,
            &fixture.client,
            &fixture.project_node,
            &fixture.execution_bundle,
            &fixture.pins,
            &fixture.executor,
            &selection,
            &context,
            Action::ExecutionOpen,
            RemoteExecutionPolicyPurpose::Execution,
            now,
        )
        .unwrap();
        let current = verify_remote_execution_policy_delegation_source(
            source_policy.state(),
            &stream.request.intent,
            &PublicKey::of(&fixture.project_node),
            &PublicKey::of(&fixture.client),
            now,
        )
        .unwrap();
        let trust =
            approve_task_execution_policy(&current, None, &fixture.project_node, now).unwrap();
        let source = task_policy_source_from_current(&current, source_policy.pins(), now).unwrap();
        let mut import = begin_task_policy_import(
            &trust,
            &source,
            &stream.request,
            &stream.header,
            &[],
            PolicyCarrierOrigin::SignedTranscript,
            now,
        )
        .unwrap();
        for page in &stream.pages {
            import.append_page(page, now).unwrap();
        }
        import.finish_current(&stream.witness, now).unwrap()
    }

    fn noise_peer(source: &Identity, target: &Identity) -> ActualPolicyNoisePeer {
        let mut source_handshake = crate::Handshake::new(
            true,
            &source.secret_hex(),
            Some(target.node_id()),
            b"task-business-policy-test",
        )
        .unwrap();
        let mut target_handshake = crate::Handshake::new(
            false,
            &target.secret_hex(),
            Some(source.node_id()),
            b"task-business-policy-test",
        )
        .unwrap();
        target_handshake
            .read(&source_handshake.write(&[]).unwrap())
            .unwrap();
        source_handshake
            .read(&target_handshake.write(&[]).unwrap())
            .unwrap();
        target_handshake
            .read(&source_handshake.write(&[]).unwrap())
            .unwrap();
        ActualPolicyNoisePeer::from_finished_handshake(&target_handshake).unwrap()
    }

    fn original_authorization(
        state: &VerifiedPolicySet,
        wire: &SignedExecutionHandoff,
        now: u64,
    ) -> VerifiedAuthorization {
        let event: Event =
            serde_json::from_str(&wire.handoff.project_dispatch.canonical_body).unwrap();
        let EventCause::Invocation { admission } = event.cause else {
            panic!("fixture lacks original admission")
        };
        let facts = request_facts(&admission.invocation.payload);
        let historical =
            remote_resolution_history::at_heads(state, &admission.challenge.policies).unwrap();
        let admitted =
            remote_resolution_history::admitted(&historical, &admission, &facts).unwrap();
        state
            .verify_operation_authorization(
                &admitted,
                &facts,
                &admission.transport.into_evidence(),
                now,
            )
            .unwrap()
    }

    #[test]
    fn task_business_source_uses_local_grant_roots_real_noise_and_exact_wire() {
        let fixture = retained_history_fixture_with_execution_source();
        let now = fixture.invocation.payload.not_before_ms;
        let current = current_policy(&fixture);
        let wire = TaskBusinessWire::Submit(fixture.actual.handoff.clone());
        let original = original_authorization(current.state(), &fixture.actual.handoff, now);
        let transport = TransportEvidence::VerifiedPeer {
            origin: DeviceId::from_key(&PublicKey::of(&fixture.project_node)),
        };
        let verified = current
            .state()
            .verify_execution_handoff(&fixture.actual.handoff, &original, &transport, None, now)
            .unwrap();
        let mut export = build_task_business_policy_export(
            &current,
            TaskBusinessAuthorization::Submit(&verified),
            &wire,
            &fixture.project_node,
            now,
        )
        .unwrap();
        let source = require_confirmed_client_policy(&fixture.source).unwrap();
        let mut pages = Vec::new();
        for entry in source.journal() {
            if export.allows_entry(entry).unwrap() {
                pages.push(
                    export
                        .append_entry_page(&fixture.project_node, entry.clone(), now)
                        .unwrap(),
                );
            }
        }
        let (last, witness) = export.finish_pages(&fixture.project_node, now).unwrap();
        pages.push(last);
        let c_primary = &fixture.state;
        assert!(
            c_primary
                .resource(
                    &export.scope().source_project.space,
                    &export.scope().source_project.resource
                )
                .is_none()
        );
        assert!(
            c_primary
                .find_device(&DeviceId::from_key(&PublicKey::of(&fixture.project_node)))
                .is_err()
        );
        let peer = noise_peer(&fixture.project_node, &fixture.executor);
        let c_key = PublicKey::of(&fixture.executor);
        let mut import =
            begin_task_business_policy_import(c_primary, &peer, export.header(), &c_key, &[], now)
                .unwrap();
        let wrong_peer = noise_peer(&fixture.client, &fixture.executor);
        assert!(
            begin_task_business_policy_import(
                c_primary,
                &wrong_peer,
                export.header(),
                &c_key,
                &[],
                now
            )
            .is_err()
        );
        let mut wrong_header = export.header().payload.clone();
        wrong_header.scope.binding_generation += 1;
        wrong_header.scope.task_ref = EventRef::parse(&"fe".repeat(32)).unwrap();
        let wrong_header = Signed::sign(wrong_header, &fixture.project_node).unwrap();
        assert!(
            begin_task_business_policy_import(c_primary, &peer, &wrong_header, &c_key, &[], now)
                .is_err()
        );
        let mut wrong_roots = export.pins().to_vec();
        wrong_roots[0].genesis_root = PublicKey::of(&fixture.client);
        assert!(
            begin_task_business_policy_import(
                c_primary,
                &peer,
                export.header(),
                &c_key,
                &wrong_roots,
                now
            )
            .is_err()
        );
        if pages.len() > 1 {
            assert!(import.append_page(&pages[1], now).is_err());
        }
        assert!(import.clone().finish(&witness, now).is_err());
        for page in &pages {
            import.append_page(page, now).unwrap();
            import.append_page(page, now).unwrap();
            assert!(import.proof_usage().counts().authorization_bytes <= MAX_PROOF_BYTES);
            assert!(import.proof_usage().counts().unique_signatures <= MAX_PROOF_SIGNATURES);
        }
        let source = import.finish(&witness, now).unwrap();
        assert_eq!(
            source.original_page_envelope_hashes(),
            pages
                .iter()
                .map(ObjectHash::body)
                .collect::<Result<Vec<_>>>()
                .unwrap()
        );
        source.verify_wire(&wire, now).unwrap();
        let mut changed = fixture.actual.handoff.clone();
        changed.invocation.payload.operation_id.push_str("-changed");
        assert!(
            source
                .verify_wire(&TaskBusinessWire::Submit(changed), now)
                .is_err()
        );
        assert!(
            source
                .verify_wire(&wire, export.header().payload.expires_at_ms)
                .is_err()
        );
        let view = source.scoped_view(c_primary, &c_key, &wire, now).unwrap();
        assert!(
            view.state()
                .resource(
                    &source.scope().source_project.space,
                    &source.scope().source_project.resource
                )
                .is_some()
        );
        let original = original_authorization(view.state(), &fixture.actual.handoff, now);
        assert!(
            c_primary
                .verify_execution_handoff(&fixture.actual.handoff, &original, &transport, None, now)
                .is_err()
        );
        let accepted = view
            .state()
            .verify_execution_handoff(&fixture.actual.handoff, &original, &transport, None, now)
            .unwrap();
        let mut usage = view.proof_usage().clone();
        usage.merge(accepted.proof_usage());
        usage.enforce_limits().unwrap();
        let mut floor = source.pins().to_vec();
        floor[0].minimum_head.as_mut().unwrap().sequence += 1;
        assert!(source.check_saved_floors(&floor).is_err());
        let mut revoked = c_primary.clone();
        let domain = PolicyDomain::Space(source.scope().execution_resource.space.clone());
        let mut space = revoked
            .space(&source.scope().execution_resource.space)
            .unwrap()
            .clone();
        space.previous = revoked.head(&domain).cloned();
        space.sequence += 1;
        space
            .grants
            .retain(|hash| hash != &source.scope().execution_grant);
        revoked
            .install_space(Signed::sign(space, &fixture.root).unwrap())
            .unwrap();
        assert!(source.scoped_view(&revoked, &c_key, &wire, now).is_err());
    }
}
