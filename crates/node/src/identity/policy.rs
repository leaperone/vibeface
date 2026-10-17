use crate::{crypto::LocalIdentity, store::Store};
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use vibeface_secure::authorization::bootstrap::{self, NodeClaim};
use vibeface_secure::authorization::{
    AccountId, Actor, Audience, CheckStage, ClientAuthorizationBundle, DeviceId,
    FreshnessChallenge, FreshnessReceipt, Invocation, InvocationContext, JournalEntry, ObjectHash,
    PolicyDomain, PolicyPin, PublicKey, RequestFacts, ResourceId, ResourceKind, ResourceRef,
    ServiceId, SessionRef, Signed, SpaceId, TransportEvidence, VerifiedAuthorization,
    VerifiedPolicySet,
};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FreshnessProof {
    pub challenge: FreshnessChallenge,
    pub receipts: Vec<Signed<FreshnessReceipt>>,
}

pub(crate) fn request_facts_from(
    connection: &Connection,
    policies: &VerifiedPolicySet,
    body: &Value,
    audience: &Audience,
) -> Result<RequestFacts> {
    let text = |field: &str| -> Result<&str> {
        let value = body[field]
            .as_str()
            .with_context(|| format!("{field} is required"))?;
        ensure!(
            !value.is_empty() && value.len() <= 512,
            "invalid request identifier"
        );
        Ok(value)
    };
    let space = SpaceId::parse(text("space_id")?)?;
    let action = crate::model::authorization_action(body)?;
    let method = text("method")?;
    let operation_id = text("authorization_id")?.to_owned();
    let request = if method == "operate" {
        Some(serde_json::from_value::<crate::model::OperationRequest>(
            body["request"].clone(),
        )?)
    } else {
        None
    };
    if let Some(request) = &request {
        ensure!(
            request.network_id == space.as_str() && request.operation_id == operation_id,
            "operation and authorization scope differ"
        );
    }
    let mut conversation = request
        .as_ref()
        .map(|request| request.conversation_id.clone())
        .or_else(|| body["conversation_id"].as_str().map(str::to_owned));
    if matches!(method, "operation" | "reconcile" | "resolve_unknown") {
        let encoded: Option<String> = connection
            .query_row(
                "SELECT request FROM operations WHERE space_id=? AND operation_id=?",
                params![space.as_str(), text("operation_id")?],
                |row| row.get(0),
            )
            .optional()?;
        if let Some(encoded) = encoded {
            let operation: crate::model::OperationRequest = serde_json::from_str(&encoded)?;
            if let Some(scope) = &conversation {
                ensure!(
                    scope == &operation.conversation_id,
                    "operation conversation mismatch"
                );
            }
            conversation = Some(operation.conversation_id);
        } else {
            ensure!(method == "operation", "operation is unavailable");
        }
    }
    if method == "ack" {
        let encoded: String = connection.query_row(
            "SELECT record FROM notification_outbox WHERE space_id=? AND notification_id=?",
            params![space.as_str(), text("notification_id")?],
            |row| row.get(0),
        )?;
        let notification: crate::model::Notification = serde_json::from_str(&encoded)?;
        if let Some(scope) = &conversation {
            ensure!(
                scope == &notification.conversation_id,
                "notification conversation mismatch"
            );
        }
        conversation = Some(notification.conversation_id);
    }
    if method == "session" && conversation.is_none() {
        let mut statement = connection.prepare(
            "SELECT request FROM operations WHERE space_id=? AND target_node=? AND session_id=? ORDER BY operation_id",
        )?;
        let rows = statement.query_map(
            params![space.as_str(), text("target_node")?, text("session_id")?],
            |row| row.get::<_, String>(0),
        )?;
        for row in rows {
            let operation: crate::model::OperationRequest = serde_json::from_str(&row?)?;
            if let Some(scope) = &conversation {
                ensure!(
                    scope == &operation.conversation_id,
                    "ambiguous session conversation"
                );
            }
            conversation = Some(operation.conversation_id);
        }
        ensure!(
            conversation.is_some(),
            "session has no admitted resource context"
        );
    }
    let stored_resource: Option<String> = if let Some(scope) = &conversation {
        connection
            .query_row(
                "SELECT resource_id FROM conversations WHERE space_id=? AND conversation_id=?",
                params![space.as_str(), scope],
                |row| row.get(0),
            )
            .optional()?
    } else {
        None
    };
    if conversation.is_some() && method != "create_conversation" {
        ensure!(stored_resource.is_some(), "unknown conversation resource");
    }
    let claimed_resource = ResourceId::parse(text("resource_id")?)?;
    let resource = match stored_resource {
        Some(resource) => {
            ensure!(
                claimed_resource.as_str() == resource,
                "request resource mismatch"
            );
            ResourceId::parse(&resource)?
        }
        None => claimed_resource,
    };
    let record = policies
        .resource(&space, &resource)
        .context("resource is not registered by current space policy")?;
    if method == "create_conversation" {
        ensure!(
            record.payload.kind == ResourceKind::Project,
            "conversations require a project resource"
        );
    }
    ensure!(
        record.payload.host == audience.service,
        "request must target the registered resource host"
    );
    let mut context = InvocationContext {
        conversation: conversation.clone(),
        workspace_resource: Some(resource.clone()),
        ..Default::default()
    };
    let task = request
        .as_ref()
        .and_then(|request| request.task_id.as_deref())
        .or_else(|| body["task_id"].as_str());
    if let Some(task) = task {
        let scope = conversation
            .as_deref()
            .context("task requires a conversation")?;
        if method != "create_task" {
            let mut statement = connection.prepare("SELECT envelope FROM events WHERE space_id=? AND scope_id=? AND status='ready' ORDER BY hash")?;
            let rows = statement.query_map(params![space.as_str(), scope], |row| {
                row.get::<_, String>(0)
            })?;
            let mut creation = None;
            for encoded in rows {
                let envelope: crate::model::SignedEvent = serde_json::from_str(&encoded?)?;
                let event: crate::model::Event = serde_json::from_str(&envelope.canonical_body)?;
                if matches!(&event.body, crate::model::EventBody::TaskCreated { task_id, .. } if task_id == task)
                {
                    ensure!(creation.is_none(), "ambiguous task creation");
                    if let Some(expected) = body["task_ref"].as_str() {
                        ensure!(
                            expected == envelope.hash,
                            "task creation reference mismatch"
                        );
                    }
                    creation = Some(ObjectHash::body(&envelope)?);
                }
            }
            context.task = Some(creation.context("task creation is missing")?);
        }
    }
    if let Some(request) = &request {
        context.execution_session = Some(SessionRef {
            target: audience.device.clone(),
            session_id: request.session_id.clone(),
        });
        context.executor_kind = request.action["executor"].as_str().map(str::to_owned);
        if context.executor_kind.is_none() {
            let encoded: Option<String> = connection.query_row(
                "SELECT record FROM execution_sessions WHERE space_id=? AND target_node=? AND session_id=?",
                params![space.as_str(), request.target_node, request.session_id], |row| row.get(0)).optional()?;
            if let Some(encoded) = encoded {
                let session: crate::model::ExecutionSession = serde_json::from_str(&encoded)?;
                ensure!(
                    session.network_id == space.as_str()
                        && session.target_node == request.target_node
                        && session.session_id == request.session_id,
                    "execution session projection mismatch"
                );
                context.executor_kind = Some(session.executor);
            }
        }
        let key: String = connection.query_row(
            "SELECT public_key FROM node_certificates WHERE space_id=? AND node_id=?",
            params![space.as_str(), request.target_node],
            |row| row.get(0),
        )?;
        ensure!(
            DeviceId::from_key(&PublicKey::parse(&key)?) == audience.device,
            "operation targets another device"
        );
    } else if matches!(method, "bind_execution" | "rebind_execution") {
        let target_key: String = connection.query_row(
            "SELECT public_key FROM node_certificates WHERE space_id=? AND node_id=?",
            params![space.as_str(), text("target_node")?],
            |row| row.get(0),
        )?;
        context.execution_session = Some(SessionRef {
            target: DeviceId::from_key(&PublicKey::parse(&target_key)?),
            session_id: text("session_id")?.to_owned(),
        });
        context.executor_kind = Some(text("executor")?.to_owned());
    } else if method == "seal_execution" {
        let binding: crate::binding::SignedBinding =
            serde_json::from_value(body["binding"].clone())?;
        let target_key: String = connection.query_row(
            "SELECT public_key FROM node_certificates WHERE space_id=? AND node_id=?",
            params![space.as_str(), binding.binding.target_node],
            |row| row.get(0),
        )?;
        context.execution_session = Some(SessionRef {
            target: DeviceId::from_key(&PublicKey::parse(&target_key)?),
            session_id: binding.binding.session_id,
        });
        context.executor_kind = binding.binding.open_spec["executor"]
            .as_str()
            .map(str::to_owned);
    }
    if !body["sdk_session"].is_null() {
        let session: SessionRef = serde_json::from_value(body["sdk_session"].clone())?;
        let encoded: String = connection.query_row(
            "SELECT value FROM local_values WHERE space_id=? AND key=?",
            params![
                space.as_str(),
                format!("agent-session:{}", session.session_id)
            ],
            |row| row.get(0),
        )?;
        let saved: Value = serde_json::from_str(&encoded)?;
        ensure!(
            saved["sdk_session"] == body["sdk_session"]
                && saved["parent_invocation"] == body["parent_invocation"]
                && saved["conversation_id"].as_str() == conversation.as_deref(),
            "agent session context mismatch"
        );
        context.sdk_session = Some(session);
    }
    let workspace = request
        .as_ref()
        .and_then(|request| request.action["cwd"].as_str())
        .or_else(|| {
            matches!(method, "bind_execution" | "rebind_execution")
                .then(|| body["cwd"].as_str())
                .flatten()
        });
    if let Some(workspace) = workspace {
        let root = resource_root_from(connection, space.as_str(), &resource)?;
        let canonical = std::fs::canonicalize(workspace)?;
        ensure!(
            canonical.to_str() == Some(workspace) && canonical.starts_with(&root),
            "workspace is outside the registered project root"
        );
    }
    Ok(RequestFacts {
        resource: ResourceRef { space, resource },
        action,
        audience: audience.clone(),
        operation_id,
        body_hash: ObjectHash::body(body)?,
        context,
    })
}

fn resource_root_from(
    connection: &Connection,
    space: &str,
    resource: &ResourceId,
) -> Result<std::path::PathBuf> {
    let encoded: String = connection
        .query_row(
            "SELECT value FROM local_values WHERE space_id=? AND key=?",
            params![space, format!("resource-root:{resource}")],
            |row| row.get(0),
        )
        .context("project has no local execution root")?;
    let root: std::path::PathBuf = serde_json::from_str(&encoded)?;
    ensure!(
        root.is_absolute() && root.is_dir() && std::fs::canonicalize(&root)? == root,
        "registered project root changed"
    );
    Ok(root)
}

impl Store {
    fn accepted_policy_journal(&self) -> Result<Vec<JournalEntry>> {
        let mut statement = self
            .connection
            .prepare("SELECT entry FROM policy_journal ORDER BY ordinal")?;
        let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
        rows.map(|row| Ok(serde_json::from_str(&row?)?)).collect()
    }

    fn authenticate_account(&self, principal: &str, params: &Value, now_ms: u64) -> Result<Value> {
        ensure!(
            params["device_public_key"].as_str() == Some(principal),
            "account device differs from Noise identity"
        );
        let account = AccountId::parse(
            params["account_id"]
                .as_str()
                .context("account_id is required")?,
        )?;
        let space = SpaceId::parse(
            params["space_id"]
                .as_str()
                .context("space_id is required")?,
        )?;
        let key = PublicKey::parse(principal)?;
        let requested_resource: Option<ResourceId> = params
            .get("resource_id")
            .cloned()
            .map(serde_json::from_value)
            .transpose()?;
        let requested_audience: Option<Audience> = params
            .get("audience")
            .cloned()
            .map(serde_json::from_value)
            .transpose()?;
        let journal = self.accepted_policy_journal()?;
        let current = self
            .policy
            .verified()
            .space(&space)
            .context("unknown space")?;
        for leaf in &current.grants {
            let grant = self
                .policy
                .verified()
                .grant(leaf)
                .context("registered grant proof is missing")?;
            if requested_audience
                .as_ref()
                .is_some_and(|audience| audience != &grant.payload.audience)
            {
                continue;
            }
            let Ok(bundle) = self.policy.verified().client_authorization_projection(
                self.policy.pins(),
                &journal,
                &account,
                &space,
                &key,
                leaf,
                now_ms,
            ) else {
                continue;
            };
            let pins = bundle_pins(self.policy.pins(), &bundle);
            let verified = vibeface_secure::authorization::verify_client_authorization(
                &serde_json::to_string(&bundle)?,
                &serde_json::to_string(&pins)?,
                account.as_str(),
                space.as_str(),
                principal,
                now_ms as f64,
            )
            .map_err(anyhow::Error::msg)?;
            if requested_resource.as_ref().is_some_and(|resource| {
                !verified
                    .resources()
                    .iter()
                    .any(|record| &record.payload.resource == resource)
            }) {
                continue;
            }
            return Ok(
                json!({"scope":verified.scope(),"authorization":bundle,"pins":pins,"resources":verified.resources()}),
            );
        }
        anyhow::bail!("device has no current account authorization for this space")
    }
    pub fn install_policy_journal(&mut self, journal: &[JournalEntry], now_ms: u64) -> Result<()> {
        let update = self.policy.prepare_network_update(journal, now_ms)?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let state = super::journal::commit_in(&tx, &update)?;
        project_policy_in(&tx, &state)?;
        tx.commit()?;
        self.policy = state;
        Ok(())
    }

    fn node_claim(
        &mut self,
        identity: &LocalIdentity,
        capability: &str,
        now_ms: u64,
    ) -> Result<Value> {
        self.verify_bootstrap_capability(capability)?;
        let previous: Option<String> = self
            .connection
            .query_row(
                "SELECT value FROM identity_meta WHERE key='node_claim'",
                [],
                |row| row.get(0),
            )
            .optional()?;
        if let Some(previous) = previous {
            let claim: Signed<NodeClaim> = serde_json::from_str(&previous)?;
            ensure!(
                claim.payload.node_public_key.as_str() == identity.public_key_hex()?
                    && claim.payload.claim_nonce.as_str() == capability,
                "stored Node claim differs from this installation"
            );
            if claim.payload.issued_at_ms <= now_ms && now_ms < claim.payload.expires_at_ms {
                return Ok(json!({"claim":claim}));
            }
        }
        let signer = vibeface_secure::Identity::from_secret_hex(&identity.secret_key_hex)
            .map_err(anyhow::Error::msg)?;
        let encoded = bootstrap::create_node_claim(
            &signer,
            capability,
            now_ms as f64,
            now_ms
                .checked_add(bootstrap::NODE_CLAIM_MAX_AGE_MS)
                .context("claim expiry overflow")? as f64,
        )
        .map_err(anyhow::Error::msg)?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute("DELETE FROM identity_meta WHERE key='node_possession'", [])?;
        tx.execute("INSERT INTO identity_meta(key,value) VALUES('node_claim',?) ON CONFLICT(key) DO UPDATE SET value=excluded.value", [&encoded])?;
        tx.commit()?;
        Ok(json!({"claim":serde_json::from_str::<Value>(&encoded)?}))
    }

    fn node_possession(
        &mut self,
        identity: &LocalIdentity,
        principal: &str,
        request: &Value,
        now_ms: u64,
    ) -> Result<Value> {
        let claim: Signed<NodeClaim> = serde_json::from_value(request["claim"].clone())?;
        self.verify_bootstrap_capability(claim.payload.claim_nonce.as_str())?;
        let saved: String = self.connection.query_row(
            "SELECT value FROM identity_meta WHERE key='node_claim'",
            [],
            |row| row.get(0),
        )?;
        ensure!(
            serde_json::from_str::<Signed<NodeClaim>>(&saved)? == claim,
            "claim is not the current local challenge"
        );
        let request_hash = ObjectHash::body(request)?;
        let previous: Option<String> = self
            .connection
            .query_row(
                "SELECT value FROM identity_meta WHERE key='node_possession'",
                [],
                |row| row.get(0),
            )
            .optional()?;
        if let Some(previous) = previous {
            let previous: Value = serde_json::from_str(&previous)?;
            ensure!(
                previous["request_hash"] == request_hash.as_str()
                    && previous["principal"] == principal,
                "claim was already bound to another account or device"
            );
            return Ok(json!({"proof":previous["proof"]}));
        }
        let signer = vibeface_secure::Identity::from_secret_hex(&identity.secret_key_hex)
            .map_err(anyhow::Error::msg)?;
        let proof = bootstrap::sign_node_possession(
            &serde_json::to_string(request)?,
            &signer,
            claim.payload.claim_nonce.as_str(),
            now_ms as f64,
        )
        .map_err(anyhow::Error::msg)?;
        let receipt = json!({"request_hash":request_hash,"principal":principal,"request":request,"proof":proof});
        self.connection.execute(
            "INSERT INTO identity_meta(key,value) VALUES('node_possession',?)",
            [serde_json::to_string(&receipt)?],
        )?;
        Ok(json!({"proof":proof}))
    }

    fn bootstrap_account(
        &mut self,
        identity: &LocalIdentity,
        principal: &str,
        params: &Value,
        roots: &[std::path::PathBuf],
        now_ms: u64,
    ) -> Result<Value> {
        let request_hash = ObjectHash::body(params)?;
        let old: Option<String> = self
            .connection
            .query_row(
                "SELECT value FROM identity_meta WHERE key='account_bootstrap'",
                [],
                |row| row.get(0),
            )
            .optional()?;
        if let Some(old) = old {
            let old: Value = serde_json::from_str(&old)?;
            ensure!(
                old["request_hash"] == request_hash.as_str() && old["principal"] == principal,
                "Node has already been claimed"
            );
            return Ok(old["response"].clone());
        }
        let pins: Vec<PolicyPin> = serde_json::from_value(params["pins"].clone())?;
        let bundle: ClientAuthorizationBundle =
            serde_json::from_value(params["authorization"].clone())?;
        ensure!(
            pins.len() == 2,
            "account creation must pin exactly one account and personal space"
        );
        let account = pins
            .iter()
            .find_map(|pin| match &pin.domain {
                PolicyDomain::Account(id) => Some(id),
                _ => None,
            })
            .context("account pin is missing")?;
        let space = pins
            .iter()
            .find_map(|pin| match &pin.domain {
                PolicyDomain::Space(id) => Some(id),
                _ => None,
            })
            .context("space pin is missing")?;
        let verified = vibeface_secure::authorization::verify_client_authorization(
            &serde_json::to_string(&bundle)?,
            &serde_json::to_string(&pins)?,
            account.as_str(),
            space.as_str(),
            principal,
            now_ms as f64,
        )
        .map_err(anyhow::Error::msg)?;
        let key = PublicKey::parse(&identity.public_key_hex()?)?;
        ensure!(
            verified.audience().device == DeviceId::from_key(&key)
                && verified.audience().service == ServiceId::from_key(&key),
            "account targets another Node"
        );
        let possession: String = self.connection.query_row(
            "SELECT value FROM identity_meta WHERE key='node_possession'",
            [],
            |row| row.get(0),
        )?;
        let possession: Value = serde_json::from_str(&possession)?;
        ensure!(
            possession["principal"] == principal,
            "account claimant differs from Noise device"
        );
        let request = &possession["request"];
        let claim: Signed<NodeClaim> = serde_json::from_value(request["claim"].clone())?;
        self.verify_bootstrap_capability(claim.payload.claim_nonce.as_str())?;
        ensure!(
            now_ms < claim.payload.expires_at_ms,
            "account creation challenge expired"
        );
        let mut matching_node = false;
        let mut matching_genesis = false;
        for entry in &bundle.journal {
            match entry {
                JournalEntry::Device(certificate) if certificate.payload.public_key == key => {
                    let mut unsigned = certificate.payload.clone();
                    unsigned.possession = None;
                    ensure!(
                        serde_json::to_value(&unsigned)? == request["node_certificate"]
                            && certificate
                                .payload
                                .possession
                                .as_ref()
                                .map(|proof| proof.as_str())
                                == possession["proof"].as_str(),
                        "Node certificate differs from approved possession request"
                    );
                    matching_node = true;
                }
                JournalEntry::AccountGenesis(document) => {
                    ensure!(
                        serde_json::to_value(document)? == request["account_genesis"],
                        "account genesis differs from approved possession request"
                    );
                    matching_genesis = true;
                }
                _ => {}
            }
        }
        ensure!(
            matching_node && matching_genesis,
            "account bootstrap lacks approved Node enrollment"
        );
        let update = self
            .policy
            .prepare_bootstrap(&pins, &bundle.journal, now_ms)?;
        let response = json!({"scope":verified.scope(),"authorization":bundle,"pins":pins,"resources":verified.resources()});
        let receipt =
            json!({"request_hash":request_hash,"principal":principal,"response":response});
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let state = super::journal::commit_in(&tx, &update)?;
        project_policy_in(&tx, &state)?;
        let selected_root = if let Some(selected) = params["project_root"].as_str() {
            let selected = std::fs::canonicalize(selected)?;
            ensure!(
                roots
                    .iter()
                    .any(|root| std::fs::canonicalize(root).is_ok_and(|root| root == selected)),
                "project root is not configured on this Node"
            );
            Some(selected)
        } else if roots.len() == 1 {
            Some(std::fs::canonicalize(&roots[0])?)
        } else {
            None
        };
        if let Some(root) = selected_root {
            for resource in verified.resources() {
                tx.execute(
                    "INSERT INTO local_values(space_id,key,value) VALUES(?,?,?)",
                    params![
                        space.as_str(),
                        format!("resource-root:{}", resource.payload.resource),
                        serde_json::to_string(&root)?
                    ],
                )?;
            }
        }
        let changed = tx.execute("UPDATE identity_meta SET value='consumed:'||value WHERE key='bootstrap_capability_hash' AND value=?", [crate::crypto::digest(claim.payload.claim_nonce.as_str().as_bytes())])?;
        ensure!(changed == 1, "bootstrap capability was already consumed");
        tx.execute(
            "INSERT INTO identity_meta(key,value) VALUES('account_bootstrap',?)",
            [serde_json::to_string(&receipt)?],
        )?;
        tx.commit()?;
        self.policy = state;
        Ok(response)
    }
    pub fn authorization_state(&self) -> &VerifiedPolicySet {
        self.policy.verified()
    }

    pub fn trusted_policy_pins(&self) -> &[PolicyPin] {
        self.policy.pins()
    }

    pub fn request_facts(&self, body: &Value, audience: &Audience) -> Result<RequestFacts> {
        request_facts_from(&self.connection, self.policy.verified(), body, audience)
    }

    pub fn resource_root(&self, space: &str, resource: &ResourceId) -> Result<std::path::PathBuf> {
        resource_root_from(&self.connection, space, resource)
    }

    pub fn authorized_conversation_ids(&self, auth: &VerifiedAuthorization) -> Result<Vec<String>> {
        self.resource_conversation_ids(auth.resource().space.as_str(), &auth.resource().resource)
    }

    pub fn resource_conversation_ids(
        &self,
        space: &str,
        resource: &ResourceId,
    ) -> Result<Vec<String>> {
        let mut statement = self.connection.prepare(
            "SELECT conversation_id FROM conversations WHERE space_id=? AND resource_id=? ORDER BY conversation_id",
        )?;
        Ok(statement
            .query_map(params![space, resource.as_str()], |row| row.get(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?)
    }

    pub fn resource_event_hashes(
        &self,
        space: &str,
        resource: &ResourceId,
        after: Option<&str>,
        limit: usize,
    ) -> Result<Vec<String>> {
        ensure!(
            (1..=257).contains(&limit),
            "invalid resource inventory page size"
        );
        let mut statement = self.connection.prepare(
            "SELECT hash FROM events WHERE space_id=? AND resource_id=? AND hash>? ORDER BY hash LIMIT ?",
        )?;
        Ok(statement
            .query_map(
                params![space, resource.as_str(), after.unwrap_or(""), limit],
                |row| row.get(0),
            )?
            .collect::<std::result::Result<Vec<_>, _>>()?)
    }

    pub fn resource_blob_allowed(
        &self,
        space: &str,
        resource: &ResourceId,
        hash: &str,
    ) -> Result<bool> {
        let mut statement = self.connection.prepare("SELECT e.envelope FROM events e JOIN json_each(json_extract(e.envelope,'$.canonical_body'),'$.blobs') b WHERE e.space_id=? AND e.resource_id=? AND e.status='ready' AND json_extract(b.value,'$.hash')=? LIMIT 1")?;
        let encoded: Option<String> = statement
            .query_row(params![space, resource.as_str(), hash], |row| row.get(0))
            .optional()?;
        if let Some(encoded) = encoded {
            let envelope = serde_json::from_str(&encoded)?;
            let event = self.verify_event(space, &envelope)?;
            ensure!(
                event.resource_id == *resource,
                "blob resource projection mismatch"
            );
            return Ok(event.blobs.iter().any(|blob| blob.hash == hash));
        }
        Ok(false)
    }

    pub fn authorize_request(
        &self,
        invocation: &Signed<Invocation>,
        facts: &RequestFacts,
        transport: &TransportEvidence,
        now_ms: u64,
    ) -> Result<VerifiedAuthorization> {
        Ok(self
            .policy
            .verified()
            .verify_invocation(invocation, facts, transport, now_ms)?)
    }

    pub fn issue_freshness(
        &mut self,
        auth: &VerifiedAuthorization,
        stage: CheckStage,
        now_ms: u64,
    ) -> Result<FreshnessChallenge> {
        self.policy.verified().check_current(auth, now_ms)?;
        let lifetime = if stage == CheckStage::RemoteRead {
            vibeface_secure::authorization::REMOTE_READ_MAX_AGE_MS
        } else {
            vibeface_secure::authorization::EXECUTION_LEASE_MS
        };
        let expires_at_ms = now_ms
            .checked_add(lifetime)
            .context("challenge expiry overflow")?
            .min(auth.expires_at_ms());
        let challenge = FreshnessChallenge::new(
            auth,
            stage,
            vibeface_secure::authorization::Nonce::random()?,
            now_ms,
            expires_at_ms,
        )?;
        let encoded = serde_json::to_string(&challenge)?;
        let stage = serde_json::to_string(&stage)?;
        self.connection.execute(
            "INSERT INTO authorization_challenges(space_id,nonce,invocation_hash,stage,challenge,expires_at_ms) VALUES(?,?,?,?,?,?)",
            params![auth.resource().space.as_str(), challenge.nonce.as_str(), auth.invocation_hash().as_str(), stage.trim_matches('"'), encoded, expires_at_ms],
        )?;
        Ok(challenge)
    }

    pub fn sign_current_receipts(
        &self,
        challenge: &FreshnessChallenge,
        identity: &LocalIdentity,
        now_ms: u64,
    ) -> Result<Vec<Signed<FreshnessReceipt>>> {
        use vibeface_secure::authorization::{DeviceId, PublicKey, ServiceId};
        ensure!(
            challenge.issued_at_ms <= now_ms && now_ms < challenge.expires_at_ms,
            "freshness challenge expired"
        );
        let key = PublicKey::parse(&identity.public_key_hex()?)?;
        let signer = vibeface_secure::Identity::from_secret_hex(&identity.secret_key_hex)
            .map_err(anyhow::Error::msg)?;
        let mut receipts = Vec::new();
        for expected in &challenge.policies {
            let current = self
                .policy
                .verified()
                .head(&expected.domain)
                .context("freshness policy is not installed")?;
            ensure!(
                current == expected,
                "freshness challenge references an outdated policy"
            );
            let authority = match &expected.domain {
                PolicyDomain::Account(id) => {
                    &self
                        .policy
                        .verified()
                        .account(id)
                        .context("account policy is missing")?
                        .authority
                }
                PolicyDomain::Space(id) => {
                    &self
                        .policy
                        .verified()
                        .space(id)
                        .context("space policy is missing")?
                        .authority
                }
            };
            if authority.public_key == key {
                ensure!(
                    authority.device == DeviceId::from_key(&key)
                        && authority.service == ServiceId::from_key(&key),
                    "authority identity mismatch"
                );
                receipts.push(Signed::sign(
                    FreshnessReceipt {
                        version: vibeface_secure::authorization::AUTH_VERSION,
                        current: current.clone(),
                        challenge_hash: challenge.hash()?,
                        issued_at_ms: now_ms,
                        expires_at_ms: challenge.expires_at_ms,
                    },
                    &signer,
                )?);
            }
        }
        Ok(receipts)
    }
}

fn bundle_pins(pins: &[PolicyPin], bundle: &ClientAuthorizationBundle) -> Vec<PolicyPin> {
    let domains: std::collections::BTreeSet<_> = bundle
        .journal
        .iter()
        .filter_map(|entry| match entry {
            JournalEntry::AccountGenesis(document) | JournalEntry::Account(document) => {
                Some(PolicyDomain::Account(document.payload.account.clone()))
            }
            JournalEntry::SpaceGenesis(document) | JournalEntry::Space(document) => {
                Some(PolicyDomain::Space(document.payload.space.clone()))
            }
            JournalEntry::AccountCommit(commit) => Some(PolicyDomain::Account(
                commit.proposal.payload.account.clone(),
            )),
            JournalEntry::SpaceCommit(commit) => {
                Some(PolicyDomain::Space(commit.proposal.payload.space.clone()))
            }
            _ => None,
        })
        .collect();
    pins.iter()
        .filter(|pin| domains.contains(&pin.domain))
        .cloned()
        .collect()
}

pub(crate) fn project_policy_in(
    connection: &Connection,
    policy: &super::journal::PolicyState,
) -> Result<()> {
    let mut statement = connection.prepare("SELECT entry FROM policy_journal ORDER BY ordinal")?;
    let entries = statement
        .query_map([], |row| row.get::<_, String>(0))?
        .map(|row| Ok(serde_json::from_str::<JournalEntry>(&row?)?))
        .collect::<Result<Vec<_>>>()?;
    drop(statement);
    for pin in policy.pins() {
        let PolicyDomain::Space(space) = &pin.domain else {
            continue;
        };
        let history = policy
            .verified()
            .space_history(space)
            .context("space projection has no verified history")?;
        let current = history.last().context("space projection is empty")?;
        connection.execute("INSERT INTO spaces(space_id,revision,bundle) VALUES(?,?,?) ON CONFLICT(space_id) DO UPDATE SET revision=excluded.revision,bundle=excluded.bundle",
            params![space.as_str(),current.payload.sequence,serde_json::to_string(current)?])?;
        for entry in &entries {
            match entry {
                JournalEntry::Resource(record) if record.payload.space == *space => {
                    let kind = match record.payload.kind {
                        ResourceKind::Project => "project",
                        ResourceKind::SpaceControl => "space_control",
                        ResourceKind::ExecutionService => "execution_service",
                    };
                    let hash = record.hash()?;
                    connection.execute("INSERT INTO resource_records(space_id,resource_id,kind,host_service_id,record_hash,signed_record) VALUES(?,?,?,?,?,?) ON CONFLICT(space_id,resource_id) DO UPDATE SET kind=excluded.kind,host_service_id=excluded.host_service_id,record_hash=excluded.record_hash,signed_record=excluded.signed_record",
                        params![space.as_str(),record.payload.resource.as_str(),kind,record.payload.host.as_str(),hash.as_str(),serde_json::to_string(record)?])?;
                }
                JournalEntry::Device(certificate)
                    if matches!(certificate.payload.subject, Actor::Service(_)) =>
                {
                    let associated = match &certificate.payload.owner {
                        PolicyDomain::Space(owner) => owner == space,
                        PolicyDomain::Account(owner) => matches!(&current.payload.root,
                            vibeface_secure::authorization::SpaceRoot::Personal { owner: account, .. } if account == owner),
                    };
                    if associated {
                        let node =
                            crate::crypto::fingerprint(certificate.payload.public_key.as_str())?;
                        let old: Option<String> = connection.query_row("SELECT public_key FROM node_certificates WHERE space_id=? AND node_id=?",
                            params![space.as_str(),node], |row| row.get(0)).optional()?;
                        ensure!(
                            old.as_deref()
                                .is_none_or(|old| old == certificate.payload.public_key.as_str()),
                            "historical Node public key changed"
                        );
                        connection.execute("INSERT OR IGNORE INTO node_certificates(space_id,node_id,public_key) VALUES(?,?,?)",
                            params![space.as_str(),node,certificate.payload.public_key.as_str()])?;
                    }
                }
                _ => {}
            }
        }
    }
    Ok(())
}

pub(crate) fn validate_resource_projections(connection: &Connection) -> Result<()> {
    let mut statement = connection.prepare("SELECT space_id,resource_id,kind,host_service_id,record_hash,signed_record FROM resource_records")?;
    let rows = statement.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, String>(3)?,
            row.get::<_, String>(4)?,
            row.get::<_, String>(5)?,
        ))
    })?;
    for row in rows {
        let (space, resource, kind, host, hash, encoded) = row?;
        let record: Signed<vibeface_secure::authorization::ResourceRecord> =
            serde_json::from_str(&encoded)?;
        let expected_kind = match record.payload.kind {
            ResourceKind::SpaceControl => "space_control",
            ResourceKind::Project => "project",
            ResourceKind::ExecutionService => "execution_service",
        };
        ensure!(
            record.payload.space.as_str() == space
                && record.payload.resource.as_str() == resource
                && record.payload.host.as_str() == host
                && expected_kind == kind
                && record.hash()?.as_str() == hash,
            "stored resource metadata differs from signed record"
        );
        let accepted: String = connection.query_row(
            "SELECT entry FROM policy_journal WHERE entry_hash=?",
            [&hash],
            |row| row.get(0),
        )?;
        ensure!(
            serde_json::from_str::<JournalEntry>(&accepted)? == JournalEntry::Resource(record),
            "resource was not accepted by the policy journal"
        );
    }
    Ok(())
}

impl crate::runtime::Runtime {
    pub async fn identity_command(
        &self,
        noise_public_key: &str,
        method: &str,
        params: &Value,
    ) -> Result<Value> {
        let now_ms = crate::runtime::now_ms() as u64;
        PublicKey::parse(noise_public_key)?;
        let mut store = self.store.lock().await;
        match method {
            "status" => Ok(
                json!({"initialized":!store.trusted_policy_pins().is_empty(),"node_id":self.identity.node_id()?,
                "public_key":self.identity.public_key_hex()?,"roles":self.config.roles,"version":env!("CARGO_PKG_VERSION")}),
            ),
            "node_claim" => store.node_claim(
                &self.identity,
                params["capability"]
                    .as_str()
                    .context("connection capability is required")?,
                now_ms,
            ),
            "node_possession" => {
                store.node_possession(&self.identity, noise_public_key, params, now_ms)
            }
            "bootstrap" => store.bootstrap_account(
                &self.identity,
                noise_public_key,
                params,
                &self.config.roots,
                now_ms,
            ),
            "recovery_context" => {
                store.recovery_context(&self.identity, noise_public_key, params, now_ms)
            }
            "recovery_node_possession" => {
                store.recovery_node_possession(&self.identity, noise_public_key, params, now_ms)
            }
            "recover" => store.recover_account(&self.identity, noise_public_key, params, now_ms),
            "authenticate" => store.authenticate_account(noise_public_key, params, now_ms),
            "device_directory" => {
                let authenticated = store.authenticate_account(noise_public_key, params, now_ms)?;
                let account = AccountId::parse(
                    params["account_id"]
                        .as_str()
                        .context("account_id is required")?,
                )?;
                let document = store
                    .policy
                    .verified()
                    .account(&account)
                    .context("account policy is missing")?;
                let devices = document
                    .devices
                    .iter()
                    .map(|hash| {
                        store
                            .policy
                            .verified()
                            .device(hash)
                            .cloned()
                            .context("registered account device proof is missing")
                    })
                    .collect::<Result<Vec<_>>>()?;
                Ok(json!({"authorization":authenticated["authorization"],"devices":devices}))
            }
            "policy_commit" => {
                let entries: Vec<JournalEntry> = serde_json::from_value(params["journal"].clone())?;
                // Public signed updates prove their own exact authority; new roots are never learned here.
                store.install_policy_journal(&entries, now_ms)?;
                Ok(json!({"accepted":true}))
            }
            _ => anyhow::bail!("unknown identity method"),
        }
    }
}
