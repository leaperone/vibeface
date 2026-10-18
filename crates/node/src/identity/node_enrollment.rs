use super::{journal, policy::project_policy_in};
use crate::{crypto::LocalIdentity, store::Store};
use anyhow::{Context, Result, bail, ensure};
use rusqlite::{OptionalExtension, Transaction, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use vibeface_secure::authorization::{
    AccountDocument, ClientAuthorizationBundle, Committed, DeviceId, ObjectHash, PolicyPin,
    PublicKey, Signed,
    bootstrap::{
        NodeClaim,
        recovery::node_enrollment::{
            self, AccountNodeEnrollmentCommitRequest, AccountNodePossessionRequest,
        },
    },
};

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PossessionRecord {
    request_hash: ObjectHash,
    principal: String,
    request: AccountNodePossessionRequest,
    proof: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct InstallationRequest {
    request: AccountNodeEnrollmentCommitRequest,
    pins: Vec<PolicyPin>,
    bundle: ClientAuthorizationBundle,
    committed: Committed<AccountDocument>,
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

impl Store {
    pub(crate) fn node_enrollment_command(
        &mut self,
        identity: &LocalIdentity,
        principal: &str,
        method: &str,
        params: &Value,
        now: u64,
    ) -> Result<Value> {
        ensure!(
            serde_json::to_vec(params)?.len() <= 2_097_152,
            "node enrollment input is too large"
        );
        match method {
            "account_node_possession" => {
                self.account_node_possession(identity, principal, params, now)
            }
            "account_node_commit" => self.account_node_commit(identity, principal, params, now),
            "account_node_install" => self.account_node_install(identity, principal, params, now),
            _ => bail!("unknown node enrollment method"),
        }
    }

    fn account_node_possession(
        &mut self,
        identity: &LocalIdentity,
        principal: &str,
        params: &Value,
        now: u64,
    ) -> Result<Value> {
        let request: AccountNodePossessionRequest = serde_json::from_value(params.clone())?;
        let intent = &request.intent;
        ensure!(
            intent.payload.requesting_device == DeviceId::from_key(&PublicKey::parse(principal)?),
            "node enrollment belongs to another Noise client"
        );
        self.verify_bootstrap_capability(intent.payload.claim.payload.claim_nonce.as_str())?;
        ensure!(
            intent.payload.claim.payload.node_public_key.as_str() == identity.public_key_hex()?,
            "Node claim belongs to another installation"
        );
        let request_hash = ObjectHash::body(intent)?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        check_claim_in(&tx, &intent.payload.claim)?;
        if let Some(previous) = metadata(&tx, "node_possession")? {
            let previous: PossessionRecord = serde_json::from_str(&previous)?;
            ensure!(
                previous.request_hash == request_hash
                    && previous.principal == principal
                    && previous.request.intent == request.intent
                    && previous.request.pins == request.pins
                    && previous.request.bundle == request.bundle,
                "claim was already bound to another account or enrollment request"
            );
            return Ok(json!({"proof":previous.proof}));
        }
        let signer = vibeface_secure::Identity::from_secret_hex(&identity.secret_key_hex)
            .map_err(anyhow::Error::msg)?;
        let proof = node_enrollment::sign_account_node_enrollment_possession(
            &serde_json::to_string(&request)?,
            &signer,
            intent.payload.claim.payload.claim_nonce.as_str(),
            now as f64,
        )
        .map_err(anyhow::Error::msg)?;
        let record = PossessionRecord {
            request_hash,
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
        let current = journal::PolicyState::load(&tx)?;
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
            let response = receipt["response"].clone();
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
        let response = json!({"journal_delta":delta});
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
        let installation: InstallationRequest = serde_json::from_value(params.clone())?;
        require_principal(&installation.request, principal)?;
        ensure!(
            installation.committed.proposal == installation.request.proposal
                && installation.request.certificate.payload.public_key.as_str()
                    == identity.public_key_hex()?,
            "installation changed the approved account proposal or local Node"
        );
        let input_hash = ObjectHash::body(params)?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let current = journal::PolicyState::load(&tx)?;
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
            let response = receipt["response"].clone();
            tx.commit()?;
            self.policy = current;
            return Ok(response);
        }
        let claim = &installation.request.intent.payload.claim;
        check_claim_in(&tx, claim)?;
        let saved: PossessionRecord = serde_json::from_str(
            &metadata(&tx, "node_possession")?.context("Node possession is missing")?,
        )?;
        ensure!(
            saved.request_hash == ObjectHash::body(&installation.request.intent)?
                && saved.principal == principal
                && saved.request.intent == installation.request.intent
                && saved.request.pins == installation.pins
                && saved.request.bundle == installation.bundle
                && installation
                    .request
                    .certificate
                    .payload
                    .possession
                    .as_ref()
                    .map(|proof| proof.as_str())
                    == Some(saved.proof.as_str()),
            "installation differs from this Node's approved possession request"
        );
        let signer = vibeface_secure::Identity::from_secret_hex(&identity.secret_key_hex)
            .map_err(anyhow::Error::msg)?;
        let verified = node_enrollment::verify_account_node_installation(
            &serde_json::to_string(params)?,
            &signer,
            claim.payload.claim_nonce.as_str(),
            now as f64,
        )
        .map_err(anyhow::Error::msg)?;
        ensure!(
            verified.request_hash()? == saved.request_hash
                && verified.node_public_key().as_str() == identity.public_key_hex()?,
            "verified installation differs from this local Node"
        );
        let update = current.prepare_bootstrap(verified.pins(), verified.journal(), now)?;
        update
            .verified_after()
            .verify_transport_device(&DeviceId::from_key(verified.node_public_key()), now)?;
        update.verified_after().verify_account_client_device(
            verified.account(),
            &installation.request.intent.payload.requesting_device,
            now,
        )?;
        let response = json!({"installed":true,"account_id":verified.account(),
            "node_public_key":verified.node_public_key(),"pins":verified.pins()});
        let receipt = json!({"request_hash":verified.request_hash()?,"input_hash":input_hash,"principal":principal,"response":response});
        let state = journal::commit_in(&tx, &update)?;
        project_policy_in(&tx, &state)?;
        ensure!(tx.execute("UPDATE identity_meta SET value='consumed:'||value WHERE key='bootstrap_capability_hash' AND value=?",
            [crate::crypto::digest(claim.payload.claim_nonce.as_str().as_bytes())])? == 1,
            "bootstrap capability was already consumed");
        tx.execute(
            "INSERT INTO identity_meta(key,value) VALUES('account_node_installation',?)",
            [serde_json::to_string(&receipt)?],
        )?;
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
        test_support::{TestNetwork, now},
    };
    use vibeface_secure::{
        Identity,
        authorization::{
            Audience, AuthorizationSelection, ResourceRef, ServiceId,
            bootstrap::{self, VerifiedNodeClaim, recovery},
        },
    };

    struct Fixture {
        network: TestNetwork,
        authority: Store,
        fresh: Store,
        node: LocalIdentity,
        capability: BootstrapCapability,
        claim: VerifiedNodeClaim,
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
            self.fresh.node_enrollment_command(
                &self.node,
                &self.network.client.public_key_hex(),
                "account_node_possession",
                &serde_json::from_str(&prepared.request_json().map_err(anyhow::Error::msg)?)?,
                now(),
            )
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

    #[test]
    fn additional_node_joins_the_same_account_and_retries_after_reopen() -> Result<()> {
        let mut f = Fixture::new()?;
        let (prepared, request) = f.commit_request()?;
        let principal = f.network.client.public_key_hex();
        let before = count(&f.authority, "policy_journal")?;
        let grants_before = f
            .authority
            .authorization_state()
            .space(&f.network.space)
            .context("space missing")?
            .grants
            .clone();
        assert!(
            f.authority
                .node_enrollment_command(
                    &f.network.node,
                    &Identity::generate()
                        .map_err(anyhow::Error::msg)?
                        .public_key_hex(),
                    "account_node_commit",
                    &request,
                    now()
                )
                .is_err()
        );
        assert_eq!(count(&f.authority, "policy_journal")?, before);
        let response = f.authority.node_enrollment_command(
            &f.network.node,
            &principal,
            "account_node_commit",
            &request,
            now(),
        )?;
        assert_eq!(count(&f.authority, "policy_journal")?, before + 2);
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
        let verified = node_enrollment::finish_account_node_enrollment(
            &prepared,
            &serde_json::to_string(&response["journal_delta"])?,
            now() as f64,
        )
        .map_err(anyhow::Error::msg)?;
        let successor = verified
            .authorization()
            .context("current client authorization missing")?;
        assert_eq!(successor.scope().account_id, f.network.account);
        assert_eq!(
            successor.heads().first().map(|head| &head.domain),
            Some(&vibeface_secure::authorization::PolicyDomain::Account(
                f.network.account.clone()
            ))
        );
        assert!(successor.client_policy_context_json().is_err());
        let delayed = node_enrollment::finish_account_node_enrollment(
            &prepared,
            &serde_json::to_string(&response["journal_delta"])?,
            (now() + 20_000_000) as f64,
        )
        .map_err(anyhow::Error::msg)?;
        assert!(delayed.authorization().is_none());
        assert_eq!(delayed.pins(), verified.pins());
        let installation: Value =
            serde_json::from_str(&verified.installation_json().map_err(anyhow::Error::msg)?)?;
        assert!(
            f.fresh
                .node_enrollment_command(
                    &LocalIdentity::generate(),
                    &principal,
                    "account_node_install",
                    &installation,
                    now()
                )
                .is_err()
        );
        assert_eq!(count(&f.fresh, "policy_journal")?, 0);
        f.fresh
            .verify_bootstrap_capability(&f.capability.capability)?;
        let installed = f.fresh.node_enrollment_command(
            &f.node,
            &principal,
            "account_node_install",
            &installation,
            now(),
        )?;
        assert_eq!(installed["account_id"], f.network.account.as_str());
        assert_eq!(installed["node_public_key"], f.node.public_key_hex()?);
        assert!(
            f.fresh
                .verify_bootstrap_capability(&f.capability.capability)
                .is_err()
        );
        let device = DeviceId::from_key(&PublicKey::parse(&f.node.public_key_hex()?)?);
        f.fresh
            .authorization_state()
            .verify_transport_device(&device, now())?;
        assert_eq!(
            f.fresh
                .authorization_state()
                .account(&f.network.account)
                .context("account missing")?
                .devices
                .len(),
            f.authority
                .authorization_state()
                .account(&f.network.account)
                .context("account missing")?
                .devices
                .len()
        );
        assert_eq!(
            f.fresh
                .authorization_state()
                .space(&f.network.space)
                .context("space missing")?
                .grants,
            grants_before
        );
        assert_eq!(
            f.authority
                .authorization_state()
                .space(&f.network.space)
                .context("space missing")?
                .grants,
            grants_before
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
        Ok(())
    }

    #[test]
    fn node_enrollment_rejects_an_unconfirmed_client_handle() -> Result<()> {
        let f = Fixture::new()?;
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
                now() as f64,
            )
            .is_err()
        );
        assert!(metadata(&f.fresh.connection, "node_possession")?.is_none());
        Ok(())
    }

    #[test]
    fn capability_cannot_be_rebound_to_another_account_or_client() -> Result<()> {
        let mut f = Fixture::new()?;
        let first = f.prepare(&f.network)?;
        let first_response = f.possession(&first)?;
        assert_eq!(f.possession(&first)?, first_response);
        assert_eq!(
            f.fresh.node_enrollment_command(
                &f.node,
                &f.network.client.public_key_hex(),
                "account_node_possession",
                &serde_json::from_str(&first.request_json().map_err(anyhow::Error::msg)?)?,
                now() + 200_000
            )?,
            first_response
        );
        let other = TestNetwork::new()?;
        let second = f.prepare(&other)?;
        let params: Value =
            serde_json::from_str(&second.request_json().map_err(anyhow::Error::msg)?)?;
        assert!(
            f.fresh
                .node_enrollment_command(
                    &f.node,
                    &other.client.public_key_hex(),
                    "account_node_possession",
                    &params,
                    now()
                )
                .is_err()
        );
        let params: Value =
            serde_json::from_str(&first.request_json().map_err(anyhow::Error::msg)?)?;
        let mut changed_context = params.clone();
        changed_context["bundle"]["grant_leaf"] = json!(ObjectHash::body(&"unapproved anchor")?);
        assert!(
            f.fresh
                .node_enrollment_command(
                    &f.node,
                    &f.network.client.public_key_hex(),
                    "account_node_possession",
                    &changed_context,
                    now()
                )
                .is_err()
        );
        assert!(
            f.fresh
                .node_enrollment_command(
                    &f.node,
                    &other.client.public_key_hex(),
                    "account_node_possession",
                    &params,
                    now()
                )
                .is_err()
        );
        assert_eq!(count(&f.fresh, "policy_journal")?, 0);
        f.fresh
            .verify_bootstrap_capability(&f.capability.capability)?;
        Ok(())
    }

    #[test]
    fn wrong_node_possession_and_stale_head_cannot_change_policy() -> Result<()> {
        let mut f = Fixture::new()?;
        let prepared = f.prepare(&f.network)?;
        let params: Value =
            serde_json::from_str(&prepared.request_json().map_err(anyhow::Error::msg)?)?;
        assert!(
            f.fresh
                .node_enrollment_command(
                    &LocalIdentity::generate(),
                    &f.network.client.public_key_hex(),
                    "account_node_possession",
                    &params,
                    now()
                )
                .is_err()
        );
        assert!(metadata(&f.fresh.connection, "node_possession")?.is_none());
        let (_, request) = f.commit_request()?;
        let mut other = Fixture::new()?;
        let other_prepared = other.prepare(&f.network)?;
        let other_params: Value =
            serde_json::from_str(&other_prepared.request_json().map_err(anyhow::Error::msg)?)?;
        let proof = other.fresh.node_enrollment_command(
            &other.node,
            &f.network.client.public_key_hex(),
            "account_node_possession",
            &other_params,
            now(),
        )?;
        let other_commit = node_enrollment::prepare_account_node_enrollment_commit(
            &other_prepared,
            &Fixture::material(&f.network)?,
            proof["proof"].as_str().context("proof missing")?,
            now() as f64,
        )
        .map_err(anyhow::Error::msg)?;
        let other_request =
            serde_json::from_str(&other_commit.request_json().map_err(anyhow::Error::msg)?)?;
        f.authority.node_enrollment_command(
            &f.network.node,
            &f.network.client.public_key_hex(),
            "account_node_commit",
            &other_request,
            now(),
        )?;
        let before = count(&f.authority, "policy_journal")?;
        assert!(
            f.authority
                .node_enrollment_command(
                    &f.network.node,
                    &f.network.client.public_key_hex(),
                    "account_node_commit",
                    &request,
                    now()
                )
                .is_err()
        );
        assert_eq!(count(&f.authority, "policy_journal")?, before);
        Ok(())
    }

    #[test]
    fn receipt_retries_reload_revocation_from_a_second_connection() -> Result<()> {
        let mut f = Fixture::new()?;
        let (prepared, request) = f.commit_request()?;
        let principal = f.network.client.public_key_hex();
        let response = f.authority.node_enrollment_command(
            &f.network.node,
            &principal,
            "account_node_commit",
            &request,
            now(),
        )?;
        let verified = node_enrollment::finish_account_node_enrollment(
            &prepared,
            &serde_json::to_string(&response["journal_delta"])?,
            now() as f64,
        )
        .map_err(anyhow::Error::msg)?;
        let installation =
            serde_json::from_str(&verified.installation_json().map_err(anyhow::Error::msg)?)?;
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
        let device = DeviceId::from_key(&PublicKey::of(&f.network.client));
        let mut independent_authority = Store::open(f.network.directory.join("authority.sqlite3"))?;
        f.network.revoke_client(&mut independent_authority)?;
        f.authority.policy.verified().verify_account_client_device(
            &f.network.account,
            &device,
            now() + 2,
        )?;
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
        let mut independent_target = Store::open(f.network.directory.join("new-node.sqlite3"))?;
        f.network.revoke_client(&mut independent_target)?;
        f.fresh.policy.verified().verify_account_client_device(
            &f.network.account,
            &device,
            now() + 2,
        )?;
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

    #[test]
    fn failed_commit_or_install_rolls_back_policy_receipt_and_capability() -> Result<()> {
        let mut f = Fixture::new()?;
        let (prepared, request) = f.commit_request()?;
        let principal = f.network.client.public_key_hex();
        let before = count(&f.authority, "policy_journal")?;
        f.authority.connection.execute_batch("CREATE TEMP TRIGGER fail_node_commit BEFORE INSERT ON identity_meta WHEN NEW.key GLOB 'node-enrollment-commit:*' BEGIN SELECT RAISE(ABORT,'expected commit failure'); END;")?;
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
        let verified = node_enrollment::finish_account_node_enrollment(
            &prepared,
            &serde_json::to_string(&response["journal_delta"])?,
            now() as f64,
        )
        .map_err(anyhow::Error::msg)?;
        let installation =
            serde_json::from_str(&verified.installation_json().map_err(anyhow::Error::msg)?)?;
        f.fresh.connection.execute_batch("CREATE TEMP TRIGGER fail_node_install BEFORE INSERT ON identity_meta WHEN NEW.key='account_node_installation' BEGIN SELECT RAISE(ABORT,'expected install failure'); END;")?;
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
        assert!(f.fresh.policy.pins().is_empty());
        f.fresh
            .verify_bootstrap_capability(&f.capability.capability)?;
        assert!(metadata(&f.fresh.connection, "account_node_installation")?.is_none());
        f.fresh
            .connection
            .execute_batch("DROP TRIGGER fail_node_install")?;
        assert!(
            f.fresh.node_enrollment_command(
                &f.node,
                &principal,
                "account_node_install",
                &installation,
                now()
            )?["installed"]
                .as_bool()
                .unwrap_or(false)
        );
        Ok(())
    }
}
