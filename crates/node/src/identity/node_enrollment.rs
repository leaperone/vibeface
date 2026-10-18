use super::{journal, policy::project_policy_in};
use crate::{crypto::LocalIdentity, store::Store};
use anyhow::{Context, Result, bail, ensure};
use rusqlite::{OptionalExtension, Transaction, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use vibeface_secure::authorization::{
    DeviceId, ObjectHash, PublicKey, Signed,
    bootstrap::{
        NodeClaim,
        recovery::node_enrollment::{
            self, AccountNodeEnrollmentCommitRequest, AccountNodeEnrollmentCredential,
            AccountNodeInstallation,
        },
    },
};

mod import_cache;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PossessionRecord {
    purpose: String,
    request_hash: ObjectHash,
    context: ObjectHash,
    principal: String,
    credential: AccountNodeEnrollmentCredential,
    approved_at_ms: u64,
    approval: Signed<node_enrollment::AccountNodeEnrollmentImportApproval>,
    proof: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct InstallationRequest {
    context: ObjectHash,
    installation: AccountNodeInstallation,
}

fn metadata(connection: &rusqlite::Connection, key: &str) -> Result<Option<String>> {
    Ok(connection
        .query_row(
            "SELECT value FROM identity_meta WHERE key=?",
            [key],
            |row| row.get(0),
        )
        .optional()?)
}

fn require_principal(request: &AccountNodeEnrollmentCommitRequest, principal: &str) -> Result<()> {
    ensure!(
        request.intent.payload.requesting_device
            == DeviceId::from_key(&PublicKey::parse(principal)?),
        "node enrollment belongs to another Noise client"
    );
    request.intent.verify_signature()?;
    Ok(())
}

fn check_claim_in(tx: &Transaction<'_>, claim: &Signed<NodeClaim>) -> Result<()> {
    let saved = metadata(tx, "node_claim")?.context("local Node claim is missing")?;
    ensure!(
        serde_json::from_str::<Signed<NodeClaim>>(&saved)? == *claim,
        "claim is not the current local Node challenge"
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
    ensure!(!initialized, "Node is already initialized");
    Ok(())
}

fn bounded_response(params: &Value, response: Value) -> Result<Value> {
    let total = serde_json::to_vec(params)?
        .len()
        .checked_add(serde_json::to_vec(&response)?.len())
        .context("Node enrollment carrier size overflow")?;
    ensure!(
        total <= vibeface_secure::authorization::MAX_PROOF_BYTES,
        "Node enrollment request and response exceed the carrier limit"
    );
    Ok(response)
}

impl Store {
    pub(crate) fn node_enrollment_command(
        &mut self,
        identity: &LocalIdentity,
        principal: &str,
        method: &str,
        params: &Value,
        now: u64,
    ) -> Result<Value> {
        let _scope = vibeface_secure::signature_verification::ScopedSignatureVerification::new();
        ensure!(
            serde_json::to_vec(params)?.len() <= vibeface_secure::authorization::MAX_PROOF_BYTES,
            "node enrollment input is too large"
        );
        match method {
            "account_node_import_begin" => {
                self.account_node_import_begin(identity, principal, params, now)
            }
            "account_node_import_page" => {
                self.account_node_import_page(identity, principal, params, now)
            }
            "account_node_import_finish" => {
                self.account_node_import_finish(identity, principal, params, now)
            }
            "account_node_import_resume" => {
                self.account_node_import_resume(identity, principal, params, now)
            }
            "account_node_commit" => self.account_node_commit(identity, principal, params, now),
            "account_node_install" => self.account_node_install(identity, principal, params, now),
            _ => bail!("unknown node enrollment method"),
        }
    }

    fn account_node_commit(
        &mut self,
        identity: &LocalIdentity,
        principal: &str,
        params: &Value,
        now: u64,
    ) -> Result<Value> {
        let request: AccountNodeEnrollmentCommitRequest = serde_json::from_value(params.clone())?;
        require_principal(&request, principal)?;
        let account = &request.intent.payload.account;
        let signer = vibeface_secure::Identity::from_secret_hex(&identity.secret_key_hex)
            .map_err(anyhow::Error::msg)?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let current = journal::PolicyState::reload_verified(&tx, &self.policy)?;
        let document = current
            .verified()
            .account(account)
            .context("account policy is unavailable")?;
        current.verified().verify_account_client_device(
            account,
            &request.intent.payload.requesting_device,
            now,
        )?;
        ensure!(
            document.authority.public_key.as_str() == identity.public_key_hex()?,
            "node enrollment requires the current local account authority"
        );
        let request_hash = ObjectHash::body(&request.intent)?;
        let input_hash = ObjectHash::body(&request)?;
        let receipt_key = format!("node-enrollment-commit:{request_hash}");
        if let Some(receipt) = metadata(&tx, &receipt_key)? {
            let receipt: Value = serde_json::from_str(&receipt)?;
            ensure!(
                receipt["input_hash"] == input_hash.as_str() && receipt["principal"] == principal,
                "node enrollment completion changed its signed request"
            );
            let response = bounded_response(params, receipt["response"].clone())?;
            tx.commit()?;
            self.policy = current;
            return Ok(response);
        }
        let delta = node_enrollment::commit_account_node_enrollment_in_state(
            &request,
            current.verified(),
            &signer,
            now,
        )?;
        let update = current.prepare_network_update(&delta, now)?;
        let response = bounded_response(params, json!({"journal_delta":delta}))?;
        let receipt = json!({"request_hash":request_hash,"input_hash":input_hash,"principal":principal,"response":response});
        let state = journal::commit_in(&tx, &update)?;
        project_policy_in(&tx, &state)?;
        tx.execute(
            "INSERT INTO identity_meta(key,value) VALUES(?,?)",
            params![receipt_key, serde_json::to_string(&receipt)?],
        )?;
        tx.commit()?;
        self.policy = state;
        Ok(response)
    }

    fn account_node_install(
        &mut self,
        identity: &LocalIdentity,
        principal: &str,
        params: &Value,
        now: u64,
    ) -> Result<Value> {
        let input: InstallationRequest = serde_json::from_value(params.clone())?;
        let installation = &input.installation;
        require_principal(&installation.request, principal)?;
        ensure!(
            installation
                .request
                .intent
                .payload
                .claim
                .payload
                .node_public_key
                .as_str()
                == identity.public_key_hex()?,
            "installation belongs to another local Node"
        );
        let input_hash = ObjectHash::body(params)?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let current = journal::PolicyState::reload_verified(&tx, &self.policy)?;
        if let Some(receipt) = metadata(&tx, "account_node_installation")? {
            let receipt: Value = serde_json::from_str(&receipt)?;
            ensure!(
                receipt["input_hash"] == input_hash.as_str() && receipt["principal"] == principal,
                "Node has already installed another account enrollment"
            );
            current.verified().verify_account_client_device(
                &installation.request.intent.payload.account,
                &installation.request.intent.payload.requesting_device,
                now,
            )?;
            current.verified().verify_transport_device(
                &DeviceId::from_key(&PublicKey::parse(&identity.public_key_hex()?)?),
                now,
            )?;
            let response = bounded_response(params, receipt["response"].clone())?;
            tx.commit()?;
            self.policy = current;
            return Ok(response);
        }
        check_claim_in(&tx, &installation.request.intent.payload.claim)?;
        let saved: PossessionRecord = serde_json::from_str(
            &metadata(&tx, "node_possession")?.context("Node possession is missing")?,
        )?;
        import_cache::verify_saved_possession(&saved)?;
        ensure!(
            saved.purpose == "account_node"
                && saved.request_hash == ObjectHash::body(&installation.request.intent)?
                && saved.context == input.context
                && saved.principal == principal
                && saved.credential.intent == installation.request.intent
                && saved.credential.descriptor == installation.request.descriptor
                && installation
                    .request
                    .certificate
                    .payload
                    .possession
                    .as_ref()
                    .map(|proof| proof.as_str())
                    == Some(saved.proof.as_str()),
            "installation differs from this Node's approved purpose and possession request"
        );
        let importer = self
            .account_node_enrollment_import
            .as_ref()
            .context("Node enrollment context must be resumed before installation")?;
        ensure!(
            importer.context_hash()? == input.context,
            "Node enrollment context changed"
        );
        let context = importer
            .completed_context()
            .context("Node enrollment policy import is incomplete")?;
        ensure!(
            context.approved_at_ms() == saved.approved_at_ms,
            "Node enrollment changed its original approval clock"
        );
        let signer = vibeface_secure::Identity::from_secret_hex(&identity.secret_key_hex)
            .map_err(anyhow::Error::msg)?;
        let verified = node_enrollment::verify_account_node_installation(
            context,
            &serde_json::to_string(installation)?,
            &signer,
            installation
                .request
                .intent
                .payload
                .claim
                .payload
                .claim_nonce
                .as_str(),
            now as f64,
        )
        .map_err(anyhow::Error::msg)?;
        ensure!(
            verified.request_hash() == &saved.request_hash,
            "verified installation differs from this local Node request"
        );
        let mut usage = verified.proof_usage().clone();
        usage.record_signed(&saved.approval)?;
        usage.record_metadata(params)?;
        usage.enforce_limits()?;
        let response = bounded_response(
            params,
            json!({"installed":true,"account_id":verified.account(),
            "node_public_key":verified.node_public_key(),"pins":verified.pins()}),
        )?;
        let receipt = json!({"request_hash":verified.request_hash(),"input_hash":input_hash,
            "principal":principal,"response":response});
        let update = current.prepare_account_node_installation(verified, now)?;
        let state = journal::commit_in(&tx, &update)?;
        project_policy_in(&tx, &state)?;
        ensure!(tx.execute("UPDATE identity_meta SET value='consumed:'||value WHERE key='bootstrap_capability_hash' AND value=?",
            [crate::crypto::digest(installation.request.intent.payload.claim.payload.claim_nonce.as_str().as_bytes())])? == 1,
            "bootstrap capability was already consumed");
        tx.execute(
            "INSERT INTO identity_meta(key,value) VALUES('account_node_installation',?)",
            [serde_json::to_string(&receipt)?],
        )?;
        tx.commit()?;
        self.policy = state;
        self.account_node_enrollment_import = None;
        Ok(response)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::identity::{
        local::BootstrapCapability,
        test_support::{TestNetwork, now},
    };
    use vibeface_secure::{
        Identity,
        authorization::{
            Audience, AuthorizationSelection, ResourceRef, ServiceId,
            bootstrap::{self, VerifiedNodeClaim, recovery},
        },
        signature_verification::SignatureVerification,
    };
    struct Fixture {
        network: TestNetwork,
        authority: Store,
        fresh: Store,
        node: LocalIdentity,
        capability: BootstrapCapability,
        claim: VerifiedNodeClaim,
        context: Option<Value>,
    }

    impl Fixture {
        fn new() -> Result<Self> {
            let network = TestNetwork::new()?;
            let authority = network.at(&network.directory.join("authority.sqlite3"))?;
            let mut fresh = Store::initialize(&network.directory.join("new-node.sqlite3"))?;
            let capability = BootstrapCapability::generate();
            fresh.install_bootstrap_capability(&capability)?;
            let node = LocalIdentity::generate();
            let signer =
                Identity::from_secret_hex(&node.secret_key_hex).map_err(anyhow::Error::msg)?;
            let claim_json = bootstrap::create_node_claim(
                &signer,
                &capability.capability,
                now() as f64,
                (now() + 100_000) as f64,
            )
            .map_err(anyhow::Error::msg)?;
            fresh.connection.execute(
                "INSERT INTO identity_meta(key,value) VALUES('node_claim',?)",
                [&claim_json],
            )?;
            let claim = bootstrap::verify_node_claim(
                &claim_json,
                &node.public_key_hex()?,
                &capability.capability,
                now() as f64,
            )
            .map_err(anyhow::Error::msg)?;
            Ok(Self {
                network,
                authority,
                fresh,
                node,
                capability,
                claim,
                context: None,
            })
        }

        fn material(network: &TestNetwork) -> Result<String> {
            recovery::build_account_recovery_material(
                &network.created,
                &network.root,
                &network.recovery,
                "https://authority.example",
                now() as f64,
            )
            .map_err(anyhow::Error::msg)
        }

        fn prepare(
            &self,
            network: &TestNetwork,
        ) -> Result<node_enrollment::PreparedAccountNodeEnrollment> {
            let mut source = network.at(&network.directory.join(format!(
                "node-enrollment-reader-{}.sqlite3",
                uuid::Uuid::new_v4()
            )))?;
            let node = PublicKey::parse(&network.node.public_key_hex()?)?;
            let authorization =
                crate::identity::client_policy_read::verified_client_policy_for_test(
                    &mut source,
                    &network.node,
                    &network.client,
                    &network.account,
                    AuthorizationSelection {
                        resource: ResourceRef {
                            space: network.space.clone(),
                            resource: network.resource.clone(),
                        },
                        audience: Audience {
                            service: ServiceId::from_key(&node),
                            device: DeviceId::from_key(&node),
                        },
                        grant_leaf: network.bundle.grant_leaf.clone(),
                    },
                    &network.pins,
                    now(),
                )?;
            node_enrollment::prepare_account_node_enrollment(
                &Self::material(network)?,
                &authorization,
                &network.client,
                &self.claim,
                (now() + 1_000_000) as f64,
                now() as f64,
            )
            .map_err(anyhow::Error::msg)
        }

        fn possession(
            &mut self,
            prepared: &node_enrollment::PreparedAccountNodeEnrollment,
        ) -> Result<Value> {
            let proof = relay(
                &mut self.authority,
                &self.network.node,
                &mut self.fresh,
                &self.node,
                &self.network.client.public_key_hex(),
                prepared,
                now(),
            )?;
            self.context = Some(proof["context"].clone());
            Ok(proof)
        }
        fn installation(
            &self,
            verified: &node_enrollment::VerifiedAccountNodeEnrollment,
        ) -> Result<Value> {
            Ok(
                json!({"context":self.context.as_ref().context("context missing")?,
                "installation":serde_json::from_str::<Value>(&verified.installation_json().map_err(anyhow::Error::msg)?)?}),
            )
        }

        fn next_target(&mut self, index: usize) -> Result<()> {
            let mut fresh = Store::initialize(
                &self
                    .network
                    .directory
                    .join(format!("long-node-{index}.sqlite3")),
            )?;
            let capability = BootstrapCapability::generate();
            fresh.install_bootstrap_capability(&capability)?;
            let node = LocalIdentity::generate();
            let signer =
                Identity::from_secret_hex(&node.secret_key_hex).map_err(anyhow::Error::msg)?;
            let claim = bootstrap::create_node_claim(
                &signer,
                &capability.capability,
                now() as f64,
                (now() + 100_000) as f64,
            )
            .map_err(anyhow::Error::msg)?;
            fresh.connection.execute(
                "INSERT INTO identity_meta(key,value) VALUES('node_claim',?)",
                [&claim],
            )?;
            self.claim = bootstrap::verify_node_claim(
                &claim,
                &node.public_key_hex()?,
                &capability.capability,
                now() as f64,
            )
            .map_err(anyhow::Error::msg)?;
            self.fresh = fresh;
            self.node = node;
            self.capability = capability;
            self.context = None;
            Ok(())
        }
        fn commit_request(
            &mut self,
        ) -> Result<(node_enrollment::PreparedAccountNodeEnrollmentCommit, Value)> {
            let prepared = self.prepare(&self.network)?;
            let proof = self.possession(&prepared)?;
            let commit = node_enrollment::prepare_account_node_enrollment_commit(
                &prepared,
                &Self::material(&self.network)?,
                proof["proof"].as_str().context("proof missing")?,
                now() as f64,
            )
            .map_err(anyhow::Error::msg)?;
            let request =
                serde_json::from_str(&commit.request_json().map_err(anyhow::Error::msg)?)?;
            Ok((commit, request))
        }
    }

    fn count(store: &Store, table: &str) -> Result<u64> {
        ensure!(["policy_journal", "current_policy_heads", "policy_nonces"].contains(&table));
        Ok(store
            .connection
            .query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))?)
    }
    fn relay(
        source: &mut Store,
        source_node: &LocalIdentity,
        target: &mut Store,
        target_node: &LocalIdentity,
        principal: &str,
        prepared: &node_enrollment::PreparedAccountNodeEnrollment,
        at: u64,
    ) -> Result<Value> {
        let params: Value =
            serde_json::from_str(&prepared.request_json().map_err(anyhow::Error::msg)?)?;
        let snapshot = source.begin_enrollment_policy_read(source_node, principal, &params, at)?;
        let mut begin = params;
        begin["snapshot"] = snapshot["snapshot"].clone();
        let mut response = target.node_enrollment_command(
            target_node,
            principal,
            "account_node_import_begin",
            &begin,
            at,
        )?;
        if response["proof"].is_string() {
            return Ok(response);
        }
        let context = response["context"].clone();
        loop {
            let page = source.enrollment_policy_page(
                source_node,
                principal,
                &response["page_request"],
                at,
            )?;
            response = target.node_enrollment_command(
                target_node,
                principal,
                "account_node_import_page",
                &json!({"context":context,"page":page["page"]}),
                at,
            )?;
            if page["page"]["payload"]["complete"] == true {
                break;
            }
        }
        let witness = source.enrollment_policy_witness(
            source_node,
            principal,
            &response["page_request"],
            at,
        )?;
        target.node_enrollment_command(
            target_node,
            principal,
            "account_node_import_finish",
            &json!({"context":context,"witness":witness["witness"]}),
            at,
        )
    }
    fn committed(
        f: &mut Fixture,
    ) -> Result<(
        node_enrollment::PreparedAccountNodeEnrollmentCommit,
        Value,
        Value,
    )> {
        let (prepared, request) = f.commit_request()?;
        let response = f.authority.node_enrollment_command(
            &f.network.node,
            &f.network.client.public_key_hex(),
            "account_node_commit",
            &request,
            now(),
        )?;
        Ok((prepared, request, response))
    }
    fn finish(
        prepared: &node_enrollment::PreparedAccountNodeEnrollmentCommit,
        response: &Value,
        at: u64,
    ) -> Result<node_enrollment::VerifiedAccountNodeEnrollment> {
        node_enrollment::finish_account_node_enrollment(
            prepared,
            &serde_json::to_string(&response["journal_delta"])?,
            at as f64,
        )
        .map_err(anyhow::Error::msg)
    }
    #[test]
    fn bounded_node_enrollment_installs_and_retries_after_reopen() -> Result<()> {
        let mut f = Fixture::new()?;
        let before = count(&f.authority, "policy_journal")?;
        let grants = f
            .authority
            .authorization_state()
            .space(&f.network.space)
            .context("space missing")?
            .grants
            .clone();
        let (prepared, request, response) = committed(&mut f)?;
        assert_eq!(count(&f.authority, "policy_journal")?, before + 2);
        let principal = f.network.client.public_key_hex();
        assert_eq!(
            f.authority.node_enrollment_command(
                &f.network.node,
                &principal,
                "account_node_commit",
                &request,
                now() + 1
            )?,
            response
        );
        let verified = finish(&prepared, &response, now())?;
        let authorization = verified
            .authorization()
            .context("current native authorization missing")?;
        assert_eq!(authorization.scope().account_id, f.network.account);
        assert!(authorization.client_policy_context_json().is_err());
        let delayed = finish(&prepared, &response, now() + 20_000_000)?;
        assert!(delayed.authorization().is_none());
        assert_eq!(delayed.pins(), verified.pins());
        let installation = f.installation(&verified)?;
        assert!(installation.get("bundle").is_none());
        assert!(installation["installation"].get("bundle").is_none());
        let installed = f.fresh.node_enrollment_command(
            &f.node,
            &principal,
            "account_node_install",
            &installation,
            now(),
        )?;
        assert_eq!(installed["installed"], true);
        assert_eq!(
            f.fresh
                .authorization_state()
                .space(&f.network.space)
                .context("space missing")?
                .grants,
            grants
        );
        assert!(
            f.fresh
                .verify_bootstrap_capability(&f.capability.capability)
                .is_err()
        );
        let path = f.network.directory.join("new-node.sqlite3");
        drop(f.fresh);
        let mut reopened = Store::open(&path)?;
        assert_eq!(
            reopened.node_enrollment_command(
                &f.node,
                &principal,
                "account_node_install",
                &installation,
                now() + 1
            )?,
            installed
        );
        let mut wrong = installation;
        wrong["context"] = json!(ObjectHash::body(&"another context")?);
        assert!(
            reopened
                .node_enrollment_command(
                    &f.node,
                    &principal,
                    "account_node_install",
                    &wrong,
                    now() + 1
                )
                .is_err()
        );
        Ok(())
    }
    #[test]
    fn twelve_node_public_history_enrolls_a_fresh_node_without_a_bundle_carrier() -> Result<()> {
        let mut f = Fixture::new()?;
        for index in 0..12 {
            let (prepared, _, response) = committed(&mut f)?;
            let finished = finish(&prepared, &response, now())?;
            f.network.pins = finished.pins().to_vec();
            f.network.bundle.journal.extend(serde_json::from_value::<
                Vec<vibeface_secure::authorization::JournalEntry>,
            >(response["journal_delta"].clone())?);
            f.next_target(index)?;
        }
        assert!(
            serde_json::to_vec(&f.network.bundle)?.len()
                > vibeface_secure::authorization::MAX_PROOF_BYTES
        );
        let before = f.network.bundle.journal.len();
        let (prepared, _, response) = committed(&mut f)?;
        let finished = finish(&prepared, &response, now())?;
        let installation = f.installation(&finished)?;
        assert!(
            serde_json::to_vec(&installation)?.len()
                <= vibeface_secure::authorization::MAX_PROOF_BYTES
        );
        assert_eq!(
            f.fresh.node_enrollment_command(
                &f.node,
                &f.network.client.public_key_hex(),
                "account_node_install",
                &installation,
                now()
            )?["installed"],
            true
        );
        assert_eq!(count(&f.fresh, "policy_journal")? as usize, before + 2);
        Ok(())
    }

    #[test]
    fn project_host_does_not_replace_the_account_enrollment_authority() -> Result<()> {
        use vibeface_secure::authorization::{
            JournalEntry, Nonce, PolicyDomain, ResourceId, ResourceKind, ResourceRecord,
            ResourceScope,
        };
        let mut f = Fixture::new()?;
        let (prepared, _, response) = committed(&mut f)?;
        let finished = finish(&prepared, &response, now())?;
        let installation = f.installation(&finished)?;
        let principal = f.network.client.public_key_hex();
        f.fresh.node_enrollment_command(
            &f.node,
            &principal,
            "account_node_install",
            &installation,
            now(),
        )?;
        let host = PublicKey::parse(&f.node.public_key_hex()?)?;
        let head = f
            .authority
            .authorization_state()
            .head(&PolicyDomain::Space(f.network.space.clone()))
            .context("space head missing")?
            .clone();
        let resource = ResourceRef {
            space: f.network.space.clone(),
            resource: ResourceId::random()?,
        };
        let record = Signed::sign(
            ResourceRecord {
                version: vibeface_secure::authorization::AUTH_VERSION,
                space: resource.space.clone(),
                resource: resource.resource.clone(),
                kind: ResourceKind::Project,
                host: ServiceId::from_key(&host),
                issued_under: head.clone(),
                nonce: Nonce::random()?,
            },
            &f.network.root,
        )?;
        let mut grant = f
            .authority
            .authorization_state()
            .grant(&f.network.bundle.grant_leaf)
            .context("owner grant missing")?
            .payload
            .clone();
        grant.audience = Audience {
            service: ServiceId::from_key(&host),
            device: DeviceId::from_key(&host),
        };
        grant.scope = ResourceScope::Exact(resource.clone());
        grant.issued_under = head.clone();
        grant.nonce = Nonce::random()?;
        let grant = Signed::sign(grant, &f.network.root)?;
        let mut space = f
            .authority
            .authorization_state()
            .space(&f.network.space)
            .context("space missing")?
            .clone();
        space.previous = Some(head);
        space.sequence += 1;
        space.resources.push(record.hash()?);
        space.resources.sort();
        space.grants.push(grant.hash()?);
        space.grants.sort();
        let delta = [
            JournalEntry::Resource(record),
            JournalEntry::Grant(grant.clone()),
            JournalEntry::Space(Signed::sign(space, &f.network.root)?),
        ];
        f.authority.install_policy_journal(&delta, now())?;
        f.fresh.install_policy_journal(&delta, now())?;
        let authorization = crate::identity::client_policy_read::verified_client_policy_for_test(
            &mut f.fresh,
            &f.node,
            &f.network.client,
            &f.network.account,
            AuthorizationSelection {
                resource,
                audience: Audience {
                    service: ServiceId::from_key(&host),
                    device: DeviceId::from_key(&host),
                },
                grant_leaf: grant.hash()?,
            },
            finished.pins(),
            now(),
        )?;
        let authority = PublicKey::parse(&f.network.node.public_key_hex()?)?;
        assert_eq!(authorization.audience_public_key(), &host);
        assert_ne!(host, authority);
        f.next_target(100)?;
        let prepared = node_enrollment::prepare_account_node_enrollment(
            &Fixture::material(&f.network)?,
            &authorization,
            &f.network.client,
            &f.claim,
            (now() + 1_000_000) as f64,
            now() as f64,
        )
        .map_err(anyhow::Error::msg)?;
        let request: Value =
            serde_json::from_str(&prepared.request_json().map_err(anyhow::Error::msg)?)?;
        assert_eq!(
            request["credential"]["credential"]["descriptor"]["source_node_public_key"],
            authority.as_str()
        );
        assert_eq!(
            request["request"]["payload"]["node_public_key"],
            authority.as_str()
        );
        assert!(f.possession(&prepared)?["proof"].is_string());
        Ok(())
    }

    #[test]
    fn first_read_rejects_unconfirmed_handle_noise_target_and_purpose() -> Result<()> {
        let mut f = Fixture::new()?;
        let unconfirmed = vibeface_secure::authorization::verify_client_authorization(
            &serde_json::to_string(&f.network.bundle)?,
            &serde_json::to_string(&f.network.pins)?,
            f.network.account.as_str(),
            f.network.space.as_str(),
            &f.network.client.public_key_hex(),
            now() as f64,
        )
        .map_err(anyhow::Error::msg)?;
        assert!(
            node_enrollment::prepare_account_node_enrollment(
                &Fixture::material(&f.network)?,
                &unconfirmed,
                &f.network.client,
                &f.claim,
                (now() + 1_000_000) as f64,
                now() as f64
            )
            .is_err()
        );
        let prepared = f.prepare(&f.network)?;
        let params: Value =
            serde_json::from_str(&prepared.request_json().map_err(anyhow::Error::msg)?)?;
        assert!(params["credential"]["credential"].get("bundle").is_none());
        let principal = f.network.client.public_key_hex();
        let snapshot = f.authority.begin_enrollment_policy_read(
            &f.network.node,
            &principal,
            &params,
            now(),
        )?;
        let mut begin = params;
        begin["snapshot"] = snapshot["snapshot"].clone();
        assert!(
            f.fresh
                .node_enrollment_command(
                    &LocalIdentity::generate(),
                    &principal,
                    "account_node_import_begin",
                    &begin,
                    now()
                )
                .is_err()
        );
        assert!(
            f.fresh
                .node_enrollment_command(
                    &f.node,
                    &Identity::generate()
                        .map_err(anyhow::Error::msg)?
                        .public_key_hex(),
                    "account_node_import_begin",
                    &begin,
                    now()
                )
                .is_err()
        );
        let mut wrong = begin.clone();
        wrong["credential"]["purpose"] = json!("browser_device");
        assert!(
            f.fresh
                .node_enrollment_command(
                    &f.node,
                    &principal,
                    "account_node_import_begin",
                    &wrong,
                    now()
                )
                .is_err()
        );
        let started = f.fresh.node_enrollment_command(
            &f.node,
            &principal,
            "account_node_import_begin",
            &begin,
            now(),
        )?;
        let page = f.authority.enrollment_policy_page(
            &f.network.node,
            &principal,
            &started["page_request"],
            now(),
        )?;
        let mut wrong = page["page"].clone();
        wrong["payload"]["purpose"] = json!("browser_device");
        assert!(
            f.fresh
                .node_enrollment_command(
                    &f.node,
                    &principal,
                    "account_node_import_page",
                    &json!({"context":started["context"],"page":wrong}),
                    now()
                )
                .is_err()
        );
        assert_eq!(
            f.fresh
                .account_node_enrollment_import
                .as_ref()
                .context("import missing")?
                .accepted_page_count(),
            0
        );
        assert!(metadata(&f.fresh.connection, "node_possession")?.is_none());
        assert_eq!(count(&f.fresh, "policy_journal")?, 0);
        let input = json!({"context":started["context"],"page":page["page"]});
        let accepted = f.fresh.node_enrollment_command(
            &f.node,
            &principal,
            "account_node_import_page",
            &input,
            now(),
        )?;
        let cached = metadata(&f.fresh.connection, "account_node_import")?;
        let verification = SignatureVerification::new();
        let retried = verification.run(|| {
            f.fresh.node_enrollment_command(
                &f.node,
                &principal,
                "account_node_import_page",
                &input,
                now(),
            )
        })?;
        assert_eq!(retried, accepted);
        assert!((1..=32).contains(&verification.counts().actual_verifications));
        assert_eq!(
            metadata(&f.fresh.connection, "account_node_import")?,
            cached
        );
        assert_eq!(
            f.fresh
                .account_node_enrollment_import
                .as_ref()
                .context("import missing")?
                .accepted_page_count(),
            1
        );
        f.fresh
            .verify_bootstrap_capability(&f.capability.capability)?;
        Ok(())
    }
    #[test]
    fn complete_carrier_rejects_two_individually_bounded_messages() -> Result<()> {
        let half = vibeface_secure::authorization::MAX_PROOF_BYTES / 6 + 1;
        let request = json!({"value":"界".repeat(half)});
        let response = json!({"value":"界".repeat(half)});
        assert!(
            serde_json::to_vec(&request)?.len() < vibeface_secure::authorization::MAX_PROOF_BYTES
        );
        assert!(
            serde_json::to_vec(&response)?.len() < vibeface_secure::authorization::MAX_PROOF_BYTES
        );
        assert!(bounded_response(&request, response).is_err());
        assert_eq!(
            bounded_response(&json!({"id":1}), json!({"installed":true}))?,
            json!({"installed":true})
        );
        Ok(())
    }

    #[test]
    fn failed_commit_and_install_roll_back_policy_nonce_receipt_and_claim() -> Result<()> {
        let mut f = Fixture::new()?;
        let (prepared, request) = f.commit_request()?;
        let principal = f.network.client.public_key_hex();
        let before = count(&f.authority, "policy_journal")?;
        f.authority.connection.execute_batch("CREATE TEMP TRIGGER fail_node_commit BEFORE INSERT ON identity_meta WHEN NEW.key GLOB 'node-enrollment-commit:*' BEGIN SELECT RAISE(ABORT,'expected failure'); END;")?;
        assert!(
            f.authority
                .node_enrollment_command(
                    &f.network.node,
                    &principal,
                    "account_node_commit",
                    &request,
                    now()
                )
                .is_err()
        );
        assert_eq!(count(&f.authority, "policy_journal")?, before);
        f.authority
            .connection
            .execute_batch("DROP TRIGGER fail_node_commit")?;
        let response = f.authority.node_enrollment_command(
            &f.network.node,
            &principal,
            "account_node_commit",
            &request,
            now(),
        )?;
        let installation = f.installation(&finish(&prepared, &response, now())?)?;
        f.fresh.connection.execute_batch("CREATE TEMP TRIGGER fail_node_install BEFORE INSERT ON identity_meta WHEN NEW.key='account_node_installation' BEGIN SELECT RAISE(ABORT,'expected failure'); END;")?;
        assert!(
            f.fresh
                .node_enrollment_command(
                    &f.node,
                    &principal,
                    "account_node_install",
                    &installation,
                    now()
                )
                .is_err()
        );
        for table in ["policy_journal", "current_policy_heads", "policy_nonces"] {
            assert_eq!(count(&f.fresh, table)?, 0);
        }
        f.fresh
            .verify_bootstrap_capability(&f.capability.capability)?;
        assert!(metadata(&f.fresh.connection, "account_node_installation")?.is_none());
        f.fresh
            .connection
            .execute_batch("DROP TRIGGER fail_node_install")?;
        assert_eq!(
            f.fresh.node_enrollment_command(
                &f.node,
                &principal,
                "account_node_install",
                &installation,
                now()
            )?["installed"],
            true
        );
        Ok(())
    }
    #[test]
    fn committed_node_resumes_each_signed_page_after_expiry_and_reopen() -> Result<()> {
        let mut f = Fixture::new()?;
        let (prepared, _, response) = committed(&mut f)?;
        let installation = f.installation(&finish(&prepared, &response, now())?)?;
        let principal = f.network.client.public_key_hex();
        let path = f.network.directory.join("new-node.sqlite3");
        drop(f.fresh);
        let mut reopened = Store::open(&path)?;
        let late = now() + 200_000;
        assert!(
            reopened
                .node_enrollment_command(
                    &f.node,
                    &principal,
                    "account_node_import_resume",
                    &json!({"context":installation["context"]}),
                    late
                )
                .is_err()
        );
        let resume =
            json!({"context":installation["context"],"installation":installation["installation"]});
        let saved =
            metadata(&reopened.connection, "node_possession")?.context("possession missing")?;
        let mut changed: Value = serde_json::from_str(&saved)?;
        changed["approved_at_ms"] = json!(now() + 1);
        reopened.connection.execute(
            "UPDATE identity_meta SET value=? WHERE key='node_possession'",
            [serde_json::to_string(&changed)?],
        )?;
        assert!(
            reopened
                .node_enrollment_command(
                    &f.node,
                    &principal,
                    "account_node_import_resume",
                    &resume,
                    late
                )
                .is_err()
        );
        assert!(reopened.account_node_enrollment_import.is_none());
        reopened.connection.execute(
            "UPDATE identity_meta SET value=? WHERE key='node_possession'",
            [saved],
        )?;
        let saved = metadata(&reopened.connection, "account_node_import")?
            .context("signed pages missing")?;
        let mut changed: Value = serde_json::from_str(&saved)?;
        changed["pages"][0]["signature"] = json!("00".repeat(64));
        reopened.connection.execute(
            "UPDATE identity_meta SET value=? WHERE key='account_node_import'",
            [serde_json::to_string(&changed)?],
        )?;
        assert!(
            reopened
                .node_enrollment_command(
                    &f.node,
                    &principal,
                    "account_node_import_resume",
                    &resume,
                    late
                )
                .is_err()
        );
        assert!(reopened.account_node_enrollment_import.is_none());
        reopened.connection.execute(
            "UPDATE identity_meta SET value=? WHERE key='account_node_import'",
            [saved],
        )?;
        let mut previous = 0;
        loop {
            let verification = SignatureVerification::new();
            let value = verification.run(|| {
                reopened.node_enrollment_command(
                    &f.node,
                    &principal,
                    "account_node_import_resume",
                    &resume,
                    late,
                )
            })?;
            assert!(verification.counts().actual_verifications <= 32);
            if value["ready"] == true {
                break;
            }
            let accepted = value["replayed_pages"]
                .as_u64()
                .context("resume progress missing")?;
            assert_eq!(accepted, previous + 1);
            previous = accepted;
            assert_eq!(count(&reopened, "policy_journal")?, 0);
        }
        assert!(previous > 1);
        assert_eq!(
            reopened.node_enrollment_command(
                &f.node,
                &principal,
                "account_node_install",
                &installation,
                late
            )?["installed"],
            true
        );
        Ok(())
    }
    #[test]
    fn exact_receipt_retry_reloads_current_revocation() -> Result<()> {
        let mut f = Fixture::new()?;
        let (prepared, request, response) = committed(&mut f)?;
        let principal = f.network.client.public_key_hex();
        let installation = f.installation(&finish(&prepared, &response, now())?)?;
        f.fresh.node_enrollment_command(
            &f.node,
            &principal,
            "account_node_install",
            &installation,
            now(),
        )?;
        let mut independent = Store::open(f.network.directory.join("authority.sqlite3"))?;
        f.network.revoke_client(&mut independent)?;
        assert!(
            f.authority
                .node_enrollment_command(
                    &f.network.node,
                    &principal,
                    "account_node_commit",
                    &request,
                    now() + 2
                )
                .is_err()
        );
        let mut independent = Store::open(f.network.directory.join("new-node.sqlite3"))?;
        f.network.revoke_client(&mut independent)?;
        assert!(
            f.fresh
                .node_enrollment_command(
                    &f.node,
                    &principal,
                    "account_node_install",
                    &installation,
                    now() + 2
                )
                .is_err()
        );
        Ok(())
    }
}
