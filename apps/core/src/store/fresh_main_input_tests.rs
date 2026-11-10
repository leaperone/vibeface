use super::*;

struct Fixture {store:Store,a:LocalIdentity,b:LocalIdentity,network:String,state:crate::identity::NetworkState,base:Vec<SignedEvent>}
fn fixture()->Result<Fixture> {
    let a=LocalIdentity::generate();let b=LocalIdentity::generate();let network=a.fixture_network_id()?;
    let state=a.sign_network_fixture(1,vec![a.device_id()?,b.device_id()?])?;
    let mut store=Store::open(":memory:")?;store.install_trust_bundle(&network,&state)?;
    store.create_conversation(&a,&network,"c","Fresh main",a.owner_fixture(&network,"c",&a.device_id()?)?,1)?;
    let body=a.fixture_admin().attest(&network,"c",EventBody::BotCreated {bot_id:"bot".into(),name:"Fresh main".into(),instructions:"Synthetic".into(),project:None,attestation:None,cause:None})?;
    store.append_event(&a,&network,"c",body,vec![],2)?;
    let base=store.events_from(&network,&a.device_id()?,1,100)?;
    Ok(Fixture{store,a,b,network,state,base})
}
fn body(f:&Fixture,id:&str,text:&str,intent:InputIntent)->Result<EventBody> {
    f.a.fixture_admin().attest(&f.network,"c",EventBody::InputPosted {message_id:id.into(),target:InputTarget::BotMain {bot_id:"bot".into()},
        intent,sequence:1,expected_revision:None,text:text.into(),attachments:vec![],report_results:Some(true),attestation:None})
}
fn snapshot(store:&Store,network:&str)->Result<Vec<String>> {
    let mut result=Vec::new();
    for table in crate::store::reduce::TABLES {
        let mut statement=store.connection.prepare(&format!("SELECT * FROM {table} WHERE network_id=?"))?;
        let columns=statement.column_count();
        let mut rows=statement.query_map([network],|row|(0..columns).map(|index|row.get::<_,rusqlite::types::Value>(index)).collect::<rusqlite::Result<Vec<_>>>())?
            .map(|row|Ok(format!("{table}:{:?}",row?))).collect::<Result<Vec<_>>>()?;
        rows.sort();result.extend(rows);
    }
    for sql in ["SELECT hash,main,effective FROM events WHERE network_id=? ORDER BY hash",
        "SELECT author,max_seq,head_hash FROM log_heads WHERE network_id=? ORDER BY author",
        "SELECT hash,envelope,missing FROM pending_events WHERE network_id=? ORDER BY hash"] {
        let mut statement=store.connection.prepare(sql)?;let columns=statement.column_count();
        for row in statement.query_map([network],|row|(0..columns).map(|index|row.get::<_,rusqlite::types::Value>(index)).collect::<rusqlite::Result<Vec<_>>>())? {
            result.push(format!("{sql}:{:?}",row?));
        }
    }
    Ok(result)
}
fn replica(f:&Fixture)->Result<Store> {
    let mut store=Store::open(":memory:")?;store.install_trust_bundle(&f.network,&f.state)?;
    store.ingest_events_at(&f.network,&f.base,2)?;Ok(store)
}
fn reset_metrics(){reset_projection_ancestor_walks();reset_fresh_local_main_inputs();}
fn rebuild(store:&Store,network:&str)->Result<()> {
    let before=snapshot(store,network)?;
    store.connection.execute("UPDATE events SET effective=0 WHERE network_id=?",[network])?;
    crate::store::reduce::clear(&store.connection,network)?;
    crate::store::log::recompute(&store.connection,&membership_from(&store.connection,network)?)?;
    assert_eq!(snapshot(store,network)?,before);Ok(())
}

#[test]
fn fresh_local_multi_author_main_inputs_match_full_authorization_and_rebuild()->Result<()> {
    let mut f=fixture()?;let mut full=replica(&f)?;
    let fixtures=(0..12).map(|index|body(&f,&format!("input-{index}"),&format!("{index}:{}","历史\\\"\n".repeat(32)),
        if index%3==0{InputIntent::Defer}else{InputIntent::Continue})).collect::<Result<Vec<_>>>()?;
    for (index,body) in fixtures.into_iter().enumerate() {
        let author=if index%2==0{&f.a}else{&f.b};
        reset_metrics();
        let actual=f.store.append_event(author,&f.network,"c",body.clone(),vec![],10+index as i64)?;
        assert_eq!(fresh_local_main_inputs(),1);assert_eq!(authorization_ancestor_walks(),0);
        let expected=crate::store::log::test_metrics::with_full_workspace_authorization(||full.append_event(author,&f.network,"c",body,vec![],10+index as i64))?;
        assert_eq!(actual.hash,expected.hash);
        assert_eq!(snapshot(&f.store,&f.network)?,snapshot(&full,&f.network)?);
        rebuild(&f.store,&f.network)?;
    }
    Ok(())
}

#[test]
fn duplicate_invalid_and_controlled_inputs_keep_the_full_authorization_boundary()->Result<()> {
    let mut f=fixture()?;
    let first=body(&f,"saved","Original",InputIntent::Defer)?;
    let original=f.store.append_event(&f.a,&f.network,"c",first.clone(),vec![],3)?;
    reset_metrics();f.store.append_event(&f.a,&f.network,"c",first,vec![],4)?;
    assert_eq!(fresh_local_main_inputs(),0);assert!(authorization_ancestor_walks()>0);
    let before=snapshot(&f.store,&f.network)?;
    let changed=body(&f,"saved","Changed without a control",InputIntent::Defer)?;
    let ordinary=f.store.append_event(&f.a,&f.network,"c",changed.clone(),vec![],5).unwrap_err().to_string();
    let full=crate::store::log::test_metrics::with_full_workspace_authorization(||f.store.append_event(&f.a,&f.network,"c",changed,vec![],5)).unwrap_err().to_string();
    assert_eq!(ordinary,full);assert!(ordinary.contains("another signed intent"));assert_eq!(snapshot(&f.store,&f.network)?,before);
    let command=WorkspaceCommand::ControlInput {message_id:"saved".into(),expected_event:original.hash,
        action:InputControl::Edit {text:"Edited input".into(),attachments:vec![]}};
    let signed=f.a.fixture_admin().attest(&f.network,WORKSPACE_SCOPE,EventBody::WorkspaceCommanded {command_id:"edit".into(),command:command.clone(),attestation:None,cause:None,acting_assignment:None})?;
    reset_metrics();f.store.apply_workspace_command(&f.a,&f.network,"edit",command,signed.attestation().cloned(),None,None,6)?;
    assert_eq!(fresh_local_main_inputs(),0);assert!(authorization_ancestor_walks()>0);
    let current=f.store.queued_workspace_input(&f.network,"saved")?.context("controlled input")?;
    assert_eq!(current.input.text,"Edited input");
    let command=WorkspaceCommand::ControlInput {message_id:"saved".into(),expected_event:current.control_event,action:InputControl::Release};
    let signed=f.a.fixture_admin().attest(&f.network,WORKSPACE_SCOPE,EventBody::WorkspaceCommanded {command_id:"release".into(),command:command.clone(),attestation:None,cause:None,acting_assignment:None})?;
    reset_metrics();f.store.apply_workspace_command(&f.a,&f.network,"release",command,signed.attestation().cloned(),None,None,7)?;
    assert_eq!(fresh_local_main_inputs(),0);assert!(authorization_ancestor_walks()>0);
    let mut invalid=body(&f,"invalid","Signed",InputIntent::Continue)?;
    if let EventBody::InputPosted {text,..}=&mut invalid{*text="Tampered after client signing".into();}
    reset_metrics();assert!(f.store.append_event(&f.a,&f.network,"c",invalid,vec![],8).is_err());
    assert_eq!(fresh_local_main_inputs(),0);
    let mut zero=body(&f,"zero","Invalid routing sequence",InputIntent::Continue)?;
    if let EventBody::InputPosted {sequence,..}=&mut zero{*sequence=0;}
    // Re-signing still cannot make a zero sequence pass shape validation.
    zero=f.a.fixture_admin().attest(&f.network,"c",zero)?;
    assert!(f.store.append_event(&f.a,&f.network,"c",zero,vec![],8).is_err());
    rebuild(&f.store,&f.network)
}

#[test]
fn sync_forks_and_cutoff_events_never_take_the_local_append_shortcut()->Result<()> {
    let mut f=fixture()?;let mut second=replica(&f)?;
    let left=body(&f,"left","Left branch",InputIntent::Continue)?;
    let right=body(&f,"right","Right branch",InputIntent::Continue)?;
    f.store.append_event(&f.a,&f.network,"c",left,vec![],3)?;
    let fork=second.append_event(&f.a,&f.network,"c",right,vec![],3)?;
    reset_metrics();f.store.ingest_events_at(&f.network,std::slice::from_ref(&fork),50)?;
    assert_eq!(fresh_local_main_inputs(),0);assert!(authorization_ancestor_walks()>0);
    rebuild(&f.store,&f.network)?;
    let next=body(&f,"blocked","Forked author",InputIntent::Continue)?;
    reset_metrics();assert!(f.store.append_event(&f.a,&f.network,"c",next,vec![],4).is_err());
    assert_eq!(fresh_local_main_inputs(),0);assert!(authorization_ancestor_walks()>0);
    let fresh=body(&f,"peer","Other author after fork",InputIntent::Continue)?;
    reset_metrics();f.store.append_event(&f.b,&f.network,"c",fresh,vec![],5)?;
    assert_eq!(fresh_local_main_inputs(),1);rebuild(&f.store,&f.network)?;
    let mut cutoff=replica(&f)?;
    let last=f.base.last().context("prelude")?;
    let last_event:Event=serde_json::from_str(&last.canonical_body)?;
    let mut revoked=f.state.clone();revoked.append(crate::identity::GovernanceEvent::revoke(&f.a.fixture_admin(),&f.state.project()?,
        crate::identity::Revocation::new(&f.a.device_id()?,Some((last_event.seq,&last.hash)))?)?)?;
    cutoff.install_trust_bundle(&f.network,&revoked)?;
    reset_metrics();cutoff.ingest_events_at(&f.network,&[fork],50)?;
    assert_eq!(fresh_local_main_inputs(),0);assert!(authorization_ancestor_walks()>0);
    let effective:bool=cutoff.connection.query_row("SELECT effective FROM events WHERE network_id=? AND subject='right'",[&f.network],|row|row.get(0))?;
    assert!(!effective);rebuild(&cutoff,&f.network)
}

#[test]
fn task_channel_wrong_scope_and_attestation_checks_remain_exact()->Result<()> {
    let mut f=fixture()?;
    let command=WorkspaceCommand::CreateTask {task_id:"task".into(),channel_id:None,parent_task_id:None,owner_bot:"bot".into(),
        title:"Task".into(),request:"Synthetic".into(),expected_result:"Result".into(),source_events:vec![],dependencies:vec![],reply_only:false};
    let signed=f.a.fixture_admin().attest(&f.network,WORKSPACE_SCOPE,EventBody::WorkspaceCommanded {command_id:"create-task".into(),command:command.clone(),attestation:None,cause:None,acting_assignment:None})?;
    f.store.apply_workspace_command(&f.a,&f.network,"create-task",command,signed.attestation().cloned(),None,None,3)?;
    let mut task=body(&f,"task-input","Task input",InputIntent::Continue)?;
    if let EventBody::InputPosted {target,expected_revision,..}=&mut task {*target=InputTarget::Task {task_id:"task".into()};*expected_revision=Some(1);}
    task=f.a.fixture_admin().attest(&f.network,"c",task)?;
    reset_metrics();f.store.append_event(&f.a,&f.network,"c",task,vec![],4)?;
    assert_eq!(fresh_local_main_inputs(),0);assert!(authorization_ancestor_walks()>0);
    let mut channel=body(&f,"channel-input","Channel input",InputIntent::Continue)?;
    if let EventBody::InputPosted {target,..}=&mut channel {*target=InputTarget::ChannelMember {channel_id:"channel".into(),bot_id:"bot".into()};}
    channel=f.a.fixture_admin().attest(&f.network,"c",channel)?;
    for (scope,input) in [("c",channel),("different",f.a.fixture_admin().attest(&f.network,"different",body(&f,"wrong-scope","Wrong scope",InputIntent::Continue)?)?)] {
        reset_metrics();
        let actual=f.store.append_event(&f.a,&f.network,scope,input.clone(),vec![],5).unwrap_err().to_string();
        let expected=crate::store::log::test_metrics::with_full_workspace_authorization(||f.store.append_event(&f.a,&f.network,scope,input,vec![],5)).unwrap_err().to_string();
        assert_eq!(actual,expected);assert_eq!(fresh_local_main_inputs(),0);
    }
    let mut missing=body(&f,"missing-signature","Unsigned",InputIntent::Continue)?;
    if let EventBody::InputPosted {attestation,..}=&mut missing {*attestation=None;}
    reset_metrics();assert!(f.store.append_event(&f.a,&f.network,"c",missing,vec![],6).is_err());assert_eq!(fresh_local_main_inputs(),0);
    rebuild(&f.store,&f.network)
}

#[test]
#[ignore = "signed local BotMain append measurements; baseline requires an explicit environment opt-in"]
fn signed_local_main_input_append_scale()->Result<()> {
    use std::time::Instant;
    let sizes=std::env::var("VIBEFACE_PERF_SIZES").unwrap_or_else(|_|"1000".into());
    let baseline=std::env::var("VIBEFACE_PERF_LOCAL_BASELINE").is_ok_and(|value|value=="1");
    for size in sizes.split(',').map(str::parse::<usize>) {
        let size=size?;
        ensure!(size > 0 && size <= 10_000,"local append measurement supports 1 to 10000 inputs");
        ensure!(!baseline || [100,256,1000].contains(&size),"full-authorization baseline sizes are 100,256,1000");
        for escaped in [true,false] {
            let mut f=fixture()?;
            let text=if escaped {"历史\\\"\n".repeat(1024)}else{"完整消息".repeat(1024)};
            // Human attestation generation and fixture allocation are outside the measured append.
            let fixtures=(0..size).map(|index|body(&f,&format!("input-{index}"),&format!("{index}:{text}"),InputIntent::Continue))
                .collect::<Result<Vec<_>>>()?;
            let mut samples=Vec::new();
            for repeat in 0..3 {
                if repeat!=0 {f.store=replica(&f)?;}
                reset_metrics();
                let start=Instant::now();
                let mut append=||->Result<()> {
                    for (index,body) in fixtures.iter().enumerate() {f.store.append_event(&f.a,&f.network,"c",body.clone(),vec![],10+index as i64)?;}
                    Ok(())
                };
                if baseline {crate::store::log::test_metrics::with_full_workspace_authorization(append)?;}else{append()?;}
                let elapsed=start.elapsed();samples.push(elapsed.as_nanos());
                assert_eq!(fresh_local_main_inputs(),if baseline{0}else{size});
                assert_eq!(authorization_ancestor_walks(),if baseline{size}else{0});
                let messages=f.store.messages(&f.network,"c")?;
                assert_eq!(messages.len(),size);
                assert!(messages.iter().any(|message|message.message_id==format!("input-{}",size.saturating_sub(1))));
                eprintln!("signed_local_inputs={size} escaped={escaped} force_full_authorization={baseline} repeat={repeat} elapsed_ns={} throughput_per_second={}",elapsed.as_nanos(),size as f64/elapsed.as_secs_f64());
            }
            samples.sort_unstable();eprintln!("signed_local_append samples={} p50_ns={} min_ns={} max_ns={}",samples.len(),samples[samples.len()/2],samples[0],samples[samples.len()-1]);
        }
    }
    Ok(())
}
