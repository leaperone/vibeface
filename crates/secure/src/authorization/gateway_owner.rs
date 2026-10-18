//! Current account owners approve a gateway route with their private root material.

use super::{
    bootstrap::recovery::{RecoveryMaterial, material_state, recovery_material_usage},
    client_policy_import::{
        bind_client_policy_source_node, confirmed_client_policy_source_node,
        record_confirmed_client_policy_metadata, require_confirmed_client_policy,
    },
    execution_handoff::ExecutionProofUsage,
    gateway_forward::{GatewayRouteApproval, verify_gateway_route_approval},
    *,
};
use crate::signature_verification::{ScopedSignatureVerification, SignatureVerification};
use serde::de::DeserializeOwned;

fn parse<T: DeserializeOwned>(json: &str) -> Result<T> {
    require(
        json.len() <= MAX_PROOF_BYTES,
        "gateway owner JSON exceeds byte limit",
    )?;
    serde_json::from_str(json)
        .map_err(|_| AuthorizationError::InvalidProof("invalid gateway owner JSON"))
}

fn material_root(
    material: &RecoveryMaterial,
    usage: &mut ExecutionProofUsage,
) -> Result<(Identity, RecoveryPolicy)> {
    require(
        material.format == "vibeface-account-recovery"
            && material.version == AUTH_VERSION
            && lower_hex(&material.root_secret_hex, 32)
            && lower_hex(&material.recovery_secret_hex, 32)
            && material.bundle.journal.len() <= 128
            && material.scope.space_id == SpaceId::personal(&material.scope.account_id)
            && material.pins.len() == 2
            && material
                .pins
                .windows(2)
                .all(|pins| pins[0].domain < pins[1].domain)
            && material.entry.endpoint.len() <= 4096
            && (material.entry.endpoint.starts_with("http://")
                || material.entry.endpoint.starts_with("https://"))
            && !material
                .entry
                .endpoint
                .bytes()
                .any(|b| b.is_ascii_control() || b.is_ascii_whitespace())
            && !material.entry.endpoint.contains(['#', '@'])
            && material
                .entry
                .endpoint
                .split_once("://")
                .is_some_and(|(_, tail)| !tail.is_empty() && !tail.starts_with('/'))
            && material
                .pins
                .iter()
                .any(|pin| pin.domain == PolicyDomain::Account(material.scope.account_id.clone()))
            && material
                .pins
                .iter()
                .any(|pin| pin.domain == PolicyDomain::Space(material.scope.space_id.clone())),
        "invalid gateway owner recovery material",
    )?;
    usage.merge(&recovery_material_usage(material)?);
    usage.enforce_limits()?;
    let root = Identity::from_secret_hex(&material.root_secret_hex)
        .map_err(|_| AuthorizationError::InvalidProof("invalid gateway owner root"))?;
    let recovery = Identity::from_secret_hex(&material.recovery_secret_hex)
        .map_err(|_| AuthorizationError::InvalidProof("invalid gateway owner recovery key"))?;
    let state = material_state(material)?;
    let account = state
        .account(&material.scope.account_id)
        .ok_or(AuthorizationError::Revoked)?;
    let personal = state
        .space(&material.scope.space_id)
        .ok_or(AuthorizationError::Revoked)?;
    require(
        account.current_root == PublicKey::of(&root)
            && personal.current_root == PublicKey::of(&root)
            && account.recovery.threshold == 1
            && account.recovery.keys == [PublicKey::of(&recovery)]
            && personal.recovery == account.recovery,
        "gateway owner material has a different root or recovery policy",
    )?;
    let recovery_policy = account.recovery.clone();
    let certificate = &state.find_device(&material.scope.device_id)?.payload;
    let at = certificate.not_before_ms;
    let saved_key = certificate.public_key.clone();
    let saved = verify_client_policy_state(
        state,
        &material.bundle,
        &material.pins,
        material.scope.account_id.clone(),
        material.scope.space_id.clone(),
        saved_key,
        at,
    )?;
    require(
        saved.scope == material.scope
            && saved.audience_public_key == material.entry.node_public_key,
        "gateway owner material has a different saved client or entry",
    )?;
    usage.record_budget(&saved.budget)?;
    Ok((root, recovery_policy))
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = prepareGatewayOwnerRoute))]
pub fn prepare_gateway_owner_route(
    material_json: &str,
    owner: &VerifiedClientAuthorization,
    route_payload_json: &str,
    current_identity: &Identity,
    now_ms: f64,
) -> crate::Result<String> {
    let _signature_scope = ScopedSignatureVerification::new();
    let prepare = || -> Result<String> {
        let now = bootstrap::timestamp(now_ms)?;
        let material: RecoveryMaterial = parse(material_json)?;
        let route: GatewayRouteApproval = parse(route_payload_json)?;
        let original = require_confirmed_client_policy(owner)?;
        let signer = PublicKey::of(current_identity);
        require(
            material.scope.account_id == owner.scope.account_id
                && owner.scope.space_id == SpaceId::personal(&owner.scope.account_id)
                && owner.scope.device_id == DeviceId::from_key(&signer)
                && *confirmed_client_policy_source_node(owner)? == route.gateway
                && owner.resources.len() == 1,
            "gateway approval requires this owner's confirmed PersonalSpace client on the gateway",
        )?;
        let snapshot = original.with_independent_pins(&material.pins)?;
        let mut usage = ExecutionProofUsage::default();
        usage.record_metadata(&material_json)?;
        usage.record_metadata(&route_payload_json)?;
        usage.enforce_limits()?;
        let (root, recovery) = material_root(&material, &mut usage)?;
        let state = snapshot.state();
        let account = state
            .account(&owner.scope.account_id)
            .ok_or(AuthorizationError::Revoked)?;
        let personal = state
            .space(&owner.scope.space_id)
            .ok_or(AuthorizationError::Revoked)?;
        require(
            account.current_root == PublicKey::of(&root)
                && personal.current_root == PublicKey::of(&root)
                && account.recovery == recovery
                && personal.recovery == recovery,
            "gateway approval requires the current account root and recovery policy",
        )?;
        let resource = &owner.resources[0].payload;
        let mut current = verify_client_policy_state_selected(
            state.clone(),
            &ClientAuthorizationBundle {
                journal: Vec::new(),
                grant_leaf: owner.grant_leaf.clone(),
            },
            snapshot.pins(),
            owner.scope.account_id.clone(),
            owner.scope.space_id.clone(),
            signer,
            Some(&ResourceRef {
                space: resource.space.clone(),
                resource: resource.resource.clone(),
            }),
            now,
        )?;
        bind_client_policy_source_node(&mut current, &route.gateway, now)?;
        require(
            current.scope == owner.scope
                && current.heads == owner.heads
                && current.audience == owner.audience
                && current.audience_public_key == owner.audience_public_key,
            "gateway approval requires explicit current owner policy synchronization",
        )?;
        require(
            route
                .source_pins
                .iter()
                .any(|pin| pin.domain == PolicyDomain::Account(route.client_account.clone()))
                && route
                    .source_pins
                    .iter()
                    .any(|pin| pin.domain == PolicyDomain::Space(route.resource.space.clone())),
            "gateway route is missing its client account or resource space pin",
        )?;
        let approval = Signed::sign(route, &root)?;
        verify_gateway_route_approval(
            state,
            &owner.scope.account_id,
            &approval.payload.gateway,
            &approval,
            approval.payload.generation,
            &approval.payload.source_pins,
            now,
        )?;
        usage.record_budget(&current.budget)?;
        record_confirmed_client_policy_metadata(owner, &mut usage)?;
        usage.record_signed(&approval)?;
        if let Some(counts) = SignatureVerification::current_counts() {
            usage.record_actual_signature_counts(counts);
        }
        usage.enforce_limits()?;
        let encoded = serde_json::to_string(&approval)
            .map_err(|_| AuthorizationError::InvalidProof("gateway approval encoding failed"))?;
        require(
            encoded.len() <= MAX_PROOF_BYTES,
            "gateway approval exceeds byte limit",
        )?;
        Ok(encoded)
    };
    prepare().map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::authorization::{
        bootstrap::*, gateway_forward::GatewayRouteState,
        remote_execution_policy::tests::confirmed_source,
    };

    const NOW: u64 = 10_000;
    const END: u64 = 1_000_000;

    fn key(seed: u8) -> Identity {
        Identity::from_secret_hex(&hex::encode([seed; 32])).unwrap()
    }

    fn wire<T: Serialize>(value: &T) -> String {
        serde_json::to_string(value).unwrap()
    }

    struct Owner {
        root: Identity,
        client: Identity,
        node: Identity,
        material: String,
        bundle: ClientAuthorizationBundle,
        pins: Vec<PolicyPin>,
        resource: ResourceRef,
    }

    impl Owner {
        fn new(seed: u8) -> Self {
            let root = key(seed);
            let recovery = key(seed + 1);
            let client = key(seed + 2);
            let node = key(seed + 3);
            let nonce = Nonce::from_bytes([seed; 32]).to_string();
            let claim = verify_node_claim(
                &create_node_claim(&node, &nonce, NOW as f64, (NOW + 60_000) as f64).unwrap(),
                &node.public_key_hex(),
                &nonce,
                NOW as f64,
            )
            .unwrap();
            let prepared =
                prepare_account_creation(&root, &recovery, &client, &claim, END as f64, NOW as f64)
                    .unwrap();
            let possession = sign_node_possession(
                &prepared.node_request_json().unwrap(),
                &node,
                &nonce,
                NOW as f64,
            )
            .unwrap();
            let created =
                finish_account_creation(&prepared, &root, &possession, NOW as f64).unwrap();
            let material = recovery::build_account_recovery_material(
                &created,
                &root,
                &recovery,
                "https://gateway.example",
                NOW as f64,
            )
            .unwrap();
            let bundle: ClientAuthorizationBundle =
                serde_json::from_str(&created.bundle_json().unwrap()).unwrap();
            let pins = serde_json::from_str(&created.pins_json().unwrap()).unwrap();
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
            Self {
                root,
                client,
                node,
                material,
                bundle,
                pins,
                resource,
            }
        }

        fn authorization(&self) -> VerifiedClientAuthorization {
            confirmed_source(
                &self.bundle,
                &self.pins,
                &self.client,
                &self.node,
                &self.resource,
                NOW,
            )
            .unwrap()
        }

        fn route(&self, target: &Self) -> GatewayRouteApproval {
            GatewayRouteApproval {
                version: AUTH_VERSION,
                route_id: Nonce::from_bytes([90; 32]),
                generation: 1,
                gateway: PublicKey::of(&self.node),
                client_account: AccountId::from_root(&PublicKey::of(&target.root)),
                client_key: PublicKey::of(&target.client),
                target_key: PublicKey::of(&target.node),
                resource: target.resource.clone(),
                allowed_actions: vec![Action::MessageAppend, Action::OperationStatusRead],
                source_pins: target.pins.clone(),
                state: GatewayRouteState::Active,
                issued_at_ms: NOW,
                expires_at_ms: NOW + 60_000,
            }
        }
    }

    #[test]
    fn current_owner_signs_active_and_revoked_routes_and_retries_identically() {
        let owner = Owner::new(20);
        let target = Owner::new(40);
        let current = owner.authorization();
        let mut route = owner.route(&target);
        let prepare = |route: &GatewayRouteApproval| {
            prepare_gateway_owner_route(
                &owner.material,
                &current,
                &wire(route),
                &owner.client,
                NOW as f64,
            )
            .unwrap()
        };
        let first = prepare(&route);
        assert_eq!(first, prepare(&route));
        let signed: Signed<GatewayRouteApproval> = serde_json::from_str(&first).unwrap();
        signed.verify_signature().unwrap();
        assert_eq!(signed.issuer, PublicKey::of(&owner.root));
        assert_eq!(signed.payload, route);
        assert!(!first.contains(&owner.root.secret_hex()));
        assert!(!first.contains(&key(21).secret_hex()));
        route.generation += 1;
        route.state = GatewayRouteState::Revoked;
        let revoked: Signed<GatewayRouteApproval> = serde_json::from_str(&prepare(&route)).unwrap();
        revoked.verify_signature().unwrap();
        assert_eq!(revoked.payload, route);
        assert_eq!(revoked.issuer, signed.issuer);
        assert_eq!(revoked.payload.route_id, signed.payload.route_id);
    }

    #[test]
    fn owner_approval_rejects_wrong_root_account_device_target_and_window() {
        let owner = Owner::new(20);
        let target = Owner::new(40);
        let current = owner.authorization();
        let route = owner.route(&target);
        let prepare = |material: &str,
                       auth: &VerifiedClientAuthorization,
                       route: &GatewayRouteApproval,
                       client: &Identity,
                       now| {
            prepare_gateway_owner_route(material, auth, &wire(route), client, now)
        };
        let mut material: serde_json::Value = serde_json::from_str(&owner.material).unwrap();
        for replacement in [key(90).secret_hex(), owner.client.secret_hex()] {
            material["root_secret_hex"] = replacement.into();
            assert!(
                prepare(
                    &wire(&material),
                    &current,
                    &route,
                    &owner.client,
                    NOW as f64
                )
                .is_err()
            );
        }
        assert!(
            prepare(
                &target.material,
                &current,
                &route,
                &owner.client,
                NOW as f64
            )
            .is_err()
        );
        assert!(
            prepare(
                &owner.material,
                &current,
                &route,
                &target.client,
                NOW as f64
            )
            .is_err()
        );
        let mut unconfirmed = current.clone();
        unconfirmed.client_policy_context = None;
        assert!(
            prepare(
                &owner.material,
                &unconfirmed,
                &route,
                &owner.client,
                NOW as f64
            )
            .is_err()
        );
        let mut wrong_target = route.clone();
        wrong_target.gateway = PublicKey::of(&target.node);
        assert!(
            prepare(
                &owner.material,
                &current,
                &wrong_target,
                &owner.client,
                NOW as f64
            )
            .is_err()
        );
        let mut expired = route.clone();
        expired.expires_at_ms = NOW;
        assert!(
            prepare(
                &owner.material,
                &current,
                &expired,
                &owner.client,
                NOW as f64
            )
            .is_err()
        );
        let mut invalid = route.clone();
        invalid.allowed_actions.reverse();
        assert!(
            prepare(
                &owner.material,
                &current,
                &invalid,
                &owner.client,
                NOW as f64
            )
            .is_err()
        );
        invalid = route.clone();
        invalid.generation = 0;
        assert!(
            prepare(
                &owner.material,
                &current,
                &invalid,
                &owner.client,
                NOW as f64
            )
            .is_err()
        );
        invalid = route.clone();
        invalid.target_key = invalid.client_key.clone();
        assert!(
            prepare(
                &owner.material,
                &current,
                &invalid,
                &owner.client,
                NOW as f64
            )
            .is_err()
        );
        invalid = route.clone();
        invalid.resource.space = owner.resource.space.clone();
        assert!(
            prepare(
                &owner.material,
                &current,
                &invalid,
                &owner.client,
                NOW as f64
            )
            .is_err()
        );
        assert!(prepare(&owner.material, &current, &route, &owner.client, f64::NAN).is_err());
        assert!(prepare(&owner.material, &current, &route, &owner.client, END as f64).is_err());
    }

    #[test]
    fn older_backup_can_approve_from_a_new_current_device_and_grant() {
        let mut owner = Owner::new(20);
        let target = Owner::new(40);
        let mut state = replay_policies(&owner.pins, &owner.bundle.journal).unwrap();
        let account = AccountId::from_root(&PublicKey::of(&owner.root));
        let next_client = key(70);
        let head = state
            .head(&PolicyDomain::Account(account.clone()))
            .unwrap()
            .clone();
        let mut certificate = DeviceCertificate {
            version: AUTH_VERSION,
            owner: PolicyDomain::Account(account.clone()),
            subject: Actor::Account(account.clone()),
            device: DeviceId::from_key(&PublicKey::of(&next_client)),
            public_key: PublicKey::of(&next_client),
            uses: vec![DeviceUse::Noise, DeviceUse::AccountInvocation],
            possession: None,
            enrollment_grant: None,
            issued_under: head.clone(),
            nonce: Nonce::from_bytes([71; 32]),
            not_before_ms: NOW,
            expires_at_ms: END,
        };
        certificate.prove_possession(&next_client).unwrap();
        let certificate = Signed::sign(certificate, &owner.root).unwrap();
        let hash = state.install_device(certificate.clone(), NOW).unwrap();
        let mut document = state.account(&account).unwrap().clone();
        document.previous = Some(head);
        document.sequence += 1;
        document.devices.push(hash);
        document.devices.sort();
        let document = Signed::sign(document, &owner.root).unwrap();
        state.install_account(document.clone()).unwrap();
        owner.bundle.journal.extend([
            JournalEntry::Device(certificate),
            JournalEntry::Account(document),
        ]);
        let head = state
            .head(&PolicyDomain::Space(owner.resource.space.clone()))
            .unwrap()
            .clone();
        let mut grant = state
            .grant(&owner.bundle.grant_leaf)
            .unwrap()
            .payload
            .clone();
        grant.issued_under = head.clone();
        grant.nonce = Nonce::from_bytes([72; 32]);
        let grant = Signed::sign(grant, &owner.root).unwrap();
        let hash = state.install_grant(grant.clone(), NOW).unwrap();
        let mut document = state.space(&owner.resource.space).unwrap().clone();
        document.previous = Some(head);
        document.sequence += 1;
        document.grants.push(hash.clone());
        document.grants.sort();
        let document = Signed::sign(document, &owner.root).unwrap();
        state.install_space(document.clone()).unwrap();
        owner
            .bundle
            .journal
            .extend([JournalEntry::Grant(grant), JournalEntry::Space(document)]);
        owner.bundle.grant_leaf = hash;
        for pin in &mut owner.pins {
            pin.minimum_head = state.head(&pin.domain).cloned();
        }
        owner.client = next_client;
        let current = owner.authorization();
        let saved: RecoveryMaterial = serde_json::from_str(&owner.material).unwrap();
        assert_ne!(saved.scope.device_id, current.scope.device_id);
        assert_ne!(saved.bundle.grant_leaf, current.grant_leaf);
        let result = prepare_gateway_owner_route(
            &owner.material,
            &current,
            &wire(&owner.route(&target)),
            &owner.client,
            NOW as f64,
        )
        .unwrap();
        let approved: Signed<GatewayRouteApproval> = serde_json::from_str(&result).unwrap();
        approved.verify_signature().unwrap();
        assert_eq!(approved.issuer, PublicKey::of(&owner.root));
    }

    #[test]
    fn saved_root_cannot_approve_after_current_account_root_rotation() {
        let mut owner = Owner::new(20);
        let target = Owner::new(40);
        let next_root = key(75);
        let mut state = replay_policies(&owner.pins, &owner.bundle.journal).unwrap();
        let account = AccountId::from_root(&PublicKey::of(&owner.root));
        let mut document = state.account(&account).unwrap().clone();
        document.previous = state.head(&PolicyDomain::Account(account)).cloned();
        document.sequence += 1;
        document.root_generation += 1;
        document.current_root = PublicKey::of(&next_root);
        document.accept_root(&next_root).unwrap();
        let document = Signed::sign(document, &owner.root).unwrap();
        state.install_account(document.clone()).unwrap();
        owner.bundle.journal.push(JournalEntry::Account(document));
        let mut document = state.space(&owner.resource.space).unwrap().clone();
        document.previous = state
            .head(&PolicyDomain::Space(owner.resource.space.clone()))
            .cloned();
        document.sequence += 1;
        document.root_generation += 1;
        document.current_root = PublicKey::of(&next_root);
        let document = Signed::sign(document, &next_root).unwrap();
        state.install_space(document.clone()).unwrap();
        owner.bundle.journal.push(JournalEntry::Space(document));
        for pin in &mut owner.pins {
            pin.minimum_head = state.head(&pin.domain).cloned();
        }
        let error = prepare_gateway_owner_route(
            &owner.material,
            &owner.authorization(),
            &wire(&owner.route(&target)),
            &owner.client,
            NOW as f64,
        )
        .unwrap_err();
        assert!(error.contains("current account root and recovery policy"));
    }

    #[test]
    fn owner_input_limits_apply_before_json_decode() {
        let owner = Owner::new(20);
        let target = Owner::new(40);
        let current = owner.authorization();
        let oversized = " ".repeat(MAX_PROOF_BYTES + 1);
        assert!(
            prepare_gateway_owner_route(
                &oversized,
                &current,
                &wire(&owner.route(&target)),
                &owner.client,
                NOW as f64,
            )
            .is_err()
        );
        assert!(
            prepare_gateway_owner_route(
                &owner.material,
                &current,
                &oversized,
                &owner.client,
                NOW as f64,
            )
            .is_err()
        );
        let padded_material = format!("{}{}", owner.material, " ".repeat(32_768));
        let padded_route = format!("{}{}", wire(&owner.route(&target)), " ".repeat(32_768));
        assert!(padded_material.len() < MAX_PROOF_BYTES);
        assert!(padded_route.len() < MAX_PROOF_BYTES);
        assert!(
            prepare_gateway_owner_route(
                &padded_material,
                &current,
                &padded_route,
                &owner.client,
                NOW as f64,
            )
            .is_err()
        );
    }
}
