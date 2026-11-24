#![cfg(unix)]

use super::*;
use crate::{
    config::Config,
    crypto::LocalIdentity,
    discovery::Host,
    providers::AppServerMode,
    runtime::test_support::{initialize_node, named_socket, scripted_app_server},
};
use anyhow::Context;
use std::{
    path::{Path, PathBuf},
    sync::Mutex,
};

#[derive(Default)]
struct Wire {
    requests: Vec<Value>,
    turns: Vec<Value>,
    journaled_before_send: Vec<bool>,
    omit_turn_id: bool,
    reject_interrupt: bool,
}

struct Fixture {
    root: PathBuf,
    cwd: PathBuf,
    node: Arc<Runtime>,
    caller: Caller,
    wire: Arc<Mutex<Wire>>,
}

impl Fixture {
    async fn open(can_write: bool) -> Result<Self> {
        // Runtime::open must not discover the operator's provider homes or environment.
        let host = Host::current();
        assert!(host.home.is_none());
        assert!(host.codex_home.is_none());
        assert!(host.claude_config.is_none());
        assert!(host.nvm_dir.is_none());
        assert!(host.path.is_none());
        assert!(host.inherited_env.is_empty());
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../.artifacts")
            .join(format!("vfgw-{}", uuid::Uuid::new_v4().simple()));
        let data = root.join("node");
        let cwd = root.join("workspace");
        std::fs::create_dir_all(&cwd)?;
        let cwd = cwd.canonicalize()?;
        let mut config = Config::local("127.0.0.1:0".parse()?, vec![cwd.clone()], false);
        config.providers.execution = [Provider::Codex].into();
        config.providers.codex_app_server.mode = AppServerMode::System;
        config.providers.codex_app_server.socket = Some(
            std::env::temp_dir().join(format!("vf-gw-{}.sock", uuid::Uuid::new_v4().simple())),
        );
        config.triage.scan_codex_sessions = false;
        initialize_node(&data, &config, &LocalIdentity::generate())?;
        let node = Arc::new(Runtime::open(&data).await?);
        assert!(!node.has_membership().await?);
        assert!(node.manager().is_none());
        let minted = node.gateway.mint(can_write, None, None)?;
        let caller = node
            .gateway
            .authenticate(minted["key"].as_str().context("fixture key")?)?;
        let wire = Arc::new(Mutex::new(Wire::default()));
        let state = wire.clone();
        let weak = Arc::downgrade(&node);
        let owner = caller.clone();
        let _requests = scripted_app_server(&node, move |request| {
            let mut wire = state.lock().unwrap();
            wire.requests.push(request.clone());
            let params = &request["params"];
            match request["method"].as_str() {
                Some("thread/start" | "thread/resume") => {
                    json!({"result":{"thread":{"id":"gateway-thread"}}})
                }
                Some("turn/start") => {
                    let persisted = weak.upgrade().and_then(|node| {
                        node.gateway
                            .get(&owner, params["clientUserMessageId"].as_str()?)
                            .ok()
                    });
                    wire.journaled_before_send
                        .push(persisted.is_some_and(|entry| {
                            entry.thread.as_deref() == Some("gateway-thread")
                                && entry.record.state.thread() == Some("gateway-thread")
                                && matches!(entry.record.state, WorkState::Starting { .. })
                        }));
                    let turn_id = format!("gateway-turn-{}", wire.turns.len() + 1);
                    wire.turns.push(json!({
                        "id":turn_id,"status":"inProgress",
                        "items":[{"type":"userMessage","clientId":params["clientUserMessageId"]}]
                    }));
                    if wire.omit_turn_id {
                        json!({"result":{}})
                    } else {
                        json!({"result":{"turn":{"id":turn_id}}})
                    }
                }
                Some("thread/read") => {
                    json!({"result":{"thread":{"id":"gateway-thread","turns":wire.turns}}})
                }
                Some("turn/steer") => {
                    let turn = wire
                        .turns
                        .iter_mut()
                        .find(|turn| turn["id"] == params["expectedTurnId"])
                        .expect("steer targets a fixture turn");
                    turn["items"].as_array_mut().unwrap().push(json!({
                        "type":"userMessage","clientId":params["clientUserMessageId"]
                    }));
                    json!({"result":{"turnId":turn["id"]}})
                }
                Some("turn/interrupt") if wire.reject_interrupt => {
                    json!({"error":{"code":-32000,"message":"fixture cannot confirm interrupt"}})
                }
                Some("turn/interrupt") => {
                    assert!(wire.turns.iter().any(|turn| turn["id"] == params["turnId"]));
                    json!({"result":{}})
                }
                _ => json!({"error":{"code":-32601,"message":"unexpected fixture request"}}),
            }
        })
        .await?;
        let events = node
            .codex_rx
            .lock()
            .await
            .take()
            .context("fixture Codex events")?;
        node.start_codex(events);
        node.start_gateway_workers()?;
        assert!(node.codex.ready().await, "fixture App Server must connect");
        Ok(Self {
            root,
            cwd,
            node,
            caller,
            wire,
        })
    }

    fn start(&self, reference: &str) -> Value {
        json!({"call_ref":reference,"provider":"codex","cwd":self.cwd,"prompt":"fixture prompt",
            "model":"fixture-model","effort":"high"})
    }

    fn requests(&self, method: &str) -> Vec<Value> {
        self.wire
            .lock()
            .unwrap()
            .requests
            .iter()
            .filter(|request| request["method"] == method)
            .cloned()
            .collect()
    }

    fn complete(&self, index: usize, text: &str) {
        let mut wire = self.wire.lock().unwrap();
        let turn = &mut wire.turns[index];
        turn["status"] = json!("completed");
        turn["items"]
            .as_array_mut()
            .unwrap()
            .push(json!({"type":"agentMessage","text":text}));
    }

    async fn running(&self, id: &str) -> Result<()> {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let entry = self.node.gateway.get(&self.caller, id)?;
                if matches!(entry.record.state, WorkState::Running { .. }) {
                    return Ok(());
                }
                anyhow::ensure!(
                    !entry.record.state.finished(),
                    "fixture work ended before running"
                );
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .context("gateway work did not start")?
    }

    async fn wait(&self, id: &str) -> Result<Value> {
        self.node
            .gateway_call(
                &self.caller,
                "wait",
                json!({"work_id":id,"timeout_ms":5000}),
            )
            .await
    }

    async fn assert_local_only(&self) -> Result<()> {
        self.node
            .store
            .call("gateway.test.no_bot_events", |store| {
                assert!(store.network()?.is_none());
                for table in ["work_runs", "events", "conversations"] {
                    let count: i64 = store.connection.query_row(
                        &format!("SELECT COUNT(*) FROM {table}"),
                        [],
                        |row| row.get(0),
                    )?;
                    assert_eq!(count, 0, "gateway must keep {table} empty");
                }
                Ok(())
            })
            .await
    }

    async fn close(self) -> Result<()> {
        self.node.shutdown().await;
        let socket = named_socket(&self.node);
        match std::fs::remove_file(socket) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        drop(self.node);
        std::fs::remove_dir_all(self.root)?;
        Ok(())
    }
}

fn assert_error(error: anyhow::Error, code: &str) {
    assert_eq!(
        error.downcast_ref::<GatewayError>().map(|error| error.code),
        Some(code)
    );
}

#[tokio::test]
async fn codex_wire_journals_before_send_and_probes_compact_result_without_bot() -> Result<()> {
    let fixture = Fixture::open(false).await?;
    let result = async {
        let request = fixture.start("readonly-start");
        let started = fixture
            .node
            .gateway_call(&fixture.caller, "start", request.clone())
            .await?;
        let id = started["work_id"].as_str().context("work id")?;
        fixture.running(id).await?;
        let threads = fixture.requests("thread/start");
        assert_eq!(threads.len(), 1);
        assert_eq!(threads[0]["params"]["approvalPolicy"], "never");
        assert_eq!(threads[0]["params"]["sandbox"], "read-only");
        assert_eq!(threads[0]["params"]["config"]["web_search"], "disabled");
        let turns = fixture.requests("turn/start");
        assert_eq!(turns.len(), 1);
        let turn = &turns[0]["params"];
        assert_eq!(turn["threadId"], "gateway-thread");
        assert_eq!(turn["clientUserMessageId"], id);
        assert_eq!(turn["approvalPolicy"], "never");
        assert_eq!(
            turn["sandboxPolicy"],
            json!({"type":"readOnly","networkAccess":false})
        );
        assert_eq!(turn["model"], "fixture-model");
        assert_eq!(turn["effort"], "high");
        assert_eq!(turn["cwd"], json!(fixture.cwd));
        assert_eq!(turn["input"][0]["text"], "fixture prompt");
        assert_eq!(
            fixture.wire.lock().unwrap().journaled_before_send,
            vec![true]
        );

        let replay = fixture
            .node
            .gateway_call(&fixture.caller, "start", request.clone())
            .await?;
        assert_eq!(replay["work_id"], id);
        let mut changed = request;
        changed["prompt"] = json!("different prompt");
        assert_error(
            fixture
                .node
                .gateway_call(&fixture.caller, "start", changed)
                .await
                .unwrap_err(),
            "idempotency_conflict",
        );
        assert_eq!(fixture.requests("turn/start").len(), 1);

        {
            let mut wire = fixture.wire.lock().unwrap();
            let items = wire.turns[0]["items"].as_array_mut().unwrap();
            items.push(json!({"type":"fileChange","status":"completed","changes":[{"path":"notes.md"},{"path":"notes.md"}]}));
            items.push(json!({"type":"commandExecution","status":"failed","exitCode":2,"command":"fixture-check","aggregatedOutput":"é".repeat(4000)}));
        }
        fixture.complete(0, "fixture final answer");
        let terminal = fixture.wait(id).await?;
        assert_eq!(terminal["state"], "finished");
        assert_eq!(terminal["outcome"], "completed");
        assert_eq!(terminal["final_reply"], "fixture final answer");
        assert_eq!(terminal["changed_files"], json!(["notes.md"]));
        assert_eq!(terminal["failed_commands"], json!([{"command":"fixture-check","exit_code":2,"output":"é".repeat(1024)}]));
        assert_eq!(terminal["thread"], "gateway-thread");
        for op in ["status", "result"] {
            let value = fixture
                .node
                .gateway_call(&fixture.caller, op, json!({"work_id":id}))
                .await?;
            assert_eq!(value, terminal);
        }
        assert!(!fixture.requests("thread/read").is_empty());
        assert_eq!(fixture.requests("turn/start").len(), 1);
        fixture.assert_local_only().await
    }
    .await;
    fixture.close().await?;
    result
}

#[tokio::test]
async fn codex_write_queue_waits_then_continues_same_thread_and_sandbox() -> Result<()> {
    let fixture = Fixture::open(true).await?;
    let result = async {
        let mut request = fixture.start("write-start");
        request["write"] = json!(true);
        let started = fixture.node.gateway_call(&fixture.caller, "start", request).await?;
        let first = started["work_id"].as_str().context("first work id")?;
        fixture.running(first).await?;
        let queued = fixture.node.gateway_call(&fixture.caller, "queue", json!({
            "work_id":first,"call_ref":"write-followup","prompt":"continue fixture","timeout_ms":5000
        }));
        tokio::pin!(queued);
        assert!(tokio::time::timeout(Duration::from_millis(100), &mut queued).await.is_err());
        assert_eq!(fixture.requests("turn/start").len(), 1);
        assert!(fixture.requests("thread/resume").is_empty());
        fixture.complete(0, "first answer");
        let admitted = tokio::time::timeout(Duration::from_secs(5), &mut queued).await??;
        let second = admitted["work_id"].as_str().context("second work id")?;
        assert_ne!(first, second);
        fixture.running(second).await?;
        let resumed = fixture.requests("thread/resume");
        assert_eq!(resumed.len(), 1);
        assert_eq!(resumed[0]["params"]["threadId"], "gateway-thread");
        assert_eq!(resumed[0]["params"]["approvalPolicy"], "never");
        assert_eq!(resumed[0]["params"]["sandbox"], "workspace-write");
        assert_eq!(fixture.requests("thread/start")[0]["params"]["sandbox"], "workspace-write");
        let turns = fixture.requests("turn/start");
        assert_eq!(turns.len(), 2);
        for (turn, id) in turns.iter().zip([first, second]) {
            assert_eq!(turn["params"]["threadId"], "gateway-thread");
            assert_eq!(turn["params"]["clientUserMessageId"], id);
            assert_eq!(turn["params"]["approvalPolicy"], "never");
            assert_eq!(turn["params"]["sandboxPolicy"], json!({
                "type":"workspaceWrite","writableRoots":[fixture.cwd],"networkAccess":false,
                "excludeTmpdirEnvVar":true,"excludeSlashTmp":true
            }));
            assert_eq!(turn["params"]["effort"], "high");
        }
        assert_eq!(turns[1]["params"]["input"][0]["text"], "continue fixture");
        assert_eq!(fixture.wire.lock().unwrap().journaled_before_send, vec![true, true]);
        fixture.complete(1, "second answer");
        let terminal = fixture.wait(second).await?;
        assert_eq!(terminal["state"], "finished");
        assert_eq!(terminal["final_reply"], "second answer");
        assert_eq!(terminal["write"], true);
        assert_eq!(fixture.node.gateway.get(&fixture.caller, first)?.record.text, "first answer");
        fixture.assert_local_only().await
    }.await;
    fixture.close().await?;
    result
}

#[tokio::test]
async fn codex_controls_use_live_turn_and_reject_missing_or_finished_work() -> Result<()> {
    let fixture = Fixture::open(false).await?;
    let result = async {
        for op in ["steer", "stop"] {
            let mut args = json!({"work_id":"missing"});
            if op == "steer" {
                args["call_ref"] = json!("missing-steer");
                args["prompt"] = json!("hello");
            }
            assert_error(
                fixture
                    .node
                    .gateway_call(&fixture.caller, op, args)
                    .await
                    .unwrap_err(),
                "work_not_found",
            );
        }
        let started = fixture
            .node
            .gateway_call(&fixture.caller, "start", fixture.start("controls"))
            .await?;
        let id = started["work_id"].as_str().context("work id")?;
        fixture.running(id).await?;
        let steer_args = json!({"work_id":id,"call_ref":"steer-once","prompt":"adjust fixture"});
        let accepted = fixture
            .node
            .gateway_call(&fixture.caller, "steer", steer_args.clone())
            .await?;
        assert_eq!(accepted["accepted"], true);
        assert_eq!(accepted["turn_id"], "gateway-turn-1");
        let replay = fixture
            .node
            .gateway_call(&fixture.caller, "steer", steer_args)
            .await?;
        assert_eq!(replay, accepted);
        let steers = fixture.requests("turn/steer");
        assert_eq!(steers.len(), 1);
        assert_eq!(steers[0]["params"]["threadId"], "gateway-thread");
        assert_eq!(steers[0]["params"]["expectedTurnId"], "gateway-turn-1");
        assert!(
            steers[0]["params"]["clientUserMessageId"]
                .as_str()
                .is_some_and(|id| !id.is_empty())
        );
        assert!(
            steers[0]["params"]["input"][0]["text"]
                .as_str()
                .unwrap()
                .contains("adjust fixture")
        );
        let stopped = fixture
            .node
            .gateway_call(&fixture.caller, "stop", json!({"work_id":id}))
            .await?;
        assert_eq!(stopped["state"], "interrupting");
        let interrupts = fixture.requests("turn/interrupt");
        assert_eq!(interrupts.len(), 1);
        assert_eq!(
            interrupts[0]["params"],
            json!({"threadId":"gateway-thread","turnId":"gateway-turn-1"})
        );
        fixture.wire.lock().unwrap().turns[0]["status"] = json!("interrupted");
        let terminal = fixture.wait(id).await?;
        assert_eq!(terminal["state"], "finished");
        assert_eq!(terminal["outcome"], "interrupted");
        for op in ["steer", "stop"] {
            let mut args = json!({"work_id":id});
            if op == "steer" {
                args["call_ref"] = json!("finished-steer");
                args["prompt"] = json!("hello");
            }
            assert_error(
                fixture
                    .node
                    .gateway_call(&fixture.caller, op, args)
                    .await
                    .unwrap_err(),
                "not_running",
            );
        }
        assert_eq!(fixture.requests("turn/start").len(), 1);
        fixture.assert_local_only().await
    }
    .await;
    fixture.close().await?;
    result
}

#[tokio::test]
async fn unconfirmed_start_is_read_back_and_timed_out_queue_never_sends() -> Result<()> {
    let fixture = Fixture::open(false).await?;
    let result = async {
        fixture.wire.lock().unwrap().omit_turn_id = true;
        let request = fixture.start("uncertain");
        let started = fixture
            .node
            .gateway_call(&fixture.caller, "start", request.clone())
            .await?;
        let id = started["work_id"].as_str().context("work id")?;
        fixture.running(id).await?;
        let again = fixture
            .node
            .gateway_call(&fixture.caller, "start", request)
            .await?;
        assert_eq!(again["work_id"], id);
        let wait = fixture
            .node
            .gateway_call(
                &fixture.caller,
                "wait",
                json!({"work_id":id,"timeout_ms":5}),
            )
            .await?;
        assert_eq!(wait["state"], "running");
        let queued = fixture
            .node
            .gateway_call(
                &fixture.caller,
                "queue",
                json!({"work_id":id,"timeout_ms":5,"call_ref":"expired","prompt":"never send"}),
            )
            .await
            .unwrap_err();
        assert_error(queued, "timeout");
        fixture.wire.lock().unwrap().reject_interrupt = true;
        let stop = fixture
            .node
            .gateway_call(&fixture.caller, "stop", json!({"work_id":id}))
            .await
            .unwrap_err();
        assert_error(stop, "submission_unverified");
        fixture.complete(0, "confirmed by native readback");
        let terminal = fixture.wait(id).await?;
        assert_eq!(terminal["outcome"], "completed");
        assert_eq!(terminal["final_reply"], "confirmed by native readback");
        assert_eq!(fixture.requests("turn/start").len(), 1);
        assert!(fixture.requests("thread/resume").is_empty());
        Ok(())
    }
    .await;
    fixture.close().await?;
    result
}
