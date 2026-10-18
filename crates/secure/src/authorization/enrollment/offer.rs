//! Public trust transfer from an existing account manager to a new browser.

use super::*;
use crate::authorization::{enrollment_policy_import::*, execution_handoff::ExecutionProofUsage};

pub const MAX_ENROLLMENT_OFFER_BYTES: usize = MAX_PROOF_BYTES;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct EnrollmentEntry {
    pub endpoint: String,
    pub node_public_key: PublicKey,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct EnrollmentOffer {
    pub version: u16,
    pub account: AccountId,
    pub pins: Vec<PolicyPin>,
    pub required_heads: Vec<PolicyStamp>,
    pub manager_certificate_hash: ObjectHash,
    pub entry: EnrollmentEntry,
    pub requested_rights: Vec<AccountRight>,
    pub certificate_expires_at_ms: u64,
    pub issued_at_ms: u64,
    pub expires_at_ms: u64,
    pub nonce: Nonce,
}
impl sealed::Sealed for EnrollmentOffer {}
impl SignedObject for EnrollmentOffer {
    const DOMAIN: &'static str = "device-enrollment-offer-v2";
}

#[derive(Clone, Debug)]
#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen)]
pub struct EnrollmentOfferDescriptor {
    signed: Signed<EnrollmentOffer>,
}

#[derive(Clone, Debug)]
#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen)]
pub struct VerifiedEnrollmentOffer {
    signed: Signed<EnrollmentOffer>,
    account_head: PolicyStamp,
    reader_public_key: PublicKey,
    snapshot: VerifiedPolicySnapshot,
}

#[derive(Clone, Debug)]
#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen)]
pub struct EnrollmentOfferImport {
    offer: EnrollmentOfferDescriptor,
    inner: EnrollmentPolicyImport,
    usage: ExecutionProofUsage,
}

pub fn enrollment_offer_hash(offer: &Signed<EnrollmentOffer>) -> Result<ObjectHash> {
    ObjectHash::body(offer)
}

pub fn decode_enrollment_offer(bytes: &[u8]) -> Result<Signed<EnrollmentOffer>> {
    require(
        bytes.len() <= MAX_ENROLLMENT_OFFER_BYTES,
        "enrollment offer too large",
    )?;
    let offer: Signed<EnrollmentOffer> = serde_json::from_slice(bytes)
        .map_err(|_| AuthorizationError::InvalidProof("invalid enrollment offer JSON"))?;
    require(
        canonical(&offer)?.len() <= MAX_ENROLLMENT_OFFER_BYTES,
        "enrollment offer too large",
    )?;
    Ok(offer)
}

fn endpoint_is_valid(value: &str) -> bool {
    value.len() <= 4096
        && (value.starts_with("https://") || value.starts_with("http://"))
        && !value
            .bytes()
            .any(|b| b.is_ascii_control() || b.is_ascii_whitespace())
        && !value.contains(['#', '@'])
        && value
            .split_once("://")
            .is_some_and(|(_, tail)| !tail.is_empty() && !tail.starts_with('/'))
}

fn verify_offer_descriptor(signed: &Signed<EnrollmentOffer>, now: u64) -> Result<()> {
    require(
        canonical(signed)?.len() <= MAX_ENROLLMENT_OFFER_BYTES,
        "enrollment offer too large",
    )?;
    signed.verify_signature()?;
    let offer = &signed.payload;
    validate_window(offer.issued_at_ms, offer.expires_at_ms, now)?;
    require(
        offer.version == AUTH_VERSION
            && offer.expires_at_ms <= offer.issued_at_ms.saturating_add(INVOCATION_MAX_AGE_MS)
            && offer.expires_at_ms <= offer.certificate_expires_at_ms
            && offer.requested_rights.len() <= 3
            && sorted_unique(&offer.requested_rights)
            && endpoint_is_valid(&offer.entry.endpoint)
            && offer.pins.len() == 2
            && offer
                .pins
                .windows(2)
                .all(|pair| pair[0].domain < pair[1].domain)
            && offer.pins.iter().all(|pin| {
                pin.domain == PolicyDomain::Account(offer.account.clone())
                    || pin.domain == PolicyDomain::Space(SpaceId::personal(&offer.account))
            })
            && offer.required_heads.len() == offer.pins.len()
            && offer
                .pins
                .iter()
                .zip(&offer.required_heads)
                .all(|(pin, head)| {
                    pin.domain == head.domain && pin.minimum_head.as_ref() == Some(head)
                }),
        "invalid finite enrollment offer descriptor",
    )
}

pub fn enrollment_offer_policy_descriptor(
    signed: &Signed<EnrollmentOffer>,
) -> EnrollmentPolicyDescriptor {
    let offer = &signed.payload;
    EnrollmentPolicyDescriptor {
        version: AUTH_VERSION,
        purpose: EnrollmentPolicyPurpose::BrowserDevice,
        account: offer.account.clone(),
        requesting_device: DeviceId::from_key(&signed.issuer),
        source_node_public_key: offer.entry.node_public_key.clone(),
        pins: offer.pins.clone(),
        required_heads: offer.required_heads.clone(),
        target_heads: offer.required_heads.clone(),
        claim_hash: None,
        nonce: offer.nonce.clone(),
        issued_at_ms: offer.issued_at_ms,
        expires_at_ms: offer.expires_at_ms,
    }
}

pub fn verify_enrollment_offer_current(
    state: &VerifiedPolicySet,
    signed: &Signed<EnrollmentOffer>,
    now: u64,
) -> Result<ExecutionProofUsage> {
    let _signature_scope = crate::signature_verification::ScopedSignatureVerification::new();
    verify_offer_descriptor(signed, now)?;
    let offer = &signed.payload;
    require(
        offer
            .required_heads
            .iter()
            .all(|head| state.head(&head.domain) == Some(head)),
        "enrollment offer policy heads changed",
    )?;
    let mut budget = Budget::default();
    let role = actor_in(
        state,
        &offer.account,
        &signed.issuer,
        AccountRight::EnrollDevice,
        now,
        &mut budget,
    )?;
    let EnrollmentActor::Device {
        authority,
        certificate,
    } = role
    else {
        return Err(AuthorizationError::InvalidProof(
            "enrollment offer requires a current manager device",
        ));
    };
    let manager = state
        .devices
        .get(&offer.manager_certificate_hash)
        .ok_or(AuthorizationError::Revoked)?;
    require(
        manager.payload == *certificate
            && manager.payload.public_key == signed.issuer
            && offer
                .requested_rights
                .iter()
                .all(|right| authority.rights.contains(right))
            && offer.certificate_expires_at_ms <= authority.expires_at_ms
            && offer.certificate_expires_at_ms <= certificate.expires_at_ms,
        "enrollment offer exceeds its exact current manager authority",
    )?;
    let target = state.find_device(&DeviceId::from_key(&offer.entry.node_public_key))?;
    require(
        target.payload.public_key == offer.entry.node_public_key
            && target.payload.owner == PolicyDomain::Account(offer.account.clone())
            && target.payload.subject
                == Actor::Service(ServiceId::from_key(&offer.entry.node_public_key))
            && target.payload.uses.contains(&DeviceUse::Noise)
            && offer.certificate_expires_at_ms <= target.payload.expires_at_ms,
        "enrollment offer target is not its current account node",
    )?;
    state.validate_certificate(
        target,
        now,
        &mut budget,
        &mut BTreeMap::new(),
        &mut BTreeSet::new(),
    )?;
    for pin in &offer.pins {
        let root = match &pin.domain {
            PolicyDomain::Account(account) => state
                .account_history(account)
                .and_then(|history| history.first())
                .map(|document| &document.payload.genesis_root),
            PolicyDomain::Space(space) => state
                .space_history(space)
                .and_then(|history| history.first())
                .map(|document| match &document.payload.root {
                    SpaceRoot::Personal { genesis_root, .. } => genesis_root,
                    SpaceRoot::Team { genesis_root, .. } => genesis_root,
                }),
        }
        .ok_or(AuthorizationError::PolicyConflict)?;
        require(
            root == &pin.genesis_root,
            "enrollment offer changed independently transferred genesis trust",
        )?;
    }
    let account_genesis = state
        .account_history(&offer.account)
        .and_then(|history| history.first())
        .ok_or(AuthorizationError::PolicyConflict)?;
    require(
        matches!(state.space(&SpaceId::personal(&offer.account)).map(|space| &space.root),
        Some(SpaceRoot::Personal { owner, genesis_root }) if owner == &offer.account && genesis_root == &account_genesis.payload.genesis_root),
        "enrollment offer personal space differs from its account genesis",
    )?;
    let mut usage = ExecutionProofUsage::default();
    usage.record_signed(signed)?;
    usage.record_budget(&budget)?;
    if let Some(counts) = crate::signature_verification::SignatureVerification::current_counts() {
        usage.record_actual_signature_counts(counts);
    }
    usage.enforce_limits()?;
    Ok(usage)
}

fn retained_offer_state(
    state: &VerifiedPolicySet,
    signed: &Signed<EnrollmentOffer>,
) -> Result<VerifiedPolicySet> {
    let mut retained = state.clone();
    for head in &signed.payload.required_heads {
        match &head.domain {
            PolicyDomain::Account(account) => {
                let known = retained
                    .accounts
                    .get_mut(account)
                    .ok_or(AuthorizationError::PolicyConflict)?;
                let position = known
                    .history
                    .iter()
                    .position(|document| document.hash().ok().as_ref() == Some(&head.head))
                    .ok_or(AuthorizationError::PolicyConflict)?;
                require(
                    state.roots.contains_key(head),
                    "enrollment account head is not retained verified history",
                )?;
                known.history.truncate(position + 1);
                known.stamp = head.clone();
            }
            PolicyDomain::Space(space) => {
                let known = retained
                    .spaces
                    .get_mut(space)
                    .ok_or(AuthorizationError::PolicyConflict)?;
                let position = known
                    .history
                    .iter()
                    .position(|document| document.hash().ok().as_ref() == Some(&head.head))
                    .ok_or(AuthorizationError::PolicyConflict)?;
                require(
                    state.roots.contains_key(head),
                    "enrollment space head is not retained verified history",
                )?;
                known.history.truncate(position + 1);
                known.stamp = head.clone();
            }
        }
    }
    let known_devices: BTreeSet<_> = retained
        .accounts
        .values()
        .flat_map(|account| account.history.iter())
        .flat_map(|document| document.payload.devices.iter().cloned())
        .chain(
            retained
                .spaces
                .values()
                .flat_map(|space| space.history.iter())
                .flat_map(|document| document.payload.devices.iter().cloned()),
        )
        .collect();
    retained
        .devices
        .retain(|hash, _| known_devices.contains(hash));
    Ok(retained)
}

pub fn verify_enrollment_offer_history(
    state: &VerifiedPolicySet,
    signed: &Signed<EnrollmentOffer>,
    now: u64,
) -> Result<()> {
    verify_enrollment_offer_current(&retained_offer_state(state, signed)?, signed, now).map(|_| ())
}

fn verify_intent_descriptor(
    signed: &Signed<EnrollmentOffer>,
    intent: &DeviceEnrollmentIntent,
    expected_new_device: &PublicKey,
    now: u64,
) -> Result<()> {
    verify_offer_descriptor(signed, now)?;
    let offer = &signed.payload;
    validate_window(
        intent.certificate_not_before_ms,
        intent.certificate_expires_at_ms,
        now,
    )?;
    let account_head = offer
        .required_heads
        .iter()
        .find(|head| head.domain == PolicyDomain::Account(offer.account.clone()))
        .ok_or(AuthorizationError::PolicyConflict)?;
    require(
        intent.account == offer.account
            && intent.new_device_public_key == *expected_new_device
            && intent.requested_rights == offer.requested_rights
            && intent.certificate_not_before_ms >= offer.issued_at_ms
            && intent.certificate_expires_at_ms <= offer.certificate_expires_at_ms
            && intent.target == DeviceId::from_key(&offer.entry.node_public_key)
            && intent.expected_heads == [account_head.clone()],
        "device enrollment intent differs from its exact verified offer",
    )
}

pub fn verify_enrollment_intent_in_state(
    state: &VerifiedPolicySet,
    signed: &Signed<EnrollmentOffer>,
    intent: &DeviceEnrollmentIntent,
    expected_new_device: &PublicKey,
    now: u64,
) -> Result<()> {
    verify_enrollment_offer_current(state, signed, now)?;
    verify_intent_descriptor(signed, intent, expected_new_device, now)?;
    independent_device_key(state, &intent.account, expected_new_device)
}

pub fn verify_enrollment_record_history(
    state: &VerifiedPolicySet,
    offer: &Signed<EnrollmentOffer>,
    intent: &DeviceEnrollmentIntent,
    approval: Option<&Signed<DeviceEnrollmentIntent>>,
    now: u64,
) -> Result<()> {
    let retained = retained_offer_state(state, offer)?;
    verify_enrollment_intent_in_state(
        &retained,
        offer,
        intent,
        &intent.new_device_public_key,
        now,
    )?;
    if let Some(approval) = approval {
        require(
            approval.payload == *intent && approval.issuer == offer.issuer,
            "stored enrollment manager approval changed",
        )?;
        verify_account_enrollment(&retained, approval, now)?;
    }
    Ok(())
}

pub fn verify_enrollment_offer_policy_read(
    state: &VerifiedPolicySet,
    offer: &Signed<EnrollmentOffer>,
    request: &Signed<EnrollmentPolicyReadRequest>,
    source: &PublicKey,
    noise: &PublicKey,
    now: u64,
) -> Result<ExecutionProofUsage> {
    let mut usage = verify_enrollment_offer_current(state, offer, now)?;
    require(
        request.payload.purpose == EnrollmentPolicyPurpose::BrowserDevice
            && request.payload.reader_public_key == request.payload.target_public_key,
        "browser enrollment policy read changed its independent new key",
    )?;
    independent_device_key(
        state,
        &offer.payload.account,
        &request.payload.target_public_key,
    )?;
    usage.merge(&verify_enrollment_policy_read_request(
        request,
        &enrollment_offer_policy_descriptor(offer),
        &enrollment_offer_hash(offer)?,
        source,
        noise,
        now,
    )?);
    usage.enforce_limits()?;
    Ok(usage)
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = createDeviceEnrollmentOffer))]
pub fn create_device_enrollment_offer(
    manager: &Identity,
    account_id: &str,
    endpoint: &str,
    target_node_public_key: &str,
    requested_rights_json: &str,
    certificate_expires_at_ms: f64,
    authorization: &VerifiedClientAuthorization,
    now_ms: f64,
) -> crate::Result<String> {
    let _signature_scope = crate::signature_verification::ScopedSignatureVerification::new();
    let create = || -> Result<Signed<EnrollmentOffer>> {
        let now = time(now_ms)?;
        let snapshot = confirmed_policy(authorization, now)?;
        let state = snapshot.state();
        let mut pins = snapshot.pins().to_vec();
        let account = AccountId::parse(account_id)?;
        authorized_account(authorization, &account)?;
        pins.retain(|pin| {
            pin.domain == PolicyDomain::Account(account.clone())
                || pin.domain == PolicyDomain::Space(SpaceId::personal(&account))
        });
        for pin in &mut pins {
            pin.minimum_head = state.head(&pin.domain).cloned();
        }
        let expires = time(certificate_expires_at_ms)?;
        let signed = Signed::sign(
            EnrollmentOffer {
                version: AUTH_VERSION,
                account: account.clone(),
                pins: pins.clone(),
                required_heads: pins
                    .iter()
                    .map(|pin| {
                        pin.minimum_head
                            .clone()
                            .ok_or(AuthorizationError::PolicyConflict)
                    })
                    .collect::<Result<Vec<_>>>()?,
                manager_certificate_hash: state
                    .find_device(&DeviceId::from_key(&PublicKey::of(manager)))?
                    .hash()?,
                entry: EnrollmentEntry {
                    endpoint: endpoint.to_owned(),
                    node_public_key: PublicKey::parse(target_node_public_key)?,
                },
                requested_rights: parse(requested_rights_json)?,
                certificate_expires_at_ms: expires,
                issued_at_ms: now,
                expires_at_ms: expires.min(now.saturating_add(INVOCATION_MAX_AGE_MS)),
                nonce: Nonce::random()?,
            },
            manager,
        )?;
        verify_enrollment_offer_current(state, &signed, now)?;
        Ok(signed)
    };
    encode(&create().map_err(|error| error.to_string())?)
}

/// Accept this transfer only from the user's existing device, such as a scanned card.
#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = verifyDeviceEnrollmentOffer))]
pub fn verify_device_enrollment_offer(
    offer_json: &str,
    now_ms: f64,
) -> crate::Result<EnrollmentOfferDescriptor> {
    let _signature_scope = crate::signature_verification::ScopedSignatureVerification::new();
    let signed =
        decode_enrollment_offer(offer_json.as_bytes()).map_err(|error| error.to_string())?;
    verify_offer_descriptor(&signed, time(now_ms).map_err(|error| error.to_string())?)
        .map_err(|error| error.to_string())?;
    Ok(EnrollmentOfferDescriptor { signed })
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = buildEnrollmentOfferPolicyReadRequest))]
pub fn build_enrollment_offer_policy_read_request(
    offer: &EnrollmentOfferDescriptor,
    new_device: &Identity,
    now_ms: f64,
    expires_at_ms: f64,
) -> crate::Result<String> {
    let _signature_scope = crate::signature_verification::ScopedSignatureVerification::new();
    let build = || -> Result<Signed<EnrollmentPolicyReadRequest>> {
        let now = time(now_ms)?;
        verify_offer_descriptor(&offer.signed, now)?;
        let descriptor = enrollment_offer_policy_descriptor(&offer.signed);
        let request = Signed::sign(
            EnrollmentPolicyReadRequest {
                version: AUTH_VERSION,
                purpose: EnrollmentPolicyPurpose::BrowserDevice,
                credential_hash: enrollment_offer_hash(&offer.signed)?,
                descriptor_hash: ObjectHash::body(&descriptor)?,
                reader_public_key: PublicKey::of(new_device),
                target_public_key: PublicKey::of(new_device),
                node_public_key: descriptor.source_node_public_key.clone(),
                nonce: Nonce::random()?,
                issued_at_ms: now,
                expires_at_ms: time(expires_at_ms)?.min(descriptor.expires_at_ms),
            },
            new_device,
        )?;
        verify_enrollment_policy_read_request(
            &request,
            &descriptor,
            &request.payload.credential_hash,
            &descriptor.source_node_public_key,
            &PublicKey::of(new_device),
            now,
        )?;
        Ok(request)
    };
    encode(&build().map_err(|error| error.to_string())?)
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = beginEnrollmentOfferImport))]
pub fn begin_enrollment_offer_import(
    offer: &EnrollmentOfferDescriptor,
    request_json: &str,
    snapshot_json: &str,
    new_device: &Identity,
    noise_source_public_key: &str,
    now_ms: f64,
) -> crate::Result<EnrollmentOfferImport> {
    let _signature_scope = crate::signature_verification::ScopedSignatureVerification::new();
    let begin = || -> Result<_> {
        require(
            offer.signed.payload.entry.node_public_key
                == PublicKey::parse(noise_source_public_key)?,
            "enrollment offer import changed its pinned Noise source",
        )?;
        let now = time(now_ms)?;
        verify_offer_descriptor(&offer.signed, now)?;
        let request: Signed<EnrollmentPolicyReadRequest> = decode_signed(request_json.as_bytes())?;
        let snapshot: Signed<EnrollmentPolicySnapshot> = decode_signed(snapshot_json.as_bytes())?;
        require(
            canonical(&(&offer.signed, &request, &snapshot))?.len() <= MAX_PROOF_BYTES,
            "enrollment offer begin carrier too large",
        )?;
        require(
            request.payload.reader_public_key == request.payload.target_public_key
                && request.payload.target_public_key == PublicKey::of(new_device),
            "enrollment offer import belongs to another new device",
        )?;
        let inner = EnrollmentPolicyImport::begin(
            enrollment_offer_policy_descriptor(&offer.signed),
            &enrollment_offer_hash(&offer.signed)?,
            request,
            snapshot,
            &PublicKey::of(new_device),
            &PublicKey::of(new_device),
            &PublicKey::parse(noise_source_public_key)?,
            now,
        )?;
        let mut usage = inner.last_proof_usage().clone();
        usage.record_signed(&offer.signed)?;
        if let Some(counts) = crate::signature_verification::SignatureVerification::current_counts()
        {
            usage.record_actual_signature_counts(counts);
        }
        usage.enforce_limits()?;
        Ok(EnrollmentOfferImport {
            offer: offer.clone(),
            inner,
            usage,
        })
    };
    begin().map_err(|error| error.to_string())
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen)]
impl EnrollmentOfferImport {
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = pageRequestJson))]
    pub fn page_request_json(&self) -> crate::Result<String> {
        encode(&self.inner.page_request())
    }
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = appendPage))]
    pub fn append_page(&mut self, page_json: &str, now_ms: f64) -> crate::Result<()> {
        let _signature_scope = crate::signature_verification::ScopedSignatureVerification::new();
        let mut append = || -> Result<()> {
            self.inner
                .append_page(decode_signed(page_json.as_bytes())?, time(now_ms)?)?;
            self.usage = self.inner.last_proof_usage().clone();
            Ok(())
        };
        append().map_err(|error| error.to_string())
    }
    pub fn finish(
        &mut self,
        witness_json: &str,
        now_ms: f64,
    ) -> crate::Result<VerifiedEnrollmentOffer> {
        let _signature_scope = crate::signature_verification::ScopedSignatureVerification::new();
        let mut finish = || -> Result<_> {
            let witness: Signed<EnrollmentPolicyWitness> = decode_signed(witness_json.as_bytes())?;
            let now = time(now_ms)?;
            let mut candidate = self.inner.clone();
            let snapshot = candidate.finish(&witness, now)?;
            let mut usage = candidate.last_proof_usage().clone();
            usage.merge(&verify_enrollment_offer_current(
                snapshot.state(),
                &self.offer.signed,
                now,
            )?);
            if let Some(counts) =
                crate::signature_verification::SignatureVerification::current_counts()
            {
                usage.record_actual_signature_counts(counts);
            }
            usage.enforce_limits()?;
            let account_head = snapshot
                .state()
                .head(&PolicyDomain::Account(
                    self.offer.signed.payload.account.clone(),
                ))
                .cloned()
                .ok_or(AuthorizationError::PolicyConflict)?;
            let result = VerifiedEnrollmentOffer {
                signed: self.offer.signed.clone(),
                account_head,
                reader_public_key: candidate.request().payload.target_public_key.clone(),
                snapshot,
            };
            self.inner = candidate;
            self.usage = usage;
            Ok(result)
        };
        finish().map_err(|error| error.to_string())
    }
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = proofUsageJson))]
    pub fn proof_usage_json(&self) -> crate::Result<String> {
        encode(&self.usage.counts())
    }
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen)]
impl EnrollmentOfferDescriptor {
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = offerHash))]
    pub fn offer_hash(&self) -> crate::Result<String> {
        enrollment_offer_hash(&self.signed)
            .map(|hash| hash.to_string())
            .map_err(|error| error.to_string())
    }
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = pinsJson))]
    pub fn pins_json(&self) -> crate::Result<String> {
        encode(&self.signed.payload.pins)
    }
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = entryJson))]
    pub fn entry_json(&self) -> crate::Result<String> {
        encode(&self.signed.payload.entry)
    }
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = accountId))]
    pub fn account_id(&self) -> String {
        self.signed.payload.account.to_string()
    }
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = expiresAtMs))]
    pub fn expires_at_ms(&self) -> f64 {
        self.signed.payload.expires_at_ms as f64
    }
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = requestedRightsJson))]
    pub fn requested_rights_json(&self) -> crate::Result<String> {
        encode(&self.signed.payload.requested_rights)
    }
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen)]
impl VerifiedEnrollmentOffer {
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = offerHash))]
    pub fn offer_hash(&self) -> crate::Result<String> {
        enrollment_offer_hash(&self.signed)
            .map(|hash| hash.to_string())
            .map_err(|error| error.to_string())
    }
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = pinsJson))]
    pub fn pins_json(&self) -> crate::Result<String> {
        encode(&self.signed.payload.pins)
    }
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = entryJson))]
    pub fn entry_json(&self) -> crate::Result<String> {
        encode(&self.signed.payload.entry)
    }
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = accountId))]
    pub fn account_id(&self) -> String {
        self.signed.payload.account.to_string()
    }
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = expiresAtMs))]
    pub fn expires_at_ms(&self) -> f64 {
        self.signed.payload.expires_at_ms as f64
    }
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = requestedRightsJson))]
    pub fn requested_rights_json(&self) -> crate::Result<String> {
        encode(&self.signed.payload.requested_rights)
    }
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = createDeviceEnrollmentIntentFromOffer))]
pub fn create_device_enrollment_intent_from_offer(
    offer: &VerifiedEnrollmentOffer,
    new_device: &Identity,
    now_ms: f64,
) -> crate::Result<String> {
    let _signature_scope = crate::signature_verification::ScopedSignatureVerification::new();
    let create = || -> Result<_> {
        let now = time(now_ms)?;
        require(
            PublicKey::of(new_device) == offer.reader_public_key,
            "enrollment offer was imported for another new device",
        )?;
        verify_enrollment_offer_current(offer.snapshot.state(), &offer.signed, now)?;
        independent_device_key(
            offer.snapshot.state(),
            &offer.signed.payload.account,
            &PublicKey::of(new_device),
        )?;
        let value = &offer.signed.payload;
        Ok(DeviceEnrollmentIntent {
            account: value.account.clone(),
            new_device_public_key: PublicKey::of(new_device),
            requested_rights: value.requested_rights.clone(),
            certificate_not_before_ms: now,
            certificate_expires_at_ms: value.certificate_expires_at_ms,
            target: DeviceId::from_key(&value.entry.node_public_key),
            expected_heads: vec![offer.account_head.clone()],
        })
    };
    encode(&create().map_err(|error| error.to_string())?)
}

fn intent_state_from_offer(
    offer: &VerifiedEnrollmentOffer,
    intent: &DeviceEnrollmentIntent,
    expected_new_device: &PublicKey,
    now: u64,
) -> Result<VerifiedPolicySet> {
    require(
        *expected_new_device == offer.reader_public_key,
        "enrollment offer was imported for another new device",
    )?;
    verify_enrollment_intent_in_state(
        offer.snapshot.state(),
        &offer.signed,
        intent,
        expected_new_device,
        now,
    )?;
    Ok(offer.snapshot.state().clone())
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = verifyDeviceEnrollmentIntentForOffer))]
pub fn verify_device_enrollment_intent_for_offer(
    offer: &VerifiedEnrollmentOffer,
    intent_json: &str,
    expected_new_device_public_key: &str,
    now_ms: f64,
) -> crate::Result<String> {
    let _signature_scope = crate::signature_verification::ScopedSignatureVerification::new();
    let verify = || -> Result<String> {
        let intent: DeviceEnrollmentIntent = parse(intent_json)?;
        intent_state_from_offer(
            offer,
            &intent,
            &PublicKey::parse(expected_new_device_public_key)?,
            time(now_ms)?,
        )?;
        String::from_utf8(canonical(&intent)?)
            .map_err(|_| AuthorizationError::InvalidProof("invalid enrollment intent UTF-8"))
    };
    verify().map_err(|error| error.to_string())
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = verifyAccountEnrollmentFromOffer))]
pub fn verify_account_enrollment_from_offer(
    signed_intent_json: &str,
    offer: &VerifiedEnrollmentOffer,
    now_ms: f64,
) -> crate::Result<VerifiedAccountEnrollment> {
    let _signature_scope = crate::signature_verification::ScopedSignatureVerification::new();
    let verify = || -> Result<VerifiedAccountEnrollment> {
        let signed: Signed<DeviceEnrollmentIntent> = decode_signed(signed_intent_json.as_bytes())?;
        require(
            signed.issuer == offer.signed.issuer,
            "enrollment approval is from another manager",
        )?;
        let now = time(now_ms)?;
        let state = intent_state_from_offer(
            offer,
            &signed.payload,
            &signed.payload.new_device_public_key,
            now,
        )?;
        verify_account_enrollment(&state, &signed, now)
    };
    verify().map_err(|error| error.to_string())
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = signPasskeyEnrollmentIntentFromOffer))]
pub fn sign_passkey_enrollment_intent_from_offer(
    intent_json: &str,
    new_device: &Identity,
    offer: &VerifiedEnrollmentOffer,
    now_ms: f64,
) -> crate::Result<String> {
    let _signature_scope = crate::signature_verification::ScopedSignatureVerification::new();
    let sign = || -> Result<Signed<DeviceEnrollmentIntent>> {
        let now = time(now_ms)?;
        let intent: DeviceEnrollmentIntent = parse(intent_json)?;
        let state = intent_state_from_offer(offer, &intent, &PublicKey::of(new_device), now)?;
        let signed = Signed::sign(intent, new_device)?;
        verify_passkey_enrollment_request(&state, &signed, now)?;
        Ok(signed)
    };
    encode(&sign().map_err(|error| error.to_string())?)
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = verifyPasskeyEnrollmentRequestFromOffer))]
pub fn verify_passkey_enrollment_request_from_offer(
    signed_json: &str,
    offer: &VerifiedEnrollmentOffer,
    now_ms: f64,
) -> crate::Result<VerifiedPasskeyEnrollmentRequest> {
    let _signature_scope = crate::signature_verification::ScopedSignatureVerification::new();
    let verify = || -> Result<VerifiedPasskeyEnrollmentRequest> {
        let now = time(now_ms)?;
        let signed: Signed<DeviceEnrollmentIntent> = decode_signed(signed_json.as_bytes())?;
        let state = intent_state_from_offer(
            offer,
            &signed.payload,
            &signed.payload.new_device_public_key,
            now,
        )?;
        verify_passkey_enrollment_request(&state, &signed, now)
    };
    verify().map_err(|error| error.to_string())
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = preparePasskeyDeviceEnrollmentFromOffer))]
pub fn prepare_passkey_device_enrollment_from_offer(
    signed_intent_json: &str,
    signed_certificate_json: &str,
    assertion_proof_json: &str,
    new_device: &Identity,
    offer: &VerifiedEnrollmentOffer,
    now_ms: f64,
) -> crate::Result<PreparedAccountDeviceChange> {
    let _signature_scope = crate::signature_verification::ScopedSignatureVerification::new();
    let prepare = || -> Result<PreparedAccountDeviceChange> {
        let now = time(now_ms)?;
        let proof = PasskeyEnrollmentProof {
            intent: decode_signed(signed_intent_json.as_bytes())?,
            certificate: decode_signed(signed_certificate_json.as_bytes())?,
            assertion: parse(assertion_proof_json)?,
        };
        let mut state = intent_state_from_offer(
            offer,
            &proof.intent.payload,
            &PublicKey::of(new_device),
            now,
        )?;
        let request = prepare_passkey_request(&mut state, proof, new_device, now)?;
        Ok(PreparedAccountDeviceChange {
            request,
            pins: offer.signed.payload.pins.clone(),
            state: offer.snapshot.state().clone(),
            imported_snapshot: Some(offer.snapshot.clone()),
            authorization: None,
        })
    };
    prepare().map_err(|error| error.to_string())
}
