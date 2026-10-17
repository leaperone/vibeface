use super::{journal, policy::project_policy_in};
use crate::{crypto::LocalIdentity, store::Store};
use anyhow::{Context, Result, ensure};
use rusqlite::{OptionalExtension, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use vibeface_secure::authorization::{
    Actor, ClientAuthorizationBundle, JournalEntry, Nonce, ObjectHash, PolicyDomain, PolicyPin,
    PolicyStamp, PublicKey, Signed,
    bootstrap::{
        self, NodeClaim,
        recovery::{self, context::RecoveryContextRequest},
    },
};

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RecoveryContext {
    request: Signed<RecoveryContextRequest>,
    claim: Signed<NodeClaim>,
    pins: Vec<PolicyPin>,
    authorization: ClientAuthorizationBundle,
    possession: Option<Value>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RecoveryCommit {
    expected_account_head: PolicyStamp,
    expected_space_head: PolicyStamp,
    journal_delta: Vec<JournalEntry>,
    claim_nonce: Nonce,
    request_hash: ObjectHash,
}

fn context_key(nonce: &Nonce) -> String {
    format!("recovery-context:{nonce}")
}

impl Store {
    pub(crate) fn recovery_context(
        &mut self,
        identity: &LocalIdentity,
        principal: &str,
        params: &Value,
        now_ms: u64,
    ) -> Result<Value> {
        let request: Signed<RecoveryContextRequest> =
            serde_json::from_value(params["request"].clone())?;
        let key = PublicKey::parse(&identity.public_key_hex()?)?;
        let verified = self
            .policy
            .verified()
            .verify_recovery_context_request(&request, &key, now_ms)?;
        ensure!(
            verified.new_device_public_key().as_str() == principal,
            "recovery request belongs to another Noise device"
        );
        let account_domain = PolicyDomain::Account(verified.account().clone());
        let space_domain = PolicyDomain::Space(verified.space().clone());
        let pins: Vec<_> = self
            .policy
            .pins()
            .iter()
            .filter(|pin| pin.domain == account_domain || pin.domain == space_domain)
            .cloned()
            .collect();
        ensure!(pins.len() == 2, "recovery policy roots are not installed");
        let mut statement = self
            .connection
            .prepare("SELECT entry FROM policy_journal ORDER BY ordinal")?;
        let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
        let mut entries = Vec::new();
        for row in rows {
            let entry: JournalEntry = serde_json::from_str(&row?)?;
            let belongs = match &entry {
                JournalEntry::AccountGenesis(value) | JournalEntry::Account(value) => {
                    value.payload.account == *verified.account()
                }
                JournalEntry::SpaceGenesis(value) | JournalEntry::Space(value) => {
                    value.payload.space == *verified.space()
                }
                JournalEntry::AccountCommit(value) => {
                    value.proposal.payload.account == *verified.account()
                }
                JournalEntry::SpaceCommit(value) => {
                    value.proposal.payload.space == *verified.space()
                }
                JournalEntry::Device(value) => {
                    value.payload.owner == account_domain || value.payload.owner == space_domain
                }
                JournalEntry::Authenticator(value) => value.payload.account == *verified.account(),
                JournalEntry::Resource(value) => value.payload.space == *verified.space(),
                JournalEntry::Membership(value) => value.payload.space == *verified.space(),
                JournalEntry::Grant(value) => value.payload.owner_space == *verified.space(),
            };
            if belongs {
                entries.push(entry);
            }
        }
        drop(statement);
        let leaf = entries
            .iter()
            .find_map(|entry| match entry {
                JournalEntry::Grant(grant)
                    if grant.payload.grantee
                        == vibeface_secure::authorization::Grantee::Actor(Actor::Account(
                            verified.account().clone(),
                        )) =>
                {
                    grant.hash().ok()
                }
                _ => None,
            })
            .context("recovery history has no account grant anchor")?;
        let authorization = ClientAuthorizationBundle {
            journal: entries,
            grant_leaf: leaf,
        };
        let replayed =
            vibeface_secure::authorization::replay_policies(&pins, &authorization.journal)
                .context("recovery history requires additional scoped proof")?;
        for domain in [&account_domain, &space_domain] {
            ensure!(
                replayed.head(domain) == self.policy.verified().head(domain),
                "recovery projection is incomplete"
            );
        }
        let row_key = context_key(verified.nonce());
        let previous: Option<String> = self
            .connection
            .query_row(
                "SELECT value FROM identity_meta WHERE key=?",
                [&row_key],
                |row| row.get(0),
            )
            .optional()?;
        if let Some(previous) = previous {
            let previous: RecoveryContext = serde_json::from_str(&previous)?;
            ensure!(
                previous.request == request,
                "recovery nonce was reused with another request"
            );
            return Ok(
                json!({"claim":previous.claim,"authorization":previous.authorization,"pins":previous.pins}),
            );
        }
        let signer = vibeface_secure::Identity::from_secret_hex(&identity.secret_key_hex)
            .map_err(anyhow::Error::msg)?;
        let claim = bootstrap::create_node_claim(
            &signer,
            verified.nonce().as_str(),
            now_ms as f64,
            verified.expires_at_ms() as f64,
        )
        .map_err(anyhow::Error::msg)?;
        let claim: Signed<NodeClaim> = serde_json::from_str(&claim)?;
        let context = RecoveryContext {
            request,
            claim: claim.clone(),
            pins: pins.clone(),
            authorization: authorization.clone(),
            possession: None,
        };
        self.connection.execute(
            "INSERT INTO identity_meta(key,value) VALUES(?,?)",
            params![row_key, serde_json::to_string(&context)?],
        )?;
        Ok(json!({"claim":claim,"authorization":authorization,"pins":pins}))
    }

    pub(crate) fn recovery_node_possession(
        &mut self,
        identity: &LocalIdentity,
        principal: &str,
        request: &Value,
        now_ms: u64,
    ) -> Result<Value> {
        let claim: Signed<NodeClaim> = serde_json::from_value(request["claim"].clone())?;
        let row_key = context_key(&claim.payload.claim_nonce);
        let encoded: String = self.connection.query_row(
            "SELECT value FROM identity_meta WHERE key=?",
            [&row_key],
            |row| row.get(0),
        )?;
        let mut context: RecoveryContext = serde_json::from_str(&encoded)?;
        ensure!(
            context.claim == claim
                && context.request.payload.new_device_public_key.as_str() == principal,
            "recovery claim or Noise device changed"
        );
        self.policy.verified().verify_recovery_context_request(
            &context.request,
            &PublicKey::parse(&identity.public_key_hex()?)?,
            now_ms,
        )?;
        let request_hash = ObjectHash::body(request)?;
        if let Some(possession) = &context.possession {
            ensure!(
                possession["request_hash"] == request_hash.as_str(),
                "recovery claim was already bound to another proposal"
            );
            return Ok(json!({"proof":possession["proof"]}));
        }
        for (name, domain) in [
            (
                "expected_account_head",
                PolicyDomain::Account(context.request.payload.account.clone()),
            ),
            (
                "expected_space_head",
                PolicyDomain::Space(context.request.payload.space.clone()),
            ),
        ] {
            ensure!(
                Some(serde_json::from_value::<PolicyStamp>(
                    request[name].clone()
                )?)
                .as_ref()
                    == self.policy.verified().head(&domain),
                "recovery proposal has stale policy heads"
            );
        }
        let signer = vibeface_secure::Identity::from_secret_hex(&identity.secret_key_hex)
            .map_err(anyhow::Error::msg)?;
        let proof = recovery::sign_recovery_node_possession(
            &serde_json::to_string(request)?,
            &signer,
            &serde_json::to_string(&context.pins)?,
            &serde_json::to_string(&context.authorization)?,
            claim.payload.claim_nonce.as_str(),
            now_ms as f64,
        )
        .map_err(anyhow::Error::msg)?;
        context.possession =
            Some(json!({"request":request,"request_hash":request_hash,"proof":proof}));
        ensure!(
            self.connection.execute(
                "UPDATE identity_meta SET value=? WHERE key=? AND value=?",
                params![serde_json::to_string(&context)?, row_key, encoded]
            )? == 1,
            "recovery proposal changed concurrently"
        );
        Ok(json!({"proof":proof}))
    }

    pub(crate) fn recover_account(
        &mut self,
        identity: &LocalIdentity,
        principal: &str,
        params: &Value,
        now_ms: u64,
    ) -> Result<Value> {
        let input: RecoveryCommit = serde_json::from_value(params.clone())?;
        let completion_hash = ObjectHash::body(params)?;
        let completion_key = format!("recovery-completed:{}", input.claim_nonce);
        let previous: Option<String> = self
            .connection
            .query_row(
                "SELECT value FROM identity_meta WHERE key=?",
                [&completion_key],
                |row| row.get(0),
            )
            .optional()?;
        if let Some(previous) = previous {
            let previous: Value = serde_json::from_str(&previous)?;
            ensure!(
                previous["completion_hash"] == completion_hash.as_str()
                    && previous["principal"] == principal,
                "recovery nonce was already consumed by another request"
            );
            return Ok(previous["response"].clone());
        }
        let row_key = context_key(&input.claim_nonce);
        let encoded: String = self.connection.query_row(
            "SELECT value FROM identity_meta WHERE key=?",
            [&row_key],
            |row| row.get(0),
        )?;
        let context: RecoveryContext = serde_json::from_str(&encoded)?;
        let local_key = PublicKey::parse(&identity.public_key_hex()?)?;
        self.policy.verified().verify_recovery_context_request(
            &context.request,
            &local_key,
            now_ms,
        )?;
        ensure!(
            context.request.payload.new_device_public_key.as_str() == principal,
            "recovery belongs to another Noise device"
        );
        let possession = context
            .possession
            .as_ref()
            .context("recovery Node possession is not approved")?;
        ensure!(
            possession["request_hash"] == input.request_hash.as_str(),
            "recovery commit differs from approved proposal"
        );
        let request = &possession["request"];
        ensure!(
            serde_json::to_value(&input.expected_account_head)? == request["expected_account_head"]
                && serde_json::to_value(&input.expected_space_head)?
                    == request["expected_space_head"],
            "recovery heads differ from approved proposal"
        );
        ensure!(
            self.policy
                .verified()
                .head(&input.expected_account_head.domain)
                == Some(&input.expected_account_head)
                && self
                    .policy
                    .verified()
                    .head(&input.expected_space_head.domain)
                    == Some(&input.expected_space_head),
            "recovery policy changed before commit"
        );
        let mut account_proposal = false;
        let mut node_certificate = false;
        for entry in &input.journal_delta {
            match entry {
                JournalEntry::Account(account)
                    if serde_json::to_value(account)? == request["account_recovery"] =>
                {
                    account_proposal = true
                }
                JournalEntry::Device(certificate)
                    if certificate.payload.public_key == local_key =>
                {
                    let mut unsigned = certificate.payload.clone();
                    unsigned.possession = None;
                    ensure!(
                        serde_json::to_value(unsigned)? == request["node_certificate"]
                            && certificate
                                .payload
                                .possession
                                .as_ref()
                                .map(|proof| proof.as_str())
                                == possession["proof"].as_str(),
                        "recovery Node certificate differs from approved possession"
                    );
                    node_certificate = true;
                }
                _ => {}
            }
        }
        ensure!(
            account_proposal && node_certificate,
            "recovery commit lacks approved account or Node certificate"
        );
        let update = self
            .policy
            .prepare_network_update(&input.journal_delta, now_ms)?;
        let state = update.verified_after();
        let mut pins = context.pins.clone();
        for pin in &mut pins {
            pin.minimum_head = state.head(&pin.domain).cloned();
        }
        let mut full_journal = context.authorization.journal.clone();
        full_journal.extend(input.journal_delta.clone());
        let grant_leaf = input
            .journal_delta
            .iter()
            .find_map(|entry| match entry {
                JournalEntry::Grant(grant)
                    if grant.payload.grantee
                        == vibeface_secure::authorization::Grantee::Actor(Actor::Account(
                            context.request.payload.account.clone(),
                        )) =>
                {
                    grant.hash().ok()
                }
                _ => None,
            })
            .context("recovery lacks a new account grant")?;
        let bundle = ClientAuthorizationBundle {
            journal: full_journal,
            grant_leaf,
        };
        let verified = vibeface_secure::authorization::verify_client_authorization(
            &serde_json::to_string(&bundle)?,
            &serde_json::to_string(&pins)?,
            context.request.payload.account.as_str(),
            context.request.payload.space.as_str(),
            principal,
            now_ms as f64,
        )
        .map_err(anyhow::Error::msg)?;
        let response = json!({"scope":verified.scope(),"authorization":bundle,"pins":pins,"resources":verified.resources()});
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let actual: String = tx.query_row(
            "SELECT value FROM identity_meta WHERE key=?",
            [&row_key],
            |row| row.get(0),
        )?;
        ensure!(actual == encoded, "recovery request changed concurrently");
        let state = journal::commit_in(&tx, &update)?;
        project_policy_in(&tx, &state)?;
        tx.execute("INSERT INTO identity_meta(key,value) VALUES(?,?)", params![completion_key,
            serde_json::to_string(&json!({"completion_hash":completion_hash,"principal":principal,"response":response}))?])?;
        tx.commit()?;
        self.policy = state;
        Ok(response)
    }
}
