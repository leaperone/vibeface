use super::super::test_support::{
    cleanup, complete_tracked_work, connected, controlled_pi, main_input_fact, with_agent,
    with_node,
};
use super::super::*;
use crate::crypto::LocalIdentity;
use crate::model::VersionVector;
use crate::runtime::test_support::{
    bot, named_socket, node, pi_fixture_log, provider_work, scripted_app_server, start_reply,
};
use std::collections::BTreeMap;
use std::sync::atomic::Ordering;

#[tokio::test]
async fn work_status_lists_dispatches_the_dag_does_not_name_yet() -> Result<()> {
    use crate::work::{Dispatch, DispatchState, WorkSpec};
    with_node(async |node, admin, _| {
        bot(&node, &admin, "c", "前端", &[]).await?;
        let network = node.network_id().await?;
        let trigger = crate::scheduling::message_trigger("m1");
        let spec = crate::work::WorkRequest {
            work_id: WorkSpec::id("c", &trigger, "r"),
            conversation: "c".into(),
            trigger,
            target: LocalIdentity::generate().device_id()?,
            dispatcher: node.identity.device_id()?,
            call_ref: "r".into(),
            kind: crate::work::RequestedWorkKind::Task,
            model: None,
            cwd: "/w".into(),
            title: "t".into(),
            prompt: "p".into(),
            continues: None,
            parent: None,
            thread: None,
        };
        {
            let dispatch = (Dispatch {
                spec: spec.clone(),
                state: DispatchState::Pending,
            })
            .to_owned();
            node.store
                .call("triage.test.save_dispatch", move |store| {
                    store.save_dispatch(&dispatch)
                })
                .await
        }?;
        let access = crate::runtime::AgentAccess::ReadOnly {
            network_id: network,
            conversation_id: "c".into(),
        };
        let all = node.tool(access.clone(), "work_status", json!({})).await?;
        assert_eq!(
            (&all["works"][0]["work_id"], &all["works"][0]["state"]),
            (&json!(spec.work_id), &json!("dispatch_waiting"))
        );
        let one = node
            .tool(access, "work_status", json!({"work_id": spec.work_id}))
            .await?;
        assert_eq!(one["state"], "dispatch_waiting");
        assert_eq!(one["call_record"]["execution"], "read");
        assert_eq!(one["call_record"]["call_ref"], spec.call_ref);
        Ok::<_, anyhow::Error>(())
    })
    .await
}

#[tokio::test]
async fn a_work_asking_what_its_agent_cannot_do_is_refused_before_it_is_registered() -> Result<()> {
    with_node(async |node, admin, dir| {
        with_agent(&node);
        node.settings.lock().unwrap().roots = vec![dir.clone()];
        {
            let mut providers = node.providers.lock().unwrap();
            for provider in [Provider::Claude, Provider::Opencode] {
                providers.switches.execution.insert(provider);
                let bin = format!("/nonexistent/{}", provider.name());
                (providers.bins).insert(provider, bin.into());
            }
        }
        bot(&node, &admin, "c", "前端", &[]).await?;
        let network = node.network_id().await?;
        let (trigger, attempt) = start_reply(&node, &admin, "m1").await?;
        let access = crate::runtime::AgentAccess::ReadOnly {
            network_id: network.clone(),
            conversation_id: "c".into(),
        };
        let access = node
            .manager_access_fixture("token", access.clone(), &attempt, &trigger, Provider::Codex)
            .await?;
        let asks = |call_ref: &str, agent: &str, extra: Value| {
            let mut args = json!({"cwd":dir.display().to_string(),"call_ref":call_ref,
                "title":"t","prompt":"p","agent":agent});
            args.as_object_mut()
                .unwrap()
                .extend(extra.as_object().unwrap().clone());
            args
        };
        let images = node
            .tool(
                access.clone(),
                "track_work",
                asks("r1", "claude", json!({"images":["/a.png"]})),
            )
            .await;
        let effort = node
            .tool(
                access.clone(),
                "track_work",
                asks("r2", "opencode", json!({"effort":"high"})),
            )
            .await;
        let plain = node
            .tool(
                access.clone(),
                "track_work",
                asks("r3", "claude", json!({"images":[]})),
            )
            .await;
        node.finish_manager_attempt(&attempt).await;
        let images = format!("{:#}", images.expect_err("Claude Code takes no images"));
        assert!(
            images.contains("takes no images") && images.contains("\"pi\""),
            "{images}"
        );
        let effort = format!("{:#}", effort.expect_err("opencode takes no effort"));
        assert!(effort.contains("takes no effort"), "{effort}");
        let plain = format!(
            "{:#}",
            plain.expect_err("the start fails on the missing program")
        );
        assert!(
            !plain.contains("takes no"),
            "an empty image list asks for nothing: {plain}"
        );
        for call_ref in ["r1", "r2"] {
            let work_id = crate::work::WorkSpec::id("c", &trigger, call_ref);
            assert!(
                {
                    let work_id = (work_id).to_owned();
                    node.store
                        .call("triage.test.work_record", move |store| {
                            store.work_record(&work_id)
                        })
                        .await
                }?
                .is_none(),
                "{call_ref} left no work behind"
            );
        }
        Ok::<_, anyhow::Error>(())
    })
    .await
}

#[tokio::test]
async fn a_work_without_agent_goes_on_where_its_thread_or_original_ran() -> Result<()> {
    use crate::model::WorkKind;
    with_node(async |node, admin, dir| {
        with_agent(&node);
        if let Some(manager) = node.settings.lock().unwrap().manager.as_mut() {
            manager.provider = Provider::Claude;
        }
        {
            let mut providers = node.providers.lock().unwrap();
            providers.switches.execution.insert(Provider::Claude);
            (providers.bins).insert(Provider::Claude, "/nonexistent/claude".into());
        }
        node.settings.lock().unwrap().roots = vec![dir.canonicalize()?];
        let native_cwd = dir.canonicalize()?.display().to_string();
        let mut native_queries = scripted_app_server(&node, move |request| match request["method"].as_str() {
            Some("thread/read") => json!({"result":{"thread":{"id":request["params"]["threadId"],"cwd":native_cwd}}}),
            _ => json!({"result":{}}),
        }).await?;
        tokio::time::timeout(Duration::from_secs(5), connected(&node)).await?;
        bot(&node, &admin, "c", "前端", &[]).await?;
        let network = node.network_id().await?;
        let (trigger, attempt) = start_reply(&node, &admin, "m1").await?;
        let access = crate::runtime::AgentAccess::ReadOnly { network_id: network.clone(), conversation_id: "c".into() };
        let access = node.manager_access_fixture(
            "token", access.clone(), &attempt, &trigger, Provider::Claude,
        ).await?;
        let local = node.identity.device_id()?;
        let origin = main_input_fact(&node, &admin, "c", "m0").await?;
        let original = crate::work::WorkSpec {
            work_id: "w-codex".into(),
            trigger: origin,
            call_ref: "w-codex".into(),
            thread: Some("t-done".into()),
            ..provider_work(Provider::Codex, &local, Path::new(""), "p")
        };
        {
            let identity = (node.identity).to_owned();
            let network = (network).to_owned();
            let spec = original;
            node.store.call("triage.test.accept_work_fixture", move |store| {
                store.accept_work_fixture(&identity, &network, spec, now_ms())
            }).await
        }?;
        complete_tracked_work(&node, &network, "w-codex").await?;
        let cwd = dir.display().to_string();
        let call = |args: Value| node.tool(access.clone(), "track_work", args);
        let answered = call(json!({"thread":"t-codex","call_ref":"a1","title":"t","prompt":"yes"})).await?;
        complete_tracked_work(&node, &network, answered["work_id"].as_str().context("answered work")?).await?;
        let followed = call(json!({"continues":"w-codex","cwd":cwd,"call_ref":"f1","title":"t","prompt":"more"})).await?;
        node.bind_work(
            &network,
            "c",
            followed["work_id"].as_str().context("followed work")?,
            vibeface_bridge::codex::StartReport::Thread {
                thread_id: followed["thread_id"].as_str().context("continued thread")?.into(),
            },
        )
        .await?;
        complete_tracked_work(&node, &network, followed["work_id"].as_str().context("followed work")?).await?;
        let crossed = call(json!({"continues":"w-codex","cwd":cwd,"call_ref":"f2","title":"t","prompt":"more","agent":"claude"})).await;
        let foreign = call(json!({"thread":"t-codex","call_ref":"a2","title":"t","prompt":"yes","agent":"claude"})).await;
        let ready = node.tool(access.clone(), "wait_work", json!({"thread":"t-codex"})).await;
        {
            let mut providers = node.providers.lock().unwrap();
            providers.switches.execution = [Provider::Codex].into();
        }
        let fresh = call(json!({"cwd":cwd,"call_ref":"n1","title":"t","prompt":"new"})).await;
        node.providers.lock().unwrap().switches.execution.insert(Provider::Claude);
        let retried = call(json!({"cwd":cwd,"call_ref":"n1","title":"t","prompt":"new"})).await;
        let changed = call(json!({"cwd":cwd,"call_ref":"n1","title":"t","prompt":"other"})).await;
        node.finish_manager_attempt(&attempt).await;

        let kind = async |work_id: &Value| -> Result<WorkKind> {
            let id = work_id.as_str().context("work_id")?.to_owned();
            node.store.call("triage.test.work_kind", move |store| {
                Ok(store.work_record(&id)?.context("recorded")?.kind)
            }).await
        };
        assert_eq!(answered["may_send"], true, "the Codex CLI sends the answer");
        assert_eq!(kind(&answered["work_id"]).await?, WorkKind::Agent(Provider::Codex));
        assert_eq!(kind(&followed["work_id"]).await?, WorkKind::Agent(Provider::Codex));
        for (refused, what) in [(crossed, "continuation"), (foreign, "thread")] {
            let refused = format!("{:#}", refused.expect_err(what));
            assert!(refused.contains("agent") && refused.contains("Codex"), "{what}: {refused}");
        }
        assert_eq!(ready?["agent"], "codex", "the CLI reads a Codex thread first");
        let fresh = fresh?;
        assert_eq!(kind(&fresh["work_id"]).await?, WorkKind::Agent(Provider::Codex));
        let retried = retried?;
        assert_eq!(retried["work_id"], fresh["work_id"]);
        assert_eq!(retried["may_send"], false, "a retry only reads back");
        let changed = format!("{:#}", changed.expect_err("another prompt under one call_ref"));
        assert!(changed.contains("prompt") && !changed.contains("kind"), "{changed}");
        let native_queries: Vec<_> = std::iter::from_fn(|| native_queries.try_recv().ok()).collect();
        assert!(native_queries.iter().any(|request| request["method"] == "thread/read"
            && request["params"]["threadId"] == "t-codex"), "thread inputs verify the native directory before admission");
        Ok::<_, anyhow::Error>(())
    })
    .await
}

#[tokio::test]
async fn a_bad_call_ref_is_named_so_the_agent_fixes_it_at_once() -> Result<()> {
    with_node(async |node, admin, _| {
        with_agent(&node);
        bot(&node, &admin, "c", "前端", &[]).await?;
        let network = node.network_id().await?;
        let (trigger, attempt) = start_reply(&node, &admin, "m1").await?;
        let access = crate::runtime::AgentAccess::ReadOnly {
            network_id: network.clone(),
            conversation_id: "c".into(),
        };
        let access = node
            .manager_access_fixture(
                "token",
                access.clone(),
                &attempt,
                &trigger,
                crate::providers::Provider::Codex,
            )
            .await?;
        let create = |call_ref: &str| json!({"call_ref":call_ref,"name":"2SOMEone/LEAPERone"});
        let refused = node
            .tool(access.clone(), "create_bot", create("bots/2SOMEone"))
            .await;
        let created = node
            .tool(access, "create_bot", create("create-2someone"))
            .await;
        node.finish_manager_attempt(&attempt).await;
        let refused = refused.expect_err("a call_ref with a slash");
        assert!(
            refused
                .to_string()
                .starts_with("arguments.call_ref is invalid: id must contain"),
            "{refused:#}"
        );
        assert_eq!(created?["name"], "2SOMEone/LEAPERone");
        Ok::<_, anyhow::Error>(())
    })
    .await
}

/// A Codex App Server fixture for every connection: `threads` (ID to directory) read and
/// resume, others are "thread not loaded" as Codex 0.167 says; turn/start runs a turn,
/// thread/start makes a thread. Returns its task, every request and the turns by thread.
async fn codex_fixture(
    socket: &Path,
    threads: &[(&str, &str)],
) -> Result<(
    tokio::task::JoinHandle<()>,
    Arc<std::sync::Mutex<Vec<Value>>>,
    Arc<std::sync::Mutex<BTreeMap<String, Value>>>,
)> {
    use futures_util::{SinkExt as _, StreamExt as _};
    let mut listener = vibeface_bridge::platform::ipc::Listener::bind(
        &vibeface_bridge::platform::ipc::Endpoint::from_path(socket)?,
    )
    .await?;
    let dirs: Arc<std::sync::Mutex<BTreeMap<String, String>>> = Arc::new(std::sync::Mutex::new(
        threads
            .iter()
            .map(|(id, cwd)| ((*id).into(), (*cwd).into()))
            .collect(),
    ));
    let requests = Arc::new(std::sync::Mutex::new(Vec::<Value>::new()));
    let turns = Arc::new(std::sync::Mutex::new(BTreeMap::<String, Value>::new()));
    let (seen, served) = (requests.clone(), turns.clone());
    let task = tokio::spawn(async move {
        let mut sessions = tokio::task::JoinSet::new();
        while let Ok(stream) = listener.accept().await {
            let (seen, turns, dirs) = (seen.clone(), served.clone(), dirs.clone());
            sessions.spawn(async move {
                let Ok(mut server) = tokio_tungstenite::accept_async(stream).await else { return; };
                while let Some(Ok(message)) = server.next().await {
                    let Ok(request) = serde_json::from_slice::<Value>(&message.into_data()) else { continue; };
                    seen.lock().unwrap().push(request.clone());
                    let thread = request["params"]["threadId"].as_str().unwrap_or_default().to_owned();
                    let reply = {
                        let mut turns = turns.lock().unwrap();
                        let mut dirs = dirs.lock().unwrap();
                        let known = dirs.get(&thread).cloned();
                        match (request["method"].as_str(), known) {
                            (Some("initialize"), _) => json!({"result":{"userAgent":"codex/fixture"}}),
                            (Some("thread/read" | "thread/resume"), Some(cwd)) => {
                                let turn = turns.get(&thread).cloned();
                                let active = turn.as_ref().is_some_and(|turn| turn["status"] == "inProgress");
                                json!({"result":{"thread":{"id":thread,"cwd":cwd,"source":"cli",
                                    "status":{"type":if active {"active"} else {"idle"}},
                                    "turns":turn.into_iter().collect::<Vec<_>>()}}})
                            }
                            (Some("thread/read" | "thread/resume"), None) => {
                                json!({"error":{"code":-32600,"message":format!("thread not loaded: {thread}")}})
                            }
                            (Some("turn/start"), Some(_)) => {
                                let turn = json!({"id":format!("turn-{thread}"),"status":"inProgress",
                                    "items":[{"type":"userMessage","clientId":request["params"]["clientUserMessageId"]}]});
                                turns.insert(thread, turn.clone());
                                json!({"result":{"turn":turn}})
                            }
                            (Some("thread/start"), _) => {
                                let id = format!("new-{}", dirs.len());
                                dirs.insert(id.clone(), request["params"]["cwd"].as_str().unwrap_or_default().into());
                                json!({"result":{"thread":{"id":id}}})
                            }
                            _ => json!({"result":{}}),
                        }
                    };
                    if request.get("id").is_some() {
                        let mut reply = reply;
                        reply["id"] = request["id"].clone();
                        let _ = server.send(reply.to_string().into()).await;
                    }
                }
            });
        }
    });
    Ok((task, requests, turns))
}

/// The user's own Codex sessions live on their own App Server, which the Node only reaches
/// when its socket is set. Only threads found there are sent there, keeping the session's
/// settings, and the Node settles them by reading them there; its own threads and new
/// threads stay on its own App Server. A changed socket only serves later lookups, and work
/// there never holds a launch change. The test Node's named App Server stands in for the
/// one it starts.
#[tokio::test]
async fn sends_reach_the_users_own_codex_only_for_threads_found_there() -> Result<()> {
    let (node, admin, dir) = node().await?;
    let cwd = dir.canonicalize()?.display().to_string();
    let (own, own_seen, _) = codex_fixture(&named_socket(&node), &[("t-own", &cwd)]).await?;
    let user_dir =
        vibeface_bridge::platform::ipc::runtime_dir(&uuid::Uuid::new_v4().simple().to_string())?;
    vibeface_bridge::platform::privatefs::create_dir(&user_dir)?;
    let user_socket = user_dir.join("user.sock");
    let (user, user_seen, user_turns) = codex_fixture(&user_socket, &[("t-user", &cwd)]).await?;
    let result = async {
        with_agent(&node);
        node.settings.lock().unwrap().roots = vec![dir.canonicalize()?];
        bot(&node, &admin, "c", "Bot", &[]).await?;
        let network = node.network_id().await?;
        let (trigger, attempt) = start_reply(&node, &admin, "m1").await?;
        let access = node
            .manager_access_fixture(
                "user-codex-token",
                crate::runtime::AgentAccess::ReadOnly {
                    network_id: network.clone(),
                    conversation_id: "c".into(),
                },
                &attempt,
                &trigger,
                Provider::Codex,
            )
            .await?;
        connected(&node).await;
        let dispatch = |thread: Option<&str>, call: &str| {
            let mut args = json!({"prompt":"p","call_ref":call,"agent":"codex"});
            match thread {
                Some(thread) => args["thread"] = json!(thread),
                None => {
                    args["cwd"] = json!(cwd);
                    args["title"] = json!("t");
                }
            }
            node.tool(access.clone(), "dispatch_work", args)
        };
        let works = async || -> Result<Vec<crate::work::WorkRecord>> {
            node.store
                .call("triage.test.work_records", |store| store.work_records())
                .await
        };
        let wrote = |seen: &std::sync::Mutex<Vec<Value>>, thread: &str| {
            seen.lock().unwrap().iter().any(|request| {
                request["params"]["threadId"] == thread
                    && request["method"].as_str().is_some_and(|method| {
                        method.starts_with("turn/") || method == "thread/resume"
                    })
            })
        };

        let refused = dispatch(Some("t-user"), "unset")
            .await
            .expect_err("no user socket yet");
        assert!(
            refused
                .to_string()
                .contains("is not on this device's Codex App Server"),
            "{refused:#}"
        );
        assert!(works().await?.is_empty(), "nothing was registered");

        node.codex.set_user_socket(Some(user_socket.clone()));
        let sent = dispatch(Some("t-user"), "user").await?;
        let work_id = sent["work_id"].as_str().context("user work")?.to_owned();
        assert!(
            !wrote(&own_seen, "t-user"),
            "nothing went to the Node's own App Server"
        );
        let start = user_seen
            .lock()
            .unwrap()
            .iter()
            .find(|request| request["method"] == "turn/start")
            .cloned()
            .context("sent on the user's App Server")?;
        assert_eq!(start["params"]["threadId"], "t-user");
        for kept in ["approvalPolicy", "sandboxPolicy", "model"] {
            assert!(
                start["params"].get(kept).is_none(),
                "the session keeps its own {kept}: {start}"
            );
        }
        let record = async |id: &str| -> Result<crate::work::WorkRecord> {
            let id = id.to_owned();
            node.store
                .call("triage.test.work_record", move |store| {
                    store.work_record(&id)
                })
                .await?
                .context("work")
        };
        let running = record(&work_id).await?;
        assert_eq!(
            running
                .state
                .handle()
                .context("running on the user's turn")?
                .turn_id,
            "turn-t-user"
        );
        // A changed or cleared socket only serves later lookups: the thread stays where it
        // was found, so a chat message steered into its turn and the reads go there too.
        node.codex.set_user_socket(None);
        assert_eq!(node.codex.user_thread("t-user"), Some(user_socket.clone()));
        // A new send follows the cleared setting and is refused.
        assert!(matches!(
            node.codex.locate("t-user").await,
            vibeface_control::agent::ThreadAt::Missing { .. }
        ));
        use crate::runtime::executor::{Executor as _, SteerAck, SteerInput, SteerTrace};
        let into = crate::scheduling::SteerTarget {
            provider: Provider::Codex,
            attempt_id: format!("work:{work_id}"),
            thread_id: "t-user".into(),
            turn_id: "turn-t-user".into(),
        };
        let input = SteerInput {
            text: "more".into(),
            files: Vec::new(),
        };
        assert_eq!(
            node.codex.send_turn(&into, "steer-1", &input).await,
            SteerAck::Accepted { turn_id: None }
        );
        assert!(
            user_seen
                .lock()
                .unwrap()
                .iter()
                .any(|request| request["method"] == "turn/steer"
                    && request["params"]["threadId"] == "t-user"),
            "steered on the user's App Server"
        );
        assert!(!wrote(&own_seen, "t-user"));
        assert_eq!(
            node.codex.read_back(&into, "steer-1").await,
            Some(SteerTrace::Pending)
        );
        user_turns
            .lock()
            .unwrap()
            .get_mut("t-user")
            .context("user turn")?["status"] = json!("completed");
        node.probe_work(&network, &work_id, true).await?;
        assert!(
            matches!(
                record(&work_id).await?.state,
                crate::work::WorkState::Finished { .. }
            ),
            "the Node read the user's turn and settled it"
        );
        node.codex.set_user_socket(Some(user_socket.clone()));

        let own_work = dispatch(Some("t-own"), "own").await?;
        assert!(own_work["state"]["state"] == "running", "{own_work}");
        let own_start = own_seen
            .lock()
            .unwrap()
            .iter()
            .find(|request| request["method"] == "turn/start")
            .cloned()
            .context("sent on the Node's App Server")?;
        assert_eq!(own_start["params"]["threadId"], "t-own");
        assert_eq!(own_start["params"]["approvalPolicy"], "never");
        assert!(
            !user_seen
                .lock()
                .unwrap()
                .iter()
                .any(|request| request["params"]["threadId"] == "t-own"),
            "the user's App Server is not asked about the Node's thread"
        );

        // One work at a time holds the folder.
        let own_id = own_work["work_id"].as_str().context("own work")?;
        node.signal_work(
            &network,
            own_id,
            crate::work::Signal::TurnCompleted {
                turn_id: "turn-t-own".into(),
                status: vibeface_bridge::turn::TurnStatus::Completed,
                text: "done".into(),
                error: None,
            },
        )
        .await?;
        let fresh = dispatch(None, "new").await?;
        assert!(fresh["state"]["state"] == "running", "{fresh}");
        assert!(
            own_seen
                .lock()
                .unwrap()
                .iter()
                .any(|request| request["method"] == "thread/start"),
            "a new thread starts on the Node's App Server"
        );
        assert!(
            !user_seen
                .lock()
                .unwrap()
                .iter()
                .any(|request| request["method"] == "thread/start"),
            "never on the user's"
        );
        let fresh_id = fresh["work_id"].as_str().context("new work")?;
        let fresh_turn = record(fresh_id)
            .await?
            .state
            .handle()
            .context("new turn")?
            .turn_id
            .clone();
        node.signal_work(
            &network,
            fresh_id,
            crate::work::Signal::TurnCompleted {
                turn_id: fresh_turn,
                status: vibeface_bridge::turn::TurnStatus::Completed,
                text: "done".into(),
                error: None,
            },
        )
        .await?;
        let again = dispatch(Some("t-user"), "user-again").await?;
        assert!(again["state"]["state"] == "running", "{again}");

        let registered = works().await?.len();
        let refused = dispatch(Some("t-gone"), "gone")
            .await
            .expect_err("neither App Server has it");
        for said in [
            "is not on this device's Codex App Server or the user's own Codex",
            "nothing was registered or sent",
            "must not be copied, linked or resumed",
        ] {
            assert!(refused.to_string().contains(said), "{refused:#}");
        }
        assert!(!wrote(&own_seen, "t-gone") && !wrote(&user_seen, "t-gone"));

        user.abort();
        let _ = std::fs::remove_file(&user_socket);
        let refused = dispatch(Some("t-elsewhere"), "unreachable")
            .await
            .expect_err("the user's App Server is gone");
        for said in [
            "cannot tell which Codex App Server has thread t-elsewhere",
            "the user's own Codex App Server did not answer",
            "nothing was registered or sent",
        ] {
            assert!(refused.to_string().contains(said), "{refused:#}");
        }
        assert_eq!(
            works().await?.len(),
            registered,
            "neither refusal registered a work"
        );
        assert!(
            !refused
                .to_string()
                .contains(&*user_socket.to_string_lossy()),
            "the chat never sees the socket: {refused:#}"
        );
        node.finish_manager_attempt(&attempt).await;

        // A launch change waits only for work on the Node's own App Server, and the user's
        // turn still running does not hold that launch.
        {
            let (identity, network, trigger, attempt) = (
                node.identity.clone(),
                network.clone(),
                trigger.clone(),
                attempt.clone(),
            );
            node.store
                .call("triage.test.finish_user_codex_reply", move |store| {
                    store
                        .finish_job(
                            &identity,
                            &network,
                            "c",
                            &trigger,
                            &attempt,
                            crate::scheduling::JobOutcome::Completed,
                            "done",
                            now_ms(),
                        )
                        .map(|_| ())
                })
                .await?;
        }
        node.pending_provider_launches.store(true, Ordering::SeqCst);
        node.codex.configuration_changed();
        assert!(
            node.apply_provider_launches().await?,
            "the user's running turn does not hold a launch change"
        );
        assert!(!node.pending_provider_launches.load(Ordering::SeqCst));
        assert!(
            !node
                .codex
                .configuration_pending(&node.providers().codex_target(true)),
            "nor the Node's App Server launch"
        );
        Ok::<_, anyhow::Error>(())
    }
    .await;
    own.abort();
    user.abort();
    cleanup(node, dir).await?;
    let _ = std::fs::remove_dir_all(user_dir);
    result
}

#[tokio::test]
async fn outside_root_dispatch_is_a_definite_refusal_but_missing_path_stays_unknown() -> Result<()>
{
    use crate::work::{RequestedWorkKind, WorkRequest, WorkSpec};
    with_node(async |node, admin, dir| {
        with_agent(&node);
        controlled_pi(&node, &dir, "reply")?;
        let allowed = dir.join("allowed");
        std::fs::create_dir(&allowed)?;
        let allowed = allowed.canonicalize()?;
        let missing_root = dir.join("missing-configured-root");
        node.settings.lock().unwrap().roots = vec![allowed.clone()];
        bot(&node, &admin, "c", "Bot", &[]).await?;
        let network = node.network_id().await?;
        let (trigger, attempt) = start_reply(&node, &admin, "root-refusal-source").await?;
        let local = node.identity.device_id()?;
        let baseline = {
            let network = network.clone();
            node.store.call("triage.test.root_refusal_baseline", move |store| {
                Ok((store.version_vector(&network)?, store.scheduler_jobs("c")?.len()))
            }).await?
        };
        for (call_ref, cwd, roots, definite) in [
            ("outside-root", dir.clone(), vec![allowed.clone()], true),
            ("missing-path", allowed.join("not-created"), vec![allowed.clone()], false),
            ("missing-root", dir.clone(), vec![missing_root.clone()], false),
            ("unmatched-and-missing-root", dir.clone(), vec![allowed.clone(), missing_root.clone()], false),
        ] {
            node.settings.lock().unwrap().roots = roots;
            let request = WorkRequest {
                work_id: WorkSpec::id("c", &trigger, call_ref),
                conversation: "c".into(), trigger: trigger.clone(), target: local.clone(),
                dispatcher: local.clone(), call_ref: call_ref.into(),
                kind: RequestedWorkKind::Agent(Some(Provider::Pi)), model: None,
                cwd: cwd.display().to_string(), title: "root authorization".into(),
                prompt: "must not execute".into(), continues: None, parent: None, thread: None,
            };
            let error = match node.accept_dispatch(&network, request.clone(), &Value::Null).await {
                Err(error) => error,
                Ok(_) => anyhow::bail!("{call_ref} unexpectedly admitted work"),
            };
            assert_eq!(error.is::<AdmissionRejected>(), definite, "{call_ref}: {error:#}");
            if definite {
                assert_eq!(error.to_string(), "execution path is outside configured roots; ask the user to allow its folder with propose_provider_config roots");
            } else {
                assert_eq!(error.downcast_ref::<std::io::Error>().context("original filesystem failure")?.kind(), std::io::ErrorKind::NotFound);
            }
            let network = network.clone();
            let baseline = baseline.clone();
            node.store.call("triage.test.root_refusal_no_side_effects", move |store| {
                assert!(store.accepted_work(&network, &request)?.is_none());
                assert!(store.work_record(&request.work_id)?.is_none());
                assert!(store.work_records()?.is_empty());
                assert_eq!(store.version_vector(&network)?, baseline.0);
                assert_eq!(store.scheduler_jobs("c")?.len(), baseline.1);
                Ok(())
            }).await?;
            assert!(pi_fixture_log(&dir)?.is_empty(), "{call_ref} must not invoke the provider");
        }
        let request = WorkRequest {
            work_id: WorkSpec::id("c", &trigger, "matching-root"),
            conversation: "c".into(),
            trigger,
            target: local.clone(),
            dispatcher: local,
            call_ref: "matching-root".into(),
            kind: RequestedWorkKind::Agent(Some(Provider::Pi)),
            model: None,
            cwd: allowed.display().to_string(),
            title: "root authorization".into(),
            prompt: "admit only; do not execute".into(),
            continues: None,
            parent: None,
            thread: None,
        };
        node.settings.lock().unwrap().roots = vec![missing_root.clone(), allowed];
        let admitted = node.accept_dispatch(&network, request.clone(), &Value::Null).await?;
        assert!(admitted.created, "a valid matching root permits admission despite another root's I/O error");
        assert_eq!(admitted.record.state, crate::work::WorkState::Accepted);
        node.settings.lock().unwrap().roots = vec![missing_root];
        let duplicate = node.accept_dispatch(&network, request.clone(), &Value::Null).await?;
        assert!(!duplicate.created, "durable admission is read before changed root settings");
        assert_eq!(duplicate.spec, admitted.spec);
        assert_eq!(duplicate.record, admitted.record);
        let stored_network = network.clone();
        node.store.call("triage.test.matching_root_admitted_once", move |store| {
            assert!(store.accepted_work(&stored_network, &request)?.is_some());
            assert_eq!(store.work_records()?.len(), 1);
            assert_eq!(store.scheduler_jobs("c")?.len(), baseline.1);
            assert_eq!(crate::store::context_after(&store.connection, &stored_network, "c", None, &VersionVector::new())?
                .events.iter().filter(|event| matches!(event.event.body, EventBody::WorkDispatched { .. })).count(), 1);
            Ok(())
        }).await?;
        assert!(pi_fixture_log(&dir)?.is_empty(), "admission and its retry do not start a provider");
        node.finish_manager_attempt(&attempt).await;
        Ok(())
    }).await
}

#[tokio::test]
async fn admitted_local_retries_keep_the_frozen_selection_after_models_and_permissions_change()
-> Result<()> {
    let (node, admin, dir) = node()
        .await
        .context("open frozen-selection retry fixture")?;
    let result = async {
        with_agent(&node);
        controlled_pi(&node, &dir, "reply").context("configure frozen-selection Pi fixture")?;
        node.providers.lock().unwrap().switches.execution_models.insert(Provider::Pi, "fake/frozen".into());
        node.settings.lock().unwrap().roots = vec![dir.clone()];
        bot(&node, &admin, "c", "Bot", &[]).await.context("create frozen-selection Bot")?;
        let network = node.network_id().await.context("read frozen-selection network")?;
        let (trigger, attempt) = start_reply(&node, &admin, "m1").await.context("start frozen-selection manager attempt")?;
        let access = node.manager_access_fixture("retry-token", crate::runtime::AgentAccess::ReadOnly { network_id: network.clone(), conversation_id: "c".into() }, &attempt, &trigger, Provider::Codex).await.context("authorize frozen-selection manager attempt")?;
        let args = |call: &str, explicit: bool| -> Result<Value> {
            let cwd = dir.join(call);
            std::fs::create_dir(&cwd)?;
            let mut args = json!({"cwd":cwd.display().to_string(),"call_ref":call,"title":"task","prompt":"controlled work"});
            if explicit { args["agent"] = json!("pi"); }
            Ok(args)
        };
        let mut original = Vec::new();
        for (call, explicit) in [("explicit", true), ("implicit", false), ("provider-default", false)] {
            if call == "provider-default" { node.providers.lock().unwrap().switches.execution_models.remove(&Provider::Pi); }
            let args = args(call, explicit)?;
            let first = node.tool(access.clone(), "track_work", args.clone()).await
                .with_context(|| format!("admit {call} Pi work with its original frozen selection"))?;
            assert_eq!(first["may_send"], false);
            assert_eq!(first["resolved"], json!({"provider":"pi","model":if call == "provider-default" { Value::Null } else { json!("fake/frozen") }}));
            original.push((args, first));
        }
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if {
                    node.store.call("triage.test.work_records", move |store| {
                        store.work_records()
                    }).await.context("read admitted Pi work records for completion")
                }?.iter().all(|run| run.state.finished()) { break Ok::<_, anyhow::Error>(()); }
                tokio::task::yield_now().await;
            }
        }).await.context("wait for all frozen-selection Pi works to finish")??;
        {
            let _controls = node.manager_controls.lock().await;
            let mut providers = node.providers.lock().unwrap();
            providers.switches.execution.clear();
            providers.switches.execution_models.insert(Provider::Pi, "fake/new-default".into());
            node.settings.lock().unwrap().manager = None;
        }
        for (args, first) in &original {
            let retried = node.tool(access.clone(), "track_work", args.clone()).await
                .with_context(|| format!("retry admitted Pi work {} after changing models and permissions", args["call_ref"]))?;
            assert_eq!(retried["resolved"], first["resolved"]);
            assert_eq!(retried["may_send"], false);
            assert_eq!(retried["work_id"], first["work_id"]);
            for (key, value) in [("agent", json!("codex")), ("model", json!("changed")), ("prompt", json!("changed"))] {
                let mut changed = args.clone(); changed[key] = value;
                assert!(node.tool(access.clone(), "track_work", changed).await.is_err());
            }
        }
        assert_eq!(pi_fixture_log(&dir)?.iter().filter(|entry| entry.get("prompt").is_some()).count(), 3);
        let network = network.clone();
        node.store.call("triage.test.dispatch_count_3", move |store| {

            assert_eq!(store.work_records()?.len(), 3);
            assert_eq!(crate::store::context_after(&store.connection, &network, "c", None, &VersionVector::new())?.events.iter().filter(|event| matches!(event.event.body, EventBody::WorkDispatched { .. })).count(), 3);
            Ok(())
        }).await.context("verify frozen-selection work and dispatch counts")?;
        node.finish_manager_attempt(&attempt).await;
        Ok::<_, anyhow::Error>(())
    }.await;
    match (result, cleanup(node, dir).await) {
        (Err(error), Err(cleanup)) => Err(error.context(format!(
            "frozen-selection fixture cleanup also failed: {cleanup:#}"
        ))),
        (Ok(()), result) | (result, Ok(())) => result,
    }
}

#[tokio::test]
async fn thread_dispatch_retries_read_the_durable_work_before_execution_and_native_state()
-> Result<()> {
    use futures_util::{SinkExt as _, StreamExt as _};
    let (node, admin, dir) = node().await?;
    let native_dirs = [
        "explicit",
        "implicit",
        "explicit-default",
        "implicit-default",
    ]
    .into_iter()
    .map(|call| {
        let thread = format!("t-{call}");
        let cwd = dir.join(&thread);
        std::fs::create_dir(&cwd)?;
        Ok((thread, cwd.canonicalize()?.display().to_string()))
    })
    .collect::<Result<BTreeMap<_, _>>>()?;
    let served_dirs = native_dirs.clone();
    let requests = Arc::new(std::sync::Mutex::new(Vec::<Value>::new()));
    let turns = Arc::new(std::sync::Mutex::new(BTreeMap::<String, Value>::new()));
    let mut listener = vibeface_bridge::platform::ipc::Listener::bind(
        &vibeface_bridge::platform::ipc::Endpoint::from_path(&named_socket(&node))?,
    )
    .await?;
    let seen = requests.clone();
    let server = tokio::spawn(async move {
        let mut sessions = tokio::task::JoinSet::new();
        while let Ok(stream) = listener.accept().await {
            let seen = seen.clone();
            let turns = turns.clone();
            let native_dirs = served_dirs.clone();
            sessions.spawn(async move {
                let Ok(mut server) = tokio_tungstenite::accept_async(stream).await else { return; };
                let mut direct = false;
                while let Some(Ok(message)) = server.next().await {
                    let Ok(request) = serde_json::from_slice::<Value>(&message.into_data()) else { continue; };
                    if request["method"] == "initialize" { direct = request["params"]["clientInfo"]["title"] == "Vibeface Agent CLI"; }
                    if direct || request["method"].as_str().is_some_and(|method| method.starts_with("turn/")) { seen.lock().unwrap().push(request.clone()); }
                    let thread = request["params"]["threadId"].as_str().unwrap_or_default();
                    let result = {
                    let mut turns = turns.lock().unwrap();
                    match request["method"].as_str() {
                        Some("initialize") => json!({"userAgent":"codex/fixture"}),
                        Some("thread/read") if thread == "missing" => json!({"thread":null}),
                        Some("thread/read") if thread == "sub" => json!({"thread":{"id":"sub","source":{"subAgent":{}},"parentThreadId":"lead"}}),
                        Some("turn/start") => {
                            let turn = json!({"id":format!("turn-{thread}"),"status":"inProgress","items":[{"type":"userMessage","clientId":request["params"]["clientUserMessageId"]}]});
                            turns.insert(thread.into(), turn.clone());
                            json!({"turn":turn})
                        }
                        Some("turn/steer") => {
                            let turn = turns.get_mut(thread).unwrap();
                            assert_eq!(request["params"]["expectedTurnId"], turn["id"]);
                            turn["items"].as_array_mut().unwrap().push(json!({"type":"userMessage","clientId":request["params"]["clientUserMessageId"]}));
                            json!({"turnId":turn["id"]})
                        }
                        Some("thread/read" | "thread/resume") => json!({"thread":{"id":thread,"cwd":native_dirs.get(thread),"source":"cli","status":{"type":if turns.contains_key(thread) {"active"} else {"idle"}},"turns":turns.get(thread).cloned().into_iter().collect::<Vec<_>>()}}),
                        _ => json!({}),
                    }
                    };
                    if request.get("id").is_some() {
                        let _ = server.send(json!({"id":request["id"],"result":result}).to_string().into()).await;
                    }
                }
            });
        }
    });
    let result = async {
        with_agent(&node);
        node.settings.lock().unwrap().roots = vec![dir.canonicalize()?];
        bot(&node, &admin, "c", "Bot", &[]).await?;
        let network = node.network_id().await?;
        let (trigger, attempt) = start_reply(&node, &admin, "m1").await?;
        let access = node.manager_access_fixture("thread-retry-token", crate::runtime::AgentAccess::ReadOnly { network_id: network.clone(), conversation_id: "c".into() }, &attempt, &trigger, Provider::Codex).await?;
        let mut original = Vec::new();
        for (call, explicit, model) in [("explicit", true, Some("frozen")), ("implicit", false, Some("frozen")), ("explicit-default", true, None), ("implicit-default", false, None)] {
            {
                let mut providers = node.providers.lock().unwrap();
                match model { Some(model) => { providers.switches.execution_models.insert(Provider::Codex, model.into()); }, None => { providers.switches.execution_models.remove(&Provider::Codex); } }
            }
            let mut args = json!({"thread":format!("t-{call}"),"prompt":"\n follow up \nmore","call_ref":call});
            if explicit { args["agent"] = json!("codex"); }
            let first = node.tool(access.clone(), "dispatch_work", args.clone()).await?;
            let expected = json!({"provider":"codex","model":model});
            let work = {
                let network = (network).to_owned();
                node.store.call("triage.test.works", move |store| {
                    store.works(&network, "c")
                }).await
            }?.into_iter().find(|work| first["work_id"].as_str() == Some(work.work_id.as_str())).context("accepted work")?;
            assert_eq!(first["call_record"]["execution"], "started");
            let mut tracked_args = args.clone();
            tracked_args["title"] = json!(work.title);
            tracked_args["cwd"] = json!(work.cwd);
            let tracked = node.tool(access.clone(), "track_work", tracked_args).await?;
            assert_eq!(tracked["may_send"], false);
            assert_eq!(tracked["call_record"]["execution"], "started");
            assert_eq!(work.cwd, native_dirs[&format!("t-{call}")]);
            assert_eq!(work.title, "follow up");
            assert_eq!(json!(work.resolved), expected);
            original.push((args, first["work_id"].clone(), expected));
        }
        let supplement = json!({"agent":"codex","thread":"t-explicit","prompt":"same turn supplement","call_ref":"supplement"});
        let second = node.tool(access.clone(), "dispatch_work", supplement.clone()).await?;
        let second_id = second["work_id"].as_str().context("supplement work")?;
        assert_eq!({
            let network = (network).to_owned();
            let work_id = (second_id).to_owned();
            node.store.call("triage.test.work_native_execution", move |store| {
                store.work_native_execution(&network, &work_id)
            }).await
            }?.context("native permit")?.turn_id, "turn-t-explicit");
        assert_ne!(second["work_id"], original[0].1);
        assert_eq!({
            let work_id = (second_id).to_owned();
            node.store.call("triage.test.work_record", move |store| {
                store.work_record(&work_id)
            }).await
            }?.context("supplement")?.state.handle().context("supplement handle")?.turn_id, "turn-t-explicit");
        let invalid = node.tool(access.clone(), "bind_work", json!({"work_id":second_id,
            "report":vibeface_bridge::codex::StartReport::Turn { turn_id:"another-turn".into() }}))
            .await.expect_err("a mismatched report must remain unknown");
        let record = &invalid.downcast_ref::<crate::tools::ToolFailure>().context("tool failure")?.response["call_record"];
        assert_eq!(record["execution"], "unknown");
        assert_eq!(record["retry"], "none");
        assert!(matches!({
            let work_id = (second_id).to_owned();
            node.store.call("triage.test.work_record", move |store| {
                store.work_record(&work_id)
            }).await
                }?.context("supplement")?.state, crate::work::WorkState::Unverified { .. }));
        original.push((supplement, second["work_id"].clone(), json!({"provider":"codex","model":null})));
        for (thread, reason) in [("missing", "was not returned"), ("sub", "parent thread lead")] {
            let failure = node.tool(access.clone(), "dispatch_work", json!({"thread":thread,"prompt":"p","call_ref":thread})).await.expect_err("invalid native thread must not create work");
            assert!(failure.to_string().contains(reason), "{failure:#}");
            let record = &failure.downcast_ref::<crate::tools::ToolFailure>().context("tool failure")?.response["call_record"];
            assert_eq!(record["execution"], "not_started");
            assert_eq!(record["retry"], "none");
            assert_eq!({
                node.store.call("triage.test.work_records", move |store| {
                    store.work_records()
                }).await
                }?.len(), 5);
        }
        node.signal_work(&network, original[0].1.as_str().context("work id")?, crate::work::Signal::TurnCompleted { turn_id: "turn-t-explicit".into(), status: vibeface_bridge::turn::TurnStatus::Completed, text: "done".into(), error: None }).await?;
        node.signal_work(&network, original[1].1.as_str().context("work id")?, crate::work::Signal::Disconnected).await?;
        assert!(matches!({
            let work_id = (original[0].1.as_str().context("work id")?).to_owned();
            node.store.call("triage.test.work_record", move |store| {
                store.work_record(&work_id)
            }).await
                }?.context("finished")?.state, crate::work::WorkState::Finished { .. }));
        assert!(matches!({
            let work_id = (original[1].1.as_str().context("work id")?).to_owned();
            node.store.call("triage.test.work_record", move |store| {
                store.work_record(&work_id)
            }).await
                }?.context("unknown")?.state, crate::work::WorkState::Unverified { .. }));
        for (index, execution, retry) in [(0, "completed", "none"), (1, "unknown", "lookup_same_ref")] {
            let lookup = node.tool(access.clone(), "call_lookup", json!({"call_ref":original[index].0["call_ref"]})).await?;
            assert_eq!(lookup["call_record"]["execution"], execution);
            assert_eq!(lookup["call_record"]["retry"], retry);
        }
        let status = node.tool(access.clone(), "work_status", json!({"work_id": original[0].1})).await?;
        assert_eq!(status["call_record"]["execution"], "read");
        assert_eq!(status["call_record"]["call_ref"], original[0].0["call_ref"]);
        let stopped = node.tool(access.clone(), "interrupt_work", json!({"work_id":original[0].1})).await?;
        assert_eq!(stopped["call_record"]["execution"], "not_started");
        {
            let _controls = node.manager_controls.lock().await;
            node.providers.lock().unwrap().switches.execution.clear();
            node.providers.lock().unwrap().switches.execution_models.insert(Provider::Codex, "changed-default".into());
            node.settings.lock().unwrap().manager = None;
        }
        let before = requests.lock().unwrap().clone();
        assert_eq!(before.iter().filter(|request| request["method"] == "turn/start").count(), 4);
        assert_eq!(before.iter().filter(|request| request["method"] == "turn/steer").count(), 1);
        for (args, work_id, expected) in &original {
            for (tool, input) in [("dispatch_work", args.clone()), ("tool_call", json!({"tool":"dispatch_work","arguments":args,"call_ref":args["call_ref"]}))] {
                let retried = node.tool(access.clone(), tool, input).await?;
                assert_eq!(&retried["work_id"], work_id);
                assert_eq!(&retried["resolved"], expected);
                assert_eq!(retried["may_send"], false);
            }
            for (field, value) in [("title", json!("changed title")), ("prompt", json!("changed prompt")), ("thread", json!("changed-thread")), ("model", json!("changed model")), ("agent", json!("claude"))] {
                let mut changed = args.clone(); changed[field] = value;
                let failure = node.tool(access.clone(), "dispatch_work", changed).await.expect_err("Store must reject changed payloads");
                assert!(failure.to_string().contains("different request"), "{field}: {failure:#}");
            }
            let mut changed_cwd = args.clone(); changed_cwd["cwd"] = json!("/still-ignored");
            assert_eq!(node.tool(access.clone(), "dispatch_work", changed_cwd).await?["work_id"], *work_id);
        }
        assert_eq!(*requests.lock().unwrap(), before);
        let failure = node.tool(access.clone(), "dispatch_work", json!({"thread":"fresh","prompt":"p","call_ref":"no-ready"})).await.expect_err("ready None must not create work");
        assert!(failure.to_string().contains("no execution agent"), "{failure:#}");
        assert_eq!(*requests.lock().unwrap(), before);
        server.abort();
        let _ = std::fs::remove_file(named_socket(&node));
        node.providers.lock().unwrap().switches.execution.insert(Provider::Codex);
        for (args, work_id, expected) in &original {
            let retried = tokio::time::timeout(Duration::from_secs(2), node.tool(access.clone(), "dispatch_work", args.clone())).await??;
            assert_eq!(&retried["work_id"], work_id);
            assert_eq!(&retried["resolved"], expected);
            assert_eq!(retried["may_send"], false);
        }
        let network = network.clone();
        node.store.call("triage.test.dispatch_count_5", move |store| {

            assert_eq!(store.work_records()?.len(), 5);
            assert_eq!(crate::store::context_after(&store.connection, &network, "c", None, &VersionVector::new())?.events.iter().filter(|event| matches!(event.event.body, EventBody::WorkDispatched { .. })).count(), 5);
            Ok(())
        }).await?;
        let (args, id, _) = &original[0];
        let missing = id.as_str().context("work id")?.to_owned();
        node.store.call("triage.test.projected_without_admission", move |store| {
            assert_eq!(store.connection.execute("DELETE FROM work_runs WHERE work_id=?", [missing])?, 1);
            Ok(())
        }).await?;
        let failure = node.tool(access, "dispatch_work", args.clone()).await.unwrap_err();
        let record = &failure.downcast_ref::<crate::tools::ToolFailure>().context("tool failure")?.response["call_record"];
        assert_eq!(record["execution"], "unknown");
        assert_eq!(record["call_ref"], args["call_ref"]);
        assert_eq!(record["retry"], "lookup_same_ref");
        assert_eq!(*requests.lock().unwrap(), before);
        assert_eq!(node.store.call("triage.test.no_second_admission", |store| Ok(store.work_records()?.len())).await?, 4);
        node.finish_manager_attempt(&attempt).await;
        Ok::<_, anyhow::Error>(())
    }.await;
    server.abort();
    cleanup(node, dir).await?;
    result
}

#[tokio::test]
async fn a_work_runs_on_the_provider_it_names() -> Result<()> {
    with_node(async |node, admin, dir| {
        assert!(std::ptr::addr_eq(
            node.executor(Provider::Codex)?,
            &*node.codex
        ));
        assert!(std::ptr::addr_eq(
            node.executor(Provider::Cursor)?,
            &node.cursor_executor
        ));
        with_agent(&node);
        node.settings.lock().unwrap().roots = vec![dir.canonicalize()?];
        let native_cwd = dir.canonicalize()?.display().to_string();
        let mut native_queries = scripted_app_server(&node, move |request| match request["method"].as_str() {
            Some("thread/read") => json!({"result":{"thread":{"id":request["params"]["threadId"],"cwd":native_cwd}}}),
            _ => json!({"result":{}}),
        }).await?;
        tokio::time::timeout(Duration::from_secs(5), connected(&node)).await?;
        bot(&node, &admin, "c", "前端", &[]).await?;
        let network = node.network_id().await?;
        let (trigger, attempt) = start_reply(&node, &admin, "m1").await?;
        let access = crate::runtime::AgentAccess::ReadOnly { network_id: network.clone(), conversation_id: "c".into() };
        let access = node.manager_access_fixture(
            "token", access.clone(), &attempt, &trigger, crate::providers::Provider::Codex,
        ).await?;
        let send = |call_ref: &str, agent: Value| {
            json!({"thread":"t-other","call_ref":call_ref,"title":"t","prompt":"p","agent":agent})
        };
        let off = node
            .tool(access.clone(), "track_work", send("r1", json!("claude")))
            .await;
        let named = node
            .tool(access.clone(), "track_work", send("r2", json!("codex")))
            .await?;
        let retried = node
            .tool(access.clone(), "track_work", send("r2", json!("codex")))
            .await?;
        complete_tracked_work(&node, &network, named["work_id"].as_str().context("named work")?).await?;
        let only = node
            .tool(access.clone(), "track_work", send("r3", Value::Null))
            .await?;
        complete_tracked_work(&node, &network, only["work_id"].as_str().context("only work")?).await?;
        {
            let mut providers = node.providers.lock().unwrap();
            providers.switches.execution.insert(Provider::Claude);
            (providers.bins).insert(Provider::Claude, "/nonexistent/claude".into());
        }
        // A Codex thread is no session of Claude Code's own works.
        let foreign = node
            .tool(access.clone(), "track_work", send("r4", json!("claude")))
            .await;
        let defaulted = node
            .tool(access.clone(), "track_work", send("r6", Value::Null))
            .await?;
        complete_tracked_work(&node, &network, defaulted["work_id"].as_str().context("defaulted work")?).await?;
        let local = node.identity.device_id()?;
        let origin = main_input_fact(&node, &admin, "c", "m0").await?;
        let earlier = crate::work::WorkSpec {
            work_id: "w-earlier".into(),
            trigger: origin,
            call_ref: "w-earlier".into(),
            thread: Some("t-claude".into()),
            ..provider_work(Provider::Claude, &local, Path::new(""), "p")
        };
        {
            let identity = (node.identity).to_owned();
            let network = (network).to_owned();
            let spec = earlier;
            node.store.call("triage.test.accept_work_fixture", move |store| {
                store.accept_work_fixture(&identity, &network, spec, now_ms())
            }).await
        }?;
        let own = json!({"thread":"t-claude","call_ref":"r5","title":"t","prompt":"p",
            "agent":"claude"});
        let crossed = json!({"thread":"t-claude","call_ref":"r7","title":"t","prompt":"p",
            "agent":"codex"});
        let crossed = node.tool(access.clone(), "track_work", crossed).await;
        let own = node.tool(access, "track_work", own).await;
        node.finish_manager_attempt(&attempt).await;
        let off = off.expect_err("claude executes no work here");
        assert!(format!("{off:#}").contains("turned off"), "{off:#}");
        assert_eq!(named["may_send"], true, "the Codex CLI sends it");
        assert_eq!(retried["may_send"], false, "a retry never sends again");
        assert_eq!(only["may_send"], true, "codex is the only executor");
        let foreign = foreign.expect_err("a session no Claude Code work ran in");
        assert!(
            format!("{foreign:#}").contains("set agent to codex"),
            "{foreign:#}"
        );
        assert_eq!(
            defaulted["may_send"],
            true,
            "with two executors the bot's own provider runs it"
        );
        let on_thread: Vec<_> = {
            let network = (network).to_owned();
            node.store.call("triage.test.works", move |store| {
                store.works(&network, "c")
            }).await
        }?.into_iter().filter(|work| {
            work.thread.as_deref() == Some("t-other") || work.thread_id.as_deref() == Some("t-other")
        }).collect();
        assert_eq!(on_thread.len(), 3, "the refused work left nothing behind");
        assert!(
            (on_thread.iter()).all(|run| run.kind == crate::model::WorkKind::Agent(Provider::Codex))
        );
        let crossed = format!("{:#}", crossed.expect_err("Codex into a Claude Code session"));
        assert!(crossed.contains("set agent to claude"), "{crossed}");
        let own = format!("{:#}", own.expect_err("Claude Code refuses the start"));
        assert!(
            own.contains("native session directory could not be verified") && own.contains("invalid character") && own.contains("new call_ref"),
            "the Agent reads why its start was refused: {own}"
        );
        let refused = crate::work::WorkSpec::id("c", &trigger, "r5");
        let refused = {
            let work_id = (refused).to_owned();
            node.store.call("triage.test.work_record", move |store| {
                store.work_record(&work_id)
            }).await
        }?
            .context("the refused work")?;
        assert_eq!(
            refused.state,
            crate::work::WorkState::Finished {
                outcome: crate::model::WorkOutcome::Rejected
            }
        );
        let native_queries: Vec<_> = std::iter::from_fn(|| native_queries.try_recv().ok()).collect();
        assert!(native_queries.iter().any(|request| request["method"] == "thread/read"
            && request["params"]["threadId"] == "t-other"), "thread inputs verify the native directory before admission");
        Ok::<_, anyhow::Error>(())
    })
    .await
}
