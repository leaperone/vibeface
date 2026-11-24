use super::*;
use crate::{
    config::Config,
    crypto::LocalIdentity,
    model::{ResolvedModel, WorkKind},
    providers::Provider,
    runtime::test_support::initialize_node,
    work::{Signal, WorkSpec},
};
use vibeface_bridge::turn::TurnStatus;

struct Fixture {
    node: Arc<Runtime>,
    dir: PathBuf,
    origin: String,
    owner: String,
    client: reqwest::Client,
    server: tokio::task::JoinHandle<()>,
}

impl Fixture {
    async fn new(local: bool) -> Result<Self> {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../.artifacts/gateway-mvp/api-tests")
            .join(uuid::Uuid::new_v4().to_string());
        std::fs::create_dir_all(dir.join("allowed"))?;
        std::fs::create_dir_all(dir.join("outside"))?;
        let mut config = Config::local("127.0.0.1:0".parse()?, vec![dir.join("allowed")], false);
        // Disabled executors ensure these protocol tests cannot launch a provider.
        config.providers.execution.clear();
        initialize_node(&dir, &config, &LocalIdentity::generate())?;
        let node = Arc::new(Runtime::open(&dir).await?);
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let origin = format!("http://{}", listener.local_addr()?);
        node.local_access
            .publish_listener(origin.clone(), node.identity.device_id()?)?;
        let launcher: crate::local_access::Launcher =
            serde_json::from_slice(&std::fs::read(dir.join("local-access.json"))?)?;
        let app = super::super::browser_router(node.clone(), vec![Site::parse(&origin)?], local);
        let server = tokio::spawn(async move {
            axum::serve(
                listener,
                app.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .await
            .unwrap();
        });
        Ok(Self {
            node,
            dir,
            origin,
            owner: launcher.capability,
            client: reqwest::Client::builder().no_proxy().build()?,
            server,
        })
    }

    fn request(&self, method: Method, path: &str, key: Option<&str>) -> reqwest::RequestBuilder {
        let request = self
            .client
            .request(method, format!("{}{path}", self.origin));
        match key {
            Some(key) => request.bearer_auth(key),
            None => request,
        }
    }

    async fn post(&self, op: &str, key: Option<&str>, input: Value) -> Result<(StatusCode, Value)> {
        let response = self
            .request(Method::POST, &format!("{PREFIX}/{op}"), key)
            .json(&input)
            .send()
            .await?;
        Ok((response.status(), response.json().await?))
    }

    async fn caller(&self) -> Result<String> {
        let (status, minted) = self.post("callers", Some(&self.owner), json!({})).await?;
        assert_eq!(status, StatusCode::CREATED);
        assert!(minted["caller_id"].as_str().is_some());
        Ok(minted["key"].as_str().context("caller key")?.to_owned())
    }

    async fn rpc(&self, key: Option<&str>, input: Value) -> Result<(StatusCode, Value)> {
        let response = self
            .request(Method::POST, "/api/gateway/mcp", key)
            .json(&input)
            .send()
            .await?;
        Ok((response.status(), response.json().await?))
    }

    fn start(&self) -> Value {
        json!({"call_ref":"fixture","provider":"codex","cwd":self.dir.join("allowed"),"prompt":"hello","title":"fixture"})
    }

    fn completed(&self, key: &str) -> Result<String> {
        let caller = self.node.gateway.authenticate(key)?;
        let conversation = format!("gateway:{}", caller.id);
        let target = self.node.identity.device_id()?;
        let spec = WorkSpec {
            work_id: WorkSpec::id(&conversation, "gateway", "fixture"),
            conversation,
            trigger: "gateway".into(),
            target: target.clone(),
            dispatcher: target,
            call_ref: "fixture".into(),
            kind: WorkKind::Agent(Provider::Codex),
            model: None,
            resolved: ResolvedModel {
                provider: Provider::Codex,
                model: None,
            },
            cwd: self
                .dir
                .join("allowed")
                .canonicalize()?
                .to_string_lossy()
                .into_owned(),
            title: "fixture".into(),
            prompt: "hello".into(),
            continues: None,
            parent: None,
            thread: None,
        };
        self.node.gateway.accept(&caller, &spec, false, None)?;
        self.node.gateway.signal(
            &spec.work_id,
            Signal::ThreadCreated("fixture-thread".into()),
        )?;
        self.node.gateway.signal(
            &spec.work_id,
            Signal::TurnStarted {
                turn_id: "fixture-turn".into(),
                epoch: 1,
            },
        )?;
        self.node.gateway.signal(
            &spec.work_id,
            Signal::TurnCompleted {
                turn_id: "fixture-turn".into(),
                status: TurnStatus::Completed,
                text: "fixture complete".into(),
                error: None,
            },
        )?;
        Ok(spec.work_id)
    }

    async fn close(self) -> Result<()> {
        self.server.abort();
        let _ = self.server.await;
        self.node.shutdown().await;
        drop(self.node);
        std::fs::remove_dir_all(self.dir)?;
        Ok(())
    }
}

#[tokio::test]
async fn owner_mints_caller_and_all_protocols_require_a_caller_key() -> Result<()> {
    let f = Fixture::new(true).await?;
    let result = async {
        for key in [None, Some("wrong-key"), Some(f.owner.as_str())] {
            let response = f
                .request(Method::GET, &format!("{PREFIX}/capabilities"), key)
                .send()
                .await?;
            assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
            assert_eq!(response.json::<Value>().await?["code"], "unauthorized");
            for method in ["initialize", "tools/list", "tools/call"] {
                let (status, value) = f
                    .rpc(key, json!({"jsonrpc":"2.0","id":1,"method":method}))
                    .await?;
                assert_eq!(status, StatusCode::UNAUTHORIZED);
                assert_eq!(value["code"], "unauthorized");
            }
        }
        let key = f.caller().await?;
        for token in [None, Some("wrong-key"), Some(key.as_str())] {
            let (status, value) = f.post("callers", token, json!({"can_write":true})).await?;
            assert_eq!(status, StatusCode::FORBIDDEN);
            assert_eq!(value["code"], "owner_required");
        }
        let response = f
            .request(Method::GET, &format!("{PREFIX}/capabilities"), Some(&key))
            .send()
            .await?;
        assert_eq!(response.status(), StatusCode::OK);
        let manifest: Value = response.json().await?;
        assert_eq!(manifest["local_only"], true);
        assert_eq!(manifest["default_sandbox"], "read-only");
        assert_eq!(manifest["can_write"], false);
        for provider in ["codex", "claude"] {
            for operation in &OPERATIONS[1..] {
                assert!(manifest["providers"][provider]["operations"][*operation].is_boolean());
            }
            assert_eq!(
                manifest["providers"][provider]["sandbox_modes"],
                json!(["read-only", "danger-full-access"])
            );
            for field in ["images", "effort", "model"] {
                assert!(manifest["providers"][provider][field].is_boolean());
            }
        }
        let (status, value) = f
            .post("callers", Some(&f.owner), json!({"can_write":"yes"}))
            .await?;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(value["code"], "invalid_request");
        let writable = f
            .post("callers", Some(&f.owner), json!({"can_write":true}))
            .await?
            .1;
        let caller = f
            .node
            .gateway
            .authenticate(writable["key"].as_str().unwrap())?;
        assert!(caller.can_write);
        Ok::<_, anyhow::Error>(())
    }
    .await;
    f.close().await?;
    result
}

#[tokio::test]
async fn owner_mint_is_unavailable_on_the_public_listener() -> Result<()> {
    let f = Fixture::new(false).await?;
    let result = f.post("callers", Some(&f.owner), json!({})).await;
    f.close().await?;
    let (status, value) = result?;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(value["code"], "owner_required");
    Ok(())
}

#[tokio::test]
async fn rest_rejects_unsafe_starts_and_malformed_requests_with_stable_errors() -> Result<()> {
    let f = Fixture::new(true).await?;
    let result = async {
        let key = f.caller().await?;
        let mut cases = Vec::new();
        let mut outside = f.start();
        outside["cwd"] = json!(f.dir.join("outside"));
        cases.push((outside, "cwd_not_allowed", StatusCode::FORBIDDEN));
        let mut write = f.start();
        write["write"] = json!(true);
        cases.push((write, "write_forbidden", StatusCode::FORBIDDEN));
        for provider in ["pi", "opencode", "cursor", "unknown"] {
            let mut input = f.start();
            input["provider"] = json!(provider);
            cases.push((input, "unsupported_provider", StatusCode::BAD_REQUEST));
        }
        for target in ["node", "target", "target_node"] {
            let mut input = f.start();
            input[target] = json!("another-node");
            cases.push((input, "cross_node_deferred", StatusCode::BAD_REQUEST));
        }
        for (input, code, expected) in cases {
            let (status, value) = f.post("start", Some(&key), input).await?;
            assert_eq!(status, expected, "{value}");
            assert_eq!(value["code"], code);
            assert!(value["message"].is_string());
        }
        for path in [format!("{PREFIX}/start"), "/api/gateway/mcp".into()] {
            let response = f
                .request(Method::POST, &path, Some(&key))
                .header(header::CONTENT_TYPE, "application/json")
                .body("{")
                .send()
                .await?;
            assert_eq!(response.status(), StatusCode::BAD_REQUEST);
            assert_eq!(response.json::<Value>().await?["code"], "invalid_request");
        }
        for input in [
            json!({}),
            json!({"work_id":"missing","timeout_ms":0}),
            json!({"work_id":"missing","timeout_ms":120001}),
        ] {
            let (status, value) = f.post("wait", Some(&key), input).await?;
            assert_eq!(status, StatusCode::BAD_REQUEST);
            assert_eq!(value["code"], "invalid_request");
        }
        let response = f
            .request(Method::POST, &format!("{PREFIX}/start"), Some(&key))
            .header(header::CONTENT_TYPE, "text/plain")
            .body("{}")
            .send()
            .await?;
        assert_eq!(response.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);
        assert_eq!(response.json::<Value>().await?["code"], "invalid_request");
        Ok::<_, anyhow::Error>(())
    }
    .await;
    f.close().await?;
    result
}

#[tokio::test]
async fn completed_fixture_has_compact_results_idempotent_start_and_clear_control_errors()
-> Result<()> {
    let f = Fixture::new(true).await?;
    let result = async {
        let key = f.caller().await?;
        let work = f.completed(&key)?;
        for operation in ["start", "status", "result", "wait"] {
            let input = match operation {
                "start" => f.start(),
                "wait" => json!({"work_id":work,"timeout_ms":1}),
                _ => json!({"work_id":work}),
            };
            let (status, value) = f.post(operation, Some(&key), input).await?;
            assert_eq!(status, StatusCode::OK, "{value}");
            assert_eq!(value["work_id"], work);
            assert_eq!(value["state"], "finished");
            assert_eq!(value["outcome"], "completed");
            assert_eq!(value["final_reply"], "fixture complete");
            assert_eq!(value["sandbox"], "read-only");
            assert_eq!(value["changed_files"], json!([]));
            assert_eq!(value["failed_commands"], json!([]));
        }
        let mut changed = f.start();
        changed["prompt"] = json!("changed");
        let (status, value) = f.post("start", Some(&key), changed).await?;
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(value["code"], "idempotency_conflict");
        for operation in ["stop", "steer"] {
            for (id, code, expected) in [
                ("missing", "work_not_found", StatusCode::NOT_FOUND),
                (work.as_str(), "not_running", StatusCode::CONFLICT),
            ] {
                let input = if operation == "steer" {
                    json!({"work_id":id,"call_ref":"control","prompt":"continue"})
                } else {
                    json!({"work_id":id})
                };
                let (status, value) = f.post(operation, Some(&key), input).await?;
                assert_eq!(status, expected, "{value}");
                assert_eq!(value["code"], code);
            }
        }
        let other = f.caller().await?;
        let (status, value) = f
            .post("result", Some(&other), json!({"work_id":work}))
            .await?;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(value["code"], "work_not_found");
        Ok::<_, anyhow::Error>(())
    }
    .await;
    f.close().await?;
    result
}

#[tokio::test]
async fn mcp_initializes_lists_schemas_and_calls_the_same_gateway_operations() -> Result<()> {
    let f = Fixture::new(true).await?;
    let result = async {
        let key = f.caller().await?;
        let (status, init) = f.rpc(Some(&key), json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25"}})).await?;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(init["result"]["capabilities"]["tools"]["listChanged"], false);
        let (_, list) = f.rpc(Some(&key), json!({"jsonrpc":"2.0","id":2,"method":"tools/list"})).await?;
        let tools = list["result"]["tools"].as_array().context("tools")?;
        assert_eq!(tools.len(), OPERATIONS.len());
        for operation in OPERATIONS {
            let tool = tools.iter().find(|tool| tool["name"] == operation).context("tool")?;
            assert!(tool["description"].as_str().unwrap().contains("capabilities"));
            assert_eq!(tool["inputSchema"]["type"], "object");
        }
        for (name, args, code) in [("capabilities", json!({}), None), ("start", json!({"call_ref":"rejected","provider":"cursor","cwd":"/unused","prompt":"hello"}), Some("unsupported_provider"))] {
            let (_, response) = f.rpc(Some(&key), json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":name,"arguments":args}})).await?;
            assert_eq!(response["result"]["isError"], code.is_some());
            let value = &response["result"]["structuredContent"];
            let text: Value = serde_json::from_str(response["result"]["content"][0]["text"].as_str().context("MCP text")?)?;
            assert_eq!(&text, value);
            if let Some(code) = code { assert_eq!(value["code"], code); } else { assert_eq!(value["local_only"], true); }
        }
        let (_, invalid) = f.rpc(Some(&key), json!({"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"start","arguments":[]}})).await?;
        assert_eq!(invalid["error"]["code"], -32602);
        let response = f.request(Method::POST, "/api/gateway/mcp", Some(&key)).json(&json!({"jsonrpc":"2.0","method":"notifications/initialized"})).send().await?;
        assert_eq!(response.status(), StatusCode::ACCEPTED);
        Ok::<_,anyhow::Error>(())
    }.await;
    f.close().await?;
    result
}
