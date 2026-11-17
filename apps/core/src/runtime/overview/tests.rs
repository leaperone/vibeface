//! Frozen pre-refactor reader: a test oracle, deliberately retaining its old unbounded reads.
use super::*;
use rusqlite::params;

impl Runtime {
    /// A bounded, human-only projection of the facts this Node has locally.
    async fn workspace_overview_reference(&self, access: &Access, input: &Value) -> Result<Value> {
        ensure!(
            access.is_user(),
            "workspace overview is unavailable to Agent tokens"
        );
        ensure!(
            input.get("cursor").is_none_or(Value::is_null),
            "workspace overview cursor is not supported"
        );
        let limit = workspace_limit(input)?;
        let include_finished = workspace_bool(input, "include_finished", false)?;
        let include_sessions = workspace_bool(input, "include_sessions", true)?;
        let requested_scope = input.get("conversation_id").and_then(Value::as_str);
        if let Some(allowed_scope) = access.conversation_id() {
            ensure!(
                requested_scope.is_none_or(|scope| scope == allowed_scope),
                "outside this tool's conversation"
            );
        }
        let scope_filter = access.conversation_id().or(requested_scope);
        let network = access.network_id();
        let local = self.identity.device_id()?;

        let query_network = network.to_owned();
        let query_scope = scope_filter.map(str::to_owned);
        let projection = self.scheduler_projection()?;
        let (
            owner_epoch,
            active_bots,
            conversations,
            shown_works,
            omitted_finished,
            dispatches,
            open_refusals,
            stops,
            questions,
            checks,
            projected_jobs,
            sessions,
            heads,
        ) = self
            .read_display(access, input, "workspace_overview", move |store| {
                let network = &query_network;
                let owner_epoch = store.public_state()?.project()?.epoch;
                let scope_filter = query_scope.as_deref();
                let active_bots = store.bots(network)?;
                let active_scopes: BTreeSet<_> = active_bots
                    .iter()
                    .map(|bot| bot.conversation_id.as_str())
                    .collect();
                let conversations = store
                    .conversations(network)?
                    .into_iter()
                    .filter(|conversation| {
                        active_scopes.contains(conversation.conversation_id.as_str())
                            && scope_filter
                                .is_none_or(|scope| scope == conversation.conversation_id)
                    })
                    .collect::<Vec<_>>();
                let scope_ids: BTreeSet<_> = conversations
                    .iter()
                    .map(|conversation| conversation.conversation_id.as_str())
                    .collect();
                let query_scopes = scope_ids
                    .iter()
                    .map(|scope| (*scope).to_owned())
                    .collect::<Vec<_>>();
                let mut shown_works = Vec::new();
                let mut omitted_finished = 0;
                for conversation in &conversations {
                    let scope = &conversation.conversation_id;
                    if include_finished {
                        shown_works.extend(store.works(network, scope)?);
                    } else {
                        shown_works.extend(store.open_works(network, Some(scope))?);
                        omitted_finished += store.finished_works(network, scope)?;
                    }
                }
                let dispatches = store
                    .dispatches_for_scopes(&query_scopes)?
                    .into_iter()
                    .map(|dispatch| {
                        let waiting_sync = dispatch.state == crate::work::DispatchState::Accepted
                            && store.work(network, &dispatch.spec.work_id)?.is_none();
                        Ok((dispatch, waiting_sync))
                    })
                    .collect::<Result<Vec<_>>>()?;
                let open_refusals: BTreeSet<_> = store
                    .open_dispatches(network, None)?
                    .into_iter()
                    .map(|dispatch| dispatch.spec.work_id)
                    .collect();
                let stops = store
                    .stop_requests()?
                    .into_iter()
                    .filter(|stop| scope_ids.contains(stop.conversation.as_str()))
                    .map(|stop| {
                        let work = store.work(network, &stop.work_id)?;
                        Ok((stop, work))
                    })
                    .collect::<Result<Vec<_>>>()?;
                let questions = store
                    .unanswered_questions(network)?
                    .into_iter()
                    .filter(|question| {
                        question["conversation_id"]
                            .as_str()
                            .is_some_and(|scope| scope_ids.contains(scope))
                    })
                    .collect::<Vec<_>>();
                let checks = store
                    .pending_checks()?
                    .into_iter()
                    .filter(|check| scope_ids.contains(check.conversation_id.as_str()))
                    .collect::<Vec<_>>();
                let projected_jobs = store
                    .scheduler_jobs_for_scopes(&query_scopes, include_finished)?
                    .into_iter()
                    .map(|job| {
                        let hold = projection.hold_reason(store, network, &job)?;
                        let context = store.scheduler_job_context_projection(&job)?;
                        Ok((job, hold, context))
                    })
                    .collect::<Result<Vec<_>>>()?;
                let sessions = if include_sessions {
                    store
                        .attributed_sessions_all(network)?
                        .into_iter()
                        .filter(|session| {
                            session["conversation_id"]
                                .as_str()
                                .is_some_and(|scope| scope_ids.contains(scope))
                        })
                        .collect::<Vec<_>>()
                } else {
                    Vec::new()
                };
                let heads = store.heads(network)?;
                Ok((
                    owner_epoch,
                    active_bots,
                    conversations,
                    shown_works,
                    omitted_finished,
                    dispatches,
                    open_refusals,
                    stops,
                    questions,
                    checks,
                    projected_jobs,
                    sessions,
                    heads,
                ))
            })
            .await?;
        let scope_ids: BTreeSet<_> = conversations
            .iter()
            .map(|conversation| conversation.conversation_id.as_str())
            .collect();
        let conversation_by_id: BTreeMap<_, _> = conversations
            .iter()
            .map(|conversation| (conversation.conversation_id.as_str(), conversation))
            .collect();
        let mut tasks = shown_works
            .iter()
            .map(|work| workspace_work_summary(work, "dag"))
            .collect::<Vec<_>>();

        let mut deliveries = Vec::new();
        for (dispatch, waiting_sync) in &dispatches {
            let waiting_sync = *waiting_sync;
            let spec = &dispatch.spec;
            if !include_finished
                && matches!(dispatch.state, crate::work::DispatchState::Rejected { .. })
                && !open_refusals.contains(&spec.work_id)
            {
                continue;
            }
            let state = dispatch.state.name();
            if waiting_sync || dispatch.state != crate::work::DispatchState::Accepted {
                let unknown_reason = if waiting_sync {
                    Some("accepted_waiting_sync")
                } else if matches!(
                    dispatch.state,
                    crate::work::DispatchState::Unverified { .. }
                ) {
                    Some("dispatch_unverified")
                } else {
                    None
                };
                let mut task = json!({
                    "work_id": spec.work_id,
                    "conversation_id": spec.conversation,
                    "trigger": spec.trigger,
                    "title": workspace_bounded(&spec.title, 256),
                    "cwd": workspace_bounded(&spec.cwd, 512),
                    "summary": Value::Null,
                    "state": if waiting_sync { "accepted_waiting_sync" } else { state },
                    "outcome": Value::Null,
                    "kind": spec.kind,
                    "target_node": spec.target,
                    "dispatcher": spec.dispatcher,
                    "parent": spec.parent,
                    "continues": spec.continues,
                    "thread": spec.thread,
                    "source": "local_dispatch",
                    "unknown_reason": unknown_reason,
                });
                if let Some(reason) = dispatch.state.reason() {
                    task["failure_reason"] = json!(workspace_bounded(reason, 512));
                }
                if let Some(failure) = dispatch.state.failure() {
                    task["failure"] = json!(failure);
                }
                tasks.push(task);
                deliveries.push(json!({
                    "work_id": spec.work_id,
                    "conversation_id": spec.conversation,
                    "target_node": spec.target,
                    "state": if waiting_sync { "dispatch_accepted" } else { state },
                    "kind": spec.kind,
                    "title": workspace_bounded(&spec.title, 256),
                    "cwd": workspace_bounded(&spec.cwd, 512),
                    "reason": dispatch.state.reason().map(|reason| workspace_bounded(reason, 512)),
                    "failure": dispatch.state.failure(),
                }));
            }
        }
        for (stop, work) in stops {
            let mut delivery = json!({
                "work_id": stop.work_id,
                "conversation_id": stop.conversation,
                "target_node": stop.target,
                "state": if stop.timed_out { "stop_timed_out" } else { "stop_waiting" },
                "requested_at_ms": stop.requested_at_ms,
                "deadline_ms": stop.deadline_ms,
            });
            if let Some(work) = work {
                delivery["cwd"] = json!(workspace_bounded(&work.cwd, 512));
            }
            deliveries.push(delivery);
        }

        let questions = questions
            .iter()
            .map(workspace_question_summary)
            .collect::<Vec<_>>();
        let pending_jobs: BTreeSet<_> = checks
            .iter()
            .map(|check| format!("{}\n{}", check.conversation_id, check.trigger))
            .collect();
        let jobs = projected_jobs
            .into_iter()
            .map(|(job, hold, context)| {
                let epoch = conversation_by_id
                    .get(job.conversation_id.as_str())
                    .map_or(0, |conversation| conversation.owner_epoch);
                let mut value = workspace_job_summary(&job, &local, epoch, &pending_jobs);
                value["hold_reason"] = json!(hold);
                value["context"] = json!(context);
                value
            })
            .collect::<Vec<_>>();
        for task in &mut tasks {
            let Some(work_id) = task["work_id"].as_str() else {
                continue;
            };
            let trigger = crate::scheduling::task_trigger(work_id);
            task["coordination_job"] = jobs
                .iter()
                .find(|job| job["trigger"].as_str() == Some(trigger.as_str()))
                .cloned()
                .unwrap_or(Value::Null);
        }
        let sessions = sessions
            .iter()
            .map(workspace_session_summary)
            .collect::<Vec<_>>();
        let bots = active_bots
            .into_iter()
            .filter(|bot| {
                scope_ids.contains(bot.conversation_id.as_str())
                    && scope_filter.is_none_or(|scope| scope == bot.conversation_id)
            })
            .map(|bot| {
                json!({
                    "bot_id": bot.bot_id,
                    "conversation_id": bot.conversation_id,
                    "name": bot.name,
                })
            })
            .collect::<Vec<_>>();
        let heads = heads
            .into_iter()
            .map(|(device, (sequence, _))| (device, sequence))
            .collect::<BTreeMap<_, _>>();
        let mut remote_owners = Vec::new();
        for conversation in &conversations {
            match conversation.owner.as_deref() {
                Some(owner) if owner != local => remote_owners.push(json!({
                    "owner": owner,
                    "reachable": Value::Null,
                    "status": "unknown",
                    "owner_epoch": conversation.owner_epoch,
                    "reason": "remote_scheduler_not_queried",
                })),
                None => remote_owners.push(json!({
                    "owner": Value::Null,
                    "reachable": Value::Null,
                    "status": "unknown",
                    "owner_epoch": conversation.owner_epoch,
                    "reason": "owner_unknown",
                })),
                _ => {}
            }
        }
        let conversation_rows = conversations
            .iter()
            .map(|conversation| {
                json!({
                    "conversation_id": conversation.conversation_id,
                    "title": workspace_bounded(&conversation.title, 256),
                    "owner": conversation.owner,
                    "owner_epoch": conversation.owner_epoch,
                })
            })
            .collect::<Vec<_>>();

        let mut relations = Vec::new();
        for work in &shown_works {
            if !include_finished
                && work.outcome.is_some()
                && work.outcome != Some(WorkOutcome::Unverified)
            {
                continue;
            }
            if let Some(parent) = &work.parent {
                relations.push(json!({"from_work":work.work_id,"to_work":parent,"kind":"parent"}));
                relations
                    .push(json!({"from_work":parent,"to_work":work.work_id,"kind":"dispatches"}));
            }
            if let Some(continues) = &work.continues {
                relations
                    .push(json!({"from_work":work.work_id,"to_work":continues,"kind":"continues"}));
            }
        }

        let omitted_tasks = tasks.len().saturating_sub(limit);
        let omitted_questions = questions.len().saturating_sub(limit);
        let omitted_jobs = jobs.len().saturating_sub(limit);
        let omitted_sessions = sessions.len().saturating_sub(limit);
        let omitted_deliveries = deliveries.len().saturating_sub(limit);
        let omitted_relations = relations.len().saturating_sub(limit);
        let truncated = omitted_tasks > 0
            || omitted_questions > 0
            || omitted_jobs > 0
            || omitted_sessions > 0
            || omitted_deliveries > 0
            || omitted_relations > 0
            || bots.len() > limit
            || conversation_rows.len() > limit
            || remote_owners.len() > limit
            || checks.len() > limit;
        tasks.truncate(limit);
        let mut questions = questions;
        questions.truncate(limit);
        let mut jobs = jobs;
        jobs.truncate(limit);
        let mut sessions = sessions;
        sessions.truncate(limit);
        deliveries.truncate(limit);
        relations.truncate(limit);
        let mut bots = bots;
        bots.truncate(limit);
        let mut conversation_rows = conversation_rows;
        conversation_rows.truncate(limit);
        remote_owners.truncate(limit);
        let omitted_checks = checks.len().saturating_sub(limit);
        let mut checks = checks
            .into_iter()
            .map(|check| {
                json!({
                    "check_id": check.check_id,
                    "conversation_id": check.conversation_id,
                    "trigger": check.trigger,
                    "created_at_ms": check.created_at_ms,
                    "unknown_reason": "process_unconfirmed",
                    "codex_turn": check.codex_turn.map(|turn| json!({
                        "server": turn.server,
                        "thread": turn.thread,
                        "turn": turn.turn,
                    })),
                })
            })
            .collect::<Vec<_>>();
        checks.truncate(limit);
        Ok(json!({
            "schema": WORKSPACE_OVERVIEW_SCHEMA,
            "generated_at_ms": now_ms(),
            "network_epoch": owner_epoch,
            "heads": heads,
            "consistency": if remote_owners.is_empty() { "local_snapshot" } else { "partial" },
            "truncated": truncated,
            "conversations": conversation_rows,
            "bots": bots,
            "tasks": tasks,
            "questions": questions,
            "checks": checks,
            "jobs": jobs,
            "sessions": sessions,
            "deliveries": deliveries,
            "relations": relations,
            "coverage": {
                "source": "workspace_overview",
                "complete": false,
                "bounded": true,
                "local_heads": true,
                "remote_owners": remote_owners,
                "omitted_finished": omitted_finished,
                "omitted_sessions": omitted_sessions,
                "omitted_tasks": omitted_tasks,
                "omitted_questions": omitted_questions,
                "omitted_jobs": omitted_jobs,
                "omitted_checks": omitted_checks,
                "omitted_deliveries": omitted_deliveries,
                "omitted_relations": omitted_relations,
            },
        }))
    }
}

#[tokio::test]
async fn bounded_overview_matches_frozen_reader_across_limits_and_scopes() -> Result<()> {
    let (node, admin, dir) = crate::runtime::test_support::node().await?;
    crate::runtime::test_support::bot(&node, &admin, "a", "Alpha", &[]).await?;
    crate::runtime::test_support::bot(&node, &admin, "b", "Beta", &[]).await?;
    let network = node.network_id().await?;
    let local = node.identity.device_id()?;
    let query_network = network.clone();
    node.store.call("overview.oracle.fixture",move |store| {
        store.connection.execute("UPDATE conversations SET owner=NULL WHERE network_id=?1 AND conversation_id='b'",[&query_network])?;
        for (id,scope,rank,outcome,parent) in [
            ("w-a","a","01",None,None),("w-b","a","02",Some("completed"),Some("w-a")),
            ("w-c","a","03",Some("failed"),Some("w-b")),("w-d","b","04",Some("unverified"),Some("w-c")),
        ] {
            let dispatch=json!({"type":"work_dispatched","work_id":id,"trigger":"message:input",
                "dispatcher":local,"kind":"task","cwd":"/fixture","title":id,"prompt":"SECRET PROMPT",
                "call_ref":id,"model":null,"resolved":{"provider":"codex","model":null},"parent":parent,
                "continues":parent,"thread":null});
            let finish=outcome.map(|outcome|json!({"type":"work_finished","work_id":id,"outcome":outcome,
                "result_text":"RAW RESULT SECRET","failure_reason":if outcome=="failed" {Some("failed")} else {None},"failure":null}));
            store.connection.execute("INSERT INTO works(network_id,work_id,conversation_id,target_node,dispatch,dispatch_rank,finish,finish_rank)
                VALUES(?1,?2,?3,?4,?5,?6,?7,?6)",params![query_network,id,scope,local,dispatch.to_string(),rank,finish.map(|value|value.to_string())])?;
        }
        for (id,state) in [("d-a","pending"),("d-b","accepted"),("d-c","rejected"),("d-d","unverified")] {
            let spec=json!({"work_id":id,"conversation":"a","target":local,"dispatcher":local,"trigger":"message:dispatch",
                "call_ref":id,"kind":"task","model":null,
                "cwd":"/dispatch","title":id,"prompt":"DISPATCH SECRET","parent":"w-a","continues":null,"thread":null});
            store.connection.execute("INSERT INTO work_dispatches(work_id,spec,state,reason) VALUES(?1,?2,?3,'reason')",params![id,spec.to_string(),state])?;
        }
        store.connection.execute("INSERT INTO work_stops(work_id,conversation_id,target_node,requested_at_ms,deadline_ms,timed_out) VALUES('w-d','b',?1,1,2,1)",[&local])?;
        for (id,scope,rank) in [("q-a","a","01"),("q-b","b","02")] {
            let question=json!({"type":"question_asked","question_id":id,"text":"Which format?","severity":"attention","choices":[],"attention":{"kind":"question","device_id":null},
                "about":{"node":local,"agent":"codex","session_id":"session","cwd":"/question","summary":"Summary"}});
            store.connection.execute("INSERT INTO questions(network_id,question_id,conversation_id,rank,author,created_at_ms,node,session_id,body)
                VALUES(?1,?2,?3,?4,?5,1,?5,?2,?6)",params![query_network,id,scope,rank,local,question.to_string()])?;
            store.connection.execute("INSERT INTO session_owners(network_id,node,session_id,conversation_id,rank,attribution,muted,created_at_ms)
                VALUES(?1,?2,?3,?4,?5,?6,0,1)",params![query_network,local,id,scope,format!("1:00000000000000000001:{rank}"),json!({"agent":"codex","cwd":"/session","summary":"handoff"}).to_string()])?;
        }
        for (scope,trigger,sequence,state) in [("a","message:before",0,"queued"),("b","task:w-d",2,"queued"),("a","task:w-a",3,"queued")] {
            store.connection.execute("INSERT INTO scheduler_jobs(conversation_id,trigger,kind,sequence,basis,reply_to,message_id,state,tool_calls)
                VALUES(?1,?2,'human',?3,'{}','input',?2,?4,0)",params![scope,trigger,sequence,state])?;
        }
        store.connection.execute("INSERT INTO pending_checks(attempt_id,conversation_id,trigger,detail,created_at_ms)
            VALUES('attempt-a','b','task:w-d','PRIVATE CHECK',2),('attempt-b','a','other','PRIVATE CHECK',1)",[])?;
        store.connection.execute("INSERT INTO manager_turns(attempt_id,server,thread_id,turn_id) VALUES('attempt-b','server','thread',NULL)",[])?;
        Ok(())
    }).await?;
    let user = Access::User {
        network_id: network.clone(),
        scope: UserScope::Network,
    };
    for limit in [1, 2, 256] {
        for include_finished in [false, true] {
            for include_sessions in [false, true] {
                for scope in [None, Some("a"), Some("b")] {
                    let mut input = json!({"limit":limit,"include_finished":include_finished,"include_sessions":include_sessions});
                    if let Some(scope) = scope {
                        input["conversation_id"] = json!(scope);
                    }
                    let mut actual = node.workspace_overview(&user, &input).await?;
                    let mut expected = node.workspace_overview_reference(&user, &input).await?;
                    actual.as_object_mut().unwrap().remove("generated_at_ms");
                    expected.as_object_mut().unwrap().remove("generated_at_ms");
                    assert_eq!(actual, expected, "query {input}");
                    let encoded = actual.to_string();
                    for secret in [
                        "SECRET PROMPT",
                        "RAW RESULT SECRET",
                        "DISPATCH SECRET",
                        "PRIVATE CHECK",
                    ] {
                        assert!(!encoded.contains(secret));
                    }
                }
            }
        }
    }
    let first = node.workspace_overview(&user, &json!({"limit":1})).await?;
    assert_eq!(first["tasks"][0]["work_id"], "w-a");
    assert_eq!(first["tasks"][0]["source"], "dag");
    assert_eq!(first["tasks"][0]["coordination_job"]["trigger"], "task:w-a");
    assert_eq!(first["jobs"][0]["trigger"], "message:before");
    assert_eq!(first["consistency"], "partial");
    assert_eq!(first["coverage"]["source"], "workspace_overview");
    assert_eq!(first["coverage"]["omitted_finished"], 2);
    assert_eq!(first["coverage"]["omitted_tasks"], 6);
    assert_eq!(first["coverage"]["omitted_deliveries"], 4);
    assert_eq!(first["coverage"]["omitted_relations"], 2);
    assert_eq!(first["coverage"]["omitted_jobs"], 2);
    assert_eq!(first["coverage"]["omitted_checks"], 1);
    assert_eq!(
        first["checks"][0]["codex_turn"],
        json!({"server":"server","thread":"thread","turn":null})
    );
    assert_eq!(first["coverage"]["omitted_questions"], 1);
    assert_eq!(first["coverage"]["omitted_sessions"], 1);
    assert!(
        !first["heads"]
            .as_object()
            .context("snapshot heads")?
            .is_empty()
    );
    let all = node.workspace_overview(&user, &json!({})).await?;
    let accepted = all["tasks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|task| task["work_id"] == "d-b")
        .context("waiting dispatch")?;
    assert_eq!(accepted["state"], "accepted_waiting_sync");
    assert_eq!(accepted["unknown_reason"], "accepted_waiting_sync");
    for (id, state, unknown) in [
        ("d-a", "dispatch_waiting", None),
        ("d-c", "dispatch_rejected", None),
        ("d-d", "dispatch_unverified", Some("dispatch_unverified")),
    ] {
        let task = all["tasks"]
            .as_array()
            .unwrap()
            .iter()
            .find(|task| task["work_id"] == id)
            .context("dispatch task")?;
        let delivery = all["deliveries"]
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["work_id"] == id)
            .context("dispatch delivery")?;
        assert_eq!(task["state"], state);
        assert_eq!(delivery["state"], state);
        assert_eq!(task["unknown_reason"], json!(unknown));
    }

    let scoped = Access::User {
        network_id: network.clone(),
        scope: UserScope::Conversation("a".into()),
    };
    assert!(
        node.workspace_overview(&scoped, &json!({"conversation_id":"b"}))
            .await
            .is_err()
    );
    let agent = Access::Agent(AgentAccess::ReadOnly {
        network_id: network.clone(),
        conversation_id: "a".into(),
    });
    assert!(node.workspace_overview(&agent, &json!({})).await.is_err());
    assert!(
        node.workspace_overview(&user, &json!({"conversation_id":"missing"}))
            .await
            .is_err()
    );
    node.shutdown().await;
    std::fs::remove_dir_all(dir)?;
    Ok(())
}

#[tokio::test]
async fn populated_contexts_match_frozen_reader_with_steering_quota_and_session_scopes()
-> Result<()> {
    use crate::scheduling::{
        ContextProjection, ContextStage, JobState, SessionScope, SessionStatusProjection, Shown,
        SteerOutcome, SteerReceipt, SteerTarget,
    };
    let (node, admin, dir) = crate::runtime::test_support::node().await?;
    crate::runtime::test_support::bot(&node, &admin, "context", "Context", &[]).await?;
    let network = node.network_id().await?;
    let query_network = network.clone();
    node.store.call("overview.context.fixture",move |store| {
        let scopes=[SessionScope::Main,SessionScope::Task {work_id:"work".into()},SessionScope::Routine {trigger:"schedule:fixture:1".into()}];
        for (index,scope) in scopes.into_iter().enumerate() {
            let attempt=format!("attempt-{index}");
            let trigger=format!("running-{index}");
            let context=ContextProjection {
                network_id:query_network.clone(),scope:SessionScope::Main,attempt_id:attempt.clone(),provider:Provider::Codex,
                stable_fingerprint:"stable".into(),dynamic_fingerprint:"dynamic".into(),session_generation:7,
                coverage:Default::default(),stable_bytes:10,dynamic_bytes:20,invalidation_reason:None,stage:ContextStage::Started,
                session_status:Some(SessionStatusProjection {resumable:false,pending_generation:true}),
                sources:vec![json!({"source_event":"source-retained"})],sources_omitted:3,
                inputs:vec![json!({"stale_input":"must be replaced"})],inputs_omitted:99,
            };
            let shown=Shown {context:Some(context),..Default::default()};
            store.connection.execute("INSERT INTO scheduler_sessions(conversation_id,scope,next_sequence,active,native_id,covers,owner_epoch,permission_version,shown,provider)
                VALUES('context',?1,100,?2,'native','{\"author\":1}',1,1,?3,'codex')",params![scope.storage_key(),trigger,serde_json::to_string(&shown)?])?;
            store.connection.execute("INSERT INTO scheduler_jobs(conversation_id,scope,trigger,kind,sequence,basis,reply_to,message_id,state,attempt_id,attempt_boot,attempt_permission,tool_calls)
                VALUES('context',?1,?2,'human',?3,'{}','input',?2,'running',?4,'boot',1,0)",params![scope.storage_key(),trigger,index,attempt])?;
        }
        store.connection.execute("INSERT INTO scheduler_jobs(conversation_id,trigger,kind,sequence,basis,reply_to,message_id,state,attempt_id,attempt_boot,attempt_permission,tool_calls)
            VALUES('context','different-attempt','human',3,'{}','input','reply-other','running','other-attempt','boot',1,0)",[])?;
        for index in 0..70 {
            let into=SteerTarget {provider:Provider::Codex,attempt_id:"attempt-0".into(),thread_id:"native".into(),turn_id:"expected".into()};
            let receipt=Some(SteerReceipt {accepted_at_ms:index,actual_turn_id:Some("actual".into())});
            let state=match index%5 {
                0=>JobState::Steering {into,receipt,uncertain:false,unread_since_ms:None,running_since_ms:None},
                1=>JobState::Steering {into,receipt:None,uncertain:true,unread_since_ms:None,running_since_ms:None},
                2=>JobState::Steering {into,receipt:None,uncertain:false,unread_since_ms:None,running_since_ms:None},
                3=>JobState::Steered {into,receipt,outcome:SteerOutcome::Answered},
                _=>JobState::Steered {into,receipt:Some(SteerReceipt {accepted_at_ms:index,actual_turn_id:None}),outcome:SteerOutcome::Unknown},
            };
            let input=format!("input-{index:02}");
            store.connection.execute("INSERT INTO scheduler_jobs(conversation_id,trigger,kind,sequence,basis,reply_to,message_id,state,detail,tool_calls)
                VALUES('context',?1,'human',?2,'{}',?1,?1,?3,?4,0)",params![input,index+10,state.name(),serde_json::to_string(&state)?])?;
            if index<67 {
                store.connection.execute("INSERT INTO messages(network_id,conversation_id,message_id,event_hash,rank,author,role,text,created_at_ms,attachments)
                    VALUES(?1,'context',?2,?3,?2,'author','user','input',1,'[]')",params![query_network,input,format!("source-{index:02}")])?;
            }
        }
        Ok(())
    }).await?;
    let access = Access::User {
        network_id: network,
        scope: UserScope::Network,
    };
    for limit in [1, 256] {
        for include_finished in [false, true] {
            let input = json!({"limit":limit,"include_finished":include_finished});
            let mut actual = node.workspace_overview(&access, &input).await?;
            let mut expected = node.workspace_overview_reference(&access, &input).await?;
            actual.as_object_mut().unwrap().remove("generated_at_ms");
            expected.as_object_mut().unwrap().remove("generated_at_ms");
            assert_eq!(actual, expected, "populated contexts {input}");
            let context = &actual["jobs"][0]["context"];
            assert_eq!(context["inputs"].as_array().unwrap().len(), 64);
            assert_eq!(context["inputs_omitted"], 6);
            assert_eq!(context["sources_omitted"], 3);
            assert_eq!(context["inputs"][0]["source_event"], "source-00");
            assert_eq!(context["inputs"][63]["message_id"], "input-63");
            for (index, stage) in [
                "provider_accepted",
                "unknown",
                "submitting",
                "associated",
                "unknown",
            ]
            .into_iter()
            .enumerate()
            {
                assert_eq!(context["inputs"][index]["stage"], stage);
            }
            assert_eq!(
                context["session_status"],
                json!({"resumable":true,"pending_generation":true})
            );
            if limit == 256 {
                assert_eq!(
                    actual["jobs"][1]["context"]["scope"],
                    json!(SessionScope::Task {
                        work_id: "work".into()
                    })
                );
                assert_eq!(
                    actual["jobs"][1]["context"]["session_status"]["resumable"],
                    true
                );
                assert_eq!(
                    actual["jobs"][2]["context"]["session_status"]["resumable"],
                    false
                );
                assert!(actual["jobs"][3]["context"].is_null());
            }
        }
    }
    node.shutdown().await;
    std::fs::remove_dir_all(dir)?;
    Ok(())
}

#[test]
fn collection_byte_boundary_keeps_prefix_omissions_and_complete_heads() -> Result<()> {
    use crate::store::overview::{OverviewRows, OverviewSnapshot};
    let empty = || OverviewRows {
        rows: vec![],
        total: 0,
    };
    let cap = 8 * 1024 * 1024usize;
    for excess in [0, 1] {
        let mut task = json!({"work_id":"first","coordination_job":{"payload":""}});
        let fixed = serde_json::to_vec(&task)?.len() + 1;
        task["coordination_job"]["payload"] = json!("x".repeat(cap - fixed + excess));
        assert_eq!(serde_json::to_vec(&task)?.len() + 1, cap + excess);
        let snapshot = OverviewSnapshot {
            network_epoch: 7,
            heads: BTreeMap::from([("node-a".into(), 31), ("node-b".into(), 17)]),
            conversations: empty(),
            bots: empty(),
            tasks: OverviewRows {
                rows: vec![task, json!({"work_id":"second"})],
                total: 2,
            },
            questions: empty(),
            checks: empty(),
            jobs: empty(),
            sessions: empty(),
            deliveries: empty(),
            relations: empty(),
            remote_owners: empty(),
            omitted_finished: 19,
        };
        let actual = summary(snapshot);
        assert_eq!(
            actual["tasks"].as_array().unwrap().len(),
            usize::from(excess == 0)
        );
        assert_eq!(
            actual["coverage"]["omitted_tasks"],
            if excess == 0 { 1 } else { 2 }
        );
        assert_eq!(actual["coverage"]["omitted_finished"], 19);
        assert_eq!(actual["heads"], json!({"node-a":31,"node-b":17}));
        assert_eq!(actual["network_epoch"], 7);
        assert_eq!(actual["truncated"], true);
        assert_eq!(actual["coverage"]["source"], "workspace_overview");
        assert_eq!(actual["coverage"]["bounded"], true);
        assert_eq!(actual["coverage"]["complete"], false);
        assert_eq!(actual["consistency"], "local_snapshot");
        if excess == 0 {
            assert_eq!(actual["tasks"][0]["work_id"], "first");
        }
    }
    Ok(())
}
