use super::{journal, policy::project_policy_in};
use crate::{crypto::LocalIdentity, store::Store};
use anyhow::{Context, Result, bail, ensure};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::Value;
use std::{collections::BTreeSet, path::PathBuf};
use vibeface_secure::signature_verification::ScopedSignatureVerification;
use vibeface_secure::{
    Identity,
    authorization::{
        execution_service::{self, ExecutionServiceRegistrationRequest},
        governance::{self, SpaceGovernanceRequest, TeamCreationAcceptance, TeamCreationRequest},
        *,
    },
};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TeamCreateParams {
    request: Signed<TeamCreationRequest>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SpaceCommitParams {
    request: SpaceGovernanceRequest,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ExecutionServiceSelectParams {
    account_id: AccountId,
    device_public_key: PublicKey,
    source_project: ResourceRef,
    target_node_public_key: PublicKey,
    execution_resource: Option<ResourceRef>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ExecutionServiceMapping {
    source_project: ResourceRef,
    management_parent: ObjectHash,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct TeamCreateResponse {
    acceptance: Signed<TeamCreationAcceptance>,
    pin: PolicyPin,
    journal_delta: Vec<JournalEntry>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct SpaceCommitResponse {
    committed: Committed<SpaceDocument>,
    journal_delta: Vec<JournalEntry>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct GovernanceReceipt<T> {
    request_hash: ObjectHash,
    principal: PublicKey,
    response: T,
}

fn receipt<T: DeserializeOwned>(
    connection: &Connection,
    key: &str,
    request_hash: &ObjectHash,
    principal: &PublicKey,
) -> Result<Option<T>> {
    let encoded: Option<String> = connection
        .query_row(
            "SELECT value FROM identity_meta WHERE key=?",
            [key],
            |row| row.get(0),
        )
        .optional()?;
    let Some(encoded) = encoded else {
        return Ok(None);
    };
    let stored: GovernanceReceipt<T> = serde_json::from_str(&encoded)?;
    ensure!(
        stored.request_hash == *request_hash && stored.principal == *principal,
        "governance nonce was already used by another request"
    );
    Ok(Some(stored.response))
}

fn save_receipt<T: Serialize>(
    connection: &Connection,
    key: &str,
    request_hash: ObjectHash,
    principal: PublicKey,
    response: &T,
) -> Result<()> {
    connection.execute(
        "INSERT INTO identity_meta(key,value) VALUES(?,?)",
        params![
            key,
            serde_json::to_string(&GovernanceReceipt {
                request_hash,
                principal,
                response
            })?
        ],
    )?;
    Ok(())
}

fn source_root(connection: &Connection, source: &ResourceRef) -> Result<PathBuf> {
    let encoded: String = connection
        .query_row(
            "SELECT value FROM local_values WHERE space_id=? AND key=?",
            params![
                source.space.as_str(),
                format!("resource-root:{}", source.resource)
            ],
            |row| row.get(0),
        )
        .context("source project has no local execution root")?;
    let root: PathBuf = serde_json::from_str(&encoded)?;
    ensure!(
        root.is_absolute() && root.is_dir() && root.canonicalize()? == root,
        "registered source project root changed"
    );
    Ok(root)
}

fn verify_team_source(
    connection: &Connection,
    policy: &journal::PolicyState,
    request: &Signed<TeamCreationRequest>,
    now_ms: u64,
) -> Result<governance::VerifiedTeamCreationRequest> {
    let mut statement = connection.prepare("SELECT entry FROM policy_journal ORDER BY ordinal")?;
    let journal = statement
        .query_map([], |row| row.get::<_, String>(0))?
        .map(|row| Ok(serde_json::from_str::<JournalEntry>(&row?)?))
        .collect::<Result<Vec<_>>>()?;
    let state = policy.verified();
    let source = &request.payload.source_project;
    let current = state
        .space(&source.space)
        .context("source space is unavailable")?;
    for leaf in &current.grants {
        if !state
            .grant(leaf)
            .is_some_and(|grant| grant.payload.actions.contains(&Action::ResourceManage))
        {
            continue;
        }
        let Ok(bundle) = state.client_authorization_projection(
            policy.pins(),
            &journal,
            &request.payload.owner_account,
            &source.space,
            &request.issuer,
            leaf,
            now_ms,
        ) else {
            continue;
        };
        let domains: BTreeSet<_> = bundle
            .journal
            .iter()
            .filter_map(|entry| match entry {
                JournalEntry::AccountGenesis(value) | JournalEntry::Account(value) => {
                    Some(PolicyDomain::Account(value.payload.account.clone()))
                }
                JournalEntry::SpaceGenesis(value) | JournalEntry::Space(value) => {
                    Some(PolicyDomain::Space(value.payload.space.clone()))
                }
                JournalEntry::AccountCommit(value) => Some(PolicyDomain::Account(
                    value.proposal.payload.account.clone(),
                )),
                JournalEntry::SpaceCommit(value) => {
                    Some(PolicyDomain::Space(value.proposal.payload.space.clone()))
                }
                _ => None,
            })
            .collect();
        let pins: Vec<_> = policy
            .pins()
            .iter()
            .filter(|pin| domains.contains(&pin.domain))
            .cloned()
            .collect();
        if let Ok(verified) =
            governance::verify_team_creation_request(state, &bundle, &pins, request, now_ms)
        {
            return Ok(verified);
        }
    }
    bail!("team creation requires current owner authorization for the selected personal project")
}

fn check_team_receipt(
    response: &TeamCreateResponse,
    request: &Signed<TeamCreationRequest>,
    node: &PublicKey,
) -> Result<()> {
    response.acceptance.verify_signature()?;
    let accepted = &response.acceptance.payload;
    ensure!(
        response.acceptance.issuer == *node
            && accepted.version == AUTH_VERSION
            && accepted.request_hash == ObjectHash::body(request)?
            && accepted.owner_account == request.payload.owner_account
            && accepted.space == request.payload.team_project.space
            && accepted.target_node_public_key == *node
            && accepted.nonce == request.payload.nonce
            && accepted.expires_at_ms == request.payload.expires_at_ms
            && request.payload.issued_at_ms <= accepted.accepted_at_ms
            && accepted.accepted_at_ms < accepted.expires_at_ms
            && accepted.account_head.domain
                == PolicyDomain::Account(request.payload.owner_account.clone())
            && request
                .payload
                .expected_heads
                .contains(&accepted.account_head)
            && response.pin == request.payload.team_pin
            && response.journal_delta == request.payload.journal_delta,
        "stored Team receipt differs from its signed request"
    );
    Ok(())
}

fn check_space_receipt(
    response: &SpaceCommitResponse,
    request: &SpaceGovernanceRequest,
    node: &PublicKey,
) -> Result<()> {
    response.committed.proposal.verify_signature()?;
    response.committed.commit.verify_signature()?;
    let commit = &response.committed.commit;
    let mut domains = BTreeSet::new();
    for receipt in &response.committed.receipts {
        receipt.verify_signature()?;
        ensure!(
            receipt.issuer == *node
                && receipt.payload.version == AUTH_VERSION
                && receipt.payload.challenge_hash == commit.challenge_hash()?
                && commit
                    .payload
                    .dependencies
                    .contains(&receipt.payload.current)
                && domains.insert(receipt.payload.current.domain.clone())
                && receipt.payload.issued_at_ms >= commit.payload.issued_at_ms
                && receipt.payload.issued_at_ms <= commit.payload.accepted_at_ms
                && receipt.payload.expires_at_ms <= commit.payload.expires_at_ms,
            "stored governance freshness receipt changed"
        );
    }
    let mut expected = request.new_objects.clone();
    expected.push(JournalEntry::SpaceCommit(response.committed.clone()));
    ensure!(
        response.committed.proposal == request.proposal
            && commit.issuer == *node
            && commit.payload.version == AUTH_VERSION
            && request.proposal.payload.previous.as_ref() == Some(&commit.payload.previous)
            && commit.payload.proposal_hash == request.proposal.hash()?
            && commit.payload.nonce == request.nonce
            && response.committed.receipts.len() == commit.payload.dependencies.len()
            && response.journal_delta == expected,
        "stored governance receipt differs from its signed request"
    );
    Ok(())
}

impl Store {
    pub(crate) fn execution_service_select(
        &mut self,
        identity: &LocalIdentity,
        principal: &str,
        params: &Value,
        now_ms: u64,
    ) -> Result<Value> {
        let _signature_scope = ScopedSignatureVerification::new();
        ensure!(
            serde_json::to_vec(params)?.len() <= MAX_PROOF_BYTES,
            "execution selection request too large"
        );
        let params: ExecutionServiceSelectParams = serde_json::from_value(params.clone())?;
        let principal = PublicKey::parse(principal)?;
        let node = PublicKey::parse(&identity.public_key_hex()?)?;
        ensure!(
            params.device_public_key == principal,
            "execution selector differs from Noise identity"
        );
        ensure!(
            params.target_node_public_key == node,
            "execution selection target differs from the physical node"
        );
        let personal = SpaceId::personal(&params.account_id);
        ensure!(
            params
                .execution_resource
                .as_ref()
                .is_none_or(|resource| resource.space == personal),
            "execution selection must use the physical owner's personal space"
        );
        let tx = self.connection.transaction()?;
        let policy = journal::PolicyState::reload_verified(&tx, &self.policy)?;
        let state = policy.verified();
        let source = state
            .space(&params.source_project.space)
            .context("unknown execution source space")?;
        let mut allowed = false;
        for leaf in &source.grants {
            if !state
                .grant(leaf)
                .is_some_and(|grant| grant.payload.actions.contains(&Action::ResourceManage))
            {
                continue;
            }
            match execution_service::verify_execution_service_source_management(
                state,
                &params.account_id,
                &principal,
                &params.source_project,
                leaf,
                &node,
                now_ms,
            ) {
                Ok(()) => {
                    allowed = true;
                    break;
                }
                Err(error @ AuthorizationError::BudgetExceeded) => return Err(error.into()),
                Err(_) => {}
            }
        }
        ensure!(
            allowed,
            "execution selection requires current physical owner resource management"
        );
        let root = source_root(&tx, &params.source_project)?;
        let journal = {
            let mut statement = tx.prepare("SELECT entry FROM policy_journal ORDER BY ordinal")?;
            statement
                .query_map([], |row| row.get::<_, String>(0))?
                .map(|row| Ok(serde_json::from_str::<JournalEntry>(&row?)?))
                .collect::<Result<Vec<_>>>()?
        };
        let mappings = {
            let mut statement = tx.prepare(
                "SELECT key,value FROM local_values WHERE space_id=? AND key LIKE 'execution-service-source:%' ORDER BY key"
            )?;
            statement
                .query_map([personal.as_str()], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?
        };
        let mut selections = Vec::new();
        for (key, encoded) in mappings {
            let resource = ResourceRef {
                space: personal.clone(),
                resource: ResourceId::parse(
                    key.strip_prefix("execution-service-source:")
                        .context("invalid execution service mapping")?,
                )?,
            };
            if params
                .execution_resource
                .as_ref()
                .is_some_and(|expected| expected != &resource)
            {
                continue;
            }
            let mapping: ExecutionServiceMapping = serde_json::from_str(&encoded)?;
            if mapping.source_project != params.source_project {
                continue;
            }
            ensure!(
                source_root(&tx, &resource)? == root,
                "execution source mapping changed"
            );
            match execution_service::project_execution_service_selection(
                state,
                policy.pins(),
                &journal,
                &params.account_id,
                &principal,
                &mapping.source_project,
                &resource,
                &mapping.management_parent,
                &node,
                now_ms,
            ) {
                Ok(selection) => selections.push(selection),
                Err(error @ AuthorizationError::BudgetExceeded) => return Err(error.into()),
                Err(_) => {}
            }
        }
        ensure!(
            params.execution_resource.is_none() || selections.len() == 1,
            "execution service has no current registered management selection"
        );
        tx.commit()?;
        self.policy = policy;
        Ok(serde_json::json!({"selections": selections}))
    }

    pub(crate) fn execution_service_registration_challenge(
        &mut self,
        identity: &LocalIdentity,
        principal: &str,
        params: &Value,
        now_ms: u64,
    ) -> Result<FreshnessChallenge> {
        let _signature_scope = ScopedSignatureVerification::new();
        ensure!(
            serde_json::to_vec(params)?.len() <= MAX_PROOF_BYTES,
            "governance request too large"
        );
        let request: ExecutionServiceRegistrationRequest = serde_json::from_value(params.clone())?;
        ensure!(
            request.intent.issuer.as_str() == principal,
            "execution registration differs from Noise identity"
        );
        let node = PublicKey::parse(&identity.public_key_hex()?)?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let policy = journal::PolicyState::reload_verified(&tx, &self.policy)?;
        let challenge = execution_service::execution_service_registration_challenge(
            policy.verified(),
            &request,
            &node,
            now_ms,
        )?;
        source_root(&tx, &request.intent.payload.source_project)?;
        tx.commit()?;
        Ok(challenge)
    }

    pub(crate) fn governance_command(
        &mut self,
        identity: &LocalIdentity,
        principal: &str,
        method: &str,
        params: &Value,
        now_ms: u64,
    ) -> Result<Value> {
        let _signature_scope =
            (method == "execution_service_register").then(ScopedSignatureVerification::new);
        self.governance_command_with_registration_freshness(
            identity, principal, method, params, now_ms, None,
        )
    }

    pub(crate) fn governance_command_with_registration_freshness(
        &mut self,
        identity: &LocalIdentity,
        principal: &str,
        method: &str,
        params: &Value,
        now_ms: u64,
        registration_freshness: Option<&super::policy::FreshnessProof>,
    ) -> Result<Value> {
        let _signature_scope =
            (method == "execution_service_register").then(ScopedSignatureVerification::new);
        ensure!(
            serde_json::to_vec(params)?.len() <= MAX_PROOF_BYTES,
            "governance request too large"
        );
        let principal = PublicKey::parse(principal)?;
        let node = PublicKey::parse(&identity.public_key_hex()?)?;
        let signer =
            Identity::from_secret_hex(&identity.secret_key_hex).map_err(anyhow::Error::msg)?;
        match method {
            "team_create" => {
                let params: TeamCreateParams = serde_json::from_value(params.clone())?;
                let request = params.request;
                request.verify_signature()?;
                ensure!(
                    request.issuer == principal,
                    "team creator differs from Noise identity"
                );
                ensure!(
                    request.payload.target_node_public_key == node,
                    "team creation targets another Node"
                );
                let request_hash = ObjectHash::body(&request)?;
                let key = format!(
                    "team-create:{}:{}",
                    request.payload.owner_account, request.payload.nonce
                );
                let root = if receipt::<TeamCreateResponse>(
                    &self.connection,
                    &key,
                    &request_hash,
                    &principal,
                )?
                .is_some()
                {
                    None
                } else {
                    Some(self.resource_root(
                        request.payload.source_project.space.as_str(),
                        &request.payload.source_project.resource,
                    )?)
                };
                let tx = self
                    .connection
                    .transaction_with_behavior(TransactionBehavior::Immediate)?;
                if let Some(response) =
                    receipt::<TeamCreateResponse>(&tx, &key, &request_hash, &principal)?
                {
                    check_team_receipt(&response, &request, &node)?;
                    let policy = journal::PolicyState::load(&tx)?;
                    tx.commit()?;
                    self.policy = policy;
                    return Ok(serde_json::to_value(response)?);
                }
                let root = root.context("stored Team receipt disappeared")?;
                ensure!(
                    source_root(&tx, &request.payload.source_project)? == root,
                    "source project mapping changed"
                );
                let policy = journal::PolicyState::load(&tx)?;
                let verified = verify_team_source(&tx, &policy, &request, now_ms)?;
                let update = policy.prepare_team_creation(&verified, now_ms)?;
                let acceptance = Signed::sign(
                    TeamCreationAcceptance {
                        version: AUTH_VERSION,
                        request_hash: request_hash.clone(),
                        space: request.payload.team_project.space.clone(),
                        owner_account: request.payload.owner_account.clone(),
                        target_node_public_key: node,
                        account_head: policy
                            .verified()
                            .head(&PolicyDomain::Account(
                                request.payload.owner_account.clone(),
                            ))
                            .context("owner account head missing")?
                            .clone(),
                        nonce: request.payload.nonce.clone(),
                        accepted_at_ms: now_ms,
                        expires_at_ms: request.payload.expires_at_ms,
                    },
                    &signer,
                )?;
                let response = TeamCreateResponse {
                    acceptance,
                    pin: request.payload.team_pin.clone(),
                    journal_delta: request.payload.journal_delta.clone(),
                };
                let policy = journal::commit_in(&tx, &update)?;
                project_policy_in(&tx, &policy)?;
                tx.execute(
                    "INSERT INTO local_values(space_id,key,value) VALUES(?,?,?)",
                    params![
                        request.payload.team_project.space.as_str(),
                        format!("resource-root:{}", request.payload.team_project.resource),
                        serde_json::to_string(&root)?
                    ],
                )?;
                save_receipt(&tx, &key, request_hash, principal, &response)?;
                ensure!(
                    source_root(&tx, &request.payload.source_project)? == root,
                    "source project mapping changed"
                );
                tx.commit()?;
                self.policy = policy;
                Ok(serde_json::to_value(response)?)
            }
            "space_policy_commit" | "execution_service_register" => {
                let registration: Option<ExecutionServiceRegistrationRequest> =
                    if method == "execution_service_register" {
                        Some(serde_json::from_value(params.clone())?)
                    } else {
                        None
                    };
                let request = if let Some(registration) = &registration {
                    registration.intent.verify_signature()?;
                    ensure!(
                        registration.intent.issuer == principal,
                        "execution registration intent differs from Noise identity"
                    );
                    registration.request.clone()
                } else {
                    serde_json::from_value::<SpaceCommitParams>(params.clone())?.request
                };
                request.proposal.verify_signature()?;
                ensure!(
                    registration.is_some() || request.proposal.issuer == principal,
                    "governor differs from Noise identity"
                );
                let request_hash = if let Some(registration) = &registration {
                    ObjectHash::body(registration)?
                } else {
                    ObjectHash::body(&request)?
                };
                let key = format!(
                    "{}:{}:{}",
                    if registration.is_some() {
                        "execution-service-register"
                    } else {
                        "space-governance"
                    },
                    request.proposal.payload.space,
                    request.nonce
                );
                let tx = self
                    .connection
                    .transaction_with_behavior(TransactionBehavior::Immediate)?;
                if let Some(response) =
                    receipt::<SpaceCommitResponse>(&tx, &key, &request_hash, &principal)?
                {
                    check_space_receipt(&response, &request, &node)?;
                    let policy = if registration.is_some() {
                        journal::PolicyState::reload_verified(&tx, &self.policy)?
                    } else {
                        journal::PolicyState::load(&tx)?
                    };
                    tx.commit()?;
                    self.policy = policy;
                    return Ok(serde_json::to_value(response)?);
                }
                let policy = if registration.is_some() {
                    journal::PolicyState::reload_verified(&tx, &self.policy)?
                } else {
                    journal::PolicyState::load(&tx)?
                };
                let state = policy.verified();
                let (commit_heads, authorization_expiry, execution_root) =
                    if let Some(registration) = &registration {
                        let verified = execution_service::verify_execution_service_registration(
                            state,
                            registration,
                            &node,
                            now_ms,
                        )?;
                        if let Some(proof) = registration_freshness {
                            execution_service::verify_execution_service_registration_freshness(
                                state,
                                registration,
                                &proof.challenge,
                                &proof.receipts,
                                &node,
                                now_ms,
                            )?;
                        } else {
                            let remote: Vec<_> = verified
                                .expected_heads()
                                .iter()
                                .filter(|head| {
                                    let authority = match &head.domain {
                                        PolicyDomain::Account(id) => {
                                            state.account(id).map(|value| &value.authority)
                                        }
                                        PolicyDomain::Space(id) => {
                                            state.space(id).map(|value| &value.authority)
                                        }
                                    };
                                    authority.is_none_or(|authority| authority.public_key != node)
                                })
                                .map(|head| head.domain.clone())
                                .collect();
                            if !remote.is_empty() {
                                return Err(AuthorizationError::NeedFreshness(remote).into());
                            }
                        }
                        let root = source_root(&tx, &registration.intent.payload.source_project)?;
                        (
                            verified.commit_heads().to_vec(),
                            verified
                                .expires_at_ms()
                                .min(registration.intent.payload.expires_at_ms),
                            Some(root),
                        )
                    } else {
                        let verified =
                            governance::verify_space_governance_request(state, &request, now_ms)?;
                        (
                            verified.expected_heads().to_vec(),
                            verified.expires_at_ms(),
                            None,
                        )
                    };
                let space = state
                    .space(&request.proposal.payload.space)
                    .context("space is unavailable")?;
                ensure!(
                    space.authority.public_key == node,
                    "this Node is not the current space authority"
                );
                let expires_at_ms = now_ms
                    .saturating_add(EXECUTION_LEASE_MS)
                    .min(authorization_expiry);
                let challenge = Signed::sign(
                    PolicyCommit {
                        version: AUTH_VERSION,
                        previous: state
                            .head(&PolicyDomain::Space(space.space.clone()))
                            .context("space head missing")?
                            .clone(),
                        proposal_hash: request.proposal.hash()?,
                        accepted_at_ms: 0,
                        issued_at_ms: now_ms,
                        expires_at_ms,
                        nonce: request.nonce.clone(),
                        dependencies: commit_heads,
                    },
                    &signer,
                )?;
                let challenge_hash = challenge.challenge_hash()?;
                let receipts = challenge
                    .payload
                    .dependencies
                    .iter()
                    .map(|current| {
                        let authority = match &current.domain {
                            PolicyDomain::Account(id) => {
                                state.account(id).map(|value| &value.authority)
                            }
                            PolicyDomain::Space(id) => {
                                state.space(id).map(|value| &value.authority)
                            }
                        }
                        .context("dependency authority is unavailable")?;
                        ensure!(
                            authority.public_key == node,
                            "governance requires freshness from a remote authority"
                        );
                        Ok(Signed::sign(
                            FreshnessReceipt {
                                version: AUTH_VERSION,
                                current: current.clone(),
                                challenge_hash: challenge_hash.clone(),
                                issued_at_ms: now_ms,
                                expires_at_ms,
                            },
                            &signer,
                        )?)
                    })
                    .collect::<Result<Vec<_>>>()?;
                let mut accepted = challenge.payload;
                accepted.accepted_at_ms = now_ms;
                let committed = Committed {
                    proposal: request.proposal,
                    commit: Signed::sign(accepted, &signer)?,
                    receipts,
                };
                if let Some(registration) = &registration {
                    execution_service::verify_execution_service_registration_commit(
                        state,
                        registration,
                        &committed,
                        registration_freshness
                            .map(|proof| (&proof.challenge, proof.receipts.as_slice())),
                        &node,
                        now_ms,
                    )?;
                }
                let mut journal_delta = request.new_objects;
                journal_delta.push(JournalEntry::SpaceCommit(committed.clone()));
                let update = policy.prepare_network_update(&journal_delta, now_ms)?;
                let response = SpaceCommitResponse {
                    committed,
                    journal_delta,
                };
                let policy = journal::commit_in(&tx, &update)?;
                project_policy_in(&tx, &policy)?;
                if let Some(registration) = &registration {
                    let root = execution_root
                        .as_ref()
                        .context("execution root is missing")?;
                    let resource = &registration.intent.payload.execution_resource;
                    tx.execute(
                        "INSERT INTO local_values(space_id,key,value) VALUES(?,?,?)",
                        params![
                            resource.space.as_str(),
                            format!("resource-root:{}", resource.resource),
                            serde_json::to_string(root)?
                        ],
                    )?;
                    let [JournalEntry::Resource(_), JournalEntry::Grant(parent)] =
                        registration.request.new_objects.as_slice()
                    else {
                        bail!("execution registration management parent is missing");
                    };
                    tx.execute(
                        "INSERT INTO local_values(space_id,key,value) VALUES(?,?,?)",
                        params![
                            resource.space.as_str(),
                            format!("execution-service-source:{}", resource.resource),
                            serde_json::to_string(&ExecutionServiceMapping {
                                source_project: registration.intent.payload.source_project.clone(),
                                management_parent: parent.hash()?,
                            })?
                        ],
                    )?;
                }
                save_receipt(&tx, &key, request_hash, principal, &response)?;
                if let Some(registration) = &registration {
                    let root = execution_root
                        .as_ref()
                        .context("execution root is missing")?;
                    ensure!(
                        source_root(&tx, &registration.intent.payload.source_project)? == *root
                            && source_root(&tx, &registration.intent.payload.execution_resource)?
                                == *root,
                        "execution registration project mapping changed"
                    );
                }
                tx.commit()?;
                self.policy = policy;
                Ok(serde_json::to_value(response)?)
            }
            _ => bail!("unknown governance method"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::identity::test_support::{TestNetwork, now};
    use governance::{
        DirectionalGrantIntent, PreparedSpaceGovernanceChange, PreparedTeamCreation,
        SpaceMembershipIntent, SpaceResourceIntent,
    };
    use serde_json::json;

    struct TeamClient {
        bundle: ClientAuthorizationBundle,
        pins: Vec<PolicyPin>,
        space: SpaceId,
        resource: ResourceId,
    }

    fn prepare_team(network: &TestNetwork) -> Result<PreparedTeamCreation> {
        let root = Identity::generate().map_err(anyhow::Error::msg)?;
        governance::prepare_team_creation(
            &root,
            &network.client,
            network.account.as_str(),
            &network.node.public_key_hex()?,
            &network.recovery.public_key_hex(),
            network.resource.as_str(),
            &serde_json::to_string(&network.pins)?,
            &serde_json::to_string(&network.bundle)?,
            (now() + 90_000) as f64,
            now() as f64,
        )
        .map_err(anyhow::Error::msg)
    }

    fn create_team(network: &TestNetwork, store: &mut Store) -> Result<TeamClient> {
        let prepared = prepare_team(network)?;
        let request: Signed<TeamCreationRequest> =
            serde_json::from_str(&prepared.request_json().map_err(anyhow::Error::msg)?)?;
        let response: TeamCreateResponse = serde_json::from_value(store.governance_command(
            &network.node,
            &network.client.public_key_hex(),
            "team_create",
            &json!({"request":request}),
            now(),
        )?)?;
        let verified = governance::finish_team_creation(
            &prepared,
            &serde_json::to_string(&response.acceptance)?,
            now() as f64,
        )
        .map_err(anyhow::Error::msg)?;
        Ok(TeamClient {
            bundle: serde_json::from_str(&verified.bundle_json().map_err(anyhow::Error::msg)?)?,
            pins: serde_json::from_str(&verified.pins_json().map_err(anyhow::Error::msg)?)?,
            space: request.payload.team_project.space,
            resource: request.payload.team_project.resource,
        })
    }

    impl TeamClient {
        fn pins_json(&self) -> Result<String> {
            Ok(serde_json::to_string(&self.pins)?)
        }
        fn bundle_json(&self) -> Result<String> {
            Ok(serde_json::to_string(&self.bundle)?)
        }
        fn commit(
            &mut self,
            network: &TestNetwork,
            store: &mut Store,
            prepared: &PreparedSpaceGovernanceChange,
        ) -> Result<SpaceCommitResponse> {
            let request: SpaceGovernanceRequest =
                serde_json::from_str(&prepared.request_json().map_err(anyhow::Error::msg)?)?;
            let params = json!({"request":request});
            let response: SpaceCommitResponse = serde_json::from_value(store.governance_command(
                &network.node,
                &network.client.public_key_hex(),
                "space_policy_commit",
                &params,
                now(),
            )?)?;
            let count = row_counts(store)?;
            assert_eq!(
                store.governance_command(
                    &network.node,
                    &network.client.public_key_hex(),
                    "space_policy_commit",
                    &params,
                    now() + 200_000,
                )?,
                serde_json::to_value(&response)?
            );
            assert_eq!(row_counts(store)?, count);
            let mut conflicting = request.clone();
            let mut proposal = conflicting.proposal.payload;
            proposal.sequence += 1;
            conflicting.proposal = Signed::sign(proposal, &network.client)?;
            let error = store
                .governance_command(
                    &network.node,
                    &network.client.public_key_hex(),
                    "space_policy_commit",
                    &json!({"request":conflicting}),
                    now(),
                )
                .unwrap_err();
            assert!(
                error.to_string().contains("nonce was already used"),
                "{error}"
            );
            assert_eq!(row_counts(store)?, count);
            let verified = governance::finish_space_governance_change(
                prepared,
                &serde_json::to_string(&response.committed)?,
                now() as f64,
            )
            .map_err(anyhow::Error::msg)?;
            self.bundle =
                serde_json::from_str(&verified.bundle_json().map_err(anyhow::Error::msg)?)?;
            self.pins = serde_json::from_str(&verified.pins_json().map_err(anyhow::Error::msg)?)?;
            Ok(response)
        }
    }

    fn row_counts(store: &Store) -> Result<Vec<u64>> {
        [
            "policy_pins",
            "policy_journal",
            "current_policy_heads",
            "policy_nonces",
            "identity_meta",
            "local_values",
            "resource_records",
            "spaces",
        ]
        .iter()
        .map(|table| {
            Ok(store
                .connection
                .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                    row.get(0)
                })?)
        })
        .collect()
    }

    fn prepare_execution_registration(
        network: &TestNetwork,
        source: &ResourceRef,
        pins: &[PolicyPin],
        bundle: &ClientAuthorizationBundle,
    ) -> Result<execution_service::PreparedExecutionServiceRegistration> {
        let material = bootstrap::recovery::build_account_recovery_material(
            &network.created,
            &network.root,
            &network.recovery,
            "https://node.example",
            now() as f64,
        )
        .map_err(anyhow::Error::msg)?;
        execution_service::prepare_execution_service_registration(
            &serde_json::to_string(source)?,
            &material,
            &network.client,
            &serde_json::to_string(pins)?,
            &serde_json::to_string(bundle)?,
            &network.node.public_key_hex()?,
            now() as f64,
            (now() + 60_000) as f64,
        )
        .map_err(anyhow::Error::msg)
    }

    #[test]
    fn execution_service_registration_maps_personal_and_team_projects_atomically() -> Result<()> {
        for team_source in [false, true] {
            let network = TestNetwork::new()?;
            let path = network.directory.join("execution-registration.sqlite3");
            let mut store = network.at(&path)?;
            let (source, pins, bundle) = if team_source {
                let team = create_team(&network, &mut store)?;
                (
                    ResourceRef {
                        space: team.space,
                        resource: team.resource,
                    },
                    team.pins,
                    team.bundle,
                )
            } else {
                (
                    ResourceRef {
                        space: network.space.clone(),
                        resource: network.resource.clone(),
                    },
                    network.pins.clone(),
                    network.bundle.clone(),
                )
            };
            let prepared = prepare_execution_registration(&network, &source, &pins, &bundle)?;
            eprintln!(
                "execution registration source={} phase=prepare usage={}",
                if team_source { "team" } else { "personal" },
                prepared.proof_usage_json().map_err(anyhow::Error::msg)?
            );
            let params: Value =
                serde_json::from_str(&prepared.request_json().map_err(anyhow::Error::msg)?)?;
            let execution = ResourceRef {
                space: SpaceId::parse(&prepared.space_id())?,
                resource: ResourceId::parse(&prepared.resource_id())?,
            };
            assert_eq!(execution.space, SpaceId::personal(&network.account));
            assert_ne!(execution.resource, source.resource);
            let response = store.governance_command(
                &network.node,
                &network.client.public_key_hex(),
                "execution_service_register",
                &params,
                now(),
            )?;
            assert_eq!(source_root(&store.connection, &source)?, network.project);
            assert_eq!(source_root(&store.connection, &execution)?, network.project);
            let record = store
                .policy
                .verified()
                .resource(&execution.space, &execution.resource)
                .context("execution resource missing")?;
            assert_eq!(record.payload.kind, ResourceKind::ExecutionService);
            assert_eq!(
                record.payload.host,
                ServiceId::from_key(&PublicKey::parse(&network.node.public_key_hex()?)?)
            );
            let verified = execution_service::finish_execution_service_registration(
                &prepared,
                &serde_json::to_string(&response["committed"])?,
                (now() + 200_000) as f64,
            )
            .map_err(anyhow::Error::msg)?;
            eprintln!(
                "execution registration source={} phase=commit usage={}",
                if team_source { "team" } else { "personal" },
                verified.proof_usage_json().map_err(anyhow::Error::msg)?
            );
            assert_eq!(verified.resource_id(), execution.resource.as_str());
            assert_eq!(verified.space_id(), execution.space.as_str());
            let result_bundle: ClientAuthorizationBundle =
                serde_json::from_str(&verified.bundle_json().map_err(anyhow::Error::msg)?)?;
            let selection_params = json!({
                "account_id": network.account,
                "device_public_key": network.client.public_key_hex(),
                "source_project": source,
                "target_node_public_key": network.node.public_key_hex()?,
                "execution_resource": execution,
            });
            let before_select = row_counts(&store)?;
            let selected = store.execution_service_select(
                &network.node,
                &network.client.public_key_hex(),
                &selection_params,
                now(),
            )?;
            assert_eq!(row_counts(&store)?, before_select);
            assert_eq!(selected["selections"].as_array().unwrap().len(), 1);
            let selection: execution_service::ExecutionServiceSelection =
                serde_json::from_value(selected["selections"][0].clone())?;
            assert_eq!(selection.authorization.grant_leaf, result_bundle.grant_leaf);
            assert_eq!(selection.source_project, source);
            assert_eq!(selection.execution_resource, execution);
            let selection_json = serde_json::to_string(&selection)?;
            assert!(!selection_json.contains(network.project.to_str().unwrap()));
            assert!(!selection_json.contains("secret"));
            let native_selection = execution_service::verify_execution_service_selection(
                &selection_json,
                &verified.pins_json().map_err(anyhow::Error::msg)?,
                &serde_json::to_string(&source)?,
                &serde_json::to_string(&execution)?,
                network.account.as_str(),
                &network.client.public_key_hex(),
                &network.node.public_key_hex()?,
                now() as f64,
            )
            .map_err(anyhow::Error::msg)?;
            assert_eq!(native_selection.grant_leaf(), &result_bundle.grant_leaf);
            for wrong_target in [
                network.client.public_key_hex(),
                Identity::generate()
                    .map_err(anyhow::Error::msg)?
                    .public_key_hex(),
            ] {
                assert!(
                    execution_service::verify_execution_service_selection(
                        &selection_json,
                        &verified.pins_json().map_err(anyhow::Error::msg)?,
                        &serde_json::to_string(&source)?,
                        &serde_json::to_string(&execution)?,
                        network.account.as_str(),
                        &network.client.public_key_hex(),
                        &wrong_target,
                        now() as f64,
                    )
                    .is_err()
                );
            }
            let parent = store
                .policy
                .verified()
                .grant(&result_bundle.grant_leaf)
                .context("execution management parent missing")?;
            assert_eq!(
                parent.payload.scope,
                ResourceScope::Exact(execution.clone())
            );
            assert!(parent.payload.actions.contains(&Action::ExecutionSubmit));
            assert!(parent.payload.actions.contains(&Action::ExecutionResolve));
            assert_eq!(parent.payload.remaining_delegations, 1);
            assert!(parent.payload.parent.is_none());
            assert_eq!(parent.issuer, PublicKey::of(&network.root));
            assert_eq!(
                store.policy.verified().grant(&network.bundle.grant_leaf),
                replay_policies(&network.pins, &network.bundle.journal)?
                    .grant(&network.bundle.grant_leaf)
            );
            if team_source {
                assert!(!result_bundle.journal.iter().any(|entry| match entry {
                    JournalEntry::SpaceGenesis(value) | JournalEntry::Space(value) =>
                        value.payload.space == source.space,
                    JournalEntry::SpaceCommit(value) =>
                        value.proposal.payload.space == source.space,
                    _ => false,
                }));
            }
            bootstrap::recovery::inspect_account_recovery_material(
                &verified.recovery_material_json(),
                now() as f64,
            )
            .map_err(anyhow::Error::msg)?;
            let after = replay_policies(
                &serde_json::from_str::<Vec<PolicyPin>>(
                    &verified.pins_json().map_err(anyhow::Error::msg)?,
                )?,
                &serde_json::from_str::<ClientAuthorizationBundle>(
                    &verified.bundle_json().map_err(anyhow::Error::msg)?,
                )?
                .journal,
            )?;
            assert_eq!(
                after.resource(&execution.space, &execution.resource),
                store
                    .policy
                    .verified()
                    .resource(&execution.space, &execution.resource)
            );
            let counts = row_counts(&store)?;
            drop(store);
            let mut reopened = Store::open(&path)?;
            assert_eq!(
                reopened.governance_command(
                    &network.node,
                    &network.client.public_key_hex(),
                    "execution_service_register",
                    &params,
                    now() + 200_000
                )?,
                response
            );
            assert_eq!(row_counts(&reopened)?, counts);
            assert_eq!(
                reopened.execution_service_select(
                    &network.node,
                    &network.client.public_key_hex(),
                    &selection_params,
                    now() + 200_000,
                )?,
                selected
            );
            assert_eq!(row_counts(&reopened)?, counts);
            assert_eq!(
                source_root(&reopened.connection, &execution)?,
                network.project
            );
        }
        Ok(())
    }

    #[test]
    fn execution_service_selection_keeps_original_parent_and_reloads_revocations() -> Result<()> {
        for revoke in ["source", "client", "parent", "node"] {
            let network = TestNetwork::new()?;
            let path = network.directory.join("execution-selection.sqlite3");
            let mut store = network.at(&path)?;
            let source = ResourceRef {
                space: network.space.clone(),
                resource: network.resource.clone(),
            };
            let prepared =
                prepare_execution_registration(&network, &source, &network.pins, &network.bundle)?;
            let request: Value =
                serde_json::from_str(&prepared.request_json().map_err(anyhow::Error::msg)?)?;
            let response = store.governance_command(
                &network.node,
                &network.client.public_key_hex(),
                "execution_service_register",
                &request,
                now(),
            )?;
            let resource = ResourceRef {
                space: SpaceId::parse(&prepared.space_id())?,
                resource: ResourceId::parse(&prepared.resource_id())?,
            };
            let committed = execution_service::finish_execution_service_registration(
                &prepared,
                &serde_json::to_string(&response["committed"])?,
                now() as f64,
            )
            .map_err(anyhow::Error::msg)?;
            let bundle: ClientAuthorizationBundle =
                serde_json::from_str(&committed.bundle_json().map_err(anyhow::Error::msg)?)?;
            let parent = bundle.grant_leaf;
            let mut other = store
                .policy
                .verified()
                .grant(&parent)
                .context("management parent missing")?
                .payload
                .clone();
            other.actions = vec![Action::OperationStatusRead];
            other.remaining_delegations = 0;
            other.nonce = Nonce::random()?;
            other.issued_under = store
                .policy
                .verified()
                .head(&PolicyDomain::Space(network.space.clone()))
                .unwrap()
                .clone();
            let other = Signed::sign(other, &network.root)?;
            let mut document = store
                .policy
                .verified()
                .space(&network.space)
                .unwrap()
                .clone();
            document.previous = Some(other.payload.issued_under.clone());
            document.sequence += 1;
            document.grants.push(other.hash()?);
            document.grants.sort();
            store.install_policy_journal(
                &[
                    JournalEntry::Grant(other.clone()),
                    JournalEntry::Space(Signed::sign(document, &network.root)?),
                ],
                now() + 1,
            )?;
            let params = json!({
                "account_id":network.account,"device_public_key":network.client.public_key_hex(),
                "source_project":source,"target_node_public_key":network.node.public_key_hex()?,
                "execution_resource":resource,
            });
            drop(store);
            let mut store = Store::open(&path)?;
            let before = row_counts(&store)?;
            let selected = store.execution_service_select(
                &network.node,
                &network.client.public_key_hex(),
                &params,
                now() + 2,
            )?;
            let selection: execution_service::ExecutionServiceSelection =
                serde_json::from_value(selected["selections"][0].clone())?;
            assert_eq!(selection.authorization.grant_leaf, parent);
            assert_ne!(selection.authorization.grant_leaf, other.hash()?);
            let mut list_params = params.clone();
            list_params
                .as_object_mut()
                .unwrap()
                .remove("execution_resource");
            assert_eq!(
                store.execution_service_select(
                    &network.node,
                    &network.client.public_key_hex(),
                    &list_params,
                    now() + 2,
                )?,
                selected
            );
            let verified = execution_service::verify_execution_service_selection(
                &serde_json::to_string(&selection)?,
                &committed.pins_json().map_err(anyhow::Error::msg)?,
                &serde_json::to_string(&source)?,
                &serde_json::to_string(&resource)?,
                network.account.as_str(),
                &network.client.public_key_hex(),
                &network.node.public_key_hex()?,
                (now() + 2) as f64,
            )
            .map_err(anyhow::Error::msg)?;
            assert_eq!(verified.grant_leaf(), &parent);
            let mut wrong_leaf = selection.clone();
            wrong_leaf.authorization.grant_leaf = other.hash()?;
            wrong_leaf
                .authorization
                .journal
                .push(JournalEntry::Grant(other));
            assert!(
                execution_service::verify_execution_service_selection(
                    &serde_json::to_string(&wrong_leaf)?,
                    &committed.pins_json().map_err(anyhow::Error::msg)?,
                    &serde_json::to_string(&source)?,
                    &serde_json::to_string(&resource)?,
                    network.account.as_str(),
                    &network.client.public_key_hex(),
                    &network.node.public_key_hex()?,
                    (now() + 2) as f64,
                )
                .is_err()
            );
            let foreign = TestNetwork::new()?;
            let mut foreign_params = params.clone();
            foreign_params["account_id"] = serde_json::to_value(&foreign.account)?;
            foreign_params["device_public_key"] = json!(foreign.client.public_key_hex());
            assert!(
                store
                    .execution_service_select(
                        &network.node,
                        &foreign.client.public_key_hex(),
                        &foreign_params,
                        now() + 2,
                    )
                    .is_err()
            );
            let mut wrong_target = params.clone();
            wrong_target["target_node_public_key"] = json!(foreign.node.public_key_hex()?);
            assert!(
                store
                    .execution_service_select(
                        &foreign.node,
                        &network.client.public_key_hex(),
                        &wrong_target,
                        now() + 2,
                    )
                    .is_err()
            );
            assert!(
                store
                    .execution_service_select(
                        &network.node,
                        &foreign.client.public_key_hex(),
                        &params,
                        now() + 2,
                    )
                    .is_err()
            );
            assert_eq!(row_counts(&store)?, before);
            let mut independent = Store::open(&path)?;
            match revoke {
                "client" => network.revoke_client(&mut independent)?,
                "node" => network.revoke_node(&mut independent)?,
                "source" | "parent" => {
                    let mut document = independent
                        .policy
                        .verified()
                        .space(&network.space)
                        .unwrap()
                        .clone();
                    document.previous = independent
                        .policy
                        .verified()
                        .head(&PolicyDomain::Space(network.space.clone()))
                        .cloned();
                    document.sequence += 1;
                    let removed = if revoke == "source" {
                        &network.bundle.grant_leaf
                    } else {
                        &parent
                    };
                    document.grants.retain(|leaf| leaf != removed);
                    independent.install_policy_journal(
                        &[JournalEntry::Space(Signed::sign(document, &network.root)?)],
                        now() + 3,
                    )?;
                }
                _ => unreachable!(),
            }
            let after_revoke = row_counts(&independent)?;
            assert!(
                store
                    .execution_service_select(
                        &network.node,
                        &network.client.public_key_hex(),
                        &params,
                        now() + 4,
                    )
                    .is_err(),
                "stale selection accepted {revoke} revocation"
            );
            assert_eq!(row_counts(&store)?, after_revoke);
        }
        Ok(())
    }

    #[test]
    fn execution_service_registration_rejects_tampering_foreign_owner_and_missing_root()
    -> Result<()> {
        let network = TestNetwork::new()?;
        let mut store = network.memory()?;
        let source = ResourceRef {
            space: network.space.clone(),
            resource: network.resource.clone(),
        };
        let prepared =
            prepare_execution_registration(&network, &source, &network.pins, &network.bundle)?;
        let request: ExecutionServiceRegistrationRequest =
            serde_json::from_str(&prepared.request_json().map_err(anyhow::Error::msg)?)?;
        let original = serde_json::to_value(&request)?;
        let counts = row_counts(&store)?;
        let stranger = Identity::generate().map_err(anyhow::Error::msg)?;
        assert!(
            store
                .governance_command(
                    &network.node,
                    &stranger.public_key_hex(),
                    "execution_service_register",
                    &original,
                    now()
                )
                .is_err()
        );
        for mutation in 0..5 {
            let mut changed = request.clone();
            match mutation {
                0 => changed.intent.payload.source_project.resource = ResourceId::random()?,
                1 => changed.intent.payload.execution_resource = source.clone(),
                2 => changed.intent.payload.target_node_public_key = PublicKey::of(&stranger),
                3 => {
                    changed.intent.payload.account = AccountId::from_root(&PublicKey::of(&stranger))
                }
                _ => {
                    changed.intent.payload.policy_request_hash =
                        ObjectHash::body(&"other governance")?
                }
            }
            changed.intent = Signed::sign(changed.intent.payload, &network.client)?;
            changed.root_approval = Signed::sign(
                execution_service::ExecutionServiceRootApproval {
                    version: AUTH_VERSION,
                    intent_hash: ObjectHash::body(&changed.intent)?,
                },
                &network.root,
            )?;
            assert!(
                store
                    .governance_command(
                        &network.node,
                        &network.client.public_key_hex(),
                        "execution_service_register",
                        &serde_json::to_value(changed)?,
                        now()
                    )
                    .is_err()
            );
            assert_eq!(row_counts(&store)?, counts);
        }
        store.connection.execute(
            "DELETE FROM local_values WHERE space_id=? AND key=?",
            params![
                source.space.as_str(),
                format!("resource-root:{}", source.resource)
            ],
        )?;
        let missing_root_counts = row_counts(&store)?;
        assert!(
            store
                .governance_command(
                    &network.node,
                    &network.client.public_key_hex(),
                    "execution_service_register",
                    &original,
                    now()
                )
                .is_err()
        );
        assert_eq!(row_counts(&store)?, missing_root_counts);
        store.local_set(
            source.space.as_str(),
            &format!("resource-root:{}", source.resource),
            &serde_json::json!(network.project),
        )?;
        assert!(
            store
                .governance_command(
                    &network.node,
                    &network.client.public_key_hex(),
                    "execution_service_register",
                    &original,
                    now() + 60_000
                )
                .is_err()
        );
        assert_eq!(row_counts(&store)?, counts);
        Ok(())
    }

    #[test]
    fn execution_service_registration_rolls_back_mapping_and_receipt_failures() -> Result<()> {
        let network = TestNetwork::new()?;
        let mut store = network.memory()?;
        let source = ResourceRef {
            space: network.space.clone(),
            resource: network.resource.clone(),
        };
        let prepared =
            prepare_execution_registration(&network, &source, &network.pins, &network.bundle)?;
        let request: ExecutionServiceRegistrationRequest =
            serde_json::from_str(&prepared.request_json().map_err(anyhow::Error::msg)?)?;
        let original = serde_json::to_value(&request)?;
        let execution = &request.intent.payload.execution_resource;
        let before = row_counts(&store)?;
        for (name, statement) in [
            (
                "reject_execution_mapping",
                "CREATE TEMP TRIGGER reject_execution_mapping BEFORE INSERT ON local_values WHEN NEW.key LIKE 'resource-root:%' BEGIN SELECT RAISE(ABORT,'injected mapping failure'); END;",
            ),
            (
                "reject_execution_receipt",
                "CREATE TEMP TRIGGER reject_execution_receipt BEFORE INSERT ON identity_meta WHEN NEW.key LIKE 'execution-service-register:%' BEGIN SELECT RAISE(ABORT,'injected receipt failure'); END;",
            ),
        ] {
            store.connection.execute_batch(statement)?;
            assert!(
                store
                    .governance_command(
                        &network.node,
                        &network.client.public_key_hex(),
                        "execution_service_register",
                        &original,
                        now()
                    )
                    .is_err()
            );
            assert_eq!(row_counts(&store)?, before);
            assert!(
                store
                    .policy
                    .verified()
                    .resource(&execution.space, &execution.resource)
                    .is_none()
            );
            assert!(
                journal::PolicyState::load(&store.connection)?
                    .verified()
                    .resource(&execution.space, &execution.resource)
                    .is_none()
            );
            store
                .connection
                .execute_batch(&format!("DROP TRIGGER {name}"))?;
        }
        let response = store.governance_command(
            &network.node,
            &network.client.public_key_hex(),
            "execution_service_register",
            &original,
            now(),
        )?;
        let counts = row_counts(&store)?;
        let mut changed = request.clone();
        changed.intent.payload.source_project.resource = ResourceId::random()?;
        changed.intent = Signed::sign(changed.intent.payload, &network.client)?;
        assert!(
            store
                .governance_command(
                    &network.node,
                    &network.client.public_key_hex(),
                    "execution_service_register",
                    &serde_json::to_value(changed)?,
                    now() + 200_000
                )
                .is_err()
        );
        assert_eq!(
            store.governance_command(
                &network.node,
                &network.client.public_key_hex(),
                "execution_service_register",
                &original,
                now() + 200_000
            )?,
            response
        );
        assert_eq!(row_counts(&store)?, counts);
        Ok(())
    }

    #[test]
    fn execution_service_registration_reloads_current_account_after_second_connection_revocation()
    -> Result<()> {
        let network = TestNetwork::new()?;
        let path = network.directory.join("execution-revocation.sqlite3");
        let mut store = network.at(&path)?;
        let source = ResourceRef {
            space: network.space.clone(),
            resource: network.resource.clone(),
        };
        let prepared =
            prepare_execution_registration(&network, &source, &network.pins, &network.bundle)?;
        let params: Value =
            serde_json::from_str(&prepared.request_json().map_err(anyhow::Error::msg)?)?;
        let mut independent = Store::open(&path)?;
        network.revoke_client(&mut independent)?;
        let before = row_counts(&independent)?;
        assert!(
            store
                .governance_command(
                    &network.node,
                    &network.client.public_key_hex(),
                    "execution_service_register",
                    &params,
                    now()
                )
                .is_err()
        );
        assert_eq!(row_counts(&store)?, before);
        assert!(
            journal::PolicyState::load(&store.connection)?
                .verified()
                .resource(&source.space, &ResourceId::parse(&prepared.resource_id())?,)
                .is_none()
        );
        Ok(())
    }

    #[test]
    fn execution_service_registration_collects_remote_team_freshness_without_personal_replica()
    -> Result<()> {
        let network = TestNetwork::new()?;
        let mut local = network.memory()?;
        let team_root = Identity::generate().map_err(anyhow::Error::msg)?;
        let prepared_team = governance::prepare_team_creation(
            &team_root,
            &network.client,
            network.account.as_str(),
            &network.node.public_key_hex()?,
            &network.recovery.public_key_hex(),
            network.resource.as_str(),
            &serde_json::to_string(&network.pins)?,
            &serde_json::to_string(&network.bundle)?,
            (now() + 90_000) as f64,
            now() as f64,
        )
        .map_err(anyhow::Error::msg)?;
        let team_request: Signed<TeamCreationRequest> =
            serde_json::from_str(&prepared_team.request_json().map_err(anyhow::Error::msg)?)?;
        let response: TeamCreateResponse = serde_json::from_value(local.governance_command(
            &network.node,
            &network.client.public_key_hex(),
            "team_create",
            &json!({"request":team_request}),
            now(),
        )?)?;
        let team_created = governance::finish_team_creation(
            &prepared_team,
            &serde_json::to_string(&response.acceptance)?,
            now() as f64,
        )
        .map_err(anyhow::Error::msg)?;
        let team_bundle: ClientAuthorizationBundle =
            serde_json::from_str(&team_created.bundle_json().map_err(anyhow::Error::msg)?)?;
        let source = team_request.payload.team_project;
        let remote_identity = LocalIdentity::generate();
        let remote_signer = Identity::from_secret_hex(&remote_identity.secret_key_hex)
            .map_err(anyhow::Error::msg)?;
        let remote_key = PublicKey::of(&remote_signer);
        let account_head = local
            .policy
            .verified()
            .head(&PolicyDomain::Account(network.account.clone()))
            .context("account head missing")?
            .clone();
        let team_head = local
            .policy
            .verified()
            .head(&PolicyDomain::Space(source.space.clone()))
            .context("team head missing")?
            .clone();
        let mut certificate = DeviceCertificate {
            version: AUTH_VERSION,
            owner: PolicyDomain::Account(network.account.clone()),
            subject: Actor::Service(ServiceId::from_key(&remote_key)),
            device: DeviceId::from_key(&remote_key),
            public_key: remote_key.clone(),
            uses: vec![DeviceUse::Noise, DeviceUse::ServiceInvocation],
            possession: None,
            enrollment_grant: None,
            issued_under: account_head.clone(),
            nonce: Nonce::random()?,
            not_before_ms: now(),
            expires_at_ms: now() + 1_000_000,
        };
        certificate.prove_possession(&remote_signer)?;
        let certificate = Signed::sign(certificate, &network.root)?;
        let mut account = local
            .policy
            .verified()
            .account(&network.account)
            .context("account missing")?
            .clone();
        account.previous = Some(account_head);
        account.sequence += 1;
        account.devices.push(certificate.hash()?);
        account.devices.sort();
        let mut grant = local
            .policy
            .verified()
            .grant(&team_bundle.grant_leaf)
            .context("team owner grant missing")?
            .payload
            .clone();
        grant.audience = Audience {
            service: ServiceId::from_key(&remote_key),
            device: DeviceId::from_key(&remote_key),
        };
        grant.issued_under = team_head.clone();
        grant.nonce = Nonce::random()?;
        let grant = Signed::sign(grant, &team_root)?;
        let mut team = local
            .policy
            .verified()
            .space(&source.space)
            .context("team missing")?
            .clone();
        team.previous = Some(team_head);
        team.authority = AuthorityBinding::from_key(remote_key.clone());
        team.authority_epoch += 1;
        team.sequence = 0;
        team.governance_grant = None;
        team.devices.push(certificate.hash()?);
        team.devices.sort();
        team.grants.retain(|hash| hash != &team_bundle.grant_leaf);
        team.grants.push(grant.hash()?);
        team.grants.sort();
        local.install_policy_journal(
            &[
                JournalEntry::Device(certificate),
                JournalEntry::Account(Signed::sign(account, &network.root)?),
                JournalEntry::Grant(grant.clone()),
                JournalEntry::Space(Signed::sign(team, &team_root)?),
            ],
            now(),
        )?;
        let mut statement = local
            .connection
            .prepare("SELECT entry FROM policy_journal ORDER BY ordinal")?;
        let journal: Vec<JournalEntry> = statement
            .query_map([], |row| row.get::<_, String>(0))?
            .map(|row| Ok(serde_json::from_str(&row?)?))
            .collect::<Result<_>>()?;
        drop(statement);
        let bundle = ClientAuthorizationBundle {
            journal,
            grant_leaf: grant.hash()?,
        };
        let remote_journal: Vec<_> = bundle
            .journal
            .iter()
            .filter(|entry| match entry {
                JournalEntry::AccountGenesis(value) | JournalEntry::Account(value) => {
                    value.payload.account == network.account
                }
                JournalEntry::AccountCommit(value) => {
                    value.proposal.payload.account == network.account
                }
                JournalEntry::SpaceGenesis(value) | JournalEntry::Space(value) => {
                    value.payload.space == source.space
                }
                JournalEntry::SpaceCommit(value) => value.proposal.payload.space == source.space,
                JournalEntry::Device(value) => {
                    value.payload.owner == PolicyDomain::Account(network.account.clone())
                }
                JournalEntry::Authenticator(value) => value.payload.account == network.account,
                JournalEntry::Resource(value) => value.payload.space == source.space,
                JournalEntry::Membership(value) => value.payload.space == source.space,
                JournalEntry::Grant(value) => value.payload.owner_space == source.space,
            })
            .cloned()
            .collect();
        let remote_pins: Vec<_> = local
            .policy
            .pins()
            .iter()
            .filter(|pin| {
                pin.domain == PolicyDomain::Account(network.account.clone())
                    || pin.domain == PolicyDomain::Space(source.space.clone())
            })
            .cloned()
            .collect();
        let mut remote = Store::initialize(std::path::Path::new(":memory:"))?;
        let update = remote
            .policy
            .prepare_bootstrap(&remote_pins, &remote_journal, now())?;
        let tx = remote
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let state = journal::commit_in(&tx, &update)?;
        project_policy_in(&tx, &state)?;
        tx.commit()?;
        remote.policy = state;
        assert!(remote.policy.verified().space(&network.space).is_none());
        let prepared =
            prepare_execution_registration(&network, &source, local.policy.pins(), &bundle)
                .context("prepare remote Team source registration")?;
        eprintln!(
            "execution registration source=remote_team phase=prepare usage={}",
            prepared.proof_usage_json().map_err(anyhow::Error::msg)?
        );
        let params: Value =
            serde_json::from_str(&prepared.request_json().map_err(anyhow::Error::msg)?)?;
        let counts = row_counts(&local)?;
        let error = local
            .governance_command(
                &network.node,
                &network.client.public_key_hex(),
                "execution_service_register",
                &params,
                now(),
            )
            .unwrap_err();
        assert!(matches!(
            error.downcast_ref::<AuthorizationError>(),
            Some(AuthorizationError::NeedFreshness(_))
        ));
        let challenge = local.execution_service_registration_challenge(
            &network.node,
            &network.client.public_key_hex(),
            &params,
            now(),
        )?;
        let own_receipts = local.sign_current_receipts(&challenge, &network.node, now())?;
        let remote_receipts = remote.sign_current_receipts(&challenge, &remote_identity, now())?;
        assert_eq!(own_receipts.len(), 2);
        assert_eq!(remote_receipts.len(), 1);
        assert_eq!(
            remote_receipts[0].payload.current.domain,
            PolicyDomain::Space(source.space.clone())
        );
        let mut stale = challenge.clone();
        stale
            .policies
            .iter_mut()
            .find(|head| head.domain == PolicyDomain::Space(source.space.clone()))
            .context("source head missing")?
            .sequence += 1;
        assert!(
            remote
                .sign_current_receipts(&stale, &remote_identity, now())
                .is_err()
        );
        for receipts in [
            own_receipts.clone(),
            [
                own_receipts.clone(),
                remote_receipts.clone(),
                remote_receipts.clone(),
            ]
            .concat(),
        ] {
            assert!(
                local
                    .governance_command_with_registration_freshness(
                        &network.node,
                        &network.client.public_key_hex(),
                        "execution_service_register",
                        &params,
                        now(),
                        Some(&super::super::policy::FreshnessProof {
                            challenge: challenge.clone(),
                            receipts
                        })
                    )
                    .is_err()
            );
            assert_eq!(row_counts(&local)?, counts);
        }
        let proof = super::super::policy::FreshnessProof {
            challenge,
            receipts: [own_receipts, remote_receipts].concat(),
        };
        let verification = vibeface_secure::signature_verification::SignatureVerification::new();
        let result = verification.run(|| {
            local.governance_command_with_registration_freshness(
                &network.node,
                &network.client.public_key_hex(),
                "execution_service_register",
                &params,
                now(),
                Some(&proof),
            )
        });
        let actual = verification.counts();
        eprintln!(
            "execution registration source=remote_team phase=store_commit actual_verifications={} cache_hits={}",
            actual.actual_verifications, actual.cache_hits
        );
        let result = result
            .context("commit remote Team source registration with complete authority freshness")?;
        let verified = execution_service::finish_execution_service_registration(
            &prepared,
            &serde_json::to_string(&result["committed"])?,
            now() as f64,
        )
        .map_err(anyhow::Error::msg)
        .context("finish remote Team registered PersonalSpace proof")?;
        let execution = ResourceRef {
            space: SpaceId::parse(&verified.space_id())?,
            resource: ResourceId::parse(&verified.resource_id())?,
        };
        assert_eq!(source_root(&local.connection, &execution)?, network.project);
        assert!(remote.policy.verified().space(&network.space).is_none());
        let before_select = row_counts(&local)?;
        let selection = local.execution_service_select(
            &network.node,
            &network.client.public_key_hex(),
            &json!({"account_id":network.account,"device_public_key":network.client.public_key_hex(),
                "source_project":source,"target_node_public_key":network.node.public_key_hex()?,
                "execution_resource":execution}),
            now(),
        )?;
        let selection: execution_service::ExecutionServiceSelection =
            serde_json::from_value(selection["selections"][0].clone())?;
        execution_service::verify_execution_service_selection(
            &serde_json::to_string(&selection)?,
            &verified.pins_json().map_err(anyhow::Error::msg)?,
            &serde_json::to_string(&source)?,
            &serde_json::to_string(&execution)?,
            network.account.as_str(),
            &network.client.public_key_hex(),
            &network.node.public_key_hex()?,
            now() as f64,
        )
        .map_err(anyhow::Error::msg)?;
        assert_eq!(row_counts(&local)?, before_select);
        assert!(remote.policy.verified().space(&network.space).is_none());
        Ok(())
    }

    #[test]
    fn team_creation_is_atomic_and_retries_return_the_original_acceptance() -> Result<()> {
        let network = TestNetwork::new()?;
        let path = network.directory.join("team.sqlite3");
        let mut store = network.at(&path)?;
        let mut other = Store::open(&path)?;
        let prepared = prepare_team(&network)?;
        let request: Signed<TeamCreationRequest> =
            serde_json::from_str(&prepared.request_json().map_err(anyhow::Error::msg)?)?;
        let params = json!({"request":request});
        let before = row_counts(&store)?;
        store.connection.execute_batch("CREATE TEMP TRIGGER reject_team_receipt BEFORE INSERT ON identity_meta WHEN NEW.key LIKE 'team-create:%' BEGIN SELECT RAISE(ABORT,'injected receipt failure'); END;")?;
        let error = store
            .governance_command(
                &network.node,
                &network.client.public_key_hex(),
                "team_create",
                &params,
                now(),
            )
            .unwrap_err();
        assert!(
            error.to_string().contains("injected receipt failure"),
            "{error}"
        );
        assert_eq!(row_counts(&store)?, before);
        assert!(
            store
                .policy
                .verified()
                .space(&request.payload.team_project.space)
                .is_none()
        );
        store
            .connection
            .execute_batch("DROP TRIGGER reject_team_receipt")?;
        let alternate = network.directory.join("alternate-project");
        std::fs::create_dir(&alternate)?;
        let alternate = alternate.canonicalize()?;
        store.connection.execute_batch("CREATE TEMP TABLE mapping_swap(space_id TEXT,key TEXT,value TEXT); CREATE TEMP TRIGGER swap_source_mapping AFTER INSERT ON identity_meta WHEN NEW.key LIKE 'team-create:%' BEGIN UPDATE local_values SET value=(SELECT value FROM mapping_swap) WHERE space_id=(SELECT space_id FROM mapping_swap) AND key=(SELECT key FROM mapping_swap); END;")?;
        store.connection.execute(
            "INSERT INTO mapping_swap VALUES(?,?,?)",
            params![
                network.space.as_str(),
                format!("resource-root:{}", network.resource),
                serde_json::to_string(&alternate)?
            ],
        )?;
        let error = store
            .governance_command(
                &network.node,
                &network.client.public_key_hex(),
                "team_create",
                &params,
                now(),
            )
            .unwrap_err();
        assert!(
            error.to_string().contains("source project mapping changed"),
            "{error}"
        );
        assert_eq!(row_counts(&store)?, before);
        assert_eq!(
            store.resource_root(network.space.as_str(), &network.resource)?,
            network.project
        );
        assert!(
            journal::PolicyState::load(&store.connection)?
                .verified()
                .space(&request.payload.team_project.space)
                .is_none()
        );
        store
            .connection
            .execute_batch("DROP TRIGGER swap_source_mapping; DROP TABLE mapping_swap;")?;
        let response = store.governance_command(
            &network.node,
            &network.client.public_key_hex(),
            "team_create",
            &params,
            now(),
        )?;
        let after = row_counts(&store)?;
        assert_eq!(
            store.resource_root(
                request.payload.team_project.space.as_str(),
                &request.payload.team_project.resource
            )?,
            network.project
        );
        assert_eq!(
            other.governance_command(
                &network.node,
                &network.client.public_key_hex(),
                "team_create",
                &params,
                now() + 200_000
            )?,
            response
        );
        assert_eq!(row_counts(&other)?, after);
        assert!(
            other
                .policy
                .verified()
                .space(&request.payload.team_project.space)
                .is_some()
        );
        let accepted: TeamCreateResponse = serde_json::from_value(response)?;
        governance::finish_team_creation(
            &prepared,
            &serde_json::to_string(&accepted.acceptance)?,
            now() as f64,
        )
        .map_err(anyhow::Error::msg)?;
        let mut conflict = request.payload.clone();
        conflict.expires_at_ms -= 1;
        let conflict = Signed::sign(conflict, &network.client)?;
        assert!(
            store
                .governance_command(
                    &network.node,
                    &network.client.public_key_hex(),
                    "team_create",
                    &json!({"request":conflict}),
                    now()
                )
                .is_err()
        );
        drop(store);
        drop(other);
        let reopened = Store::open(&path)?;
        assert_eq!(
            reopened.resource_root(
                request.payload.team_project.space.as_str(),
                &request.payload.team_project.resource
            )?,
            network.project
        );
        Ok(())
    }

    #[test]
    fn team_creation_rejects_wrong_peer_node_locator_and_changed_source_root() -> Result<()> {
        let network = TestNetwork::new()?;
        let mut store = network.memory()?;
        let prepared = prepare_team(&network)?;
        let request: Signed<TeamCreationRequest> =
            serde_json::from_str(&prepared.request_json().map_err(anyhow::Error::msg)?)?;
        let params = json!({"request":request});
        let before = row_counts(&store)?;
        assert!(
            store
                .governance_command(
                    &network.node,
                    &network.root.public_key_hex(),
                    "team_create",
                    &params,
                    now()
                )
                .is_err()
        );
        assert!(
            store
                .governance_command(
                    &LocalIdentity::generate(),
                    &network.client.public_key_hex(),
                    "team_create",
                    &params,
                    now()
                )
                .is_err()
        );
        assert!(
            store
                .governance_command(
                    &network.node,
                    &network.client.public_key_hex(),
                    "team_create",
                    &json!({"request":request, "root":network.project}),
                    now()
                )
                .is_err()
        );
        assert_eq!(row_counts(&store)?, before);
        let alias = network.directory.join("alias");
        std::os::unix::fs::symlink(&network.project, &alias)?;
        store.local_set(
            network.space.as_str(),
            &format!("resource-root:{}", network.resource),
            &json!(alias),
        )?;
        assert!(
            store
                .governance_command(
                    &network.node,
                    &network.client.public_key_hex(),
                    "team_create",
                    &params,
                    now()
                )
                .is_err()
        );
        assert_eq!(row_counts(&store)?, before);
        store.local_set(
            network.space.as_str(),
            &format!("resource-root:{}", network.resource),
            &json!(network.project),
        )?;
        network.revoke_client(&mut store)?;
        let revoked = row_counts(&store)?;
        assert!(
            store
                .governance_command(
                    &network.node,
                    &network.client.public_key_hex(),
                    "team_create",
                    &params,
                    now() + 1
                )
                .is_err()
        );
        assert_eq!(row_counts(&store)?, revoked);
        Ok(())
    }

    #[test]
    fn team_governance_persists_membership_grant_resource_and_revocation() -> Result<()> {
        let network = TestNetwork::new()?;
        let path = network.directory.join("governance.sqlite3");
        let mut store = network.at(&path)?;
        let mut team = create_team(&network, &mut store)?;
        let guest_root = Identity::generate().map_err(anyhow::Error::msg)?;
        let guest = AccountId::from_root(&PublicKey::of(&guest_root));
        let membership = SpaceMembershipIntent {
            space: team.space.clone(),
            subject_account: guest.clone(),
            roles: vec![SpaceRole::Viewer],
            expires_at_ms: now() + 80_000,
        };
        let prepared = governance::prepare_space_membership_change(
            &serde_json::to_string(&membership)?,
            &network.client,
            &team.pins_json()?,
            &team.bundle_json()?,
            now() as f64,
        )
        .map_err(anyhow::Error::msg)?;
        team.commit(&network, &mut store, &prepared)?;
        assert!(
            store
                .policy
                .verified()
                .space(&team.space)
                .unwrap()
                .memberships
                .iter()
                .any(|hash| store
                    .policy
                    .verified()
                    .membership(hash)
                    .is_some_and(|member| member.payload.subject == Actor::Account(guest.clone())))
        );
        let grant = DirectionalGrantIntent {
            space: team.space.clone(),
            grantee: Grantee::Actor(Actor::Account(network.account.clone())),
            scope: ResourceScope::Exact(ResourceRef {
                space: team.space.clone(),
                resource: team.resource.clone(),
            }),
            actions: vec![Action::ConversationRead],
            audience_node_public_key: PublicKey::parse(&network.node.public_key_hex()?)?,
            constraints: GrantConstraints::default(),
            remaining_delegations: 1,
            expires_at_ms: now() + 80_000,
        };
        let prepared = governance::prepare_directional_grant(
            &serde_json::to_string(&grant)?,
            &network.client,
            &team.pins_json()?,
            &team.bundle_json()?,
            now() as f64,
        )
        .map_err(anyhow::Error::msg)?;
        let response = team.commit(&network, &mut store, &prepared)?;
        let child = response
            .journal_delta
            .iter()
            .find_map(|entry| match entry {
                JournalEntry::Grant(value) => Some(value.hash()),
                _ => None,
            })
            .context("grant missing")??;
        let mut child_bundle = team.bundle.clone();
        child_bundle.grant_leaf = child.clone();
        vibeface_secure::authorization::verify_client_authorization(
            &serde_json::to_string(&child_bundle)?,
            &team.pins_json()?,
            network.account.as_str(),
            team.space.as_str(),
            &network.client.public_key_hex(),
            now() as f64,
        )
        .map_err(anyhow::Error::msg)?;
        let resource = SpaceResourceIntent {
            space: team.space.clone(),
            resource: ResourceId::random()?,
            kind: ResourceKind::Project,
            host_node_public_key: PublicKey::parse(&network.node.public_key_hex()?)?,
        };
        let prepared = governance::prepare_space_resource_registration(
            &serde_json::to_string(&resource)?,
            &network.client,
            &team.pins_json()?,
            &team.bundle_json()?,
            now() as f64,
        )
        .map_err(anyhow::Error::msg)?;
        team.commit(&network, &mut store, &prepared)?;
        assert!(
            store
                .policy
                .verified()
                .resource(&team.space, &resource.resource)
                .is_some()
        );
        let prepared = governance::prepare_space_grant_revocation(
            team.space.as_str(),
            child.as_str(),
            &network.client,
            &team.pins_json()?,
            &team.bundle_json()?,
            now() as f64,
        )
        .map_err(anyhow::Error::msg)?;
        team.commit(&network, &mut store, &prepared)?;
        assert!(
            !store
                .policy
                .verified()
                .space(&team.space)
                .unwrap()
                .grants
                .contains(&child)
        );
        child_bundle = team.bundle.clone();
        child_bundle.grant_leaf = child.clone();
        assert!(
            vibeface_secure::authorization::verify_client_authorization(
                &serde_json::to_string(&child_bundle)?,
                &team.pins_json()?,
                network.account.as_str(),
                team.space.as_str(),
                &network.client.public_key_hex(),
                now() as f64,
            )
            .is_err()
        );
        let removal = SpaceMembershipIntent {
            roles: vec![],
            ..membership
        };
        let prepared = governance::prepare_space_membership_change(
            &serde_json::to_string(&removal)?,
            &network.client,
            &team.pins_json()?,
            &team.bundle_json()?,
            now() as f64,
        )
        .map_err(anyhow::Error::msg)?;
        team.commit(&network, &mut store, &prepared)?;
        assert!(
            !store
                .policy
                .verified()
                .space(&team.space)
                .unwrap()
                .memberships
                .iter()
                .any(|hash| store
                    .policy
                    .verified()
                    .membership(hash)
                    .is_some_and(|member| member.payload.subject == Actor::Account(guest.clone())))
        );
        drop(store);
        let mut reopened = Store::open(&path)?;
        let current = reopened
            .policy
            .verified()
            .space(&team.space)
            .context("Team missing after reopen")?;
        assert!(!current.grants.contains(&child));
        assert!(!current.memberships.iter().any(|hash| {
            reopened
                .policy
                .verified()
                .membership(hash)
                .is_some_and(|member| member.payload.subject == Actor::Account(guest.clone()))
        }));
        assert!(
            reopened
                .policy
                .verified()
                .resource(&team.space, &resource.resource)
                .is_some()
        );
        assert_eq!(
            reopened.resource_root(team.space.as_str(), &team.resource)?,
            network.project
        );
        network.revoke_node(&mut reopened)?;
        assert!(
            reopened
                .policy
                .verified()
                .verify_service_device(
                    &team.space,
                    &DeviceId::from_key(&PublicKey::parse(&network.node.public_key_hex()?)?),
                    now() + 1
                )
                .is_err()
        );
        Ok(())
    }

    #[test]
    fn daily_governance_rolls_back_and_never_signs_remote_authority_receipts() -> Result<()> {
        let network = TestNetwork::new()?;
        let mut store = network.memory()?;
        let team = create_team(&network, &mut store)?;
        let resource = SpaceResourceIntent {
            space: team.space.clone(),
            resource: ResourceId::random()?,
            kind: ResourceKind::Project,
            host_node_public_key: PublicKey::parse(&network.node.public_key_hex()?)?,
        };
        let prepared = governance::prepare_space_resource_registration(
            &serde_json::to_string(&resource)?,
            &network.client,
            &team.pins_json()?,
            &team.bundle_json()?,
            now() as f64,
        )
        .map_err(anyhow::Error::msg)?;
        let request: SpaceGovernanceRequest =
            serde_json::from_str(&prepared.request_json().map_err(anyhow::Error::msg)?)?;
        let params = json!({"request":request});
        let before = row_counts(&store)?;
        let before_head = store
            .policy
            .verified()
            .head(&PolicyDomain::Space(team.space.clone()))
            .cloned();
        store.connection.execute_batch("CREATE TEMP TRIGGER reject_governance_receipt BEFORE INSERT ON identity_meta WHEN NEW.key LIKE 'space-governance:%' BEGIN SELECT RAISE(ABORT,'injected receipt failure'); END;")?;
        let error = store
            .governance_command(
                &network.node,
                &network.client.public_key_hex(),
                "space_policy_commit",
                &params,
                now(),
            )
            .unwrap_err();
        assert!(
            error.to_string().contains("injected receipt failure"),
            "{error}"
        );
        assert_eq!(row_counts(&store)?, before);
        assert_eq!(
            journal::PolicyState::load(&store.connection)?
                .verified()
                .head(&PolicyDomain::Space(team.space.clone()))
                .cloned(),
            before_head
        );
        assert!(
            store
                .policy
                .verified()
                .resource(&team.space, &resource.resource)
                .is_none()
        );
        store
            .connection
            .execute_batch("DROP TRIGGER reject_governance_receipt")?;
        assert!(
            store
                .governance_command(
                    &LocalIdentity::generate(),
                    &network.client.public_key_hex(),
                    "space_policy_commit",
                    &params,
                    now()
                )
                .is_err()
        );
        assert_eq!(row_counts(&store)?, before);
        let replacement = Identity::generate().map_err(anyhow::Error::msg)?;
        let mut account = store
            .policy
            .verified()
            .account(&network.account)
            .unwrap()
            .clone();
        account.previous = store
            .policy
            .verified()
            .head(&PolicyDomain::Account(network.account.clone()))
            .cloned();
        account.authority = AuthorityBinding::from_key(PublicKey::of(&replacement));
        account.authority_epoch += 1;
        account.sequence = 0;
        store.install_policy_journal(
            &[JournalEntry::Account(Signed::sign(account, &network.root)?)],
            now(),
        )?;
        let before = row_counts(&store)?;
        let error = store
            .governance_command(
                &network.node,
                &network.client.public_key_hex(),
                "space_policy_commit",
                &params,
                now(),
            )
            .unwrap_err();
        assert!(error.to_string().contains("remote authority"), "{error}");
        assert_eq!(row_counts(&store)?, before);
        assert!(
            store
                .policy
                .verified()
                .resource(&team.space, &resource.resource)
                .is_none()
        );
        Ok(())
    }
}
