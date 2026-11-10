use super::*;

const PLAN_FILE:&str="identity-operation.json";
#[derive(Clone,Copy,Debug,PartialEq,Eq,Serialize,Deserialize)]
#[serde(rename_all="snake_case")]
pub enum IdentityOperationState { Prepared, Unknown, Recorded, Applied, Stopped }
#[derive(Serialize,Deserialize)]
#[serde(deny_unknown_fields)]
struct Plan {
    client_id:String,
    network_id:String,
    entry:Entry,
    backup_path:PathBuf,
    backup_bytes:Vec<u8>,
    request_bytes:Vec<u8>,
    genesis:vibeface_secure::membership::Genesis,
    checkpoint:Option<RootCheckpoint>,
    state:IdentityOperationState,
}
/// An encrypted pending identity operation plus an already durable, verified recovery backup.
/// No governance request is sent until `submit` is explicitly called after the user confirms the file.
pub struct IdentityPlan {dir:PathBuf,plan:Plan,password:Zeroizing<String>}
impl IdentityPlan {
    pub fn exists(key:&ClientKey)->bool {key.dir.join(PLAN_FILE).exists()}
    pub fn backup_path(&self)->&Path {&self.plan.backup_path}
    pub fn backup_sha256(&self)->String {hex::encode(Sha256::digest(&self.plan.backup_bytes))}
    pub fn network_id(&self)->&str {&self.plan.network_id}
    pub fn state(&self)->IdentityOperationState {self.plan.state}
    pub fn load(key:&ClientKey,password:&str)->Result<Self> {
        let file=privatefs::read_private_bounded(&key.dir.join(PLAN_FILE),16*1024*1024,"pending identity operation")?;
        let bytes=Zeroizing::new(recovery_open(&file,password).context("wrong recovery password for pending operation")?);
        let plan:Plan=serde_json::from_slice(&bytes)?;
        ensure!(plan.client_id==key.device_id(),"pending operation belongs to another client");
        ensure!(key.network_id().is_none_or(|network|network==plan.network_id),"pending operation belongs to another network");
        let pending=Self {dir:key.dir.clone(),plan,password:Zeroizing::new(password.into())};
        pending.verify_backup()?;Ok(pending)
    }
    pub(super) fn prepare(key:&ClientKey,entry:Entry,path:&Path,material:&RecoveryMaterial,password:&str,params:Value)->Result<Self> {
        ensure!(path.is_absolute(),"recovery backup path must be absolute");
        ensure!(!Self::exists(key),"an identity operation is pending; reopen it instead of generating another recovery key");
        ensure!(!path.exists(),"recovery backup already exists; choose a new path");
        let backup_bytes=recovery_file_bytes(material,password)?;
        let request_bytes=serde_json::to_vec(&json!({"id":uuid::Uuid::new_v4().to_string(),"method":"recover","params":params}))?;
        let pending=Self {dir:key.dir.clone(),password:Zeroizing::new(password.into()),plan:Plan {
            client_id:key.device_id().into(),network_id:material.network_id.clone(),entry,backup_path:path.to_owned(),backup_bytes,request_bytes,
            genesis:material.genesis.clone(),checkpoint:material.checkpoints.last().cloned(),state:IdentityOperationState::Prepared,
        }};
        // The encrypted journal lands first. A crash before publishing the backup resumes these same bytes.
        let sealed=pending.sealed()?;
        privatefs::create_durable(&pending.dir.join(PLAN_FILE),&sealed)?;
        pending.verify_backup()?;Ok(pending)
    }
    fn sealed(&self)->Result<Vec<u8>> {
        let bytes=Zeroizing::new(serde_json::to_vec(&self.plan)?);
        Ok(recovery_seal(&bytes,&self.password)?)
    }
    fn save(&self)->Result<()> {privatefs::write_atomic(&self.dir.join(PLAN_FILE),&self.sealed()?)}
    fn verify_backup(&self)->Result<()> {
        if !self.plan.backup_path.exists() {privatefs::create_durable(&self.plan.backup_path,&self.plan.backup_bytes)?;}
        let actual=privatefs::read_private_bounded(&self.plan.backup_path,MAX_FILE,"recovery backup")?;
        ensure!(actual==self.plan.backup_bytes,"recovery backup changed; restore the exact saved backup before submitting");Ok(())
    }
    /// A Prepared operation has never crossed the network. Unknown/Recorded only read authoritative evidence.
    pub async fn submit(&mut self,key:&mut ClientKey)->Result<Value> {
        ensure!(key.device_id()==self.plan.client_id,"identity changed since preparation");
        self.verify_backup()?;
        if self.plan.state!=IdentityOperationState::Prepared {return self.reconcile(key).await;}
        self.plan.state=IdentityOperationState::Unknown;self.save()?;
        let result=async {
            let session=key.connect(Some(&self.plan.entry)).await?;
            session.connection.call_bytes(self.plan.request_bytes.clone()).await
        }.await;
        match result {
            Ok(result)=>{self.record_receipt(key,&result)?;Ok(result)}
            Err(error)=>match self.reconcile(key).await {Ok(result)=>Ok(result),Err(_)=>Err(error).context("identity operation outcome is unknown; reopen the pending operation and query its receipt")},
        }
    }
    pub async fn reconcile(&mut self,key:&mut ClientKey)->Result<Value> {
        ensure!(key.device_id()==self.plan.client_id,"pending operation belongs to another client");
        let mut session=key.connect(Some(&self.plan.entry)).await?;
        let result=session.call("authenticate",json!({})).await?;
        drop(session);
        self.record_receipt(key,&result)?;
        Ok(result)
    }
    fn record_receipt(&mut self,key:&mut ClientKey,result:&Value)->Result<()> {
        ensure!(result["network_id"]==self.plan.network_id,"original Node has not confirmed this network; pending operation retained");
        self.plan.state=if !result["state"].is_null() {
            let state:NetworkState=serde_json::from_value(result["state"].clone())?;
            ensure!(state.genesis==self.plan.genesis,"Node returned another genesis");
            let now=result["now_ms"].as_u64().context("receipt time missing")?;
            let view=state.project_at(now)?;
            if let Some(checkpoint)=&self.plan.checkpoint {
                ensure!(state.checkpoints.contains(checkpoint),"original checkpoint is not recorded here");
                let hash=checkpoint.hash()?;
                if view.stopped.contains(&hash) {IdentityOperationState::Stopped}
                else if view.pending.as_ref().is_some_and(|pending|pending.checkpoint==hash) {IdentityOperationState::Recorded}
                else {ensure!(view.epoch>=checkpoint.epoch && view.client(key.device_id())?.manage,"recovery has not taken effect");IdentityOperationState::Applied}
            } else {ensure!(view.client(key.device_id())?.manage,"creation did not grant this client management");IdentityOperationState::Applied}
        } else {
            let checkpoint=self.plan.checkpoint.as_ref().context("original creation is not confirmed")?;
            ensure!(result["recovery"]["checkpoint_hash"]==checkpoint.hash()?,"Node has not confirmed this exact recovery checkpoint");
            match result["recovery"]["status"].as_str() {Some("waiting")=>IdentityOperationState::Recorded,Some("stopped")=>IdentityOperationState::Stopped,_=>anyhow::bail!("recovery has no authoritative receipt")}
        };
        self.save()?;
        if self.plan.state!=IdentityOperationState::Stopped {key.remember(&self.plan.network_id,self.plan.entry.clone())?;}
        Ok(())
    }
    /// Explicitly discard only a never-sent or conclusively settled plan. The recovery backup stays intact.
    pub fn discard(self)->Result<()> {
        ensure!(matches!(self.plan.state,IdentityOperationState::Prepared|IdentityOperationState::Applied|IdentityOperationState::Stopped),"unknown or waiting recovery cannot be discarded");
        std::fs::remove_file(self.dir.join(PLAN_FILE))?;privatefs::sync_dir(&self.dir)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn encrypted_plan_restores_the_same_backup_and_never_regenerates_unknown_recovery() -> Result<()> {
        let dir=std::env::temp_dir().join(format!("identity-plan-{}",uuid::Uuid::new_v4()));
        let key=ClientKey::create(&dir.join("key"),"client passphrase")?;
        let root=Identity::generate()?;let node=Identity::generate()?;
        let genesis=vibeface_secure::membership::Genesis::create(&root,Member::client(&key.identity,"client",true)?,Member::node(&node,"node")?)?;
        let backup=RecoveryMaterial {version:1,network_id:genesis.network_id.clone(),root_secret_key:Zeroizing::new(root.secret_hex()),genesis:genesis.clone(),checkpoints:vec![],events:vec![]};
        let entry=Entry {origin:"http://127.0.0.1:1".into(),node_id:node.device_id()};
        let path=dir.join("recovery.json");
        let plan=IdentityPlan::prepare(&key,entry.clone(),&path,&backup,"recovery password",json!({"genesis":genesis,"capability":"controlled-one-time-capability"}))?;
        let bytes=std::fs::read(&path)?;let request=plan.plan.request_bytes.clone();
        let journal=std::fs::read_to_string(key.dir.join(PLAN_FILE))?;
        assert!(!journal.contains("controlled-one-time-capability"));
        assert!(!journal.contains(&root.secret_hex()));
        assert!(IdentityPlan::prepare(&key,entry,&dir.join("another.json"),&backup,"recovery password",json!({})).is_err());
        std::fs::remove_file(&path)?;drop(plan);
        assert!(IdentityPlan::load(&key,"wrong password").is_err());
        let mut reopened=IdentityPlan::load(&key,"recovery password")?;
        assert_eq!(std::fs::read(&path)?,bytes,"resume restores original sealed backup bytes");
        assert_eq!(reopened.plan.request_bytes,request);
        reopened.plan.state=IdentityOperationState::Unknown;reopened.save()?;drop(reopened);
        let reopened=IdentityPlan::load(&key,"recovery password")?;
        assert_eq!(reopened.state(),IdentityOperationState::Unknown);
        assert_eq!(reopened.plan.request_bytes,request);
        assert!(reopened.discard().is_err(),"an unknown rotation cannot be discarded and replaced");
        assert_eq!(std::fs::read(&path)?,bytes);
        std::fs::remove_dir_all(dir)?;Ok(())
    }
}
