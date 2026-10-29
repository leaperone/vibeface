use super::*;
use crate::{
    crypto::LocalIdentity,
    identity::{Genesis, Member},
};
use std::{fs, os::unix::fs::PermissionsExt, path::PathBuf};
use vibeface_secure::join::{INVITE_DOMAIN, invite_hash};

struct Fixture {
    dir: PathBuf,
    store: Store,
    admin: LocalIdentity,
    network: String,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

fn fixture() -> Result<Fixture> {
    let dir = std::env::temp_dir().join(format!("vibeface-identity-tests-{}", random_hex(8)));
    fs::create_dir_all(&dir)?;
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o700))?;
    let store = Store::open(dir.join("node.sqlite3"))?;
    let root = LocalIdentity::generate();
    let admin = LocalIdentity::generate();
    let state = NetworkState::new(Genesis::create(
        &root,
        Member::client(&admin, "admin", true)?,
        Member::node(&LocalIdentity::generate(), "local")?,
    )?)?;
    store.init_network(state.clone())?;
    Ok(Fixture {
        dir,
        store,
        admin,
        network: state.genesis.network_id,
    })
}

fn invite(fixture: &Fixture, secret: [u8; 32]) -> Result<String> {
    let mut record = InviteRecord {
        network_id: fixture.network.clone(),
        invite_hash: invite_hash(&secret),
        issuer: fixture.admin.device_id()?,
        expires_at_ms: now_ms() + 60_000,
        signature: String::new(),
    };
    record.signature = fixture
        .admin
        .sign_payload(INVITE_DOMAIN, &record.signing_payload()?)?;
    let id = fixture.store.register_invite(&record, &record.issuer)?;
    assert_eq!(fixture.store.register_invite(&record, &record.issuer)?, id);
    Ok(id)
}

fn request(device: &LocalIdentity, secret: [u8; 32], nonce: &str) -> Result<Join> {
    Ok(Join {
        secret: hex::encode(secret),
        public_key: device.device_id()?,
        kind: IdentityKind::Client,
        name: "phone".into(),
        commit: join_commit(&device.device_id()?, nonce)?,
    })
}

#[test]
fn commitment_short_code_flow_admits_exactly_the_requested_device() -> Result<()> {
    let fixture = fixture()?;
    let admin = fixture.admin.device_id()?;
    let joiner = LocalIdentity::generate();
    let device = joiner.device_id()?;
    let nonce = random_hex(32);
    let join_id = invite(&fixture, [1; 32])?;
    let join = request(&joiner, [1; 32], &nonce)?;
    let pending = fixture.store.join(&join, &device)?;
    assert_eq!(
        (pending.join_id.as_str(), pending.status),
        (join_id.as_str(), JoinStatus::Pending)
    );
    assert_eq!(fixture.store.join(&join, &device)?, pending);
    let other = LocalIdentity::generate();
    assert!(
        fixture
            .store
            .join(&request(&other, [1; 32], &nonce)?, &other.device_id()?)
            .is_err()
    );
    assert!(fixture.store.join(&join, &other.device_id()?).is_err());
    assert!(
        fixture
            .store
            .join_status(&join_id, &device, Some(&nonce))
            .is_err()
    );

    let admin_nonce = random_hex(32);
    let confirming = fixture
        .store
        .approve_nonce(&join_id, &admin, &admin_nonce)?;
    assert_eq!(confirming.status, JoinStatus::Confirming);
    assert_eq!(
        fixture
            .store
            .approve_nonce(&join_id, &admin, &admin_nonce)?,
        confirming
    );
    assert!(
        fixture
            .store
            .approve_nonce(&join_id, &admin, &random_hex(32))
            .is_err()
    );
    assert_eq!(fixture.store.pending_joins(&admin)?.len(), 1);
    assert!(fixture.store.pending_joins(&device).is_err());

    let view = fixture.store.public_state()?.project()?;
    let grant = GovernanceEvent::grant(
        &fixture.admin,
        &view,
        Member::client(&joiner, "phone", true)?,
    )?;
    assert!(
        fixture
            .store
            .approve_grant(&join_id, &admin, grant.clone())
            .is_err(),
        "nonce not revealed"
    );
    let revealed = fixture.store.join_status(&join_id, &device, Some(&nonce))?;
    assert_eq!(revealed.joiner_nonce.as_deref(), Some(nonce.as_str()));
    assert!(
        fixture
            .store
            .join_status(&join_id, &device, Some(&random_hex(32)))
            .is_err()
    );
    let renamed = GovernanceEvent::grant(
        &fixture.admin,
        &view,
        Member::client(&joiner, "other", false)?,
    )?;
    assert!(
        fixture
            .store
            .approve_grant(&join_id, &admin, renamed)
            .is_err()
    );
    let (joined, state) = fixture
        .store
        .approve_grant(&join_id, &admin, grant.clone())?;
    assert_eq!(joined.status, JoinStatus::Joined);
    assert!(state.project()?.manager(&device).is_ok());
    assert_eq!(
        fixture.store.approve_grant(&join_id, &admin, grant)?.0,
        joined
    );
    assert!(fixture.store.reject_join(&join_id, &admin).is_err());
    Ok(())
}

#[test]
fn a_wrong_reveal_or_a_rejection_voids_the_request() -> Result<()> {
    let fixture = fixture()?;
    let admin = fixture.admin.device_id()?;
    let joiner = LocalIdentity::generate();
    let device = joiner.device_id()?;
    let nonce = random_hex(32);
    let join_id = invite(&fixture, [2; 32])?;
    fixture
        .store
        .join(&request(&joiner, [2; 32], &nonce)?, &device)?;
    fixture
        .store
        .approve_nonce(&join_id, &admin, &random_hex(32))?;
    let voided = fixture
        .store
        .join_status(&join_id, &device, Some(&random_hex(32)))?;
    assert_eq!(voided.status, JoinStatus::Rejected);
    assert_eq!(voided.reason.as_deref(), Some("commitment_mismatch"));
    assert!(
        fixture
            .store
            .join_status(&join_id, &device, Some(&nonce))
            .is_err()
    );
    assert!(
        fixture
            .store
            .join(&request(&joiner, [2; 32], &nonce)?, &device)
            .is_ok_and(|r| r.status == JoinStatus::Rejected)
    );

    let second = invite(&fixture, [3; 32])?;
    fixture
        .store
        .join(&request(&joiner, [3; 32], &nonce)?, &device)?;
    assert_eq!(
        fixture.store.reject_join(&second, &admin)?.status,
        JoinStatus::Rejected
    );
    assert!(
        fixture
            .store
            .approve_nonce(&second, &admin, &random_hex(32))
            .is_err()
    );
    assert!(fixture.store.pending_joins(&admin)?.is_empty());
    Ok(())
}

#[test]
fn only_the_joining_device_withdraws_and_a_withdrawn_request_cannot_be_approved() -> Result<()> {
    let fixture = fixture()?;
    let admin = fixture.admin.device_id()?;
    let joiner = LocalIdentity::generate();
    let device = joiner.device_id()?;
    let nonce = random_hex(32);
    let join_id = invite(&fixture, [4; 32])?;
    fixture
        .store
        .join(&request(&joiner, [4; 32], &nonce)?, &device)?;
    fixture
        .store
        .approve_nonce(&join_id, &admin, &random_hex(32))?;
    fixture.store.join_status(&join_id, &device, Some(&nonce))?;
    assert_eq!(fixture.store.pending_joins(&admin)?.len(), 1);

    let other = LocalIdentity::generate().device_id()?;
    assert!(
        fixture.store.withdraw_join(&join_id, &other).is_err(),
        "another device cannot withdraw this request"
    );
    assert!(fixture.store.withdraw_join(&join_id, &admin).is_err());
    assert_eq!(fixture.store.pending_joins(&admin)?.len(), 1);

    let withdrawn = fixture.store.withdraw_join(&join_id, &device)?;
    assert_eq!(withdrawn.status, JoinStatus::Withdrawn);
    assert_eq!(fixture.store.withdraw_join(&join_id, &device)?, withdrawn);
    assert!(fixture.store.pending_joins(&admin)?.is_empty());
    let view = fixture.store.public_state()?.project()?;
    let grant = GovernanceEvent::grant(
        &fixture.admin,
        &view,
        Member::client(&joiner, "phone", false)?,
    )?;
    assert!(
        fixture
            .store
            .approve_grant(&join_id, &admin, grant.clone())
            .is_err()
    );
    assert!(
        fixture
            .store
            .public_state()?
            .project()?
            .member(&device)
            .is_err()
    );

    let late = invite(&fixture, [5; 32])?;
    let nonce = random_hex(32);
    fixture
        .store
        .join(&request(&joiner, [5; 32], &nonce)?, &device)?;
    fixture
        .store
        .approve_nonce(&late, &admin, &random_hex(32))?;
    fixture.store.join_status(&late, &device, Some(&nonce))?;
    invite(&fixture, [6; 32])?;
    fixture
        .store
        .join(&request(&joiner, [6; 32], &random_hex(32))?, &device)?;
    fixture.store.approve_grant(&late, &admin, grant)?;
    assert!(
        fixture.store.pending_joins(&admin)?.is_empty(),
        "a request from a device that is already a member can no longer complete"
    );
    assert_eq!(
        fixture.store.withdraw_join(&late, &device)?.status,
        JoinStatus::Joined
    );
    Ok(())
}

#[test]
fn bootstrap_capability_is_one_time_and_requires_a_manager_session() -> Result<()> {
    let dir = std::env::temp_dir().join(format!("vibeface-bootstrap-{}", random_hex(8)));
    fs::create_dir_all(&dir)?;
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o700))?;
    let store = Store::open(dir.join("node.sqlite3"))?;
    let capability = store.bootstrap_capability()?;
    assert!(store.bootstrap_capability().is_err());
    let root = LocalIdentity::generate();
    let admin = LocalIdentity::generate();
    let state = NetworkState::new(Genesis::create(
        &root,
        Member::client(&admin, "admin", true)?,
        Member::node(&LocalIdentity::generate(), "node")?,
    )?)?;
    assert!(
        store
            .bootstrap_network(state.clone(), &capability.capability, &root.device_id()?)
            .is_err()
    );
    assert!(
        store
            .bootstrap_network(state.clone(), &random_hex(32), &admin.device_id()?)
            .is_err()
    );
    store.bootstrap_network(state.clone(), &capability.capability, &admin.device_id()?)?;
    assert!(
        store
            .verify_bootstrap_capability(&capability.capability)
            .is_err()
    );
    assert!(
        store
            .bootstrap_network(state, &capability.capability, &admin.device_id()?)
            .is_err()
    );
    fs::remove_dir_all(dir)?;
    Ok(())
}

#[test]
fn a_name_already_in_the_network_gets_the_next_number_and_a_retry_keeps_it() -> Result<()> {
    let fixture = fixture()?;
    let (first, second) = (LocalIdentity::generate(), LocalIdentity::generate());
    invite(&fixture, [7; 32])?;
    invite(&fixture, [8; 32])?;
    let ask = |device: &LocalIdentity, secret, nonce: &str| -> Result<JoinRequest> {
        let mut join = request(device, secret, nonce)?;
        join.name = "admin".into();
        fixture.store.join(&join, &device.device_id()?)
    };
    let (nonce1, nonce2) = (random_hex(32), random_hex(32));
    assert_eq!(
        ask(&first, [7; 32], &nonce1)?.name,
        "admin 2",
        "a member already has the asked name"
    );
    assert_eq!(
        ask(&second, [8; 32], &nonce2)?.name,
        "admin 3",
        "an open request holds the next number"
    );
    assert_eq!(
        ask(&first, [7; 32], &nonce1)?.name,
        "admin 2",
        "the retried request keeps its name"
    );
    let mut renamed = request(&first, [7; 32], &nonce1)?;
    renamed.name = "admin x".into();
    assert!(
        fixture.store.join(&renamed, &first.device_id()?).is_err(),
        "another name is another request"
    );

    let long = format!("{}長", "a".repeat(125));
    let cut = format!("{} 2", "a".repeat(125));
    assert_eq!(
        distinct_name(&long, &[long.clone()]),
        cut,
        "the number fits within 128 bytes on a character boundary"
    );
    assert!(numbered_from(&cut, &long));
    Ok(())
}
