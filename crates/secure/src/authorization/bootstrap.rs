//! Two-phase account creation with a pinned Node and its device possession proof.

pub mod recovery;
use super::*;

pub const NODE_CLAIM_MAX_AGE_MS: u64 = 120_000;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct NodeClaim {
    pub version: u16,
    pub node_public_key: PublicKey,
    pub claim_nonce: Nonce,
    pub issued_at_ms: u64,
    pub expires_at_ms: u64,
}
impl sealed::Sealed for NodeClaim {}
impl SignedObject for NodeClaim {
    const DOMAIN: &'static str = "node-claim-v2";
}

#[derive(Clone, Debug)]
#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen)]
pub struct VerifiedNodeClaim {
    claim: Signed<NodeClaim>,
}

#[derive(Clone, Debug)]
#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen)]
pub struct PreparedAccountCreation {
    claim: VerifiedNodeClaim,
    account_genesis: Signed<AccountDocument>,
    space_genesis: Signed<SpaceDocument>,
    client_certificate: Signed<DeviceCertificate>,
    node_certificate: DeviceCertificate,
    resource_id: ResourceId,
    resource_nonce: Nonce,
    grant_nonce: Nonce,
}

#[derive(Clone, Debug)]
#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen)]
pub struct CreatedAccount {
    bundle: ClientAuthorizationBundle,
    pins: Vec<PolicyPin>,
    authorization: VerifiedClientAuthorization,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct NodePossessionRequest {
    claim: Signed<NodeClaim>,
    account_genesis: Signed<AccountDocument>,
    node_certificate: DeviceCertificate,
}

fn timestamp(value: f64) -> Result<u64> {
    require(
        value.is_finite()
            && value >= 0.0
            && value.fract() == 0.0
            && value <= 9_007_199_254_740_991.0,
        "invalid bootstrap timestamp",
    )?;
    Ok(value as u64)
}

fn encode<T: Serialize>(value: &T) -> crate::Result<String> {
    serde_json::to_string(value).map_err(|_| "bootstrap encoding failed".to_owned())
}

fn validate_claim(
    claim: &Signed<NodeClaim>,
    expected_key: &PublicKey,
    expected_nonce: &Nonce,
    now_ms: u64,
) -> Result<()> {
    let c = &claim.payload;
    require(
        c.version == AUTH_VERSION
            && c.node_public_key == *expected_key
            && claim.issuer == *expected_key
            && c.claim_nonce == *expected_nonce,
        "node claim pin or nonce mismatch",
    )?;
    validate_window(c.issued_at_ms, c.expires_at_ms, now_ms)?;
    require(
        c.expires_at_ms - c.issued_at_ms <= NODE_CLAIM_MAX_AGE_MS,
        "node claim validity too long",
    )?;
    claim.verify_signature()
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = createNodeClaim))]
pub fn create_node_claim(
    node: &Identity,
    claim_nonce: &str,
    now_ms: f64,
    expires_at_ms: f64,
) -> crate::Result<String> {
    let create = || -> Result<Signed<NodeClaim>> {
        let now_ms = timestamp(now_ms)?;
        let nonce = Nonce::parse(claim_nonce)?;
        let claim = Signed::sign(
            NodeClaim {
                version: AUTH_VERSION,
                node_public_key: PublicKey::of(node),
                claim_nonce: nonce.clone(),
                issued_at_ms: now_ms,
                expires_at_ms: timestamp(expires_at_ms)?,
            },
            node,
        )?;
        validate_claim(&claim, &PublicKey::of(node), &nonce, now_ms)?;
        Ok(claim)
    };
    encode(&create().map_err(|e| e.to_string())?)
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = verifyNodeClaim))]
pub fn verify_node_claim(
    claim_json: &str,
    expected_node_public_key: &str,
    expected_claim_nonce: &str,
    now_ms: f64,
) -> crate::Result<VerifiedNodeClaim> {
    let verify = || -> Result<VerifiedNodeClaim> {
        let claim = decode_signed(claim_json.as_bytes())?;
        validate_claim(
            &claim,
            &PublicKey::parse(expected_node_public_key)?,
            &Nonce::parse(expected_claim_nonce)?,
            timestamp(now_ms)?,
        )?;
        Ok(VerifiedNodeClaim { claim })
    };
    verify().map_err(|e| e.to_string())
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = prepareAccountCreation))]
pub fn prepare_account_creation(
    root: &Identity,
    recovery: &Identity,
    client: &Identity,
    claim: &VerifiedNodeClaim,
    certificate_expires_at_ms: f64,
    now_ms: f64,
) -> crate::Result<PreparedAccountCreation> {
    let prepare = || -> Result<PreparedAccountCreation> {
        let now_ms = timestamp(now_ms)?;
        let expires_at_ms = timestamp(certificate_expires_at_ms)?;
        validate_window(now_ms, expires_at_ms, now_ms)?;
        let node_key = &claim.claim.payload.node_public_key;
        validate_claim(
            &claim.claim,
            node_key,
            &claim.claim.payload.claim_nonce,
            now_ms,
        )?;
        let root_key = PublicKey::of(root);
        let recovery_key = PublicKey::of(recovery);
        let client_key = PublicKey::of(client);
        let keys = BTreeSet::from([
            root_key.clone(),
            recovery_key.clone(),
            client_key.clone(),
            node_key.clone(),
        ]);
        require(keys.len() == 4, "bootstrap keys must be independent")?;
        let account = AccountId::from_root(&root_key);
        let space = SpaceId::personal(&account);
        let authority = AuthorityBinding::from_key(node_key.clone());
        let recovery = RecoveryPolicy {
            generation: 0,
            keys: vec![recovery_key],
            threshold: 1,
        };
        let account_genesis = Signed::sign(
            AccountDocument {
                version: AUTH_VERSION,
                account: account.clone(),
                genesis_root: root_key.clone(),
                current_root: root_key.clone(),
                root_generation: 0,
                previous: None,
                authority_epoch: 0,
                sequence: 0,
                authority: authority.clone(),
                recovery: recovery.clone(),
                enrollment: vec![],
                devices: vec![],
                revoked_devices: vec![],
                authenticators: vec![],
                root_acceptance: None,
                recovery_approvals: vec![],
            },
            root,
        )?;
        let mut state = VerifiedPolicySet::new();
        let account_head = state.trust_account_genesis(&root_key, account_genesis.clone())?;
        let certificate = |key: PublicKey, subject, uses, nonce| DeviceCertificate {
            version: AUTH_VERSION,
            owner: PolicyDomain::Account(account.clone()),
            subject,
            device: DeviceId::from_key(&key),
            public_key: key,
            uses,
            possession: None,
            enrollment_grant: None,
            issued_under: account_head.clone(),
            nonce,
            not_before_ms: now_ms,
            expires_at_ms,
        };
        let mut client_certificate = certificate(
            client_key,
            Actor::Account(account.clone()),
            vec![
                DeviceUse::Noise,
                DeviceUse::AccountInvocation,
                DeviceUse::EnrollDevice,
            ],
            Nonce::random()?,
        );
        client_certificate.prove_possession(client)?;
        let client_certificate = Signed::sign(client_certificate, root)?;
        state.install_device(client_certificate.clone(), now_ms)?;
        let node_certificate = certificate(
            node_key.clone(),
            Actor::Service(authority.service.clone()),
            vec![DeviceUse::Noise, DeviceUse::ServiceInvocation],
            claim.claim.payload.claim_nonce.clone(),
        );
        let space_genesis = Signed::sign(
            SpaceDocument {
                version: AUTH_VERSION,
                space,
                root: SpaceRoot::Personal {
                    owner: account,
                    genesis_root: root_key.clone(),
                },
                current_root: root_key.clone(),
                root_generation: 0,
                previous: None,
                authority_epoch: 0,
                sequence: 0,
                authority,
                recovery,
                devices: vec![],
                revoked_devices: vec![],
                memberships: vec![],
                grants: vec![],
                resources: vec![],
                governance_grant: None,
                root_acceptance: None,
                recovery_approvals: vec![],
            },
            root,
        )?;
        state.trust_space_genesis(&root_key, space_genesis.clone())?;
        Ok(PreparedAccountCreation {
            claim: claim.clone(),
            account_genesis,
            space_genesis,
            client_certificate,
            node_certificate,
            resource_id: ResourceId::random()?,
            resource_nonce: Nonce::random()?,
            grant_nonce: Nonce::random()?,
        })
    };
    prepare().map_err(|e| e.to_string())
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen)]
impl PreparedAccountCreation {
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = nodeRequestJson))]
    pub fn node_request_json(&self) -> crate::Result<String> {
        encode(&NodePossessionRequest {
            claim: self.claim.claim.clone(),
            account_genesis: self.account_genesis.clone(),
            node_certificate: self.node_certificate.clone(),
        })
    }
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = signNodePossession))]
pub fn sign_node_possession(
    request_json: &str,
    node: &Identity,
    expected_claim_nonce: &str,
    now_ms: f64,
) -> crate::Result<String> {
    let sign = || -> Result<Signature> {
        require(
            request_json.len() <= MAX_PROOF_BYTES,
            "node request too large",
        )?;
        let request: NodePossessionRequest = serde_json::from_str(request_json)
            .map_err(|_| AuthorizationError::InvalidProof("invalid node possession request"))?;
        let now_ms = timestamp(now_ms)?;
        let node_key = PublicKey::of(node);
        let nonce = Nonce::parse(expected_claim_nonce)?;
        validate_claim(&request.claim, &node_key, &nonce, now_ms)?;
        let genesis = &request.account_genesis.payload;
        let mut state = VerifiedPolicySet::new();
        let head =
            state.trust_account_genesis(&genesis.genesis_root, request.account_genesis.clone())?;
        let c = &request.node_certificate;
        require(
            genesis.authority == AuthorityBinding::from_key(node_key.clone())
                && genesis.genesis_root != node_key
                && !genesis.recovery.keys.contains(&node_key)
                && c.version == AUTH_VERSION
                && c.owner == PolicyDomain::Account(genesis.account.clone())
                && c.subject == Actor::Service(ServiceId::from_key(&node_key))
                && c.device == DeviceId::from_key(&node_key)
                && c.public_key == node_key
                && c.uses == [DeviceUse::Noise, DeviceUse::ServiceInvocation]
                && c.possession.is_none()
                && c.enrollment_grant.is_none()
                && c.issued_under == head
                && c.nonce == nonce
                && c.not_before_ms >= request.claim.payload.issued_at_ms
                && c.not_before_ms < request.claim.payload.expires_at_ms,
            "node possession request changed enrollment scope",
        )?;
        validate_window(c.not_before_ms, c.expires_at_ms, now_ms)?;
        Signature::parse(
            &node
                .sign("device-possession-v2", &c.possession_bytes()?)
                .map_err(|_| AuthorizationError::InvalidProof("node possession signing failed"))?,
        )
    };
    sign().map(|s| s.to_string()).map_err(|e| e.to_string())
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = finishAccountCreation))]
pub fn finish_account_creation(
    prepared: &PreparedAccountCreation,
    root: &Identity,
    node_possession_signature: &str,
    now_ms: f64,
) -> crate::Result<CreatedAccount> {
    let finish = || -> Result<(ClientAuthorizationBundle, Vec<PolicyPin>)> {
        let now_ms = timestamp(now_ms)?;
        let root_key = PublicKey::of(root);
        require(
            root_key == prepared.account_genesis.payload.genesis_root,
            "bootstrap root changed",
        )?;
        validate_claim(
            &prepared.claim.claim,
            &prepared.node_certificate.public_key,
            &prepared.node_certificate.nonce,
            now_ms,
        )?;
        let mut state = VerifiedPolicySet::new();
        let account_head =
            state.trust_account_genesis(&root_key, prepared.account_genesis.clone())?;
        let client_hash = state.install_device(prepared.client_certificate.clone(), now_ms)?;
        let mut node_certificate = prepared.node_certificate.clone();
        node_certificate.possession = Some(Signature::parse(node_possession_signature)?);
        let node_certificate = Signed::sign(node_certificate, root)?;
        let node_hash = state.install_device(node_certificate.clone(), now_ms)?;
        let mut account_document = prepared.account_genesis.payload.clone();
        account_document.previous = Some(account_head);
        account_document.sequence = 1;
        account_document.devices = vec![client_hash, node_hash];
        account_document.devices.sort();
        account_document.enrollment = vec![EnrollmentAuthority {
            device: prepared.client_certificate.payload.device.clone(),
            rights: vec![AccountRight::EnrollDevice, AccountRight::RevokeDevice],
            expires_at_ms: prepared.client_certificate.payload.expires_at_ms,
        }];
        let account_document = Signed::sign(account_document, root)?;
        let account_head = state.install_account(account_document.clone())?;
        let space_head = state.trust_space_genesis(&root_key, prepared.space_genesis.clone())?;
        let space = prepared.space_genesis.payload.space.clone();
        let resource = Signed::sign(
            ResourceRecord {
                version: AUTH_VERSION,
                space: space.clone(),
                resource: prepared.resource_id.clone(),
                kind: ResourceKind::Project,
                host: ServiceId::from_key(&prepared.node_certificate.public_key),
                issued_under: space_head.clone(),
                nonce: prepared.resource_nonce.clone(),
            },
            root,
        )?;
        let resource_hash = state.install_resource(resource.clone())?;
        let grant = Signed::sign(
            Grant {
                version: AUTH_VERSION,
                owner_space: space.clone(),
                scope: ResourceScope::WholeSpace(space.clone()),
                grantee: Grantee::Actor(Actor::Account(
                    prepared.account_genesis.payload.account.clone(),
                )),
                actions: vec![
                    Action::ConversationRead,
                    Action::ConversationCreate,
                    Action::MessageAppend,
                    Action::TaskCreate,
                    Action::TaskProgress,
                    Action::TaskAccept,
                    Action::TaskCancel,
                    Action::ExecutionOpen,
                    Action::ExecutionSend,
                    Action::ExecutionSteer,
                    Action::ExecutionInterrupt,
                    Action::ExecutionRespondApproval,
                    Action::ExecutionRebind,
                    Action::OperationStatusRead,
                    Action::OperationReconcile,
                    Action::OperationResolve,
                    Action::AttachmentRead,
                    Action::AttachmentWrite,
                    Action::Export,
                    Action::NotificationRead,
                    Action::NotificationAck,
                    Action::MemberManage,
                    Action::ResourceManage,
                    Action::GrantIssue,
                    Action::GrantRevoke,
                    Action::ServiceEnroll,
                    Action::ReplicaRead,
                    Action::AgentStart,
                ],
                audience: Audience {
                    service: ServiceId::from_key(&prepared.node_certificate.public_key),
                    device: prepared.node_certificate.device.clone(),
                },
                constraints: GrantConstraints::default(),
                parent: None,
                remaining_delegations: MAX_DELEGATION_DEPTH as u8,
                issued_under: space_head.clone(),
                not_before_ms: prepared.client_certificate.payload.not_before_ms,
                expires_at_ms: prepared.client_certificate.payload.expires_at_ms,
                nonce: prepared.grant_nonce.clone(),
            },
            root,
        )?;
        let grant_leaf = state.install_grant(grant.clone(), now_ms)?;
        let mut space_document = prepared.space_genesis.payload.clone();
        space_document.previous = Some(space_head);
        space_document.sequence = 1;
        space_document.resources = vec![resource_hash];
        space_document.grants = vec![grant_leaf.clone()];
        let space_document = Signed::sign(space_document, root)?;
        let space_head = state.install_space(space_document.clone())?;
        let pins = vec![
            PolicyPin {
                domain: account_head.domain.clone(),
                genesis_root: root_key.clone(),
                minimum_head: Some(account_head),
            },
            PolicyPin {
                domain: space_head.domain.clone(),
                genesis_root: root_key,
                minimum_head: Some(space_head),
            },
        ];
        let bundle = ClientAuthorizationBundle {
            journal: vec![
                JournalEntry::AccountGenesis(prepared.account_genesis.clone()),
                JournalEntry::Device(prepared.client_certificate.clone()),
                JournalEntry::Device(node_certificate),
                JournalEntry::Account(account_document),
                JournalEntry::SpaceGenesis(prepared.space_genesis.clone()),
                JournalEntry::Resource(resource),
                JournalEntry::Grant(grant),
                JournalEntry::Space(space_document),
            ],
            grant_leaf,
        };
        Ok((bundle, pins))
    };
    let (bundle, pins) = finish().map_err(|e| e.to_string())?;
    let authorization = verify_client_authorization(
        &encode(&bundle)?,
        &encode(&pins)?,
        prepared.account_genesis.payload.account.as_str(),
        prepared.space_genesis.payload.space.as_str(),
        prepared.client_certificate.payload.public_key.as_str(),
        now_ms,
    )?;
    Ok(CreatedAccount {
        bundle,
        pins,
        authorization,
    })
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen)]
impl CreatedAccount {
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = bundleJson))]
    pub fn bundle_json(&self) -> crate::Result<String> {
        encode(&self.bundle)
    }
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = pinsJson))]
    pub fn pins_json(&self) -> crate::Result<String> {
        encode(&self.pins)
    }
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = scopeJson))]
    pub fn scope_json(&self) -> crate::Result<String> {
        self.authorization.scope_json()
    }
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = clientGrantJson))]
    pub fn client_grant_json(&self) -> crate::Result<String> {
        let grant = self
            .bundle
            .journal
            .iter()
            .find_map(|entry| match entry {
                JournalEntry::Grant(grant) => Some(grant),
                _ => None,
            })
            .ok_or("created account is missing client grant")?;
        encode(grant)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: f64 = 10_000.0;
    const CLAIM_END: f64 = 100_000.0;
    const CERT_END: f64 = 1_000_000.0;

    fn key(byte: u8) -> Identity {
        Identity::from_secret_hex(&hex::encode([byte; 32])).unwrap()
    }

    fn nonce() -> String {
        Nonce::from_bytes([9; 32]).to_string()
    }

    fn claim(node: &Identity) -> VerifiedNodeClaim {
        let json = create_node_claim(node, &nonce(), NOW, CLAIM_END).unwrap();
        verify_node_claim(&json, &node.public_key_hex(), &nonce(), NOW).unwrap()
    }

    fn prepared() -> PreparedAccountCreation {
        prepare_account_creation(&key(1), &key(2), &key(3), &claim(&key(4)), CERT_END, NOW).unwrap()
    }

    fn possession(prepared: &PreparedAccountCreation) -> String {
        sign_node_possession(
            &prepared.node_request_json().unwrap(),
            &key(4),
            &nonce(),
            NOW,
        )
        .unwrap()
    }

    #[test]
    fn creates_a_verified_owner_client_without_account_powers_for_node() {
        let prepared = prepared();
        let created =
            finish_account_creation(&prepared, &key(1), &possession(&prepared), NOW).unwrap();
        let retry =
            finish_account_creation(&prepared, &key(1), &possession(&prepared), NOW + 1.0).unwrap();
        assert_eq!(created.bundle_json().unwrap(), retry.bundle_json().unwrap());
        assert_eq!(created.pins_json().unwrap(), retry.pins_json().unwrap());
        let scope: ClientScope = serde_json::from_str(&created.scope_json().unwrap()).unwrap();
        assert_eq!(
            scope.account_id,
            AccountId::from_root(&PublicKey::of(&key(1)))
        );
        assert_eq!(scope.space_id, SpaceId::personal(&scope.account_id));
        assert_eq!(scope.device_id, DeviceId::from_key(&PublicKey::of(&key(3))));
        let grant: Signed<Grant> =
            serde_json::from_str(&created.client_grant_json().unwrap()).unwrap();
        assert_eq!(grant.payload.remaining_delegations, 8);
        assert_eq!(grant.payload.expires_at_ms, CERT_END as u64);
        assert_eq!(
            grant.payload.scope,
            ResourceScope::WholeSpace(scope.space_id.clone())
        );
        assert!(grant.payload.actions.contains(&Action::GrantIssue));
        assert!(
            grant
                .payload
                .actions
                .contains(&Action::ExecutionRespondApproval)
        );
        let auth = verify_client_authorization(
            &created.bundle_json().unwrap(),
            &created.pins_json().unwrap(),
            scope.account_id.as_str(),
            scope.space_id.as_str(),
            &key(3).public_key_hex(),
            NOW,
        )
        .unwrap();
        assert_eq!(
            auth.account_enrollment_rights(),
            &[AccountRight::EnrollDevice, AccountRight::RevokeDevice]
        );
        assert!(
            verify_client_authorization(
                &created.bundle_json().unwrap(),
                &created.pins_json().unwrap(),
                scope.account_id.as_str(),
                scope.space_id.as_str(),
                &key(4).public_key_hex(),
                NOW,
            )
            .is_err()
        );
        let state = replay_policies(&created.pins, &created.bundle.journal).unwrap();
        let service = state
            .find_device(&DeviceId::from_key(&PublicKey::of(&key(4))))
            .unwrap();
        assert_eq!(
            service.payload.subject,
            Actor::Service(ServiceId::from_key(&PublicKey::of(&key(4))))
        );
        assert_eq!(
            service.payload.uses,
            [DeviceUse::Noise, DeviceUse::ServiceInvocation]
        );
    }

    #[test]
    fn claim_requires_pinned_node_and_bootstrap_nonce() {
        let json = create_node_claim(&key(4), &nonce(), NOW, CLAIM_END).unwrap();
        assert!(verify_node_claim(&json, &key(5).public_key_hex(), &nonce(), NOW).is_err());
        assert!(
            verify_node_claim(
                &json,
                &key(4).public_key_hex(),
                &Nonce::from_bytes([8; 32]).to_string(),
                NOW
            )
            .is_err()
        );
        let mut claim: Signed<NodeClaim> = serde_json::from_str(&json).unwrap();
        claim.payload.expires_at_ms -= 1;
        assert!(
            verify_node_claim(
                &encode(&claim).unwrap(),
                &key(4).public_key_hex(),
                &nonce(),
                NOW
            )
            .is_err()
        );
    }

    #[test]
    fn claim_and_certificate_windows_are_enforced_at_each_phase() {
        let json = create_node_claim(&key(4), &nonce(), NOW, CLAIM_END).unwrap();
        assert!(verify_node_claim(&json, &key(4).public_key_hex(), &nonce(), NOW - 1.0).is_err());
        assert!(verify_node_claim(&json, &key(4).public_key_hex(), &nonce(), CLAIM_END).is_err());
        assert!(
            create_node_claim(
                &key(4),
                &nonce(),
                NOW,
                NOW + NODE_CLAIM_MAX_AGE_MS as f64 + 1.0
            )
            .is_err()
        );
        assert!(
            prepare_account_creation(&key(1), &key(2), &key(3), &claim(&key(4)), NOW, NOW).is_err()
        );
        let p = prepared();
        assert!(
            sign_node_possession(
                &p.node_request_json().unwrap(),
                &key(4),
                &nonce(),
                CLAIM_END
            )
            .is_err()
        );
        assert!(finish_account_creation(&p, &key(1), &possession(&p), CLAIM_END).is_err());
        let p =
            prepare_account_creation(&key(1), &key(2), &key(3), &claim(&key(4)), NOW + 1.0, NOW)
                .unwrap();
        assert!(finish_account_creation(&p, &key(1), &possession(&p), NOW + 1.0).is_err());
    }

    #[test]
    fn all_four_key_roles_must_be_independent() {
        for keys in [
            [1, 1, 3, 4],
            [1, 2, 1, 4],
            [1, 2, 3, 1],
            [1, 2, 2, 4],
            [1, 2, 3, 2],
            [1, 2, 3, 3],
        ] {
            assert!(
                prepare_account_creation(
                    &key(keys[0]),
                    &key(keys[1]),
                    &key(keys[2]),
                    &claim(&key(keys[3])),
                    CERT_END,
                    NOW,
                )
                .is_err()
            );
        }
    }

    #[test]
    fn node_only_signs_its_exact_service_certificate() {
        let p = prepared();
        let request = p.node_request_json().unwrap();
        assert!(sign_node_possession(&request, &key(5), &nonce(), NOW).is_err());
        assert!(
            sign_node_possession(
                &request,
                &key(4),
                &Nonce::from_bytes([8; 32]).to_string(),
                NOW
            )
            .is_err()
        );
        for mutation in 0..5 {
            let mut request: NodePossessionRequest = serde_json::from_str(&request).unwrap();
            match mutation {
                0 => {
                    request.node_certificate.subject =
                        Actor::Account(request.account_genesis.payload.account.clone())
                }
                1 => request.node_certificate.uses.push(DeviceUse::EnrollDevice),
                2 => request.node_certificate.device = DeviceId::from_key(&PublicKey::of(&key(5))),
                3 => request.node_certificate.issued_under.sequence += 1,
                4 => {
                    request.account_genesis.payload.account =
                        AccountId::from_root(&PublicKey::of(&key(5)))
                }
                _ => unreachable!(),
            }
            assert!(
                sign_node_possession(&encode(&request).unwrap(), &key(4), &nonce(), NOW).is_err()
            );
        }
    }

    #[test]
    fn finish_requires_node_proof_for_unmodified_certificate_and_original_root() {
        let mut p = prepared();
        let proof = possession(&p);
        assert!(finish_account_creation(&p, &key(1), "", NOW).is_err());
        let wrong_proof = key(5)
            .sign(
                "device-possession-v2",
                &p.node_certificate.possession_bytes().unwrap(),
            )
            .unwrap();
        assert!(finish_account_creation(&p, &key(1), &wrong_proof, NOW).is_err());
        assert!(finish_account_creation(&p, &key(5), &proof, NOW).is_err());
        p.node_certificate.expires_at_ms -= 1;
        assert!(finish_account_creation(&p, &key(1), &proof, NOW).is_err());
    }

    #[test]
    fn javascript_timestamps_must_be_exact_nonnegative_integers() {
        for now in [f64::NAN, f64::INFINITY, -1.0, 1.5, 9_007_199_254_740_992.0] {
            assert!(create_node_claim(&key(4), &nonce(), now, CLAIM_END).is_err());
        }
    }
}
