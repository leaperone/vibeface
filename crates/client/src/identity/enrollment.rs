use super::*;
const FILE:&str="enrollment.json";
#[derive(Serialize,Deserialize)]
#[serde(deny_unknown_fields)]
struct Journal {
    device_id:String,entry:Entry,link:Zeroizing<String>,name:String,join_id:String,
    nonce:Zeroizing<String>,admin:Option<(String,String,String)>,
    request:Vec<u8>,sent:bool,joined:Option<String>,terminal:bool,
}
impl Journal {
    fn endpoint(&self)->Result<Endpoint> {
        let ticket=Ticket::from_link(&self.link)?;
        let addresses=ticket.entry.addrs.iter().filter(|addr|matches!(&addr.host,vibeface_secure::link::Host::Ip(_))).map(ToString::to_string).collect::<Vec<_>>();
        if self.entry.origin.is_empty() {ensure!(!addresses.is_empty(),"invitation needs a numeric direct address or a Gateway origin");Ok(Endpoint::Lan {node_id:self.entry.node_id.clone(),addresses})}
        else if addresses.is_empty() {Ok(Endpoint::Gateway(self.entry.clone()))}
        else {Ok(Endpoint::Preferred {gateway:self.entry.clone(),addresses})}
    }
    fn password(key:&ClientKey)->Zeroizing<String> {Zeroizing::new(key.identity.secret_hex())}
    fn load(key:&ClientKey)->Result<Self> {
        let encrypted=privatefs::read_private_bounded(&key.dir.join(FILE),MAX_FILE,"pending enrollment")?;
        let plain=Zeroizing::new(recovery_open(&encrypted,&Self::password(key))?);
        let journal:Self=serde_json::from_slice(&plain)?;
        ensure!(journal.device_id==key.device_id(),"enrollment belongs to another client");
        let ticket=Ticket::from_link(&journal.link)?;
        ensure!(ticket.entry_id()==journal.entry.node_id && invite_id(&ticket.invite_hash())?==journal.join_id,"enrollment ticket identity mismatch");
        Ok(journal)
    }
    fn bytes(&self,key:&ClientKey)->Result<Vec<u8>> {
        let plain=Zeroizing::new(serde_json::to_vec(self)?);
        Ok(recovery_seal(&plain,&Self::password(key))?)
    }
    fn save(&self,key:&ClientKey)->Result<()> {privatefs::write_atomic(&key.dir.join(FILE),&self.bytes(key)?)}
}

/// Joining preserves the original nonce and confirming manager across process restarts.
pub struct Enrollment<'a> {session:Session<'a>,pub join_id:String,journal:Journal}
pub enum Progress {Waiting,Code(String),Joined}
impl<'a> Enrollment<'a> {
    pub fn pending(key:&ClientKey)->bool {key.dir.join(FILE).exists()}
    pub async fn start(key:&'a ClientKey,origin:&str,link:&str,name:&str)->Result<Self> {
        if Self::pending(key) {
            let pending=Journal::load(key)?;
            ensure!(pending.entry.origin==origin && pending.link.as_str()==link && pending.name==name,"another enrollment is pending; resume its original invitation");
        } else {
            ensure!(key.network_id().is_none(),"client already belongs to a personal network");
            vibeface_secure::membership::validate_name(name)?;
            let ticket=Ticket::from_link(link)?;
            if !origin.is_empty() {crate::connection::client_url(origin)?;} else {ensure!(ticket.entry.addrs.iter().any(|addr|matches!(&addr.host,vibeface_secure::link::Host::Ip(_))),"invitation has no direct address; choose a Gateway origin");}
            let nonce=Zeroizing::new(hex::encode(rand::random::<[u8;32]>()));
            let join_id=invite_id(&ticket.invite_hash())?;
            let request=serde_json::to_vec(&json!({"id":format!("join-{join_id}"),"method":"join","params":{"secret":hex::encode(ticket.secret),"public_key":key.device_id(),"kind":"client","name":name,"commit":join_commit(key.device_id(),&nonce)?}}))?;
            let journal=Journal {device_id:key.device_id().into(),entry:Entry {origin:origin.into(),node_id:ticket.entry_id()},link:Zeroizing::new(link.into()),name:name.into(),join_id,nonce,admin:None,request,sent:false,joined:None,terminal:false};
            privatefs::create_durable(&key.dir.join(FILE),&journal.bytes(key)?)?;
        }
        Self::resume(key).await
    }
    pub async fn resume(key:&'a ClientKey)->Result<Self> {
        let mut journal=Journal::load(key)?;
        let mut session=key.connect_endpoint(&journal.endpoint()?).await?;
        if !journal.sent {
            let authenticated=session.call("authenticate",json!({})).await?;
            ensure!(authenticated["revoked"]!=true,"client was revoked; this enrollment cannot proceed");
            ensure!(authenticated["principal"].is_null(),"client is already a member");
            journal.sent=true;journal.save(key)?;
            let request=session.connection.call_bytes(journal.request.clone()).await?;
            ensure!(request["join_id"]==journal.join_id && request["invite_id"]==journal.join_id,"Node returned another join request");
        }
        Ok(Self {join_id:journal.join_id.clone(),session,journal})
    }
    pub async fn poll(&mut self)->Result<Progress> {
        let mut status=match self.session.call("join_status",json!({"join_id":self.join_id})).await {
            Ok(status)=>status,
            Err(error)=>{
                let ticket=Ticket::from_link(&self.journal.link)?;
                if self.session.call("join_check",json!({"secret":hex::encode(ticket.secret)})).await.is_ok_and(|reply|reply["invite"]=="expired") {
                    self.journal.terminal=true;self.journal.save(self.session.key)?;
                }
                return Err(error);
            }
        };
        ensure!(status["join_id"]==self.join_id && status["invite_id"]==self.join_id && status["device_id"]==self.session.key.device_id() && status["commit"]==join_commit(self.session.key.device_id(),&self.journal.nonce)?,"join status belongs to another request");
        match status["status"].as_str() {
            Some("pending")=>Ok(Progress::Waiting),
            Some("confirming")=>{
                let admin=(text(&status,"network_id")?.to_owned(),text(&status,"admin_id")?.to_owned(),text(&status,"admin_nonce")?.to_owned());
                let pinned=self.journal.admin.get_or_insert_with(||admin.clone());
                ensure!(*pinned==admin,"the approving device changed; the original enrollment is retained");
                self.journal.save(self.session.key)?;
                if status["joiner_nonce"].is_null() {status=self.session.call("join_status",json!({"join_id":self.join_id,"reveal":self.journal.nonce.as_str()})).await?;}
                ensure!(status["invite_id"]==self.join_id && status["admin_id"]==admin.1 && status["admin_nonce"]==admin.2 && status["joiner_nonce"]==self.journal.nonce.as_str(),"revealed enrollment differs from the pinned commitment");
                Ok(Progress::Code(short_code(&admin.0,text(&status,"invite_id")?,self.session.key.device_id(),&admin.1,&admin.2,&self.journal.nonce)?))
            }
            Some("joined")=>{
                let state:NetworkState=serde_json::from_value(status["state"].take())?;
                let network=text(&status,"network_id")?.to_owned();
                ensure!(state.genesis.network_id==network && self.journal.admin.as_ref().is_some_and(|(pinned,..)|*pinned==network),"joined network differs from the confirmed enrollment");
                state.project()?.client(self.session.key.device_id())?;
                self.journal.joined=Some(network);self.journal.save(self.session.key)?;Ok(Progress::Joined)
            }
            Some("expired"|"rejected"|"withdrawn")=>{self.journal.terminal=true;self.journal.save(self.session.key)?;bail!("enrollment ended; the original request is retained for review")}
            _=>bail!("enrollment has no recognized status; original request retained"),
        }
    }
    pub fn endpoint(&self)->Endpoint {self.session.entry.clone()}
    pub fn node_link(&self)->Result<String> {Ok(Ticket::from_link(&self.journal.link)?.entry.to_string())}
    pub fn finish(self)->Result<(String,Entry)> {
        let network=self.journal.joined.context("enrollment has not been confirmed joined")?;
        Ok((network,self.journal.entry))
    }
    pub fn discard_finished(key:&ClientKey)->Result<()> {
        let journal=Journal::load(key)?;
        ensure!(journal.terminal,"an uncertain or active enrollment cannot be discarded");
        std::fs::remove_file(key.dir.join(FILE))?;privatefs::sync_dir(&key.dir)
    }
}
pub(super) fn clear_joined(key:&ClientKey,network:&str)->Result<()> {
    if !Enrollment::pending(key) {return Ok(());}
    let journal=Journal::load(key)?;
    if journal.joined.as_deref()==Some(network) {std::fs::remove_file(key.dir.join(FILE))?;privatefs::sync_dir(&key.dir)?;}
    Ok(())
}
