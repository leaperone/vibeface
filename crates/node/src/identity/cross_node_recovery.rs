use super::{journal, policy::project_policy_in};
use crate::{crypto::LocalIdentity, store::Store};
use anyhow::{Context, Result, bail, ensure};
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use vibeface_secure::{
    Identity,
    authorization::{
        Actor, ClientAuthorizationBundle, DeviceId, Grantee, JournalEntry, ObjectHash,
        PolicyDomain, PublicKey, Signature, Signed,
        bootstrap::{
            NodeClaim,
            recovery::cross_node::{
                self, CrossNodeRecoveryCommitRequest, CrossNodeRecoveryCommitted,
                CrossNodeRecoveryContext, CrossNodeRecoveryPossessionRequest,
                CrossNodeRecoveryRequest,
            },
        },
    },
};

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SourceContext {
    context: CrossNodeRecoveryContext,
    source_possession: Option<SourcePossession>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SourcePossession {
    possession_hash: ObjectHash,
    proof: Signature,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct TargetPossession {
    request_hash: ObjectHash,
    possession_hash: ObjectHash,
    principal: String,
    request: CrossNodeRecoveryPossessionRequest,
    proof: Signature,
}

fn metadata(connection: &Connection, key: &str) -> Result<Option<String>> {
    Ok(connection
        .query_row(
            "SELECT value FROM identity_meta WHERE key=?",
            [key],
            |row| row.get(0),
        )
        .optional()?)
}

fn context_key(request: &Signed<CrossNodeRecoveryRequest>) -> String {
    format!(
        "cross-node-recovery-context:{}:{}",
        request.payload.account, request.payload.nonce
    )
}

fn require_principal(request: &Signed<CrossNodeRecoveryRequest>, principal: &str) -> Result<()> {
    ensure!(
        request.payload.new_device_public_key.as_str() == principal,
        "recovery belongs to another Noise device"
    );
    request.verify_signature()?;
    Ok(())
}

fn check_claim_in(
    tx: &Transaction<'_>,
    claim: &Signed<NodeClaim>,
    local: &PublicKey,
) -> Result<()> {
    ensure!(
        claim.payload.node_public_key == *local,
        "recovery targets another local Node"
    );
    let stored = metadata(tx, "node_claim")?.context("local Node claim is missing")?;
    ensure!(
        serde_json::from_str::<Signed<NodeClaim>>(&stored)? == *claim,
        "recovery claim is not the current local challenge"
    );
    let expected = crate::crypto::digest(claim.payload.claim_nonce.as_str().as_bytes());
    ensure!(
        metadata(tx, "bootstrap_capability_hash")?.as_deref() == Some(expected.as_str()),
        "bootstrap capability was already consumed or changed"
    );
    let initialized: bool =
        tx.query_row("SELECT EXISTS(SELECT 1 FROM policy_pins)", [], |row| {
            row.get(0)
        })?;
    ensure!(!initialized, "target Node is already initialized");
    Ok(())
}

fn recovery_history(
    connection: &Connection,
    request: &CrossNodeRecoveryRequest,
) -> Result<ClientAuthorizationBundle> {
    let account = PolicyDomain::Account(request.account.clone());
    let space = PolicyDomain::Space(request.space.clone());
    let mut statement = connection.prepare("SELECT entry FROM policy_journal ORDER BY ordinal")?;
    let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
    let mut journal = Vec::new();
    for row in rows {
        let entry: JournalEntry = serde_json::from_str(&row?)?;
        let belongs = match &entry {
            JournalEntry::AccountGenesis(value) | JournalEntry::Account(value) => {
                value.payload.account == request.account
            }
            JournalEntry::AccountCommit(value) => value.proposal.payload.account == request.account,
            JournalEntry::SpaceGenesis(value) | JournalEntry::Space(value) => {
                value.payload.space == request.space
            }
            JournalEntry::SpaceCommit(value) => value.proposal.payload.space == request.space,
            JournalEntry::Device(value) => {
                value.payload.owner == account || value.payload.owner == space
            }
            JournalEntry::Authenticator(value) => value.payload.account == request.account,
            JournalEntry::Resource(value) => value.payload.space == request.space,
            JournalEntry::Membership(value) => value.payload.space == request.space,
            JournalEntry::Grant(value) => value.payload.owner_space == request.space,
        };
        if belongs {
            journal.push(entry);
            ensure!(journal.len() <= 128, "recovery history requires checkpoint");
        }
    }
    let grant_leaf = journal
        .iter()
        .rev()
        .find_map(|entry| match entry {
            JournalEntry::Grant(value)
                if value.payload.grantee
                    == Grantee::Actor(Actor::Account(request.account.clone())) =>
            {
                Some(value.hash())
            }
            _ => None,
        })
        .context("recovery history has no existing account grant anchor")??;
    Ok(ClientAuthorizationBundle {
        journal,
        grant_leaf,
    })
}

impl Store {
    pub(crate) fn cross_node_recovery_command(
        &mut self,
        identity: &LocalIdentity,
        principal: &str,
        method: &str,
        params: &Value,
        now: u64,
    ) -> Result<Value> {
        ensure!(
            serde_json::to_vec(params)?.len() <= 64 * 1024,
            "recovery input is too large"
        );
        PublicKey::parse(principal)?;
        match method {
            "cross_node_recovery_context" => {
                self.cross_node_recovery_context(identity, principal, params, now)
            }
            "cross_node_recovery_source_possession" => {
                self.cross_node_recovery_source_possession(identity, principal, params, now)
            }
            "cross_node_recovery_possession" => {
                self.cross_node_recovery_possession(identity, principal, params, now)
            }
            "cross_node_recovery_commit" => {
                self.cross_node_recovery_commit(identity, principal, params, now)
            }
            "cross_node_recovery_install" => {
                self.cross_node_recovery_install(identity, principal, params, now)
            }
            _ => bail!("unknown cross-Node recovery method"),
        }
    }

    fn cross_node_recovery_context(
        &mut self,
        identity: &LocalIdentity,
        principal: &str,
        params: &Value,
        now: u64,
    ) -> Result<Value> {
        let request: Signed<CrossNodeRecoveryRequest> =
            serde_json::from_value(params["request"].clone())?;
        require_principal(&request, principal)?;
        let signer =
            Identity::from_secret_hex(&identity.secret_key_hex).map_err(anyhow::Error::msg)?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let current = journal::PolicyState::load(&tx)?;
        cross_node::verify_cross_node_recovery_request(
            current.verified(),
            &request,
            &PublicKey::of(&signer),
            now,
        )?;
        let bundle = recovery_history(&tx, &request.payload)?;
        let context = cross_node::create_cross_node_recovery_witness(
            current.verified(),
            &request,
            &bundle,
            &signer,
            now,
        )?;
        let key = context_key(&request);
        if let Some(previous) = metadata(&tx, &key)? {
            let previous: SourceContext = serde_json::from_str(&previous)?;
            ensure!(
                ObjectHash::body(&previous.context.request)? == ObjectHash::body(&request)?
                    && previous.context.witness.payload.request_hash == ObjectHash::body(&request)?
                    && previous.context.witness.payload.expected_account_head
                        == context.witness.payload.expected_account_head
                    && previous.context.witness.payload.expected_space_head
                        == context.witness.payload.expected_space_head
                    && previous.context.witness.payload.context_hash
                        == context.witness.payload.context_hash,
                "recovery context or policy heads changed"
            );
            previous.context.witness.verify_signature()?;
            ensure!(
                previous.context.witness.issuer == PublicKey::of(&signer),
                "recovery witness belongs to another source Node"
            );
            tx.commit()?;
            self.policy = current;
            return Ok(json!({"context":previous.context}));
        }
        let record = SourceContext {
            context,
            source_possession: None,
        };
        tx.execute(
            "INSERT INTO identity_meta(key,value) VALUES(?,?)",
            params![key, serde_json::to_string(&record)?],
        )?;
        tx.commit()?;
        self.policy = current;
        Ok(json!({"context":record.context}))
    }

    fn cross_node_recovery_source_possession(
        &mut self,
        identity: &LocalIdentity,
        principal: &str,
        params: &Value,
        now: u64,
    ) -> Result<Value> {
        let request: CrossNodeRecoveryPossessionRequest = serde_json::from_value(params.clone())?;
        require_principal(&request.context.request, principal)?;
        let signer =
            Identity::from_secret_hex(&identity.secret_key_hex).map_err(anyhow::Error::msg)?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let current = journal::PolicyState::load(&tx)?;
        let key = context_key(&request.context.request);
        let encoded = metadata(&tx, &key)?.context("source recovery witness is missing")?;
        let mut record: SourceContext = serde_json::from_str(&encoded)?;
        ensure!(
            ObjectHash::body(&record.context)? == ObjectHash::body(&request.context)?,
            "recovery witness differs from the source's approved context"
        );
        let proof = cross_node::sign_cross_node_recovery_source_possession(
            current.verified(),
            &request,
            &signer,
            now,
        )?;
        let possession_hash = ObjectHash::body(&request)?;
        if let Some(previous) = &record.source_possession {
            ensure!(
                previous.possession_hash == possession_hash && previous.proof == proof,
                "source recovery witness was already bound to another proposal"
            );
        } else {
            record.source_possession = Some(SourcePossession {
                possession_hash,
                proof: proof.clone(),
            });
            tx.execute(
                "UPDATE identity_meta SET value=? WHERE key=?",
                params![serde_json::to_string(&record)?, key],
            )?;
        }
        tx.commit()?;
        self.policy = current;
        Ok(json!({"proof":proof}))
    }

    fn cross_node_recovery_possession(
        &mut self,
        identity: &LocalIdentity,
        principal: &str,
        params: &Value,
        now: u64,
    ) -> Result<Value> {
        let request: CrossNodeRecoveryPossessionRequest = serde_json::from_value(params.clone())?;
        let signed = &request.context.request;
        require_principal(signed, principal)?;
        let local = PublicKey::parse(&identity.public_key_hex()?)?;
        let request_hash = ObjectHash::body(signed)?;
        let possession_hash = ObjectHash::body(&request)?;
        let claim = &signed.payload.target_claim;
        let signer =
            Identity::from_secret_hex(&identity.secret_key_hex).map_err(anyhow::Error::msg)?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        check_claim_in(&tx, claim, &local)?;
        if let Some(previous) = metadata(&tx, "node_possession")? {
            let previous: TargetPossession = serde_json::from_str(&previous)?;
            ensure!(
                previous.request_hash == request_hash
                    && previous.possession_hash == possession_hash
                    && previous.principal == principal,
                "bootstrap capability was already bound to another recovery"
            );
            return Ok(json!({"proof":previous.proof}));
        }
        let proof = cross_node::sign_cross_node_recovery_possession(
            &request,
            &signer,
            &claim.payload.claim_nonce,
            now,
        )?;
        let record = TargetPossession {
            request_hash,
            possession_hash,
            principal: principal.to_owned(),
            request,
            proof: proof.clone(),
        };
        tx.execute(
            "INSERT INTO identity_meta(key,value) VALUES('node_possession',?)",
            [serde_json::to_string(&record)?],
        )?;
        tx.commit()?;
        Ok(json!({"proof":proof}))
    }

    fn cross_node_recovery_commit(
        &mut self,
        identity: &LocalIdentity,
        principal: &str,
        params: &Value,
        now: u64,
    ) -> Result<Value> {
        let request: CrossNodeRecoveryCommitRequest = serde_json::from_value(params.clone())?;
        let signed = &request.possession.context.request;
        require_principal(signed, principal)?;
        let local = PublicKey::parse(&identity.public_key_hex()?)?;
        ensure!(
            signed.payload.source_node_public_key == local,
            "recovery commits at another source Node"
        );
        let request_hash = ObjectHash::body(signed)?;
        let input_hash = ObjectHash::body(&request)?;
        let receipt_key = format!("cross-node-recovery-completed:{request_hash}");
        let signer =
            Identity::from_secret_hex(&identity.secret_key_hex).map_err(anyhow::Error::msg)?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let Some(previous) = metadata(&tx, &receipt_key)? {
            let previous: Value = serde_json::from_str(&previous)?;
            ensure!(
                previous["input_hash"] == input_hash.as_str() && previous["principal"] == principal,
                "recovery completion changed its original signed request"
            );
            return Ok(json!({"committed":previous["committed"]}));
        }
        let current = journal::PolicyState::load(&tx)?;
        let context: SourceContext = serde_json::from_str(
            &metadata(&tx, &context_key(signed))?.context("source recovery witness is missing")?,
        )?;
        ensure!(
            ObjectHash::body(&context.context)? == ObjectHash::body(&request.possession.context)?,
            "recovery changed its source witness"
        );
        let possession = context
            .source_possession
            .context("source Node possession is missing")?;
        ensure!(
            possession.possession_hash == ObjectHash::body(&request.possession)?,
            "recovery changed its approved source certificate"
        );
        let verified = cross_node::verify_cross_node_recovery_commit(
            current.verified(),
            &request,
            &local,
            now,
        )?;
        let update = current.prepare_network_update(verified.journal_delta(), now)?;
        let source = update
            .verified_after()
            .verify_transport_device(&DeviceId::from_key(&local), now)?;
        ensure!(
            source.payload.possession.as_ref() == Some(&possession.proof),
            "recovery changed source Node possession"
        );
        let state = journal::commit_in(&tx, &update)?;
        project_policy_in(&tx, &state)?;
        let committed = cross_node::accept_cross_node_recovery_commit(&verified, &signer, now)?;
        tx.execute("INSERT INTO identity_meta(key,value) VALUES(?,?)",params![receipt_key,
            serde_json::to_string(&json!({"request_hash":request_hash,"input_hash":input_hash,"principal":principal,"committed":committed}))?])?;
        tx.commit()?;
        self.policy = state;
        Ok(json!({"committed":committed}))
    }

    fn cross_node_recovery_install(
        &mut self,
        identity: &LocalIdentity,
        principal: &str,
        params: &Value,
        now: u64,
    ) -> Result<Value> {
        let committed: CrossNodeRecoveryCommitted = serde_json::from_value(params.clone())?;
        let signed = &committed.request.possession.context.request;
        require_principal(signed, principal)?;
        let local = PublicKey::parse(&identity.public_key_hex()?)?;
        let claim = &signed.payload.target_claim;
        ensure!(
            claim.payload.node_public_key == local,
            "recovery installation targets another Node"
        );
        let input_hash = ObjectHash::body(&committed.request)?;
        let completion_hash = ObjectHash::body(&committed)?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let Some(previous) = metadata(&tx, "cross_node_recovery_installation")? {
            let previous: Value = serde_json::from_str(&previous)?;
            ensure!(
                previous["completion_hash"] == completion_hash.as_str()
                    && previous["principal"] == principal,
                "Node already installed another recovery completion"
            );
            return Ok(previous["response"].clone());
        }
        check_claim_in(&tx, claim, &local)?;
        let possession: TargetPossession = serde_json::from_str(
            &metadata(&tx, "node_possession")?.context("target Node possession is missing")?,
        )?;
        ensure!(
            possession.request_hash == ObjectHash::body(signed)?
                && possession.possession_hash == ObjectHash::body(&committed.request.possession)?
                && possession.principal == principal,
            "recovery installation differs from this Node's approved possession"
        );
        let verified = cross_node::verify_cross_node_recovery_installation(
            &committed,
            &local,
            &claim.payload.claim_nonce,
            now,
        )?;
        ensure!(
            verified.input_hash()? == input_hash
                && verified.possession_hash()? == possession.possession_hash,
            "recovery installation changed its committed request"
        );
        let current = journal::PolicyState::load(&tx)?;
        let update = current.prepare_bootstrap(verified.pins(), &verified.bundle().journal, now)?;
        let target = update
            .verified_after()
            .verify_transport_device(&DeviceId::from_key(&local), now)?;
        ensure!(
            target.payload.possession.as_ref() == Some(&possession.proof),
            "recovery changed target Node possession"
        );
        update.verified_after().verify_account_client_device(
            verified.account(),
            &DeviceId::from_key(verified.new_device_public_key()),
            now,
        )?;
        let state = journal::commit_in(&tx, &update)?;
        project_policy_in(&tx, &state)?;
        ensure!(tx.execute("UPDATE identity_meta SET value='consumed:'||value WHERE key='bootstrap_capability_hash' AND value=?",
            [crate::crypto::digest(claim.payload.claim_nonce.as_str().as_bytes())])? == 1, "bootstrap capability was already consumed");
        let response = json!({"installed":true,"account_id":verified.account(),"node_public_key":verified.target_node_public_key(),"pins":verified.pins()});
        tx.execute("INSERT INTO identity_meta(key,value) VALUES('cross_node_recovery_installation',?)",
            [serde_json::to_string(&json!({"request_hash":verified.request_hash()?,"input_hash":input_hash,"completion_hash":completion_hash,"principal":principal,"response":response}))?])?;
        tx.commit()?;
        self.policy = state;
        Ok(response)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::identity::{
        local::BootstrapCapability,
        policy::FreshnessProof,
        test_support::{TestNetwork, now},
    };
    use vibeface_secure::authorization::{
        AUTH_VERSION, Audience, CheckStage, Invocation, Nonce, ResourceRef, ServiceId,
        TransportEvidence,
        bootstrap::{self, VerifiedNodeClaim, recovery},
    };

    struct Fixture {
        network: TestNetwork,
        source: Store,
        target: Store,
        target_node: LocalIdentity,
        next_root: Identity,
        next_client: Identity,
        capability: BootstrapCapability,
        claim: VerifiedNodeClaim,
        material: String,
        request: Signed<CrossNodeRecoveryRequest>,
        original_event: crate::model::SignedEvent,
    }

    impl Fixture {
        fn new() -> Result<Self> {
            let network = TestNetwork::new()?;
            let mut source = network.at(&network.directory.join("source.sqlite3"))?;
            network.conversation(&mut source, "archive")?;
            let original_event = network.message(&mut source, "archive", "preserved")?;
            let mut target = Store::initialize(&network.directory.join("target.sqlite3"))?;
            let target_node = LocalIdentity::generate();
            let target_signer = Identity::from_secret_hex(&target_node.secret_key_hex)
                .map_err(anyhow::Error::msg)?;
            let capability = BootstrapCapability::generate();
            target.install_bootstrap_capability(&capability)?;
            let claim_json = bootstrap::create_node_claim(
                &target_signer,
                &capability.capability,
                now() as f64,
                (now() + 100_000) as f64,
            )
            .map_err(anyhow::Error::msg)?;
            target.connection.execute(
                "INSERT INTO identity_meta(key,value) VALUES('node_claim',?)",
                [&claim_json],
            )?;
            let claim = bootstrap::verify_node_claim(
                &claim_json,
                &target_node.public_key_hex()?,
                &capability.capability,
                now() as f64,
            )
            .map_err(anyhow::Error::msg)?;
            let next_root = Identity::generate().map_err(anyhow::Error::msg)?;
            let next_client = Identity::generate().map_err(anyhow::Error::msg)?;
            let material = recovery::build_account_recovery_material(
                &network.created,
                &network.root,
                &network.recovery,
                "https://source.example",
                now() as f64,
            )
            .map_err(anyhow::Error::msg)?;
            let request = Self::request(&network, &material, &next_root, &next_client, &claim)?;
            Ok(Self {
                network,
                source,
                target,
                target_node,
                next_root,
                next_client,
                capability,
                claim,
                material,
                request,
                original_event,
            })
        }

        fn request(
            network: &TestNetwork,
            material: &str,
            root: &Identity,
            client: &Identity,
            claim: &VerifiedNodeClaim,
        ) -> Result<Signed<CrossNodeRecoveryRequest>> {
            let resources = [ResourceRef {
                space: network.space.clone(),
                resource: network.resource.clone(),
            }];
            Ok(serde_json::from_str(
                &cross_node::build_cross_node_recovery_request(
                    material,
                    root,
                    client,
                    claim,
                    &serde_json::to_string(&resources)?,
                    (now() + 900_000) as f64,
                    now() as f64,
                    (now() + 60_000) as f64,
                )
                .map_err(anyhow::Error::msg)?,
            )?)
        }

        fn prepare(&mut self) -> Result<Value> {
            let response = self.source.cross_node_recovery_command(
                &self.network.node,
                &self.next_client.public_key_hex(),
                "cross_node_recovery_context",
                &json!({"request":self.request}),
                now(),
            )?;
            let verified = cross_node::verify_cross_node_recovery_context(
                &serde_json::to_string(&response["context"])?,
                &serde_json::to_string(&self.request)?,
                now() as f64,
            )
            .map_err(anyhow::Error::msg)?;
            let prepared = cross_node::prepare_cross_node_account_recovery(
                &self.material,
                &verified,
                &self.next_root,
                &self.next_client,
                (now() + 1_000_000) as f64,
                now() as f64,
            )
            .map_err(anyhow::Error::msg)?;
            Ok(serde_json::from_str(
                &prepared
                    .possession_request_json()
                    .map_err(anyhow::Error::msg)?,
            )?)
        }

        fn commit_request(
            &mut self,
        ) -> Result<(cross_node::PreparedCrossNodeRecoveryCommit, Value)> {
            let response = self.source.cross_node_recovery_command(
                &self.network.node,
                &self.next_client.public_key_hex(),
                "cross_node_recovery_context",
                &json!({"request":self.request}),
                now(),
            )?;
            let verified = cross_node::verify_cross_node_recovery_context(
                &serde_json::to_string(&response["context"])?,
                &serde_json::to_string(&self.request)?,
                now() as f64,
            )
            .map_err(anyhow::Error::msg)?;
            let prepared = cross_node::prepare_cross_node_account_recovery(
                &self.material,
                &verified,
                &self.next_root,
                &self.next_client,
                (now() + 1_000_000) as f64,
                now() as f64,
            )
            .map_err(anyhow::Error::msg)?;
            let request: Value = serde_json::from_str(
                &prepared
                    .possession_request_json()
                    .map_err(anyhow::Error::msg)?,
            )?;
            let source = self.source.cross_node_recovery_command(
                &self.network.node,
                &self.next_client.public_key_hex(),
                "cross_node_recovery_source_possession",
                &request,
                now(),
            )?;
            let target = self.target.cross_node_recovery_command(
                &self.target_node,
                &self.next_client.public_key_hex(),
                "cross_node_recovery_possession",
                &request,
                now(),
            )?;
            let commit = cross_node::finish_cross_node_account_recovery(
                &prepared,
                &self.next_root,
                target["proof"].as_str().context("target proof missing")?,
                source["proof"].as_str().context("source proof missing")?,
                now() as f64,
            )
            .map_err(anyhow::Error::msg)?;
            let request =
                serde_json::from_str(&commit.request_json().map_err(anyhow::Error::msg)?)?;
            Ok((commit, request))
        }

        fn source_commit(&mut self, request: &Value, at: u64) -> Result<Value> {
            self.source.cross_node_recovery_command(
                &self.network.node,
                &self.next_client.public_key_hex(),
                "cross_node_recovery_commit",
                request,
                at,
            )
        }

        fn install(&mut self, committed: &Value, at: u64) -> Result<Value> {
            self.target.cross_node_recovery_command(
                &self.target_node,
                &self.next_client.public_key_hex(),
                "cross_node_recovery_install",
                committed,
                at,
            )
        }

        fn read_invocation(
            &self,
            method: &str,
        ) -> Result<(
            Signed<Invocation>,
            vibeface_secure::authorization::RequestFacts,
            TransportEvidence,
        )> {
            let response = self.source.authenticate_account(&self.next_client.public_key_hex(),
                &json!({"account_id":self.network.account,"space_id":self.network.space,
                    "resource_id":self.network.resource,"device_public_key":self.next_client.public_key_hex()}),now())?;
            let bundle: ClientAuthorizationBundle =
                serde_json::from_value(response["authorization"].clone())?;
            let source_key = PublicKey::parse(&self.network.node.public_key_hex()?)?;
            let audience = Audience {
                service: ServiceId::from_key(&source_key),
                device: DeviceId::from_key(&source_key),
            };
            let mut body = self.network.body(method, "archive");
            if method == "message" {
                body["text"] = json!("denied write");
                body["message_id"] = json!(uuid::Uuid::new_v4().to_string());
            }
            let facts = self.source.request_facts(&body, &audience)?;
            let device = DeviceId::from_key(&PublicKey::of(&self.next_client));
            let expected_policies = [
                PolicyDomain::Account(self.network.account.clone()),
                PolicyDomain::Space(self.network.space.clone()),
            ]
            .iter()
            .map(|domain| {
                self.source
                    .authorization_state()
                    .head(domain)
                    .cloned()
                    .context("head missing")
            })
            .collect::<Result<Vec<_>>>()?;
            let signed = Signed::sign(
                Invocation {
                    version: AUTH_VERSION,
                    actor: Actor::Account(self.network.account.clone()),
                    device: device.clone(),
                    initiator: Actor::Account(self.network.account.clone()),
                    initiating_grant: None,
                    resource: facts.resource.clone(),
                    action: facts.action,
                    audience,
                    presenter: device.clone(),
                    operation_id: facts.operation_id.clone(),
                    body_hash: facts.body_hash.clone(),
                    grant_leaf: bundle.grant_leaf,
                    expected_policies,
                    context: facts.context.clone(),
                    nonce: Nonce::random()?,
                    not_before_ms: now(),
                    expires_at_ms: now() + 100_000,
                },
                &self.next_client,
            )?;
            Ok((signed, facts, TransportEvidence::DirectNoise { device }))
        }
    }

    fn journal_count(store: &Store) -> Result<u64> {
        Ok(store
            .connection
            .query_row("SELECT count(*) FROM policy_journal", [], |row| row.get(0))?)
    }

    #[test]
    fn source_commits_before_target_install_and_old_project_remains_read_only() -> Result<()> {
        let mut f = Fixture::new()?;
        let (prepared, request) = f.commit_request()?;
        let response = f.source_commit(&request, now())?;
        assert_eq!(
            ObjectHash::body(&f.source_commit(&request, now() + 200_000)?)?,
            ObjectHash::body(&response)?
        );
        let result = cross_node::finish_cross_node_recovery_commit(
            &prepared,
            &serde_json::to_string(&response["committed"])?,
            now() as f64,
        )
        .map_err(anyhow::Error::msg)?;
        let installation: Value =
            serde_json::from_str(&result.installation_json().map_err(anyhow::Error::msg)?)?;
        let installed = f.install(&installation, now())?;
        assert_eq!(installed["account_id"], f.network.account.as_str());
        assert_eq!(
            installed["node_public_key"],
            f.target_node.public_key_hex()?
        );
        assert_eq!(f.install(&installation, now() + 200_000)?, installed);
        assert!(
            f.target
                .verify_bootstrap_capability(&f.capability.capability)
                .is_err()
        );
        let source_state = f.source.authorization_state();
        assert_eq!(
            source_state
                .account(&f.network.account)
                .context("account missing")?
                .authority
                .public_key
                .as_str(),
            f.target_node.public_key_hex()?
        );
        assert_eq!(
            source_state
                .resource(&f.network.space, &f.network.resource)
                .context("old project missing")?
                .payload
                .host,
            ServiceId::from_key(&PublicKey::parse(&f.network.node.public_key_hex()?)?)
        );
        assert!(
            source_state
                .verify_account_client_device(
                    &f.network.account,
                    &DeviceId::from_key(&PublicKey::of(&f.network.client)),
                    now()
                )
                .is_err()
        );
        let (read, facts, transport) = f.read_invocation("conversation")?;
        let auth = f
            .source
            .authorize_request(&read, &facts, &transport, now())?;
        let challenge = f
            .source
            .issue_freshness(&auth, CheckStage::RemoteRead, now())?;
        let old_source_receipts =
            f.source
                .sign_current_receipts(&challenge, &f.network.node, now())?;
        assert!(old_source_receipts.is_empty());
        assert!(
            f.source
                .authorize_read(
                    &auth,
                    &FreshnessProof {
                        challenge: challenge.clone(),
                        receipts: old_source_receipts,
                    },
                    &transport,
                    now()
                )
                .is_err()
        );
        let receipts = f
            .target
            .sign_current_receipts(&challenge, &f.target_node, now())?;
        f.source.authorize_read(
            &auth,
            &FreshnessProof {
                challenge,
                receipts,
            },
            &transport,
            now(),
        )?;
        assert_eq!(
            f.source
                .event(f.network.space.as_str(), &f.original_event.hash)?,
            Some(f.original_event.clone())
        );
        let (write, facts, transport) = f.read_invocation("message")?;
        assert!(
            f.source
                .authorize_request(&write, &facts, &transport, now())
                .is_err()
        );
        let source_path = f.network.directory.join("source.sqlite3");
        let target_path = f.network.directory.join("target.sqlite3");
        drop(f.source);
        drop(f.target);
        let mut reopened_source = Store::open(source_path)?;
        let mut reopened_target = Store::open(target_path)?;
        assert_eq!(
            ObjectHash::body(&reopened_source.cross_node_recovery_command(
                &f.network.node,
                &f.next_client.public_key_hex(),
                "cross_node_recovery_commit",
                &request,
                now() + 200_000
            )?)?,
            ObjectHash::body(&response)?
        );
        assert_eq!(
            reopened_target.cross_node_recovery_command(
                &f.target_node,
                &f.next_client.public_key_hex(),
                "cross_node_recovery_install",
                &installation,
                now() + 200_000
            )?,
            installed
        );
        Ok(())
    }

    #[test]
    fn wrong_noise_wrong_node_and_cross_account_capability_reuse_are_rejected() -> Result<()> {
        let mut f = Fixture::new()?;
        assert!(
            f.source
                .cross_node_recovery_command(
                    &f.network.node,
                    &Identity::generate()
                        .map_err(anyhow::Error::msg)?
                        .public_key_hex(),
                    "cross_node_recovery_context",
                    &json!({"request":f.request}),
                    now()
                )
                .is_err()
        );
        assert!(metadata(&f.source.connection, &context_key(&f.request))?.is_none());
        let request = f.prepare()?;
        assert!(
            f.target
                .cross_node_recovery_command(
                    &LocalIdentity::generate(),
                    &f.next_client.public_key_hex(),
                    "cross_node_recovery_possession",
                    &request,
                    now()
                )
                .is_err()
        );
        assert!(metadata(&f.target.connection, "node_possession")?.is_none());
        f.target.cross_node_recovery_command(
            &f.target_node,
            &f.next_client.public_key_hex(),
            "cross_node_recovery_possession",
            &request,
            now(),
        )?;
        let other = TestNetwork::new()?;
        let mut other_source = other.memory()?;
        let material = recovery::build_account_recovery_material(
            &other.created,
            &other.root,
            &other.recovery,
            "https://source.example",
            now() as f64,
        )
        .map_err(anyhow::Error::msg)?;
        let signed = Fixture::request(&other, &material, &f.next_root, &f.next_client, &f.claim)?;
        let context = other_source.cross_node_recovery_command(
            &other.node,
            &f.next_client.public_key_hex(),
            "cross_node_recovery_context",
            &json!({"request":signed}),
            now(),
        )?;
        let verified = cross_node::verify_cross_node_recovery_context(
            &serde_json::to_string(&context["context"])?,
            &serde_json::to_string(&signed)?,
            now() as f64,
        )
        .map_err(anyhow::Error::msg)?;
        let prepared = cross_node::prepare_cross_node_account_recovery(
            &material,
            &verified,
            &f.next_root,
            &f.next_client,
            (now() + 1_000_000) as f64,
            now() as f64,
        )
        .map_err(anyhow::Error::msg)?;
        let changed = serde_json::from_str(
            &prepared
                .possession_request_json()
                .map_err(anyhow::Error::msg)?,
        )?;
        assert!(
            f.target
                .cross_node_recovery_command(
                    &f.target_node,
                    &f.next_client.public_key_hex(),
                    "cross_node_recovery_possession",
                    &changed,
                    now()
                )
                .is_err()
        );
        assert_eq!(journal_count(&f.target)?, 0);
        f.target
            .verify_bootstrap_capability(&f.capability.capability)?;
        Ok(())
    }

    #[test]
    fn either_source_head_advance_rejects_the_complete_recovery_cas() -> Result<()> {
        for account in [true, false] {
            let mut f = Fixture::new()?;
            let (_, request) = f.commit_request()?;
            let mut independent = Store::open(f.network.directory.join("source.sqlite3"))?;
            let entry = if account {
                let mut doc = independent
                    .authorization_state()
                    .account(&f.network.account)
                    .context("account missing")?
                    .clone();
                doc.previous = independent
                    .authorization_state()
                    .head(&PolicyDomain::Account(f.network.account.clone()))
                    .cloned();
                doc.sequence += 1;
                JournalEntry::Account(Signed::sign(doc, &f.network.root)?)
            } else {
                let mut doc = independent
                    .authorization_state()
                    .space(&f.network.space)
                    .context("space missing")?
                    .clone();
                doc.previous = independent
                    .authorization_state()
                    .head(&PolicyDomain::Space(f.network.space.clone()))
                    .cloned();
                doc.sequence += 1;
                JournalEntry::Space(Signed::sign(doc, &f.network.root)?)
            };
            independent.install_policy_journal(&[entry], now() + 1)?;
            let before = journal_count(&independent)?;
            assert!(f.source_commit(&request, now() + 2).is_err());
            assert_eq!(journal_count(&f.source)?, before);
            assert!(
                metadata(
                    &f.source.connection,
                    &format!(
                        "cross-node-recovery-completed:{}",
                        ObjectHash::body(&f.request)?
                    )
                )?
                .is_none()
            );
            assert_eq!(journal_count(&f.target)?, 0);
            f.target
                .verify_bootstrap_capability(&f.capability.capability)?;
        }
        Ok(())
    }

    #[test]
    fn receipt_and_install_failures_roll_back_and_reconcile_after_request_expiry() -> Result<()> {
        let mut f = Fixture::new()?;
        let (_, request) = f.commit_request()?;
        let before = journal_count(&f.source)?;
        f.source.connection.execute_batch("CREATE TEMP TRIGGER fail_cross_commit BEFORE INSERT ON identity_meta WHEN NEW.key GLOB 'cross-node-recovery-completed:*' BEGIN SELECT RAISE(ABORT,'expected source receipt failure'); END;")?;
        assert!(f.source_commit(&request, now()).is_err());
        assert_eq!(journal_count(&f.source)?, before);
        assert_eq!(
            f.source
                .authorization_state()
                .account(&f.network.account)
                .context("account missing")?
                .current_root,
            PublicKey::of(&f.network.root)
        );
        f.source
            .connection
            .execute_batch("DROP TRIGGER fail_cross_commit")?;
        let response = f.source_commit(&request, now())?;
        let mut changed = request.clone();
        changed["pins"][0]["minimum_head"]["sequence"] = json!(999);
        assert!(f.source_commit(&changed, now() + 200_000).is_err());
        assert_eq!(
            ObjectHash::body(&f.source_commit(&request, now() + 200_000)?)?,
            ObjectHash::body(&response)?
        );
        let committed = &response["committed"];
        let mut forged = committed.clone();
        forged["receipt"]["payload"]["input_hash"] = json!(ObjectHash::body(&"forged transition")?);
        assert!(f.install(&forged, now()).is_err());
        assert_eq!(journal_count(&f.target)?, 0);
        f.target.connection.execute_batch("CREATE TEMP TRIGGER fail_cross_install BEFORE INSERT ON identity_meta WHEN NEW.key='cross_node_recovery_installation' BEGIN SELECT RAISE(ABORT,'expected target receipt failure'); END;")?;
        assert!(f.install(committed, now()).is_err());
        assert_eq!(journal_count(&f.target)?, 0);
        f.target
            .verify_bootstrap_capability(&f.capability.capability)?;
        assert!(f.target.policy.pins().is_empty());
        assert!(metadata(&f.target.connection, "cross_node_recovery_installation")?.is_none());
        f.target
            .connection
            .execute_batch("DROP TRIGGER fail_cross_install")?;
        assert_eq!(f.install(committed, now() + 200_000)?["installed"], true);
        assert!(
            f.target
                .verify_bootstrap_capability(&f.capability.capability)
                .is_err()
        );
        Ok(())
    }
}
