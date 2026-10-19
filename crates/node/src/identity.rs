//! Node-local storage and shared signed personal-network membership.

pub use vibeface_secure::membership::*;

pub mod local;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypto::{self, LocalIdentity};
    use anyhow::Result;

    #[test]
    fn historical_author_signature_survives_recovery_without_online_authority() -> Result<()> {
        let root = LocalIdentity::generate();
        let admin = LocalIdentity::generate();
        let mut state = NetworkState::new(Genesis::create(
            &root,
            vec![Member::client(&admin, "admin", true)?],
        )?)?;
        let old_node = LocalIdentity::generate();
        state.append(GovernanceEvent::grant(
            &admin,
            &state.project()?,
            Member::node(&old_node, "retired", vec![NodeRole::Replica])?,
        )?)?;
        let event = old_node.sign_event(crate::model::Event {
            version: 1,
            network_id: state.genesis.network_id.clone(),
            scope_id: "archive".into(),
            author: old_node.peer_id()?,
            seq: 1,
            prev: None,
            deps: vec![],
            created_at_ms: 1,
            body: crate::model::EventBody::ConversationCreated {
                title: "old context".into(),
            },
            blobs: vec![],
        })?;
        state.append(GovernanceEvent::revoke(
            &admin,
            &state.project()?,
            &old_node.peer_id()?,
        )?)?;
        let original = state.clone();
        let retained = state.historical_nodes()?;
        state.recover(RootCheckpoint::create_with_history(
            &root,
            2,
            vec![Member::client(&admin, "admin", true)?],
            retained.clone(),
        )?)?;
        assert!(state.events.is_empty());
        let fresh: NetworkState = serde_json::from_slice(&serde_json::to_vec(&state)?)?;
        let author = fresh
            .historical_nodes()?
            .into_iter()
            .find(|node| node.subject_id == old_node.peer_id().unwrap())
            .unwrap();
        assert_eq!(
            crypto::verify_signed_event(&fresh.genesis.network_id, &event, &author.public_key)?
                .scope_id,
            "archive"
        );
        assert!(
            fresh
                .project()?
                .authorize_node(&old_node.peer_id()?, None)
                .is_err()
        );
        assert_eq!(retained, original.historical_nodes()?);
        Ok(())
    }
}
