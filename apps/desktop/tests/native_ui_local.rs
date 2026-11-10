//! Actual local grants, Noise clients, profile storage, and exclusive SQLite leases.
#[path="../src/app.rs"] #[allow(dead_code,unused_imports)] mod app;
#[path="../src/host/mod.rs"] #[allow(dead_code,unused_imports)] mod host;
#[path="../src/ui.rs"] #[allow(dead_code)] mod ui;
#[path="../src/theme.rs"] #[allow(dead_code)] mod theme;
#[path="../src/views/mod.rs"] #[allow(dead_code,unused_imports)] mod views;
#[path="../src/widgets/mod.rs"] #[allow(dead_code)] mod widgets;
use anyhow::{Context,Result,ensure};
use futures_util::{SinkExt,StreamExt};
use serde_json::{Value,json};
use std::{collections::BTreeMap,path::PathBuf,sync::{Arc,Mutex,atomic::{AtomicBool,AtomicUsize,Ordering}},time::Duration};
use tokio::{io::{AsyncReadExt,AsyncWriteExt},sync::{Notify,mpsc,watch}};
use tokio_tungstenite::tungstenite::Message;
use vibeface_bridge::platform::privatefs;
use vibeface_client::{Client,ClientKey,Endpoint,Entry,OfflineBlockReason};
use vibeface_secure::{Handshake,Identity,membership::{Genesis,IdentityKind,Member,NetworkState}};
use views::{backend::{Backend,Command,Event},forms::Editor};
const PASSWORD:&str="controlled local profile password";

struct LocalNode {
 entry:Entry,state:NetworkState,node:Identity,member:String,
 disconnect:Mutex<BTreeMap<String,Arc<Notify>>>,connections:Mutex<Vec<Arc<Notify>>>,
 block_next:AtomicBool,blocked:AtomicBool,release:Notify,business:AtomicUsize,
}
impl LocalNode {
 async fn serve(self:Arc<Self>,listener:tokio::net::TcpListener){
  while let Ok((stream,_))=listener.accept().await {let node=self.clone();tokio::spawn(async move{let _=node.connection(stream).await;});}
 }
 async fn connection(self:Arc<Self>,mut stream:tokio::net::TcpStream)->Result<()> {
  let mut head=vec![0;16384];let(length,end)=tokio::time::timeout(Duration::from_secs(5),async{
   loop{let length=stream.peek(&mut head).await?;ensure!(length>0,"controlled local peer closed");if let Some(end)=head[..length].windows(4).position(|bytes|bytes==b"\r\n\r\n"){return Ok::<_,anyhow::Error>((length,end+4))}ensure!(length<head.len(),"controlled local headers too large");tokio::task::yield_now().await;}
  }).await??;
  let headers=std::str::from_utf8(&head[..end])?.to_owned();
  if headers.starts_with("GET /api/local/ws ") {
   let cookie=headers.lines().filter_map(|line|line.split_once(':')).find(|(name,_)|name.eq_ignore_ascii_case("cookie")).context("local WS cookie missing")?.1.trim();
   let client=cookie.strip_prefix(&format!("vibeface_local_{}=",self.entry.node_id)).context("local WS cookie scope")?.to_owned();
   ensure!(client.len()==64&&client.bytes().all(|byte|byte.is_ascii_hexdigit()),"invalid controlled local peer");
   let socket=tokio_tungstenite::accept_async(stream).await?;return self.noise(socket,client).await;
  }
  let size=headers.lines().filter_map(|line|line.split_once(':')).find(|(name,_)|name.eq_ignore_ascii_case("content-length")).map(|(_,value)|value.trim().parse::<usize>()).transpose()?.unwrap_or(0);
  ensure!(end+size<=16384&&length<=16384,"controlled local request too large");let mut request=vec![0;end+size];stream.read_exact(&mut request).await?;let body:Value=serde_json::from_slice(&request[end..])?;
  let mut cookie=None;
  let value=if headers.starts_with("POST /api/local/launch ") {
   ensure!(headers.contains(&format!("Bearer {}","a".repeat(64))),"controlled launch authentication");json!({"url":self.entry.origin,"device_id":self.entry.node_id,"token":"b".repeat(64)})
  }else if headers.starts_with("POST /api/local/exchange ") {
   ensure!(body["token"]=="b".repeat(64)&&body["node_id"]==self.entry.node_id,"controlled exchange scope");let client=body["device_id"].as_str().context("controlled client missing")?;cookie=Some(format!("Set-Cookie: vibeface_local_{}={client}; HttpOnly; SameSite=Strict; Path=/api/local; Max-Age=86400\r\n",self.entry.node_id));json!({"entry":{"address":self.entry.origin,"device_id":self.entry.node_id,"local":true},"network_id":self.state.genesis.network_id})
  }else{anyhow::bail!("unexpected controlled local HTTP endpoint")};
  let body=serde_json::to_vec(&value)?;let response=format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n{}\r\n",body.len(),cookie.unwrap_or_default());stream.write_all(response.as_bytes()).await?;stream.write_all(&body).await?;stream.shutdown().await?;Ok(())
 }
 async fn noise(self:Arc<Self>,mut socket:tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>,client:String)->Result<()> {
  let disconnect=Arc::new(Notify::new());self.disconnect.lock().unwrap().insert(client.clone(),disconnect.clone());self.connections.lock().unwrap().push(disconnect.clone());
  let mut handshake=Handshake::signed(false,&self.node,None,b"vibeface:client:v1")?;
  let Message::Binary(first)=socket.next().await.context("local first Noise")?? else{anyhow::bail!("local Noise expected")};handshake.read(&first)?;socket.send(Message::Binary(handshake.write(&[])?.into())).await?;let Message::Binary(last)=socket.next().await.context("local last Noise")?? else{anyhow::bail!("local Noise expected")};handshake.read(&last)?;let mut channel=handshake.finish()?;
  loop{
   let frame=tokio::select!{_=disconnect.notified()=>break,frame=socket.next()=>frame};let Some(frame)=frame else{break};let Message::Binary(frame)=frame? else{continue};let Some(bytes)=channel.open_record(&frame)? else{continue};let request:Value=serde_json::from_slice(&bytes)?;
   if request["method"]=="authenticate"&&self.block_next.swap(false,Ordering::SeqCst){self.blocked.store(true,Ordering::SeqCst);tokio::select!{_=self.release.notified()=>{},_=disconnect.notified()=>break};}
   let result=match request["method"].as_str(){
    Some("authenticate")=>json!({"network_id":self.state.genesis.network_id,"principal":{"source":if client==self.member{"membership"}else{"local_session"},"device_id":client,"can_manage":client==self.member}}),
    Some("directory")=>json!({"state":self.state,"now_ms":vibeface_protocol::now_ms()}),Some("write_state")=>json!({"state":"writable","conflicts":[]}),Some("subscribe")=>json!({"subscribed":true}),
    Some("rpc")=>match request["params"]["method"].as_str(){
     Some("bots")=>json!({"bots":[{"bot_id":"local-bot","conversation_id":"local-chat","name":"Controlled","models":{}}]}),
     Some("nodes")=>json!({"nodes":[{"device_id":self.entry.node_id,"name":"Controlled local Node","online":true,"revoked":false,"local":true}]}),
     Some("workspace_state")=>json!({"tasks":[],"channels":[],"assignments":[],"inputs":[],"coverage":{"limit":100,"truncated":false}}),
     Some("questions")=>json!({"questions":[]}),Some("notifications")=>json!({"notifications":[]}),
     _=>{self.business.fetch_add(1,Ordering::SeqCst);anyhow::bail!("controlled local client sent an unapproved business request")},
    },_=>{self.business.fetch_add(1,Ordering::SeqCst);anyhow::bail!("controlled local client sent an unapproved method")},
   };
   for frame in channel.seal_message(&serde_json::to_vec(&json!({"id":request["id"],"result":result}))?)?{socket.send(Message::Binary(frame.into())).await?;}
  }Ok(())
 }
 fn disconnect(&self,client:&str)->Result<()>{self.disconnect.lock().unwrap().get(client).context("controlled client not connected")?.notify_one();Ok(())}
}

struct Fixture {root:PathBuf,a_dir:PathBuf,b_dir:PathBuf,a:String,b:String,b_profile:String,node:Arc<LocalNode>,server:tokio::task::JoinHandle<()>}
impl Drop for Fixture{fn drop(&mut self){self.server.abort();self.node.release.notify_waiters();for notify in self.node.connections.lock().unwrap().iter(){notify.notify_waiters();}let _=std::fs::remove_dir_all(&self.root);}}
impl Fixture {
 async fn new()->Result<(Self,host::DesktopHost)>{
  let root=std::env::temp_dir().join(format!("vf-local-profiles-{}",uuid::Uuid::new_v4()));let profile=root.join("profile");let runtime=root.join("runtime");for directory in [&root,&profile,&runtime]{privatefs::create_dir(directory)?;}
  let mut host=host::DesktopHost::open(profile.clone(),runtime.clone(),true)?;let a_dir=host.client_storage_dir();let a_key=ClientKey::create(&a_dir,PASSWORD)?;let a=a_key.device_id().to_owned();
  let mut registry=host::profiles::ProfileRegistry::open(&a_dir)?;let b_profile=registry.create("Controlled B")?;let b_dir=registry.paths(&b_profile)?.client_dir;let b_key=ClientKey::create(&b_dir,PASSWORD)?;let b=b_key.device_id().to_owned();drop((a_key,b_key,registry));
  let listener=tokio::net::TcpListener::bind("127.0.0.1:0").await?;let node_key=Identity::generate()?;let entry=Entry{origin:format!("http://{}",listener.local_addr()?),node_id:node_key.device_id()};
  let state=NetworkState::new(Genesis::create(&Identity::generate()?,Member::new(&a,IdentityKind::Client,"Controlled A",true)?,Member::node(&node_key,"Controlled Node")?)?)?;
  let node_dir=profile.join("node");privatefs::create_dir(&node_dir)?;privatefs::write_atomic(&node_dir.join("config.json"),b"{}")?;privatefs::write_atomic(&node_dir.join("identity.json"),b"{}")?;privatefs::write_atomic(&node_dir.join("local-access.json"),&serde_json::to_vec(&json!({"origin":entry.origin,"node_id":entry.node_id,"capability":"a".repeat(64)}))?)?;
  privatefs::write_atomic(&profile.join("fixture-inspect.json"),&serde_json::to_vec(&json!({"ok":true,"result":{"data_dir":node_dir,"device_id":entry.node_id,"status":"running","local_listen":entry.origin.trim_start_matches("http://"),"listen":"127.0.0.1:1","web_enabled":true,"core_version":"controlled","error":null}}))?)?;
  let source=root.join("inspector.rs");std::fs::write(&source,INSPECTOR)?;let executable=runtime.join(if cfg!(windows){"vibeface-core.exe"}else{"vibeface-core"});let status=std::process::Command::new(std::env::var_os("RUSTC").unwrap_or_else(||"rustc".into())).args(["--edition=2024"]).arg(&source).arg("-o").arg(executable).status()?;ensure!(status.success(),"compile the existing-toolchain controlled inspector");
  host.select_node(&host::NodeIdentity{data_dir:node_dir,device_id:entry.node_id.clone()}).await?;
  let node=Arc::new(LocalNode{entry,state,node:node_key,member:a.clone(),disconnect:Mutex::new(BTreeMap::new()),connections:Mutex::new(vec![]),block_next:AtomicBool::new(false),blocked:AtomicBool::new(false),release:Notify::new(),business:AtomicUsize::new(0)});let server=tokio::spawn(node.clone().serve(listener));
  Ok((Self{root,a_dir,b_dir,a,b,b_profile:b_profile.as_str().into(),node,server},host))
 }
}
const INSPECTOR:&str=r#"use std::{env,fs,io::{self,Read}};fn main(){assert_eq!(env::args().nth(1).as_deref(),Some("--control"));let mut request=String::new();io::stdin().read_to_string(&mut request).unwrap();assert!(request.contains("\"command\":\"inspect\""));let profile=env::current_exe().unwrap().parent().unwrap().parent().unwrap().join("profile");println!("{}",fs::read_to_string(profile.join("fixture-inspect.json")).unwrap());}"#;
fn local()->Command{Command::Save{editor:Editor::ConnectLocal,values:BTreeMap::from([("passphrase".into(),PASSWORD.into())]),conversation_id:None}}
async fn observed(receiver:&mut watch::Receiver<Arc<ui::Snapshot>>,expected:impl Fn(&ui::Snapshot)->bool)->Result<Arc<ui::Snapshot>>{tokio::time::timeout(Duration::from_secs(20),async{loop{let value=receiver.borrow_and_update().clone();if expected(&value){return Ok(value)}receiver.changed().await.context("local backend ended")?;}}).await.context("local backend state timeout")?}
fn readonly(snapshot:&ui::Snapshot,client:&str){assert!(snapshot.online);assert!(snapshot.local_session);assert_eq!(snapshot.client_id.as_deref(),Some(client));assert!(!snapshot.can_manage);assert!(!snapshot.writable);assert!(!snapshot.can_control);assert!(!snapshot.can_stage_input());}
async fn next_error(events:&mut mpsc::Receiver<Event>)->Result<String>{tokio::time::timeout(Duration::from_secs(5),async{loop{let event=events.recv().await.context("local events ended")?;let event=match event{Event::Scoped{event,..}=>*event,event=>event};if let Event::Error(error)=event{return Ok(error)}}}).await?}

#[tokio::test(flavor="multi_thread",worker_threads=2)]
async fn local_profile_round_trip_reuses_its_lease_and_cancels_old_reconnect_without_membership()->Result<()> {
 let(fixture,host)=Fixture::new().await?;let(tx,mut events,mut snapshots,stop,task)=Backend::spawn(host,&tokio::runtime::Handle::current(),"controlled-local-profiles".into());
 tx.send(local()).await?;let a=observed(&mut snapshots,|s|s.online&&s.client_id.as_deref()==Some(&fixture.a)).await?;readonly(&a,&fixture.a);
 let network=fixture.node.state.genesis.network_id.clone();let outbox=fixture.a_dir.join("outbox").join(&network).join(&fixture.a);let key=Arc::new(ClientKey::open(&fixture.a_dir,PASSWORD)?);let locked=Client::open_offline(key,Endpoint::Gateway(fixture.node.entry.clone()),outbox.clone()).await;assert!(locked.err().context("active A must hold its real exclusive outbox lease")?.to_string().contains("already open"));
 tx.send(Command::SwitchProfile(fixture.b_profile.clone())).await?;observed(&mut snapshots,|s|s.profile_id.as_deref()==Some(&fixture.b_profile)&&s.client_id.is_none()).await?;tx.send(local()).await?;let b=observed(&mut snapshots,|s|s.online&&s.client_id.as_deref()==Some(&fixture.b)).await?;readonly(&b,&fixture.b);
 tx.send(Command::SwitchProfile("default".into())).await?;observed(&mut snapshots,|s|s.profile_id.as_deref()==Some("default")&&s.client_id.is_none()).await?;tx.send(local()).await?;let a=observed(&mut snapshots,|s|s.online&&s.client_id.as_deref()==Some(&fixture.a)).await?;readonly(&a,&fixture.a);
 fixture.node.block_next.store(true,Ordering::SeqCst);fixture.node.disconnect(&fixture.a)?;observed(&mut snapshots,|s|!s.online&&s.identity_busy).await?;tokio::time::timeout(Duration::from_secs(10),async{while !fixture.node.blocked.load(Ordering::SeqCst){tokio::task::yield_now().await;}}).await?;
 tx.send(local()).await?;let connected=observed(&mut snapshots,|s|s.online&&s.generation>a.generation).await?;fixture.node.release.notify_one();let settled=observed(&mut snapshots,|s|s.online&&!s.identity_busy&&s.generation==connected.generation).await?;readonly(&settled,&fixture.a);fixture.node.disconnect(&fixture.a)?;let retried=observed(&mut snapshots,|s|s.online&&!s.identity_busy&&s.generation>settled.generation).await?;readonly(&retried,&fixture.a);
 tx.send(Command::Input{conversation_id:"local-chat".into(),draft:ui::ComposerDraft{target:vibeface_protocol::workspace::InputTarget::BotMain{bot_id:"local-bot".into()},intent:vibeface_protocol::workspace::InputIntent::Continue,text:"must remain unsent".into(),attachments:vec![],expected_revision:None}}).await?;assert!(next_error(&mut events).await?.contains("此身份尚未完成可写工作区验证"));
 // Same device key with changed enrollment metadata must not reuse the old Client's key.
 tx.send(Command::SwitchProfile(fixture.b_profile.clone())).await?;observed(&mut snapshots,|s|s.profile_id.as_deref()==Some(&fixture.b_profile)).await?;let mut key=ClientKey::open(&fixture.a_dir,PASSWORD)?;key.remember_endpoint(&network,Endpoint::Gateway(fixture.node.entry.clone()))?;drop(key);tx.send(Command::SwitchProfile("default".into())).await?;observed(&mut snapshots,|s|s.profile_id.as_deref()==Some("default")&&s.client_id.is_none()).await?;tx.send(local()).await?;observed(&mut snapshots,|s|s.online&&s.can_manage&&!s.local_session).await?;
 tx.send(Command::SwitchProfile(fixture.b_profile.clone())).await?;observed(&mut snapshots,|s|s.profile_id.as_deref()==Some(&fixture.b_profile)).await?;let path=fixture.a_dir.join("client.json");let mut metadata:Value=serde_json::from_slice(&std::fs::read(&path)?)?;metadata["network_id"]=Value::Null;privatefs::write_atomic(&path,&serde_json::to_vec(&metadata)?)?;tx.send(Command::SwitchProfile("default".into())).await?;observed(&mut snapshots,|s|s.profile_id.as_deref()==Some("default")&&s.client_id.is_none()).await?;tx.send(local()).await?;let a=observed(&mut snapshots,|s|s.online&&s.client_id.as_deref()==Some(&fixture.a)).await?;readonly(&a,&fixture.a);
 assert_eq!(fixture.node.business.load(Ordering::SeqCst),0);assert!(outbox.join("outbox.sqlite3").exists());assert!(fixture.b_dir.join("outbox").join(&network).join(&fixture.b).join("outbox.sqlite3").exists());stop.send_replace(true);task.await?;drop(tx);
 let key=Arc::new(ClientKey::open(&fixture.a_dir,PASSWORD)?);assert!(key.network_id().is_none());let client=Client::open_offline(key,Endpoint::Gateway(fixture.node.entry.clone()),outbox).await?;assert_eq!(client.offline_block_reason(),Some(OfflineBlockReason::NotEnrolled));assert!(client.pending().await?.is_empty());drop(client);Ok(())
}
