//! JSON adapters and the single WASM export implementation of the membership API.

use super::{
    AuthorizationView, Genesis, GovernanceAction, GovernanceEvent, Member, NetworkState,
    RootCheckpoint, now_ms,
};
use crate::Identity;
use anyhow::{Result, ensure};
#[cfg(target_arch = "wasm32")]
use wasm_bindgen::prelude::*;

fn js<T>(result: Result<T>) -> crate::Result<T> {
    Ok(result?)
}

/// Builds a genesis with the unlocked recovery root; the browser then seals that root.
#[cfg_attr(target_arch = "wasm32", wasm_bindgen(js_name = createGenesis))]
pub fn create_genesis(
    root: &Identity,
    first_client_json: &str,
    first_node_json: &str,
) -> crate::Result<String> {
    js((|| {
        let genesis = Genesis::create(
            root,
            serde_json::from_str(first_client_json)?,
            serde_json::from_str(first_node_json)?,
        )?;
        Ok(serde_json::to_string(&genesis)?)
    })())
}

/// Canonical bytes of an unsigned governance event; sign with `signingMessage("identity-governance", ...)`.
#[cfg_attr(target_arch = "wasm32", wasm_bindgen(js_name = governancePayload))]
pub fn governance_payload(event_json: &str) -> crate::Result<Vec<u8>> {
    js((|| {
        serde_json::from_str::<GovernanceEvent>(event_json)?.signing_bytes()
    })())
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen)]
pub struct VerifiedMembership {
    state: NetworkState,
    view: AuthorizationView,
    now: u64,
}

/// `now`: the Node's clock as its last answer gave it, so both sides see the same epoch in
/// effect; this process's clock when absent.
#[cfg_attr(target_arch = "wasm32", wasm_bindgen(js_name = verifyMembership))]
pub fn verify_membership(
    state_json: &str,
    expected_network: &str,
    now: Option<f64>,
) -> crate::Result<VerifiedMembership> {
    js((|| {
        ensure!(
            state_json.len() <= crate::MAX_APPLICATION_MESSAGE,
            "identity payload too large"
        );
        let state: NetworkState = serde_json::from_str(state_json)?;
        ensure!(
            state.genesis.network_id == expected_network,
            "membership belongs to another personal network"
        );
        let now = now.map_or_else(now_ms, |now| now as u64);
        let view = state.project_at(now)?;
        Ok(VerifiedMembership { state, view, now })
    })())
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen)]
impl VerifiedMembership {
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen(js_name = viewJson))]
    pub fn view_json(&self) -> crate::Result<String> {
        Ok(serde_json::to_string(&self.view)?)
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen(js_name = stateJson))]
    pub fn state_json(&self) -> crate::Result<String> {
        Ok(serde_json::to_string(&self.state)?)
    }

    /// An unsigned `GovernanceEvent` for `{"type":"grant",...}`, `{"type":"revoke",...}`,
    /// `{"type":"veto","checkpoint":...,"at_ms":...}` or the same shape with `"type":"approve"`.
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen(js_name = governanceTemplate))]
    pub fn governance_template(&self, issuer: &str, action_json: &str) -> crate::Result<String> {
        js((|| {
            let action: GovernanceAction = serde_json::from_str(action_json)?;
            if let GovernanceAction::Veto { checkpoint, .. }
            | GovernanceAction::Approve { checkpoint, .. } = &action
            {
                self.state.ensure_checkpoint_voter(checkpoint, issuer)?;
            }
            let event = GovernanceEvent::unsigned(&self.view, issuer, action)?;
            Ok(serde_json::to_string(&event)?)
        })())
    }

    /// The next recovery checkpoint, handing recovery to `next`. `heads_json` is
    /// `{device: [seq, hash]}` from a Node's log; each current Node left out keeps what it wrote
    /// up to there. The checkpoint names this state's management heads of the root's epoch, so
    /// the state must be the Node's (`recovery_state`) or the file's whole history.
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen(js_name = createRecoveryCheckpoint))]
    pub fn create_recovery_checkpoint(
        &self,
        root: &Identity,
        next: &str,
        members_json: &str,
        heads_json: &str,
    ) -> crate::Result<String> {
        js((|| {
            self.current_root(root, false)?;
            let members: Vec<Member> = serde_json::from_str(members_json)?;
            let retired = self
                .view
                .retire(&members, &serde_json::from_str(heads_json)?)?;
            let checkpoint =
                RootCheckpoint::recovery(&self.state, root, next, members, retired, self.now)?;
            self.state
                .clone()
                .recover_at(checkpoint.clone(), self.now)?;
            Ok(serde_json::to_string(&checkpoint)?)
        })())
    }

    /// The next checkpoint, handing recovery to `next` without touching the directory.
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen(js_name = createKeyRotation))]
    pub fn create_key_rotation(&self, root: &Identity, next: &str) -> crate::Result<String> {
        js((|| {
            self.current_root(root, true)?;
            let checkpoint = RootCheckpoint::rotation(&self.state, root, next, self.now)?;
            self.state
                .clone()
                .recover_at(checkpoint.clone(), self.now)?;
            Ok(serde_json::to_string(&checkpoint)?)
        })())
    }

    fn current_root(&self, root: &Identity, rotation: bool) -> Result<()> {
        self.state.check_root(&root.device_id(), self.now, rotation)
    }
}
