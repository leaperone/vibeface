use super::*;
use vibeface_secure::authorization::{
    AUTH_VERSION, MAX_POLICY_DOMAINS, MAX_POLICY_HISTORY,
    enrollment_policy_import::{
        EnrollmentPolicyPage, EnrollmentPolicyReadRequest, EnrollmentPolicySnapshot,
        EnrollmentPolicyWitness,
    },
    execution_handoff::ExecutionProofUsage,
};

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "purpose", content = "credential", rename_all = "snake_case")]
enum CredentialEnvelope {
    AccountNode(AccountNodeEnrollmentCredential),
}
impl CredentialEnvelope {
    fn node(&self) -> &AccountNodeEnrollmentCredential {
        match self {
            Self::AccountNode(value) => value,
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Begin {
    credential: CredentialEnvelope,
    request: Signed<EnrollmentPolicyReadRequest>,
    snapshot: Signed<EnrollmentPolicySnapshot>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Cached {
    begin: Begin,
    principal: String,
    pages: Vec<Signed<EnrollmentPolicyPage>>,
    witness: Option<Signed<EnrollmentPolicyWitness>>,
    installation: Option<AccountNodeInstallation>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Page {
    context: ObjectHash,
    page: Signed<EnrollmentPolicyPage>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Finish {
    context: ObjectHash,
    witness: Signed<EnrollmentPolicyWitness>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Resume {
    context: ObjectHash,
    installation: Option<AccountNodeInstallation>,
}

fn context_hash(begin: &Begin) -> Result<ObjectHash> {
    Ok(ObjectHash::body(&(
        ObjectHash::body(begin.credential.node())?,
        begin.request.hash()?,
        begin.snapshot.hash()?,
    ))?)
}
fn load(connection: &rusqlite::Connection) -> Result<Cached> {
    let cached: Cached = serde_json::from_str(
        &metadata(connection, "account_node_import")?
            .context("Node enrollment signed input cache is missing")?,
    )?;
    ensure!(
        cached.pages.len() <= MAX_POLICY_HISTORY * MAX_POLICY_DOMAINS,
        "Node enrollment signed input cache is too large"
    );
    Ok(cached)
}
fn save(tx: &Transaction<'_>, cached: &Cached) -> Result<()> {
    tx.execute("INSERT INTO identity_meta(key,value) VALUES('account_node_import',?) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
        [serde_json::to_string(cached)?])?;
    Ok(())
}
fn signer(identity: &LocalIdentity) -> Result<vibeface_secure::Identity> {
    vibeface_secure::Identity::from_secret_hex(&identity.secret_key_hex).map_err(anyhow::Error::msg)
}
pub(super) fn record_installation_usage(
    usage: &mut ExecutionProofUsage,
    installation: &AccountNodeInstallation,
) -> Result<()> {
    usage.record_metadata(installation)?;
    usage.record_signed(&installation.request.intent)?;
    usage.record_signed(&installation.request.intent.payload.claim)?;
    usage.record_signed(&installation.request.certificate)?;
    usage.record_signed(&installation.committed.proposal)?;
    usage.record_signed(&installation.committed.commit)?;
    for receipt in &installation.committed.receipts {
        usage.record_signed(receipt)?;
    }
    Ok(())
}
pub(super) fn verify_saved_possession(saved: &PossessionRecord) -> Result<()> {
    let approval = &saved.approval;
    approval.verify_signature()?;
    ensure!(
        saved.purpose == "account_node"
            && approval.payload.version == AUTH_VERSION
            && approval.issuer
                == saved
                    .credential
                    .intent
                    .payload
                    .claim
                    .payload
                    .node_public_key
            && approval.payload.node_public_key == approval.issuer
            && approval.payload.reader_public_key == PublicKey::parse(&saved.principal)?
            && approval.payload.context_hash == saved.context
            && approval.payload.credential_hash == ObjectHash::body(&saved.credential)?
            && approval.payload.proof_hash == ObjectHash::body(&saved.proof)?
            && approval.payload.approved_at_ms == saved.approved_at_ms,
        "Node enrollment cached possession approval changed"
    );
    Ok(())
}
fn check(
    cached: &Cached,
    context: &ObjectHash,
    principal: &str,
    identity: &LocalIdentity,
) -> Result<()> {
    let credential = cached.begin.credential.node();
    ensure!(
        context_hash(&cached.begin)? == *context
            && cached.principal == principal
            && credential.intent.payload.requesting_device
                == DeviceId::from_key(&PublicKey::parse(principal)?)
            && credential
                .intent
                .payload
                .claim
                .payload
                .node_public_key
                .as_str()
                == identity.public_key_hex()?,
        "Node enrollment context belongs to another request, Noise client or installation"
    );
    Ok(())
}

fn checked_clock(cached: &Cached, possession: Option<&PossessionRecord>, now: u64) -> Result<u64> {
    let credential = cached.begin.credential.node();
    let Some(installation) = &cached.installation else {
        ensure!(
            credential.intent.payload.issued_at_ms <= now
                && now < credential.intent.payload.expires_at_ms,
            "uncommitted Node enrollment has expired"
        );
        return Ok(now);
    };
    let original =
        possession.context("committed Node enrollment has no original local possession")?;
    verify_saved_possession(original)?;
    ensure!(
        cached
            .witness
            .as_ref()
            .map(|witness| witness.hash())
            .transpose()?
            .as_ref()
            == Some(&original.approval.payload.witness_hash),
        "Node enrollment cached witness changed its signed approval"
    );
    let accepted = installation.committed.commit.payload.accepted_at_ms;
    ensure!(
        original.purpose == "account_node"
            && original.credential == *credential
            && original.request_hash == ObjectHash::body(&credential.intent)?
            && original.context == context_hash(&cached.begin)?
            && original.principal == cached.principal
            && installation.request.intent == credential.intent
            && installation.request.descriptor == credential.descriptor
            && installation
                .request
                .certificate
                .payload
                .possession
                .as_ref()
                .map(|p| p.as_str())
                == Some(original.proof.as_str())
            && installation.committed.proposal == installation.request.proposal
            && installation.committed.commit.payload.version == AUTH_VERSION
            && installation.committed.commit.issuer == credential.descriptor.source_node_public_key
            && installation.committed.commit.payload.proposal_hash
                == installation.request.proposal.hash()?
            && original.approved_at_ms <= accepted
            && accepted <= now
            && credential.intent.payload.issued_at_ms <= accepted
            && accepted < credential.intent.payload.expires_at_ms,
        "Node enrollment historical completion changed its original approval or committed DTO"
    );
    installation.request.intent.verify_signature()?;
    installation.request.certificate.verify_signature()?;
    installation.request.proposal.verify_signature()?;
    installation.committed.commit.verify_signature()?;
    ensure!(
        !installation.committed.receipts.is_empty(),
        "Node enrollment completion has no authority receipt"
    );
    for receipt in &installation.committed.receipts {
        ensure!(
            receipt.issuer == credential.descriptor.source_node_public_key,
            "Node enrollment receipt belongs to another source authority"
        );
        receipt.verify_signature()?;
    }
    Ok(original.approved_at_ms)
}

impl Store {
    pub(super) fn account_node_import_begin(
        &mut self,
        identity: &LocalIdentity,
        principal: &str,
        params: &Value,
        now: u64,
    ) -> Result<Value> {
        let begin: Begin = serde_json::from_value(params.clone())?;
        let credential = begin.credential.node();
        let node = signer(identity)?;
        let mut importer = node_enrollment::begin_account_node_enrollment_import(
            &serde_json::to_string(credential)?,
            &serde_json::to_string(&begin.request)?,
            &serde_json::to_string(&begin.snapshot)?,
            &node,
            credential.intent.payload.claim.payload.claim_nonce.as_str(),
            principal,
            now as f64,
        )
        .map_err(anyhow::Error::msg)?;
        let context = importer.context_hash()?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        check_claim_in(&tx, &credential.intent.payload.claim)?;
        if let Some(saved) = metadata(&tx, "node_possession")? {
            let saved: PossessionRecord = serde_json::from_str(&saved)?;
            verify_saved_possession(&saved)?;
            ensure!(
                saved.purpose == "account_node"
                    && saved.context == context
                    && saved.principal == principal
                    && saved.credential == *credential,
                "local Node possession is already bound to another purpose or enrollment"
            );
        }
        if metadata(&tx, "account_node_import")?.is_some() {
            let old = load(&tx)?;
            check(&old, &context, principal, identity)?;
            ensure!(
                serde_json::to_value(&old.begin)? == serde_json::to_value(&begin)?,
                "Node enrollment read snapshot is already fixed"
            );
            if let Some(live) = &self.account_node_enrollment_import {
                ensure!(
                    live.context_hash()? == context,
                    "live Node enrollment context changed"
                );
                importer = live.clone();
            } else {
                ensure!(
                    old.pages.is_empty(),
                    "Node enrollment signed pages must be resumed"
                );
            }
        } else {
            save(
                &tx,
                &Cached {
                    begin,
                    principal: principal.to_owned(),
                    pages: Vec::new(),
                    witness: None,
                    installation: None,
                },
            )?;
        }
        let request: Value =
            serde_json::from_str(&importer.page_request_json().map_err(anyhow::Error::msg)?)?;
        let proof = metadata(&tx, "node_possession")?
            .map(|value| serde_json::from_str::<PossessionRecord>(&value))
            .transpose()?
            .filter(|_| importer.completed_context().is_some())
            .map(|value| value.proof);
        let response = bounded_response(
            params,
            json!({"context":context,"page_request":request,"proof":proof}),
        )?;
        tx.commit()?;
        self.account_node_enrollment_import = Some(importer);
        Ok(response)
    }

    pub(super) fn account_node_import_page(
        &mut self,
        identity: &LocalIdentity,
        principal: &str,
        params: &Value,
        now: u64,
    ) -> Result<Value> {
        let input: Page = serde_json::from_value(params.clone())?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut cached = load(&tx)?;
        check(&cached, &input.context, principal, identity)?;
        check_claim_in(&tx, &cached.begin.credential.node().intent.payload.claim)?;
        let clock = checked_clock(&cached, None, now)?;
        let mut importer = self
            .account_node_enrollment_import
            .as_ref()
            .context("Node enrollment signed pages must be resumed")?
            .clone();
        ensure!(
            importer.context_hash()? == input.context,
            "live Node enrollment context changed"
        );
        let index = usize::try_from(input.page.payload.page_index)
            .context("Node enrollment page index overflow")?;
        if index < cached.pages.len() {
            ensure!(
                cached.pages.get(index) == Some(&input.page),
                "Node enrollment page retry changed its signed input"
            );
            let snapshot = &cached.begin.snapshot;
            let page = &input.page.payload;
            ensure!(
                input.page.issuer == snapshot.issuer
                    && page.version == AUTH_VERSION
                    && page.purpose == snapshot.payload.purpose
                    && page.credential_hash == cached.begin.request.payload.credential_hash
                    && page.request_hash == cached.begin.request.hash()?
                    && page.snapshot_id == snapshot.payload.snapshot_id
                    && page.snapshot_hash == snapshot.hash()?
                    && snapshot.payload.issued_at_ms <= page.issued_at_ms
                    && page.issued_at_ms <= clock
                    && clock < page.expires_at_ms
                    && page.expires_at_ms <= snapshot.payload.expires_at_ms,
                "Node enrollment page retry changed its source, purpose, snapshot or validity"
            );
            input.page.verify_signature()?;
            let mut usage = ExecutionProofUsage::default();
            usage.record_signed(&input.page)?;
            if let Some(entry) = &input.page.payload.entry {
                usage.merge(
                    &vibeface_secure::authorization::policy_proof::policy_entry_payload_usage(
                        entry,
                    )?,
                );
            }
            let credential = cached.begin.credential.node();
            usage.record_signed(&credential.intent)?;
            usage.record_signed(&credential.intent.payload.claim)?;
            usage.record_metadata(&credential.descriptor)?;
            usage.record_metadata(credential)?;
            usage.enforce_limits()?;
        } else {
            ensure!(
                index == cached.pages.len() && importer.accepted_page_count() == cached.pages.len(),
                "Node enrollment pages are out of order or need resuming"
            );
            importer
                .append_page(&serde_json::to_string(&input.page)?, clock as f64)
                .map_err(anyhow::Error::msg)?;
            cached.pages.push(input.page);
            save(&tx, &cached)?;
        }
        let request: Value =
            serde_json::from_str(&importer.page_request_json().map_err(anyhow::Error::msg)?)?;
        let response = bounded_response(
            params,
            json!({"context":input.context,"page_request":request}),
        )?;
        tx.commit()?;
        self.account_node_enrollment_import = Some(importer);
        Ok(response)
    }

    pub(super) fn account_node_import_finish(
        &mut self,
        identity: &LocalIdentity,
        principal: &str,
        params: &Value,
        now: u64,
    ) -> Result<Value> {
        let input: Finish = serde_json::from_value(params.clone())?;
        let node = signer(identity)?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut cached = load(&tx)?;
        check(&cached, &input.context, principal, identity)?;
        let credential = cached.begin.credential.node();
        check_claim_in(&tx, &credential.intent.payload.claim)?;
        let saved = metadata(&tx, "node_possession")?
            .map(|value| serde_json::from_str::<PossessionRecord>(&value))
            .transpose()?;
        let clock = checked_clock(&cached, saved.as_ref(), now)?;
        let mut importer = self
            .account_node_enrollment_import
            .as_ref()
            .context("Node enrollment signed pages must be resumed")?
            .clone();
        ensure!(
            importer.context_hash()? == input.context
                && importer.accepted_page_count() == cached.pages.len(),
            "Node enrollment import is incomplete or changed"
        );
        if let Some(witness) = &cached.witness {
            ensure!(
                *witness == input.witness,
                "Node enrollment witness retry changed"
            );
        }
        let original_clock = saved.as_ref().map_or(clock, |value| value.approved_at_ms);
        let context = importer
            .finish(
                &serde_json::to_string(&input.witness)?,
                &node,
                credential.intent.payload.claim.payload.claim_nonce.as_str(),
                principal,
                original_clock as f64,
            )
            .map_err(anyhow::Error::msg)?;
        let proof = node_enrollment::sign_account_node_enrollment_possession(
            &context,
            &node,
            credential.intent.payload.claim.payload.claim_nonce.as_str(),
            original_clock as f64,
        )
        .map_err(anyhow::Error::msg)?;
        let approval =
            node_enrollment::sign_account_node_enrollment_import_approval(&context, &node, &proof)?;
        let mut usage = importer.last_proof_usage().clone();
        usage.record_signed(&approval)?;
        usage.record_metadata(params)?;
        usage.enforce_limits()?;
        if let Some(saved) = saved {
            verify_saved_possession(&saved)?;
            ensure!(
                saved.purpose == "account_node"
                    && saved.context == input.context
                    && saved.principal == principal
                    && saved.credential == *credential
                    && saved.proof == proof,
                "Node possession was already bound to another enrollment"
            );
        } else {
            let record = PossessionRecord {
                purpose: "account_node".to_owned(),
                request_hash: ObjectHash::body(&credential.intent)?,
                context: input.context.clone(),
                principal: principal.to_owned(),
                credential: credential.clone(),
                approved_at_ms: original_clock,
                approval,
                proof: proof.clone(),
            };
            tx.execute(
                "INSERT INTO identity_meta(key,value) VALUES('node_possession',?)",
                [serde_json::to_string(&record)?],
            )?;
        }
        cached.witness = Some(input.witness);
        save(&tx, &cached)?;
        let response = bounded_response(params, json!({"context":input.context,"proof":proof}))?;
        tx.commit()?;
        self.account_node_enrollment_import = Some(importer);
        Ok(response)
    }

    pub(super) fn account_node_import_resume(
        &mut self,
        identity: &LocalIdentity,
        principal: &str,
        params: &Value,
        now: u64,
    ) -> Result<Value> {
        let input: Resume = serde_json::from_value(params.clone())?;
        let node = signer(identity)?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut cached = load(&tx)?;
        check(&cached, &input.context, principal, identity)?;
        check_claim_in(&tx, &cached.begin.credential.node().intent.payload.claim)?;
        if let Some(installation) = input.installation {
            if let Some(old) = &cached.installation {
                ensure!(
                    serde_json::to_value(old)? == serde_json::to_value(&installation)?,
                    "Node enrollment resume changed its original committed DTO"
                );
            }
            cached.installation = Some(installation);
        }
        let possession = metadata(&tx, "node_possession")?
            .map(|value| serde_json::from_str::<PossessionRecord>(&value))
            .transpose()?;
        let clock = checked_clock(&cached, possession.as_ref(), now)?;
        let credential = cached.begin.credential.node();
        let mut importer = match &self.account_node_enrollment_import {
            Some(importer) => {
                ensure!(
                    importer.context_hash()? == input.context,
                    "live Node enrollment context changed"
                );
                importer.clone()
            }
            None => node_enrollment::begin_account_node_enrollment_import(
                &serde_json::to_string(credential)?,
                &serde_json::to_string(&cached.begin.request)?,
                &serde_json::to_string(&cached.begin.snapshot)?,
                &node,
                credential.intent.payload.claim.payload.claim_nonce.as_str(),
                principal,
                clock as f64,
            )
            .map_err(anyhow::Error::msg)?,
        };
        let index = importer.accepted_page_count();
        let response = if let Some(page) = cached.pages.get(index) {
            importer
                .append_page(&serde_json::to_string(page)?, clock as f64)
                .map_err(anyhow::Error::msg)?;
            json!({"context":input.context,"ready":false,"replayed_pages":index+1,"cached_pages":cached.pages.len()})
        } else if let Some(witness) = &cached.witness {
            let original_clock = possession
                .as_ref()
                .context("Node enrollment cached witness has no original possession")?
                .approved_at_ms;
            let context = importer
                .finish(
                    &serde_json::to_string(witness)?,
                    &node,
                    credential.intent.payload.claim.payload.claim_nonce.as_str(),
                    principal,
                    original_clock as f64,
                )
                .map_err(anyhow::Error::msg)?;
            let proof = node_enrollment::sign_account_node_enrollment_possession(
                &context,
                &node,
                credential.intent.payload.claim.payload.claim_nonce.as_str(),
                original_clock as f64,
            )
            .map_err(anyhow::Error::msg)?;
            ensure!(
                possession
                    .as_ref()
                    .is_some_and(|saved| saved.proof == proof),
                "Node enrollment resumed possession changed"
            );
            json!({"context":input.context,"ready":true,"proof":proof})
        } else {
            json!({"context":input.context,"ready":false,"page_request":serde_json::from_str::<Value>(
                &importer.page_request_json().map_err(anyhow::Error::msg)?)?})
        };
        let mut usage = importer.last_proof_usage().clone();
        if let Some(possession) = &possession {
            usage.record_signed(&possession.approval)?;
        }
        if let Some(installation) = &cached.installation {
            record_installation_usage(&mut usage, installation)?;
        }
        usage.record_metadata(params)?;
        usage.enforce_limits()?;
        let response = bounded_response(params, response)?;
        save(&tx, &cached)?;
        tx.commit()?;
        self.account_node_enrollment_import = Some(importer);
        Ok(response)
    }
}
