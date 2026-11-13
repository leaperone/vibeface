use super::*;

const PLAN_FILE: &str = "identity-operation.json";
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IdentityOperationState {
    Prepared,
    Unknown,
    Recorded,
    Applied,
    Stopped,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum IdentityPlanKind {
    Creation,
    Recovery,
    KeyRotation,
}

#[derive(Serialize)]
pub struct IdentityPlanInfo {
    pub network_id: String,
    pub client_id: String,
    pub target: Entry,
    pub backup_path: PathBuf,
    pub backup_sha256: String,
    pub state: IdentityOperationState,
    pub kind: IdentityPlanKind,
    pub members: Option<Vec<Member>>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Plan {
    client_id: String,
    network_id: String,
    entry: Entry,
    backup_path: PathBuf,
    backup_bytes: Vec<u8>,
    request_bytes: Vec<u8>,
    genesis: vibeface_secure::membership::Genesis,
    checkpoint: Option<RootCheckpoint>,
    state: IdentityOperationState,
}
struct IdentityLock(std::fs::File);
impl Drop for IdentityLock {
    fn drop(&mut self) {
        let _ = self.0.unlock();
    }
}
impl Plan {
    fn backup_sha256(&self) -> String {
        hex::encode(Sha256::digest(&self.backup_bytes))
    }
    fn info(&self) -> IdentityPlanInfo {
        let (kind, members) = match &self.checkpoint {
            None => (IdentityPlanKind::Creation, Some(self.genesis.members())),
            Some(checkpoint) => match &checkpoint.directory {
                Some(directory) => (IdentityPlanKind::Recovery, Some(directory.members.clone())),
                None => (IdentityPlanKind::KeyRotation, None),
            },
        };
        IdentityPlanInfo {
            network_id: self.network_id.clone(),
            client_id: self.client_id.clone(),
            target: self.entry.clone(),
            backup_path: self.backup_path.clone(),
            backup_sha256: self.backup_sha256(),
            state: self.state,
            kind,
            members,
        }
    }
    fn ensure_discardable(&self) -> Result<()> {
        ensure!(
            matches!(
                self.state,
                IdentityOperationState::Prepared
                    | IdentityOperationState::Applied
                    | IdentityOperationState::Stopped
            ),
            "unknown or waiting recovery cannot be discarded"
        );
        Ok(())
    }
}
/// An encrypted pending identity operation plus an already durable, verified recovery backup.
/// No governance request is sent until `submit` is explicitly called after the user confirms the file.
pub struct IdentityPlan {
    dir: PathBuf,
    plan: Plan,
    password: Zeroizing<String>,
    _lock: IdentityLock,
}
impl IdentityPlan {
    fn lock(key: &ClientKey) -> Result<IdentityLock> {
        let lock = privatefs::open_or_create(&key.dir.join("identity-operation.lock"))?;
        lock.try_lock()
            .context("identity operation is open in another process or window")?;
        Ok(IdentityLock(lock))
    }
    pub fn exists(key: &ClientKey) -> bool {
        key.dir.join(PLAN_FILE).exists()
    }
    /// Inspect public journal metadata without repairing its backup or acquiring a lease.
    pub fn inspect(key: &ClientKey, password: &str) -> Result<Option<IdentityPlanInfo>> {
        if !Self::exists(key) {
            return Ok(None);
        }
        Ok(Some(Self::read_plan(key, password)?.info()))
    }
    /// Discard a never-sent or conclusively settled saved plan. Its backup stays intact.
    pub fn discard_saved(key: &ClientKey, password: &str) -> Result<()> {
        ensure!(Self::exists(key), "no identity operation is pending");
        let _lock = Self::lock(key)?;
        let plan = Self::read_plan(key, password)?;
        plan.ensure_discardable()?;
        Self::remove_journal(&key.dir)
    }
    /// Clear only an Applied journal whose network this client remembers.
    pub fn discard_applied(key: &ClientKey, password: &str) -> Result<bool> {
        if !Self::exists(key) {
            return Ok(false);
        }
        let _lock = Self::lock(key)?;
        if !Self::exists(key) {
            return Ok(false);
        }
        let plan = Self::read_plan(key, password)?;
        if plan.state != IdentityOperationState::Applied
            || key.network_id() != Some(plan.network_id.as_str())
        {
            return Ok(false);
        }
        Self::remove_journal(&key.dir)?;
        Ok(true)
    }
    fn remove_journal(dir: &Path) -> Result<()> {
        std::fs::remove_file(dir.join(PLAN_FILE))?;
        privatefs::sync_dir(dir)
    }
    pub fn backup_path(&self) -> &Path {
        &self.plan.backup_path
    }
    pub fn backup_sha256(&self) -> String {
        self.plan.backup_sha256()
    }
    pub fn network_id(&self) -> &str {
        &self.plan.network_id
    }
    pub fn state(&self) -> IdentityOperationState {
        self.plan.state
    }
    pub fn recovery_members(&self) -> Option<&[Member]> {
        self.plan
            .checkpoint
            .as_ref()?
            .directory
            .as_ref()
            .map(|directory| directory.members.as_slice())
    }
    /// Check retry arguments against the saved recovery without rebuilding or sending it.
    /// An omitted selection resumes the saved members; an explicit selection must match them.
    pub fn validate_recovery_intent(
        &self,
        entry: &Entry,
        new_file: &Path,
        name: &str,
        selected_members: Option<&[Member]>,
    ) -> Result<()> {
        ensure!(
            entry == &self.plan.entry && new_file == self.plan.backup_path.as_path(),
            "pending recovery uses another Node or replacement file; resume its original arguments"
        );
        let saved = self
            .recovery_members()
            .context("pending identity operation is not a member recovery")?;
        let manager = saved
            .iter()
            .find(|member| member.device_id == self.plan.client_id)
            .context("pending recovery does not contain this client")?;
        ensure!(
            manager.kind == IdentityKind::Client && manager.manage && manager.name == name,
            "pending recovery uses another manager name; resume its original arguments"
        );
        if let Some(selected) = selected_members {
            let retained: Vec<_> = saved
                .iter()
                .filter(|member| member.device_id != self.plan.client_id)
                .cloned()
                .collect();
            let mut requested = recovery_members(
                &retained,
                Some(selected),
                &self.plan.entry.node_id,
                manager.clone(),
            )?;
            requested.sort_by(|a, b| a.device_id.cmp(&b.device_id));
            ensure!(
                requested == saved,
                "--members differs from the pending recovery; resume its original selection"
            );
        }
        Ok(())
    }
    pub fn load(key: &ClientKey, password: &str) -> Result<Self> {
        let lock = Self::lock(key)?;
        let plan = Self::read_plan(key, password)?;
        let pending = Self {
            dir: key.dir.clone(),
            plan,
            password: Zeroizing::new(password.into()),
            _lock: lock,
        };
        pending.verify_backup()?;
        Ok(pending)
    }
    /// Read the saved selection without repairing its backup or changing its journal.
    pub fn recovery_review(
        key: &ClientKey,
        password: &str,
        entry: &Entry,
    ) -> Result<RecoveryReview> {
        let _lock = Self::lock(key)?;
        let plan = Self::read_plan(key, password)?;
        ensure!(entry == &plan.entry, "pending recovery uses another Node");
        let members = plan
            .checkpoint
            .as_ref()
            .and_then(|checkpoint| checkpoint.directory.as_ref())
            .context("pending identity operation is not a member recovery")?
            .members
            .as_slice();
        let new_manager = members
            .iter()
            .find(|member| member.device_id == plan.client_id)
            .context("pending recovery does not contain this client")?
            .clone();
        Ok(RecoveryReview {
            network_id: plan.network_id,
            members: members
                .iter()
                .filter(|member| member.device_id != plan.client_id)
                .cloned()
                .collect(),
            required_node: plan.entry.node_id,
            new_manager,
        })
    }
    fn read_plan(key: &ClientKey, password: &str) -> Result<Plan> {
        let file = privatefs::read_private_bounded(
            &key.dir.join(PLAN_FILE),
            16 * 1024 * 1024,
            "pending identity operation",
        )?;
        let bytes = Zeroizing::new(
            recovery_open(&file, password)
                .context("wrong recovery password for pending operation")?,
        );
        let plan: Plan = serde_json::from_slice(&bytes)?;
        ensure!(
            plan.client_id == key.device_id(),
            "pending operation belongs to another client"
        );
        ensure!(
            key.network_id()
                .is_none_or(|network| network == plan.network_id),
            "pending operation belongs to another network"
        );
        Ok(plan)
    }
    pub(super) fn prepare(
        key: &ClientKey,
        entry: Entry,
        path: &Path,
        material: &RecoveryMaterial,
        password: &str,
        params: Value,
    ) -> Result<Self> {
        let lock = Self::lock(key)?;
        ensure!(path.is_absolute(), "recovery backup path must be absolute");
        ensure!(
            !Self::exists(key),
            "an identity operation is pending; reopen it instead of generating another recovery key"
        );
        ensure!(
            !path.exists(),
            "recovery backup already exists; choose a new path"
        );
        let backup_bytes = recovery_file_bytes(material, password)?;
        let request_bytes = serde_json::to_vec(
            &json!({"id":uuid::Uuid::new_v4().to_string(),"method":"recover","params":params}),
        )?;
        let pending = Self {
            dir: key.dir.clone(),
            password: Zeroizing::new(password.into()),
            _lock: lock,
            plan: Plan {
                client_id: key.device_id().into(),
                network_id: material.network_id.clone(),
                entry,
                backup_path: path.to_owned(),
                backup_bytes,
                request_bytes,
                genesis: material.genesis.clone(),
                checkpoint: material.checkpoints.last().cloned(),
                state: IdentityOperationState::Prepared,
            },
        };
        // The encrypted journal lands first. A crash before publishing the backup resumes these same bytes.
        let sealed = pending.sealed()?;
        durable::publish_new(&pending.dir.join(PLAN_FILE), &sealed)?;
        pending.verify_backup()?;
        Ok(pending)
    }
    fn sealed(&self) -> Result<Vec<u8>> {
        let bytes = Zeroizing::new(serde_json::to_vec(&self.plan)?);
        Ok(recovery_seal(&bytes, &self.password)?)
    }
    fn save(&self) -> Result<()> {
        privatefs::write_atomic(&self.dir.join(PLAN_FILE), &self.sealed()?)
    }
    fn verify_backup(&self) -> Result<()> {
        if !self.plan.backup_path.exists() {
            durable::publish_new(&self.plan.backup_path, &self.plan.backup_bytes)?;
        }
        let actual =
            privatefs::read_private_bounded(&self.plan.backup_path, MAX_FILE, "recovery backup")?;
        ensure!(
            actual == self.plan.backup_bytes,
            "recovery backup changed; restore the exact saved backup before submitting"
        );
        Ok(())
    }
    /// A Prepared operation has never crossed the network. Unknown/Recorded only read authoritative evidence.
    pub async fn submit(&mut self, key: &mut ClientKey) -> Result<Value> {
        ensure!(
            key.device_id() == self.plan.client_id,
            "identity changed since preparation"
        );
        self.verify_backup()?;
        if self.plan.state != IdentityOperationState::Prepared {
            return self.reconcile(key).await;
        }
        self.plan.state = IdentityOperationState::Unknown;
        self.save()?;
        let result = async {
            let session = key.connect(Some(&self.plan.entry)).await?;
            session
                .connection
                .call_bytes(self.plan.request_bytes.clone())
                .await
        }
        .await;
        match result {
            Ok(result)=>{self.record_receipt(key,&result)?;Ok(result)}
            Err(error)=>match self.reconcile(key).await {Ok(result)=>Ok(result),Err(_)=>Err(error).context("identity operation outcome is unknown; reopen the pending operation and query its receipt")},
        }
    }
    pub async fn reconcile(&mut self, key: &mut ClientKey) -> Result<Value> {
        ensure!(
            key.device_id() == self.plan.client_id,
            "pending operation belongs to another client"
        );
        let mut session = key.connect(Some(&self.plan.entry)).await?;
        let result = session.call("authenticate", json!({})).await?;
        drop(session);
        self.record_receipt(key, &result)?;
        Ok(result)
    }
    fn record_receipt(&mut self, key: &mut ClientKey, result: &Value) -> Result<()> {
        ensure!(
            result["network_id"] == self.plan.network_id,
            "original Node has not confirmed this network; pending operation retained"
        );
        self.plan.state = if !result["state"].is_null() {
            let state: NetworkState = serde_json::from_value(result["state"].clone())?;
            ensure!(
                state.genesis == self.plan.genesis,
                "Node returned another genesis"
            );
            let now = result["now_ms"].as_u64().context("receipt time missing")?;
            let view = state.project_at(now)?;
            if let Some(checkpoint) = &self.plan.checkpoint {
                ensure!(
                    state.checkpoints.contains(checkpoint),
                    "original checkpoint is not recorded here"
                );
                let hash = checkpoint.hash()?;
                if view.stopped.contains(&hash) {
                    IdentityOperationState::Stopped
                } else if view
                    .pending
                    .as_ref()
                    .is_some_and(|pending| pending.checkpoint == hash)
                {
                    IdentityOperationState::Recorded
                } else {
                    ensure!(
                        view.epoch >= checkpoint.epoch
                            && (checkpoint.directory.is_none()
                                || view.client(key.device_id())?.manage),
                        "recovery has not taken effect"
                    );
                    IdentityOperationState::Applied
                }
            } else {
                ensure!(
                    view.client(key.device_id())?.manage,
                    "creation did not grant this client management"
                );
                IdentityOperationState::Applied
            }
        } else {
            let checkpoint = self
                .plan
                .checkpoint
                .as_ref()
                .context("original creation is not confirmed")?;
            ensure!(
                result["recovery"]["checkpoint_hash"] == checkpoint.hash()?,
                "Node has not confirmed this exact recovery checkpoint"
            );
            match result["recovery"]["status"].as_str() {
                Some("waiting") => IdentityOperationState::Recorded,
                Some("stopped") => IdentityOperationState::Stopped,
                _ => anyhow::bail!("recovery has no authoritative receipt"),
            }
        };
        self.save()?;
        if self.plan.state != IdentityOperationState::Stopped {
            key.remember(&self.plan.network_id, self.plan.entry.clone())?;
        }
        Ok(())
    }
    /// Explicitly discard only a never-sent or conclusively settled plan. The recovery backup stays intact.
    pub fn discard(self) -> Result<()> {
        self.plan.ensure_discardable()?;
        Self::remove_journal(&self.dir)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recovery_readback_requires_the_exact_checkpoint_and_valid_signed_state() -> Result<()> {
        let dir = std::env::temp_dir().join(format!("identity-receipt-{}", uuid::Uuid::new_v4()));
        let mut key = ClientKey::create(&dir.join("key"), "client passphrase")?;
        let root = Identity::generate()?;
        let node = Identity::generate()?;
        let mut state = NetworkState::new(vibeface_secure::membership::Genesis::create(
            &root,
            Member::client(&key.identity, "cli", true)?,
            Member::node(&node, "node")?,
        )?)?;
        let network = state.genesis.network_id.clone();
        let now = now_ms() as u64;
        let next = Identity::generate()?;
        let checkpoint = RootCheckpoint::recovery(
            &state,
            &root,
            &next.device_id(),
            state.project_at(now)?.members,
            vec![],
            now,
        )?;
        let backup = RecoveryMaterial {
            version: 1,
            network_id: network.clone(),
            root_secret_key: Zeroizing::new(next.secret_hex()),
            genesis: state.genesis.clone(),
            checkpoints: vec![checkpoint.clone()],
            events: vec![],
        };
        let mut plan = IdentityPlan::prepare(
            &key,
            Entry {
                origin: "http://127.0.0.1:1".into(),
                node_id: node.device_id(),
            },
            &dir.join("recovery.json"),
            &backup,
            "recovery password",
            json!({"genesis":state.genesis,"checkpoints":[checkpoint],"events":[]}),
        )?;
        assert!(
            plan.record_receipt(
                &mut key,
                &json!({
                    "network_id":network,"state":state,"now_ms":now,"principal":{"can_manage":true}
                })
            )
            .is_err(),
            "management alone cannot confirm the original checkpoint"
        );
        for status in ["waiting", "stopped"] {
            let reply = json!({"network_id":network,"recovery":{"status":status,"checkpoint_hash":checkpoint.hash()?}});
            let mut wrong = reply.clone();
            wrong["recovery"]["checkpoint_hash"] = json!("another-checkpoint");
            assert!(plan.record_receipt(&mut key, &wrong).is_err());
            wrong = reply.clone();
            wrong["network_id"] = json!("another-network");
            assert!(plan.record_receipt(&mut key, &wrong).is_err());
            plan.record_receipt(&mut key, &reply)?;
            assert_eq!(
                plan.state(),
                if status == "waiting" {
                    IdentityOperationState::Recorded
                } else {
                    IdentityOperationState::Stopped
                }
            );
        }
        state.recover_at(checkpoint.clone(), now)?;
        let mut reply = json!({"network_id":network,"state":state,"now_ms":now});
        plan.record_receipt(&mut key, &reply)?;
        assert_eq!(plan.state(), IdentityOperationState::Recorded);
        reply["network_id"] = json!("another-network");
        assert!(plan.record_receipt(&mut key, &reply).is_err());
        reply["network_id"] = json!(network);
        reply["state"]["checkpoints"][0]["signature"] = json!("invalid");
        assert!(plan.record_receipt(&mut key, &reply).is_err());
        drop(plan);
        std::fs::remove_dir_all(dir)?;
        Ok(())
    }

    #[test]
    fn encrypted_plan_restores_the_same_backup_and_never_regenerates_unknown_recovery() -> Result<()>
    {
        let dir = std::env::temp_dir().join(format!("identity-plan-{}", uuid::Uuid::new_v4()));
        let key = ClientKey::create(&dir.join("key"), "client passphrase")?;
        let root = Identity::generate()?;
        let node = Identity::generate()?;
        let genesis = vibeface_secure::membership::Genesis::create(
            &root,
            Member::client(&key.identity, "client", true)?,
            Member::node(&node, "node")?,
        )?;
        let backup = RecoveryMaterial {
            version: 1,
            network_id: genesis.network_id.clone(),
            root_secret_key: Zeroizing::new(root.secret_hex()),
            genesis: genesis.clone(),
            checkpoints: vec![],
            events: vec![],
        };
        let entry = Entry {
            origin: "http://127.0.0.1:1".into(),
            node_id: node.device_id(),
        };
        let path = dir.join("recovery.json");
        let plan = IdentityPlan::prepare(
            &key,
            entry.clone(),
            &path,
            &backup,
            "recovery password",
            json!({"genesis":genesis,"capability":"controlled-one-time-capability"}),
        )?;
        let bytes = std::fs::read(&path)?;
        let request = plan.plan.request_bytes.clone();
        assert!(
            IdentityPlan::load(&key, "recovery password").is_err(),
            "another process cannot submit a stale Prepared plan"
        );
        let journal = std::fs::read_to_string(key.dir.join(PLAN_FILE))?;
        assert!(!journal.contains("controlled-one-time-capability"));
        assert!(!journal.contains(&root.secret_hex()));
        assert!(
            IdentityPlan::prepare(
                &key,
                entry,
                &dir.join("another.json"),
                &backup,
                "recovery password",
                json!({})
            )
            .is_err()
        );
        std::fs::remove_file(&path)?;
        let inherited_lock = plan._lock.0.try_clone()?;
        drop(plan);
        assert!(IdentityPlan::load(&key, "wrong password").is_err());
        let mut reopened = IdentityPlan::load(&key, "recovery password")
            .context("a dropped plan must release its lock while a duplicate stays open")?;
        drop(inherited_lock);
        assert!(
            IdentityPlan::load(&key, "recovery password").is_err(),
            "closing a stale duplicate must not unlock the current plan"
        );
        assert_eq!(
            std::fs::read(&path)?,
            bytes,
            "resume restores original sealed backup bytes"
        );
        assert_eq!(reopened.plan.request_bytes, request);
        reopened.plan.state = IdentityOperationState::Unknown;
        reopened.save()?;
        drop(reopened);
        let reopened = IdentityPlan::load(&key, "recovery password")?;
        assert_eq!(reopened.state(), IdentityOperationState::Unknown);
        assert_eq!(reopened.plan.request_bytes, request);
        assert!(
            reopened.discard().is_err(),
            "an unknown rotation cannot be discarded and replaced"
        );
        assert_eq!(std::fs::read(&path)?, bytes);
        std::fs::remove_dir_all(dir)?;
        Ok(())
    }
}

#[cfg(test)]
mod lifecycle_tests;
