//! User-directed forwarding without adding the gateway to the target policy.

use super::{
    client_policy_import::*, execution_handoff::ExecutionProofUsage, prepare::PrepareIntent, *,
};
use crate::signature_verification::{ScopedSignatureVerification, SignatureVerification};
use serde_json::Value;

macro_rules! gateway_object {
    ($name:ident, $domain:literal) => {
        impl sealed::Sealed for $name {}
        impl SignedObject for $name {
            const DOMAIN: &'static str = $domain;
        }
    };
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum GatewayRouteState {
    Active,
    Revoked,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct GatewayRouteApproval {
    pub version: u16,
    pub route_id: Nonce,
    pub generation: u64,
    pub gateway: PublicKey,
    pub client_account: AccountId,
    pub client_key: PublicKey,
    pub target_key: PublicKey,
    pub resource: ResourceRef,
    pub allowed_actions: Vec<Action>,
    pub source_pins: Vec<PolicyPin>,
    pub state: GatewayRouteState,
    pub issued_at_ms: u64,
    pub expires_at_ms: u64,
}
gateway_object!(GatewayRouteApproval, "gateway-route-approval-v2");

#[derive(Clone, Debug)]
pub struct VerifiedGatewayRouteUpdate {
    approval: Signed<GatewayRouteApproval>,
    minimum_pins: Vec<PolicyPin>,
    usage: ExecutionProofUsage,
}

#[derive(Clone, Debug)]
pub struct VerifiedGatewayRoute {
    update: VerifiedGatewayRouteUpdate,
}

fn same_pin_roots(pins: &[PolicyPin], expected: &[PolicyPin]) -> bool {
    pins.len() == expected.len()
        && pins.iter().zip(expected).all(|(left, right)| {
            left.domain == right.domain && left.genesis_root == right.genesis_root
        })
}

fn validate_gateway_pins(pins: &[PolicyPin]) -> Result<()> {
    require(
        !pins.is_empty()
            && pins.len() <= MAX_POLICY_DOMAINS
            && pins.windows(2).all(|pair| pair[0].domain < pair[1].domain)
            && pins.iter().all(|pin| {
                pin.minimum_head
                    .as_ref()
                    .is_none_or(|head| head.domain == pin.domain)
            }),
        "invalid gateway independent policy pins",
    )
}

/// The caller supplies its current claimed account, persisted route generation and floors.
pub fn verify_gateway_route_approval(
    state: &VerifiedPolicySet,
    local_account: &AccountId,
    local_gateway: &PublicKey,
    approval: &Signed<GatewayRouteApproval>,
    current_generation: u64,
    minimum_pins: &[PolicyPin],
    now: u64,
) -> Result<VerifiedGatewayRouteUpdate> {
    let _scope = ScopedSignatureVerification::new();
    require(
        canonical(approval)?.len() <= MAX_PROOF_BYTES,
        "gateway route exceeds byte limit",
    )?;
    approval.verify_signature()?;
    let route = &approval.payload;
    let owner = state
        .account(local_account)
        .ok_or(AuthorizationError::Revoked)?;
    let (certificate, budget) = state.verify_service_device_with_budget(
        &SpaceId::personal(local_account),
        &DeviceId::from_key(local_gateway),
        now,
    )?;
    require(
        approval.issuer == owner.current_root
            && certificate.payload.public_key == *local_gateway
            && certificate.payload.owner == PolicyDomain::Account(local_account.clone())
            && certificate
                .payload
                .uses
                .contains(&DeviceUse::ServiceInvocation)
            && route.version == AUTH_VERSION
            && route.gateway == *local_gateway
            && route.gateway != route.client_key
            && route.gateway != route.target_key
            && route.client_key != route.target_key
            && route.generation == current_generation
            && route.generation > 0
            && !route.allowed_actions.is_empty()
            && route
                .allowed_actions
                .windows(2)
                .all(|pair| pair[0] < pair[1]),
        "gateway route differs from current owner, service or generation",
    )?;
    validate_window(route.issued_at_ms, route.expires_at_ms, now)?;
    validate_gateway_pins(&route.source_pins)?;
    validate_gateway_pins(minimum_pins)?;
    require(
        same_pin_roots(&route.source_pins, minimum_pins),
        "gateway route changed independent roots",
    )?;
    for (approved, current) in route.source_pins.iter().zip(minimum_pins) {
        require(
            approved.minimum_head.as_ref().is_none_or(|old| {
                current.minimum_head.as_ref().is_some_and(|new| {
                    new.authority_epoch > old.authority_epoch
                        || (new.authority_epoch == old.authority_epoch
                            && new.sequence > old.sequence)
                        || new == old
                })
            }),
            "gateway route lowered an approved minimum",
        )?;
    }
    let mut usage = ExecutionProofUsage::default();
    usage.record_budget(&budget)?;
    usage.record_signed(approval)?;
    finish_usage(&mut usage)?;
    Ok(VerifiedGatewayRouteUpdate {
        approval: approval.clone(),
        minimum_pins: minimum_pins.to_vec(),
        usage,
    })
}

pub fn verify_gateway_route(
    state: &VerifiedPolicySet,
    local_account: &AccountId,
    local_gateway: &PublicKey,
    approval: &Signed<GatewayRouteApproval>,
    current_generation: u64,
    minimum_pins: &[PolicyPin],
    now: u64,
) -> Result<VerifiedGatewayRoute> {
    let update = verify_gateway_route_approval(
        state,
        local_account,
        local_gateway,
        approval,
        current_generation,
        minimum_pins,
        now,
    )?;
    if update.approval.payload.state != GatewayRouteState::Active {
        return Err(AuthorizationError::Revoked);
    }
    Ok(VerifiedGatewayRoute { update })
}

impl VerifiedGatewayRouteUpdate {
    pub fn approval(&self) -> &Signed<GatewayRouteApproval> {
        &self.approval
    }
    pub fn approval_hash(&self) -> Result<ObjectHash> {
        self.approval.hash()
    }
}

impl VerifiedGatewayRoute {
    pub fn approval(&self) -> &Signed<GatewayRouteApproval> {
        &self.update.approval
    }
    pub fn approval_hash(&self) -> Result<ObjectHash> {
        self.update.approval.hash()
    }
    fn check_window(&self, now: u64) -> Result<()> {
        validate_window(
            self.update.approval.payload.issued_at_ms,
            self.update.approval.payload.expires_at_ms,
            now,
        )
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum GatewaySourceFrame {
    Begin {
        request: Signed<ClientPolicyReadRequest>,
        snapshot: Signed<ClientPolicySnapshot>,
    },
    Page {
        page: Signed<ClientPolicyPage>,
    },
    Finish {
        witness: Signed<ClientPolicyWitness>,
        confirm_request: Signed<ClientPolicyAuthConfirmRequest>,
        confirmation: Signed<ClientPolicyAuthConfirmation>,
    },
}

#[derive(Clone)]
pub struct GatewayClientPolicyImport {
    route: VerifiedGatewayRoute,
    import: ClientPolicyImport,
    expires_at_ms: u64,
}

/// This importer accepts signed originals. It never attests a U-to-B Noise handshake.
pub fn begin_gateway_client_import(
    route: &VerifiedGatewayRoute,
    actual_client: &PublicKey,
    request: Signed<ClientPolicyReadRequest>,
    snapshot: Signed<ClientPolicySnapshot>,
    now: u64,
) -> Result<GatewayClientPolicyImport> {
    let _scope = ScopedSignatureVerification::new();
    route.check_window(now)?;
    let approved = &route.update.approval.payload;
    require(
        actual_client == &approved.client_key
            && request.issuer == *actual_client
            && request.payload.account == approved.client_account
            && request.payload.space == approved.resource.space
            && request.payload.node_public_key == approved.target_key
            && request.payload.selection.as_ref().is_some_and(|selection| {
                selection.resource == approved.resource
                    && selection.audience.device == DeviceId::from_key(&approved.target_key)
            })
            && same_pin_roots(&request.payload.saved_pins, &approved.source_pins),
        "gateway source differs from approved client, target or resource",
    )?;
    let expires_at_ms = request
        .payload
        .expires_at_ms
        .min(snapshot.payload.expires_at_ms);
    let import = begin_client_policy_transcript(
        request.payload.saved_pins.clone(),
        request,
        snapshot,
        actual_client,
        approved.target_key.clone(),
        now,
    )?;
    Ok(GatewayClientPolicyImport {
        route: route.clone(),
        import,
        expires_at_ms,
    })
}

#[derive(Clone, Debug)]
pub struct VerifiedGatewayClientSource {
    authorization: VerifiedClientAuthorization,
    route_hash: ObjectHash,
    reference: ClientPolicyContextReference,
    floors: Vec<PolicyPin>,
    expires_at_ms: u64,
}

impl GatewayClientPolicyImport {
    pub fn append_page(&mut self, page: Signed<ClientPolicyPage>, now: u64) -> Result<()> {
        let _scope = ScopedSignatureVerification::new();
        self.route.check_window(now)?;
        self.import.append_transcript_page(page, now)
    }

    pub fn finish(
        self,
        witness: Signed<ClientPolicyWitness>,
        confirm_request: Signed<ClientPolicyAuthConfirmRequest>,
        confirmation: Signed<ClientPolicyAuthConfirmation>,
        now: u64,
    ) -> Result<VerifiedGatewayClientSource> {
        let _scope = ScopedSignatureVerification::new();
        self.route.check_window(now)?;
        let expires_at_ms = self
            .expires_at_ms
            .min(witness.payload.expires_at_ms)
            .min(confirm_request.payload.expires_at_ms)
            .min(self.route.update.approval.payload.expires_at_ms);
        let reference = ClientPolicyContextReference {
            snapshot_id: witness.payload.snapshot_id.clone(),
            request_hash: witness.payload.request_hash.clone(),
            snapshot_hash: witness.payload.snapshot_hash.clone(),
            witness_hash: witness.hash()?,
        };
        let auth = self.import.finish_transcript(
            witness,
            confirm_request.payload.selection.clone(),
            now,
        )?;
        let auth = confirm_client_policy_transcript(
            &auth,
            confirm_request,
            confirmation,
            self.route.update.approval.payload.target_key.clone(),
            now,
        )?;
        let snapshot = require_confirmed_client_policy(&auth)?;
        let mut floors = Vec::with_capacity(self.route.update.minimum_pins.len());
        for pin in &self.route.update.minimum_pins {
            let approved = self
                .route
                .update
                .approval
                .payload
                .source_pins
                .iter()
                .find(|approved| approved.domain == pin.domain)
                .ok_or(AuthorizationError::PolicyConflict)?;
            for minimum in [approved.minimum_head.as_ref(), pin.minimum_head.as_ref()]
                .into_iter()
                .flatten()
            {
                if !snapshot.state().roots.contains_key(minimum) {
                    return Err(AuthorizationError::NeedPolicySync(vec![minimum.clone()]));
                }
            }
            let current = auth
                .heads
                .iter()
                .find(|head| head.domain == pin.domain)
                .ok_or(AuthorizationError::PolicyConflict)?;
            floors.push(PolicyPin {
                domain: pin.domain.clone(),
                genesis_root: pin.genesis_root.clone(),
                minimum_head: Some(current.clone()),
            });
        }
        validate_window(now, expires_at_ms, now)?;
        Ok(VerifiedGatewayClientSource {
            authorization: auth,
            route_hash: self.route.update.approval.hash()?,
            reference,
            floors,
            expires_at_ms,
        })
    }
}

impl VerifiedGatewayClientSource {
    pub fn check_route(&self, route: &VerifiedGatewayRoute, now: u64) -> Result<()> {
        route.check_window(now)?;
        require(
            self.route_hash == route.update.approval.hash()? && now < self.expires_at_ms,
            "gateway source differs from its current route or window",
        )?;
        let snapshot = require_confirmed_client_policy(&self.authorization)?;
        for minimum in route
            .update
            .minimum_pins
            .iter()
            .filter_map(|pin| pin.minimum_head.as_ref())
        {
            if !snapshot.state().roots.contains_key(minimum) {
                return Err(AuthorizationError::NeedPolicySync(vec![minimum.clone()]));
            }
        }
        Ok(())
    }

    pub fn reference(&self) -> &ClientPolicyContextReference {
        &self.reference
    }
    pub fn floors(&self) -> &[PolicyPin] {
        &self.floors
    }
    pub fn expires_at_ms(&self) -> u64 {
        self.expires_at_ms
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum GatewayForwardPurpose {
    Prepare,
    Invoke,
    Status,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum GatewayOriginal {
    Prepare {
        body: Value,
        intent: Signed<PrepareIntent>,
    },
    Invoke {
        body: Value,
        invocation: Signed<Invocation>,
    },
    Status {
        body: Value,
        invocation: Signed<Invocation>,
    },
}

impl GatewayOriginal {
    pub fn purpose(&self) -> GatewayForwardPurpose {
        match self {
            Self::Prepare { .. } => GatewayForwardPurpose::Prepare,
            Self::Invoke { .. } => GatewayForwardPurpose::Invoke,
            Self::Status { .. } => GatewayForwardPurpose::Status,
        }
    }
    pub fn request(&self) -> &Invocation {
        match self {
            Self::Prepare { intent, .. } => &intent.payload.request,
            Self::Invoke { invocation, .. } | Self::Status { invocation, .. } => {
                &invocation.payload
            }
        }
    }
    pub fn body(&self) -> &Value {
        match self {
            Self::Prepare { body, .. } | Self::Invoke { body, .. } | Self::Status { body, .. } => {
                body
            }
        }
    }
    pub fn issuer(&self) -> &PublicKey {
        match self {
            Self::Prepare { intent, .. } => &intent.issuer,
            Self::Invoke { invocation, .. } | Self::Status { invocation, .. } => &invocation.issuer,
        }
    }
    pub fn hash(&self) -> Result<ObjectHash> {
        ObjectHash::body(self)
    }
    fn verify(&self, usage: &mut ExecutionProofUsage, now: u64) -> Result<()> {
        require(
            canonical(self)?.len() <= MAX_PROOF_BYTES,
            "gateway original exceeds byte limit",
        )?;
        match self {
            Self::Prepare { intent, .. } => {
                intent.verify_signature()?;
                usage.record_signed(intent)?;
            }
            Self::Invoke { invocation, .. } | Self::Status { invocation, .. } => {
                invocation.verify_signature()?;
                usage.record_signed(invocation)?;
            }
        }
        let request = self.request();
        validate_window(request.not_before_ms, request.expires_at_ms, now)?;
        require(
            request.version == AUTH_VERSION
                && request.expires_at_ms - request.not_before_ms <= INVOCATION_MAX_AGE_MS
                && request.device == DeviceId::from_key(self.issuer())
                && request.body_hash == ObjectHash::body(self.body())?
                && valid_operation_id(&request.operation_id)
                && !request.expected_policies.is_empty()
                && request.expected_policies.len() <= MAX_POLICY_DOMAINS
                && request
                    .expected_policies
                    .windows(2)
                    .all(|pair| pair[0].domain < pair[1].domain)
                && match self {
                    Self::Status { .. } => request.action == Action::OperationStatusRead,
                    Self::Invoke { .. } => request.action != Action::OperationStatusRead,
                    Self::Prepare { .. } => true,
                },
            "gateway original changed signed request fields or purpose",
        )?;
        validate_context(&request.context)?;
        usage.record_metadata(self.body())?;
        if matches!(self, Self::Status { .. }) {
            let lookup = self
                .body()
                .get("operation_id")
                .and_then(Value::as_str)
                .ok_or(AuthorizationError::InvalidProof(
                    "gateway status has no lookup operation",
                ))?;
            require(
                valid_operation_id(lookup),
                "invalid gateway status lookup operation",
            )?;
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct GatewayForwardHeader {
    pub version: u16,
    pub purpose: GatewayForwardPurpose,
    pub route_approval_hash: ObjectHash,
    pub gateway_key: PublicKey,
    pub client_key: PublicKey,
    pub target_key: PublicKey,
    pub source: ClientPolicyContextReference,
    pub original_hash: ObjectHash,
    pub authorization_id: String,
    pub lookup_operation_id: Option<String>,
    pub nonce: Nonce,
    pub issued_at_ms: u64,
    pub expires_at_ms: u64,
}
gateway_object!(GatewayForwardHeader, "gateway-forward-header-v2");

#[derive(Clone, Debug)]
pub struct VerifiedGatewayForward {
    header: GatewayForwardHeader,
    usage: ExecutionProofUsage,
}

fn check_source_heads(auth: &VerifiedClientAuthorization, heads: &[PolicyStamp]) -> Result<()> {
    let source_domains: BTreeSet<_> = auth.heads.iter().map(|head| &head.domain).collect();
    let source_heads: Vec<_> = heads
        .iter()
        .filter(|head| source_domains.contains(&head.domain))
        .cloned()
        .collect();
    require(
        source_heads == auth.heads,
        "gateway original changed exact source policy heads",
    )
}

pub fn verify_gateway_original(
    route: &VerifiedGatewayRoute,
    source: &VerifiedGatewayClientSource,
    original: &GatewayOriginal,
    actual_client: &PublicKey,
    now: u64,
) -> Result<VerifiedGatewayForward> {
    let _scope = ScopedSignatureVerification::new();
    source.check_route(route, now)?;
    let approved = &route.update.approval.payload;
    let request = original.request();
    require(
        actual_client == &approved.client_key
            && original.issuer() == actual_client
            && request.resource == approved.resource
            && approved.allowed_actions.contains(&request.action)
            && request.audience.device == DeviceId::from_key(&approved.target_key),
        "gateway original differs from its current route or source",
    )?;
    let mut usage = source
        .authorization
        .client_policy_usage
        .clone()
        .ok_or(AuthorizationError::PolicyConflict)?;
    usage.merge(&route.update.usage);
    original.verify(&mut usage, now)?;
    validate_client_invocation_fields(
        &source.authorization,
        request,
        original.body(),
        request.action,
        &request.operation_id,
        actual_client,
        &DeviceId::from_key(&approved.gateway),
        now,
    )?;
    check_source_heads(&source.authorization, &request.expected_policies)?;
    usage.record_signed(&route.update.approval)?;
    finish_usage(&mut usage)?;
    Ok(VerifiedGatewayForward {
        usage,
        header: GatewayForwardHeader {
            version: AUTH_VERSION,
            purpose: original.purpose(),
            route_approval_hash: source.route_hash.clone(),
            gateway_key: approved.gateway.clone(),
            client_key: actual_client.clone(),
            target_key: approved.target_key.clone(),
            source: source.reference.clone(),
            original_hash: original.hash()?,
            authorization_id: request.operation_id.clone(),
            lookup_operation_id: status_lookup(original),
            nonce: Nonce::random()?,
            issued_at_ms: now,
            expires_at_ms: request
                .expires_at_ms
                .min(source.expires_at_ms)
                .min(approved.expires_at_ms),
        },
    })
}

fn status_lookup(original: &GatewayOriginal) -> Option<String> {
    matches!(original, GatewayOriginal::Status { .. }).then(|| {
        original
            .body()
            .get("operation_id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned()
    })
}

impl VerifiedGatewayForward {
    pub fn sign_header(
        &self,
        original: &GatewayOriginal,
        gateway: &Identity,
        now: u64,
    ) -> Result<Signed<GatewayForwardHeader>> {
        require(
            PublicKey::of(gateway) == self.header.gateway_key,
            "gateway signer differs from route",
        )?;
        validate_window(self.header.issued_at_ms, self.header.expires_at_ms, now)?;
        require(
            original.hash()? == self.header.original_hash,
            "gateway signer changed the original",
        )?;
        let signed = Signed::sign(self.header.clone(), gateway)?;
        let mut usage = self.usage.clone();
        usage.record_signed(&signed)?;
        usage.enforce_limits()?;
        require(
            canonical(&(&signed, original))?.len() <= MAX_PROOF_BYTES,
            "gateway forward carrier exceeds byte limit",
        )?;
        Ok(signed)
    }
}

#[derive(Clone, Debug)]
pub struct VerifiedDirectedGateway {
    header: Signed<GatewayForwardHeader>,
    original_hash: ObjectHash,
    usage: ExecutionProofUsage,
}

fn verify_header(
    actual_gateway: &PublicKey,
    header: &Signed<GatewayForwardHeader>,
    original: &GatewayOriginal,
    local_target: &PublicKey,
    now: u64,
) -> Result<ExecutionProofUsage> {
    require(
        canonical(&(header, original))?.len() <= MAX_PROOF_BYTES,
        "gateway forward carrier exceeds byte limit",
    )?;
    let mut usage = ExecutionProofUsage::default();
    header.verify_signature()?;
    usage.record_signed(header)?;
    original.verify(&mut usage, now)?;
    let h = &header.payload;
    let request = original.request();
    validate_window(h.issued_at_ms, h.expires_at_ms, now)?;
    require(
        h.version == AUTH_VERSION
            && h.purpose == original.purpose()
            && header.issuer == *actual_gateway
            && h.gateway_key == *actual_gateway
            && h.client_key == *original.issuer()
            && h.client_key != h.gateway_key
            && h.target_key == *local_target
            && h.target_key != h.gateway_key
            && request.presenter == DeviceId::from_key(actual_gateway)
            && request.audience.device == DeviceId::from_key(local_target)
            && h.original_hash == original.hash()?
            && h.authorization_id == request.operation_id
            && h.lookup_operation_id == status_lookup(original)
            && h.issued_at_ms >= request.not_before_ms
            && h.expires_at_ms <= request.expires_at_ms,
        "directed gateway changed Noise identity, original, target or window",
    )?;
    finish_usage(&mut usage)?;
    Ok(usage)
}

/// `actual_gateway` must come from this connection's completed Noise handshake.
pub fn verify_directed_gateway(
    actual_gateway: &PublicKey,
    header: &Signed<GatewayForwardHeader>,
    original: &GatewayOriginal,
    local_target: &PublicKey,
    now: u64,
) -> Result<VerifiedDirectedGateway> {
    let _scope = ScopedSignatureVerification::new();
    let usage = verify_header(actual_gateway, header, original, local_target, now)?;
    Ok(VerifiedDirectedGateway {
        header: header.clone(),
        original_hash: original.hash()?,
        usage,
    })
}

impl VerifiedDirectedGateway {
    pub fn header(&self) -> &Signed<GatewayForwardHeader> {
        &self.header
    }
    pub fn proof_usage(&self) -> &ExecutionProofUsage {
        &self.usage
    }
    pub fn check_original(
        &self,
        original: &GatewayOriginal,
        local_target: &PublicKey,
        now: u64,
    ) -> Result<()> {
        require(
            original.hash()? == self.original_hash,
            "directed gateway proof belongs to another original",
        )?;
        validate_window(
            self.header.payload.issued_at_ms,
            self.header.payload.expires_at_ms,
            now,
        )?;
        require(
            &self.header.payload.target_key == local_target,
            "directed gateway proof belongs to another target",
        )
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(
    tag = "kind",
    content = "value",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum GatewayOutcome {
    Ok(Value),
    Error(Value),
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct GatewayReplyHeader {
    pub version: u16,
    pub forward_hash: ObjectHash,
    pub original_hash: ObjectHash,
    pub purpose: GatewayForwardPurpose,
    pub target_key: PublicKey,
    pub outcome_hash: ObjectHash,
    pub issued_at_ms: u64,
    pub expires_at_ms: u64,
}
gateway_object!(GatewayReplyHeader, "gateway-reply-header-v2");

pub fn sign_gateway_reply(
    directed: &VerifiedDirectedGateway,
    original: &GatewayOriginal,
    outcome: &GatewayOutcome,
    target: &Identity,
    now: u64,
) -> Result<Signed<GatewayReplyHeader>> {
    directed.check_original(original, &PublicKey::of(target), now)?;
    require(
        canonical(outcome)?.len() <= MAX_PROOF_BYTES,
        "gateway outcome exceeds byte limit",
    )?;
    let signed = Signed::sign(
        GatewayReplyHeader {
            version: AUTH_VERSION,
            forward_hash: directed.header.hash()?,
            original_hash: original.hash()?,
            purpose: original.purpose(),
            target_key: PublicKey::of(target),
            outcome_hash: ObjectHash::body(outcome)?,
            issued_at_ms: now,
            expires_at_ms: directed.header.payload.expires_at_ms,
        },
        target,
    )?;
    require(
        canonical(&(&signed, outcome))?.len() <= MAX_PROOF_BYTES,
        "gateway reply exceeds byte limit",
    )?;
    let mut usage = directed.proof_usage().clone();
    usage.record_signed(&signed)?;
    usage.record_metadata(outcome)?;
    usage.enforce_limits()?;
    Ok(signed)
}

#[derive(Clone, Debug)]
pub struct GatewayReplyHandle {
    original: GatewayOriginal,
    source: ClientPolicyContextReference,
    route_hash: ObjectHash,
    gateway: PublicKey,
    target: PublicKey,
}

impl GatewayReplyHandle {
    /// Keep this handle beside the original U pending request, outside the gateway.
    pub fn new(
        original: GatewayOriginal,
        source: ClientPolicyContextReference,
        route_hash: ObjectHash,
        gateway: PublicKey,
        target: PublicKey,
        expected_client: &PublicKey,
        now: u64,
    ) -> Result<Self> {
        let _scope = ScopedSignatureVerification::new();
        let mut usage = ExecutionProofUsage::default();
        original.verify(&mut usage, now)?;
        require(
            original.issuer() == expected_client
                && original.request().presenter == DeviceId::from_key(&gateway)
                && original.request().audience.device == DeviceId::from_key(&target)
                && gateway != target,
            "gateway reply handle differs from original presenter or target",
        )?;
        finish_usage(&mut usage)?;
        Ok(Self {
            original,
            source,
            route_hash,
            gateway,
            target,
        })
    }
}

#[derive(Clone, Debug)]
pub struct VerifiedGatewayReply {
    outcome: GatewayOutcome,
    original_hash: ObjectHash,
}
impl VerifiedGatewayReply {
    /// Proven B origin does not validate a normalized invocation template for signing.
    pub fn outcome(&self) -> &GatewayOutcome {
        &self.outcome
    }
    pub fn original_hash(&self) -> &ObjectHash {
        &self.original_hash
    }
}

pub fn verify_gateway_reply(
    handle: &GatewayReplyHandle,
    forward: &Signed<GatewayForwardHeader>,
    reply: &Signed<GatewayReplyHeader>,
    outcome: &GatewayOutcome,
    now: u64,
) -> Result<VerifiedGatewayReply> {
    let _scope = ScopedSignatureVerification::new();
    let mut usage = verify_header(
        &handle.gateway,
        forward,
        &handle.original,
        &handle.target,
        now,
    )?;
    require(
        canonical(&(reply, outcome))?.len() <= MAX_PROOF_BYTES,
        "gateway reply exceeds byte limit",
    )?;
    reply.verify_signature()?;
    usage.record_signed(reply)?;
    usage.record_metadata(outcome)?;
    let r = &reply.payload;
    require(
        forward.payload.source == handle.source
            && forward.payload.route_approval_hash == handle.route_hash
            && reply.issuer == handle.target
            && r.target_key == handle.target
            && r.version == AUTH_VERSION
            && r.forward_hash == forward.hash()?
            && r.original_hash == handle.original.hash()?
            && r.purpose == handle.original.purpose()
            && r.outcome_hash == ObjectHash::body(outcome)?
            && r.issued_at_ms >= forward.payload.issued_at_ms
            && r.expires_at_ms <= forward.payload.expires_at_ms,
        "gateway reply changed target, original request or outcome",
    )?;
    validate_window(r.issued_at_ms, r.expires_at_ms, now)?;
    finish_usage(&mut usage)?;
    Ok(VerifiedGatewayReply {
        outcome: outcome.clone(),
        original_hash: r.original_hash.clone(),
    })
}

fn finish_usage(usage: &mut ExecutionProofUsage) -> Result<()> {
    if let Some(counts) = SignatureVerification::current_counts() {
        usage.record_actual_signature_counts(counts);
    }
    usage.enforce_limits()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::authorization::tests::{Fixture, NOW};
    use serde_json::json;

    fn key(byte: u8) -> Identity {
        Identity::from_secret_hex(&hex::encode([byte; 32])).unwrap()
    }

    fn original(
        user: &Identity,
        gateway: &Identity,
        target: &Identity,
        status: bool,
    ) -> GatewayOriginal {
        let account = AccountId::from_root(&PublicKey::of(&key(101)));
        let resource = ResourceRef {
            space: SpaceId::personal(&account),
            resource: ResourceId::from_bytes([102; 32]),
        };
        let body = if status {
            json!({"operation_id":"original_write","authorization_id":"status_read"})
        } else {
            json!({"text":"hello","authorization_id":"new_write"})
        };
        let invocation = Signed::sign(
            Invocation {
                version: AUTH_VERSION,
                actor: Actor::Account(account.clone()),
                device: DeviceId::from_key(&PublicKey::of(user)),
                initiator: Actor::Account(account),
                initiating_grant: None,
                resource: resource.clone(),
                action: if status {
                    Action::OperationStatusRead
                } else {
                    Action::MessageAppend
                },
                audience: Audience {
                    service: ServiceId::from_key(&PublicKey::of(target)),
                    device: DeviceId::from_key(&PublicKey::of(target)),
                },
                presenter: DeviceId::from_key(&PublicKey::of(gateway)),
                operation_id: if status { "status_read" } else { "new_write" }.into(),
                body_hash: ObjectHash::body(&body).unwrap(),
                grant_leaf: ObjectHash::body(&"grant").unwrap(),
                expected_policies: vec![PolicyStamp {
                    domain: PolicyDomain::Space(resource.space),
                    authority_epoch: 0,
                    sequence: 1,
                    head: ObjectHash::body(&"head").unwrap(),
                }],
                context: InvocationContext::default(),
                nonce: Nonce::from_bytes([103; 32]),
                not_before_ms: NOW,
                expires_at_ms: NOW + 60_000,
            },
            user,
        )
        .unwrap();
        if status {
            GatewayOriginal::Status { body, invocation }
        } else {
            GatewayOriginal::Invoke { body, invocation }
        }
    }

    fn header(
        original: &GatewayOriginal,
        gateway: &Identity,
        target: &Identity,
    ) -> Signed<GatewayForwardHeader> {
        Signed::sign(
            GatewayForwardHeader {
                version: AUTH_VERSION,
                purpose: original.purpose(),
                route_approval_hash: ObjectHash::body(&"route").unwrap(),
                gateway_key: PublicKey::of(gateway),
                client_key: original.issuer().clone(),
                target_key: PublicKey::of(target),
                source: ClientPolicyContextReference {
                    snapshot_id: Nonce::from_bytes([104; 32]),
                    request_hash: ObjectHash::body(&"read").unwrap(),
                    snapshot_hash: ObjectHash::body(&"snapshot").unwrap(),
                    witness_hash: ObjectHash::body(&"witness").unwrap(),
                },
                original_hash: original.hash().unwrap(),
                authorization_id: original.request().operation_id.clone(),
                lookup_operation_id: status_lookup(original),
                nonce: Nonce::from_bytes([105; 32]),
                issued_at_ms: NOW,
                expires_at_ms: NOW + 30_000,
            },
            gateway,
        )
        .unwrap()
    }

    #[test]
    fn directed_gateway_binds_real_noise_original_target_and_window() {
        let user = key(90);
        let gateway = key(91);
        let target = key(92);
        let request = original(&user, &gateway, &target, false);
        let signed = header(&request, &gateway, &target);
        let proof = verify_directed_gateway(
            &PublicKey::of(&gateway),
            &signed,
            &request,
            &PublicKey::of(&target),
            NOW,
        )
        .unwrap();
        proof
            .check_original(&request, &PublicKey::of(&target), NOW)
            .unwrap();
        assert!(
            verify_directed_gateway(
                &PublicKey::of(&user),
                &signed,
                &request,
                &PublicKey::of(&target),
                NOW
            )
            .is_err()
        );
        assert!(
            verify_directed_gateway(
                &PublicKey::of(&gateway),
                &signed,
                &request,
                &PublicKey::of(&user),
                NOW
            )
            .is_err()
        );
        assert!(
            verify_directed_gateway(
                &PublicKey::of(&gateway),
                &signed,
                &request,
                &PublicKey::of(&target),
                NOW + 30_000
            )
            .is_err()
        );
        let mut replaced = request.clone();
        if let GatewayOriginal::Invoke { body, invocation } = &mut replaced {
            *body = json!({"text":"different"});
            invocation.payload.body_hash = ObjectHash::body(body).unwrap();
            *invocation = Signed::sign(invocation.payload.clone(), &user).unwrap();
        }
        assert!(
            proof
                .check_original(&replaced, &PublicKey::of(&target), NOW)
                .is_err()
        );
        assert!(
            verify_directed_gateway(
                &PublicKey::of(&gateway),
                &signed,
                &replaced,
                &PublicKey::of(&target),
                NOW
            )
            .is_err()
        );
        let mut extended = signed.payload.clone();
        extended.expires_at_ms = request.request().expires_at_ms + 1;
        let extended = Signed::sign(extended, &gateway).unwrap();
        assert!(
            verify_directed_gateway(
                &PublicKey::of(&gateway),
                &extended,
                &request,
                &PublicKey::of(&target),
                NOW
            )
            .is_err()
        );
    }

    #[test]
    fn status_separates_new_read_authorization_from_original_write_lookup() {
        let user = key(90);
        let gateway = key(91);
        let target = key(92);
        let request = original(&user, &gateway, &target, true);
        let signed = header(&request, &gateway, &target);
        assert_eq!(signed.payload.authorization_id, "status_read");
        assert_eq!(
            signed.payload.lookup_operation_id.as_deref(),
            Some("original_write")
        );
        verify_directed_gateway(
            &PublicKey::of(&gateway),
            &signed,
            &request,
            &PublicKey::of(&target),
            NOW,
        )
        .unwrap();
        let mut changed = signed.payload.clone();
        changed.lookup_operation_id = Some("status_read".into());
        let changed = Signed::sign(changed, &gateway).unwrap();
        assert!(
            verify_directed_gateway(
                &PublicKey::of(&gateway),
                &changed,
                &request,
                &PublicKey::of(&target),
                NOW
            )
            .is_err()
        );
    }

    #[test]
    fn reply_binds_target_exact_outcome_and_original_pending() {
        let user = key(90);
        let gateway = key(91);
        let target = key(92);
        let request = original(&user, &gateway, &target, false);
        let forward = header(&request, &gateway, &target);
        let proof = verify_directed_gateway(
            &PublicKey::of(&gateway),
            &forward,
            &request,
            &PublicKey::of(&target),
            NOW,
        )
        .unwrap();
        let outcome = GatewayOutcome::Ok(json!({"status":"unknown"}));
        let reply = sign_gateway_reply(&proof, &request, &outcome, &target, NOW).unwrap();
        let handle = GatewayReplyHandle::new(
            request.clone(),
            forward.payload.source.clone(),
            forward.payload.route_approval_hash.clone(),
            PublicKey::of(&gateway),
            PublicKey::of(&target),
            &PublicKey::of(&user),
            NOW,
        )
        .unwrap();
        assert_eq!(
            verify_gateway_reply(&handle, &forward, &reply, &outcome, NOW)
                .unwrap()
                .outcome(),
            &outcome
        );
        assert!(
            verify_gateway_reply(
                &handle,
                &forward,
                &reply,
                &GatewayOutcome::Error(json!({"status":"unknown"})),
                NOW
            )
            .is_err()
        );
        let oversized_reply = GatewayOutcome::Ok(Value::String("x".repeat(MAX_PROOF_BYTES - 64)));
        assert!(canonical(&oversized_reply).unwrap().len() < MAX_PROOF_BYTES);
        assert!(sign_gateway_reply(&proof, &request, &oversized_reply, &target, NOW).is_err());
        let forged = Signed::sign(reply.payload.clone(), &gateway).unwrap();
        assert!(verify_gateway_reply(&handle, &forward, &forged, &outcome, NOW).is_err());
        let mut expired = reply.payload.clone();
        expired.expires_at_ms = request.request().expires_at_ms + 1;
        let expired = Signed::sign(expired, &target).unwrap();
        assert!(verify_gateway_reply(&handle, &forward, &expired, &outcome, NOW).is_err());
    }

    #[test]
    fn reply_budget_includes_the_original_request() {
        let user = key(90);
        let gateway = key(91);
        let target = key(92);
        let mut request = original(&user, &gateway, &target, false);
        if let GatewayOriginal::Invoke { body, invocation } = &mut request {
            *body = json!({"text":"x".repeat(35_000),"authorization_id":"new_write"});
            invocation.payload.body_hash = ObjectHash::body(body).unwrap();
            *invocation = Signed::sign(invocation.payload.clone(), &user).unwrap();
        }
        let forward = header(&request, &gateway, &target);
        let proof = verify_directed_gateway(
            &PublicKey::of(&gateway),
            &forward,
            &request,
            &PublicKey::of(&target),
            NOW,
        )
        .unwrap();
        let small = GatewayOutcome::Ok(json!({"status":"unknown"}));
        sign_gateway_reply(&proof, &request, &small, &target, NOW).unwrap();
        let large = GatewayOutcome::Ok(Value::String("y".repeat(35_000)));
        assert!(matches!(
            sign_gateway_reply(&proof, &request, &large, &target, NOW),
            Err(AuthorizationError::BudgetExceeded)
        ));
    }

    #[test]
    fn route_requires_current_local_root_service_generation_and_state() {
        let fixture = Fixture::new();
        let remote_root = PublicKey::of(&key(90));
        let remote_account = AccountId::from_root(&remote_root);
        let pins = vec![PolicyPin {
            domain: PolicyDomain::Account(remote_account.clone()),
            genesis_root: remote_root,
            minimum_head: None,
        }];
        let route = GatewayRouteApproval {
            version: AUTH_VERSION,
            route_id: Nonce::from_bytes([106; 32]),
            generation: 1,
            gateway: PublicKey::of(&fixture.node),
            client_account: remote_account.clone(),
            client_key: PublicKey::of(&key(91)),
            target_key: PublicKey::of(&key(92)),
            resource: ResourceRef {
                space: SpaceId::personal(&remote_account),
                resource: ResourceId::from_bytes([107; 32]),
            },
            allowed_actions: vec![Action::MessageAppend],
            source_pins: pins.clone(),
            state: GatewayRouteState::Active,
            issued_at_ms: NOW,
            expires_at_ms: NOW + 60_000,
        };
        let signed = Signed::sign(route.clone(), &fixture.root).unwrap();
        verify_gateway_route(
            &fixture.state,
            &fixture.account,
            &PublicKey::of(&fixture.node),
            &signed,
            1,
            &pins,
            NOW,
        )
        .unwrap();
        assert!(
            verify_gateway_route(
                &fixture.state,
                &fixture.account,
                &PublicKey::of(&fixture.node),
                &signed,
                2,
                &pins,
                NOW
            )
            .is_err()
        );
        let wrong_owner = Signed::sign(route.clone(), &key(91)).unwrap();
        assert!(
            verify_gateway_route(
                &fixture.state,
                &fixture.account,
                &PublicKey::of(&fixture.node),
                &wrong_owner,
                1,
                &pins,
                NOW
            )
            .is_err()
        );
        let mut revoked = route;
        revoked.state = GatewayRouteState::Revoked;
        let revoked = Signed::sign(revoked, &fixture.root).unwrap();
        let update = verify_gateway_route_approval(
            &fixture.state,
            &fixture.account,
            &PublicKey::of(&fixture.node),
            &revoked,
            1,
            &pins,
            NOW,
        )
        .unwrap();
        assert_eq!(update.approval().payload.state, GatewayRouteState::Revoked);
        assert_eq!(
            verify_gateway_route(
                &fixture.state,
                &fixture.account,
                &PublicKey::of(&fixture.node),
                &revoked,
                1,
                &pins,
                NOW
            )
            .unwrap_err(),
            AuthorizationError::Revoked
        );
    }

    struct AccountFixture {
        root: Identity,
        client: Identity,
        node: Identity,
        state: VerifiedPolicySet,
        bundle: ClientAuthorizationBundle,
        pins: Vec<PolicyPin>,
        resource: ResourceRef,
    }
    fn account_fixture(seed: u8) -> AccountFixture {
        use bootstrap::*;
        let root = key(seed);
        let recovery = key(seed + 1);
        let client = key(seed + 2);
        let node = key(seed + 3);
        let nonce = Nonce::from_bytes([seed; 32]).to_string();
        let claim = create_node_claim(&node, &nonce, NOW as f64, (NOW + 60_000) as f64).unwrap();
        let claim = verify_node_claim(&claim, &node.public_key_hex(), &nonce, NOW as f64).unwrap();
        let prepared = prepare_account_creation(
            &root,
            &recovery,
            &client,
            &claim,
            (NOW + 600_000) as f64,
            NOW as f64,
        )
        .unwrap();
        let possession = sign_node_possession(
            &prepared.node_request_json().unwrap(),
            &node,
            &nonce,
            NOW as f64,
        )
        .unwrap();
        let created = finish_account_creation(&prepared, &root, &possession, NOW as f64).unwrap();
        let bundle: ClientAuthorizationBundle =
            serde_json::from_str(&created.bundle_json().unwrap()).unwrap();
        let pins: Vec<PolicyPin> = serde_json::from_str(&created.pins_json().unwrap()).unwrap();
        let state = replay_policies(&pins, &bundle.journal).unwrap();
        let resource = bundle
            .journal
            .iter()
            .find_map(|entry| match entry {
                JournalEntry::Resource(record) => Some(ResourceRef {
                    space: record.payload.space.clone(),
                    resource: record.payload.resource.clone(),
                }),
                _ => None,
            })
            .unwrap();
        AccountFixture {
            root,
            client,
            node,
            state,
            bundle,
            pins,
            resource,
        }
    }

    #[test]
    fn signed_transcript_import_keeps_a_independent_and_retains_exact_source_floors() {
        let a = account_fixture(110);
        let b = account_fixture(120);
        let b_account = AccountId::from_root(&PublicKey::of(&b.root));
        let selection = AuthorizationSelection {
            resource: b.resource.clone(),
            audience: b
                .state
                .grant(&b.bundle.grant_leaf)
                .unwrap()
                .payload
                .audience
                .clone(),
            grant_leaf: b.bundle.grant_leaf.clone(),
        };
        let approval = Signed::sign(
            GatewayRouteApproval {
                version: AUTH_VERSION,
                route_id: Nonce::from_bytes([125; 32]),
                generation: 1,
                gateway: PublicKey::of(&a.node),
                client_account: b_account.clone(),
                client_key: PublicKey::of(&b.client),
                target_key: PublicKey::of(&b.node),
                resource: b.resource.clone(),
                allowed_actions: vec![Action::MessageAppend],
                source_pins: b.pins.clone(),
                state: GatewayRouteState::Active,
                issued_at_ms: NOW,
                expires_at_ms: NOW + 60_000,
            },
            &a.root,
        )
        .unwrap();
        let route = verify_gateway_route(
            &a.state,
            &AccountId::from_root(&PublicKey::of(&a.root)),
            &PublicKey::of(&a.node),
            &approval,
            1,
            &b.pins,
            NOW,
        )
        .unwrap();
        let request = Signed::sign(
            ClientPolicyReadRequest {
                version: AUTH_VERSION,
                account: b_account,
                space: b.resource.space.clone(),
                device_public_key: PublicKey::of(&b.client),
                node_public_key: PublicKey::of(&b.node),
                saved_pins: b.pins.clone(),
                selection: Some(selection.clone()),
                nonce: Nonce::from_bytes([126; 32]),
                issued_at_ms: NOW,
                expires_at_ms: NOW + 60_000,
            },
            &b.client,
        )
        .unwrap();
        let current = verify_client_policy_read(
            &b.state,
            &request,
            &PublicKey::of(&b.node),
            &PublicKey::of(&b.client),
            NOW,
        )
        .unwrap();
        let snapshot = Signed::sign(
            ClientPolicySnapshot {
                version: AUTH_VERSION,
                request_hash: request.hash().unwrap(),
                snapshot_id: Nonce::from_bytes([127; 32]),
                saved_pins_hash: client_policy_saved_pins_hash(&b.pins).unwrap(),
                target_heads: current.heads().to_vec(),
                selection: selection.clone(),
                source_node_public_key: PublicKey::of(&b.node),
                issued_at_ms: NOW,
                expires_at_ms: NOW + 60_000,
            },
            &b.node,
        )
        .unwrap();
        let mut import = begin_gateway_client_import(
            &route,
            &PublicKey::of(&b.client),
            request.clone(),
            snapshot.clone(),
            NOW,
        )
        .unwrap();
        let mut previous = snapshot.hash().unwrap();
        let mut cursor = None;
        let mut pages = Vec::new();
        for (index, entry) in b.bundle.journal.iter().enumerate() {
            let complete = index + 1 == b.bundle.journal.len();
            let page = Signed::sign(
                ClientPolicyPage {
                    version: AUTH_VERSION,
                    request_hash: request.hash().unwrap(),
                    snapshot_id: snapshot.payload.snapshot_id.clone(),
                    target_heads_hash: client_policy_target_heads_hash(current.heads()).unwrap(),
                    page_index: index as u64,
                    request_cursor: cursor,
                    previous_page_hash: previous,
                    entry: Some(entry.clone()),
                    next_cursor: if complete {
                        None
                    } else {
                        Some(ObjectHash::body(&index).unwrap())
                    },
                    complete,
                },
                &b.node,
            )
            .unwrap();
            previous = page.hash().unwrap();
            cursor = page.payload.next_cursor.clone();
            import.append_page(page.clone(), NOW).unwrap();
            pages.push(page);
        }
        let witness = Signed::sign(
            ClientPolicyWitness {
                version: AUTH_VERSION,
                request_hash: request.hash().unwrap(),
                snapshot_id: snapshot.payload.snapshot_id.clone(),
                target_heads: current.heads().to_vec(),
                final_page_hash: previous,
                page_count: pages.len() as u64,
                entry_count: pages.len() as u64,
                snapshot_hash: snapshot.hash().unwrap(),
                source_node_public_key: PublicKey::of(&b.node),
                issued_at_ms: NOW,
                expires_at_ms: NOW + 60_000,
            },
            &b.node,
        )
        .unwrap();
        let reference = ClientPolicyContextReference {
            snapshot_id: snapshot.payload.snapshot_id.clone(),
            request_hash: request.hash().unwrap(),
            snapshot_hash: snapshot.hash().unwrap(),
            witness_hash: witness.hash().unwrap(),
        };
        let confirm = Signed::sign(
            ClientPolicyAuthConfirmRequest {
                version: AUTH_VERSION,
                context: reference.clone(),
                selection: selection.clone(),
                target_heads: current.heads().to_vec(),
                device_public_key: PublicKey::of(&b.client),
                issued_at_ms: NOW,
                expires_at_ms: NOW + 60_000,
            },
            &b.client,
        )
        .unwrap();
        let confirmation = Signed::sign(
            ClientPolicyAuthConfirmation {
                version: AUTH_VERSION,
                context: reference.clone(),
                confirmation_request_hash: confirm.hash().unwrap(),
                selection,
                target_heads: current.heads().to_vec(),
                device_public_key: PublicKey::of(&b.client),
                source_node_public_key: PublicKey::of(&b.node),
                accepted_at_ms: NOW,
            },
            &b.node,
        )
        .unwrap();
        let source = import
            .clone()
            .finish(witness.clone(), confirm.clone(), confirmation.clone(), NOW)
            .unwrap();
        let independent_clone = import
            .finish(witness.clone(), confirm.clone(), confirmation.clone(), NOW)
            .unwrap();
        assert_eq!(source.reference(), independent_clone.reference());
        source.check_route(&route, NOW).unwrap();
        assert_eq!(source.reference(), &reference);
        assert!(
            a.state
                .account(&AccountId::from_root(&PublicKey::of(&b.root)))
                .is_none()
        );
        assert!(source.floors().iter().all(|pin| {
            current
                .heads()
                .iter()
                .any(|head| pin.minimum_head.as_ref() == Some(head))
        }));
        let body = json!({"text":"hello","authorization_id":"gateway_write"});
        let invocation = Invocation {
            version: AUTH_VERSION,
            actor: Actor::Account(source.authorization.scope.account_id.clone()),
            device: DeviceId::from_key(&PublicKey::of(&b.client)),
            initiator: Actor::Account(source.authorization.scope.account_id.clone()),
            initiating_grant: None,
            resource: b.resource.clone(),
            action: Action::MessageAppend,
            audience: source.authorization.audience.clone(),
            presenter: DeviceId::from_key(&PublicKey::of(&a.node)),
            operation_id: "gateway_write".into(),
            body_hash: ObjectHash::body(&body).unwrap(),
            grant_leaf: b.bundle.grant_leaf.clone(),
            expected_policies: source.authorization.heads.clone(),
            context: InvocationContext::default(),
            nonce: Nonce::from_bytes([128; 32]),
            not_before_ms: NOW,
            expires_at_ms: NOW + 60_000,
        };
        let original = GatewayOriginal::Invoke {
            body: body.clone(),
            invocation: Signed::sign(invocation.clone(), &b.client).unwrap(),
        };
        let forwarded =
            verify_gateway_original(&route, &source, &original, &PublicKey::of(&b.client), NOW)
                .unwrap();
        let signed = forwarded.sign_header(&original, &a.node, NOW).unwrap();
        verify_directed_gateway(
            &PublicKey::of(&a.node),
            &signed,
            &original,
            &PublicKey::of(&b.node),
            NOW,
        )
        .unwrap();
        let mut remote = invocation.clone();
        remote.expected_policies.push(PolicyStamp {
            domain: PolicyDomain::Space(SpaceId::personal(&AccountId::from_root(&PublicKey::of(
                &key(130),
            )))),
            authority_epoch: 0,
            sequence: 1,
            head: ObjectHash::body(&"unimported-c").unwrap(),
        });
        remote
            .expected_policies
            .sort_by(|left, right| left.domain.cmp(&right.domain));
        let remote = GatewayOriginal::Invoke {
            body: body.clone(),
            invocation: Signed::sign(remote, &b.client).unwrap(),
        };
        verify_gateway_original(&route, &source, &remote, &PublicKey::of(&b.client), NOW).unwrap();
        let mut changed = invocation;
        changed.expected_policies[0].sequence += 1;
        let changed = GatewayOriginal::Invoke {
            body,
            invocation: Signed::sign(changed, &b.client).unwrap(),
        };
        assert!(
            verify_gateway_original(&route, &source, &changed, &PublicKey::of(&b.client), NOW)
                .is_err()
        );
        assert!(
            verify_gateway_original(
                &route,
                &source,
                &original,
                &PublicKey::of(&b.client),
                NOW + 60_000
            )
            .is_err()
        );
        let mut high_pins = b.pins.clone();
        high_pins[0].minimum_head.as_mut().unwrap().sequence += 100;
        let high_route = verify_gateway_route(
            &a.state,
            &AccountId::from_root(&PublicKey::of(&a.root)),
            &PublicKey::of(&a.node),
            &approval,
            1,
            &high_pins,
            NOW,
        )
        .unwrap();
        assert!(matches!(
            source.check_route(&high_route, NOW),
            Err(AuthorizationError::NeedPolicySync(_))
        ));
        assert!(source.check_route(&route, NOW + 60_000).is_err());
        assert!(matches!(
            verify_gateway_original(
                &high_route,
                &source,
                &original,
                &PublicKey::of(&b.client),
                NOW
            ),
            Err(AuthorizationError::NeedPolicySync(_))
        ));
        let mut high_import = begin_gateway_client_import(
            &high_route,
            &PublicKey::of(&b.client),
            request,
            snapshot,
            NOW,
        )
        .unwrap();
        for page in pages {
            high_import.append_page(page, NOW).unwrap();
        }
        assert!(matches!(
            high_import.finish(witness, confirm, confirmation, NOW),
            Err(AuthorizationError::NeedPolicySync(_))
        ));
    }
}
