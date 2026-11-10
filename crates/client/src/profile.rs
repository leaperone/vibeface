//! Last verified public membership evidence. It only authorizes local storage while offline.
use anyhow::{Result,ensure};
use serde::{Deserialize,Serialize};
use vibeface_secure::membership::NetworkState;
use crate::{ClientKey,ClientAuthority};

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
        ensure!(key.network_id()==Some(self.network_id.as_str()) && key.device_id()==self.client_id && target==self.target_node && self.state.genesis.network_id==self.network_id,"cached profile belongs to another network, client or target");
        ensure!(!self.revoked && self.write_gate!=2,"known revoked or forked profile cannot be used offline");
        ensure!(self.write_gate<=3,"invalid cached write state");
        let view=self.state.project_at(vibeface_protocol::now_ms().max(self.observed_at_ms))?;
        view.node(target)?;
        let member=view.client(key.device_id())?;
        Ok(ClientAuthority::Membership {can_manage:member.manage})
    }
}
