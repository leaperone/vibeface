//! Client authentication imports public policy in bounded, purpose-bound carriers.

use super::execution_handoff::ExecutionProofUsage;
use super::{
    policy_import::{BoundedPolicyImport, VerifiedPolicySnapshot},
    *,
};
use crate::signature_verification::{ScopedSignatureVerification, SignatureVerification};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AuthorizationSelection {
    pub resource: ResourceRef,
    pub audience: Audience,
    pub grant_leaf: ObjectHash,
}

macro_rules! client_policy_object {
    ($name:ident, $domain:literal) => {
        impl sealed::Sealed for $name {}
        impl SignedObject for $name {
            const DOMAIN: &'static str = $domain;
        }
    };
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ClientPolicyReadRequest {
    pub version: u16,
    pub account: AccountId,
    pub space: SpaceId,
    pub device_public_key: PublicKey,
    pub node_public_key: PublicKey,
    pub saved_pins: Vec<PolicyPin>,
    pub selection: Option<AuthorizationSelection>,
    pub nonce: Nonce,
    pub issued_at_ms: u64,
    pub expires_at_ms: u64,
}
client_policy_object!(ClientPolicyReadRequest, "client-policy-read-request-v2");

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ClientPolicySnapshot {
    pub version: u16,
    pub request_hash: ObjectHash,
    pub snapshot_id: Nonce,
    pub saved_pins_hash: ObjectHash,
    pub target_heads: Vec<PolicyStamp>,
    pub selection: AuthorizationSelection,
    pub source_node_public_key: PublicKey,
    pub issued_at_ms: u64,
    pub expires_at_ms: u64,
}
client_policy_object!(ClientPolicySnapshot, "client-policy-snapshot-v2");

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ClientPolicyPageRequest {
    pub snapshot_id: Nonce,
    pub request_hash: ObjectHash,
    pub cursor: Option<ObjectHash>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ClientPolicyPage {
    pub version: u16,
    pub request_hash: ObjectHash,
    pub snapshot_id: Nonce,
    pub target_heads_hash: ObjectHash,
    pub page_index: u64,
    pub previous_page_hash: ObjectHash,
    pub entry: Option<JournalEntry>,
    pub next_cursor: Option<ObjectHash>,
    pub complete: bool,
}
client_policy_object!(ClientPolicyPage, "client-policy-page-v2");

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ClientPolicyWitness {
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
client_policy_object!(ClientPolicyWitness, "client-policy-witness-v2");

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ClientPolicySelectionsPage {
    pub version: u16,
    pub request_hash: ObjectHash,
    pub snapshot_id: Nonce,
    pub saved_pins_hash: ObjectHash,
    pub selections: Vec<AuthorizationSelection>,
    pub next_cursor: Option<ObjectHash>,
    pub source_node_public_key: PublicKey,
    pub issued_at_ms: u64,
    pub expires_at_ms: u64,
}
client_policy_object!(
    ClientPolicySelectionsPage,
    "client-policy-selections-page-v2"
);

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ClientPolicyContextReference {
    pub snapshot_id: Nonce,
    pub request_hash: ObjectHash,
    pub snapshot_hash: ObjectHash,
    pub witness_hash: ObjectHash,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ClientPolicyAuthConfirmRequest {
    pub version: u16,
    pub context: ClientPolicyContextReference,
    pub selection: AuthorizationSelection,
    pub target_heads: Vec<PolicyStamp>,
    pub device_public_key: PublicKey,
    pub issued_at_ms: u64,
    pub expires_at_ms: u64,
}
client_policy_object!(
    ClientPolicyAuthConfirmRequest,
    "client-policy-auth-confirm-request-v2"
);

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ClientPolicyAuthConfirmation {
    pub version: u16,
    pub context: ClientPolicyContextReference,
    pub confirmation_request_hash: ObjectHash,
    pub selection: AuthorizationSelection,
    pub target_heads: Vec<PolicyStamp>,
    pub device_public_key: PublicKey,
    pub source_node_public_key: PublicKey,
    pub accepted_at_ms: u64,
}
client_policy_object!(
    ClientPolicyAuthConfirmation,
    "client-policy-auth-confirmation-v2"
);

#[derive(Clone, Debug)]
pub(crate) struct ClientPolicyProvenance {
    request: Signed<ClientPolicyReadRequest>,
    snapshot: Signed<ClientPolicySnapshot>,
    witness: Signed<ClientPolicyWitness>,
    confirmed: bool,
}

pub fn client_policy_request_hash(request: &Signed<ClientPolicyReadRequest>) -> Result<ObjectHash> {
    request.hash()
}
pub fn client_policy_saved_pins_hash(pins: &[PolicyPin]) -> Result<ObjectHash> {
    ObjectHash::body(&pins)
}
pub fn client_policy_target_heads_hash(heads: &[PolicyStamp]) -> Result<ObjectHash> {
    ObjectHash::body(&heads)
}

pub fn client_policy_resource<'a>(
    state: &'a VerifiedPolicySet,
    hash: &ObjectHash,
) -> Option<&'a Signed<ResourceRecord>> {
    state.resources.get(hash)
}

fn parse<T: for<'de> Deserialize<'de>>(value: &str) -> Result<T> {
    require(
        value.len() <= MAX_PROOF_BYTES,
        "client policy carrier exceeds byte limit",
    )?;
    serde_json::from_str(value)
        .map_err(|_| AuthorizationError::InvalidProof("invalid client policy carrier"))
}
fn encode<T: Serialize>(value: &T) -> crate::Result<String> {
    let encoded =
        serde_json::to_string(value).map_err(|_| "client policy encoding failed".to_owned())?;
    require(
        encoded.len() <= MAX_PROOF_BYTES,
        "client policy carrier exceeds byte limit",
    )
    .map_err(|error| error.to_string())?;
    Ok(encoded)
}
fn window(issued: u64, expires: u64, now: u64) -> Result<()> {
    validate_window(issued, expires, now)?;
    require(
        expires - issued <= bootstrap::NODE_CLAIM_MAX_AGE_MS,
        "client policy validity too long",
    )
}
fn pin_genesis(state: &VerifiedPolicySet, domain: &PolicyDomain) -> Result<PublicKey> {
    match domain {
        PolicyDomain::Account(account) => state
            .account(account)
            .map(|value| value.genesis_root.clone()),
        PolicyDomain::Space(space) => state.space(space).map(|value| match &value.root {
            SpaceRoot::Personal { genesis_root, .. } | SpaceRoot::Team { genesis_root } => {
                genesis_root.clone()
            }
        }),
    }
    .ok_or(AuthorizationError::PolicyConflict)
}
fn validate_pins(pins: &[PolicyPin]) -> Result<()> {
    require(
        !pins.is_empty()
            && pins.len() <= MAX_POLICY_DOMAINS
            && pins.windows(2).all(|pair| pair[0].domain < pair[1].domain)
            && pins.iter().all(|pin| {
                pin.minimum_head
                    .as_ref()
                    .is_none_or(|head| head.domain == pin.domain)
            }),
        "invalid independent client policy pins",
    )
}
fn check_saved_domains(
    state: &VerifiedPolicySet,
    pins: &[PolicyPin],
    heads: &[PolicyStamp],
) -> Result<()> {
    for head in heads {
        let pin = pins.iter().find(|pin| pin.domain == head.domain).ok_or(
            AuthorizationError::InvalidProof("client policy requires independently saved trust"),
        )?;
        require(
            pin.genesis_root == pin_genesis(state, &head.domain)?
                && pin
                    .minimum_head
                    .as_ref()
                    .is_none_or(|minimum| state.roots.contains_key(minimum)),
            "client policy changed independent roots or saved exact heads",
        )?;
    }
    Ok(())
}

pub struct VerifiedClientPolicyRead {
    heads: Vec<PolicyStamp>,
    selection: Option<AuthorizationSelection>,
    usage: ExecutionProofUsage,
}
impl VerifiedClientPolicyRead {
    pub fn heads(&self) -> &[PolicyStamp] {
        &self.heads
    }
    pub fn domains(&self) -> BTreeSet<PolicyDomain> {
        self.heads.iter().map(|head| head.domain.clone()).collect()
    }
    pub fn selection(&self) -> Option<&AuthorizationSelection> {
        self.selection.as_ref()
    }
    pub fn proof_usage(&self) -> &ExecutionProofUsage {
        &self.usage
    }
}

pub fn verify_client_policy_read(
    state: &VerifiedPolicySet,
    signed: &Signed<ClientPolicyReadRequest>,
    local_node: &PublicKey,
    noise: &PublicKey,
    now: u64,
) -> Result<VerifiedClientPolicyRead> {
    verify_client_policy_read_inner(
        state,
        signed,
        signed.payload.selection.as_ref(),
        local_node,
        noise,
        now,
    )
}

pub fn verify_client_policy_read_selection(
    state: &VerifiedPolicySet,
    signed: &Signed<ClientPolicyReadRequest>,
    selection: &AuthorizationSelection,
    local_node: &PublicKey,
    noise: &PublicKey,
    now: u64,
) -> Result<VerifiedClientPolicyRead> {
    require(
        signed
            .payload
            .selection
            .as_ref()
            .is_none_or(|expected| expected == selection),
        "client policy candidate differs from the requested selection",
    )?;
    verify_client_policy_read_inner(state, signed, Some(selection), local_node, noise, now)
}

fn verify_client_policy_read_inner(
    state: &VerifiedPolicySet,
    signed: &Signed<ClientPolicyReadRequest>,
    selection: Option<&AuthorizationSelection>,
    local_node: &PublicKey,
    noise: &PublicKey,
    now: u64,
) -> Result<VerifiedClientPolicyRead> {
    let _signature_scope = ScopedSignatureVerification::new();
    require(
        canonical(signed)?.len() <= MAX_PROOF_BYTES,
        "client policy request too large",
    )?;
    let request = &signed.payload;
    validate_pins(&request.saved_pins)?;
    window(request.issued_at_ms, request.expires_at_ms, now)?;
    require(
        request.version == AUTH_VERSION
            && request.node_public_key == *local_node
            && request.device_public_key == *noise
            && signed.issuer == *noise,
        "client policy read differs from Noise identity or node",
    )?;
    let mut budget = Budget::default();
    budget.verify(signed)?;
    let mut dependencies = BTreeMap::new();
    let certificate = state.find_device(&DeviceId::from_key(noise))?;
    require(
        certificate.payload.owner == PolicyDomain::Account(request.account.clone())
            && certificate.payload.uses.contains(&DeviceUse::Noise)
            && certificate
                .payload
                .uses
                .contains(&DeviceUse::AccountInvocation)
            && state.actor_for_signer(noise, now, &mut budget, &mut dependencies)?
                == Actor::Account(request.account.clone()),
        "client policy read requires this account's current client",
    )?;
    state.expand_authorities(now, &mut budget, &mut dependencies)?;
    let mut usage = ExecutionProofUsage::default();
    let heads = if let Some(selection) = selection {
        require(
            selection.resource.space == request.space,
            "client policy selection changed space",
        )?;
        let path = state.grant_chain(Some(&selection.grant_leaf))?;
        state.verify_grant_path(&path, now, &mut budget, &mut dependencies, true)?;
        let leaf = &path
            .last()
            .ok_or(AuthorizationError::InvalidProof(
                "missing client policy grant",
            ))?
            .payload;
        require(
            leaf.owner_space == request.space
                && leaf.audience == selection.audience
                && scope_covers(&leaf.scope, &selection.resource)
                && state.matches_grantee(
                    &leaf.grantee,
                    &Actor::Account(request.account.clone()),
                    now,
                    &mut budget,
                    &mut dependencies,
                )?,
            "client policy selection does not authorize this account and resource",
        )?;
        let resource = state
            .resource(&selection.resource.space, &selection.resource.resource)
            .ok_or(AuthorizationError::InvalidProof(
                "missing selected client policy resource",
            ))?;
        require(
            matches!(
                resource.payload.kind,
                ResourceKind::Project | ResourceKind::ExecutionService
            ) && resource.payload.host == selection.audience.service,
            "selected client policy resource changed kind or audience",
        )?;
        budget.verify(resource)?;
        let target = state.find_device(&selection.audience.device)?;
        require(
            target.payload.subject == Actor::Service(selection.audience.service.clone()),
            "client policy target service mismatch",
        )?;
        state.validate_service_reference(&request.space, target)?;
        state.validate_certificate(
            target,
            now,
            &mut budget,
            &mut dependencies,
            &mut BTreeSet::new(),
        )?;
        state.expand_authorities(now, &mut budget, &mut dependencies)?;
        usage.record_chain_depth(path.len());
        dependencies.into_values().collect()
    } else {
        let space_domain = PolicyDomain::Space(request.space.clone());
        let space_pin = request
            .saved_pins
            .iter()
            .find(|pin| pin.domain == space_domain)
            .ok_or(AuthorizationError::InvalidProof(
                "client policy catalog has no independent space trust",
            ))?;
        require(
            space_pin.genesis_root == pin_genesis(state, &space_domain)?
                && space_pin
                    .minimum_head
                    .as_ref()
                    .is_none_or(|head| state.roots.contains_key(head)),
            "client policy catalog changed saved space trust",
        )?;
        dependencies.into_values().collect()
    };
    usage.record_budget(&budget)?;
    check_saved_domains(state, &request.saved_pins, &heads)?;
    if let Some(counts) = SignatureVerification::current_counts() {
        usage.record_actual_signature_counts(counts);
    }
    usage.enforce_limits()?;
    Ok(VerifiedClientPolicyRead {
        heads,
        selection: selection.cloned(),
        usage,
    })
}

pub fn verify_client_policy_auth_confirm(
    state: &VerifiedPolicySet,
    read: &Signed<ClientPolicyReadRequest>,
    snapshot: &Signed<ClientPolicySnapshot>,
    witness: &Signed<ClientPolicyWitness>,
    confirm: &Signed<ClientPolicyAuthConfirmRequest>,
    local_node: &PublicKey,
    noise: &PublicKey,
    now: u64,
) -> Result<()> {
    let _signature_scope = ScopedSignatureVerification::new();
    let current = verify_client_policy_read(state, read, local_node, noise, now)?;
    snapshot.verify_signature()?;
    witness.verify_signature()?;
    confirm.verify_signature()?;
    window(
        confirm.payload.issued_at_ms,
        confirm.payload.expires_at_ms,
        now,
    )?;
    require(
        snapshot.issuer == *local_node
            && witness.issuer == *local_node
            && snapshot.payload.source_node_public_key == *local_node
            && witness.payload.source_node_public_key == *local_node
            && snapshot.payload.request_hash == read.hash()?
            && witness.payload.request_hash == read.hash()?
            && snapshot.payload.selection == confirm.payload.selection
            && witness.payload.snapshot_hash == snapshot.hash()?
            && witness.payload.snapshot_id == snapshot.payload.snapshot_id
            && snapshot.payload.saved_pins_hash
                == client_policy_saved_pins_hash(&read.payload.saved_pins)?
            && confirm.payload.expires_at_ms <= read.payload.expires_at_ms
            && confirm.issuer == *noise
            && confirm.payload.version == AUTH_VERSION
            && confirm.payload.device_public_key == *noise
            && current.selection() == Some(&confirm.payload.selection)
            && current.heads() == confirm.payload.target_heads
            && snapshot.payload.target_heads == confirm.payload.target_heads
            && witness.payload.target_heads == confirm.payload.target_heads
            && confirm.payload.context
                == (ClientPolicyContextReference {
                    snapshot_id: snapshot.payload.snapshot_id.clone(),
                    request_hash: read.hash()?,
                    snapshot_hash: snapshot.hash()?,
                    witness_hash: witness.hash()?,
                }),
        "client policy confirmation changed its exact authorization context",
    )?;
    Ok(())
}

fn finish_usage(usage: &mut ExecutionProofUsage) -> Result<()> {
    if let Some(counts) = SignatureVerification::current_counts() {
        usage.record_actual_signature_counts(counts);
    }
    usage.enforce_limits()
}

fn verify_read_binding(
    independent: &[PolicyPin],
    request: &Signed<ClientPolicyReadRequest>,
    own: &Identity,
    node: &PublicKey,
    now: u64,
) -> Result<()> {
    validate_pins(independent)?;
    request.verify_signature()?;
    window(
        request.payload.issued_at_ms,
        request.payload.expires_at_ms,
        now,
    )?;
    require(
        request.payload.version == AUTH_VERSION
            && request.payload.saved_pins == independent
            && request.issuer == PublicKey::of(own)
            && request.payload.device_public_key == PublicKey::of(own)
            && request.payload.node_public_key == *node,
        "client policy request changed its independent trust, client or Noise node",
    )
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = buildClientPolicyReadRequest))]
pub fn build_client_policy_read_request(
    account: &str,
    space: &str,
    own: &Identity,
    node_public_key: &str,
    independent_pins_json: &str,
    selection_json: &str,
    now_ms: f64,
    expires_at_ms: f64,
) -> crate::Result<String> {
    let _signature_scope = ScopedSignatureVerification::new();
    let build = || -> Result<_> {
        let pins: Vec<PolicyPin> = parse(independent_pins_json)?;
        validate_pins(&pins)?;
        let request = Signed::sign(
            ClientPolicyReadRequest {
                version: AUTH_VERSION,
                account: AccountId::parse(account)?,
                space: SpaceId::parse(space)?,
                device_public_key: PublicKey::of(own),
                node_public_key: PublicKey::parse(node_public_key)?,
                saved_pins: pins,
                selection: parse(selection_json)?,
                nonce: Nonce::random()?,
                issued_at_ms: bootstrap::timestamp(now_ms)?,
                expires_at_ms: bootstrap::timestamp(expires_at_ms)?,
            },
            own,
        )?;
        window(
            request.payload.issued_at_ms,
            request.payload.expires_at_ms,
            request.payload.issued_at_ms,
        )?;
        Ok(request)
    };
    encode(&build().map_err(|error| error.to_string())?)
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = verifyClientPolicySelections))]
pub fn verify_client_policy_selections(
    independent_pins_json: &str,
    request_json: &str,
    page_json: &str,
    own: &Identity,
    noise_node_public_key: &str,
    now_ms: f64,
) -> crate::Result<String> {
    let _signature_scope = ScopedSignatureVerification::new();
    let verify = || -> Result<_> {
        let request: Signed<ClientPolicyReadRequest> = parse(request_json)?;
        let page: Signed<ClientPolicySelectionsPage> = parse(page_json)?;
        let node = PublicKey::parse(noise_node_public_key)?;
        let now = bootstrap::timestamp(now_ms)?;
        verify_read_binding(
            &parse::<Vec<PolicyPin>>(independent_pins_json)?,
            &request,
            own,
            &node,
            now,
        )?;
        page.verify_signature()?;
        window(page.payload.issued_at_ms, page.payload.expires_at_ms, now)?;
        require(
            request.payload.selection.is_none()
                && page.issuer == node
                && page.payload.version == AUTH_VERSION
                && page.payload.source_node_public_key == node
                && page.payload.request_hash == request.hash()?
                && page.payload.saved_pins_hash
                    == client_policy_saved_pins_hash(&request.payload.saved_pins)?
                && page.payload.expires_at_ms <= request.payload.expires_at_ms
                && page.payload.selections.len() <= 1
                && page
                    .payload
                    .selections
                    .iter()
                    .all(|selection| selection.resource.space == request.payload.space),
            "client policy selection page changed its request or source",
        )?;
        let mut usage = ExecutionProofUsage::default();
        usage.record_signed(&request)?;
        usage.record_signed(&page)?;
        finish_usage(&mut usage)?;
        Ok(page.payload)
    };
    encode(&verify().map_err(|error| error.to_string())?)
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen)]
pub struct ClientPolicyImport {
    request: Signed<ClientPolicyReadRequest>,
    snapshot: Signed<ClientPolicySnapshot>,
    import: BoundedPolicyImport,
    page_count: u64,
    entry_count: u64,
    previous_page_hash: ObjectHash,
    next_cursor: Option<ObjectHash>,
    last_page: Option<Signed<ClientPolicyPage>>,
    complete: bool,
    usage: ExecutionProofUsage,
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = beginClientPolicyImport))]
pub fn begin_client_policy_import(
    independent_pins_json: &str,
    request_json: &str,
    snapshot_json: &str,
    own: &Identity,
    noise_node_public_key: &str,
    now_ms: f64,
) -> crate::Result<ClientPolicyImport> {
    let _signature_scope = ScopedSignatureVerification::new();
    let begin = || -> Result<_> {
        let independent: Vec<PolicyPin> = parse(independent_pins_json)?;
        let request: Signed<ClientPolicyReadRequest> = parse(request_json)?;
        let snapshot: Signed<ClientPolicySnapshot> = parse(snapshot_json)?;
        require(
            canonical(&(&request, &snapshot))?.len() <= MAX_PROOF_BYTES,
            "client policy begin carrier exceeds byte limit",
        )?;
        let node = PublicKey::parse(noise_node_public_key)?;
        let now = bootstrap::timestamp(now_ms)?;
        verify_read_binding(&independent, &request, own, &node, now)?;
        snapshot.verify_signature()?;
        window(
            snapshot.payload.issued_at_ms,
            snapshot.payload.expires_at_ms,
            now,
        )?;
        let header = &snapshot.payload;
        require(
            snapshot.issuer == node
                && header.source_node_public_key == node
                && header.version == AUTH_VERSION
                && header.request_hash == request.hash()?
                && header.saved_pins_hash == client_policy_saved_pins_hash(&independent)?
                && request.payload.selection.as_ref() == Some(&header.selection)
                && header.selection.resource.space == request.payload.space
                && header.expires_at_ms <= request.payload.expires_at_ms
                && !header.target_heads.is_empty()
                && header
                    .target_heads
                    .windows(2)
                    .all(|pair| pair[0].domain < pair[1].domain),
            "client policy snapshot changed its exact request, selection or Noise source",
        )?;
        let pins: Vec<_> = independent
            .into_iter()
            .filter(|pin| {
                header
                    .target_heads
                    .iter()
                    .any(|head| head.domain == pin.domain)
            })
            .collect();
        require(
            pins.len() == header.target_heads.len(),
            "client policy snapshot introduced an untrusted domain",
        )?;
        let required = pins
            .iter()
            .filter_map(|pin| pin.minimum_head.clone())
            .collect();
        let import = BoundedPolicyImport::new(pins, required)?;
        let mut usage = ExecutionProofUsage::default();
        usage.record_signed(&request)?;
        usage.record_signed(&snapshot)?;
        finish_usage(&mut usage)?;
        Ok(ClientPolicyImport {
            request,
            previous_page_hash: snapshot.hash()?,
            snapshot,
            import,
            page_count: 0,
            entry_count: 0,
            next_cursor: None,
            last_page: None,
            complete: false,
            usage,
        })
    };
    begin().map_err(|error| error.to_string())
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen)]
impl ClientPolicyImport {
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = pageRequestJson))]
    pub fn page_request_json(&self) -> crate::Result<String> {
        encode(&ClientPolicyPageRequest {
            snapshot_id: self.snapshot.payload.snapshot_id.clone(),
            request_hash: self.request.hash().map_err(|error| error.to_string())?,
            cursor: self.next_cursor.clone(),
        })
    }
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = proofUsageJson))]
    pub fn proof_usage_json(&self) -> crate::Result<String> {
        encode(
            &serde_json::json!({"last_carrier":self.usage.counts(),"page_count":self.page_count,"entry_count":self.entry_count}),
        )
    }
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = appendPage))]
    pub fn append_page(&mut self, page_json: &str, now_ms: f64) -> crate::Result<()> {
        let _signature_scope = ScopedSignatureVerification::new();
        let mut append = || -> Result<()> {
            let now = bootstrap::timestamp(now_ms)?;
            window(
                self.snapshot.payload.issued_at_ms,
                self.snapshot.payload.expires_at_ms,
                now,
            )?;
            let page: Signed<ClientPolicyPage> = parse(page_json)?;
            page.verify_signature()?;
            require(
                page.issuer == self.snapshot.issuer
                    && page.payload.version == AUTH_VERSION
                    && page.payload.request_hash == self.request.hash()?
                    && page.payload.snapshot_id == self.snapshot.payload.snapshot_id
                    && page.payload.target_heads_hash
                        == client_policy_target_heads_hash(&self.snapshot.payload.target_heads)?,
                "client policy page changed source or snapshot",
            )?;
            if self.last_page.as_ref() == Some(&page) {
                let mut usage = ExecutionProofUsage::default();
                usage.record_signed(&page)?;
                finish_usage(&mut usage)?;
                self.usage = usage;
                return Ok(());
            }
            require(
                !self.complete
                    && page.payload.page_index == self.page_count
                    && page.payload.previous_page_hash == self.previous_page_hash
                    && page.payload.complete == page.payload.next_cursor.is_none(),
                "client policy page is missing, duplicated or out of order",
            )?;
            let mut usage = ExecutionProofUsage::default();
            usage.record_signed(&page)?;
            if let Some(entry) = &page.payload.entry {
                usage.merge(&policy_proof::policy_entry_usage(
                    self.import.state(),
                    entry,
                )?);
            }
            usage.enforce_limits()?;
            if let Some(entry) = &page.payload.entry {
                self.import.apply_entry(entry.clone())?;
            }
            finish_usage(&mut usage)?;
            self.previous_page_hash = page.hash()?;
            self.next_cursor = page.payload.next_cursor.clone();
            self.page_count += 1;
            self.entry_count += u64::from(page.payload.entry.is_some());
            self.complete = page.payload.complete;
            self.last_page = Some(page);
            self.usage = usage;
            Ok(())
        };
        append().map_err(|error| error.to_string())
    }
    pub fn finish(
        self,
        witness_json: &str,
        selection_json: &str,
        now_ms: f64,
    ) -> crate::Result<VerifiedClientAuthorization> {
        let _signature_scope = ScopedSignatureVerification::new();
        let finish = || -> Result<_> {
            let now = bootstrap::timestamp(now_ms)?;
            let witness: Signed<ClientPolicyWitness> = parse(witness_json)?;
            let selection: AuthorizationSelection = parse(selection_json)?;
            witness.verify_signature()?;
            window(
                witness.payload.issued_at_ms,
                witness.payload.expires_at_ms,
                now,
            )?;
            require(
                self.complete
                    && witness.issuer == self.snapshot.issuer
                    && witness.payload.version == AUTH_VERSION
                    && witness.payload.source_node_public_key == self.snapshot.issuer
                    && witness.payload.request_hash == self.request.hash()?
                    && witness.payload.snapshot_id == self.snapshot.payload.snapshot_id
                    && witness.payload.snapshot_hash == self.snapshot.hash()?
                    && witness.payload.target_heads == self.snapshot.payload.target_heads
                    && witness.payload.final_page_hash == self.previous_page_hash
                    && witness.payload.page_count == self.page_count
                    && witness.payload.entry_count == self.entry_count
                    && witness.payload.expires_at_ms <= self.snapshot.payload.expires_at_ms
                    && selection == self.snapshot.payload.selection,
                "client policy witness changed its completed stream or selection",
            )?;
            let snapshot = self.import.finish(&self.snapshot.payload.target_heads)?;
            let (state, pins, journal) = snapshot.clone().into_parts();
            let bundle = ClientAuthorizationBundle {
                journal,
                grant_leaf: selection.grant_leaf.clone(),
            };
            let mut auth = verify_client_policy_state_selected(
                state,
                &bundle,
                &pins,
                self.request.payload.account.clone(),
                self.request.payload.space.clone(),
                self.request.issuer.clone(),
                Some(&selection.resource),
                now,
            )?;
            require(
                auth.heads == self.snapshot.payload.target_heads
                    && auth.audience == selection.audience,
                "client policy imported authorization differs from the exact snapshot",
            )?;
            let mut usage = ExecutionProofUsage::default();
            usage.record_signed(&witness)?;
            usage.record_budget(&auth.budget)?;
            usage.record_chain_depth(snapshot.state().grant_chain(Some(&auth.grant_leaf))?.len());
            finish_usage(&mut usage)?;
            auth.imported_snapshot = Some(snapshot);
            auth.client_policy_context = Some(ClientPolicyProvenance {
                request: self.request,
                snapshot: self.snapshot,
                witness,
                confirmed: false,
            });
            Ok(auth)
        };
        finish().map_err(|error| error.to_string())
    }
}

fn provenance_reference(
    provenance: &ClientPolicyProvenance,
) -> Result<ClientPolicyContextReference> {
    Ok(ClientPolicyContextReference {
        snapshot_id: provenance.snapshot.payload.snapshot_id.clone(),
        request_hash: provenance.request.hash()?,
        snapshot_hash: provenance.snapshot.hash()?,
        witness_hash: provenance.witness.hash()?,
    })
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = buildClientPolicyAuthConfirmRequest))]
pub fn build_client_policy_auth_confirm_request(
    authorization: &VerifiedClientAuthorization,
    own: &Identity,
    now_ms: f64,
    expires_at_ms: f64,
) -> crate::Result<String> {
    let _signature_scope = ScopedSignatureVerification::new();
    let build = || -> Result<_> {
        let provenance = authorization.client_policy_context.as_ref().ok_or(
            AuthorizationError::InvalidProof(
                "client policy confirmation requires completed bounded import",
            ),
        )?;
        require(
            authorization.imported_snapshot.is_some()
                && provenance.request.issuer == PublicKey::of(own)
                && authorization.scope.device_id == DeviceId::from_key(&PublicKey::of(own)),
            "client policy confirmation changed its native authorization or device",
        )?;
        let now = bootstrap::timestamp(now_ms)?;
        let expiry = bootstrap::timestamp(expires_at_ms)?;
        window(now, expiry, now)?;
        require(
            expiry <= provenance.request.payload.expires_at_ms,
            "client policy confirmation outlives the original read",
        )?;
        Signed::sign(
            ClientPolicyAuthConfirmRequest {
                version: AUTH_VERSION,
                context: provenance_reference(provenance)?,
                selection: provenance.snapshot.payload.selection.clone(),
                target_heads: authorization.heads.clone(),
                device_public_key: PublicKey::of(own),
                issued_at_ms: now,
                expires_at_ms: expiry,
            },
            own,
        )
    };
    encode(&build().map_err(|error| error.to_string())?)
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = confirmClientPolicyAuthorization))]
pub fn confirm_client_policy_authorization(
    authorization: &VerifiedClientAuthorization,
    confirmation_request_json: &str,
    confirmation_json: &str,
    noise_node_public_key: &str,
    now_ms: f64,
) -> crate::Result<VerifiedClientAuthorization> {
    let _signature_scope = ScopedSignatureVerification::new();
    let confirm =
        || -> Result<_> {
            let provenance = authorization.client_policy_context.as_ref().ok_or(
                AuthorizationError::InvalidProof(
                    "client policy confirmation requires completed bounded import",
                ),
            )?;
            let snapshot = authorization.imported_snapshot.as_ref().ok_or(
                AuthorizationError::InvalidProof(
                    "client policy confirmation requires a genuine imported snapshot",
                ),
            )?;
            let request: Signed<ClientPolicyAuthConfirmRequest> = parse(confirmation_request_json)?;
            let confirmation: Signed<ClientPolicyAuthConfirmation> = parse(confirmation_json)?;
            let now = bootstrap::timestamp(now_ms)?;
            let node = PublicKey::parse(noise_node_public_key)?;
            verify_client_policy_auth_confirm(
                snapshot.state(),
                &provenance.request,
                &provenance.snapshot,
                &provenance.witness,
                &request,
                &node,
                &provenance.request.issuer,
                now,
            )?;
            confirmation.verify_signature()?;
            let receipt = &confirmation.payload;
            require(
                confirmation.issuer == node
                    && node == provenance.request.payload.node_public_key
                    && receipt.version == AUTH_VERSION
                    && receipt.source_node_public_key == node
                    && receipt.context == provenance_reference(provenance)?
                    && receipt.confirmation_request_hash == request.hash()?
                    && receipt.selection == provenance.snapshot.payload.selection
                    && receipt.target_heads == authorization.heads
                    && receipt.device_public_key == provenance.request.issuer
                    && request.payload.issued_at_ms <= receipt.accepted_at_ms
                    && receipt.accepted_at_ms < request.payload.expires_at_ms
                    && receipt.accepted_at_ms <= now,
                "client policy confirmation belongs to another source, snapshot or device",
            )?;
            let current = verify_client_policy_read(
                snapshot.state(),
                &provenance.request,
                &node,
                &provenance.request.issuer,
                receipt.accepted_at_ms,
            )?;
            let mut usage = current.proof_usage().clone();
            usage.record_signed(&provenance.snapshot)?;
            usage.record_signed(&provenance.witness)?;
            usage.record_signed(&request)?;
            usage.record_signed(&confirmation)?;
            finish_usage(&mut usage)?;
            let mut auth = authorization.clone();
            auth.client_policy_context
                .as_mut()
                .ok_or(AuthorizationError::PolicyConflict)?
                .confirmed = true;
            Ok(auth)
        };
    confirm().map_err(|error| error.to_string())
}

pub(crate) fn require_confirmed_client_policy(
    authorization: &VerifiedClientAuthorization,
) -> Result<&VerifiedPolicySnapshot> {
    let provenance =
        authorization
            .client_policy_context
            .as_ref()
            .ok_or(AuthorizationError::InvalidProof(
                "bounded client authentication and confirmation are required",
            ))?;
    require(
        provenance.confirmed
            && provenance.request.payload.account == authorization.scope.account_id
            && provenance.request.payload.space == authorization.scope.space_id
            && provenance.request.issuer == provenance.request.payload.device_public_key
            && DeviceId::from_key(&provenance.request.issuer) == authorization.scope.device_id,
        "bounded client authorization is not confirmed for this account and device",
    )?;
    authorization
        .imported_snapshot
        .as_ref()
        .ok_or(AuthorizationError::InvalidProof(
            "bounded client authorization has no genuine policy snapshot",
        ))
}
