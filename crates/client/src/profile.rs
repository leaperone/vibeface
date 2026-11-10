//! Last verified public membership evidence. It only authorizes local storage while offline.
use anyhow::{Result,ensure};
use serde::{Deserialize,Serialize};
use vibeface_secure::membership::NetworkState;
use crate::{ClientKey,ClientAuthority,OfflineBlockReason};

#[derive(Clone,Serialize,Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CachedProfile {
    pub network_id:String,
    pub client_id:String,
    pub target_node:String,
    pub state:NetworkState,
    pub observed_at_ms:u64,
    pub write_gate:u8,
    pub revoked:bool,
}
impl CachedProfile {
    pub fn verify_offline(&self,key:&ClientKey,target:&str)->Result<ClientAuthority> {
        ensure!(key.network_id().is_none_or(|network|network==self.network_id) && key.device_id()==self.client_id && target==self.target_node && self.state.genesis.network_id==self.network_id,"cached profile belongs to another network, client or target");
        ensure!(self.write_gate<=3,"invalid cached write state");
        self.state.project_at(self.observed_at_ms)?.node(target)?;
        let view=self.state.project_at(vibeface_protocol::now_ms().max(self.observed_at_ms))?;
        let blocked=if self.revoked {Some(OfflineBlockReason::Revoked)} else if self.write_gate==2 {Some(OfflineBlockReason::Forked)} else if key.network_id().is_none() {Some(OfflineBlockReason::NotEnrolled)} else if view.node(target).is_err() {Some(OfflineBlockReason::NodeRemoved)} else if view.client(key.device_id()).is_err() {Some(OfflineBlockReason::NotMember)} else {None};
        if let Some(reason)=blocked {return Ok(ClientAuthority::PrivateDataOnly {reason});}
        let member=view.client(key.device_id())?;
        Ok(ClientAuthority::Membership {can_manage:member.manage})
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Entry;
    use vibeface_secure::{Identity,membership::{Genesis,Member}};
    #[test]
    fn a_blocked_signed_cache_only_authorizes_private_data_and_never_another_identity()->Result<()> {
        let dir=std::env::temp_dir().join(format!("native-proof-{}",uuid::Uuid::new_v4()));
        let mut key=ClientKey::create(&dir.join("key"),"fixture password")?;let node=Identity::generate()?;
        let state=NetworkState::new(Genesis::create(&Identity::generate()?,Member::client(&key.identity,"client",true)?,Member::node(&node,"node")?)?)?;
        key.remember(&state.genesis.network_id,Entry {origin:"http://127.0.0.1:1".into(),node_id:node.device_id()})?;
        let mut proof=CachedProfile {network_id:state.genesis.network_id.clone(),client_id:key.device_id().into(),target_node:node.device_id(),state,observed_at_ms:vibeface_protocol::now_ms(),write_gate:1,revoked:false};
        assert_eq!(proof.verify_offline(&key,&node.device_id())?,ClientAuthority::Membership {can_manage:true});
        proof.revoked=true;
        assert_eq!(proof.verify_offline(&key,&node.device_id())?,ClientAuthority::PrivateDataOnly {reason:OfflineBlockReason::Revoked});
        proof.revoked=false;proof.write_gate=2;
        assert_eq!(proof.verify_offline(&key,&node.device_id())?,ClientAuthority::PrivateDataOnly {reason:OfflineBlockReason::Forked});
        assert!(proof.verify_offline(&key,&Identity::generate()?.device_id()).is_err());
        let other=ClientKey::create(&dir.join("other"),"fixture password")?;
        assert!(proof.verify_offline(&other,&node.device_id()).is_err());
        proof.state.genesis.signature="0".repeat(128);
        assert!(proof.verify_offline(&key,&node.device_id()).is_err(),"a blocked profile still verifies its cryptographic binding");
        std::fs::remove_dir_all(dir)?;Ok(())
    }
}
