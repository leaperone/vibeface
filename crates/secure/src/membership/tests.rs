use super::projection::Effect;
use crate::Identity;
use anyhow::{Context, Result};

use super::*;

fn identity() -> Identity {
    Identity::generate().expect("test identity randomness")
}

fn setup() -> Result<(Identity, Identity, Identity, NetworkState)> {
    let root = identity();
    let a = identity();
    let node = identity();
    let state = NetworkState::new(Genesis::create(
        &root,
        Member::client(&a, "a", true)?,
        Member::node(&node, "node")?,
    )?)?;
    Ok((root, a, node, state))
}

fn id(identity: &Identity) -> String {
    identity.device_id()
}

fn hash(n: u64) -> String {
    format!("{n:064x}")
}

fn cut(device: &Identity, seq: u64) -> Revocation {
    let hash = hash(seq);
    Revocation::new(&id(device), (seq > 0).then_some((seq, hash.as_str())))
        .expect("valid test revocation")
}

#[test]
fn device_and_network_ids_are_public_keys() -> Result<()> {
    let (root, a, node, state) = setup()?;
    assert_eq!(state.network_id(), id(&root));
    let view = state.project()?;
    assert!(view.manager(&id(&a)).is_ok());
    assert!(view.node(&id(&node)).is_ok());
    assert!(Member::new("00".repeat(32).as_str(), IdentityKind::Client, "x", false).is_err());
    assert!(Member::new(&id(&node), IdentityKind::Node, "n", true).is_err());
    assert!(Member::new(&id(&node), IdentityKind::Node, " ", false).is_err());
    Ok(())
}

#[test]
fn removal_evidence_uses_the_effective_revoke_and_its_concurrent_barriers() -> Result<()> {
    let (_, a, node, mut base) = setup()?;
    let (b, c, later) = (identity(), identity(), identity());
    for manager in [&b, &c] {
        base.append(GovernanceEvent::grant(
            &a,
            &base.project()?,
            Member::client(manager, "manager", true)?,
        )?)?;
    }
    let view = base.project()?;
    let x = GovernanceEvent::revoke(&b, &view, cut(&a, 0))?;
    let r1 = GovernanceEvent::revoke(&a, &view, cut(&node, 0))?;
    let r2 = GovernanceEvent::revoke(&b, &view, cut(&node, 0))?;
    let mut receiver = base.clone();
    receiver.append(x.clone())?;
    let mut first = receiver.clone();
    first.append(r1.clone())?;
    first.append(r2.clone())?;
    let proof = first.removal_state_at(&id(&node), None, T)?;
    assert!(proof.events.contains(&r2) && !proof.events.contains(&r1));
    assert!(
        receiver
            .merge(&proof)?
            .project_at(T)?
            .removed_node(&id(&node))
    );

    let y = GovernanceEvent::revoke(&c, &view, cut(&b, 0))?;
    let mut stronger = receiver.clone();
    stronger.append(y.clone())?;
    stronger.append(r1.clone())?;
    stronger.append(GovernanceEvent::grant(
        &c,
        &stronger.project()?,
        Member::client(&later, "later-private-name", false)?,
    )?)?;
    let proof = stronger.removal_state_at(&id(&node), None, T)?;
    assert!(proof.events.contains(&r1) && proof.events.contains(&x) && proof.events.contains(&y));
    assert!(
        receiver
            .merge(&proof)?
            .project_at(T)?
            .removed_node(&id(&node)),
        "the noncausal Y barrier is required to keep X ineffective at the receiver"
    );
    assert!(!serde_json::to_string(&proof)?.contains(&id(&later)));
    assert!(!serde_json::to_string(&proof)?.contains("later-private-name"));
    Ok(())
}

#[test]
fn removal_evidence_fails_closed_when_effect_support_would_disclose_a_later_grant() -> Result<()> {
    let (_, a, node, mut state) = setup()?;
    let (b, c, d) = (identity(), identity(), identity());
    for manager in [&b, &d] {
        state.append(GovernanceEvent::grant(
            &a,
            &state.project()?,
            Member::client(manager, "manager", true)?,
        )?)?;
    }
    let view = state.project()?;
    let mut private = state.clone();
    private.append(GovernanceEvent::grant(
        &d,
        &view,
        Member::client(&c, "private-manager", true)?,
    )?)?;
    state.append(GovernanceEvent::revoke(&a, &view, cut(&node, 0))?)?;
    state.append(GovernanceEvent::revoke(&b, &view, cut(&a, 0))?)?;
    state = state.merge(&private)?;
    let known = state.clone();
    assert!(known.project_at(T)?.node(&id(&node)).is_ok());
    state.append(GovernanceEvent::revoke(
        &c,
        &private.project()?,
        cut(&b, 0),
    )?)?;
    assert!(state.project_at(T)?.removed_node(&id(&node)));
    assert!(state.removal_state_at(&id(&node), None, T).is_err());
    let proof = state.removal_state_at(&id(&node), Some(&known), T)?;
    assert!(known.merge(&proof)?.project_at(T)?.removed_node(&id(&node)));
    let mut forged = known;
    forged.events.last_mut().unwrap().signature = "00".repeat(64);
    assert!(
        state
            .removal_state_at(&id(&node), Some(&forged), T)
            .is_err(),
        "self-reported identities do not authorize disclosure"
    );
    Ok(())
}

#[test]
fn removal_evidence_never_revives_a_revoke_cut_by_a_later_rotation_scope() -> Result<()> {
    let (root, a, node, mut base) = setup()?;
    let b = identity();
    base.append(GovernanceEvent::grant(
        &a,
        &base.project()?,
        Member::client(&b, "b", true)?,
    )?)?;
    let old = GovernanceEvent::revoke(&a, &base.project()?, cut(&node, 0))?;
    let rotation = RootCheckpoint::rotation(&base, &root, &id(&identity()), 0)?;
    let mut receiver = base.clone();
    receiver.recover_at(rotation.clone(), 0)?;
    let view = receiver.project_at(T)?;
    let barrier = GovernanceEvent::revoke(&b, &view, cut(&a, 0))?;
    let current = GovernanceEvent::revoke(&b, &view, cut(&node, 0))?;
    receiver.append(barrier.clone())?;
    assert!(receiver.project_at(T)?.node(&id(&node)).is_ok());
    let mut state = base;
    state.append(old.clone())?;
    state.recover_at(rotation, 0)?;
    state.append(barrier)?;
    state.append(current.clone())?;
    let proof = state.removal_state_at(&id(&node), None, T)?;
    assert_eq!(proof.checkpoints.len(), 1);
    assert!(proof.events.contains(&current) && !proof.events.contains(&old));
    assert!(
        receiver
            .merge(&proof)?
            .project_at(T)?
            .removed_node(&id(&node))
    );
    Ok(())
}

#[test]
fn removal_evidence_follows_a_restoration_and_omits_later_recovery_directories() -> Result<()> {
    let (root, a, node, mut state) = setup()?;
    state.append(GovernanceEvent::revoke(
        &a,
        &state.project()?,
        cut(&node, 0),
    )?)?;
    let next = identity();
    let restored = RootCheckpoint::recovery(
        &state,
        &root,
        &id(&next),
        vec![Member::client(&a, "a", true)?, Member::node(&node, "node")?],
        vec![],
        0,
    )?;
    state.recover_at(restored, T)?;
    let known = state.clone();
    let removal = GovernanceEvent::revoke(&a, &state.project_at(T)?, cut(&node, 0))?;
    state.append(removal.clone())?;
    let later = identity();
    let mut members = state.project_at(T)?.members;
    members.push(Member::client(&later, "future-private-directory", false)?);
    let checkpoint = RootCheckpoint::recovery(
        &state,
        &next,
        &id(&identity()),
        members,
        vec![cut(&node, 0)],
        0,
    )?;
    state.recover_at(checkpoint, T)?;
    let proof = state.removal_state_at(&id(&node), Some(&known), T)?;
    assert_eq!(
        proof.checkpoints.len(),
        1,
        "the proof follows the restoration and stops before the later directory"
    );
    assert!(proof.events.contains(&removal));
    assert!(known.merge(&proof)?.project_at(T)?.removed_node(&id(&node)));
    assert!(!serde_json::to_string(&proof)?.contains(&id(&later)));
    Ok(())
}

#[test]
fn removal_evidence_keeps_recovery_retirement_before_later_repeated_revokes() -> Result<()> {
    let (root, a, service, mut state) = setup()?;
    let removed = identity();
    state.append(GovernanceEvent::grant(
        &a,
        &state.project()?,
        Member::node(&removed, "removed")?,
    )?)?;
    let known = state.clone();
    let next = identity();
    let checkpoint = RootCheckpoint::recovery(
        &state,
        &root,
        &id(&next),
        vec![
            Member::client(&a, "a", true)?,
            Member::node(&service, "service")?,
        ],
        vec![cut(&removed, 0)],
        0,
    )?;
    state.recover_at(checkpoint, T)?;
    let later = identity();
    state.append(GovernanceEvent::grant(
        &a,
        &state.project_at(T)?,
        Member::client(&later, "private-after-retirement", false)?,
    )?)?;
    state.append(GovernanceEvent::revoke(
        &a,
        &state.project_at(T)?,
        cut(&removed, 0),
    )?)?;
    for held in [None, Some(&known)] {
        let proof = state.removal_state_at(&id(&removed), held, T)?;
        assert!(proof.project_at(T)?.removed_node(&id(&removed)));
        assert!(
            known
                .merge(&proof)?
                .project_at(T)?
                .removed_node(&id(&removed))
        );
        let encoded = serde_json::to_string(&proof)?;
        assert!(!encoded.contains(&id(&later)));
        assert!(!encoded.contains("private-after-retirement"));
    }
    Ok(())
}

#[test]
fn a_late_veto_or_local_rotation_rescinds_a_removal_proof() -> Result<()> {
    let (root, a, node, mut state) = setup()?;
    let checkpoint = RootCheckpoint::recovery(
        &state,
        &root,
        &id(&identity()),
        vec![Member::client(&a, "a", true)?],
        vec![cut(&node, 0)],
        0,
    )?;
    state.recover_at(checkpoint.clone(), 0)?;
    let proof = state.removal_state_at(&id(&node), None, T)?;
    assert!(proof.project_at(T)?.removed_node(&id(&node)));
    let event = veto(&a, &state.project_at(0)?, &checkpoint, 1)?;
    let mut vetoed = state.clone();
    vetoed.append(event)?;
    assert!(vetoed.project_at(T)?.node(&id(&node)).is_ok());
    assert!(vetoed.removal_state_at(&id(&node), None, T).is_err());
    let rotation = RootCheckpoint::rotation(&state, &root, &id(&identity()), 1)?;
    state.recover_at(rotation, 1)?;
    assert!(state.project_at(T)?.node(&id(&node)).is_ok());
    assert!(state.removal_state_at(&id(&node), None, T).is_err());
    Ok(())
}

#[test]
fn managers_grant_management_and_ordinary_clients_cannot_govern() -> Result<()> {
    let (_, a, _, mut state) = setup()?;
    let b = identity();
    let ordinary = identity();
    state.append(GovernanceEvent::grant(
        &a,
        &state.project()?,
        Member::client(&ordinary, "ordinary", false)?,
    )?)?;
    assert!(
        GovernanceEvent::grant(
            &ordinary,
            &state.project()?,
            Member::client(&ordinary, "ordinary", true)?
        )
        .is_err()
    );
    let mut forged = GovernanceEvent::unsigned(
        &state.project()?,
        &id(&a),
        GovernanceAction::Grant {
            member: Member::client(&ordinary, "ordinary", true)?,
        },
    )?;
    forged.issuer = id(&ordinary);
    refused(
        state.append(forged.sign(&ordinary)?),
        "management permission required",
    );
    state.append(GovernanceEvent::grant(
        &a,
        &state.project()?,
        Member::client(&b, "b", true)?,
    )?)?;
    state.append(GovernanceEvent::grant(
        &b,
        &state.project()?,
        Member::client(&ordinary, "promoted", true)?,
    )?)?;
    assert!(state.project()?.manager(&id(&ordinary)).is_ok());
    Ok(())
}

#[test]
fn a_client_leaves_by_revoking_itself_and_rejoins_only_with_a_new_key() -> Result<()> {
    let (_, a, node, mut state) = setup()?;
    let (ordinary, other) = (identity(), identity());
    for device in [&ordinary, &other] {
        state.append(GovernanceEvent::grant(
            &a,
            &state.project()?,
            Member::client(device, "client", false)?,
        )?)?;
    }
    let refused = |result: Result<GovernanceEvent>, reason: &str| {
        assert!(format!("{:#}", result.unwrap_err()).contains(reason));
    };
    let view = state.project()?;
    refused(
        GovernanceEvent::revoke(&ordinary, &view, cut(&other, 0)),
        "management permission required",
    );
    refused(
        GovernanceEvent::revoke(&node, &view, cut(&node, 0)),
        "management permission required",
    );
    refused(GovernanceEvent::revoke(&a, &view, cut(&a, 0)), LAST_MANAGER);
    let mut forged = GovernanceEvent::revoke(&a, &view, cut(&other, 0))?;
    forged.issuer = id(&ordinary);
    let error = state.append(forged.sign(&ordinary)?).unwrap_err();
    assert!(
        format!("{error:#}").contains("management permission required"),
        "{error:#}"
    );

    state.append(GovernanceEvent::revoke(
        &ordinary,
        &view,
        cut(&ordinary, 0),
    )?)?;
    let view = state.project()?;
    assert!(view.member(&id(&ordinary)).is_err());
    assert_eq!(view.cutoff(&id(&ordinary)), Some(0));
    assert!(view.member(&id(&other)).is_ok());
    assert!(GovernanceEvent::revoke(&ordinary, &view, cut(&ordinary, 0)).is_err());
    state.append(GovernanceEvent::grant(
        &a,
        &view,
        Member::client(&ordinary, "back", false)?,
    )?)?;
    assert!(state.project()?.member(&id(&ordinary)).is_err());
    let fresh = identity();
    state.append(GovernanceEvent::grant(
        &a,
        &state.project()?,
        Member::client(&fresh, "back", false)?,
    )?)?;
    assert!(state.project()?.member(&id(&fresh)).is_ok());
    Ok(())
}

#[test]
fn a_manager_can_be_demoted_while_another_remains() -> Result<()> {
    let (_, a, _, mut state) = setup()?;
    let b = identity();
    let last = |result: Result<GovernanceEvent>| {
        assert!(format!("{:#}", result.unwrap_err()).contains(LAST_MANAGER));
    };
    last(GovernanceEvent::grant(
        &a,
        &state.project()?,
        Member::client(&a, "a", false)?,
    ));
    state.append(GovernanceEvent::grant(
        &a,
        &state.project()?,
        Member::client(&b, "b", true)?,
    )?)?;
    state.append(GovernanceEvent::grant(
        &b,
        &state.project()?,
        Member::client(&a, "a", false)?,
    )?)?;
    let view = state.project()?;
    assert!(view.manager(&id(&a)).is_err() && view.client(&id(&a)).is_ok());
    assert!(GovernanceEvent::grant(&a, &view, Member::client(&a, "a", true)?).is_err());
    last(GovernanceEvent::grant(
        &b,
        &view,
        Member::client(&b, "b", false)?,
    ));
    state.append(GovernanceEvent::grant(
        &b,
        &view,
        Member::client(&a, "a", true)?,
    )?)?;
    assert!(state.project()?.manager(&id(&a)).is_ok());
    Ok(())
}

#[test]
fn issuer_authority_comes_from_causal_ancestors_only() -> Result<()> {
    let (_, a, _, base) = setup()?;
    let b = identity();
    let c = identity();
    let mut granted = base.clone();
    granted.append(GovernanceEvent::grant(
        &a,
        &granted.project()?,
        Member::client(&b, "b", true)?,
    )?)?;
    let early = signed_at(
        &base,
        &b,
        1,
        vec![],
        GovernanceAction::Grant {
            member: Member::client(&c, "c", false)?,
        },
    )?;
    refused(granted.append(early), "management permission required");
    Ok(())
}

#[test]
fn a_revoked_manager_cannot_backdate_a_derived_manager() -> Result<()> {
    let (_, a, _, mut state) = setup()?;
    let (b, derived) = (identity(), identity());
    state.append(GovernanceEvent::grant(
        &a,
        &state.project()?,
        Member::client(&b, "b", true)?,
    )?)?;
    let before_removal = state.clone();
    state.append(GovernanceEvent::revoke(&b, &state.project()?, cut(&a, 0))?)?;
    let attack = GovernanceEvent::grant(
        &a,
        &before_removal.project()?,
        Member::client(&derived, "derived", true)?,
    )?;
    let attack_hash = attack.hash()?;
    state.append(attack)?;
    let view = state.project()?;
    assert!(view.member(&id(&derived)).is_err());
    assert!(!view.attester(&id(&derived)));
    assert!(
        state
            .events
            .iter()
            .any(|event| event.hash().ok().as_ref() == Some(&attack_hash))
    );
    Ok(())
}

fn signed_at(
    state: &NetworkState,
    issuer: &Identity,
    epoch: u64,
    parents: Vec<String>,
    action: GovernanceAction,
) -> Result<GovernanceEvent> {
    GovernanceEvent {
        version: IDENTITY_VERSION,
        network_id: state.network_id().to_owned(),
        epoch,
        parents,
        issuer: id(issuer),
        action,
        signature: String::new(),
    }
    .sign(issuer)
}

fn two_managers() -> Result<(Identity, Identity, Identity, Identity, NetworkState)> {
    let (root, a, node, mut state) = setup()?;
    let b = identity();
    state.append(GovernanceEvent::grant(
        &a,
        &state.project()?,
        Member::client(&b, "b", true)?,
    )?)?;
    Ok((root, a, b, node, state))
}

#[test]
fn causal_grants_survive_the_grantee_revoking_the_grantor_across_rotations() -> Result<()> {
    for rotations in [0, 1, 3] {
        let (mut root, a, _, mut state) = setup()?;
        let x = identity();
        state.append(GovernanceEvent::grant(
            &a,
            &state.project_at(T)?,
            Member::client(&x, "x", true)?,
        )?)?;
        let grant = state.events[0].hash()?;
        for _ in 0..rotations {
            let next = identity();
            state.recover_at(RootCheckpoint::rotation(&state, &root, &id(&next), T)?, T)?;
            root = next;
        }
        let revoke = GovernanceEvent::revoke(&x, &state.project_at(T)?, cut(&a, 0))?;
        assert!(revoke.parents.contains(&grant));
        let before = state.clone();
        state.append(revoke)?;
        let view = state.project_at(T)?;
        assert!(view.manager(&id(&x)).is_ok() && view.attester(&id(&x)));
        assert_eq!(view.cutoff(&id(&a)), Some(0));
        assert_eq!(state, before.merge(&state)?);
        assert_eq!(view, state.merge(&before)?.project_at(T)?);
    }
    Ok(())
}

#[test]
fn invalid_derived_grants_and_revokes_preserve_history_without_isolating_other_devices()
-> Result<()> {
    let (_, a, b, node, base) = two_managers()?;
    let (x, y) = (identity(), identity());
    let g = GovernanceEvent::grant(&a, &base.project_at(T)?, Member::client(&x, "x", true)?)?;
    let h = signed_at(
        &base,
        &x,
        1,
        vec![g.hash()?],
        GovernanceAction::Grant {
            member: Member::client(&y, "y", true)?,
        },
    )?;
    let r = signed_at(
        &base,
        &y,
        1,
        vec![h.hash()?],
        GovernanceAction::Revoke(cut(&node, 9)),
    )?;
    let mut removed = base.clone();
    removed.append(GovernanceEvent::revoke(
        &b,
        &base.project_at(T)?,
        cut(&a, 0),
    )?)?;
    let mut attack = base;
    for event in [&g, &h, &r] {
        attack.append(event.clone())?;
    }
    let merged = removed.merge(&attack)?;
    assert_eq!(merged, attack.merge(&removed)?);
    assert_eq!(merged, merged.merge(&attack)?.merge(&removed)?);
    let view = merged.project_at(T)?;
    for device in [&x, &y] {
        assert!(view.member(&id(device)).is_err() && !view.attester(&id(device)));
    }
    assert!(view.node(&id(&node)).is_ok());
    assert_eq!(view.cutoff(&id(&node)), None);
    assert_eq!(view.signed_removals(), [id(&a)]);
    for event in [g, h, r] {
        assert!(merged.events.contains(&event));
    }
    Ok(())
}

#[test]
fn mutual_and_derived_cycles_keep_unknown_permissions_closed() -> Result<()> {
    let (_, a, b, _, mut base) = two_managers()?;
    let c = identity();
    base.append(GovernanceEvent::grant(
        &a,
        &base.project_at(T)?,
        Member::client(&c, "c", true)?,
    )?)?;
    for devices in [vec![&a, &b], vec![&a, &b, &c]] {
        let view = base.project_at(T)?;
        let mut branches = Vec::new();
        for (index, issuer) in devices.iter().enumerate() {
            let mut branch = base.clone();
            branch.append(GovernanceEvent::revoke(
                *issuer,
                &view,
                cut(devices[(index + 1) % devices.len()], 0),
            )?)?;
            branches.push(branch);
        }
        let forward = branches
            .iter()
            .try_fold(base.clone(), |state, branch| state.merge(branch))?;
        let reverse = branches
            .iter()
            .rev()
            .try_fold(base.clone(), |state, branch| state.merge(branch))?;
        assert_eq!(forward, reverse);
        let view = forward.project_at(T)?;
        for device in &devices {
            assert!(view.member(&id(device)).is_err());
            assert_eq!(
                view.cutoff(&id(device)),
                None,
                "Unknown is not a signed cutoff"
            );
        }
        assert!(view.signed_removals().is_empty());
    }
    let x = identity();
    let g = GovernanceEvent::grant(&a, &base.project_at(T)?, Member::client(&x, "x", true)?)?;
    let s = signed_at(
        &base,
        &x,
        1,
        vec![g.hash()?],
        GovernanceAction::Revoke(cut(&b, 0)),
    )?;
    let mut left = base.clone();
    left.append(g)?;
    left.append(s)?;
    let mut right = base;
    right.append(GovernanceEvent::revoke(
        &b,
        &right.project_at(T)?,
        cut(&a, 0),
    )?)?;
    let merged = left.merge(&right)?;
    assert_eq!(merged, right.merge(&left)?);
    let view = merged.project_at(T)?;
    for device in [&a, &b, &x] {
        assert!(view.member(&id(device)).is_err());
    }
    assert!(!view.attester(&id(&x)) && view.signed_removals().is_empty());
    Ok(())
}

#[test]
fn a_causal_removal_preserves_its_safe_bound_when_its_issuer_is_removed() -> Result<()> {
    let (_, a, b, node, mut state) = two_managers()?;
    state.append(GovernanceEvent::revoke(
        &a,
        &state.project_at(T)?,
        cut(&node, 7),
    )?)?;
    state.append(GovernanceEvent::revoke(
        &b,
        &state.project_at(T)?,
        cut(&a, 0),
    )?)?;
    let view = state.project_at(T)?;
    assert_eq!(
        view.write_bound(&id(&node)),
        Some(WriteBound::Cutoff(&cut(&node, 7)))
    );
    assert_eq!(view.cutoff(&id(&a)), Some(0));
    Ok(())
}

#[test]
fn demotion_blocks_backdated_management_but_preserves_self_leave_and_causal_repromotion()
-> Result<()> {
    let (_, a, b, _, base) = two_managers()?;
    let (x, y) = (identity(), identity());
    let stale =
        GovernanceEvent::grant(&a, &base.project_at(T)?, Member::client(&x, "stale", true)?)?;
    let mut demoted = base.clone();
    demoted.append(GovernanceEvent::grant(
        &b,
        &base.project_at(T)?,
        Member::client(&a, "a", false)?,
    )?)?;
    demoted.append(stale)?;
    assert!(demoted.project_at(T)?.member(&id(&x)).is_err());
    let mut leaving = demoted.clone();
    leaving.append(GovernanceEvent::revoke(
        &a,
        &leaving.project_at(T)?,
        cut(&a, 0),
    )?)?;
    assert_eq!(leaving.project_at(T)?.cutoff(&id(&a)), Some(0));
    assert!(leaving.project_at(T)?.manager(&id(&b)).is_ok());

    demoted.append(GovernanceEvent::grant(
        &b,
        &demoted.project_at(T)?,
        Member::client(&a, "a", true)?,
    )?)?;
    demoted.append(GovernanceEvent::grant(
        &a,
        &demoted.project_at(T)?,
        Member::client(&y, "causal", true)?,
    )?)?;
    assert!(demoted.project_at(T)?.manager(&id(&y)).is_ok());
    assert!(
        demoted.project_at(T)?.member(&id(&x)).is_err(),
        "later promotion does not bless stale parents"
    );
    let before_second = demoted.project_at(T)?;
    demoted.append(GovernanceEvent::grant(
        &b,
        &before_second,
        Member::client(&a, "a", false)?,
    )?)?;
    let z = identity();
    demoted.append(GovernanceEvent::grant(
        &a,
        &before_second,
        Member::client(&z, "stale again", true)?,
    )?)?;
    assert!(demoted.project_at(T)?.manager(&id(&y)).is_ok());
    assert!(demoted.project_at(T)?.member(&id(&z)).is_err());

    let mut concurrent = base.clone();
    concurrent.append(GovernanceEvent::grant(
        &b,
        &base.project_at(T)?,
        Member::client(&a, "a", false)?,
    )?)?;
    concurrent.append(GovernanceEvent::grant(
        &b,
        &base.project_at(T)?,
        Member::client(&a, "a", true)?,
    )?)?;
    let g = GovernanceEvent::grant(
        &a,
        &base.project_at(T)?,
        Member::client(&x, "concurrent", true)?,
    )?;
    concurrent.append(g)?;
    let view = concurrent.project_at(T)?;
    assert!(view.client(&id(&a)).is_ok() && view.manager(&id(&a)).is_err());
    assert!(view.member(&id(&x)).is_err());
    Ok(())
}

#[test]
fn invalid_and_unknown_promotions_do_not_release_a_demotion_barrier() -> Result<()> {
    let (_, a, b, _, mut base) = two_managers()?;
    let (c, x, target) = (identity(), identity(), identity());
    base.append(GovernanceEvent::grant(
        &b,
        &base.project_at(T)?,
        Member::client(&c, "c", true)?,
    )?)?;
    let d = GovernanceEvent::grant(&b, &base.project_at(T)?, Member::client(&a, "a", false)?)?;
    for unknown in [false, true] {
        let gx = GovernanceEvent::grant(&c, &base.project_at(T)?, Member::client(&x, "x", true)?)?;
        let mut history = base.clone();
        history.append(gx.clone())?;
        let authority = history.project_at(T)?;
        let mut left = history.clone();
        left.append(d.clone())?;
        let issuer = if unknown { &c } else { &x };
        let p = signed_at(
            &history,
            issuer,
            1,
            left.project_at(T)?.heads,
            GovernanceAction::Grant {
                member: Member::client(&a, "a", true)?,
            },
        )?;
        let g = signed_at(
            &history,
            &a,
            1,
            vec![p.hash()?],
            GovernanceAction::Grant {
                member: Member::client(&target, "target", true)?,
            },
        )?;
        left.append(p)?;
        left.append(g)?;
        if unknown {
            history.append(GovernanceEvent::revoke(&c, &authority, cut(&x, 0))?)?;
            history.append(GovernanceEvent::revoke(&x, &authority, cut(&c, 0))?)?;
        } else {
            let mut removal = base.clone();
            removal.append(GovernanceEvent::revoke(
                &b,
                &base.project_at(T)?,
                cut(&c, 0),
            )?)?;
            history = history.merge(&removal)?;
        }
        let view = history.merge(&left)?.project_at(T)?;
        assert!(view.member(&id(&target)).is_err() && !view.attester(&id(&target)));
        assert!(view.manager(&id(&a)).is_err());
    }
    Ok(())
}

#[test]
fn an_invalid_grant_cannot_override_an_independent_valid_grant() -> Result<()> {
    let (_, a, b, _, base) = two_managers()?;
    let x = identity();
    let mut state = base.clone();
    state.append(GovernanceEvent::revoke(
        &b,
        &base.project_at(T)?,
        cut(&a, 0),
    )?)?;
    state.append(GovernanceEvent::grant(
        &a,
        &base.project_at(T)?,
        Member::client(&x, "aaa invalid", false)?,
    )?)?;
    state.append(GovernanceEvent::grant(
        &b,
        &base.project_at(T)?,
        Member::client(&x, "valid", true)?,
    )?)?;
    let member = state.project_at(T)?.manager(&id(&x))?;
    assert_eq!(member.name, "valid");
    Ok(())
}

#[test]
fn a_rotation_checkpoint_freezes_its_old_epoch_electorate() -> Result<()> {
    let (root, a, b, node, before_grant) = two_managers()?;
    let x = identity();
    let mut state = before_grant.clone();
    state.append(GovernanceEvent::grant(
        &a,
        &state.project_at(T)?,
        Member::client(&x, "x", true)?,
    )?)?;
    let next = identity();
    state.recover_at(RootCheckpoint::rotation(&state, &root, &id(&next), T)?, T)?;
    let (_, _, checkpoint) = recovering(&state, &next, &node, T + 1)?;
    assert!(!checkpoint.heads.is_empty());
    state.recover_at(checkpoint.clone(), T + 1)?;
    state.append(veto(&x, &state.project_at(T + 1)?, &checkpoint, T + 2)?)?;
    let mut removed = before_grant;
    removed.append(GovernanceEvent::revoke(
        &b,
        &removed.project_at(T)?,
        cut(&a, 0),
    )?)?;
    let merged = state.merge(&removed)?;
    assert_eq!(merged, removed.merge(&state)?);
    let view = merged.project_at(T + RECOVERY_DELAY_MS + 2)?;
    assert_eq!(view.epoch, 2);
    assert!(view.stopped.contains(&checkpoint.hash()?));
    assert!(view.member(&id(&x)).is_err() && !view.attester(&id(&x)));
    let (_, _, later) = recovering(&merged, &next, &node, T + 3)?;
    let mut later_state = merged;
    later_state.recover_at(later.clone(), T + 3)?;
    let event = signed_at(
        &later_state,
        &x,
        2,
        later_state.project_at(T + 3)?.heads,
        GovernanceAction::Veto {
            checkpoint: later.hash()?,
            at_ms: T + 4,
        },
    )?;
    later_state.append(event)?;
    assert!(later_state.project_at(T + 4)?.pending.is_some());
    Ok(())
}

#[test]
fn a_manager_outside_the_checkpoint_cut_cannot_vote_even_with_complete_parents() -> Result<()> {
    let (root, a, node, mut state) = setup()?;
    let (_, _, checkpoint) = recovering(&state, &root, &node, T)?;
    state.recover_at(checkpoint.clone(), T)?;
    let x = identity();
    state.append(GovernanceEvent::grant(
        &a,
        &state.project_at(T)?,
        Member::client(&x, "x", true)?,
    )?)?;
    for action in [
        GovernanceAction::Veto {
            checkpoint: checkpoint.hash()?,
            at_ms: T + 1,
        },
        GovernanceAction::Approve {
            checkpoint: checkpoint.hash()?,
            at_ms: T + 1,
        },
    ] {
        let encoded = serde_json::to_string(&state)?;
        let verified = verify_membership(&encoded, state.network_id(), Some(T as f64))?;
        assert!(
            verified
                .governance_template(&id(&x), &serde_json::to_string(&action)?)
                .is_err()
        );
        let event = signed_at(&state, &x, 1, state.project_at(T)?.heads, action)?;
        state.append(event)?;
        assert_eq!(state.project_at(T + 2)?.epoch, 1);
        assert!(state.project_at(T + 2)?.pending.is_some());
    }
    Ok(())
}

#[test]
fn scope_frontiers_keep_eighty_heads_and_depth_is_bounded_per_epoch() -> Result<()> {
    let (root, a, node, mut state) = setup()?;
    for epoch in [1, 2] {
        if epoch == 2 {
            state.recover_at(
                RootCheckpoint::rotation(&state, &root, &id(&identity()), T)?,
                T,
            )?;
        }
        let mut incoming = state.clone();
        for index in 0..40 {
            incoming.events.push(signed_at(
                &state,
                &a,
                epoch,
                vec![],
                GovernanceAction::Grant {
                    member: Member::client(&identity(), &format!("{epoch}-{index}"), false)?,
                },
            )?);
        }
        state = state.merge(&incoming)?;
    }
    assert_eq!(state.project_at(T)?.heads.len(), 80);
    let next_root = state.checkpoints[0].recovery_key.clone();
    let heads = state.heads_of(&next_root)?;
    assert_eq!(heads.len(), 80);
    let event = GovernanceEvent::grant(&a, &state.project_at(T)?, Member::node(&node, "renamed")?)?;
    assert_eq!(event.parents.len(), 80);
    state.append(event)?;
    assert_eq!(state.project_at(T)?.heads.len(), 1);

    let (root, a, node, mut state) = setup()?;
    let mut parent = vec![];
    for epoch in [1, 2] {
        if epoch == 2 {
            state.checkpoints.push(RootCheckpoint::rotation(
                &state,
                &root,
                &id(&identity()),
                T,
            )?);
        }
        for _ in 0..MAX_MANAGEMENT_DEPTH {
            let event = signed_at(
                &state,
                &a,
                epoch,
                parent.clone(),
                GovernanceAction::Grant {
                    member: Member::node(&node, "node")?,
                },
            )?;
            parent = vec![event.hash()?];
            state.events.push(event);
        }
    }
    let mut canonical = state.clone();
    canonical.events.clear();
    state = canonical.merge(&state)?;
    assert_eq!(state.project_at(T)?.heads, parent);
    let before = state.clone();
    let too_deep = signed_at(
        &state,
        &a,
        2,
        parent,
        GovernanceAction::Grant {
            member: Member::node(&node, "too deep")?,
        },
    )?;
    assert!(state.append(too_deep).is_err());
    assert_eq!(state, before);
    let parents = state
        .events
        .iter()
        .filter(|event| event.epoch == 1)
        .take(65)
        .map(GovernanceEvent::hash)
        .collect::<Result<Vec<_>>>()?;
    let mut parents = parents;
    parents.sort();
    let too_wide = signed_at(
        &state,
        &a,
        2,
        parents,
        GovernanceAction::Grant {
            member: Member::node(&node, "too wide")?,
        },
    )?;
    assert!(state.append(too_wide).is_err());
    assert_eq!(state, before);
    Ok(())
}

#[test]
fn a_new_explicit_directory_rejects_old_scope_and_future_parents() -> Result<()> {
    let (root, a, node, mut state) = setup()?;
    state.append(GovernanceEvent::grant(
        &a,
        &state.project_at(T)?,
        Member::node(&node, "renamed")?,
    )?)?;
    let old = state.events[0].hash()?;
    let (manager, _, checkpoint) = recovering(&state, &root, &node, T)?;
    state.recover_at(checkpoint, T)?;
    let event = signed_at(
        &state,
        &manager,
        2,
        vec![old],
        GovernanceAction::Grant {
            member: Member::client(&identity(), "foreign parent", false)?,
        },
    )?;
    let before = state.clone();
    assert!(state.append(event).is_err());
    assert_eq!(state, before);
    let valid = signed_at(
        &state,
        &manager,
        2,
        vec![],
        GovernanceAction::Grant {
            member: Member::client(&identity(), "new scope", false)?,
        },
    )?;
    state.append(valid.clone())?;
    let future = signed_at(
        &state,
        &a,
        1,
        vec![valid.hash()?],
        GovernanceAction::Grant {
            member: Member::client(&identity(), "future parent", false)?,
        },
    )?;
    let before = state.clone();
    assert!(state.append(future).is_err());
    assert_eq!(state, before);
    Ok(())
}

#[test]
fn raw_invalid_removals_and_fake_nodes_do_not_gain_projection_sources() -> Result<()> {
    let (root, a, b, node, base) = two_managers()?;
    let fake = identity();
    let mut state = base.clone();
    state.append(GovernanceEvent::revoke(
        &b,
        &base.project_at(T)?,
        cut(&a, 0),
    )?)?;
    state.append(GovernanceEvent::revoke(
        &a,
        &base.project_at(T)?,
        cut(&node, 7),
    )?)?;
    state.append(GovernanceEvent::grant(
        &a,
        &base.project_at(T)?,
        Member::node(&fake, "fake")?,
    )?)?;
    let before = state.project_at(T)?;
    assert!(!before.signed_removals().contains(&id(&node)));
    let kept = vec![Member::client(&b, "b", true)?];
    let checkpoint = RootCheckpoint::recovery(
        &state,
        &root,
        &id(&identity()),
        kept,
        vec![cut(&a, 0), cut(&node, 7), cut(&fake, 3)],
        T,
    )?;
    state.recover_at(checkpoint, T)?;
    let view = state.project_at(T + RECOVERY_DELAY_MS)?;
    assert!(view.removed_node(&id(&node)));
    assert!(!view.removed_node(&id(&a)) && !view.removed_node(&id(&fake)));
    assert!(!view.signed_removals().contains(&id(&node)));
    assert!(!view.signed_removals().contains(&id(&fake)));
    assert_eq!(view.write_bound(&id(&fake)), None);
    assert_eq!(view.write_bound(&id(&a)), None);
    let mut daily = base.clone();
    daily.append(GovernanceEvent::revoke(
        &b,
        &base.project_at(T)?,
        cut(&node, 7),
    )?)?;
    assert!(daily.project_at(T)?.signed_removals().contains(&id(&node)));
    Ok(())
}

#[test]
fn checkpoint_conflicts_fail_closed_in_both_merge_directions() -> Result<()> {
    let (root, _, node, base) = setup()?;
    let (_, _, recovery) = recovering(&base, &root, &node, T)?;
    let (_, _, other_recovery) = recovering(&base, &root, &node, T)?;
    let rotation = RootCheckpoint::rotation(&base, &root, &id(&identity()), T)?;
    let other_rotation = RootCheckpoint::rotation(&base, &root, &id(&identity()), T)?;
    for (left, right) in [
        (recovery.clone(), other_recovery),
        (rotation.clone(), other_rotation),
        (recovery, rotation),
    ] {
        let (mut l, mut r) = (base.clone(), base.clone());
        l.recover_at(left, T)?;
        r.recover_at(right, T)?;
        let before = (l.clone(), r.clone());
        for error in [l.merge(&r).unwrap_err(), r.merge(&l).unwrap_err()] {
            assert!(format!("{error:#}").contains("conflicting recovery checkpoint at epoch 2"));
        }
        assert_eq!((l.clone(), r.clone()), before);
        assert_eq!(l.merge(&l)?, l);
        assert_eq!(r.merge(&r)?, r);
    }
    Ok(())
}

#[test]
fn recovery_capacity_diagnostics_preserve_the_existing_directory() -> Result<()> {
    let (mut root, _, _, mut state) = setup()?;
    for epoch in 2..=MAX_CHECKPOINTS as u64 + 1 {
        let next = identity();
        state.checkpoints.push(RootCheckpoint::create(
            state.network_id(),
            epoch,
            &root,
            &id(&next),
            None,
            vec![],
            T,
        )?);
        root = next;
    }
    let before = state.clone();
    let view = state.project_at(T)?;
    let next = RootCheckpoint::rotation(&state, &root, &id(&identity()), T)?;
    refused(
        state.recover_at(next, T),
        "active recovery checkpoint limit reached",
    );
    assert_eq!(state, before);
    assert_eq!(state.project_at(T)?, view);
    let mut oversized = state.clone();
    oversized
        .checkpoints
        .resize(MAX_CHECKPOINT_RECORDS + 1, state.checkpoints[0].clone());
    refused(
        oversized.project_at(T),
        "recovery checkpoint record limit reached",
    );
    assert_eq!(state, before);
    Ok(())
}

#[test]
fn effect_gates_preserve_unknown_and_absorbing_constants() {
    assert_eq!(Effect::Unknown.not(), Effect::Unknown);
    assert_eq!(Effect::No.and(Effect::Unknown), Effect::No);
    assert_eq!(Effect::Yes.or(Effect::Unknown), Effect::Yes);
    for effect in [Effect::No, Effect::Unknown, Effect::Yes] {
        assert_eq!(effect.and(Effect::Yes), effect);
        assert_eq!(effect.or(Effect::No), effect);
    }
}

#[test]
fn revocation_wins_and_records_the_largest_cutoff() -> Result<()> {
    let (_, a, node, base) = setup()?;
    let b = identity();
    let mut with_b = base.clone();
    with_b.append(GovernanceEvent::grant(
        &a,
        &with_b.project()?,
        Member::client(&b, "b", true)?,
    )?)?;
    let mut left = with_b.clone();
    let mut right = with_b.clone();
    left.append(GovernanceEvent::grant(
        &b,
        &left.project()?,
        Member::node(&node, "renamed")?,
    )?)?;
    right.append(GovernanceEvent::revoke(
        &a,
        &right.project()?,
        cut(&node, 7),
    )?)?;
    let mut merged = left.merge(&right)?;
    assert_eq!(merged.project()?, right.merge(&left)?.project()?);
    assert!(merged.project()?.node(&id(&node)).is_err());
    assert_eq!(merged.project()?.cutoff(&id(&node)), Some(7));
    merged.append(GovernanceEvent::revoke(
        &a,
        &merged.project()?,
        cut(&node, 3),
    )?)?;
    assert_eq!(merged.project()?.cutoff(&id(&node)), Some(7));
    let same_seq = Revocation::new(&id(&node), Some((7, &"f".repeat(64))))?;
    merged.append(GovernanceEvent::revoke(
        &a,
        &merged.project()?,
        same_seq.clone(),
    )?)?;
    assert_eq!(merged.project()?.revocation(&id(&node)), Some(&same_seq));
    merged.append(GovernanceEvent::grant(
        &a,
        &merged.project()?,
        Member::node(&node, "rejoin")?,
    )?)?;
    assert!(merged.project()?.node(&id(&node)).is_err());
    merged.append(GovernanceEvent::revoke(&b, &merged.project()?, cut(&a, 0))?)?;
    let view = merged.project()?;
    assert!(GovernanceEvent::revoke(&a, &view, cut(&b, 0)).is_err());
    let last = GovernanceEvent::revoke(&b, &view, cut(&b, 0)).unwrap_err();
    assert!(format!("{last:#}").contains(LAST_MANAGER));
    Ok(())
}

#[test]
fn concurrent_grants_keep_the_narrower_permission() -> Result<()> {
    let (_, a, _, base) = setup()?;
    let b = identity();
    let c = identity();
    let mut with_b = base;
    with_b.append(GovernanceEvent::grant(
        &a,
        &with_b.project()?,
        Member::client(&b, "b", true)?,
    )?)?;
    let mut left = with_b.clone();
    let mut right = with_b;
    left.append(GovernanceEvent::grant(
        &a,
        &left.project()?,
        Member::client(&c, "zeta", true)?,
    )?)?;
    right.append(GovernanceEvent::grant(
        &b,
        &right.project()?,
        Member::client(&c, "alpha", false)?,
    )?)?;
    let member = left.merge(&right)?.project()?.client(&id(&c))?;
    assert!(!member.manage);
    assert_eq!(member.name, "alpha");
    Ok(())
}

#[test]
fn device_kind_and_root_key_are_fixed() -> Result<()> {
    let (root, a, node, mut state) = setup()?;
    assert!(
        state
            .append(GovernanceEvent::grant(
                &a,
                &state.project()?,
                Member::client(&node, "client", false)?
            )?)
            .is_err()
    );
    assert!(
        state
            .append(GovernanceEvent::grant(
                &a,
                &state.project()?,
                Member::client(&root, "root", false)?
            )?)
            .is_err()
    );
    assert!(
        Genesis::create(
            &root,
            Member::client(&root, "r", true)?,
            Member::node(&node, "n")?
        )
        .is_err()
    );
    let next = identity();
    let keeps =
        |member: Member| -> Result<Vec<Member>> { Ok(vec![member, Member::node(&node, "n")?]) };
    assert!(
        RootCheckpoint::recovery(
            &state,
            &root,
            &id(&next),
            keeps(Member::client(&next, "n", true)?)?,
            vec![],
            0
        )
        .is_err(),
        "the next recovery key cannot be a daily device"
    );
    let keeps_root = RootCheckpoint::recovery(
        &state,
        &root,
        &id(&next),
        keeps(Member::client(&root, "r", true)?)?,
        vec![],
        0,
    )?;
    assert!(state.clone().recover(keeps_root).is_err());
    assert!(RootCheckpoint::rotation(&state, &root, &id(&root), now_ms()).is_err());
    Ok(())
}

#[test]
fn recovery_replaces_members_and_rejects_rollback_or_foreign_roots() -> Result<()> {
    let (root, a, node, mut state) = setup()?;
    let original = state.clone();
    let recovered = identity();
    let members = vec![
        Member::client(&recovered, "recovered", true)?,
        Member::node(&node, "node")?,
    ];
    let next = identity();
    let checkpoint =
        RootCheckpoint::recovery(&state, &root, &id(&next), members.clone(), vec![], 0)?;
    assert!(state.recover(checkpoint.clone())?);
    assert!(!state.recover(checkpoint)?);
    assert!(state.project()?.manager(&id(&recovered)).is_ok());
    assert!(state.project()?.client(&id(&a)).is_err());
    assert_eq!(state.project()?.recovery_key, id(&next));
    assert_eq!(state.merge(&original)?, state);
    assert_eq!(original.merge(&state)?, state);
    let mut conflict = original.clone();
    conflict.recover(RootCheckpoint::recovery(
        &original,
        &root,
        &id(&identity()),
        vec![Member::client(&a, "conflicting", true)?],
        vec![],
        0,
    )?)?;
    assert!(state.merge(&conflict).is_err());
    let foreign = identity();
    assert!(
        state
            .recover(RootCheckpoint::recovery(
                &state,
                &foreign,
                &id(&identity()),
                members.clone(),
                vec![],
                0
            )?)
            .is_err()
    );
    assert!(
        RootCheckpoint::recovery(
            &state,
            &next,
            &id(&identity()),
            vec![Member::node(&node, "n")?],
            vec![],
            0
        )
        .is_err()
    );
    assert!(
        RootCheckpoint::recovery(
            &state,
            &next,
            &id(&identity()),
            members.clone(),
            vec![cut(&node, 2)],
            0
        )
        .is_err(),
        "a retired device cannot stay a member"
    );
    let mut renamed = members;
    renamed[0].name = "renamed".into();
    assert!(
        state
            .recover(RootCheckpoint::recovery(
                &original,
                &root,
                &id(&next),
                renamed,
                vec![],
                0
            )?)
            .is_err()
    );
    Ok(())
}

#[test]
fn a_superseded_recovery_key_signs_nothing_anywhere_in_the_chain() -> Result<()> {
    let (root, a, node, mut state) = setup()?;
    let genesis_only = state.clone();
    let next = identity();
    let keep = vec![Member::client(&a, "a", true)?, Member::node(&node, "n")?];
    state.recover(RootCheckpoint::recovery(
        &state,
        &root,
        &id(&next),
        keep.clone(),
        vec![],
        0,
    )?)?;
    let now = now_ms();
    let refused = state.check_root(&id(&root), now, false).unwrap_err();
    assert!(format!("{refused:#}").contains(SUPERSEDED));
    state.check_root(&id(&next), now, false)?;
    assert!(state.check_root(&id(&identity()), now, false).is_err());

    let stolen = RootCheckpoint::rotation(&state, &root, &id(&identity()), now_ms())?;
    let refused = state.clone().recover(stolen.clone()).unwrap_err();
    assert!(format!("{refused:#}").contains("replaced by a newer recovery"));
    let mut forked = state.clone();
    forked.checkpoints.push(stolen);
    assert!(forked.project().is_err() && state.merge(&forked).is_err());

    let mut rival = genesis_only.clone();
    rival.recover(RootCheckpoint::recovery(
        &genesis_only,
        &root,
        &id(&identity()),
        keep.clone(),
        vec![],
        0,
    )?)?;
    assert!(state.merge(&rival).is_err() && rival.merge(&state).is_err());

    let skip = RootCheckpoint::create(
        state.network_id(),
        4,
        &next,
        &id(&identity()),
        None,
        vec![],
        0,
    )?;
    assert!(state.clone().recover(skip.clone()).is_err());
    let mut gap = state.clone();
    gap.checkpoints.push(skip);
    assert!(gap.project().is_err());

    let verified = verify_membership(&serde_json::to_string(&state)?, &id(&root), None)?;
    let error = verified
        .create_key_rotation(&root, &id(&identity()))
        .unwrap_err();
    assert!(format!("{error:?}").contains("replaced by a newer recovery"));
    assert!(
        verified
            .create_key_rotation(&identity(), &id(&identity()))
            .is_err()
    );
    let rotated: RootCheckpoint =
        serde_json::from_str(&verified.create_key_rotation(&next, &id(&identity()))?)?;
    assert!(state.recover(rotated)?);
    let refused = state.check_root(&id(&next), now_ms(), false).unwrap_err();
    assert!(format!("{refused:#}").contains(SUPERSEDED));
    Ok(())
}

#[test]
fn a_key_rotation_keeps_the_directory_and_late_revocations() -> Result<()> {
    let (root, a, node, mut state) = setup()?;
    let b = identity();
    state.append(GovernanceEvent::grant(
        &a,
        &state.project()?,
        Member::client(&b, "b", true)?,
    )?)?;
    let mut late = state.clone();
    late.append(GovernanceEvent::revoke(
        &b,
        &late.project()?,
        cut(&node, 7),
    )?)?;
    let next = identity();
    state.recover(RootCheckpoint::rotation(
        &state,
        &root,
        &id(&next),
        now_ms(),
    )?)?;
    let view = state.project()?;
    assert_eq!(
        (view.epoch, view.recovery_key.as_str()),
        (2, id(&next).as_str()),
        "a rotation takes effect at once"
    );
    assert!(view.pending.is_none());
    assert!(view.manager(&id(&b)).is_ok() && view.node(&id(&node)).is_ok());

    state.append(GovernanceEvent::grant(
        &a,
        &view,
        Member::client(&identity(), "c", false)?,
    )?)?;
    let merged = state.merge(&late)?;
    assert_eq!(merged, late.merge(&state)?);
    let view = merged.project()?;
    assert_eq!(view.epoch, 2);
    assert_eq!(
        view.write_bound(&id(&node)),
        Some(WriteBound::Cutoff(&cut(&node, 7))),
        "a revocation the rotation never saw still counts"
    );
    assert_eq!(view.members.len(), 3);
    Ok(())
}

#[test]
fn missing_parents_wrong_epoch_and_tampering_fail_closed() -> Result<()> {
    let (_, a, node, mut state) = setup()?;
    let valid = GovernanceEvent::revoke(&a, &state.project()?, cut(&node, 1))?;
    let mut missing = valid.clone();
    missing.parents = vec!["0".repeat(64)];
    assert!(state.append(missing.sign(&a)?).is_err());
    let mut epoch = valid.clone();
    epoch.epoch += 1;
    assert!(state.append(epoch.sign(&a)?).is_err());
    let mut tampered = valid.clone();
    tampered.action = GovernanceAction::Revoke(cut(&a, 1));
    assert!(state.append(tampered).is_err());
    let mut changed = state.genesis.clone();
    changed.first_client.name = "other".into();
    assert!(changed.verify().is_err());
    assert!(state.append(valid.clone())?);
    assert!(!state.append(valid)?);
    Ok(())
}

#[test]
fn canonical_signatures_have_shared_browser_encoding() -> Result<()> {
    assert_eq!(
        canonical_json(&serde_json::json!({"z":1,"a":{"b":2,"a":true}}))?,
        br#"{"a":{"a":true,"b":2},"z":1}"#
    );
    assert!(canonical_json(&serde_json::json!({"n":9_007_199_254_740_992u64})).is_err());
    assert!(canonical_json(&serde_json::json!({"n":1.5})).is_err());
    Ok(())
}

#[test]
fn browser_helpers_verify_before_building_or_signing() -> Result<()> {
    let (root, a, node, state) = setup()?;
    let state_json = serde_json::to_string(&state)?;
    let verified = verify_membership(&state_json, &id(&root), None)?;
    assert!(verify_membership(&state_json, &id(&a), None).is_err());
    let mut forged = state.clone();
    forged.genesis.signature = "00".repeat(64);
    assert!(verify_membership(&serde_json::to_string(&forged)?, &id(&root), None).is_err());

    let action = serde_json::json!({"type":"revoke","device_id":id(&node),
        "cutoff_seq":4,"cutoff_hash":hash(4)});
    assert_eq!(
        serde_json::to_value(GovernanceAction::Revoke(cut(&node, 4)))?,
        action
    );
    let unhashed = serde_json::json!({"type":"revoke","device_id":id(&node),"cutoff_seq":4});
    assert!(
        verified
            .governance_template(&id(&a), &unhashed.to_string())
            .is_err()
    );
    let template = verified.governance_template(&id(&a), &action.to_string())?;
    assert!(
        verified
            .governance_template(&id(&node), &action.to_string())
            .is_err()
    );
    let mut event: GovernanceEvent = serde_json::from_str(&template)?;
    let payload = governance_payload(&template)?;
    let message = crate::signing_message(GOVERNANCE_DOMAIN, &payload)?;
    event.signature = a.sign_message(&message);
    let mut next = state.clone();
    next.append(event)?;
    assert_eq!(next.project()?.cutoff(&id(&node)), Some(4));

    let members = serde_json::to_string(&vec![Member::client(&a, "a", true)?])?;
    let heads = serde_json::json!({ id(&node): [9, hash(9)] }).to_string();
    let next = id(&identity());
    let signed = verified.create_recovery_checkpoint(&root, &next, &members, &heads)?;
    let checkpoint: RootCheckpoint = serde_json::from_str(&signed)?;
    checkpoint.verify(&state.genesis, &id(&root))?;
    assert_eq!((checkpoint.epoch, &checkpoint.recovery_key), (2, &next));
    assert_eq!(
        checkpoint.directory.map(|directory| directory.retired),
        Some(vec![cut(&node, 9)])
    );
    let signed = verified.create_recovery_checkpoint(&root, &next, &members, "{}")?;
    let checkpoint: RootCheckpoint = serde_json::from_str(&signed)?;
    assert_eq!(
        checkpoint.directory.map(|directory| directory.retired),
        Some(vec![cut(&node, 0)])
    );
    assert!(
        verified
            .create_recovery_checkpoint(&a, &next, &members, &heads)
            .is_err()
    );
    assert!(
        verified
            .create_recovery_checkpoint(&root, &id(&root), &members, &heads)
            .is_err()
    );
    let genesis = create_genesis(
        &root,
        &serde_json::to_string(&Member::client(&a, "a", true)?)?,
        &serde_json::to_string(&Member::node(&node, "n")?)?,
    )?;
    serde_json::from_str::<Genesis>(&genesis)?.verify()?;
    Ok(())
}

#[test]
fn merge_is_a_union_in_any_order() -> Result<()> {
    let (root, a, node, base) = setup()?;
    let b = identity();
    let c = identity();
    let mut granted = base.clone();
    granted.append(GovernanceEvent::grant(
        &a,
        &granted.project()?,
        Member::client(&b, "b", true)?,
    )?)?;
    let mut revoked = base.clone();
    revoked.append(GovernanceEvent::revoke(
        &a,
        &revoked.project()?,
        cut(&node, 2),
    )?)?;
    let mut recovered = base.clone();
    recovered.recover(RootCheckpoint::recovery(
        &base,
        &root,
        &id(&identity()),
        vec![Member::client(&a, "a", true)?, Member::node(&node, "node")?],
        vec![],
        0,
    )?)?;
    recovered.append(GovernanceEvent::grant(
        &a,
        &recovered.project()?,
        Member::client(&c, "c", false)?,
    )?)?;
    let parts = [&base, &granted, &revoked, &recovered];
    let mut results = Vec::new();
    for order in [[0, 1, 2, 3], [3, 2, 1, 0], [2, 0, 3, 1], [1, 3, 0, 2]] {
        let mut state = parts[order[0]].clone();
        for index in &order[1..] {
            state = state.merge(parts[*index])?;
        }
        assert_eq!(state.merge(&state)?, state);
        results.push(state);
    }
    assert!(results.windows(2).all(|pair| pair[0] == pair[1]));
    let state = &results[0];
    assert_eq!(state.epoch(), 2);
    assert_eq!(state.events.len(), 3);
    let view = state.project()?;
    assert!(view.client(&id(&b)).is_err() && view.attester(&id(&b)));
    assert_eq!(view.write_bound(&id(&node)), Some(WriteBound::Unlimited));
    assert!(view.client(&id(&c)).is_ok());
    Ok(())
}

#[test]
fn recovery_keeps_history_retires_left_out_nodes_and_old_events_cannot_widen() -> Result<()> {
    let (root, a, node, mut state) = setup()?;
    let b = identity();
    let kept = identity();
    let left = identity();
    let gone = identity();
    for member in [
        Member::client(&b, "b", false)?,
        Member::node(&kept, "kept")?,
        Member::node(&left, "left")?,
        Member::node(&gone, "gone")?,
    ] {
        state.append(GovernanceEvent::grant(&a, &state.project()?, member)?)?;
    }
    state.append(GovernanceEvent::revoke(
        &a,
        &state.project()?,
        cut(&gone, 3),
    )?)?;
    let epoch_one = state.clone();
    let recovered = identity();
    let members = vec![
        Member::client(&recovered, "recovered", true)?,
        Member::node(&kept, "kept")?,
    ];
    let heads = serde_json::json!({ id(&left): [5, hash(5)], id(&kept): [8, hash(8)] });
    let verified = verify_membership(&serde_json::to_string(&state)?, &id(&root), Some(0.0))?;
    let checkpoint: RootCheckpoint = serde_json::from_str(&verified.create_recovery_checkpoint(
        &root,
        &id(&identity()),
        &serde_json::to_string(&members)?,
        &heads.to_string(),
    )?)?;
    assert_eq!(
        checkpoint.directory.as_ref().map(|d| d.retired.clone()),
        Some({
            let mut retired = vec![cut(&gone, 3), cut(&left, 5), cut(&node, 0)];
            retired.sort_by(|x, y| x.device_id.cmp(&y.device_id));
            retired
        })
    );
    state.recover(checkpoint)?;
    assert_eq!(state.events.len(), 5);
    assert_eq!(state.checkpoints.len(), 1);
    let view = state.project()?;
    assert_eq!(view.epoch, 2);
    assert_eq!(view.write_bound(&id(&kept)), Some(WriteBound::Unlimited));
    assert_eq!(
        view.write_bound(&id(&left)),
        Some(WriteBound::Cutoff(&cut(&left, 5)))
    );
    assert_eq!(
        view.write_bound(&id(&gone)),
        Some(WriteBound::Cutoff(&cut(&gone, 3)))
    );
    assert_eq!(view.write_bound(&id(&identity())), None);
    for client in [&a, &b, &recovered] {
        assert!(view.attester(&id(client)));
    }
    assert!(!view.attester(&id(&kept)));
    assert!(view.client(&id(&a)).is_err());

    state.append(GovernanceEvent::grant(
        &recovered,
        &view,
        Member::node(&left, "back")?,
    )?)?;
    assert!(state.project()?.node(&id(&left)).is_err());

    let mut late = epoch_one.clone();
    late.append(GovernanceEvent::revoke(
        &a,
        &late.project()?,
        cut(&left, 9),
    )?)?;
    let merged = state.merge(&late)?;
    assert_eq!(merged, late.merge(&state)?);
    assert_eq!(
        merged.project()?.revocation(&id(&left)),
        Some(&cut(&left, 5))
    );
    Ok(())
}

/// Signed at a fixed time, so each test picks the moment it projects at.
const T: u64 = 1_800_000_000_000;

/// A recovery of `state` that keeps only `node` and adds a new manager, signed at `issued`.
fn recovering(
    state: &NetworkState,
    root: &Identity,
    node: &Identity,
    issued: u64,
) -> Result<(Identity, Identity, RootCheckpoint)> {
    let (manager, next) = (identity(), identity());
    let members = vec![
        Member::client(&manager, "found", true)?,
        Member::node(node, "node")?,
    ];
    let checkpoint = RootCheckpoint::recovery(state, root, &id(&next), members, vec![], issued)?;
    Ok((manager, next, checkpoint))
}

fn veto(
    admin: &Identity,
    view: &AuthorizationView,
    checkpoint: &RootCheckpoint,
    at_ms: u64,
) -> Result<GovernanceEvent> {
    let checkpoint = checkpoint.hash()?;
    GovernanceEvent::unsigned(
        view,
        &id(admin),
        GovernanceAction::Veto { checkpoint, at_ms },
    )?
    .sign(admin)
}

fn approve(
    admin: &Identity,
    view: &AuthorizationView,
    checkpoint: &RootCheckpoint,
    at_ms: u64,
) -> Result<GovernanceEvent> {
    let checkpoint = checkpoint.hash()?;
    GovernanceEvent::unsigned(
        view,
        &id(admin),
        GovernanceAction::Approve { checkpoint, at_ms },
    )?
    .sign(admin)
}

fn refused(result: Result<impl std::fmt::Debug>, reason: &str) {
    let error = format!("{:#}", result.expect_err(reason));
    assert!(error.contains(reason), "{error}");
}

#[test]
fn a_waiting_recovery_takes_effect_after_the_delay() -> Result<()> {
    let (root, a, node, mut state) = setup()?;
    let (found, next, checkpoint) = recovering(&state, &root, &node, T)?;
    assert!(state.recover_at(checkpoint.clone(), T)?);
    let due = T + RECOVERY_DELAY_MS;
    let before = state.project_at(due - 1)?;
    assert_eq!(
        (before.epoch, before.recovery_key.as_str()),
        (1, id(&root).as_str())
    );
    assert!(before.manager(&id(&a)).is_ok() && before.client(&id(&found)).is_err());
    assert!(!before.attester(&id(&found)));
    let pending = before.pending.context("the recovery waits")?;
    assert_eq!(pending.checkpoint, checkpoint.hash()?);
    assert_eq!(pending.effective_at_ms, due);
    assert!(pending.members.iter().any(|m| m.device_id == id(&found)));

    // While it waits the old file starts no other recovery and the new file signs nothing;
    // only a rotation by the old file is free (see the next test).
    refused(state.check_root(&id(&root), T, false), RECOVERY_WAITING);
    state.check_root(&id(&root), T, true)?;
    refused(state.check_root(&id(&next), T, true), RECOVERY_WAITING);
    let (_, _, again) = recovering(&state, &root, &node, T)?;
    refused(state.clone().recover_at(again, T), RECOVERY_WAITING);
    let stacked = RootCheckpoint::rotation(&state, &next, &id(&identity()), T)?;
    refused(state.clone().recover_at(stacked, T), RECOVERY_WAITING);

    let after = state.project_at(due)?;
    assert_eq!(
        (after.epoch, after.recovery_key.as_str()),
        (2, id(&next).as_str())
    );
    assert!(after.manager(&id(&found)).is_ok() && after.client(&id(&a)).is_err());
    assert!(after.pending.is_none() && after.stopped.is_empty());
    assert!(after.attester(&id(&found)) && after.attester(&id(&a)));
    state.check_root(&id(&next), due, false)?;
    refused(state.check_root(&id(&root), due, false), SUPERSEDED);
    Ok(())
}

#[test]
fn a_manager_approval_makes_a_waiting_recovery_live_immediately() -> Result<()> {
    let (root, a, node, mut state) = setup()?;
    let (found, _, checkpoint) = recovering(&state, &root, &node, T)?;
    state.recover_at(checkpoint.clone(), T)?;
    let before = state.project_at(T + 1)?;
    assert_eq!(before.epoch, 1);
    let event = approve(&a, &before, &checkpoint, T + 1)?;
    assert!(state.append(event)?);

    assert_eq!(state.project_at(T)?.epoch, 1);
    assert_eq!(state.project_at(T)?.pending.unwrap().effective_at_ms, T + 1);
    let view = state.project_at(T + 2)?;
    assert_eq!(view.epoch, 2);
    assert!(view.manager(&id(&found)).is_ok());
    assert!(view.pending.is_none());
    Ok(())
}

#[test]
fn approvals_are_idempotent_and_merge_in_any_order() -> Result<()> {
    let (root, a, node, mut state) = setup()?;
    let (_, _, checkpoint) = recovering(&state, &root, &node, T)?;
    state.recover_at(checkpoint.clone(), T)?;
    let event = approve(&a, &state.project_at(T)?, &checkpoint, T + 1)?;
    let mut approved = state.clone();
    assert!(approved.append(event.clone())?);
    assert!(!approved.append(event)?);

    let merged = state.merge(&approved)?;
    assert_eq!(merged, approved);
    assert_eq!(approved.merge(&state)?, merged);
    assert_eq!(merged.project_at(T)?.epoch, 1);
    assert_eq!(merged.project_at(T + 2)?.epoch, 2);
    Ok(())
}

#[test]
fn an_approval_needs_the_recovery_management_heads() -> Result<()> {
    let (root, manager, node, mut state) = setup()?;
    state.append(GovernanceEvent::grant(
        &manager,
        &state.project_at(T)?,
        Member::client(&identity(), "other", true)?,
    )?)?;
    let (_, _, checkpoint) = recovering(&state, &root, &node, T)?;
    state.recover_at(checkpoint.clone(), T)?;
    let valid = approve(&manager, &state.project_at(T)?, &checkpoint, T + 1)?;
    let mut missing = valid.clone();
    missing.parents.clear();
    state.append(missing.sign(&manager)?)?;
    assert_eq!(state.project_at(T + 2)?.epoch, 1);
    assert_eq!(
        state.project_at(T + 2)?.pending.unwrap().checkpoint,
        checkpoint.hash()?
    );
    state.append(valid)?;
    assert_eq!(state.project_at(T + 2)?.epoch, 2);
    Ok(())
}

#[test]
fn veto_takes_priority_over_a_concurrent_approval() -> Result<()> {
    let (root, a, node, base) = setup()?;
    let (_, _, checkpoint) = recovering(&base, &root, &node, T)?;
    let mut recovered = base.clone();
    recovered.recover_at(checkpoint.clone(), T)?;
    let view = recovered.project_at(T)?;
    let approval = approve(&a, &view, &checkpoint, T + 1)?;
    let veto = veto(&a, &view, &checkpoint, T + 1)?;
    let mut approved = recovered.clone();
    approved.append(approval)?;
    let mut vetoed = recovered;
    vetoed.append(veto)?;

    let merged = approved.merge(&vetoed)?;
    for at in [T + 2, T + RECOVERY_DELAY_MS] {
        let view = merged.project_at(at)?;
        assert_eq!(view.epoch, 1);
        assert_eq!(view.stopped, vec![checkpoint.hash()?]);
    }
    Ok(())
}

#[test]
fn approvals_reject_wrong_target_non_managers_and_missing_pending_recovery() -> Result<()> {
    let (root, a, node, mut state) = setup()?;
    let (_, _, checkpoint) = recovering(&state, &root, &node, T)?;
    state.recover_at(checkpoint.clone(), T)?;
    let view = state.project_at(T)?;

    refused(
        GovernanceEvent::unsigned(
            &view,
            &id(&a),
            GovernanceAction::Approve {
                checkpoint: hash(7),
                at_ms: T + 1,
            },
        ),
        NO_RECOVERY_WAITING,
    );
    refused(
        GovernanceEvent::unsigned(
            &view,
            &id(&node),
            GovernanceAction::Approve {
                checkpoint: checkpoint.hash()?,
                at_ms: T + 1,
            },
        ),
        "management permission required",
    );
    refused(
        GovernanceEvent::unsigned(
            &view,
            &id(&a),
            GovernanceAction::Approve {
                checkpoint: checkpoint.hash()?,
                at_ms: T + RECOVERY_DELAY_MS,
            },
        ),
        "this recovery is already due",
    );

    let due = T + RECOVERY_DELAY_MS;
    let after = state.project_at(due)?;
    assert!(
        GovernanceEvent::unsigned(
            &after,
            &id(&a),
            GovernanceAction::Approve {
                checkpoint: checkpoint.hash()?,
                at_ms: T + 1,
            },
        )
        .is_err()
    );
    Ok(())
}

#[test]
fn a_rotation_retires_a_waiting_recovery_in_any_order() -> Result<()> {
    let (root, a, node, base) = setup()?;
    let (_, next, checkpoint) = recovering(&base, &root, &node, T)?;
    let mut waiting = base.clone();
    waiting.recover_at(checkpoint.clone(), T)?;
    let rotated_key = identity();
    let rotation = RootCheckpoint::rotation(&waiting, &root, &id(&rotated_key), T + 1)?;
    let mut rotated = waiting.clone();
    assert!(rotated.recover_at(rotation.clone(), T + 1)?);
    for at in [T + 1, T + RECOVERY_DELAY_MS, T * 2] {
        let view = rotated.project_at(at)?;
        assert_eq!(
            (view.epoch, view.recovery_key.as_str()),
            (3, id(&rotated_key).as_str())
        );
        assert!(view.manager(&id(&a)).is_ok() && view.pending.is_none());
        assert_eq!(view.stopped, vec![checkpoint.hash()?]);
    }
    refused(
        rotated.check_root(&id(&next), T + 1, false),
        RECOVERY_STOPPED,
    );
    refused(rotated.check_root(&id(&root), T + 1, true), SUPERSEDED);
    assert_eq!(waiting.merge(&rotated)?, rotated.merge(&waiting)?);

    // Signed once the recovery took effect, a rotation of the old file is no branch at all.
    let due = T + RECOVERY_DELAY_MS;
    let mut late = waiting.clone();
    late.checkpoints.push(RootCheckpoint::rotation(
        &waiting,
        &root,
        &id(&identity()),
        due,
    )?);
    refused(late.project_at(due), "earlier recoveries were vetoed");
    // Dated back, it is one, and it wins wherever it arrives.
    assert_eq!(waiting.project_at(due)?.recovery_key, id(&next));
    assert_eq!(
        waiting.merge(&rotated)?.project_at(due)?.recovery_key,
        id(&rotated_key)
    );
    Ok(())
}

#[test]
fn a_veto_keeps_the_directory_and_the_old_file() -> Result<()> {
    let (root, a, node, mut state) = setup()?;
    let (found, next, checkpoint) = recovering(&state, &root, &node, T)?;
    state.recover_at(checkpoint.clone(), T)?;
    // The browser builds it from the Node's clock.
    let verified = verify_membership(&serde_json::to_string(&state)?, &id(&root), Some(T as f64))?;
    let action = serde_json::json!({"type":"veto","checkpoint":checkpoint.hash()?,"at_ms":T + 1});
    let mut event: GovernanceEvent =
        serde_json::from_str(&verified.governance_template(&id(&a), &action.to_string())?)?;
    event.signature = a.sign_message(&crate::signing_message(
        GOVERNANCE_DOMAIN,
        &event.signing_bytes()?,
    )?);
    assert!(state.append(event)?);
    for at in [T, T + RECOVERY_DELAY_MS, T * 2] {
        let view = state.project_at(at)?;
        assert_eq!(
            (view.epoch, view.recovery_key.as_str()),
            (1, id(&root).as_str())
        );
        assert!(view.manager(&id(&a)).is_ok() && view.client(&id(&found)).is_err());
        assert!(view.pending.is_none() && !view.attester(&id(&found)));
        assert_eq!(view.stopped, vec![checkpoint.hash()?]);
    }
    state.check_root(&id(&root), T, false)?;
    refused(state.check_root(&id(&next), T, false), RECOVERY_STOPPED);
    let mut stacked = state.clone();
    stacked.checkpoints.push(RootCheckpoint::rotation(
        &state,
        &next,
        &id(&identity()),
        T,
    )?);
    assert_eq!(
        stacked.project_at(T + RECOVERY_DELAY_MS)?.stopped.len(),
        2,
        "built on it, void too"
    );
    refused(
        state.clone().recover(stacked.checkpoints[1].clone()),
        RECOVERY_STOPPED,
    );

    // The old file still rotates at once, or starts another recovery.
    let mut rotated = state.clone();
    rotated.recover_at(
        RootCheckpoint::rotation(&state, &root, &id(&identity()), T)?,
        T,
    )?;
    assert_eq!(rotated.project_at(T)?.epoch, 3);
    let (_, _, again) = recovering(&state, &root, &node, T)?;
    assert!(state.recover_at(again, T)?);
    assert!(state.project_at(T)?.pending.is_some());
    Ok(())
}

#[test]
fn only_a_manager_of_the_epoch_it_ends_vetoes_and_only_in_time() -> Result<()> {
    let (root, a, node, mut state) = setup()?;
    let (ordinary, removed, other) = (identity(), identity(), identity());
    let mut grants = vec![];
    for member in [
        Member::client(&ordinary, "ordinary", false)?,
        Member::client(&removed, "removed", true)?,
        Member::client(&other, "other", true)?,
    ] {
        let grant = GovernanceEvent::grant(&a, &state.project()?, member)?;
        grants.push(grant.hash()?);
        state.append(grant)?;
    }
    state.append(GovernanceEvent::revoke(
        &a,
        &state.project()?,
        cut(&removed, 0),
    )?)?;
    let (_, _, checkpoint) = recovering(&state, &root, &node, T)?;
    state.recover_at(checkpoint.clone(), T)?;
    let view = state.project_at(T)?;
    let signed_by = |device: &Identity| -> Result<GovernanceEvent> {
        let mut event = veto(&a, &view, &checkpoint, T)?;
        event.issuer = id(device);
        event.sign(device)
    };
    refused(
        state.clone().append(signed_by(&ordinary)?),
        "management permission required",
    );
    let mut saved_removal = state.clone();
    assert!(saved_removal.append(signed_by(&removed)?)?);
    assert!(saved_removal.project_at(T)?.pending.is_some());
    // On top of parents from before its removal it signs a valid event, which stops nothing:
    // its causal past misses the heads the root saw, the removal among them.
    let mut rewound = veto(&a, &view, &checkpoint, T)?;
    (rewound.issuer, rewound.parents) = (id(&removed), vec![grants[1].clone()]);
    let mut sneaked = state.clone();
    assert!(sneaked.append(rewound.sign(&removed)?)?);
    assert!(sneaked.project_at(T)?.pending.is_some());
    assert_eq!(sneaked.project_at(T + RECOVERY_DELAY_MS)?.epoch, 2);

    let mut late = state.clone();
    late.append(veto(&a, &view, &checkpoint, T + RECOVERY_DELAY_MS)?)?;
    assert!(
        late.project_at(T)?.pending.is_some(),
        "a veto signed too late counts for nothing"
    );
    assert_eq!(late.project_at(T + RECOVERY_DELAY_MS)?.epoch, 2);
    Ok(())
}

#[test]
fn removing_the_device_that_vetoed_undoes_nothing() -> Result<()> {
    let (root, a, node, mut state) = setup()?;
    let other = identity();
    state.append(GovernanceEvent::grant(
        &a,
        &state.project()?,
        Member::client(&other, "other", true)?,
    )?)?;
    let (_, _, checkpoint) = recovering(&state, &root, &node, T)?;
    state.recover_at(checkpoint.clone(), T)?;
    state.append(veto(&a, &state.project_at(T)?, &checkpoint, T + 1)?)?;
    let vetoed = state.clone();
    let (_, _, again) = recovering(&state, &root, &node, T + 2)?;
    state.recover_at(again.clone(), T + 2)?;
    // Weeks later the device that vetoed is removed as lost.
    let weeks = T + 10 * RECOVERY_DELAY_MS;
    let mut removed = state.clone();
    removed.append(GovernanceEvent::revoke(
        &other,
        &removed.project_at(T)?,
        cut(&a, 0),
    )?)?;
    let view = removed.project_at(weeks)?;
    assert_eq!(
        view.epoch, 3,
        "the recovery signed after the veto took effect"
    );
    assert!(view.stopped.contains(&checkpoint.hash()?));
    assert_eq!(view, state.project_at(weeks)?);
    // With only the first recovery, removing the device keeps it stopped too.
    let mut alone = vetoed.clone();
    alone.append(GovernanceEvent::revoke(
        &other,
        &alone.project_at(T)?,
        cut(&a, 0),
    )?)?;
    let view = alone.project_at(weeks)?;
    assert_eq!((view.epoch, view.stopped), (1, vec![checkpoint.hash()?]));
    Ok(())
}

#[test]
fn replicas_that_saw_the_removal_at_different_times_agree() -> Result<()> {
    let (root, a, node, mut state) = setup()?;
    let other = identity();
    state.append(GovernanceEvent::grant(
        &a,
        &state.project()?,
        Member::client(&other, "other", true)?,
    )?)?;
    let (_, _, checkpoint) = recovering(&state, &root, &node, T)?;
    state.recover_at(checkpoint.clone(), T)?;
    state.append(veto(&a, &state.project_at(T)?, &checkpoint, T + 1)?)?;
    let (_, next, again) = recovering(&state, &root, &node, T + 2)?;
    state.recover_at(again, T + 2)?;
    // P takes the removal of the device that vetoed while the second recovery waits; N does
    // not until after its time.
    let mut p = state.clone();
    p.append(GovernanceEvent::revoke(
        &other,
        &p.project_at(T)?,
        cut(&a, 0),
    )?)?;
    let n = state;
    let due = T + 2 + RECOVERY_DELAY_MS;
    for replica in [&p, &n] {
        assert_eq!(replica.project_at(due)?.recovery_key, id(&next));
    }
    let merged = n.merge(&p)?;
    assert_eq!(merged, p.merge(&n)?);
    assert_eq!(merged.project_at(due)?.recovery_key, id(&next));
    Ok(())
}

#[test]
fn a_recovery_naming_heads_held_nowhere_here_waits_and_does_not_install() -> Result<()> {
    let (root, a, node, base) = setup()?;
    let mut ahead = base.clone();
    ahead.append(GovernanceEvent::grant(
        &a,
        &ahead.project()?,
        Member::client(&identity(), "other", true)?,
    )?)?;
    let (_, next, checkpoint) = recovering(&ahead, &root, &node, T)?;
    assert_eq!(checkpoint.heads, ahead.project()?.heads);
    refused(
        base.clone().recover_at(checkpoint.clone(), T),
        RECOVERY_INCOMPLETE,
    );
    // A forged head is never held anywhere.
    let mut forged = checkpoint.clone();
    forged.heads = vec![hash(7)];
    forged.signature = root.sign_message(&crate::signing_message(
        RECOVERY_DOMAIN,
        &forged.signing_bytes()?,
    )?);
    refused(
        ahead.clone().recover_at(forged.clone(), T),
        RECOVERY_INCOMPLETE,
    );
    // Synced in without them, it waits past its time; with them, it takes effect.
    let due = T + RECOVERY_DELAY_MS;
    let mut synced = base.clone();
    synced.checkpoints.push(checkpoint);
    let view = synced.project_at(due)?;
    assert_eq!((view.epoch, view.pending), (1, None));
    refused(synced.check_root(&id(&root), T, false), RECOVERY_WAITING);
    assert_eq!(
        synced.merge(&ahead)?.project_at(due)?.recovery_key,
        id(&next)
    );
    let mut stuck = ahead.clone();
    stuck.checkpoints.push(forged);
    assert_eq!(stuck.project_at(due * 2)?.epoch, 1);
    // The old file can still rotate it away.
    stuck.recover_at(
        RootCheckpoint::rotation(&stuck, &root, &id(&identity()), T)?,
        T,
    )?;
    assert_eq!(stuck.project_at(T)?.epoch, 3);
    Ok(())
}

#[test]
fn replicas_reach_the_same_outcome_in_any_order_and_replays_change_nothing() -> Result<()> {
    let (root, a, node, base) = setup()?;
    let (_, _, checkpoint) = recovering(&base, &root, &node, T)?;
    let mut recovered = base.clone();
    recovered.recover_at(checkpoint.clone(), T)?;
    let event = veto(&a, &recovered.project_at(T)?, &checkpoint, T + 1)?;
    let mut vetoed = recovered.clone();
    assert!(vetoed.append(event.clone())?);
    assert!(!vetoed.append(event)? && !vetoed.recover_at(checkpoint.clone(), T)?);
    let parts = [&base, &recovered, &vetoed];
    for order in [[0, 1, 2], [2, 1, 0], [1, 2, 0], [2, 0, 1]] {
        let merged = order[1..]
            .iter()
            .try_fold(parts[order[0]].clone(), |state, index| {
                state.merge(parts[*index])
            })?;
        assert_eq!(merged, vetoed);
    }
    // Without the veto every replica flips at the same signed time. A veto signed in the
    // window that arrives after it puts the old directory back, the same on every replica.
    let due = T + RECOVERY_DELAY_MS;
    let flipped = recovered.project_at(due)?;
    assert_eq!(flipped.epoch, 2);
    let late = veto(&a, &recovered.project_at(T)?, &checkpoint, due - 1)?;
    let mut backdated = recovered.clone();
    backdated.append(late)?;
    for state in [&backdated, &recovered.merge(&vetoed)?, &vetoed] {
        let view = state.project_at(due)?;
        assert_eq!((view.epoch, view.recovery_key), (1, id(&root)));
        assert_eq!(view.stopped, vec![checkpoint.hash()?]);
    }
    // Signed after the time, a veto counts for nothing.
    let mut stale = recovered.clone();
    stale.append(veto(&a, &recovered.project_at(T)?, &checkpoint, due)?)?;
    assert_eq!(stale.project_at(due)?.epoch, 2);
    Ok(())
}
