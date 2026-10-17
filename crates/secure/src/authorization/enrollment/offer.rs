//! Public trust transfer from an existing account manager to a new browser.

use super::*;

pub const MAX_ENROLLMENT_OFFER_BYTES: usize = 1_048_576;

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
    pub manager_journal: Vec<JournalEntry>,
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
pub struct VerifiedEnrollmentOffer {
    signed: Signed<EnrollmentOffer>,
    account_head: PolicyStamp,
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

fn account_entry(entry: &JournalEntry, account: &AccountId) -> bool {
    match entry {
        JournalEntry::AccountGenesis(value) | JournalEntry::Account(value) => {
            value.payload.account == *account
        }
        JournalEntry::AccountCommit(value) => value.proposal.payload.account == *account,
        JournalEntry::Device(value) => {
            value.payload.owner == PolicyDomain::Account(account.clone())
        }
        JournalEntry::Authenticator(value) => value.payload.account == *account,
        _ => false,
    }
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

pub fn verify_enrollment_offer(
    signed: &Signed<EnrollmentOffer>,
    now_ms: u64,
) -> Result<VerifiedEnrollmentOffer> {
    require(
        canonical(signed)?.len() <= MAX_ENROLLMENT_OFFER_BYTES,
        "enrollment offer too large",
    )?;
    signed.verify_signature()?;
    let offer = &signed.payload;
    validate_window(offer.issued_at_ms, offer.expires_at_ms, now_ms)?;
    require(
        offer.version == AUTH_VERSION
            && offer.expires_at_ms <= offer.issued_at_ms.saturating_add(INVOCATION_MAX_AGE_MS)
            && offer.expires_at_ms <= offer.certificate_expires_at_ms
            && offer.requested_rights.len() <= 3
            && sorted_unique(&offer.requested_rights)
            && endpoint_is_valid(&offer.entry.endpoint)
            && offer.manager_journal.len() <= 125
            && offer
                .manager_journal
                .iter()
                .all(|entry| account_entry(entry, &offer.account))
            && offer.pins.len() == 2
            && offer
                .pins
                .windows(2)
                .all(|pair| pair[0].domain < pair[1].domain)
            && offer.pins.iter().all(|pin| {
                pin.domain == PolicyDomain::Account(offer.account.clone())
                    || pin.domain == PolicyDomain::Space(SpaceId::personal(&offer.account))
            }),
        "invalid public enrollment offer",
    )?;
    let account_pins: Vec<_> = offer
        .pins
        .iter()
        .filter(|pin| pin.domain == PolicyDomain::Account(offer.account.clone()))
        .cloned()
        .collect();
    let state = replay_policies(&account_pins, &offer.manager_journal)?;
    let role = actor(
        &state,
        &offer.account,
        &signed.issuer,
        AccountRight::EnrollDevice,
        now_ms,
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
    require(
        offer
            .requested_rights
            .iter()
            .all(|right| authority.rights.contains(right))
            && offer.certificate_expires_at_ms <= authority.expires_at_ms
            && offer.certificate_expires_at_ms <= certificate.expires_at_ms,
        "enrollment offer exceeds current manager authority",
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
        now_ms,
        &mut Budget::default(),
        &mut BTreeMap::new(),
        &mut BTreeSet::new(),
    )?;
    let account_head = state
        .head(&PolicyDomain::Account(offer.account.clone()))
        .cloned()
        .ok_or(AuthorizationError::PolicyConflict)?;
    require(
        account_pins[0].minimum_head.as_ref() == Some(&account_head),
        "enrollment offer needs the current account pin",
    )?;
    let space_pin = offer
        .pins
        .iter()
        .find(|pin| pin.domain == PolicyDomain::Space(SpaceId::personal(&offer.account)))
        .ok_or(AuthorizationError::PolicyConflict)?;
    let genesis = state
        .account_history(&offer.account)
        .and_then(|history| history.first())
        .ok_or(AuthorizationError::PolicyConflict)?;
    require(
        space_pin.genesis_root == genesis.payload.genesis_root,
        "personal space pin differs from account genesis",
    )?;
    Ok(VerifiedEnrollmentOffer {
        signed: signed.clone(),
        account_head,
    })
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = createDeviceEnrollmentOffer))]
pub fn create_device_enrollment_offer(
    manager: &Identity,
    account_id: &str,
    endpoint: &str,
    target_node_public_key: &str,
    requested_rights_json: &str,
    certificate_expires_at_ms: f64,
    pins_json: &str,
    bundle_json: &str,
    now_ms: f64,
) -> crate::Result<String> {
    let create = || -> Result<Signed<EnrollmentOffer>> {
        let now = time(now_ms)?;
        let (state, mut pins, bundle) = state_from(pins_json, bundle_json)?;
        let account = AccountId::parse(account_id)?;
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
                pins,
                manager_journal: bundle
                    .journal
                    .into_iter()
                    .filter(|entry| account_entry(entry, &account))
                    .collect(),
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
        verify_enrollment_offer(&signed, now)?;
        Ok(signed)
    };
    encode(&create().map_err(|error| error.to_string())?)
}

/// Accept this transfer only from the user's existing device, such as a scanned card.
#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = verifyDeviceEnrollmentOffer))]
pub fn verify_device_enrollment_offer(
    offer_json: &str,
    now_ms: f64,
) -> crate::Result<VerifiedEnrollmentOffer> {
    let verify = || -> Result<VerifiedEnrollmentOffer> {
        verify_enrollment_offer(
            &decode_enrollment_offer(offer_json.as_bytes())?,
            time(now_ms)?,
        )
    };
    verify().map_err(|error| error.to_string())
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
    let create = || -> Result<DeviceEnrollmentIntent> {
        let now = time(now_ms)?;
        let checked = verify_enrollment_offer(&offer.signed, now)?;
        let value = &checked.signed.payload;
        let account_pins: Vec<_> = value
            .pins
            .iter()
            .filter(|pin| pin.domain == PolicyDomain::Account(value.account.clone()))
            .cloned()
            .collect();
        let state = replay_policies(&account_pins, &value.manager_journal)?;
        independent_device_key(&state, &value.account, &PublicKey::of(new_device))?;
        Ok(DeviceEnrollmentIntent {
            account: value.account.clone(),
            new_device_public_key: PublicKey::of(new_device),
            requested_rights: value.requested_rights.clone(),
            certificate_not_before_ms: now,
            certificate_expires_at_ms: value.certificate_expires_at_ms,
            target: DeviceId::from_key(&value.entry.node_public_key),
            expected_heads: vec![checked.account_head],
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
    let checked = verify_enrollment_offer(&offer.signed, now)?;
    let value = &checked.signed.payload;
    validate_window(
        intent.certificate_not_before_ms,
        intent.certificate_expires_at_ms,
        now,
    )?;
    require(
        intent.account == value.account
            && intent.new_device_public_key == *expected_new_device
            && intent.requested_rights == value.requested_rights
            && intent.certificate_not_before_ms >= value.issued_at_ms
            && intent.certificate_expires_at_ms <= value.certificate_expires_at_ms
            && intent.target == DeviceId::from_key(&value.entry.node_public_key)
            && intent.expected_heads == [checked.account_head],
        "device enrollment intent differs from verified offer",
    )?;
    let account_pins: Vec<_> = value
        .pins
        .iter()
        .filter(|pin| pin.domain == PolicyDomain::Account(value.account.clone()))
        .cloned()
        .collect();
    let state = replay_policies(&account_pins, &value.manager_journal)?;
    independent_device_key(&state, &intent.account, expected_new_device)?;
    Ok(state)
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = verifyDeviceEnrollmentIntentForOffer))]
pub fn verify_device_enrollment_intent_for_offer(
    offer: &VerifiedEnrollmentOffer,
    intent_json: &str,
    expected_new_device_public_key: &str,
    now_ms: f64,
) -> crate::Result<String> {
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
            bundle: None,
            account_journal: offer.signed.payload.manager_journal.clone(),
        })
    };
    prepare().map_err(|error| error.to_string())
}
