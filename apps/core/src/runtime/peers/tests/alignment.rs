use super::super::test_support::node;
use super::super::*;
use crate::crypto::LocalIdentity;

#[tokio::test]
async fn a_restart_from_the_same_file_writes_alone_and_a_copy_waits() -> Result<()> {
    let ia = LocalIdentity::generate();
    let others = [LocalIdentity::generate(), LocalIdentity::generate()];
    let state = ia.sign_network_fixture(
        1,
        vec![
            ia.device_id()?,
            others[0].device_id()?,
            others[1].device_id()?,
        ],
    )?;
    let network = ia.fixture_network_id()?;
    let (a, dir) = node(&ia).await?;
    let result = async {
        a.install_membership(state).await?;
        assert!(
            !a.align_on_start().await?,
            "no record of writing from this file yet"
        );
        a.confirm_writable(&network, "test").await?;

        a.set_write_state(&network, &[WriteState::Writable], WriteState::Aligning)
            .await;
        assert!(
            a.align_on_start().await?,
            "the file it last wrote from holds all it wrote; no other Node needs to answer"
        );
        assert_eq!(*a.write_state.borrow(), WriteState::Writable);

        let copy = dir.canonicalize()?.join("copy.sqlite3");
        {
            let copy = copy.to_str().context("path")?.to_owned();
            a.store
                .call("peers.copy_fixture", move |store| {
                    store.connection.execute("VACUUM INTO ?", [copy])?;
                    Ok(())
                })
                .await?;
        }
        assert!(
            !Store::open(&copy)?.wrote_from_this_file()?,
            "a copied or restored database waits for the other Nodes or the user"
        );
        Ok::<_, anyhow::Error>(())
    }
    .await;
    drop(a);
    let _ = std::fs::remove_dir_all(dir);
    result
}

#[tokio::test]
async fn alignment_waits_for_every_member_node_to_confirm() -> Result<()> {
    use crate::identity::{GovernanceEvent, Revocation};
    let (ia, ib, ic) = (
        LocalIdentity::generate(),
        LocalIdentity::generate(),
        LocalIdentity::generate(),
    );
    let (b, c) = (ib.device_id()?, ic.device_id()?);
    let state = ia.sign_network_fixture(1, vec![ia.device_id()?, b.clone(), c.clone()])?;
    let network = ia.fixture_network_id()?;
    let (a, dir) = node(&ia).await?;
    let result = async {
        a.install_membership(state.clone()).await?;
        assert!(!a.align_on_start().await?);
        let mark = |peer: &str, confirmed: bool| {
            let mut states = a.sync_peers.lock().expect("sync peers");
            let entry = states.entry(peer.into()).or_default();
            entry.confirmed = confirmed;
            entry.failures = u32::from(!confirmed);
        };
        let now = || *a.write_state.borrow();
        mark(&b, true);
        a.align(&network).await;
        assert_eq!(now(), WriteState::Aligning, "c has not answered yet");
        mark(&c, false);
        a.align(&network).await;
        assert_eq!(now(), WriteState::ReadOnly);
        mark(&c, true);
        a.align(&network).await;
        assert_eq!(now(), WriteState::Writable);

        a.set_write_state(&network, &[WriteState::Writable], WriteState::Aligning)
            .await;
        mark(&c, false);
        let mut next = state.clone();
        next.append(GovernanceEvent::revoke(
            &ia.fixture_admin(),
            &state.project()?,
            Revocation::new(&c, None)?,
        )?)?;
        a.install_membership(next).await?;
        assert_eq!(
            now(),
            WriteState::Writable,
            "a revoked Node no longer counts"
        );
        Ok::<_, anyhow::Error>(())
    }
    .await;
    drop(a);
    let _ = std::fs::remove_dir_all(dir);
    result
}
