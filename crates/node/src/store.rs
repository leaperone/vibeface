use crate::{
    crypto::{LocalIdentity, digest, verify_signed_event, verify_trust_bundle},
    model::*,
};
use anyhow::{Context as _, Result, bail, ensure};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde::de::DeserializeOwned;
use serde_json::Value;
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Component, Path},
};

const INITIAL_SCHEMA: &str = include_str!("../migrations/202610160001_initial.sql");

pub struct Store {
    connection: Connection,
}
#[derive(Clone)]
struct StoredEvent {
    envelope: SignedEvent,
    event: Event,
    status: String,
}

impl Store {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        if path != Path::new(":memory:")
            && let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty())
        {
            let existed = parent.exists();
            std::fs::create_dir_all(parent)?;
            #[cfg(unix)]
            if !existed {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))?;
            }
        }
        let connection = Connection::open(path)?;
        #[cfg(unix)]
        if path != Path::new(":memory:") {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
        }
        connection.busy_timeout(std::time::Duration::from_secs(5))?;
        connection.execute_batch("PRAGMA foreign_keys=ON; PRAGMA journal_mode=WAL;")?;
        let version: u32 = connection.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        match version {
            0 => {
                let tables:i64 = connection.query_row("SELECT count(*) FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%'",[],|r|r.get(0))?;
                ensure!(tables == 0, "unversioned database is not supported");
                connection.execute_batch(&format!("BEGIN IMMEDIATE;{INITIAL_SCHEMA}COMMIT;"))?;
            }
            1 => {}
            _ => bail!("unsupported database schema version {version}"),
        }
        let mut store = Self { connection };
        store.recover_running()?;
        for network in store.networks()? {
            store.rebuild_projections(&network)?;
        }
        Ok(store)
    }
    pub fn install_trust_bundle(&mut self, network_id: &str, bundle: &TrustBundle) -> Result<bool> {
        ensure!(
            bundle.network_id == network_id,
            "trust bundle belongs to another network"
        );
        verify_trust_bundle(bundle)?;
        let encoded = serde_json::to_string(bundle)?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let old: Option<(u64, String)> = tx
            .query_row(
                "SELECT revision,bundle FROM networks WHERE network_id=?",
                [network_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        if let Some((revision, previous)) = old {
            if revision == bundle.revision {
                ensure!(previous == encoded, "conflicting trust revision");
                return Ok(false);
            }
            ensure!(bundle.revision > revision, "trust bundle rollback rejected");
        }
        tx.execute("INSERT INTO networks(network_id,revision,bundle) VALUES(?,?,?) ON CONFLICT(network_id) DO UPDATE SET revision=excluded.revision,bundle=excluded.bundle",params![network_id,bundle.revision,encoded])?;
        for node in &bundle.nodes {
            let old_key: Option<String> = tx
                .query_row(
                    "SELECT public_key FROM node_certificates WHERE network_id=? AND node_id=?",
                    params![network_id, node.node_id],
                    |r| r.get(0),
                )
                .optional()?;
            ensure!(
                old_key.as_ref().is_none_or(|key| key == &node.public_key),
                "node certificate key changed"
            );
            tx.execute("INSERT OR IGNORE INTO node_certificates(network_id,node_id,public_key) VALUES(?,?,?)",params![network_id,node.node_id,node.public_key])?;
        }
        tx.commit()?;
        Ok(true)
    }
    pub fn trust_bundle(&self, network_id: &str) -> Result<Option<TrustBundle>> {
        optional_json(
            self.connection
                .query_row(
                    "SELECT bundle FROM networks WHERE network_id=?",
                    [network_id],
                    |r| r.get(0),
                )
                .optional()?,
        )
    }
    pub fn authorize_node(
        &self,
        network_id: &str,
        node_id: &str,
        role: Option<NodeRole>,
    ) -> Result<NodeCertificate> {
        authorize_node_from(&self.connection, network_id, node_id, role)
    }
    pub fn save_local_identity(
        &mut self,
        network_id: &str,
        identity: &LocalIdentity,
    ) -> Result<()> {
        let node_id = identity.node_id()?;
        self.authorize_node(network_id, &node_id, None)?;
        self.connection.execute("INSERT INTO local_identities(network_id,node_id,identity) VALUES(?,?,?) ON CONFLICT(network_id,node_id) DO UPDATE SET identity=excluded.identity",params![network_id,node_id,serde_json::to_string(identity)?])?;
        Ok(())
    }
    pub fn local_identity(&self, network_id: &str, node_id: &str) -> Result<Option<LocalIdentity>> {
        optional_json(
            self.connection
                .query_row(
                    "SELECT identity FROM local_identities WHERE network_id=? AND node_id=?",
                    params![network_id, node_id],
                    |r| r.get(0),
                )
                .optional()?,
        )
    }
    fn public_key(&self, network_id: &str, author: &str) -> Result<String> {
        self.connection
            .query_row(
                "SELECT public_key FROM node_certificates WHERE network_id=? AND node_id=?",
                params![network_id, author],
                |r| r.get(0),
            )
            .optional()?
            .context("unknown historical event author")
    }
    pub fn verify_event(&self, network_id: &str, envelope: &SignedEvent) -> Result<Event> {
        let event: Event = serde_json::from_str(&envelope.canonical_body)?;
        let key = self.public_key(network_id, &event.author)?;
        verify_signed_event(network_id, envelope, &key)
    }
    pub fn ingest_event(
        &mut self,
        network_id: &str,
        envelope: &SignedEvent,
    ) -> Result<IngestOutcome> {
        let event = self.verify_event(network_id, envelope)?;
        let encoded = serde_json::to_string(envelope)?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let old: Option<String> = tx
            .query_row(
                "SELECT envelope FROM events WHERE network_id=? AND hash=?",
                params![network_id, envelope.hash],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(old) = old {
            ensure!(old == encoded, "conflicting envelope for event hash");
        } else {
            tx.execute("INSERT INTO events(network_id,hash,scope_id,author,sequence,envelope,status) VALUES(?,?,?,?,?,?,'pending')",params![network_id,envelope.hash,event.scope_id,event.author,event.sequence,encoded])?;
        }
        rebuild_projections_from(&tx, network_id)?;
        tx.commit()?;
        let status: String = self.connection.query_row(
            "SELECT status FROM events WHERE network_id=? AND hash=?",
            params![network_id, envelope.hash],
            |r| r.get(0),
        )?;
        Ok(IngestOutcome {
            hash: envelope.hash.clone(),
            status,
            missing: self.missing_dependencies(network_id, std::slice::from_ref(&envelope.hash))?,
        })
    }
    pub fn append_event(
        &mut self,
        identity: &LocalIdentity,
        network_id: &str,
        scope_id: &str,
        body: EventBody,
        blobs: Vec<BlobRef>,
        created_at_ms: i64,
    ) -> Result<SignedEvent> {
        let author = identity.node_id()?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        authorize_node_from(&tx, network_id, &author, None)?;
        let events = load_events_from(&tx, network_id)?;
        let heads = heads_from_events(&events, scope_id);
        for hash in closure_hashes(&events, &heads) {
            let stored = events.get(&hash).context("scope has missing history")?;
            ensure!(
                stored.status == "ready",
                "scope has incomplete or conflicting history"
            );
            for blob in &stored.event.blobs {
                ensure!(
                    has_blob_from(&tx, network_id, blob)?,
                    "scope has missing blob content"
                );
            }
        }
        let sequence:u64=tx.query_row("SELECT COALESCE(MAX(sequence),0)+1 FROM events WHERE network_id=? AND scope_id=? AND author=?",params![network_id,scope_id,author],|r|r.get(0))?;
        let previous: Option<String> = if sequence == 1 {
            None
        } else {
            Some(tx.query_row("SELECT hash FROM events WHERE network_id=? AND scope_id=? AND author=? AND sequence=? AND status='ready'",params![network_id,scope_id,author,sequence-1],|r|r.get(0))?)
        };
        for blob in &blobs {
            ensure!(
                has_blob_from(&tx, network_id, blob)?,
                "required blob missing"
            );
        }
        let envelope = identity.sign_event(Event {
            version: WIRE_VERSION,
            network_id: network_id.into(),
            scope_id: scope_id.into(),
            author: author.clone(),
            sequence,
            previous,
            parents: heads,
            created_at_ms,
            body,
            blobs,
        })?;
        tx.execute("INSERT INTO events(network_id,hash,scope_id,author,sequence,envelope,status) VALUES(?,?,?,?,?,?,'ready')",params![network_id,envelope.hash,scope_id,author,sequence,serde_json::to_string(&envelope)?])?;
        rebuild_projections_from(&tx, network_id)?;
        tx.commit()?;
        Ok(envelope)
    }
    fn load_events(&self, network_id: &str) -> Result<BTreeMap<String, StoredEvent>> {
        load_events_from(&self.connection, network_id)
    }
    pub fn event(&self, network_id: &str, hash: &str) -> Result<Option<SignedEvent>> {
        optional_json(
            self.connection
                .query_row(
                    "SELECT envelope FROM events WHERE network_id=? AND hash=?",
                    params![network_id, hash],
                    |r| r.get(0),
                )
                .optional()?,
        )
    }
    pub fn heads(&self, network_id: &str, scope_id: &str) -> Result<Vec<EventHash>> {
        Ok(heads_from_events(&self.load_events(network_id)?, scope_id))
    }
    pub fn missing_dependencies(
        &self,
        network_id: &str,
        heads: &[EventHash],
    ) -> Result<MissingDependencies> {
        let events = self.load_events(network_id)?;
        let mut pending = Vec::new();
        let mut visited = BTreeSet::new();
        let mut stack = heads.to_vec();
        let mut missing = MissingDependencies::default();
        while let Some(hash) = stack.pop() {
            if !visited.insert(hash.clone()) {
                continue;
            }
            let Some(entry) = events.get(&hash) else {
                missing.events.push(hash);
                continue;
            };
            if matches!(entry.status.as_str(), "conflict" | "invalid") {
                missing.conflicts.push(hash.clone());
            }
            if entry.status == "pending" {
                pending.push(hash.clone());
            }
            stack.extend(dependencies(&entry.event).cloned());
            for blob in &entry.event.blobs {
                if !self.has_blob(network_id, blob)? {
                    missing.blobs.push(blob.clone());
                }
            }
        }
        if missing.events.is_empty() && missing.blobs.is_empty() && missing.conflicts.is_empty() {
            missing.conflicts.extend(pending);
        }
        missing.events.sort();
        missing.events.dedup();
        missing.blobs.sort();
        missing.blobs.dedup();
        missing.conflicts.sort();
        missing.conflicts.dedup();
        Ok(missing)
    }
    pub fn context(
        &self,
        network_id: &str,
        scope_id: &str,
        basis_heads: &[EventHash],
    ) -> Result<Context> {
        let heads = if basis_heads.is_empty() {
            self.heads(network_id, scope_id)?
        } else {
            basis_heads.to_vec()
        };
        let missing = self.missing_dependencies(network_id, &heads)?;
        let events = self.load_events(network_id)?;
        let closure = closure_hashes(&events, &heads);
        for hash in &heads {
            if let Some(event) = events.get(hash) {
                ensure!(
                    event.event.scope_id == scope_id,
                    "basis head belongs to another scope"
                );
            }
        }
        let ordered = topological(&events, &closure);
        Ok(Context {
            network_id: network_id.into(),
            scope_id: scope_id.into(),
            heads,
            events: ordered
                .into_iter()
                .filter_map(|h| events.get(&h))
                .filter(|e| e.status == "ready")
                .map(|e| e.event.clone())
                .collect(),
            missing,
        })
    }
    pub fn export_snapshot(&self, network_id: &str, scope_id: &str) -> Result<Snapshot> {
        let events = self.load_events(network_id)?;
        let heads = self.heads(network_id, scope_id)?;
        let closure = closure_hashes(&events, &heads);
        Ok(Snapshot {
            version: WIRE_VERSION,
            network_id: network_id.into(),
            scope_id: scope_id.into(),
            heads,
            events: closure
                .iter()
                .filter_map(|h| events.get(h))
                .map(|e| e.envelope.clone())
                .collect(),
        })
    }
    pub fn import_snapshot(
        &mut self,
        network_id: &str,
        snapshot: &Snapshot,
    ) -> Result<MissingDependencies> {
        ensure!(
            snapshot.version == WIRE_VERSION && snapshot.network_id == network_id,
            "invalid snapshot network/version"
        );
        for event in &snapshot.events {
            self.verify_event(network_id, event)?;
        }
        for event in &snapshot.events {
            self.ingest_event(network_id, event)?;
        }
        for hash in &snapshot.heads {
            if let Some(event) = self.event(network_id, hash)? {
                let body: Event = serde_json::from_str(&event.canonical_body)?;
                ensure!(
                    body.scope_id == snapshot.scope_id,
                    "snapshot head scope mismatch"
                );
            }
        }
        self.missing_dependencies(network_id, &snapshot.heads)
    }
    pub fn put_blob(&mut self, network_id: &str, expected: &BlobRef, content: &[u8]) -> Result<()> {
        ensure!(
            content.len() <= MAX_BLOB_BYTES && content.len() as u64 == expected.size,
            "blob size mismatch or limit exceeded"
        );
        ensure!(digest(content) == expected.hash, "blob hash mismatch");
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute(
            "INSERT INTO blobs(network_id,hash,size,content) VALUES(?,?,?,?) ON CONFLICT(network_id,hash) DO UPDATE SET size=excluded.size,content=excluded.content",
            params![network_id, expected.hash, expected.size, content],
        )?;
        rebuild_projections_from(&tx, network_id)?;
        tx.commit()?;
        Ok(())
    }
    pub fn get_blob(&self, network_id: &str, hash: &str) -> Result<Option<Vec<u8>>> {
        let content: Option<Vec<u8>> = self
            .connection
            .query_row(
                "SELECT content FROM blobs WHERE network_id=? AND hash=?",
                params![network_id, hash],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(content) = &content {
            ensure!(
                content.len() <= MAX_BLOB_BYTES && digest(content) == hash,
                "stored blob failed content verification"
            );
        }
        Ok(content)
    }
    pub fn has_blob(&self, network_id: &str, blob: &BlobRef) -> Result<bool> {
        has_blob_from(&self.connection, network_id, blob)
    }
    pub fn rebuild_projections(&mut self, network_id: &str) -> Result<()> {
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        rebuild_projections_from(&tx, network_id)?;
        tx.commit()?;
        Ok(())
    }
    pub fn conversations(&self, network_id: &str) -> Result<Vec<Conversation>> {
        let mut statement=self.connection.prepare("SELECT conversation_id,title FROM conversations WHERE network_id=? ORDER BY conversation_id")?;
        let rows = statement.query_map([network_id], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        })?;
        let mut result = Vec::new();
        for row in rows {
            let (conversation_id, title) = row?;
            let heads = self.heads(network_id, &conversation_id)?;
            result.push(Conversation {
                conversation_id,
                title,
                heads,
            });
        }
        Ok(result)
    }
    pub fn messages(&self, network_id: &str, scope_id: &str) -> Result<Vec<Message>> {
        let mut statement=self.connection.prepare("SELECT message_id,role,text,event_hash,created_at_ms FROM messages WHERE network_id=? AND conversation_id=? ORDER BY ordinal")?;
        Ok(statement
            .query_map(params![network_id, scope_id], |r| {
                Ok(Message {
                    message_id: r.get(0)?,
                    role: r.get(1)?,
                    text: r.get(2)?,
                    event_hash: r.get(3)?,
                    created_at_ms: r.get(4)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?)
    }
    pub fn tasks(&self, network_id: &str, scope_id: &str) -> Result<Vec<Task>> {
        let mut statement=self.connection.prepare("SELECT task_id,conversation_id,title,goal,acceptance,status,detail,event_hash FROM tasks WHERE network_id=? AND conversation_id=? ORDER BY task_id")?;
        Ok(statement
            .query_map(params![network_id, scope_id], |r| {
                Ok(Task {
                    task_id: r.get(0)?,
                    conversation_id: r.get(1)?,
                    title: r.get(2)?,
                    goal: r.get(3)?,
                    acceptance: r.get(4)?,
                    status: r.get(5)?,
                    detail: r.get(6)?,
                    event_hash: r.get(7)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?)
    }
    pub fn local_value(&self, network_id: &str, key: &str) -> Result<Option<Value>> {
        optional_json(
            self.connection
                .query_row(
                    "SELECT value FROM local_values WHERE network_id=? AND key=?",
                    params![network_id, key],
                    |r| r.get(0),
                )
                .optional()?,
        )
    }
    pub fn local_set(&mut self, network_id: &str, key: &str, value: &Value) -> Result<()> {
        self.connection.execute("INSERT INTO local_values(network_id,key,value) VALUES(?,?,?) ON CONFLICT(network_id,key) DO UPDATE SET value=excluded.value",params![network_id,key,serde_json::to_string(value)?])?;
        Ok(())
    }
    pub fn local_delete(&mut self, network_id: &str, key: &str) -> Result<()> {
        self.connection.execute(
            "DELETE FROM local_values WHERE network_id=? AND key=?",
            params![network_id, key],
        )?;
        Ok(())
    }
    pub fn session(
        &self,
        network_id: &str,
        target_node: &str,
        session_id: &str,
    ) -> Result<Option<ExecutionSession>> {
        optional_json(self.connection.query_row("SELECT record FROM execution_sessions WHERE network_id=? AND target_node=? AND session_id=?",params![network_id,target_node,session_id],|r|r.get(0)).optional()?)
    }
    pub fn sessions(&self, network_id: &str) -> Result<Vec<ExecutionSession>> {
        let mut statement=self.connection.prepare("SELECT record FROM execution_sessions WHERE network_id=? ORDER BY target_node,session_id")?;
        let rows = statement.query_map([network_id], |r| r.get::<_, String>(0))?;
        rows.map(|r| Ok(serde_json::from_str(&r?)?)).collect()
    }
    pub fn upsert_session(&mut self, network_id: &str, session: &ExecutionSession) -> Result<()> {
        ensure!(session.network_id == network_id, "session network mismatch");
        ensure!(
            session.revision <= i64::MAX as u64,
            "session revision overflow"
        );
        if let Some(old) = self.session(network_id, &session.target_node, &session.session_id)? {
            ensure!(
                session.revision >= old.revision,
                "session revision rollback"
            );
        }
        let changed=self.connection.execute("INSERT INTO execution_sessions(network_id,target_node,session_id,revision,record) VALUES(?,?,?,?,?) ON CONFLICT(network_id,target_node,session_id) DO UPDATE SET revision=excluded.revision,record=excluded.record WHERE excluded.revision>=execution_sessions.revision",params![network_id,session.target_node,session.session_id,session.revision,serde_json::to_string(session)?])?;
        ensure!(changed == 1, "session revision rollback");
        Ok(())
    }
    pub fn operation(
        &self,
        network_id: &str,
        operation_id: &str,
    ) -> Result<Option<OperationRecord>> {
        let row: Option<(String, String, String)> = self
            .connection
            .query_row(
                "SELECT request,state,result FROM operations WHERE network_id=? AND operation_id=?",
                params![network_id, operation_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        row.map(|(request, state, result)| {
            Ok(OperationRecord {
                request: serde_json::from_str(&request)?,
                state: serde_json::from_str(&state)?,
                result: serde_json::from_str(&result)?,
            })
        })
        .transpose()
    }
    pub fn operations(&self, network_id: &str) -> Result<Vec<OperationRecord>> {
        let mut statement = self.connection.prepare(
            "SELECT operation_id FROM operations WHERE network_id=? ORDER BY operation_id",
        )?;
        let ids = statement
            .query_map([network_id], |r| r.get::<_, String>(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        ids.into_iter()
            .map(|id| {
                self.operation(network_id, &id)?
                    .context("operation disappeared")
            })
            .collect()
    }
    pub fn task_creation(&self, network: &str, scope: &str, task_id: &str) -> Result<SignedEvent> {
        let context = self.context(network, scope, &[])?;
        ensure!(
            context.missing.is_complete(),
            "task history is incomplete or conflicting"
        );
        let matches = context.events.iter().filter(|event| {
            event.scope_id == scope && matches!(&event.body, EventBody::TaskCreated { task_id: id, .. } if id == task_id)
        }).collect::<Vec<_>>();
        ensure!(matches.len() == 1, "task creation is missing or ambiguous");
        let hash = digest(&serde_json::to_vec(matches[0])?);
        self.event(network, &hash)?
            .context("task creation envelope is missing")
    }

    fn verify_binding_creation(&self, proof: &crate::binding::SignedBinding) -> Result<()> {
        let binding = &proof.binding;
        let key = self.public_key(&binding.network_id, &binding.authority_node)?;
        crate::binding::verify_binding(proof, &key)?;
        self.verify_binding_task(binding)
    }

    fn verify_binding_task(&self, binding: &crate::binding::TaskBinding) -> Result<()> {
        crate::binding::validate_binding(binding)?;
        let envelope = self
            .event(&binding.network_id, &binding.task_ref)?
            .context("task creation is missing")?;
        let creation = self.verify_event(&binding.network_id, &envelope)?;
        ensure!(
            creation.author == binding.authority_node
                && creation.scope_id == binding.conversation_id,
            "binding authority does not match signed task creation"
        );
        ensure!(
            matches!(&creation.body, EventBody::TaskCreated { task_id, .. } if task_id == &binding.task_id),
            "binding references a different task"
        );
        ensure!(
            self.missing_dependencies(
                &binding.network_id,
                std::slice::from_ref(&binding.task_ref)
            )?
            .is_complete(),
            "binding task history is incomplete or conflicting"
        );
        self.authorize_node(
            &binding.network_id,
            &binding.target_node,
            Some(NodeRole::Executor),
        )?;
        if let Some(seal) = &binding.previous_seal {
            let previous = binding
                .previous_binding_hash
                .as_ref()
                .context("missing previous binding")?;
            let value = self
                .local_value(
                    &binding.network_id,
                    &format!("task-binding-record:{previous}"),
                )?
                .context("previous binding is missing")?;
            let prior: crate::binding::TaskBinding =
                serde_json::from_value(value["binding"].clone())?;
            ensure!(
                crate::binding::binding_hash(&prior)? == *previous
                    && prior.task_ref == binding.task_ref
                    && prior.generation.checked_add(1) == Some(binding.generation),
                "binding history does not extend previous generation"
            );
            let key = self.public_key(&binding.network_id, &prior.target_node)?;
            crate::binding::verify_seal(seal, &key)?;
            ensure!(
                seal.seal.network_id == binding.network_id
                    && seal.seal.binding_hash == *previous
                    && seal.seal.target_node == prior.target_node,
                "seal does not close previous binding"
            );
        }
        Ok(())
    }

    pub fn task_binding(
        &self,
        network: &str,
        task_ref: &str,
    ) -> Result<Option<crate::binding::SignedBinding>> {
        let Some(value) = self.local_value(network, &format!("task-binding-current:{task_ref}"))?
        else {
            return Ok(None);
        };
        let hash = value.as_str().context("invalid binding pointer")?;
        let record = self
            .local_value(network, &format!("task-binding-record:{hash}"))?
            .context("missing binding record")?;
        if record["signature"].is_null() {
            return Ok(None);
        }
        Ok(Some(serde_json::from_value(record)?))
    }

    pub fn bind_task(
        &mut self,
        identity: &LocalIdentity,
        binding: &crate::binding::TaskBinding,
    ) -> Result<crate::binding::SignedBinding> {
        ensure!(
            binding.authority_node == identity.node_id()?,
            "this Node is not the task binding authority"
        );
        let authority = self.authorize_node(&binding.network_id, &binding.authority_node, None)?;
        ensure!(
            authority.roles.contains(&NodeRole::Gateway)
                || authority.roles.contains(&NodeRole::Scheduler),
            "task authority cannot bind execution"
        );
        self.verify_binding_task(binding)?;
        let hash = crate::binding::binding_hash(binding)?;
        let record_key = format!("task-binding-record:{hash}");
        let current_key = format!("task-binding-current:{}", binding.task_ref);
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let ready: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM events WHERE network_id=? AND hash=? AND status='ready')",
            params![binding.network_id, binding.task_ref],
            |r| r.get(0),
        )?;
        ensure!(ready, "task creation changed before binding confirmation");
        let current: Option<String> = tx
            .query_row(
                "SELECT value FROM local_values WHERE network_id=? AND key=?",
                params![binding.network_id, current_key],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(current) = current {
            let current: String = serde_json::from_str(&current)?;
            if current != hash {
                ensure!(
                    binding.previous_binding_hash.as_ref() == Some(&current),
                    "task is already bound to another execution; query its existing binding"
                );
                ensure!(
                    binding.previous_seal.is_some(),
                    "changing executor requires a sealed previous binding"
                );
            }
        } else {
            ensure!(
                binding.generation == 1 && binding.previous_binding_hash.is_none(),
                "initial binding must be generation one"
            );
        }
        let unsigned = serde_json::json!({"binding":binding,"signature":null});
        tx.execute(
            "INSERT OR IGNORE INTO local_values(network_id,key,value) VALUES(?,?,?)",
            params![
                binding.network_id,
                record_key,
                serde_json::to_string(&unsigned)?
            ],
        )?;
        tx.execute("INSERT INTO local_values(network_id,key,value) VALUES(?,?,?) ON CONFLICT(network_id,key) DO UPDATE SET value=excluded.value",params![binding.network_id,current_key,serde_json::to_string(&hash)?])?;
        tx.commit()?;
        // No binding proof leaves this Node before its unique value is durable.
        let proof = crate::binding::sign_binding(identity, binding)?;
        self.local_set(
            &binding.network_id,
            &record_key,
            &serde_json::to_value(&proof)?,
        )?;
        Ok(proof)
    }

    pub fn install_binding(&mut self, proof: &crate::binding::SignedBinding) -> Result<()> {
        self.verify_binding_creation(proof)?;
        let binding = &proof.binding;
        let hash = crate::binding::binding_hash(binding)?;
        let current_key = format!("task-binding-current:{}", binding.task_ref);
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let current: Option<String> = tx
            .query_row(
                "SELECT value FROM local_values WHERE network_id=? AND key=?",
                params![binding.network_id, current_key],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(current) = current {
            let current: String = serde_json::from_str(&current)?;
            if current != hash {
                let record: String = tx.query_row(
                    "SELECT value FROM local_values WHERE network_id=? AND key=?",
                    params![binding.network_id, format!("task-binding-record:{current}")],
                    |r| r.get(0),
                )?;
                let previous: crate::binding::TaskBinding = serde_json::from_value(
                    serde_json::from_str::<Value>(&record)?["binding"].clone(),
                )?;
                ensure!(
                    binding.generation != previous.generation,
                    "conflicting signed binding generation"
                );
                if binding.generation < previous.generation {
                    // Historical proof may be retained, but must never move the current fence backwards.
                    tx.execute(
                        "INSERT OR IGNORE INTO local_values(network_id,key,value) VALUES(?,?,?)",
                        params![
                            binding.network_id,
                            format!("task-binding-record:{hash}"),
                            serde_json::to_string(proof)?
                        ],
                    )?;
                    tx.commit()?;
                    return Ok(());
                }
                ensure!(
                    binding.previous_binding_hash.as_ref() == Some(&current),
                    "binding generation has missing predecessors"
                );
            }
        }
        tx.execute("INSERT INTO local_values(network_id,key,value) VALUES(?,?,?) ON CONFLICT(network_id,key) DO UPDATE SET value=excluded.value",params![binding.network_id,format!("task-binding-record:{hash}"),serde_json::to_string(proof)?])?;
        tx.execute("INSERT INTO local_values(network_id,key,value) VALUES(?,?,?) ON CONFLICT(network_id,key) DO UPDATE SET value=excluded.value",params![binding.network_id,current_key,serde_json::to_string(&hash)?])?;
        if let Some(seal) = &binding.previous_seal {
            tx.execute(
                "INSERT OR IGNORE INTO local_values(network_id,key,value) VALUES(?,?,?)",
                params![
                    binding.network_id,
                    format!("task-binding-seal:{}", seal.seal.binding_hash),
                    serde_json::to_string(seal)?
                ],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn seal_binding(
        &mut self,
        identity: &LocalIdentity,
        proof: &crate::binding::SignedBinding,
    ) -> Result<crate::binding::SignedSeal> {
        self.verify_binding_creation(proof)?;
        let binding = &proof.binding;
        ensure!(
            binding.target_node == identity.node_id()?,
            "only the bound executor can seal execution"
        );
        let hash = crate::binding::binding_hash(binding)?;
        let key = format!("task-binding-seal:{hash}");
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let existing: Option<String> = tx
            .query_row(
                "SELECT value FROM local_values WHERE network_id=? AND key=?",
                params![binding.network_id, key],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(existing) = existing {
            let value: Value = serde_json::from_str(&existing)?;
            let seal: crate::binding::BindingSeal = serde_json::from_value(value["seal"].clone())?;
            tx.commit()?;
            let signed = crate::binding::sign_seal(identity, &seal)?;
            self.local_set(&binding.network_id, &key, &serde_json::to_value(&signed)?)?;
            return Ok(signed);
        }
        let busy:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM operations WHERE network_id=? AND target_node=? AND session_id=? AND state IN ('\"accepted\"','\"running\"','\"unknown\"'))",params![binding.network_id,binding.target_node,binding.session_id],|r|r.get(0))?;
        ensure!(
            !busy,
            "cannot seal an unresolved execution; reconcile unknown results first"
        );
        let pending = {
            let mut statement=tx.prepare("SELECT o.operation_id,o.state FROM operations o JOIN local_values v ON v.network_id=o.network_id AND v.key='completion:' || o.operation_id WHERE o.network_id=? AND o.target_node=? AND o.session_id=?")?;
            statement
                .query_map(
                    params![binding.network_id, binding.target_node, binding.session_id],
                    |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
                )?
                .collect::<std::result::Result<Vec<_>, _>>()?
        };
        if !pending.is_empty() {
            let events = load_events_from(&tx, &binding.network_id)?;
            for (operation, state) in pending {
                let state: OperationState = serde_json::from_str(&state)?;
                let expected = serde_json::to_value(state)?
                    .as_str()
                    .context("invalid operation state")?
                    .to_owned();
                ensure!(events.values().any(|stored|stored.status == "ready" && stored.event.author == binding.target_node
                    && stored.event.scope_id == binding.conversation_id
                    && matches!(&stored.event.body,EventBody::ExecutionObserved{operation_id,target_node,session_id,status,..}
                        if operation_id == &operation && target_node == &binding.target_node && session_id == &binding.session_id && status == &expected)),
                    "execution completion must be durable before sealing");
            }
        }
        let session:Option<String>=tx.query_row("SELECT record FROM execution_sessions WHERE network_id=? AND target_node=? AND session_id=?",params![binding.network_id,binding.target_node,binding.session_id],|r|r.get(0)).optional()?;
        let revision = if let Some(session) = session {
            let session: ExecutionSession = serde_json::from_str(&session)?;
            ensure!(
                matches!(session.state.as_str(), "idle" | "closed"),
                "native execution session is not idle"
            );
            session.revision
        } else {
            0
        };
        let seal = crate::binding::BindingSeal {
            network_id: binding.network_id.clone(),
            binding_hash: hash,
            target_node: binding.target_node.clone(),
            final_revision: revision,
        };
        tx.execute(
            "INSERT INTO local_values(network_id,key,value) VALUES(?,?,?)",
            params![
                binding.network_id,
                key,
                serde_json::to_string(&serde_json::json!({"seal":seal,"signature":null}))?
            ],
        )?;
        tx.commit()?;
        let signed = crate::binding::sign_seal(identity, &seal)?;
        self.local_set(&binding.network_id, &key, &serde_json::to_value(&signed)?)?;
        Ok(signed)
    }

    pub fn validate_operation_binding(&self, request: &OperationRequest) -> Result<()> {
        let proof = request
            .binding
            .as_ref()
            .context("execution requires a confirmed task binding")?;
        self.verify_binding_creation(proof)?;
        validate_operation_binding_from(&self.connection, request)
    }

    #[cfg(test)]
    pub(crate) fn bind_test_operation(
        &mut self,
        identity: &LocalIdentity,
        request: &mut OperationRequest,
    ) -> Result<()> {
        let task_id = request
            .task_id
            .get_or_insert_with(|| format!("fixture-{}", request.operation_id))
            .clone();
        let creation =
            match self.task_creation(&request.network_id, &request.conversation_id, &task_id) {
                Ok(event) => event,
                Err(_) => self.append_event(
                    identity,
                    &request.network_id,
                    &request.conversation_id,
                    EventBody::TaskCreated {
                        task_id: task_id.clone(),
                        title: "fixture".into(),
                        goal: "fixture".into(),
                        acceptance: "fixture".into(),
                    },
                    vec![],
                    2,
                )?,
            };
        if let Some(proof) = self.task_binding(&request.network_id, &creation.hash)? {
            request.binding = Some(proof);
            return Ok(());
        }
        let open_spec =
            if request.action["type"] == "open" && request.action["executor"].is_string() {
                request.action.clone()
            } else if let Some(session) = self.session(
                &request.network_id,
                &request.target_node,
                &request.session_id,
            )? {
                serde_json::json!({"type":"open","executor":session.executor,"cwd":session.cwd})
            } else {
                serde_json::json!({"type":"open","executor":"codex","cwd":"/tmp"})
            };
        let binding = crate::binding::TaskBinding {
            network_id: request.network_id.clone(),
            conversation_id: request.conversation_id.clone(),
            task_ref: creation.hash,
            task_id,
            authority_node: identity.node_id()?,
            generation: 1,
            target_node: request.target_node.clone(),
            session_id: request.session_id.clone(),
            open_spec,
            previous_binding_hash: None,
            previous_seal: None,
        };
        request.binding = Some(self.bind_task(identity, &binding)?);
        Ok(())
    }

    pub fn accept_operation(
        &mut self,
        network_id: &str,
        request: &OperationRequest,
    ) -> Result<OperationRecord> {
        self.admit_operation(network_id, request, false)
    }
    pub fn accept_control_operation(
        &mut self,
        network_id: &str,
        request: &OperationRequest,
    ) -> Result<OperationRecord> {
        ensure!(
            request.resources.is_empty(),
            "control operations cannot reserve resources"
        );
        ensure!(
            matches!(
                request.action.get("type").and_then(Value::as_str),
                Some("read" | "steer" | "interrupt" | "respond_request")
            ),
            "invalid control operation action"
        );
        self.admit_operation(network_id, request, true)
    }
    fn admit_operation(
        &mut self,
        network_id: &str,
        request: &OperationRequest,
        control: bool,
    ) -> Result<OperationRecord> {
        ensure!(
            request.network_id == network_id,
            "operation network mismatch"
        );
        ensure!(
            !request.operation_id.is_empty() && !request.session_id.is_empty(),
            "operation and session IDs are required"
        );
        ensure!(
            request.expected_revision < i64::MAX as u64,
            "revision overflow"
        );
        ensure!(
            request.resources.len() <= 128,
            "too many resource reservations"
        );
        self.validate_operation_binding(request)?;
        let encoded = serde_json::to_string(request)?;
        ensure!(encoded.len() <= MAX_EVENT_BYTES, "operation too large");
        if let Some(old) = self.operation(network_id, &request.operation_id)? {
            ensure!(
                old.request == *request,
                "operation ID reused with different body"
            );
            return Ok(old);
        }
        self.authorize_node(network_id, &request.target_node, Some(NodeRole::Executor))?;
        let complete = self.context(network_id, &request.conversation_id, &request.basis_heads)?;
        ensure!(
            complete.missing.is_complete(),
            "required context is incomplete or conflicting"
        );
        let mut resources = request
            .resources
            .iter()
            .map(|r| canonical_resource(r))
            .collect::<Result<Vec<_>>>()?;
        resources.sort();
        resources.dedup();
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let old: Option<(String, String, String)> = tx
            .query_row(
                "SELECT request,state,result FROM operations WHERE network_id=? AND operation_id=?",
                params![network_id, request.operation_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        if let Some((old, state, result)) = old {
            ensure!(old == encoded, "operation ID reused with different body");
            return Ok(OperationRecord {
                request: request.clone(),
                state: serde_json::from_str(&state)?,
                result: serde_json::from_str(&result)?,
            });
        }
        validate_operation_binding_from(&tx, request)?;
        for event in &complete.events {
            let hash = digest(serde_json::to_string(event)?.as_bytes());
            let ready:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM events WHERE network_id=? AND hash=? AND status='ready')",params![network_id,hash],|r|r.get(0))?;
            ensure!(ready, "context changed during admission");
        }
        let revision:Option<u64>=tx.query_row("SELECT revision FROM execution_sessions WHERE network_id=? AND target_node=? AND session_id=?",params![network_id,request.target_node,request.session_id],|r|r.get(0)).optional()?;
        ensure!(
            revision.unwrap_or(0) == request.expected_revision,
            "stale execution session revision"
        );
        let busy:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM operations WHERE network_id=? AND target_node=? AND session_id=? AND state IN ('\"accepted\"','\"running\"','\"unknown\"') AND (?=0 OR control=1))",params![network_id,request.target_node,request.session_id,control],|r|r.get(0))?;
        ensure!(!busy, "execution session has an unresolved operation");
        {
            let mut statement =
                tx.prepare("SELECT resource FROM resource_reservations WHERE target_node=?")?;
            let existing =
                statement.query_map(params![request.target_node], |r| r.get::<_, String>(0))?;
            for row in existing {
                let existing = row?;
                ensure!(
                    !resources.iter().any(|r| paths_overlap(r, &existing)),
                    "resource overlaps an unresolved operation on this device"
                );
            }
        }
        tx.execute("INSERT INTO operations(network_id,operation_id,target_node,session_id,request,state,control,result) VALUES(?,?,?,?,?,'\"accepted\"',?,'null')",params![network_id,request.operation_id,request.target_node,request.session_id,encoded,control])?;
        for resource in resources {
            tx.execute("INSERT INTO resource_reservations(network_id,operation_id,target_node,resource) VALUES(?,?,?,?)",params![network_id,request.operation_id,request.target_node,resource])?;
        }
        tx.commit()?;
        Ok(OperationRecord {
            request: request.clone(),
            state: OperationState::Accepted,
            result: Value::Null,
        })
    }
    pub fn claim_operation(
        &mut self,
        network_id: &str,
        operation_id: &str,
    ) -> Result<OperationRecord> {
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let encoded: String = tx.query_row(
            "SELECT request FROM operations WHERE network_id=? AND operation_id=?",
            params![network_id, operation_id],
            |r| r.get(0),
        )?;
        let request: OperationRequest = serde_json::from_str(&encoded)?;
        validate_operation_binding_from(&tx, &request)?;
        authorize_node_from(
            &tx,
            network_id,
            &request.target_node,
            Some(NodeRole::Executor),
        )?;
        let revision:Option<u64>=tx.query_row("SELECT revision FROM execution_sessions WHERE network_id=? AND target_node=? AND session_id=?",params![network_id,request.target_node,request.session_id],|r|r.get(0)).optional()?;
        ensure!(
            revision.unwrap_or(0) == request.expected_revision,
            "execution session changed before claim"
        );
        let events = load_events_from(&tx, network_id)?;
        let heads = if request.basis_heads.is_empty() {
            heads_from_events(&events, &request.conversation_id)
        } else {
            request.basis_heads.clone()
        };
        for hash in closure_hashes(&events, &heads) {
            let event = events
                .get(&hash)
                .context("operation basis is missing before claim")?;
            ensure!(
                event.status == "ready",
                "operation basis became incomplete or conflicting"
            );
            for blob in &event.event.blobs {
                ensure!(
                    has_blob_from(&tx, network_id, blob)?,
                    "operation basis content became incomplete"
                );
            }
        }
        let changed=tx.execute("UPDATE operations SET state='\"running\"' WHERE network_id=? AND operation_id=? AND state='\"accepted\"'",params![network_id,operation_id])?;
        ensure!(
            changed == 1,
            "operation is not accepted or has already been claimed"
        );
        tx.commit()?;
        self.operation(network_id, operation_id)?
            .context("operation disappeared")
    }
    pub fn finish_operation(
        &mut self,
        network_id: &str,
        operation_id: &str,
        state: OperationState,
        result: Value,
    ) -> Result<OperationRecord> {
        ensure!(
            state.terminal() || state == OperationState::Unknown,
            "invalid operation completion state"
        );
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let row: Option<(String, String, String)> = tx
            .query_row(
                "SELECT request,state,result FROM operations WHERE network_id=? AND operation_id=?",
                params![network_id, operation_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        let (request, old_state, old_result) = row.context("unknown operation")?;
        let request: OperationRequest = serde_json::from_str(&request)?;
        let old_state: OperationState = serde_json::from_str(&old_state)?;
        if old_state.terminal() {
            ensure!(
                old_state == state && serde_json::from_str::<Value>(&old_result)? == result,
                "terminal operation cannot be changed"
            );
            return Ok(OperationRecord {
                request,
                state,
                result,
            });
        }
        ensure!(
            matches!(old_state, OperationState::Running | OperationState::Unknown)
                || (old_state == OperationState::Accepted && state == OperationState::Rejected),
            "operation has not started"
        );
        tx.execute(
            "UPDATE operations SET state=?,result=? WHERE network_id=? AND operation_id=?",
            params![
                serde_json::to_string(&state)?,
                serde_json::to_string(&result)?,
                network_id,
                operation_id
            ],
        )?;
        if state.terminal() {
            tx.execute(
                "DELETE FROM resource_reservations WHERE network_id=? AND operation_id=?",
                params![network_id, operation_id],
            )?;
            let session:Option<String>=tx.query_row("SELECT record FROM execution_sessions WHERE network_id=? AND target_node=? AND session_id=?",params![network_id,request.target_node,request.session_id],|r|r.get(0)).optional()?;
            if let Some(session) = session {
                let mut session: ExecutionSession = serde_json::from_str(&session)?;
                session.revision = session.revision.max(request.expected_revision + 1);
                tx.execute("UPDATE execution_sessions SET revision=?,record=? WHERE network_id=? AND target_node=? AND session_id=?",params![session.revision,serde_json::to_string(&session)?,network_id,request.target_node,request.session_id])?;
            }
        }
        tx.commit()?;
        Ok(OperationRecord {
            request,
            state,
            result,
        })
    }
    pub fn recover_running(&mut self) -> Result<usize> {
        let count = self.connection.execute(
            "UPDATE operations SET state='\"unknown\"' WHERE state='\"running\"'",
            [],
        )?;
        self.connection.execute("UPDATE continuation_claims SET record=json_set(record,'$.state','unknown') WHERE json_extract(record,'$.state')='running'",[])?;
        Ok(count)
    }
    pub fn claim_continuation(
        &mut self,
        network_id: &str,
        result_event_hash: &str,
        policy_version: &str,
        scheduler_node: &str,
    ) -> Result<ContinuationClaim> {
        ensure!(!policy_version.is_empty(), "policy version is required");
        self.authorize_node(network_id, scheduler_node, Some(NodeRole::Scheduler))?;
        let event = self
            .event(network_id, result_event_hash)?
            .context("result event is missing")?;
        let event = self.verify_event(network_id, &event)?;
        ensure!(
            matches!(event.body, EventBody::ExecutionObserved { .. }),
            "continuation requires an execution result"
        );
        ensure!(
            self.local_identity(network_id, &event.author)?.is_some(),
            "continuation must be claimed at its event-producing Node"
        );
        ensure!(
            self.missing_dependencies(network_id, &[result_event_hash.into()])?
                .is_complete(),
            "result history incomplete"
        );
        let claim = ContinuationClaim {
            result_event_hash: result_event_hash.into(),
            policy_version: policy_version.into(),
            scheduler_node: scheduler_node.into(),
            state: "running".into(),
            result: Value::Null,
        };
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        authorize_node_from(&tx, network_id, scheduler_node, Some(NodeRole::Scheduler))?;
        authorize_node_from(&tx, network_id, &event.author, None)?;
        let changed=tx.execute("INSERT OR IGNORE INTO continuation_claims(network_id,result_event_hash,policy_version,record) VALUES(?,?,?,?)",params![network_id,result_event_hash,policy_version,serde_json::to_string(&claim)?])?;
        ensure!(
            changed == 1,
            "continuation has already been claimed; reconcile its original attempt"
        );
        tx.commit()?;
        Ok(claim)
    }
    pub fn continuation(
        &self,
        network_id: &str,
        result_event_hash: &str,
        policy_version: &str,
    ) -> Result<Option<ContinuationClaim>> {
        optional_json(self.connection.query_row("SELECT record FROM continuation_claims WHERE network_id=? AND result_event_hash=? AND policy_version=?",params![network_id,result_event_hash,policy_version],|r|r.get(0)).optional()?)
    }
    pub fn finish_continuation(
        &mut self,
        network_id: &str,
        result_event_hash: &str,
        policy_version: &str,
        scheduler_node: &str,
        state: &str,
        result: Value,
    ) -> Result<ContinuationClaim> {
        ensure!(
            matches!(state, "completed" | "failed" | "unknown"),
            "invalid continuation completion"
        );
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let record:String=tx.query_row("SELECT record FROM continuation_claims WHERE network_id=? AND result_event_hash=? AND policy_version=?",params![network_id,result_event_hash,policy_version],|r|r.get(0))?;
        let mut claim: ContinuationClaim = serde_json::from_str(&record)?;
        ensure!(
            claim.scheduler_node == scheduler_node,
            "continuation belongs to a different processing attempt"
        );
        if matches!(claim.state.as_str(), "completed" | "failed") {
            ensure!(
                claim.state == state && claim.result == result,
                "completed continuation cannot change"
            );
            return Ok(claim);
        }
        claim.state = state.into();
        claim.result = result;
        tx.execute("UPDATE continuation_claims SET record=? WHERE network_id=? AND result_event_hash=? AND policy_version=?",params![serde_json::to_string(&claim)?,network_id,result_event_hash,policy_version])?;
        tx.commit()?;
        Ok(claim)
    }
    pub fn queue_notification(
        &mut self,
        network_id: &str,
        notification: &Notification,
    ) -> Result<()> {
        ensure!(
            !notification.notification_id.is_empty(),
            "notification ID required"
        );
        let encoded = serde_json::to_string(notification)?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let old: Option<String> = tx
            .query_row(
                "SELECT record FROM notification_outbox WHERE network_id=? AND notification_id=?",
                params![network_id, notification.notification_id],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(old) = old {
            ensure!(old == encoded, "notification ID reused with different body");
        } else {
            tx.execute(
                "INSERT INTO notification_outbox(network_id,notification_id,record) VALUES(?,?,?)",
                params![network_id, notification.notification_id, encoded],
            )?;
        }
        tx.commit()?;
        Ok(())
    }
    pub fn pending_notifications(
        &self,
        network_id: &str,
        client_id: &str,
    ) -> Result<Vec<Notification>> {
        let mut statement=self.connection.prepare("SELECT n.record FROM notification_outbox n WHERE n.network_id=? AND NOT EXISTS(SELECT 1 FROM notification_acks a WHERE a.network_id=n.network_id AND a.notification_id=n.notification_id AND a.client_id=?) ORDER BY n.notification_id")?;
        let rows =
            statement.query_map(params![network_id, client_id], |r| r.get::<_, String>(0))?;
        rows.map(|r| Ok(serde_json::from_str(&r?)?)).collect()
    }
    pub fn ack_notification(
        &mut self,
        network_id: &str,
        notification_id: &str,
        client_id: &str,
    ) -> Result<()> {
        ensure!(!client_id.is_empty(), "client ID is required");
        let exists:bool=self.connection.query_row("SELECT EXISTS(SELECT 1 FROM notification_outbox WHERE network_id=? AND notification_id=?)",params![network_id,notification_id],|r|r.get(0))?;
        ensure!(exists, "unknown notification");
        self.connection.execute("INSERT INTO notification_acks(network_id,notification_id,client_id,event_hash) VALUES(?,?,?,'') ON CONFLICT(network_id,notification_id,client_id) DO UPDATE SET event_hash=''",params![network_id,notification_id,client_id])?;
        Ok(())
    }
    pub fn budget_usage(&self, network_id: &str, day: &str, limit: u64) -> Result<BudgetUsage> {
        validate_day(day)?;
        let used = self
            .connection
            .query_row(
                "SELECT used FROM budget_days WHERE network_id=? AND day=?",
                params![network_id, day],
                |r| r.get(0),
            )
            .optional()?
            .unwrap_or(0);
        Ok(BudgetUsage {
            day: day.into(),
            used,
            limit,
        })
    }
    pub fn reserve_budget(
        &mut self,
        network_id: &str,
        day: &str,
        units: u64,
        limit: u64,
    ) -> Result<BudgetUsage> {
        validate_day(day)?;
        ensure!(
            units > 0 && limit <= i64::MAX as u64,
            "invalid budget units/limit"
        );
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let used: u64 = tx
            .query_row(
                "SELECT used FROM budget_days WHERE network_id=? AND day=?",
                params![network_id, day],
                |r| r.get(0),
            )
            .optional()?
            .unwrap_or(0);
        let next = used.checked_add(units).context("budget overflow")?;
        ensure!(next <= limit, "daily model budget exhausted");
        tx.execute("INSERT INTO budget_days(network_id,day,used) VALUES(?,?,?) ON CONFLICT(network_id,day) DO UPDATE SET used=excluded.used",params![network_id,day,next])?;
        tx.commit()?;
        Ok(BudgetUsage {
            day: day.into(),
            used: next,
            limit,
        })
    }
    pub fn networks(&self) -> Result<Vec<NetworkId>> {
        let mut statement = self
            .connection
            .prepare("SELECT network_id FROM networks ORDER BY network_id")?;
        Ok(statement
            .query_map([], |r| r.get(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?)
    }
    pub fn scopes(&self, network_id: &str) -> Result<Vec<ScopeId>> {
        let mut statement = self
            .connection
            .prepare("SELECT DISTINCT scope_id FROM events WHERE network_id=? ORDER BY scope_id")?;
        Ok(statement
            .query_map([network_id], |r| r.get(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?)
    }
}
fn rebuild_projections_from(connection: &Connection, network_id: &str) -> Result<()> {
    let mut events = load_events_from(connection, network_id)?;
    let mut author_sequences: BTreeMap<(String, String, u64), Vec<String>> = BTreeMap::new();
    for (hash, e) in &events {
        author_sequences
            .entry((
                e.event.scope_id.clone(),
                e.event.author.clone(),
                e.event.sequence,
            ))
            .or_default()
            .push(hash.clone());
    }
    for e in events.values_mut() {
        e.status = "pending".into();
    }
    for hashes in author_sequences.values().filter(|v| v.len() > 1) {
        for hash in hashes {
            events.get_mut(hash).unwrap().status = "conflict".into();
        }
    }
    for hash in events.keys().cloned().collect::<Vec<_>>() {
        let event = &events[&hash].event;
        if let Some(previous) = &event.previous
            && let Some(prior) = events.get(previous)
            && (prior.event.scope_id != event.scope_id
                || prior.event.author != event.author
                || prior.event.sequence + 1 != event.sequence)
        {
            events.get_mut(&hash).unwrap().status = "invalid".into();
        }
    }
    let all: BTreeSet<String> = events.keys().cloned().collect();
    let ordered = topological(&events, &all);
    for hash in &ordered {
        let e = &events[hash];
        if e.status != "pending" {
            continue;
        }
        let deps: Vec<String> = dependencies(&e.event).cloned().collect();
        if deps.iter().any(|h| {
            events
                .get(h)
                .is_some_and(|p| matches!(p.status.as_str(), "invalid" | "conflict"))
        }) {
            events.get_mut(hash).unwrap().status = "invalid".into();
            continue;
        }
        if !deps
            .iter()
            .all(|h| events.get(h).is_some_and(|p| p.status == "ready"))
        {
            continue;
        }
        let mut blobs_complete = true;
        for blob in &e.event.blobs {
            if !has_blob_from(connection, network_id, blob)? {
                blobs_complete = false;
                break;
            }
        }
        if blobs_complete {
            events.get_mut(hash).unwrap().status = "ready".into();
        }
    }
    for (hash, e) in &events {
        connection.execute(
            "UPDATE events SET status=? WHERE network_id=? AND hash=?",
            params![e.status, network_id, hash],
        )?;
    }
    connection.execute("DELETE FROM conversations WHERE network_id=?", [network_id])?;
    connection.execute("DELETE FROM messages WHERE network_id=?", [network_id])?;
    connection.execute("DELETE FROM tasks WHERE network_id=?", [network_id])?;
    connection.execute(
        "DELETE FROM notification_outbox WHERE network_id=? AND derived=1",
        [network_id],
    )?;
    connection.execute(
        "DELETE FROM notification_acks WHERE network_id=? AND event_hash!=''",
        [network_id],
    )?;
    for (ordinal, hash) in ordered.iter().enumerate() {
        let stored = &events[hash];
        if stored.status != "ready" {
            continue;
        }
        let event = &stored.event;
        match &event.body {
            EventBody::ConversationCreated { title } => {
                connection.execute("INSERT OR IGNORE INTO conversations(network_id,conversation_id,title) VALUES(?,?,?)",params![network_id,event.scope_id,title])?;
            }
            EventBody::MessagePosted {
                message_id,
                role,
                text,
                ..
            } => {
                connection.execute("INSERT INTO messages(network_id,conversation_id,event_hash,message_id,role,text,created_at_ms,ordinal) VALUES(?,?,?,?,?,?,?,?)",params![network_id,event.scope_id,hash,message_id,role,text,event.created_at_ms,ordinal])?;
            }
            EventBody::TaskCreated {
                task_id,
                title,
                goal,
                acceptance,
            } => {
                connection.execute("INSERT OR IGNORE INTO tasks(network_id,conversation_id,task_id,title,goal,acceptance,status,detail,event_hash) VALUES(?,?,?,?,?,?,'open','',?)",params![network_id,event.scope_id,task_id,title,goal,acceptance,hash])?;
            }
            EventBody::TaskChanged {
                task_id,
                status,
                detail,
            } => {
                connection.execute("UPDATE tasks SET status=?,detail=?,event_hash=? WHERE network_id=? AND conversation_id=? AND task_id=?",params![status,detail,hash,network_id,event.scope_id,task_id])?;
            }
            EventBody::NotificationRaised {
                notification_id,
                kind,
                text,
            } => {
                let notification = Notification {
                    notification_id: notification_id.clone(),
                    conversation_id: event.scope_id.clone(),
                    kind: kind.clone(),
                    text: text.clone(),
                    event_hash: hash.clone(),
                };
                connection.execute("INSERT OR IGNORE INTO notification_outbox(network_id,notification_id,record,derived) VALUES(?,?,?,1)",params![network_id,notification_id,serde_json::to_string(&notification)?])?;
            }
            EventBody::NotificationAcknowledged {
                notification_id,
                client_id,
            } => {
                connection.execute("INSERT OR IGNORE INTO notification_acks(network_id,notification_id,client_id,event_hash) VALUES(?,?,?,?)",params![network_id,notification_id,client_id,hash])?;
            }
            _ => {}
        }
    }
    Ok(())
}
fn authorize_node_from(
    connection: &Connection,
    network_id: &str,
    node_id: &str,
    role: Option<NodeRole>,
) -> Result<NodeCertificate> {
    let encoded: String = connection
        .query_row(
            "SELECT bundle FROM networks WHERE network_id=?",
            [network_id],
            |r| r.get(0),
        )
        .optional()?
        .context("unknown network")?;
    let bundle: TrustBundle = serde_json::from_str(&encoded)?;
    verify_trust_bundle(&bundle)?;
    ensure!(
        bundle.network_id == network_id,
        "stored trust bundle network mismatch"
    );
    let node = bundle
        .nodes
        .into_iter()
        .find(|n| n.node_id == node_id)
        .context("node is not currently authorized")?;
    ensure!(!node.revoked, "node has been revoked");
    if let Some(role) = role {
        ensure!(node.roles.contains(&role), "node lacks required role");
    }
    Ok(node)
}
fn validate_operation_binding_from(
    connection: &Connection,
    request: &OperationRequest,
) -> Result<()> {
    let proof = request
        .binding
        .as_ref()
        .context("execution requires a confirmed task binding")?;
    let binding = &proof.binding;
    ensure!(
        binding.network_id == request.network_id
            && binding.conversation_id == request.conversation_id
            && Some(binding.task_id.as_str()) == request.task_id.as_deref(),
        "operation does not match its bound task"
    );
    ensure!(
        binding.target_node == request.target_node && binding.session_id == request.session_id,
        "operation target or session differs from confirmed task binding"
    );
    if request.action["type"] == "open" {
        ensure!(
            request.action == binding.open_spec,
            "open execution differs from confirmed binding"
        );
    }
    let ready: bool = connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM events WHERE network_id=? AND hash=? AND status='ready')",
        params![binding.network_id, binding.task_ref],
        |r| r.get(0),
    )?;
    ensure!(ready, "task creation is incomplete or conflicting");
    let hash = crate::binding::binding_hash(binding)?;
    let current: Option<String> = connection
        .query_row(
            "SELECT value FROM local_values WHERE network_id=? AND key=?",
            params![
                request.network_id,
                format!("task-binding-current:{}", binding.task_ref)
            ],
            |r| r.get(0),
        )
        .optional()?;
    ensure!(
        current
            .as_deref()
            .map(serde_json::from_str::<String>)
            .transpose()?
            .as_deref()
            == Some(hash.as_str()),
        "task binding is missing or superseded"
    );
    let sealed: bool = connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM local_values WHERE network_id=? AND key=?)",
        params![request.network_id, format!("task-binding-seal:{hash}")],
        |r| r.get(0),
    )?;
    ensure!(
        !sealed,
        "execution binding is sealed; old commands cannot resume it"
    );
    Ok(())
}

fn heads_from_events(events: &BTreeMap<String, StoredEvent>, scope_id: &str) -> Vec<String> {
    let mut heads: BTreeSet<String> = events
        .iter()
        .filter(|(_, e)| e.event.scope_id == scope_id)
        .map(|(h, _)| h.clone())
        .collect();
    for entry in events.values().filter(|e| e.event.scope_id == scope_id) {
        for parent in dependencies(&entry.event) {
            heads.remove(parent);
        }
    }
    heads.into_iter().collect()
}
fn load_events_from(
    connection: &Connection,
    network_id: &str,
) -> Result<BTreeMap<String, StoredEvent>> {
    let mut query = connection
        .prepare("SELECT hash,envelope,status,scope_id,author,sequence FROM events WHERE network_id=? ORDER BY hash")?;
    let rows = query.query_map([network_id], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, String>(3)?,
            r.get::<_, String>(4)?,
            r.get::<_, u64>(5)?,
        ))
    })?;
    let mut result = BTreeMap::new();
    for row in rows {
        let (hash, envelope, status, scope, author, sequence) = row?;
        let envelope: SignedEvent = serde_json::from_str(&envelope)?;
        ensure!(envelope.hash == hash, "stored event hash mismatch");
        let body: Event = serde_json::from_str(&envelope.canonical_body)?;
        let public_key: String = connection.query_row(
            "SELECT public_key FROM node_certificates WHERE network_id=? AND node_id=?",
            params![network_id, body.author],
            |r| r.get(0),
        )?;
        let event = verify_signed_event(network_id, &envelope, &public_key)?;
        ensure!(
            event.scope_id == scope && event.author == author && event.sequence == sequence,
            "stored event metadata does not match signed body"
        );
        result.insert(
            hash,
            StoredEvent {
                envelope,
                event,
                status,
            },
        );
    }
    Ok(result)
}

fn has_blob_from(connection: &Connection, network_id: &str, blob: &BlobRef) -> Result<bool> {
    let content: Option<Vec<u8>> = connection
        .query_row(
            "SELECT content FROM blobs WHERE network_id=? AND hash=? AND size=?",
            params![network_id, blob.hash, blob.size],
            |r| r.get(0),
        )
        .optional()?;
    Ok(content.is_some_and(|c| {
        c.len() as u64 == blob.size && c.len() <= MAX_BLOB_BYTES && digest(&c) == blob.hash
    }))
}
fn optional_json<T: DeserializeOwned>(value: Option<String>) -> Result<Option<T>> {
    value.map(|v| Ok(serde_json::from_str(&v)?)).transpose()
}
fn dependencies(event: &Event) -> impl Iterator<Item = &String> {
    event.parents.iter().chain(event.previous.iter())
}
fn closure_hashes(events: &BTreeMap<String, StoredEvent>, heads: &[String]) -> BTreeSet<String> {
    let mut result = BTreeSet::new();
    let mut stack = heads.to_vec();
    while let Some(hash) = stack.pop() {
        if result.insert(hash.clone())
            && let Some(event) = events.get(&hash)
        {
            stack.extend(dependencies(&event.event).cloned());
        }
    }
    result
}
fn topological(events: &BTreeMap<String, StoredEvent>, closure: &BTreeSet<String>) -> Vec<String> {
    let mut incoming: BTreeMap<String, usize> = BTreeMap::new();
    let mut children: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for hash in closure {
        if let Some(event) = events.get(hash) {
            let deps: BTreeSet<_> = dependencies(&event.event)
                .filter(|h| closure.contains(*h) && events.contains_key(*h))
                .cloned()
                .collect();
            incoming.insert(hash.clone(), deps.len());
            for parent in deps {
                children.entry(parent).or_default().insert(hash.clone());
            }
        }
    }
    let mut ready: BTreeSet<String> = incoming
        .iter()
        .filter(|(_, n)| **n == 0)
        .map(|(h, _)| h.clone())
        .collect();
    let mut ordered = Vec::new();
    while let Some(hash) = ready.pop_first() {
        ordered.push(hash.clone());
        if let Some(next) = children.get(&hash) {
            for child in next {
                let count = incoming.get_mut(child).unwrap();
                *count -= 1;
                if *count == 0 {
                    ready.insert(child.clone());
                }
            }
        }
    }
    ordered
}
fn canonical_resource(resource: &str) -> Result<String> {
    ensure!(resource.len() <= 4096, "resource path too long");
    let path = Path::new(resource);
    ensure!(path.is_absolute(), "resource paths must be absolute");
    // Reject ambiguous non-existent paths rather than guessing how a future symlink resolves.
    let canonical = if path.exists() {
        std::fs::canonicalize(path)?
    } else {
        ensure!(
            !path.components().any(|c| matches!(c, Component::ParentDir)),
            "nonexistent resource path cannot contain parent traversal"
        );
        let mut ancestor = path.to_path_buf();
        let mut tail = Vec::new();
        while !ancestor.exists() {
            tail.push(
                ancestor
                    .file_name()
                    .context("invalid resource path")?
                    .to_os_string(),
            );
            ensure!(ancestor.pop(), "resource has no existing ancestor");
        }
        let mut canonical = std::fs::canonicalize(&ancestor)?;
        for component in tail.iter().rev() {
            canonical.push(component);
        }
        canonical
    };
    canonical
        .to_str()
        .map(str::to_string)
        .context("resource path must be UTF-8")
}
fn paths_overlap(left: &str, right: &str) -> bool {
    let left = Path::new(left);
    let right = Path::new(right);
    left.starts_with(right) || right.starts_with(left)
}
fn validate_day(day: &str) -> Result<()> {
    ensure!(
        day.len() == 10
            && day.bytes().enumerate().all(|(i, b)| if i == 4 || i == 7 {
                b == b'-'
            } else {
                b.is_ascii_digit()
            }),
        "day must be YYYY-MM-DD"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    fn roles() -> Vec<NodeRole> {
        vec![
            NodeRole::Gateway,
            NodeRole::Replica,
            NodeRole::Scheduler,
            NodeRole::Executor,
        ]
    }
    fn memory(owner: &LocalIdentity, other: Option<&LocalIdentity>) -> Result<Store> {
        let mut store = Store::open(":memory:")?;
        let mut nodes = vec![owner.certificate(roles())?];
        if let Some(other) = other {
            nodes.push(other.certificate(roles())?);
        }
        store.install_trust_bundle(&owner.network_id()?, &owner.sign_trust_bundle(1, nodes)?)?;
        store.save_local_identity(&owner.network_id()?, owner)?;
        Ok(store)
    }
    fn conversation(store: &mut Store, owner: &LocalIdentity) -> Result<SignedEvent> {
        store.append_event(
            owner,
            &owner.network_id()?,
            "conversation",
            EventBody::ConversationCreated {
                title: "work".into(),
            },
            vec![],
            1,
        )
    }
    fn message(
        owner: &LocalIdentity,
        network: &str,
        author_sequence: u64,
        previous: Option<String>,
        parents: Vec<String>,
        text: &str,
        blobs: Vec<BlobRef>,
    ) -> Result<SignedEvent> {
        owner.sign_event(Event {
            version: 1,
            network_id: network.into(),
            scope_id: "conversation".into(),
            author: owner.node_id()?,
            sequence: author_sequence,
            previous,
            parents,
            created_at_ms: 2,
            body: EventBody::MessagePosted {
                message_id: text.into(),
                scheduler_node: None,
                role: "user".into(),
                text: text.into(),
            },
            blobs,
        })
    }
    fn request(
        store: &mut Store,
        owner: &LocalIdentity,
        id: &str,
        heads: Vec<String>,
    ) -> Result<OperationRequest> {
        let mut request = OperationRequest {
            operation_id: id.into(),
            network_id: owner.network_id()?,
            conversation_id: "conversation".into(),
            task_id: None,
            target_node: owner.node_id()?,
            session_id: "session".into(),
            expected_revision: 0,
            basis_heads: heads,
            action: json!({"type":"open","executor":"codex","cwd":"/tmp"}),
            resources: vec![],
            binding: None,
        };
        store.bind_test_operation(owner, &mut request)?;
        Ok(request)
    }
    fn session(owner: &LocalIdentity, id: &str) -> Result<ExecutionSession> {
        Ok(ExecutionSession {
            network_id: owner.network_id()?,
            target_node: owner.node_id()?,
            session_id: id.into(),
            revision: 0,
            state: "idle".into(),
            executor: "codex".into(),
            cwd: "/tmp".into(),
            native_session_id: None,
        })
    }
    #[test]
    fn causal_import_waits_for_every_dependency_and_blob_then_converges() -> Result<()> {
        let a = LocalIdentity::generate();
        let b = LocalIdentity::generate();
        let network = a.network_id()?;
        let mut source = memory(&a, Some(&b))?;
        let mut replica = memory(&a, Some(&b))?;
        let root = conversation(&mut source, &a)?;
        let content = b"necessary evidence";
        let blob = BlobRef {
            hash: digest(content),
            size: content.len() as u64,
        };
        let left = message(
            &a,
            &network,
            2,
            Some(root.hash.clone()),
            vec![root.hash.clone()],
            "left",
            vec![blob.clone()],
        )?;
        let right = message(
            &b,
            &network,
            1,
            None,
            vec![root.hash.clone()],
            "right",
            vec![],
        )?;
        assert_eq!(replica.ingest_event(&network, &left)?.status, "pending");
        assert!(replica.messages(&network, "conversation")?.is_empty());
        assert_eq!(
            replica
                .missing_dependencies(&network, std::slice::from_ref(&left.hash))?
                .events,
            vec![root.hash.clone()]
        );
        replica.ingest_event(&network, &right)?;
        replica.ingest_event(&network, &root)?;
        assert_eq!(replica.messages(&network, "conversation")?.len(), 1);
        assert_eq!(
            replica
                .context(&network, "conversation", &[])?
                .missing
                .blobs,
            vec![blob.clone()]
        );
        assert!(replica.put_blob(&network, &blob, b"wrong").is_err());
        replica.put_blob(&network, &blob, content)?;
        assert_eq!(replica.messages(&network, "conversation")?.len(), 2);
        replica.ingest_event(&network, &left)?;
        assert_eq!(replica.messages(&network, "conversation")?.len(), 2);
        let snapshot = replica.export_snapshot(&network, "conversation")?;
        source.import_snapshot(&network, &snapshot)?;
        source.put_blob(&network, &blob, content)?;
        assert_eq!(
            source.heads(&network, "conversation")?,
            replica.heads(&network, "conversation")?
        );
        assert_eq!(
            source.messages(&network, "conversation")?,
            replica.messages(&network, "conversation")?
        );
        assert!(replica.operations(&network)?.is_empty());
        let context = replica.context(&network, "conversation", &snapshot.heads)?;
        assert!(context.missing.is_complete());
        assert_eq!(context.events.len(), 3);
        Ok(())
    }
    #[test]
    fn corrupted_stored_blob_blocks_context_until_verified_content_is_restored() -> Result<()> {
        let a = LocalIdentity::generate();
        let network = a.network_id()?;
        let mut store = memory(&a, None)?;
        let root = conversation(&mut store, &a)?;
        let content = b"evidence";
        let blob = BlobRef {
            hash: digest(content),
            size: content.len() as u64,
        };
        store.put_blob(&network, &blob, content)?;
        let event = message(
            &a,
            &network,
            2,
            Some(root.hash.clone()),
            vec![root.hash],
            "attached",
            vec![blob.clone()],
        )?;
        store.ingest_event(&network, &event)?;
        store.connection.execute(
            "UPDATE blobs SET content=? WHERE network_id=? AND hash=?",
            params![b"tampered".as_slice(), network, blob.hash],
        )?;
        assert!(store.get_blob(&network, &blob.hash).is_err());
        assert_eq!(
            store.context(&network, "conversation", &[])?.missing.blobs,
            vec![blob.clone()]
        );
        store.rebuild_projections(&network)?;
        assert!(store.messages(&network, "conversation")?.is_empty());
        store.put_blob(&network, &blob, content)?;
        assert_eq!(store.messages(&network, "conversation")?.len(), 1);
        assert!(
            store
                .context(&network, "conversation", &[])?
                .missing
                .is_complete()
        );
        Ok(())
    }
    #[test]
    fn author_equivocation_removes_both_projections_and_blocks_descendants() -> Result<()> {
        let a = LocalIdentity::generate();
        let network = a.network_id()?;
        let mut store = memory(&a, None)?;
        let root = conversation(&mut store, &a)?;
        let left = message(
            &a,
            &network,
            2,
            Some(root.hash.clone()),
            vec![root.hash.clone()],
            "one",
            vec![],
        )?;
        let right = message(
            &a,
            &network,
            2,
            Some(root.hash.clone()),
            vec![root.hash.clone()],
            "two",
            vec![],
        )?;
        store.ingest_event(&network, &left)?;
        assert_eq!(store.messages(&network, "conversation")?.len(), 1);
        assert_eq!(store.ingest_event(&network, &right)?.status, "conflict");
        assert!(store.messages(&network, "conversation")?.is_empty());
        let child = message(
            &a,
            &network,
            3,
            Some(left.hash.clone()),
            vec![left.hash.clone()],
            "child",
            vec![],
        )?;
        assert_eq!(store.ingest_event(&network, &child)?.status, "invalid");
        assert!(
            !store
                .context(&network, "conversation", &[])?
                .missing
                .is_complete()
        );
        assert!(
            store
                .append_event(
                    &a,
                    &network,
                    "conversation",
                    EventBody::ConversationCreated {
                        title: "bad".into()
                    },
                    vec![],
                    0
                )
                .is_err()
        );
        Ok(())
    }
    #[test]
    fn revoked_author_history_verifies_but_new_commands_are_denied() -> Result<()> {
        let a = LocalIdentity::generate();
        let b = LocalIdentity::generate();
        let network = a.network_id()?;
        let mut store = memory(&a, Some(&b))?;
        let root = conversation(&mut store, &a)?;
        let event = message(&b, &network, 1, None, vec![root.hash], "old", vec![])?;
        let mut revoked = b.certificate(roles())?;
        revoked.revoked = true;
        let bundle = a.sign_trust_bundle(2, vec![a.certificate(roles())?, revoked])?;
        store.install_trust_bundle(&network, &bundle)?;
        assert!(store.authorize_node(&network, &b.node_id()?, None).is_err());
        assert_eq!(store.ingest_event(&network, &event)?.status, "ready");
        assert!(
            store
                .append_event(
                    &b,
                    &network,
                    "conversation",
                    EventBody::ConversationCreated {
                        title: "bad".into()
                    },
                    vec![],
                    0
                )
                .is_err()
        );
        let old = a.sign_trust_bundle(1, vec![a.certificate(roles())?])?;
        assert!(store.install_trust_bundle(&network, &old).is_err());
        Ok(())
    }
    #[test]
    fn queued_operation_cannot_start_after_known_executor_revocation() -> Result<()> {
        let a = LocalIdentity::generate();
        let network = a.network_id()?;
        let mut store = memory(&a, None)?;
        let event = conversation(&mut store, &a)?;
        let request = request(&mut store, &a, "queued", vec![event.hash])?;
        store.accept_operation(&network, &request)?;
        let mut certificate = a.certificate(roles())?;
        certificate.revoked = true;
        store.install_trust_bundle(&network, &a.sign_trust_bundle(2, vec![certificate])?)?;
        assert!(store.claim_operation(&network, "queued").is_err());
        assert_eq!(
            store.operation(&network, "queued")?.unwrap().state,
            OperationState::Accepted
        );
        Ok(())
    }
    #[test]
    fn concurrent_local_writers_serialize_author_sequences_and_projection_updates() -> Result<()> {
        let root = std::env::temp_dir().join(format!("vibeface-writers-{}", uuid::Uuid::new_v4()));
        let db = root.join("node.sqlite3");
        let owner = LocalIdentity::generate();
        let network = owner.network_id()?;
        let mut initial = Store::open(&db)?;
        initial.install_trust_bundle(
            &network,
            &owner.sign_trust_bundle(1, vec![owner.certificate(roles())?])?,
        )?;
        conversation(&mut initial, &owner)?;
        drop(initial);
        let first = Store::open(&db)?;
        let second = Store::open(&db)?;
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
        let mut workers = Vec::new();
        for (writer, mut store) in [first, second].into_iter().enumerate() {
            let barrier = barrier.clone();
            let owner = owner.clone();
            let network = network.clone();
            workers.push(std::thread::spawn(move || -> Result<()> {
                barrier.wait();
                for index in 0..6 {
                    store.append_event(
                        &owner,
                        &network,
                        "conversation",
                        EventBody::MessagePosted {
                            message_id: format!("{writer}-{index}"),
                            scheduler_node: None,
                            role: "user".into(),
                            text: format!("{writer}-{index}"),
                        },
                        vec![],
                        index,
                    )?;
                }
                Ok(())
            }));
        }
        for worker in workers {
            worker.join().unwrap()?;
        }
        let final_store = Store::open(&db)?;
        assert_eq!(final_store.messages(&network, "conversation")?.len(), 12);
        let context = final_store.context(&network, "conversation", &[])?;
        assert!(context.missing.is_complete());
        assert_eq!(context.events.len(), 13);
        assert_eq!(context.heads.len(), 1);
        drop(final_store);
        std::fs::remove_dir_all(root)?;
        Ok(())
    }
    #[test]
    fn corrupted_stored_event_never_becomes_a_complete_context() -> Result<()> {
        let a = LocalIdentity::generate();
        let network = a.network_id()?;
        let mut store = memory(&a, None)?;
        let mut event = conversation(&mut store, &a)?;
        event.canonical_body = event.canonical_body.replace("work", "tampered");
        store.connection.execute(
            "UPDATE events SET envelope=? WHERE network_id=? AND hash=?",
            params![serde_json::to_string(&event)?, network, event.hash],
        )?;
        assert!(store.context(&network, "conversation", &[]).is_err());
        assert!(store.rebuild_projections(&network).is_err());
        Ok(())
    }
    #[test]
    fn task_binding_cas_rejects_competing_targets_and_recovers_unsigned_commit() -> Result<()> {
        let owner = LocalIdentity::generate();
        let executor = LocalIdentity::generate();
        let network = owner.network_id()?;
        let dir =
            std::env::temp_dir().join(format!("vibeface-binding-cas-{}", uuid::Uuid::new_v4()));
        let db = dir.join("node.sqlite3");
        let mut store = Store::open(&db)?;
        store.install_trust_bundle(
            &network,
            &owner.sign_trust_bundle(
                1,
                vec![owner.certificate(roles())?, executor.certificate(roles())?],
            )?,
        )?;
        conversation(&mut store, &owner)?;
        let creation = store.append_event(
            &owner,
            &network,
            "conversation",
            EventBody::TaskCreated {
                task_id: "task".into(),
                title: "task".into(),
                goal: "task".into(),
                acceptance: "task".into(),
            },
            vec![],
            2,
        )?;
        let first = crate::binding::TaskBinding {
            network_id: network.clone(),
            conversation_id: "conversation".into(),
            task_ref: creation.hash,
            task_id: "task".into(),
            authority_node: owner.node_id()?,
            generation: 1,
            target_node: owner.node_id()?,
            session_id: "first".into(),
            open_spec: json!({"type":"open","executor":"codex","cwd":"/tmp"}),
            previous_binding_hash: None,
            previous_seal: None,
        };
        let mut second = first.clone();
        second.target_node = executor.node_id()?;
        second.session_id = "second".into();
        drop(store);
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
        let mut threads = vec![];
        for proposal in [first, second] {
            let mut store = Store::open(&db)?;
            let owner = owner.clone();
            let barrier = barrier.clone();
            threads.push(std::thread::spawn(move || {
                barrier.wait();
                store.bind_task(&owner, &proposal)
            }));
        }
        let results = threads
            .into_iter()
            .map(|thread| thread.join().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
        let proof = results.into_iter().find_map(Result::ok).unwrap();
        let mut store = Store::open(&db)?;
        let hash = crate::binding::binding_hash(&proof.binding)?;
        // Simulate commit succeeding while the process exits before its signature is persisted.
        store.local_set(
            &network,
            &format!("task-binding-record:{hash}"),
            &json!({"binding":proof.binding,"signature":null}),
        )?;
        assert!(
            store
                .task_binding(&network, &proof.binding.task_ref)?
                .is_none()
        );
        assert_eq!(store.bind_task(&owner, &proof.binding)?, proof);
        drop(store);
        std::fs::remove_dir_all(dir)?;
        Ok(())
    }

    #[test]
    fn unknown_binding_cannot_move_and_seal_fences_delayed_commands() -> Result<()> {
        let owner = LocalIdentity::generate();
        let network = owner.network_id()?;
        let mut store = memory(&owner, None)?;
        let root = conversation(&mut store, &owner)?;
        let request = request(&mut store, &owner, "work", vec![root.hash])?;
        let proof = request.binding.as_ref().unwrap();
        let mut unbound = request.clone();
        unbound.binding = None;
        assert!(store.accept_operation(&network, &unbound).is_err());
        let mut wrong = request.clone();
        wrong.target_node = "0".repeat(64);
        assert!(store.accept_operation(&network, &wrong).is_err());
        store.accept_operation(&network, &request)?;
        store.claim_operation(&network, "work")?;
        store.finish_operation(
            &network,
            "work",
            OperationState::Unknown,
            json!({"reason":"lost connection"}),
        )?;
        assert!(store.seal_binding(&owner, proof).is_err());
        let mut replacement = proof.binding.clone();
        replacement.generation = 2;
        replacement.session_id = "replacement".into();
        replacement.previous_binding_hash = Some(crate::binding::binding_hash(&proof.binding)?);
        assert!(store.bind_task(&owner, &replacement).is_err());
        store.finish_operation(
            &network,
            "work",
            OperationState::Failed,
            json!({"verified":"external execution stopped"}),
        )?;
        store.local_set(
            &network,
            "completion:work",
            &json!({"state":"failed","result":"done"}),
        )?;
        assert!(store.seal_binding(&owner, proof).is_err());
        store.append_event(
            &owner,
            &network,
            "conversation",
            EventBody::ExecutionObserved {
                operation_id: "work".into(),
                task_id: request.task_id.clone(),
                target_node: request.target_node.clone(),
                session_id: request.session_id.clone(),
                status: "failed".into(),
                detail: json!({"verified":"external execution stopped"}),
            },
            vec![],
            3,
        )?;
        let seal = store.seal_binding(&owner, proof)?;
        assert_eq!(store.seal_binding(&owner, proof)?, seal);
        let mut delayed = request.clone();
        delayed.operation_id = "delayed".into();
        assert!(store.accept_operation(&network, &delayed).is_err());
        replacement.previous_seal = Some(seal);
        let next = store.bind_task(&owner, &replacement)?;
        assert_eq!(next.binding.generation, 2);
        assert!(store.accept_operation(&network, &delayed).is_err());
        store.install_binding(proof)?;
        assert_eq!(
            store
                .task_binding(&network, &proof.binding.task_ref)?
                .unwrap(),
            next
        );
        Ok(())
    }

    #[test]
    fn two_scheduler_requests_at_one_revision_admit_only_one_side_effect() -> Result<()> {
        let owner = LocalIdentity::generate();
        let network = owner.network_id()?;
        let dir =
            std::env::temp_dir().join(format!("vibeface-session-cas-{}", uuid::Uuid::new_v4()));
        let db = dir.join("node.sqlite3");
        let mut store = Store::open(&db)?;
        store.install_trust_bundle(
            &network,
            &owner.sign_trust_bundle(1, vec![owner.certificate(roles())?])?,
        )?;
        let root = conversation(&mut store, &owner)?;
        let request = request(&mut store, &owner, "first", vec![root.hash])?;
        store.upsert_session(&network, &session(&owner, "session")?)?;
        drop(store);
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
        let mut threads = vec![];
        for id in ["scheduler-a", "scheduler-c"] {
            let mut store = Store::open(&db)?;
            let mut request = request.clone();
            request.operation_id = id.into();
            let barrier = barrier.clone();
            let network = network.clone();
            threads.push(std::thread::spawn(move || {
                barrier.wait();
                store.accept_operation(&network, &request)
            }));
        }
        let results = threads
            .into_iter()
            .map(|thread| thread.join().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
        let store = Store::open(&db)?;
        assert_eq!(store.operations(&network)?.len(), 1);
        drop(store);
        std::fs::remove_dir_all(dir)?;
        Ok(())
    }

    #[test]
    fn operation_cas_reserves_resources_and_reconciles_unknown_without_replay() -> Result<()> {
        let a = LocalIdentity::generate();
        let network = a.network_id()?;
        let mut store = memory(&a, None)?;
        let root = conversation(&mut store, &a)?;
        store.upsert_session(&network, &session(&a, "session")?)?;
        let mut first = request(&mut store, &a, "operation", vec![root.hash])?;
        first.resources = vec!["/tmp/vibeface-lock/repo".into()];
        assert_eq!(
            store.accept_operation(&network, &first)?.state,
            OperationState::Accepted
        );
        assert_eq!(
            store.accept_operation(&network, &first)?.state,
            OperationState::Accepted
        );
        let mut changed = first.clone();
        changed.action = json!({"type":"send"});
        assert!(store.accept_operation(&network, &changed).is_err());
        let mut overlap = first.clone();
        overlap.operation_id = "other".into();
        overlap.session_id = "second".into();
        overlap.resources = vec!["/tmp/vibeface-lock/repo/child".into()];
        overlap.task_id = None;
        overlap.binding = None;
        store.bind_test_operation(&a, &mut overlap)?;
        assert!(store.accept_operation(&network, &overlap).is_err());
        let mut sibling = overlap.clone();
        sibling.resources = vec!["/tmp/vibeface-lock/repo-two".into()];
        store.accept_operation(&network, &sibling)?;
        store.claim_operation(&network, "operation")?;
        assert!(store.claim_operation(&network, "operation").is_err());
        assert_eq!(store.recover_running()?, 1);
        assert_eq!(
            store.operation(&network, "operation")?.unwrap().state,
            OperationState::Unknown
        );
        assert!(store.claim_operation(&network, "operation").is_err());
        assert!(store.accept_operation(&network, &overlap).is_err());
        store.finish_operation(
            &network,
            "operation",
            OperationState::Succeeded,
            json!({"actual":"reconciled"}),
        )?;
        assert_eq!(
            store
                .session(&network, &a.node_id()?, "session")?
                .unwrap()
                .revision,
            1
        );
        let mut stale = first.clone();
        stale.operation_id = "stale".into();
        assert!(store.accept_operation(&network, &stale).is_err());
        stale.expected_revision = 1;
        store.accept_operation(&network, &stale)?;
        assert!(
            store
                .finish_operation(&network, "operation", OperationState::Failed, Value::Null)
                .is_err()
        );
        Ok(())
    }
    #[test]
    fn running_send_allows_deduped_controls_but_blocks_other_sends_and_controls() -> Result<()> {
        let a = LocalIdentity::generate();
        let network = a.network_id()?;
        let mut store = memory(&a, None)?;
        let root = conversation(&mut store, &a)?;
        store.upsert_session(&network, &session(&a, "session")?)?;
        let mut send = request(&mut store, &a, "send", vec![root.hash])?;
        send.action = json!({"type":"send","text":"work"});
        send.resources = vec!["/tmp/vibeface-active-turn".into()];
        store.accept_operation(&network, &send)?;
        store.claim_operation(&network, "send")?;
        let mut another = send.clone();
        another.operation_id = "send-again".into();
        assert!(store.accept_operation(&network, &another).is_err());
        assert!(store.accept_control_operation(&network, &another).is_err());
        let mut control = send.clone();
        control.operation_id = "interrupt".into();
        control.action = json!({"type":"interrupt"});
        control.resources.clear();
        assert_eq!(
            store.accept_control_operation(&network, &control)?.state,
            OperationState::Accepted
        );
        store.claim_operation(&network, "interrupt")?;
        assert_eq!(
            store.accept_control_operation(&network, &control)?.state,
            OperationState::Running
        );
        let mut second = control.clone();
        second.operation_id = "steer".into();
        second.action = json!({"type":"steer","text":"more"});
        assert!(store.accept_control_operation(&network, &second).is_err());
        store.finish_operation(
            &network,
            "interrupt",
            OperationState::Succeeded,
            Value::Null,
        )?;
        assert!(store.accept_operation(&network, &another).is_err());
        second.expected_revision = 1;
        store.accept_control_operation(&network, &second)?;
        assert_eq!(
            store.operation(&network, "send")?.unwrap().state,
            OperationState::Running
        );
        let mut newer = store.session(&network, &a.node_id()?, "session")?.unwrap();
        newer.revision = 2;
        store.upsert_session(&network, &newer)?;
        assert!(store.claim_operation(&network, "steer").is_err());
        Ok(())
    }
    #[test]
    fn all_replicated_and_local_ids_are_network_scoped() -> Result<()> {
        let a = LocalIdentity::generate();
        let b = LocalIdentity::generate();
        let mut store = memory(&a, None)?;
        let na = a.network_id()?;
        let nb = b.network_id()?;
        store.install_trust_bundle(&nb, &b.sign_trust_bundle(1, vec![b.certificate(roles())?])?)?;
        let ea = conversation(&mut store, &a)?;
        let eb = conversation(&mut store, &b)?;
        assert!(store.ingest_event(&nb, &ea).is_err());
        assert!(store.event(&na, &eb.hash)?.is_none());
        assert_eq!(store.conversations(&na)?.len(), 1);
        assert_eq!(store.conversations(&nb)?.len(), 1);
        let ra = request(&mut store, &a, "same-id", vec![ea.hash])?;
        let rb = request(&mut store, &b, "same-id", vec![eb.hash])?;
        store.accept_operation(&na, &ra)?;
        store.accept_operation(&nb, &rb)?;
        assert_eq!(
            store
                .operation(&na, "same-id")?
                .unwrap()
                .request
                .target_node,
            a.node_id()?
        );
        assert_eq!(
            store
                .operation(&nb, "same-id")?
                .unwrap()
                .request
                .target_node,
            b.node_id()?
        );
        store.local_set(&na, "auth", &json!("a"))?;
        store.local_set(&nb, "auth", &json!("b"))?;
        assert_eq!(store.local_value(&na, "auth")?, Some(json!("a")));
        assert_eq!(store.local_value(&nb, "auth")?, Some(json!("b")));
        let blob = BlobRef {
            hash: digest(b"private"),
            size: 7,
        };
        store.put_blob(&na, &blob, b"private")?;
        assert!(store.get_blob(&nb, &blob.hash)?.is_none());
        Ok(())
    }
    #[test]
    fn same_device_resources_are_reserved_across_networks_without_sharing_records() -> Result<()> {
        let a = LocalIdentity::generate();
        let b = LocalIdentity::generate();
        let na = a.network_id()?;
        let nb = b.network_id()?;
        let mut store = memory(&a, None)?;
        store.install_trust_bundle(
            &nb,
            &b.sign_trust_bundle(1, vec![a.certificate(roles())?, b.certificate(roles())?])?,
        )?;
        let ea = conversation(&mut store, &a)?;
        let eb = conversation(&mut store, &b)?;
        let mut ra = request(&mut store, &a, "same-op", vec![ea.hash])?;
        ra.resources = vec!["/tmp/vibeface-shared-device".into()];
        let mut rb = request(&mut store, &b, "same-op", vec![eb.hash])?;
        rb.target_node = a.node_id()?;
        rb.task_id = Some("other-device-task".into());
        rb.binding = None;
        store.bind_test_operation(&b, &mut rb)?;
        rb.resources = ra.resources.clone();
        store.accept_operation(&na, &ra)?;
        assert!(store.accept_operation(&nb, &rb).is_err());
        assert!(store.operation(&nb, "same-op")?.is_none());
        rb.resources = vec!["/tmp/vibeface-separate-device".into()];
        store.accept_operation(&nb, &rb)?;
        Ok(())
    }
    #[test]
    fn notifications_budget_and_continuation_claims_are_durable_deduped_state() -> Result<()> {
        let a = LocalIdentity::generate();
        let network = a.network_id()?;
        let mut store = memory(&a, None)?;
        conversation(&mut store, &a)?;
        let result = store.append_event(
            &a,
            &network,
            "conversation",
            EventBody::ExecutionObserved {
                operation_id: "operation".into(),
                task_id: None,
                target_node: a.node_id()?,
                session_id: "session".into(),
                status: "completed".into(),
                detail: json!({"turn":"finished"}),
            },
            vec![],
            2,
        )?;
        let notification = Notification {
            notification_id: "notice".into(),
            conversation_id: "conversation".into(),
            kind: "turn_completed".into(),
            text: "awaiting acceptance".into(),
            event_hash: result.hash.clone(),
        };
        store.queue_notification(&network, &notification)?;
        store.queue_notification(&network, &notification)?;
        assert_eq!(store.pending_notifications(&network, "phone")?.len(), 1);
        store.ack_notification(&network, "notice", "phone")?;
        assert!(store.pending_notifications(&network, "phone")?.is_empty());
        assert_eq!(store.pending_notifications(&network, "laptop")?.len(), 1);
        assert_eq!(store.reserve_budget(&network, "2026-10-16", 2, 3)?.used, 2);
        assert!(store.reserve_budget(&network, "2026-10-16", 2, 3).is_err());
        assert_eq!(store.budget_usage(&network, "2026-10-16", 3)?.used, 2);
        store.claim_continuation(&network, &result.hash, "v1", &a.node_id()?)?;
        assert!(
            store
                .claim_continuation(&network, &result.hash, "v1", &a.node_id()?)
                .is_err()
        );
        store.recover_running()?;
        assert_eq!(
            store
                .continuation(&network, &result.hash, "v1")?
                .unwrap()
                .state,
            "unknown"
        );
        assert!(
            store
                .claim_continuation(&network, &result.hash, "v1", &a.node_id()?)
                .is_err()
        );
        store.finish_continuation(
            &network,
            &result.hash,
            "v1",
            &a.node_id()?,
            "completed",
            json!({"next":"ask_user"}),
        )?;
        store.rebuild_projections(&network)?;
        assert_eq!(store.pending_notifications(&network, "laptop")?.len(), 1);
        assert!(store.operations(&network)?.is_empty());
        Ok(())
    }
    #[test]
    fn replicated_notification_and_ack_rebuild_without_model_or_operation() -> Result<()> {
        let a = LocalIdentity::generate();
        let network = a.network_id()?;
        let mut source = memory(&a, None)?;
        let mut target = memory(&a, None)?;
        conversation(&mut source, &a)?;
        source.append_event(
            &a,
            &network,
            "conversation",
            EventBody::NotificationRaised {
                notification_id: "sync-notice".into(),
                kind: "approval".into(),
                text: "human response needed".into(),
            },
            vec![],
            2,
        )?;
        target.import_snapshot(&network, &source.export_snapshot(&network, "conversation")?)?;
        assert_eq!(target.pending_notifications(&network, "phone")?.len(), 1);
        source.append_event(
            &a,
            &network,
            "conversation",
            EventBody::NotificationAcknowledged {
                notification_id: "sync-notice".into(),
                client_id: "phone".into(),
            },
            vec![],
            3,
        )?;
        target.import_snapshot(&network, &source.export_snapshot(&network, "conversation")?)?;
        assert!(target.pending_notifications(&network, "phone")?.is_empty());
        assert_eq!(target.pending_notifications(&network, "laptop")?.len(), 1);
        target.rebuild_projections(&network)?;
        assert!(target.pending_notifications(&network, "phone")?.is_empty());
        assert!(target.operations(&network)?.is_empty());
        Ok(())
    }
    #[test]
    fn reopening_keeps_unknown_effects_reserved_and_rejects_future_schema() -> Result<()> {
        let root = std::env::temp_dir().join(format!("vibeface-core-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root)?;
        let db = root.join("node.sqlite3");
        let a = LocalIdentity::generate();
        let network = a.network_id()?;
        {
            let mut store = Store::open(&db)?;
            store.install_trust_bundle(
                &network,
                &a.sign_trust_bundle(1, vec![a.certificate(roles())?])?,
            )?;
            let event = conversation(&mut store, &a)?;
            let request = request(&mut store, &a, "durable", vec![event.hash])?;
            store.accept_operation(&network, &request)?;
            store.claim_operation(&network, "durable")?;
        }
        {
            let store = Store::open(&db)?;
            assert_eq!(
                store.operation(&network, "durable")?.unwrap().state,
                OperationState::Unknown
            );
            assert_eq!(store.conversations(&network)?.len(), 1);
        }
        let connection = Connection::open(&db)?;
        connection.execute_batch("PRAGMA user_version=2;")?;
        drop(connection);
        assert!(Store::open(&db).is_err());
        std::fs::remove_dir_all(root)?;
        Ok(())
    }
    #[cfg(unix)]
    #[test]
    fn resource_normalization_follows_symlinks_before_parent_components() -> Result<()> {
        let root = std::env::temp_dir().join(format!("vibeface-resource-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(root.join("real/child"))?;
        std::os::unix::fs::symlink(root.join("real/child"), root.join("link"))?;
        let resource = canonical_resource(root.join("link/..").to_str().unwrap())?;
        assert_eq!(
            resource,
            std::fs::canonicalize(root.join("real"))?.to_str().unwrap()
        );
        assert!(canonical_resource(root.join("absent/../other").to_str().unwrap()).is_err());
        std::fs::remove_dir_all(root)?;
        Ok(())
    }
}
