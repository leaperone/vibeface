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
    usage.record_budget(&budget)?;
    let heads = if let Some(selection) = selection {
        require(
            selection.resource.space == request.space,
            "client policy selection changed space",
        )?;
        let bundle = ClientAuthorizationBundle {
            journal: Vec::new(),
            grant_leaf: selection.grant_leaf.clone(),
        };
        let verified = verify_client_policy_state_selected(
            state.clone(),
            &bundle,
            &request.saved_pins,
            request.account.clone(),
            request.space.clone(),
            noise.clone(),
            Some(&selection.resource),
            now,
        )?;
        require(
            verified.audience == selection.audience,
            "client policy selection changed audience",
        )?;
        usage.record_budget(&verified.budget)?;
        usage.record_chain_depth(state.grant_chain(Some(&selection.grant_leaf))?.len());
        verified.heads
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
    confirm.verify_signature()?;
    window(
        confirm.payload.issued_at_ms,
        confirm.payload.expires_at_ms,
        now,
    )?;
    require(
        confirm.issuer == *noise
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
