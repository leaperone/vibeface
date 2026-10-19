use super::*;
use crate::{
    crypto::LocalIdentity,
    identity::{Genesis, GovernanceEvent},
    model::NodeRole,
};
use anyhow::Result;
use std::{
    fs,
    os::unix::fs::{PermissionsExt, symlink},
    path::PathBuf,
};

fn fixture() -> Result<(
    PathBuf,
    Store,
    LocalIdentity,
    LocalIdentity,
    LocalIdentity,
    NetworkState,
)> {
    let root_dir = std::env::temp_dir().join(format!("vibeface-identity-tests-{}", random_id()));
    fs::create_dir_all(&root_dir)?;
    fs::set_permissions(&root_dir, fs::Permissions::from_mode(0o700))?;
    let store = Store::open(root_dir.join("node.sqlite3"))?;
    let root = LocalIdentity::generate();
    let node = LocalIdentity::generate();
    let admin = LocalIdentity::generate();
    let peer_admin = LocalIdentity::generate();
    let genesis = Genesis::create(
        &root,
        vec![
            Member::node(&node, "local", vec![NodeRole::Gateway, NodeRole::Replica])?,
            Member::client(&admin, "admin", true)?,
            Member::client(&peer_admin, "peer-admin", true)?,
        ],
    )?;
    let state = NetworkState::new(genesis)?;
    store.init_network(state.clone())?;
    Ok((root_dir, store, admin, peer_admin, node, state))
}

fn invite_request(
    store: &Store,
    admin: &LocalIdentity,
    capability: &str,
    manage: bool,
) -> Result<(InvitePayload, InviteEnvelope)> {
    let view = store.public_state()?.project()?;
    let payload = store.create_invite(InviteRequest {
        request_id: random_id(),
        network_id: view.network_id,
        inviter_subject_id: admin.peer_id()?,
        epoch: view.epoch,
        capability: capability.to_owned(),
        new_kind: IdentityKind::Client,
        node_roles: vec![],
        manage,
        expires_at_ms: None,
    })?;
    let signature = admin.sign_payload("identity-invite", &payload.signing_bytes()?)?;
    Ok((
        payload.clone(),
        InviteEnvelope {
            payload,
            inviter_signature: signature,
        },
    ))
}

#[test]
fn bootstrap_capability_is_private_one_time_and_requires_the_named_manager() -> Result<()> {
    let root_dir = std::env::temp_dir().join(format!("vibeface-bootstrap-{}", random_id()));
    fs::create_dir_all(&root_dir)?;
    fs::set_permissions(&root_dir, fs::Permissions::from_mode(0o700))?;
    let store = Store::open(root_dir.join("node.sqlite3"))?;
    let cap = store.bootstrap_capability()?;
    assert!(store.bootstrap_capability().is_err());
    let root = LocalIdentity::generate();
    let admin = LocalIdentity::generate();
    let state = NetworkState::new(Genesis::create(
        &root,
        vec![Member::client(&admin, "admin", true)?],
    )?)?;
    assert!(
        store
            .bootstrap_network(state.clone(), &cap.capability, &root.peer_id()?)
            .is_err()
    );
    assert!(
        store
            .bootstrap_network(state.clone(), "00".repeat(32).as_str(), &admin.peer_id()?)
            .is_err()
    );
    store.bootstrap_network(state, &cap.capability, &admin.peer_id()?)?;
    assert!(
        store
            .bootstrap_network(
                NetworkState::new(Genesis::create(
                    &root,
                    vec![Member::client(&admin, "admin", true)?]
                )?)?,
                &cap.capability,
                &admin.peer_id()?
            )
            .is_err()
    );
    Ok(())
}

#[test]
fn invite_pair_review_and_grant_are_exact_and_capability_is_not_a_hash() -> Result<()> {
    let (_dir, mut store, admin, _peer, _root, _state) = fixture()?;
    let new_client = LocalIdentity::generate();
    let capability = random_hex(32);
    let (payload, envelope) = invite_request(&store, &admin, &capability, false)?;
    store.commit_invite(envelope)?;
    let member = Member::client(&new_client, "new", false)?;
    let request = PairRequest {
        request_id: random_id(),
        invite_id: payload.invite_id.clone(),
        capability: capability.clone(),
        nonce: random_hex(32),
        proof: String::new(),
        member: member.clone(),
    };
    let mut signed = request.clone();
    signed.proof = new_client.sign_payload("identity-pair", &request.signing_bytes()?)?;
    assert!(
        store
            .pair_request(PairRequest {
                capability: payload.capability_hash.clone(),
                ..signed.clone()
            })
            .is_err()
    );
    let review = store.pair_request(signed)?;
    let new_signature =
        new_client.sign_payload("identity-pair-review", &review.signing_bytes()?)?;
    assert!(
        store
            .accept_pair(review.clone(), new_signature.clone())
            .is_ok()
    );
    assert!(!store.pending_pairs(&admin.peer_id()?)?.is_empty());
    let inviter_signature = admin.sign_payload("identity-pair-review", &review.signing_bytes()?)?;
    let proposal = store.confirm_pair(PairConfirm {
        review: review.clone(),
        inviter_signature,
        new_subject_signature: new_signature,
    })?;
    let view = store.public_state()?.project()?;
    let event = GovernanceEvent::grant(&admin, &view, member)?;
    let final_state = store.store_signed_grant(event)?;
    assert!(
        final_state
            .project()?
            .authorize_client(&review.new_member.subject_id)
            .is_ok()
    );
    assert!(
        store
            .pair_status(
                &payload.invite_id,
                &capability,
                &review.new_member.subject_id
            )?
            .is_some()
    );
    assert!(
        store
            .confirm_pair(PairConfirm {
                review: proposal.review,
                inviter_signature: proposal.inviter_signature,
                new_subject_signature: proposal.new_subject_signature
            })
            .is_ok()
    );
    Ok(())
}

#[test]
fn management_grants_and_private_path_boundaries_are_rejected() -> Result<()> {
    let (dir, store, admin, _peer, _root, _state) = fixture()?;
    let cap = random_hex(32);
    assert!(invite_request(&store, &admin, &cap, true).is_err());
    let public_dir = std::env::temp_dir().join(format!("vibeface-public-{}", random_id()));
    fs::create_dir_all(&public_dir)?;
    fs::set_permissions(&public_dir, fs::Permissions::from_mode(0o755))?;
    assert!(Store::open(public_dir.join("node.sqlite3")).is_err());
    let link = dir.join("link.sqlite3");
    symlink(dir.join("node.sqlite3"), &link)?;
    assert!(Store::open(&link).is_err());
    let directory = dir.join("directory.sqlite3");
    fs::create_dir(&directory)?;
    assert!(Store::open(&directory).is_err());
    let parent_link = dir.join("linked-directory");
    symlink(&dir, &parent_link)?;
    assert!(Store::open(parent_link.join("node.sqlite3")).is_err());
    let sidecar_link = dir.join("unsafe.sqlite3-wal");
    symlink(dir.join("node.sqlite3"), sidecar_link)?;
    assert!(Store::open(dir.join("unsafe.sqlite3")).is_err());
    Ok(())
}

#[test]
fn pair_review_tamper_expiry_and_request_identity_reuse_are_rejected() -> Result<()> {
    let (_dir, store, admin, _peer, _root, _state) = fixture()?;
    let capability = random_hex(32);
    let (payload, envelope) = invite_request(&store, &admin, &capability, false)?;
    store.commit_invite(envelope)?;
    let first = LocalIdentity::generate();
    let member = Member::client(&first, "first", false)?;
    let request_id = random_id();
    let request = PairRequest {
        request_id: request_id.clone(),
        invite_id: payload.invite_id.clone(),
        capability: capability.clone(),
        member: member.clone(),
        nonce: random_hex(32),
        proof: String::new(),
    };
    let mut signed = request.clone();
    signed.proof = first.sign_payload("identity-pair", &request.signing_bytes()?)?;
    let review = store.pair_request(signed)?;
    let mut tampered = review.clone();
    tampered.new_member.name = "tampered".into();
    let signature = first.sign_payload("identity-pair-review", &review.signing_bytes()?)?;
    assert!(store.accept_pair(tampered, signature.clone()).is_err());
    let mut expired = review.clone();
    expired.expires_at_ms = 0;
    assert!(store.accept_pair(expired, signature).is_err());
    let second = LocalIdentity::generate();
    let member2 = Member::client(&second, "second", false)?;
    let request2 = PairRequest {
        request_id,
        invite_id: payload.invite_id,
        capability,
        member: member2.clone(),
        nonce: random_hex(32),
        proof: String::new(),
    };
    let mut signed2 = request2.clone();
    signed2.proof = second.sign_payload("identity-pair", &request2.signing_bytes()?)?;
    assert!(store.pair_request(signed2).is_err());
    Ok(())
}

#[test]
fn two_invites_have_independent_pair_and_grant_records() -> Result<()> {
    let (_dir, mut store, admin, _peer, _root, _state) = fixture()?;
    for label in ["one", "two"] {
        let client = LocalIdentity::generate();
        let capability = random_hex(32);
        let (payload, envelope) = invite_request(&store, &admin, &capability, false)?;
        store.commit_invite(envelope)?;
        let member = Member::client(&client, label, false)?;
        let request = PairRequest {
            request_id: random_id(),
            invite_id: payload.invite_id.clone(),
            capability: capability.clone(),
            member: member.clone(),
            nonce: random_hex(32),
            proof: String::new(),
        };
        let mut signed = request.clone();
        signed.proof = client.sign_payload("identity-pair", &request.signing_bytes()?)?;
        let review = store.pair_request(signed)?;
        let new_signature =
            client.sign_payload("identity-pair-review", &review.signing_bytes()?)?;
        store.accept_pair(review.clone(), new_signature.clone())?;
        let inviter_signature =
            admin.sign_payload("identity-pair-review", &review.signing_bytes()?)?;
        store.confirm_pair(PairConfirm {
            review: review.clone(),
            inviter_signature,
            new_subject_signature: new_signature,
        })?;
        let view = store.public_state()?.project()?;
        let event = GovernanceEvent::grant(&admin, &view, member)?;
        store.store_signed_grant(event)?;
        assert!(
            store
                .pair_status(
                    &payload.invite_id,
                    &capability,
                    &review.new_member.subject_id
                )?
                .is_some()
        );
    }
    Ok(())
}
