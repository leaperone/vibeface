use super::{
    journal,
    local::BootstrapCapability,
    policy::{FreshnessProof, project_policy_in},
};
use crate::{crypto::LocalIdentity, model::*, store::Store};
use anyhow::{Context, Result};
use rusqlite::{OptionalExtension, TransactionBehavior, params};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use vibeface_secure::{
    Identity,
    authorization::{
        bootstrap::{
            self,
            recovery::{self, node_enrollment},
        },
        *,
    },
};

pub(crate) fn now() -> u64 {
    static START: std::sync::LazyLock<u64> =
        std::sync::LazyLock::new(|| crate::runtime::now_ms() as u64);
    *START
}
const CERT_END: u64 = 10_000_000;

fn test_directory() -> PathBuf {
    std::env::var_os("VIBEFACE_TEST_ARTIFACT_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../.artifacts/store-tests")
        })
        .join(uuid::Uuid::new_v4().to_string())
}

pub(crate) struct TestNetwork {
    pub created: bootstrap::CreatedAccount,
    pub root: Identity,
    pub recovery: Identity,
    pub client: Identity,
    pub node: LocalIdentity,
    pub account: AccountId,
    pub space: SpaceId,
    pub resource: ResourceId,
    pub bundle: ClientAuthorizationBundle,
    pub pins: Vec<PolicyPin>,
    pub directory: PathBuf,
    pub project: PathBuf,
}

impl Drop for TestNetwork {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.directory);
    }
}

impl TestNetwork {
    pub fn new() -> Result<Self> {
        Self::with_node(LocalIdentity::generate())
    }

    pub fn with_node(node: LocalIdentity) -> Result<Self> {
        let root = Identity::generate().map_err(anyhow::Error::msg)?;
        let recovery = Identity::generate().map_err(anyhow::Error::msg)?;
        let client = Identity::generate().map_err(anyhow::Error::msg)?;
        let node_key =
            Identity::from_secret_hex(&node.secret_key_hex).map_err(anyhow::Error::msg)?;
        let nonce = Nonce::random()?;
        let claim = bootstrap::create_node_claim(
            &node_key,
            nonce.as_str(),
            now() as f64,
            (now() + 100_000) as f64,
        )
        .map_err(anyhow::Error::msg)?;
        let verified = bootstrap::verify_node_claim(
            &claim,
            &node_key.public_key_hex(),
            nonce.as_str(),
            now() as f64,
        )
        .map_err(anyhow::Error::msg)?;
        let prepared = bootstrap::prepare_account_creation(
            &root,
            &recovery,
            &client,
            &verified,
            (now() + CERT_END) as f64,
            now() as f64,
        )
        .map_err(anyhow::Error::msg)?;
        let possession = bootstrap::sign_node_possession(
            &prepared.node_request_json().map_err(anyhow::Error::msg)?,
            &node_key,
            nonce.as_str(),
            now() as f64,
        )
        .map_err(anyhow::Error::msg)?;
        let created =
            bootstrap::finish_account_creation(&prepared, &root, &possession, now() as f64)
                .map_err(anyhow::Error::msg)?;
        let bundle: ClientAuthorizationBundle =
            serde_json::from_str(&created.bundle_json().map_err(anyhow::Error::msg)?)?;
        let pins = serde_json::from_str(&created.pins_json().map_err(anyhow::Error::msg)?)?;
        let scope: ClientScope =
            serde_json::from_str(&created.scope_json().map_err(anyhow::Error::msg)?)?;
        let resource = bundle
            .journal
            .iter()
            .find_map(|entry| match entry {
                JournalEntry::Resource(record) => Some(record.payload.resource.clone()),
                _ => None,
            })
            .context("bootstrap project missing")?;
        let directory = test_directory();
        crate::config::private_dir(&directory)?;
        let directory = directory.canonicalize()?;
        crate::config::private_dir(&directory.join("project"))?;
        let project = directory.join("project").canonicalize()?;
        Ok(Self {
            created,
            root,
            recovery,
            client,
            node,
            account: scope.account_id,
            space: scope.space_id,
            resource,
            bundle,
            pins,
            directory,
            project,
        })
    }

    pub fn same_account_node(&self) -> Result<Self> {
        let directory = test_directory();
        crate::config::private_dir(&directory)?;
        let directory = directory.canonicalize()?;
        crate::config::private_dir(&directory.join("project"))?;
        let project = directory.join("project").canonicalize()?;
        let mut network = Self {
            created: self.created.clone(),
            root: Identity::from_secret_hex(&self.root.secret_hex()).map_err(anyhow::Error::msg)?,
            recovery: Identity::from_secret_hex(&self.recovery.secret_hex())
                .map_err(anyhow::Error::msg)?,
            client: Identity::from_secret_hex(&self.client.secret_hex())
                .map_err(anyhow::Error::msg)?,
            node: self.node.clone(),
            account: self.account.clone(),
            space: self.space.clone(),
            resource: self.resource.clone(),
            bundle: self.bundle.clone(),
            pins: self.pins.clone(),
            directory,
            project,
        };
        let node = LocalIdentity::generate();
        network.enroll_node(node.clone())?;
        assert_ne!(node.public_key_hex()?, self.node.public_key_hex()?);
        network.node = node;
        Ok(network)
    }

    pub fn enroll_node(&mut self, identity: LocalIdentity) -> Result<()> {
        fn carrier<T>(action: impl FnOnce() -> Result<T>) -> Result<T> {
            let verification =
                vibeface_secure::signature_verification::SignatureVerification::new();
            let result = verification.run(action)?;
            anyhow::ensure!(
                verification.counts().actual_verifications <= 32,
                "Node enrollment fixture carrier exceeded its signature budget"
            );
            Ok(result)
        }
        let source_path = self.directory.join("node-enrollment-authority.sqlite3");
        let mut authority = if source_path.exists() {
            Store::open(&source_path)?
        } else {
            self.at(&source_path)?
        };
        let target_path = self.directory.join(format!(
            "node-enrollment-target-{}.sqlite3",
            identity.public_key_hex()?
        ));
        let mut fresh = Store::initialize(&target_path)?;
        let capability = BootstrapCapability::generate();
        fresh.install_bootstrap_capability(&capability)?;
        let signer =
            Identity::from_secret_hex(&identity.secret_key_hex).map_err(anyhow::Error::msg)?;
        let claim_json = carrier(|| {
            bootstrap::create_node_claim(
                &signer,
                &capability.capability,
                now() as f64,
                (now() + 100_000) as f64,
            )
            .map_err(anyhow::Error::msg)
        })?;
        fresh.connection.execute(
            "INSERT INTO identity_meta(key,value) VALUES('node_claim',?)",
            [&claim_json],
        )?;
        let claim = carrier(|| {
            bootstrap::verify_node_claim(
                &claim_json,
                &identity.public_key_hex()?,
                &capability.capability,
                now() as f64,
            )
            .map_err(anyhow::Error::msg)
        })?;
        let source_key = PublicKey::parse(&self.node.public_key_hex()?)?;
        let authorization = super::client_policy_read::verified_client_policy_for_test(
            &mut authority,
            &self.node,
            &self.client,
            &self.account,
            AuthorizationSelection {
                resource: ResourceRef {
                    space: self.space.clone(),
                    resource: self.resource.clone(),
                },
                audience: Audience {
                    service: ServiceId::from_key(&source_key),
                    device: DeviceId::from_key(&source_key),
                },
                grant_leaf: self.bundle.grant_leaf.clone(),
            },
            &self.pins,
            now(),
        )?;
        let material = carrier(|| {
            recovery::build_account_recovery_material(
                &self.created,
                &self.root,
                &self.recovery,
                "https://authority.example",
                now() as f64,
            )
            .map_err(anyhow::Error::msg)
        })?;
        let prepared = carrier(|| {
            node_enrollment::prepare_account_node_enrollment(
                &material,
                &authorization,
                &self.client,
                &claim,
                (now() + CERT_END) as f64,
                now() as f64,
            )
            .map_err(anyhow::Error::msg)
        })?;
        let principal = self.client.public_key_hex();
        let mut begin: Value =
            serde_json::from_str(&prepared.request_json().map_err(anyhow::Error::msg)?)?;
        let response = carrier(|| {
            authority.begin_enrollment_policy_read(&self.node, &principal, &begin, now())
        })?;
        begin["snapshot"] = response["snapshot"].clone();
        let mut response = carrier(|| {
            fresh.node_enrollment_command(
                &identity,
                &principal,
                "account_node_import_begin",
                &begin,
                now(),
            )
        })?;
        let context = response["context"].clone();
        loop {
            let page = carrier(|| {
                authority.enrollment_policy_page(
                    &self.node,
                    &principal,
                    &response["page_request"],
                    now(),
                )
            })?;
            response = carrier(|| {
                fresh.node_enrollment_command(
                    &identity,
                    &principal,
                    "account_node_import_page",
                    &json!({"context":context,"page":page["page"]}),
                    now(),
                )
            })?;
            if page["page"]["payload"]["complete"] == true {
                break;
            }
        }
        let witness = carrier(|| {
            authority.enrollment_policy_witness(
                &self.node,
                &principal,
                &response["page_request"],
                now(),
            )
        })?;
        let proof = carrier(|| {
            fresh.node_enrollment_command(
                &identity,
                &principal,
                "account_node_import_finish",
                &json!({"context":context,"witness":witness["witness"]}),
                now(),
            )
        })?;
        let prepared = carrier(|| {
            node_enrollment::prepare_account_node_enrollment_commit(
                &prepared,
                &material,
                proof["proof"]
                    .as_str()
                    .context("Node possession proof missing")?,
                now() as f64,
            )
            .map_err(anyhow::Error::msg)
        })?;
        let response = carrier(|| {
            authority.node_enrollment_command(
                &self.node,
                &principal,
                "account_node_commit",
                &serde_json::from_str(&prepared.request_json().map_err(anyhow::Error::msg)?)?,
                now(),
            )
        })?;
        let finished = carrier(|| {
            node_enrollment::finish_account_node_enrollment(
                &prepared,
                &serde_json::to_string(&response["journal_delta"])?,
                now() as f64,
            )
            .map_err(anyhow::Error::msg)
        })?;
        let installation: Value =
            serde_json::from_str(&finished.installation_json().map_err(anyhow::Error::msg)?)?;
        let installed = carrier(|| {
            fresh.node_enrollment_command(
                &identity,
                &principal,
                "account_node_install",
                &json!({"context":context,"installation":installation}),
                now(),
            )
        })?;
        assert_eq!(installed["account_id"], self.account.as_str());
        assert_eq!(installed["node_public_key"], identity.public_key_hex()?);
        assert!(
            fresh
                .verify_bootstrap_capability(&capability.capability)
                .is_err()
        );
        self.bundle
            .journal
            .extend(serde_json::from_value::<Vec<JournalEntry>>(
                response["journal_delta"].clone(),
            )?);
        self.pins = finished.pins().to_vec();
        fresh.save_local_device(self.space.as_str(), &identity)?;
        fresh.local_set(
            self.space.as_str(),
            &format!("resource-root:{}", self.resource),
            &json!(self.project),
        )?;
        Ok(())
    }

    pub fn install(&self, store: &mut Store) -> Result<()> {
        let update = store
            .policy
            .prepare_bootstrap(&self.pins, &self.bundle.journal, now())?;
        let tx = store
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let policy = journal::commit_in(&tx, &update)?;
        project_policy_in(&tx, &policy)?;
        tx.commit()?;
        store.policy = policy;
        store.save_local_device(self.space.as_str(), &self.node)?;
        store.local_set(
            self.space.as_str(),
            &format!("resource-root:{}", self.resource),
            &json!(self.project),
        )?;
        Ok(())
    }

    pub fn memory(&self) -> Result<Store> {
        self.at(Path::new(":memory:"))
    }

    pub fn at(&self, path: &Path) -> Result<Store> {
        let mut store = Store::initialize(path)?;
        self.install(&mut store)?;
        Ok(store)
    }

    pub fn body(&self, method: &str, scope: &str) -> Value {
        json!({"method":method, "space_id":self.space, "resource_id":self.resource,
            "conversation_id":scope, "authorization_id":uuid::Uuid::new_v4().to_string()})
    }

    pub fn authorization(
        &self,
        store: &Store,
        body: &Value,
        now: u64,
    ) -> Result<(VerifiedAuthorization, TransportEvidence)> {
        let key = PublicKey::parse(&self.node.public_key_hex()?)?;
        let audience = Audience {
            service: ServiceId::from_key(&key),
            device: DeviceId::from_key(&key),
        };
        let facts = store.request_facts(body, &audience)?;
        let device = DeviceId::from_key(&PublicKey::of(&self.client));
        let expected_policies = vec![
            store
                .policy
                .verified()
                .head(&PolicyDomain::Account(self.account.clone()))
                .context("account head missing")?
                .clone(),
            store
                .policy
                .verified()
                .head(&PolicyDomain::Space(self.space.clone()))
                .context("space head missing")?
                .clone(),
        ];
        let invocation = Signed::sign(
            Invocation {
                version: AUTH_VERSION,
                actor: Actor::Account(self.account.clone()),
                device: device.clone(),
                initiator: Actor::Account(self.account.clone()),
                initiating_grant: None,
                resource: facts.resource.clone(),
                action: facts.action,
                audience,
                presenter: device.clone(),
                operation_id: facts.operation_id.clone(),
                body_hash: facts.body_hash.clone(),
                grant_leaf: self.bundle.grant_leaf.clone(),
                expected_policies,
                context: facts.context.clone(),
                nonce: Nonce::random()?,
                not_before_ms: now,
                expires_at_ms: now + 100_000,
            },
            &self.client,
        )?;
        let stored: Option<(String, String)> = store.connection.query_row(
            "SELECT signed_invocation,source_body FROM accepted_authorizations WHERE space_id=? AND authorization_id=?",
            params![self.space.as_str(),facts.operation_id], |row| Ok((row.get(0)?,row.get(1)?)),
        ).optional()?;
        let invocation = match stored {
            Some((invocation, original)) if serde_json::from_str::<Value>(&original)? == *body => {
                serde_json::from_str(&invocation)?
            }
            _ => invocation,
        };
        let transport = TransportEvidence::DirectNoise { device };
        Ok((
            store.authorize_request(&invocation, &facts, &transport, now)?,
            transport,
        ))
    }

    pub fn freshness(
        &self,
        store: &mut Store,
        auth: &VerifiedAuthorization,
        stage: CheckStage,
        now: u64,
    ) -> Result<FreshnessProof> {
        let challenge = store.issue_freshness(auth, stage, now)?;
        let receipts = store.sign_current_receipts(&challenge, &self.node, now)?;
        Ok(FreshnessProof {
            challenge,
            receipts,
        })
    }

    pub fn append(
        &self,
        store: &mut Store,
        body: &Value,
        event: EventBody,
        now: u64,
    ) -> Result<SignedEvent> {
        let (auth, transport) = self.authorization(store, body, now)?;
        let proof = self.freshness(store, &auth, CheckStage::Admit, now)?;
        store.append_authorized_event(
            &self.node,
            &auth,
            &proof,
            &transport,
            body,
            body["conversation_id"]
                .as_str()
                .context("conversation required")?,
            event,
            vec![],
            now as i64,
        )
    }

    pub fn conversation(&self, store: &mut Store, scope: &str) -> Result<SignedEvent> {
        let mut body = self.body("create_conversation", scope);
        body["title"] = json!("work");
        self.append(
            store,
            &body,
            EventBody::ConversationCreated {
                title: "work".into(),
            },
            now(),
        )
    }

    pub fn message(&self, store: &mut Store, scope: &str, text: &str) -> Result<SignedEvent> {
        let (body, event) = self.message_body(scope, text)?;
        self.append(store, &body, event, now())
    }

    pub fn message_body(&self, scope: &str, text: &str) -> Result<(Value, EventBody)> {
        let mut body = self.body("message", scope);
        body["message_id"] = json!(uuid::Uuid::new_v4().to_string());
        body["text"] = json!(text);
        body["scheduler_node"] = json!(self.node.node_id()?);
        Ok((
            body.clone(),
            EventBody::MessagePosted {
                message_id: body["message_id"].as_str().unwrap().into(),
                role: "user".into(),
                text: text.into(),
                scheduler_node: Some(self.node.node_id()?),
            },
        ))
    }

    pub fn signed_message(
        &self,
        store: &mut Store,
        text: &str,
        sequence: u64,
        previous: Option<String>,
        parents: Vec<String>,
    ) -> Result<SignedEvent> {
        let (body, event) = self.message_body("conversation", text)?;
        let (auth, transport) = self.authorization(store, &body, now())?;
        let proof = self.freshness(store, &auth, CheckStage::Admit, now())?;
        let tx = store
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let admission = super::admission::consume_authorization_in(
            &tx,
            &store.policy,
            &auth,
            &proof,
            &transport,
            &body,
            None,
            now(),
        )?;
        tx.commit()?;
        self.node.sign_event(Event {
            version: WIRE_VERSION,
            network_id: self.space.to_string(),
            resource_id: self.resource.clone(),
            cause: EventCause::Invocation { admission },
            scope_id: "conversation".into(),
            author: self.node.node_id()?,
            sequence,
            previous,
            parents,
            created_at_ms: now() as i64,
            body: event,
            blobs: vec![],
        })
    }

    pub fn task(&self, store: &mut Store, scope: &str, id: &str) -> Result<SignedEvent> {
        let mut body = self.body("create_task", scope);
        body["task_id"] = json!(id);
        body["title"] = json!(id);
        body["goal"] = json!("work");
        body["acceptance"] = json!("review result");
        body["auto_followup"] = json!(false);
        self.append(
            store,
            &body,
            EventBody::TaskCreated {
                task_id: id.into(),
                title: id.into(),
                goal: "work".into(),
                acceptance: "review result".into(),
            },
            now(),
        )
    }

    pub fn request(&self, store: &mut Store, id: &str, session: &str) -> Result<OperationRequest> {
        self.request_at(store, id, session, &self.project)
    }

    pub fn request_at(
        &self,
        store: &mut Store,
        id: &str,
        session: &str,
        project: &Path,
    ) -> Result<OperationRequest> {
        let task_id = format!("task-{session}");
        let creation = match store.task_creation(self.space.as_str(), "conversation", &task_id) {
            Ok(event) => event,
            Err(_) => self.task(store, "conversation", &task_id)?,
        };
        let binding =
            if let Some(binding) = store.task_binding(self.space.as_str(), &creation.hash)? {
                binding
            } else {
                self.bind(store, &creation, &task_id, session, project)?
            };
        Ok(OperationRequest {
            operation_id: id.into(),
            network_id: self.space.to_string(),
            conversation_id: "conversation".into(),
            task_id: Some(task_id),
            target_node: self.node.node_id()?,
            session_id: session.into(),
            expected_revision: 0,
            basis_heads: store.heads(self.space.as_str(), "conversation")?,
            action: binding.binding.open_spec.clone(),
            resources: vec![
                project
                    .to_str()
                    .context("project path is not UTF-8")?
                    .into(),
            ],
            binding: Some(binding),
        })
    }

    pub fn bind(
        &self,
        store: &mut Store,
        creation: &SignedEvent,
        task_id: &str,
        session: &str,
        project: &Path,
    ) -> Result<crate::binding::SignedBinding> {
        let event: Event = serde_json::from_str(&creation.canonical_body)?;
        let mut body = self.body("bind_execution", &event.scope_id);
        body["task_id"] = json!(task_id);
        body["task_ref"] = json!(creation.hash);
        body["target_node"] = json!(self.node.node_id()?);
        body["session_id"] = json!(session);
        body["executor"] = json!("codex");
        body["cwd"] = json!(project);
        let (auth, transport) = self.authorization(store, &body, now())?;
        let proof = self.freshness(store, &auth, CheckStage::Admit, now())?;
        store.bind_task_authorized(&self.node, &auth, &proof, &transport, &body, now())
    }

    pub fn admit(&self, store: &mut Store, request: &OperationRequest) -> Result<OperationRecord> {
        let mut body = self.body("operate", &request.conversation_id);
        body["authorization_id"] = json!(request.operation_id);
        body["request"] = serde_json::to_value(request)?;
        let (auth, transport) = self.authorization(store, &body, now())?;
        let proof = self.freshness(store, &auth, CheckStage::Admit, now())?;
        store.accept_authorized_operation(
            &self.node,
            &auth,
            &proof,
            &transport,
            &body,
            request,
            now(),
        )
    }

    pub fn claim(&self, store: &mut Store, operation_id: &str) -> Result<(OperationRecord, u64)> {
        let auth = store.operation_authorization(self.space.as_str(), operation_id, now() + 1)?;
        let proof = self.freshness(store, &auth, CheckStage::Start, now() + 1)?;
        store.claim_authorized_operation(self.space.as_str(), operation_id, &proof, now() + 1)
    }

    pub fn complete(
        &self,
        store: &mut Store,
        request: &OperationRequest,
        state: OperationState,
        result: Value,
    ) -> Result<SignedEvent> {
        store.finish_operation(
            self.space.as_str(),
            &request.operation_id,
            state.clone(),
            result.clone(),
        )?;
        let dispatch = store
            .operation_dispatch(self.space.as_str(), &request.operation_id)?
            .context("dispatch missing")?;
        store.append_derived_event(
            &self.node,
            self.space.as_str(),
            &dispatch.hash,
            EventBody::ExecutionObserved {
                operation_id: request.operation_id.clone(),
                task_id: request.task_id.clone(),
                target_node: request.target_node.clone(),
                session_id: request.session_id.clone(),
                status: serde_json::to_value(state)?
                    .as_str()
                    .context("invalid operation state")?
                    .into(),
                detail: result,
            },
            vec![],
            (now() + 2) as i64,
        )
    }

    pub fn revoke_client(&self, store: &mut Store) -> Result<()> {
        let mut document = store
            .policy
            .verified()
            .account(&self.account)
            .context("account missing")?
            .clone();
        document.previous = store
            .policy
            .verified()
            .head(&PolicyDomain::Account(self.account.clone()))
            .cloned();
        document.sequence += 1;
        document
            .revoked_devices
            .push(DeviceId::from_key(&PublicKey::of(&self.client)));
        document.revoked_devices.sort();
        document.revoked_devices.dedup();
        document.devices.retain(|hash| {
            store
                .policy
                .verified()
                .device(hash)
                .is_none_or(|certificate| {
                    !document
                        .revoked_devices
                        .contains(&certificate.payload.device)
                })
        });
        store.install_policy_journal(
            &[JournalEntry::Account(Signed::sign(document, &self.root)?)],
            now() + 1,
        )
    }

    pub fn revoke_node(&self, store: &mut Store) -> Result<()> {
        let mut document = store
            .policy
            .verified()
            .account(&self.account)
            .context("account missing")?
            .clone();
        document.previous = store
            .policy
            .verified()
            .head(&PolicyDomain::Account(self.account.clone()))
            .cloned();
        let replacement = Identity::generate().map_err(anyhow::Error::msg)?;
        document.authority = AuthorityBinding::from_key(PublicKey::of(&replacement));
        document.authority_epoch += 1;
        document.sequence = 0;
        document
            .revoked_devices
            .push(DeviceId::from_key(&PublicKey::parse(
                &self.node.public_key_hex()?,
            )?));
        document.revoked_devices.sort();
        document.revoked_devices.dedup();
        document.devices.retain(|hash| {
            store
                .policy
                .verified()
                .device(hash)
                .is_none_or(|certificate| {
                    !document
                        .revoked_devices
                        .contains(&certificate.payload.device)
                })
        });
        store.install_policy_journal(
            &[JournalEntry::Account(Signed::sign(document, &self.root)?)],
            now() + 1,
        )
    }
}
