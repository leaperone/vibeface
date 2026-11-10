//! Controlled protocol fixture: real Noise/TCP and actual private outbox, no Provider or user data.
#[path="../src/app.rs"]
#[allow(dead_code,unused_imports)]mod app;
#[path="../src/host/mod.rs"]
#[allow(dead_code,unused_imports)]mod host;
#[path="../src/ui.rs"]
#[allow(dead_code)]mod ui;
#[path="../src/theme.rs"]
#[allow(dead_code)]mod theme;
#[path="../src/views/mod.rs"]
#[allow(dead_code,unused_imports)]mod views;
#[path="../src/widgets/mod.rs"]
#[allow(dead_code)]mod widgets;
use anyhow::{Result,Context,ensure};
use futures_util::{SinkExt,StreamExt};
use serde_json::{Value,json};
use std::{sync::{Arc,atomic::{AtomicUsize,Ordering}},time::Duration,path::PathBuf};
use tokio::sync::{mpsc,watch};
use tokio_tungstenite::tungstenite::Message;
use vibeface_secure::{Identity,Handshake,membership::{Genesis,IdentityKind,Member,NetworkState}};
use vibeface_client::{Client,ClientKey,Entry,Endpoint,Action,OperationState,wire::{InputRequest,InputTarget,InputIntent}};
use views::{backend::{Backend,Command,Event},forms::Editor};

async fn observed(receiver:&mut watch::Receiver<Arc<ui::Snapshot>>,predicate:impl Fn(&ui::Snapshot)->bool)->Result<Arc<ui::Snapshot>>{
 tokio::time::timeout(Duration::from_secs(15),async{loop{let snapshot=receiver.borrow_and_update().clone();if predicate(&snapshot){return Ok(snapshot)}receiver.changed().await.context("native snapshot stream ended")?;}}).await.context("native fixture snapshot timeout")?
}
fn connect(entry:&Entry)->Command{Command::Save{editor:Editor::Connect,values:std::collections::BTreeMap::from([("origin".into(),entry.origin.clone()),("node_id".into(),entry.node_id.clone()),("passphrase".into(),"controlled fixture password".into()),("_submission_id".into(),"fixture-connect".into())]),conversation_id:None}}
fn host_at(profile:&PathBuf)->Result<host::DesktopHost>{host::DesktopHost::open(profile.clone(),profile.join("runtime"),true)}
async fn serve(listener:tokio::net::TcpListener,node:Identity,state:NetworkState,client_id:String,effects:Arc<AtomicUsize>)->Result<()>{
 let(tcp,_)=listener.accept().await?;let mut socket=tokio_tungstenite::accept_async(tcp).await?;ensure!(matches!(socket.next().await.context("connect")??,Message::Text(_)),"connect frame missing");let mut handshake=Handshake::signed(false,&node,None,b"vibeface:client:v1")?;
 let Message::Binary(first)=socket.next().await.context("first Noise")??else{anyhow::bail!("Noise frame missing")};handshake.read(&first)?;socket.send(Message::Binary(handshake.write(&[])?.into())).await?;let Message::Binary(last)=socket.next().await.context("last Noise")??else{anyhow::bail!("Noise frame missing")};handshake.read(&last)?;let mut channel=handshake.finish()?;
 while let Some(frame)=socket.next().await{let Message::Binary(frame)=frame?else{continue};let Some(bytes)=channel.open_record(&frame)?else{continue};let request:Value=serde_json::from_slice(&bytes)?;let result=match request["method"].as_str(){
 Some("authenticate")=>json!({"network_id":state.genesis.network_id,"principal":{"source":"membership","device_id":client_id,"can_manage":true}}),
 Some("directory")=>json!({"state":state,"now_ms":vibeface_protocol::now_ms()}),Some("write_state")=>json!({"state":"writable","conflicts":[]}),Some("subscribe")=>json!({"subscribed":true}),
 Some("rpc")=>match request["params"]["method"].as_str(){
 Some("bots")=>json!({"bots":[{"bot_id":"fixture-bot","conversation_id":"fixture-chat","name":"Fixture","models":{}}]}),
 Some("nodes")=>json!({"nodes":[{"device_id":node.device_id(),"name":"Fixture Node","online":true,"revoked":false,"local":false}]}),
 Some("workspace_state")=>json!({"tasks":[],"channels":[],"assignments":[],"inputs":[],"coverage":{"limit":100,"truncated":false}}),
 Some("questions")=>json!({"questions":[]}),Some("notifications")=>json!({"notifications":[]}),
 Some("conversation")=>json!({"conversation":{"conversation_id":"fixture-chat","title":"Fixture","owner":node.device_id(),"owner_epoch":1},"messages":[],"works":[],"scheduler":null,"questions":[],"sessions":[],"bot_settings":{"memory":[],"schedules":[],"skills":[],"memory_sync_conflicts":{"items":[],"has_more":false}}}),
 Some("message")=>{effects.fetch_add(1,Ordering::SeqCst);anyhow::bail!("fixture never authorized an online input")},
 _=>anyhow::bail!("unexpected controlled workspace read"),},_=>anyhow::bail!("unexpected controlled method"),};
 for frame in channel.seal_message(&serde_json::to_vec(&json!({"id":request["id"],"result":result}))?)?{socket.send(Message::Binary(frame.into())).await?;}
 }Ok(())
}
#[tokio::test(flavor="multi_thread",worker_threads=2)]
async fn native_backend_cold_offline_stages_original_input_in_actual_outbox()->Result<()>{
 let profile=std::env::temp_dir().join(format!("vibeface-native-cold-fixture-{}",uuid::Uuid::new_v4()));let initial_host=host_at(&profile)?;let directory=initial_host.client_storage_dir();let mut key=ClientKey::create(&directory,"controlled fixture password")?;let node=Identity::generate()?;let client_id=key.device_id().to_owned();let member=Member::new(&client_id,IdentityKind::Client,"Fixture client",true)?;let genesis=Genesis::create(&Identity::generate()?,member,Member::node(&node,"Fixture Node")?)?;let network=genesis.network_id.clone();let state=NetworkState::new(genesis)?;let listener=tokio::net::TcpListener::bind("127.0.0.1:0").await?;let entry=Entry{origin:format!("http://{}",listener.local_addr()?),node_id:node.device_id()};key.remember(&network,entry.clone())?;drop(key);
 let effects=Arc::new(AtomicUsize::new(0));let server=tokio::spawn(serve(listener,node,state,client_id.clone(),effects.clone()));let base=format!("vibeface-native://{}","1".repeat(64));let(tx,_events,mut snapshots,stop,task)=Backend::spawn(initial_host,&tokio::runtime::Handle::current(),base.clone());tx.send(connect(&entry)).await?;observed(&mut snapshots,|snapshot|snapshot.synced&&snapshot.bots.len()==1).await?;tx.send(Command::Conversation("fixture-chat".into())).await?;observed(&mut snapshots,|snapshot|snapshot.conversation.conversation.as_ref().is_some_and(|conversation|conversation.conversation_id=="fixture-chat")).await?;
 let cache=directory.join("workspace-cache").join(format!("{}.json",entry.node_id));tokio::time::timeout(Duration::from_secs(10),async{while !tokio::fs::try_exists(&cache).await?{tokio::task::yield_now().await;}Ok::<_,anyhow::Error>(())}).await??;
 stop.send_replace(true);task.await?;drop(tx);server.abort();let _=server.await;
 let(tx,mut events,mut snapshots,stop,task)=Backend::spawn(host_at(&profile)?,&tokio::runtime::Handle::current(),base);tx.send(connect(&entry)).await?;let snapshot=observed(&mut snapshots,|snapshot|snapshot.cached&&snapshot.can_save_locally).await?;assert!(!snapshot.online);assert!(!snapshot.synced);assert!(!snapshot.writable);assert_eq!(snapshot.conversation.conversation.as_ref().unwrap().conversation_id,"fixture-chat");
 tx.send(Command::Input{conversation_id:"fixture-chat".into(),draft:ui::ComposerDraft{target:InputTarget::BotMain{bot_id:"fixture-bot".into()},intent:InputIntent::Continue,text:"offline native input".into(),attachments:vec![],expected_revision:None}}).await?;
 tokio::time::timeout(Duration::from_secs(10),async{loop{let event=events.recv().await.context("native events ended")?;let event=if let Event::Scoped{event,..}=event{*event}else{event};match event{Event::InputStored(text)=>{assert_eq!(text,"offline native input");return Ok::<_,anyhow::Error>(())},Event::Error(error)=>anyhow::bail!("native staging failed: {error}"),_=>{}}}}).await??;
 #[cfg(unix)]{use std::os::unix::fs::PermissionsExt;for path in[directory.clone(),directory.join("outbox"),directory.join("outbox").join(&network),directory.join("outbox").join(&network).join(&client_id),directory.join("input-drafts"),directory.join("input-drafts").join(&network),directory.join("input-drafts").join(&network).join(&client_id)]{assert_eq!(std::fs::metadata(&path)?.permissions().mode()&0o777,0o700,"every app-owned identity/outbox/draft parent must be private");}}
 stop.send_replace(true);task.await?;drop(tx);let key=Arc::new(ClientKey::open(&directory,"controlled fixture password")?);let client=Client::open_offline(key,Endpoint::Gateway(entry.clone()),directory.join("outbox").join(&network).join(&client_id)).await?;let pending=client.pending().await?;assert_eq!(pending.len(),1);assert_eq!(pending[0].state,OperationState::Prepared);assert_eq!(pending[0].sequence,Some(1));assert!(!client.is_online());assert_eq!(effects.load(Ordering::SeqCst),0);
 let mut drafts=tokio::fs::read_dir(directory.join("input-drafts").join(&network).join(&client_id)).await?;let path=drafts.next_entry().await?.context("durable native draft missing")?.path();let draft:Value=serde_json::from_slice(&tokio::fs::read(path).await?)?;assert_eq!(draft["network_id"],network);assert_eq!(draft["client_id"],client_id);assert_eq!(draft["target_node"],entry.node_id);let input:InputRequest=serde_json::from_value(draft["input"].clone())?;assert_eq!(input.message_id,pending[0].id);assert_eq!(input.text,"offline native input");let restaged=client.stage(Action::Input{conversation_id:"fixture-chat".into(),input}).await?;assert_eq!(restaged.id,pending[0].id);assert_eq!(restaged.sequence,Some(1));drop(client);std::fs::remove_dir_all(profile)?;Ok(())
}
