//! Node-local account, space and authorization storage.

pub use vibeface_secure::membership::{NodeCertificate, NodeRole};

pub(crate) mod admission;
pub(crate) mod client_policy_read;
pub(crate) mod cross_node_recovery;
pub(crate) mod enrollment_store;
pub(crate) mod event_authorization;
pub(crate) mod execution_dispatch;
pub(crate) mod execution_dispatch_resolution;
pub mod execution_handoff;
pub(crate) mod governance_store;
pub(crate) mod journal;
pub mod local;
pub(crate) mod node_enrollment;
pub(crate) mod operation_control;
pub(crate) mod passkey_rpc;
pub(crate) mod passkey_store;
pub mod policy;
pub(crate) mod recovery;
pub mod services;
pub mod webauthn;

#[cfg(test)]
pub(crate) mod test_support;

#[cfg(test)]
mod tests {
    use crate::{identity::test_support::TestNetwork, store::Store};
    use anyhow::Result;

    #[test]
    fn revoked_service_history_keeps_its_original_admission_after_reopen() -> Result<()> {
        let network = TestNetwork::new()?;
        let path = network.directory.join("history.sqlite3");
        let mut store = network.at(&path)?;
        let creation = network.conversation(&mut store, "archive")?;
        let message = network.message(&mut store, "archive", "original context")?;
        network.revoke_node(&mut store)?;
        assert!(
            store
                .authorize_node(network.space.as_str(), &network.node.node_id()?, None)
                .is_err()
        );
        assert_eq!(
            store
                .verify_event(network.space.as_str(), &message)?
                .scope_id,
            "archive"
        );
        drop(store);
        let reopened = Store::open(&path)?;
        assert_eq!(
            reopened.event(network.space.as_str(), &creation.hash)?,
            Some(creation)
        );
        assert_eq!(
            reopened
                .verify_event(network.space.as_str(), &message)?
                .scope_id,
            "archive"
        );
        assert!(
            reopened
                .authorize_node(network.space.as_str(), &network.node.node_id()?, None)
                .is_err()
        );
        Ok(())
    }
}
