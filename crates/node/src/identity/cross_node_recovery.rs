mod target_import;

pub(crate) use target_import::TargetRecoveryImport;

use super::{
    bounded_recovery_context::{
        completed_recovery_read_in, recovery_read_reference_in, store_source_proposal_approval_in,
    },
    journal,
    policy::project_policy_in,
};
use crate::{crypto::LocalIdentity, store::Store};
use anyhow::{Context, Result, bail, ensure};
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use vibeface_secure::{
    Identity,
    authorization::{
        AUTH_VERSION, DeviceId, JournalEntry, Nonce, ObjectHash, PolicyDomain, PublicKey,
        Signature, Signed,
        bootstrap::{
            NodeClaim,
            recovery::cross_node::{self, CrossNodeRecoveryRequest},
            recovery::stream::{self, *},
        },
    },
    signature_verification::ScopedSignatureVerification,
};

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SourcePossession {
    request: StreamedCrossNodeRecoveryPossessionRequest,
    proof: Signature,
}

fn metadata(connection: &Connection, key: &str) -> Result<Option<String>> {
    let row: Option<(usize, Option<String>)> = connection
        .query_row(
            "SELECT length(CAST(value AS BLOB)),CASE WHEN length(CAST(value AS BLOB))<=65536 THEN value END FROM identity_meta WHERE key=?",
            [key],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    let Some((length, value)) = row else {
        return Ok(None);
    };
    ensure!(length <= 65_536, "stored recovery record is too large");
    Ok(Some(value.context("stored recovery record is missing")?))
}

fn possession_key(snapshot: &Nonce) -> String {
    format!("cross-node-recovery-possession:{snapshot}")
}

fn completion_key(snapshot: &Nonce) -> String {
    format!("cross-node-recovery-completed:{snapshot}")
}

fn bounded(value: Value) -> Result<Value> {
    ensure!(
        serde_json::to_vec(&value)?.len() <= 65_536,
        "recovery carrier exceeds 64 KiB"
    );
    Ok(value)
}

fn carrier_bytes(input: &Value, output: &Value) -> Result<usize> {
    Ok(serde_json::to_vec(input)?.len() + serde_json::to_vec(output)?.len())
}

fn completed_in(
    connection: &Connection,
    local: &PublicKey,
    principal: &str,
    request: &RecoveryCompletionStatusRequest,
) -> Result<Option<StreamedCrossNodeRecoveryCommitted>> {
    let Some(encoded) = metadata(connection, &completion_key(&request.context.snapshot_id))? else {
        return Ok(None);
    };
    let committed: StreamedCrossNodeRecoveryCommitted = serde_json::from_str(&encoded)?;
    let receipt = &committed.receipt;
    let original = &committed.request.possession.request;
    ensure!(
        receipt.issuer == *local
            && receipt.payload.version == AUTH_VERSION
            && receipt.payload.context == request.context
            && receipt.payload.input_hash == request.input_hash
            && receipt.payload.input_hash == ObjectHash::body(&committed.request)?
            && receipt.payload.request_hash == original.hash()?
            && receipt.payload.request_hash == request.context.request_hash
            && receipt.payload.new_device_public_key.as_str() == principal
            && original.payload.new_device_public_key.as_str() == principal
            && original.payload.source_node_public_key == *local,
        "cross-node completion changed its original input or Noise device"
    );
    receipt.verify_signature()?;
    Ok(Some(committed))
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

impl Store {
    pub(crate) fn cross_node_recovery_command(
        &mut self,
        identity: &LocalIdentity,
        principal: &str,
        method: &str,
        params: &Value,
        now: u64,
    ) -> Result<Value> {
        let _verification = ScopedSignatureVerification::new();
        bounded(params.clone())?;
        PublicKey::parse(principal)?;
        match method {
            "cross_node_recovery_context" => {
                self.cross_node_recovery_context(identity, principal, params, now)
            }
            "cross_node_recovery_context_page" => {
                self.recovery_context_page(identity, principal, params, now)
            }
            "cross_node_recovery_context_witness" => {
                self.recovery_context_witness(identity, principal, params, now)
            }
            "cross_node_recovery_status" => {
                self.cross_node_recovery_status(identity, principal, params)
            }
            "cross_node_recovery_import_begin" => {
                self.cross_node_recovery_import_begin(identity, principal, params, now)
            }
            "cross_node_recovery_import_page" => {
                self.cross_node_recovery_import_page(identity, principal, params, now, false)
            }
            "cross_node_recovery_import_cached_page" => {
                self.cross_node_recovery_import_page(identity, principal, params, now, true)
            }
            "cross_node_recovery_import_finish" => {
                self.cross_node_recovery_import_finish(identity, principal, params, now)
            }
            "cross_node_recovery_import_resume" => {
                self.cross_node_recovery_import_resume(identity, principal, params, now)
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
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct BeginRequest {
            request: Signed<CrossNodeRecoveryRequest>,
        }
        let request: BeginRequest = serde_json::from_value(params.clone())?;
        self.begin_recovery_read(
            identity,
            principal,
            vibeface_secure::authorization::bootstrap::recovery::stream::RecoveryReadRequest::CrossNode(request.request),
            now,
        )
    }

    fn cross_node_recovery_source_possession(
        &mut self,
        identity: &LocalIdentity,
        principal: &str,
        params: &Value,
        now: u64,
    ) -> Result<Value> {
        let request: StreamedCrossNodeRecoveryPossessionRequest =
            serde_json::from_value(params.clone())?;
        require_principal(&request.request, principal)?;
        let signer =
            Identity::from_secret_hex(&identity.secret_key_hex).map_err(anyhow::Error::msg)?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let context = completed_recovery_read_in(
            &tx,
            &self.policy,
            identity,
            principal,
            &request.context,
            now,
        )?;
        ensure!(
            context.request == RecoveryReadRequest::CrossNode(request.request.clone()),
            "cross-node possession changed its source request"
        );
        let local = PublicKey::of(&signer);
        let prefix = context.accepted_prefix_commitment_in(&tx, &self.policy)?;
        let key = possession_key(&request.context.snapshot_id);
        let previous = metadata(&tx, &key)?;
        let (proof, approval) = if let Some(previous) = &previous {
            let previous: SourcePossession = serde_json::from_str(previous)?;
            ensure!(
                previous.request == request,
                "source recovery already approved another proposal"
            );
            let approval = context
                .source_approval
                .as_ref()
                .context("source proposal approval is missing")?;
            stream::verify_recovery_source_proposal_approval(
                approval,
                &context.request,
                &request.context,
                &ObjectHash::body(&request)?,
                &request.node_request.account_recovery,
                &previous.proof,
                self.policy.verified(),
                &local,
                &prefix,
                now,
            )?;
            (previous.proof, approval.clone())
        } else {
            ensure!(
                context.source_approval.is_none(),
                "source proposal possession record is missing"
            );
            stream::approve_cross_node_recovery_source_proposal(
                &request,
                &context.request,
                &context.snapshot,
                &context.witness,
                self.policy.verified(),
                &signer,
                &prefix,
                now,
            )?
        };
        let response = bounded(json!({"proof":proof}))?;
        let mut usage = RecoveryProofUsage::new(carrier_bytes(params, &response)?)?;
        context.record_current_usage(self.policy.verified(), &mut usage)?;
        usage.record_cross_possession(&request, self.policy.verified())?;
        usage.record_source_proposal_approval(&approval)?;
        usage.finish()?;
        store_source_proposal_approval_in(&tx, &context, &approval, &prefix)?;
        if previous.is_none() {
            let encoded = serde_json::to_string(&SourcePossession {
                request: request.clone(),
                proof: proof.clone(),
            })?;
            ensure!(encoded.len() <= 65_536, "source possession is too large");
            tx.execute(
                "INSERT INTO identity_meta(key,value) VALUES(?,?)",
                params![key, encoded],
            )?;
        }
        tx.commit()?;
        Ok(response)
    }

    fn cross_node_recovery_status(
        &mut self,
        identity: &LocalIdentity,
        principal: &str,
        params: &Value,
    ) -> Result<Value> {
        let request: RecoveryCompletionStatusRequest = serde_json::from_value(params.clone())?;
        let local = PublicKey::parse(&identity.public_key_hex()?)?;
        let tx = self.connection.transaction()?;
        if let Some(committed) = completed_in(&tx, &local, principal, &request)? {
            let response = bounded(json!({"state":"committed","committed":committed}))?;
            let mut usage = RecoveryProofUsage::new(carrier_bytes(params, &response)?)?;
            usage.record_cross_committed_carrier(&committed)?;
            usage.finish()?;
            return Ok(response);
        }
        let context = recovery_read_reference_in(&tx, identity, principal, &request.context)?;
        ensure!(
            matches!(context.request, RecoveryReadRequest::CrossNode(_)),
            "cross-node status belongs to another recovery purpose"
        );
        let response = json!({"state":"not_committed"});
        let mut usage = RecoveryProofUsage::new(carrier_bytes(params, &response)?)?;
        context.record_reference_usage(&mut usage)?;
        usage.finish()?;
        Ok(response)
    }

    fn cross_node_recovery_commit(
        &mut self,
        identity: &LocalIdentity,
        principal: &str,
        params: &Value,
        now: u64,
    ) -> Result<Value> {
        let input: StreamedCrossNodeRecoveryCommitRequest = serde_json::from_value(params.clone())?;
        require_principal(&input.possession.request, principal)?;
        let signer =
            Identity::from_secret_hex(&identity.secret_key_hex).map_err(anyhow::Error::msg)?;
        let local = PublicKey::of(&signer);
        let status = RecoveryCompletionStatusRequest {
            context: input.possession.context.clone(),
            input_hash: ObjectHash::body(&input)?,
        };
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let Some(committed) = completed_in(&tx, &local, principal, &status)? {
            let response = bounded(json!({"committed":committed}))?;
            let mut usage = RecoveryProofUsage::new(carrier_bytes(params, &response)?)?;
            usage.record_cross_committed_carrier(&committed)?;
            usage.finish()?;
            return Ok(response);
        }
        let context = completed_recovery_read_in(
            &tx,
            &self.policy,
            identity,
            principal,
            &input.possession.context,
            now,
        )?;
        ensure!(
            context.request == RecoveryReadRequest::CrossNode(input.possession.request.clone()),
            "cross-node commit changed its source request"
        );
        let approved: SourcePossession = serde_json::from_str(
            &metadata(&tx, &possession_key(&input.possession.context.snapshot_id))?
                .context("source Node possession is missing")?,
        )?;
        ensure!(
            approved.request == input.possession
                && input.journal_delta.iter().any(|entry| matches!(
                    entry,
                    JournalEntry::Device(certificate)
                        if certificate.payload.public_key == local
                            && certificate.payload.possession.as_ref() == Some(&approved.proof)
                )),
            "cross-node commit changed its approved source possession"
        );
        let prefix = context.accepted_prefix_commitment_in(&tx, &self.policy)?;
        let source_approval = stream::verify_recovery_source_proposal_approval(
            context
                .source_approval
                .as_ref()
                .context("source proposal approval is missing")?,
            &context.request,
            &input.possession.context,
            &ObjectHash::body(&approved.request)?,
            &approved.request.node_request.account_recovery,
            &approved.proof,
            self.policy.verified(),
            &local,
            &prefix,
            now,
        )?;
        let verified = stream::verify_cross_node_recovery_stream_commit_with_source_approval(
            &input,
            &context.request,
            &context.snapshot,
            &context.witness,
            self.policy.verified(),
            &source_approval,
            &local,
            now,
        )?;
        ensure!(
            verified.input_hash() == &status.input_hash,
            "cross-node commit changed its exact input hash"
        );
        let committed_heads = verified.committed_heads().to_vec();
        let committed = stream::accept_cross_node_recovery_stream_commit(&verified, &signer, now)?;
        let response = bounded(json!({"committed":committed}))?;
        let mut usage = RecoveryProofUsage::new(carrier_bytes(params, &response)?)?;
        context.record_current_usage(self.policy.verified(), &mut usage)?;
        verified.record_proof_usage_into(&mut usage);
        usage.record_cross_completion(&committed.receipt)?;
        usage.finish()?;
        let update = self.policy.prepare_cross_recovery_commit(verified, now)?;
        ensure!(
            committed_heads.len() == 2
                && committed_heads
                    .iter()
                    .all(|head| update.verified_after().head(&head.domain) == Some(head)),
            "cross-node update differs from its verified completion heads"
        );
        let state = journal::commit_in(&tx, &update)?;
        project_policy_in(&tx, &state)?;
        let encoded = serde_json::to_string(&committed)?;
        ensure!(
            encoded.len() <= 65_536,
            "cross-node completion is too large"
        );
        tx.execute(
            "INSERT INTO identity_meta(key,value) VALUES(?,?)",
            params![completion_key(&status.context.snapshot_id), encoded],
        )?;
        tx.commit()?;
        self.policy = state;
        Ok(response)
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::identity::{
        local::BootstrapCapability,
        policy::FreshnessProof,
        test_support::{TestNetwork, now},
    };
    use vibeface_secure::authorization::{
        AUTH_VERSION, Actor, Audience, CheckStage, Invocation, Nonce, ResourceRef, ServiceId,
        TransportEvidence,
        bootstrap::{self, VerifiedNodeClaim, recovery},
        client_policy_import::AuthorizationSelection,
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
        snapshot: Option<Signed<RecoveryContextSnapshot>>,
        witness: Option<Signed<RecoveryContextWitness>>,
        reference: Option<RecoveryContextReference>,
        source_read_grant: Option<ObjectHash>,
    }

    impl Fixture {
        fn new() -> Result<Self> {
            let network = TestNetwork::new()?;
            let mut source = network.at(&network.directory.join("source.sqlite3"))?;
            network.conversation(&mut source, "archive")?;
            let original_event = network.message(&mut source, "archive", "preserved")?;
            Self::from_source(network, source, original_event)
        }

        fn from_source(
            network: TestNetwork,
            source: Store,
            original_event: crate::model::SignedEvent,
        ) -> Result<Self> {
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
                snapshot: None,
                witness: None,
                reference: None,
                source_read_grant: None,
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

        fn imported(&mut self) -> Result<VerifiedRecoveryPolicyContext> {
            let principal = self.next_client.public_key_hex();
            let response = self.source.cross_node_recovery_command(
                &self.network.node,
                &principal,
                "cross_node_recovery_context",
                &json!({"request":self.request}),
                now(),
            )?;
            let snapshot: Signed<RecoveryContextSnapshot> =
                serde_json::from_value(response["snapshot"].clone())?;
            let mut importer = stream::begin_cross_node_recovery_policy_import(
                &serde_json::to_string(&self.request)?,
                &serde_json::to_string(&snapshot)?,
                &self.target_node.public_key_hex()?,
                &self.capability.capability,
                now(),
            )?;
            let binding = self.target.cross_node_recovery_command(
                &self.target_node,
                &principal,
                "cross_node_recovery_import_begin",
                &json!({"request":self.request,"snapshot":snapshot}),
                now(),
            )?;
            assert_eq!(binding["snapshot_id"], json!(snapshot.payload.snapshot_id));
            assert_eq!(
                binding["request_hash"],
                json!(snapshot.payload.request_hash)
            );
            assert_eq!(binding["snapshot_hash"], json!(snapshot.hash()?));
            let mut cursor = None;
            loop {
                let response = self.source.cross_node_recovery_command(
                    &self.network.node, &principal, "cross_node_recovery_context_page",
                    &json!({"snapshot_id":snapshot.payload.snapshot_id,"request_hash":snapshot.payload.request_hash,"cursor":cursor}), now(),
                )?;
                let page: Signed<RecoveryContextPage> =
                    serde_json::from_value(response["page"].clone())?;
                importer.append(page.clone(), now())?;
                let imported = self.target.cross_node_recovery_command(
                    &self.target_node,
                    &principal,
                    "cross_node_recovery_import_page",
                    &json!({
                        "snapshot_id":binding["snapshot_id"],
                        "request_hash":binding["request_hash"],
                        "snapshot_hash":binding["snapshot_hash"],
                        "page":page,
                    }),
                    now(),
                )?;
                assert_eq!(imported["page_count"], page.payload.page_index + 1);
                assert_eq!(imported["complete"], page.payload.complete);
                cursor = page.payload.next_cursor.clone();
                let complete = page.payload.complete;
                if complete {
                    break;
                }
            }
            let response = self.source.cross_node_recovery_command(
                &self.network.node, &principal, "cross_node_recovery_context_witness",
                &json!({"snapshot_id":snapshot.payload.snapshot_id,"request_hash":snapshot.payload.request_hash,"cursor":null}), now(),
            )?;
            let witness: Signed<RecoveryContextWitness> =
                serde_json::from_value(response["witness"].clone())?;
            let context = importer.finish_native(witness.clone(), now())?;
            let reference = context.reference()?;
            self.target.cross_node_recovery_command(
                &self.target_node,
                &principal,
                "cross_node_recovery_import_finish",
                &json!({"context":reference,"witness":witness}),
                now(),
            )?;
            self.snapshot = Some(snapshot);
            self.witness = Some(witness);
            self.reference = Some(reference);
            Ok(context)
        }

        fn prepared(&mut self) -> Result<cross_node::PreparedCrossNodeAccountRecovery> {
            let context = self.imported()?;
            cross_node::prepare_cross_node_account_recovery(
                &self.material,
                &context,
                &self.next_root,
                &self.next_client,
                (now() + 1_000_000) as f64,
                now() as f64,
            )
            .map_err(anyhow::Error::msg)
        }

        fn prepare(&mut self) -> Result<Value> {
            let prepared = self.prepared()?;
            Ok(serde_json::from_str(
                &prepared
                    .possession_request_json()
                    .map_err(anyhow::Error::msg)?,
            )?)
        }

        fn commit_request(
            &mut self,
        ) -> Result<(cross_node::PreparedCrossNodeRecoveryCommit, Value)> {
            let prepared = self.prepared()?;
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
            let retry = self.target.cross_node_recovery_command(
                &self.target_node,
                &self.next_client.public_key_hex(),
                "cross_node_recovery_possession",
                &request,
                now() + 200_000,
            )?;
            assert_eq!(retry, target);
            let commit = cross_node::finish_cross_node_account_recovery(
                &prepared,
                &self.next_root,
                target["proof"].as_str().context("target proof missing")?,
                source["proof"].as_str().context("source proof missing")?,
                now() as f64,
            )
            .map_err(anyhow::Error::msg)?;
            let request: Value =
                serde_json::from_str(&commit.request_json().map_err(anyhow::Error::msg)?)?;
            let typed: StreamedCrossNodeRecoveryCommitRequest =
                serde_json::from_value(request.clone())?;
            let source = PublicKey::parse(&self.network.node.public_key_hex()?)?;
            self.source_read_grant = typed
                .journal_delta
                .iter()
                .find_map(|entry| match entry {
                    JournalEntry::Grant(grant)
                        if grant.payload.audience.device == DeviceId::from_key(&source) =>
                    {
                        Some(grant.hash())
                    }
                    _ => None,
                })
                .transpose()?;
            Ok((commit, request))
        }

        fn resume_target(&mut self, committed: &Value, at: u64) -> Result<()> {
            let principal = self.next_client.public_key_hex();
            let snapshot = self.snapshot.as_ref().context("snapshot missing")?;
            let reference = self.reference.as_ref().context("reference missing")?;
            let witness = self.witness.as_ref().context("witness missing")?;
            self.target.cross_node_recovery_command(
                &self.target_node,
                &principal,
                "cross_node_recovery_import_resume",
                &json!({"request":self.request,"snapshot":snapshot,"committed":committed}),
                at,
            )?;
            for _ in 0..witness.payload.page_count {
                self.target.cross_node_recovery_command(
                    &self.target_node,
                    &principal,
                    "cross_node_recovery_import_cached_page",
                    &json!({"context":reference}),
                    at,
                )?;
            }
            self.target.cross_node_recovery_command(
                &self.target_node,
                &principal,
                "cross_node_recovery_import_finish",
                &json!({"context":reference,"witness":witness}),
                at,
            )?;
            Ok(())
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
            &mut self,
            method: &str,
        ) -> Result<(
            Signed<Invocation>,
            vibeface_secure::authorization::RequestFacts,
            TransportEvidence,
        )> {
            let source_key = PublicKey::parse(&self.network.node.public_key_hex()?)?;
            let audience = Audience {
                service: ServiceId::from_key(&source_key),
                device: DeviceId::from_key(&source_key),
            };
            let grant_leaf = self
                .source_read_grant
                .as_ref()
                .context("source read grant missing")?
                .clone();
            let selection = AuthorizationSelection {
                resource: ResourceRef {
                    space: self.network.space.clone(),
                    resource: self.network.resource.clone(),
                },
                audience: audience.clone(),
                grant_leaf,
            };
            let authorization =
                crate::identity::client_policy_read::verified_client_policy_for_test(
                    &mut self.source,
                    &self.network.node,
                    &self.next_client,
                    &self.network.account,
                    selection,
                    &self.request.payload.saved_pins,
                    now(),
                )?;
            let original: crate::model::Event =
                serde_json::from_str(&self.original_event.canonical_body)?;
            let mut body = self.network.body(method, &original.scope_id);
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
                    grant_leaf: authorization.grant_leaf().clone(),
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

    pub(crate) fn recover_existing_source(
        network: TestNetwork,
        source: Store,
        historical_event: crate::model::SignedEvent,
    ) -> Result<(TestNetwork, Store, Store)> {
        let mut fixture = Fixture::from_source(network, source, historical_event)?;
        let (_, request) = fixture.commit_request()?;
        let response = fixture.source_commit(&request, now())?;
        fixture.install(&response["committed"], now())?;
        let (read, facts, transport) = fixture.read_invocation("conversation")?;
        fixture
            .source
            .authorize_request(&read, &facts, &transport, now())?;
        let (write, facts, transport) = fixture.read_invocation("message")?;
        ensure!(
            fixture
                .source
                .authorize_request(&write, &facts, &transport, now())
                .is_err(),
            "source read grant authorized a fresh message"
        );
        Ok((fixture.network, fixture.source, fixture.target))
    }

    fn journal_count(store: &Store) -> Result<u64> {
        Ok(store
            .connection
            .query_row("SELECT count(*) FROM policy_journal", [], |row| row.get(0))?)
    }

    #[test]
    fn target_accepts_each_bound_page_before_the_source_can_issue_a_witness() -> Result<()> {
        let mut f = Fixture::new()?;
        let principal = f.next_client.public_key_hex();
        let response = f.source.cross_node_recovery_command(
            &f.network.node,
            &principal,
            "cross_node_recovery_context",
            &json!({"request":f.request}),
            now(),
        )?;
        let snapshot: Signed<RecoveryContextSnapshot> =
            serde_json::from_value(response["snapshot"].clone())?;
        let binding = f.target.cross_node_recovery_command(
            &f.target_node,
            &principal,
            "cross_node_recovery_import_begin",
            &json!({"request":f.request,"snapshot":snapshot}),
            now(),
        )?;
        let page = f.source.cross_node_recovery_command(
            &f.network.node,
            &principal,
            "cross_node_recovery_context_page",
            &json!({"snapshot_id":snapshot.payload.snapshot_id,"request_hash":snapshot.payload.request_hash,"cursor":null}),
            now(),
        )?;
        assert_eq!(page["page"]["payload"]["complete"], false);
        assert!(
            f.source
                .cross_node_recovery_command(
                    &f.network.node,
                    &principal,
                    "cross_node_recovery_context_witness",
                    &json!({"snapshot_id":snapshot.payload.snapshot_id,"request_hash":snapshot.payload.request_hash,"cursor":null}),
                    now(),
                )
                .is_err()
        );
        let input = json!({
            "snapshot_id":binding["snapshot_id"],
            "request_hash":binding["request_hash"],
            "snapshot_hash":binding["snapshot_hash"],
            "page":page["page"],
        });
        for field in ["snapshot_id", "request_hash", "snapshot_hash"] {
            let mut wrong = input.clone();
            wrong[field] = if field == "snapshot_id" {
                json!(Nonce::random()?)
            } else {
                json!(ObjectHash::body(&field)?)
            };
            assert!(
                f.target
                    .cross_node_recovery_command(
                        &f.target_node,
                        &principal,
                        "cross_node_recovery_import_page",
                        &wrong,
                        now(),
                    )
                    .is_err()
            );
        }
        let accepted = f.target.cross_node_recovery_command(
            &f.target_node,
            &principal,
            "cross_node_recovery_import_page",
            &input,
            now(),
        )?;
        assert_eq!(accepted["page_count"], 1);
        assert_eq!(accepted["complete"], false);
        assert_eq!(
            f.target.cross_node_recovery_command(
                &f.target_node,
                &principal,
                "cross_node_recovery_import_page",
                &input,
                now(),
            )?,
            accepted
        );
        let context = f.imported()?;
        let mut wrong = context.reference()?;
        wrong.witness_hash = ObjectHash::body(&"another witness")?;
        assert!(
            f.target
                .cross_node_recovery_command(
                    &f.target_node,
                    &principal,
                    "cross_node_recovery_import_finish",
                    &json!({"context":wrong,"witness":f.witness}),
                    now(),
                )
                .is_err()
        );
        assert_eq!(
            f.target.cross_node_recovery_command(
                &f.target_node,
                &principal,
                "cross_node_recovery_import_finish",
                &json!({"context":context.reference()?,"witness":f.witness}),
                now(),
            )?["state"],
            "ready"
        );
        Ok(())
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
        assert_eq!(
            f.source.connection.query_row(
                "SELECT count(*) FROM identity_meta WHERE key GLOB 'recovery-read:*'",
                [],
                |row| row.get::<_, u64>(0)
            )?,
            0
        );
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
        assert!(
            f.target
                .cross_node_recovery_command(
                    &f.target_node,
                    &f.next_client.public_key_hex(),
                    "cross_node_recovery_import_begin",
                    &json!({"request":signed,"snapshot":context["snapshot"]}),
                    now(),
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
                    &completion_key(
                        &f.reference
                            .as_ref()
                            .context("reference missing")?
                            .snapshot_id
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
    fn source_proposal_rejects_a_new_device_without_a_policy_head_change() -> Result<()> {
        let mut f = Fixture::new()?;
        let (_, request) = f.commit_request()?;
        let domains = [
            PolicyDomain::Account(f.network.account.clone()),
            PolicyDomain::Space(f.network.space.clone()),
        ];
        let heads = domains
            .iter()
            .map(|domain| {
                f.source
                    .authorization_state()
                    .head(domain)
                    .cloned()
                    .context("head missing")
            })
            .collect::<Result<Vec<_>>>()?;
        let mut independent = Store::open(f.network.directory.join("source.sqlite3"))?;
        let before = journal_count(&independent)?;
        let old_client = DeviceId::from_key(&PublicKey::of(&f.network.client));
        let mut certificate = f
            .network
            .bundle
            .journal
            .iter()
            .find_map(|entry| match entry {
                JournalEntry::Device(certificate) if certificate.payload.device == old_client => {
                    Some(certificate.payload.clone())
                }
                _ => None,
            })
            .context("original client certificate is missing")?;
        certificate.public_key = PublicKey::of(&f.next_client);
        certificate.device = DeviceId::from_key(&certificate.public_key);
        certificate.nonce = Nonce::random()?;
        certificate.possession = None;
        certificate.prove_possession(&f.next_client)?;
        independent.install_policy_journal(
            &[JournalEntry::Device(Signed::sign(
                certificate,
                &f.network.root,
            )?)],
            now() + 1,
        )?;
        assert_eq!(journal_count(&independent)?, before + 1);
        assert!(
            heads
                .iter()
                .all(|head| independent.authorization_state().head(&head.domain) == Some(head))
        );
        assert!(f.source_commit(&request, now() + 2).is_err());
        assert_eq!(journal_count(&f.source)?, before + 1);
        let reference = f.reference.as_ref().context("reference missing")?;
        assert!(
            metadata(
                &f.source.connection,
                &completion_key(&reference.snapshot_id)
            )?
            .is_none()
        );
        assert_eq!(journal_count(&f.target)?, 0);
        f.target
            .verify_bootstrap_capability(&f.capability.capability)?;
        Ok(())
    }

    #[test]
    fn committed_recovery_rebuilds_from_durable_pages_after_target_restart_and_expiry() -> Result<()>
    {
        let mut f = Fixture::new()?;
        let (_, request) = f.commit_request()?;
        let reference = f.reference.clone().context("reference missing")?;
        let status = json!({"context":reference,"input_hash":ObjectHash::body(&request)?});
        let pending = f.source.cross_node_recovery_command(
            &f.network.node,
            &f.next_client.public_key_hex(),
            "cross_node_recovery_status",
            &status,
            now(),
        )?;
        assert_eq!(pending["state"], "not_committed");
        let response = f.source_commit(&request, now())?;
        let committed = &response["committed"];
        let source_count = journal_count(&f.source)?;
        f.target = Store::open(f.network.directory.join("target.sqlite3"))?;
        assert!(f.install(committed, now() + 200_000).is_err());
        assert_eq!(journal_count(&f.target)?, 0);
        f.target
            .verify_bootstrap_capability(&f.capability.capability)?;
        let resume = json!({"request":f.request,"snapshot":f.snapshot,"committed":committed});
        assert!(
            f.target
                .cross_node_recovery_command(
                    &f.target_node,
                    &f.network.client.public_key_hex(),
                    "cross_node_recovery_import_resume",
                    &resume,
                    now() + 200_000,
                )
                .is_err()
        );
        let mut before_approval: StreamedCrossNodeRecoveryCommitted =
            serde_json::from_value(committed.clone())?;
        let mut receipt = before_approval.receipt.payload.clone();
        receipt.accepted_at_ms = now() - 1;
        let source_signer = Identity::from_secret_hex(&f.network.node.secret_key_hex)
            .map_err(anyhow::Error::msg)?;
        before_approval.receipt = Signed::sign(receipt, &source_signer)?;
        assert!(
            f.target
                .cross_node_recovery_command(
                    &f.target_node,
                    &f.next_client.public_key_hex(),
                    "cross_node_recovery_import_resume",
                    &json!({"request":f.request,"snapshot":f.snapshot,"committed":before_approval}),
                    now() + 200_000,
                )
                .is_err()
        );
        f.target.cross_node_recovery_command(
            &f.target_node,
            &f.next_client.public_key_hex(),
            "cross_node_recovery_import_resume",
            &resume,
            now() + 200_000,
        )?;
        let first = f.target.cross_node_recovery_command(
            &f.target_node,
            &f.next_client.public_key_hex(),
            "cross_node_recovery_import_cached_page",
            &json!({"context":reference}),
            now() + 200_000,
        )?;
        assert_eq!(first["page_count"], 1);
        f.target = Store::open(f.network.directory.join("target.sqlite3"))?;
        f.resume_target(committed, now() + 200_000)?;
        assert_eq!(f.install(committed, now() + 200_000)?["installed"], true);
        let completed = f.source.cross_node_recovery_command(
            &f.network.node,
            &f.next_client.public_key_hex(),
            "cross_node_recovery_status",
            &status,
            now() + 200_000,
        )?;
        assert_eq!(completed["state"], "committed");
        assert_eq!(completed["committed"], *committed);
        assert_eq!(journal_count(&f.source)?, source_count);
        assert_eq!(f.source_commit(&request, now() + 200_000)?, response);
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
        f.resume_target(committed, now() + 200_000)?;
        assert_eq!(f.install(committed, now() + 200_000)?["installed"], true);
        assert!(
            f.target
                .verify_bootstrap_capability(&f.capability.capability)
                .is_err()
        );
        Ok(())
    }
}
