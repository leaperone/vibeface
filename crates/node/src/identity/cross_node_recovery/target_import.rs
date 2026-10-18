use super::*;
use crate::identity::bounded_recovery_context::target_cache::{
    UntrustedTargetRecoveryApproval, UntrustedTargetRecoveryCache,
};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BeginImport {
    request: Signed<CrossNodeRecoveryRequest>,
    snapshot: Signed<RecoveryContextSnapshot>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ResumeImport {
    request: Signed<CrossNodeRecoveryRequest>,
    snapshot: Signed<RecoveryContextSnapshot>,
    committed: StreamedCrossNodeRecoveryCommitted,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct TargetApprovalBinding {
    purpose: String,
    context: RecoveryContextReference,
    principal: String,
    possession_hash: ObjectHash,
    approval_hash: ObjectHash,
}

fn approval_binding_in(
    connection: &Connection,
    principal: &str,
    request: &Signed<CrossNodeRecoveryRequest>,
    snapshot: &Signed<RecoveryContextSnapshot>,
) -> Result<Option<TargetApprovalBinding>> {
    let Some(encoded) = metadata(connection, "node_possession")? else {
        return Ok(None);
    };
    let binding: TargetApprovalBinding = serde_json::from_str(&encoded)?;
    ensure!(
        binding.purpose == "cross_node_recovery_v2"
            && binding.principal == principal
            && binding.context.request_hash == request.hash()?
            && binding.context.snapshot_id == snapshot.payload.snapshot_id
            && binding.context.snapshot_hash == snapshot.hash()?,
        "bootstrap capability is bound to another account, client or recovery"
    );
    Ok(Some(binding))
}

fn require_approval_binding_in(
    connection: &Connection,
    principal: &str,
    snapshot: &Signed<RecoveryContextSnapshot>,
    approved: &UntrustedTargetRecoveryApproval,
) -> Result<()> {
    let binding = approval_binding_in(
        connection,
        principal,
        &approved.possession.request,
        snapshot,
    )?
    .context("target recovery approval binding is missing")?;
    ensure!(
        binding.context == approved.possession.context
            && binding.possession_hash == ObjectHash::body(&approved.possession)?
            && binding.approval_hash == approved.approval.hash()?,
        "target recovery changed its first approved possession"
    );
    Ok(())
}

enum TargetImportState {
    Reading(RecoveryContextImport),
    Ready(VerifiedRecoveryPolicyContext),
}

pub(crate) struct TargetRecoveryImport {
    request: Signed<CrossNodeRecoveryRequest>,
    snapshot: Signed<RecoveryContextSnapshot>,
    reference: Option<RecoveryContextReference>,
    approved_at: Option<u64>,
    historical_completion: Option<ObjectHash>,
    next_page: u64,
    complete: bool,
    state: TargetImportState,
}

impl TargetRecoveryImport {
    fn cache(&self) -> Result<UntrustedTargetRecoveryCache> {
        UntrustedTargetRecoveryCache::new(&self.request, &self.snapshot.payload.snapshot_id)
    }

    fn check_snapshot(
        &self,
        principal: &str,
        snapshot_id: &Nonce,
        request_hash: &ObjectHash,
        snapshot_hash: &ObjectHash,
    ) -> Result<()> {
        require_principal(&self.request, principal)?;
        ensure!(
            *snapshot_id == self.snapshot.payload.snapshot_id
                && *request_hash == self.request.hash()?
                && *snapshot_hash == self.snapshot.hash()?,
            "target recovery changed its original snapshot"
        );
        Ok(())
    }

    fn check(&self, principal: &str, reference: &RecoveryContextReference) -> Result<()> {
        self.check_snapshot(
            principal,
            &reference.snapshot_id,
            &reference.request_hash,
            &reference.snapshot_hash,
        )?;
        ensure!(
            self.reference
                .as_ref()
                .is_none_or(|saved| saved == reference),
            "target recovery changed its completion reference"
        );
        Ok(())
    }

    fn clock(&self, now: u64) -> u64 {
        self.approved_at.unwrap_or(now)
    }

    fn progress(&self) -> Result<Value> {
        Ok(json!({
            "state": if matches!(self.state, TargetImportState::Ready(_)) {"ready"} else {"reading"},
            "page_count": self.next_page,
            "complete": self.complete,
            "snapshot_id": self.snapshot.payload.snapshot_id,
            "request_hash": self.snapshot.payload.request_hash,
            "snapshot_hash": self.snapshot.hash()?,
        }))
    }

    fn record_binding(&self, usage: &mut RecoveryProofUsage) -> Result<()> {
        usage.record_request(&RecoveryReadRequest::CrossNode(self.request.clone()))?;
        usage.record_snapshot(&self.snapshot)?;
        Ok(())
    }
}

fn begin_native(
    request: &Signed<CrossNodeRecoveryRequest>,
    snapshot: &Signed<RecoveryContextSnapshot>,
    local: &PublicKey,
    now: u64,
) -> Result<RecoveryContextImport> {
    Ok(stream::begin_cross_node_recovery_policy_import(
        &serde_json::to_string(request)?,
        &serde_json::to_string(snapshot)?,
        local.as_str(),
        request.payload.target_claim.payload.claim_nonce.as_str(),
        now,
    )?)
}

fn record_cached_completion_binding(
    cache: &UntrustedTargetRecoveryCache,
    connection: &Connection,
    usage: &mut RecoveryProofUsage,
) -> Result<()> {
    let witness = cache
        .load_witness(connection)?
        .context("original recovery witness is missing")?;
    usage.record_witness(&witness)?;
    let last_index = witness
        .payload
        .page_count
        .checked_sub(1)
        .context("recovery witness has no page")?;
    let last = cache
        .load_page(connection, last_index)?
        .context("original final recovery page is missing")?;
    usage.record_page_binding(&last)?;
    Ok(())
}

impl Store {
    pub(super) fn cross_node_recovery_import_begin(
        &mut self,
        identity: &LocalIdentity,
        principal: &str,
        params: &Value,
        now: u64,
    ) -> Result<Value> {
        let input: BeginImport = serde_json::from_value(params.clone())?;
        require_principal(&input.request, principal)?;
        let local = PublicKey::parse(&identity.public_key_hex()?)?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        check_claim_in(&tx, &input.request.payload.target_claim, &local)?;
        approval_binding_in(&tx, principal, &input.request, &input.snapshot)?;
        let importer = begin_native(&input.request, &input.snapshot, &local, now)?;
        let cache =
            UntrustedTargetRecoveryCache::new(&input.request, &input.snapshot.payload.snapshot_id)?;
        cache.store_header(&tx, &input.snapshot)?;
        let active = TargetRecoveryImport {
            request: input.request,
            snapshot: input.snapshot,
            reference: None,
            approved_at: None,
            historical_completion: None,
            next_page: 0,
            complete: false,
            state: TargetImportState::Reading(importer),
        };
        let response = active.progress()?;
        let mut usage = RecoveryProofUsage::new(carrier_bytes(params, &response)?)?;
        active.record_binding(&mut usage)?;
        if let TargetImportState::Reading(importer) = &active.state {
            importer.record_proof_usage_into(&mut usage);
        }
        usage.finish()?;
        tx.commit()?;
        self.target_recovery_import = Some(active);
        Ok(response)
    }

    pub(super) fn cross_node_recovery_import_resume(
        &mut self,
        identity: &LocalIdentity,
        principal: &str,
        params: &Value,
        _now: u64,
    ) -> Result<Value> {
        let input: ResumeImport = serde_json::from_value(params.clone())?;
        require_principal(&input.request, principal)?;
        let local = PublicKey::parse(&identity.public_key_hex()?)?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        check_claim_in(&tx, &input.request.payload.target_claim, &local)?;
        let cache =
            UntrustedTargetRecoveryCache::new(&input.request, &input.snapshot.payload.snapshot_id)?;
        let header = cache
            .load_header(&tx)?
            .context("target recovery header is missing")?;
        ensure!(
            header.request == RecoveryReadRequest::CrossNode(input.request.clone())
                && header.snapshot == input.snapshot,
            "target recovery resume changed the original header"
        );
        let approved = cache
            .load_approval(&tx)?
            .context("original target approval is missing")?;
        require_approval_binding_in(&tx, principal, &input.snapshot, &approved)?;
        cache.store_committed(&tx, &input.committed)?;
        let approval = &approved.approval;
        let accepted = input.committed.receipt.payload.accepted_at_ms;
        let at = approval.payload.approved_at_ms;
        ensure!(
            approval.issuer == local
                && approved.possession.request == input.request
                && approved.possession.context == input.committed.request.possession.context
                && input.snapshot.payload.issued_at_ms <= at
                && input.request.payload.issued_at_ms <= at
                && at <= accepted
                && accepted <= approval.payload.expires_at_ms
                && accepted <= input.request.payload.expires_at_ms
                && accepted <= input.snapshot.payload.expires_at_ms,
            "target recovery resume is outside its original signed approval window"
        );
        let importer = begin_native(&input.request, &input.snapshot, &local, at)?;
        let active = TargetRecoveryImport {
            request: input.request,
            snapshot: input.snapshot,
            reference: Some(input.committed.request.possession.context.clone()),
            approved_at: Some(at),
            historical_completion: Some(ObjectHash::body(&input.committed)?),
            next_page: 0,
            complete: false,
            state: TargetImportState::Reading(importer),
        };
        let response = active.progress()?;
        let mut usage = RecoveryProofUsage::new(carrier_bytes(params, &response)?)?;
        active.record_binding(&mut usage)?;
        usage.record_cross_approval(approval)?;
        usage.record_cross_committed_carrier(&input.committed)?;
        record_cached_completion_binding(&cache, &tx, &mut usage)?;
        if let TargetImportState::Reading(importer) = &active.state {
            importer.record_proof_usage_into(&mut usage);
        }
        usage.finish()?;
        tx.commit()?;
        self.target_recovery_import = Some(active);
        Ok(response)
    }

    pub(super) fn cross_node_recovery_import_page(
        &mut self,
        identity: &LocalIdentity,
        principal: &str,
        params: &Value,
        now: u64,
        cached: bool,
    ) -> Result<Value> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct PageInput {
            snapshot_id: Nonce,
            request_hash: ObjectHash,
            snapshot_hash: ObjectHash,
            page: Signed<RecoveryContextPage>,
        }
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct CachedInput {
            context: RecoveryContextReference,
        }
        let active = self
            .target_recovery_import
            .as_ref()
            .context("target recovery import is missing")?;
        let cache = active.cache()?;
        let local = PublicKey::parse(&identity.public_key_hex()?)?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        check_claim_in(&tx, &active.request.payload.target_claim, &local)?;
        let page = if cached {
            let input: CachedInput = serde_json::from_value(params.clone())?;
            active.check(principal, &input.context)?;
            cache
                .load_page(&tx, active.next_page)?
                .context("cached recovery page is missing")?
        } else {
            let input: PageInput = serde_json::from_value(params.clone())?;
            active.check_snapshot(
                principal,
                &input.snapshot_id,
                &input.request_hash,
                &input.snapshot_hash,
            )?;
            input.page
        };
        let TargetImportState::Reading(importer) = &active.state else {
            bail!("target recovery import is already complete");
        };
        let fresh = page.payload.page_index == active.next_page;
        ensure!(
            fresh || page.payload.page_index.checked_add(1) == Some(active.next_page),
            "target recovery page is out of order"
        );
        let previous = page
            .payload
            .page_index
            .checked_sub(1)
            .map(|index| cache.load_page(&tx, index))
            .transpose()?
            .flatten();
        let mut importer = importer.clone();
        importer.append(page.clone(), active.clock(now))?;
        cache.store_page(&tx, &page)?;
        let next_page = if fresh {
            active
                .next_page
                .checked_add(1)
                .context("target recovery page count overflow")?
        } else {
            active.next_page
        };
        let response =
            json!({"state":"reading","page_count":next_page,"complete":page.payload.complete});
        let mut usage = RecoveryProofUsage::new(carrier_bytes(params, &response)?)?;
        active.record_binding(&mut usage)?;
        if let Some(previous) = previous {
            usage.record_page_binding(&previous)?;
        }
        importer.record_proof_usage_into(&mut usage);
        usage.finish()?;
        tx.commit()?;
        let active = self
            .target_recovery_import
            .as_mut()
            .context("target recovery import is missing")?;
        active.next_page = next_page;
        active.complete = page.payload.complete;
        active.state = TargetImportState::Reading(importer);
        Ok(response)
    }

    pub(super) fn cross_node_recovery_import_finish(
        &mut self,
        identity: &LocalIdentity,
        principal: &str,
        params: &Value,
        now: u64,
    ) -> Result<Value> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct FinishInput {
            context: RecoveryContextReference,
            witness: Signed<RecoveryContextWitness>,
        }
        let input: FinishInput = serde_json::from_value(params.clone())?;
        let active = self
            .target_recovery_import
            .as_ref()
            .context("target recovery import is missing")?;
        active.check(principal, &input.context)?;
        ensure!(
            input.witness.hash()? == input.context.witness_hash,
            "target recovery finish changed its original witness"
        );
        let local = PublicKey::parse(&identity.public_key_hex()?)?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        check_claim_in(&tx, &active.request.payload.target_claim, &local)?;
        let cache = active.cache()?;
        cache.store_witness(&tx, &input.witness)?;
        let verified = match &active.state {
            TargetImportState::Reading(importer) => importer
                .clone()
                .finish_native(input.witness.clone(), active.clock(now))?,
            TargetImportState::Ready(verified) => {
                ensure!(
                    verified.witness() == &input.witness,
                    "target recovery changed its completed witness"
                );
                verified.clone()
            }
        };
        ensure!(
            verified.reference()? == input.context,
            "target recovery completed another reference"
        );
        let response = json!({"state":"ready","context":input.context});
        let mut usage = RecoveryProofUsage::new(carrier_bytes(params, &response)?)?;
        active.record_binding(&mut usage)?;
        let last_index = input
            .witness
            .payload
            .page_count
            .checked_sub(1)
            .context("recovery witness has no page")?;
        let last = cache
            .load_page(&tx, last_index)?
            .context("target recovery final page is missing")?;
        usage.record_page_binding(&last)?;
        verified.record_proof_usage_into(&mut usage);
        usage.finish()?;
        tx.commit()?;
        let active = self
            .target_recovery_import
            .as_mut()
            .context("target recovery import is missing")?;
        active.reference = Some(input.context);
        active.state = TargetImportState::Ready(verified);
        Ok(response)
    }

    pub(super) fn cross_node_recovery_possession(
        &mut self,
        identity: &LocalIdentity,
        principal: &str,
        params: &Value,
        now: u64,
    ) -> Result<Value> {
        let input: StreamedCrossNodeRecoveryPossessionRequest =
            serde_json::from_value(params.clone())?;
        require_principal(&input.request, principal)?;
        let signer =
            Identity::from_secret_hex(&identity.secret_key_hex).map_err(anyhow::Error::msg)?;
        let local = PublicKey::of(&signer);
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        check_claim_in(&tx, &input.request.payload.target_claim, &local)?;
        let cache = UntrustedTargetRecoveryCache::new(&input.request, &input.context.snapshot_id)?;
        if let Some(previous) = cache.load_approval(&tx)? {
            ensure!(
                previous.possession == input,
                "target recovery already approved another proposal"
            );
            let response = bounded(json!({"proof":previous.proof,"approval":previous.approval}))?;
            let header = cache
                .load_header(&tx)?
                .context("target recovery header is missing")?;
            require_approval_binding_in(&tx, principal, &header.snapshot, &previous)?;
            let mut usage = RecoveryProofUsage::new(carrier_bytes(params, &response)?)?;
            usage.record_request(&header.request)?;
            usage.record_snapshot(&header.snapshot)?;
            usage.record_cross_approval(&previous.approval)?;
            usage.record_cross_possession_carrier(&previous.possession)?;
            record_cached_completion_binding(&cache, &tx, &mut usage)?;
            usage.finish()?;
            return Ok(response);
        }
        let active = self
            .target_recovery_import
            .as_ref()
            .context("target recovery import is missing")?;
        active.check(principal, &input.context)?;
        ensure!(
            metadata(&tx, "node_possession")?.is_none(),
            "bootstrap capability already approved another possession"
        );
        ensure!(
            active.historical_completion.is_none(),
            "historical recovery cannot approve another proposal"
        );
        let TargetImportState::Ready(context) = &active.state else {
            bail!("target recovery import is incomplete");
        };
        let (proof, approval) = stream::approve_cross_node_recovery_import(
            context,
            &input,
            &signer,
            &input.request.payload.target_claim.payload.claim_nonce,
            now,
        )?;
        cache.store_approval(
            &tx,
            &UntrustedTargetRecoveryApproval {
                possession: input.clone(),
                proof: proof.clone(),
                approval: approval.clone(),
            },
        )?;
        let binding = TargetApprovalBinding {
            purpose: "cross_node_recovery_v2".into(),
            context: input.context.clone(),
            principal: principal.into(),
            possession_hash: ObjectHash::body(&input)?,
            approval_hash: approval.hash()?,
        };
        tx.execute(
            "INSERT INTO identity_meta(key,value) VALUES('node_possession',?)",
            [serde_json::to_string(&binding)?],
        )?;
        let response = bounded(json!({"proof":proof,"approval":approval}))?;
        let mut usage = RecoveryProofUsage::new(carrier_bytes(params, &response)?)?;
        active.record_binding(&mut usage)?;
        context.record_proof_usage_into(&mut usage);
        usage.record_cross_possession_in_context(&input, context)?;
        usage.record_cross_approval(&approval)?;
        record_cached_completion_binding(&cache, &tx, &mut usage)?;
        usage.finish()?;
        tx.commit()?;
        Ok(response)
    }

    pub(super) fn cross_node_recovery_install(
        &mut self,
        identity: &LocalIdentity,
        principal: &str,
        params: &Value,
        now: u64,
    ) -> Result<Value> {
        let committed: StreamedCrossNodeRecoveryCommitted = serde_json::from_value(params.clone())?;
        let original = &committed.request.possession.request;
        require_principal(original, principal)?;
        let local = PublicKey::parse(&identity.public_key_hex()?)?;
        let completion_hash = ObjectHash::body(&committed)?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let Some(previous) = metadata(&tx, "cross_node_recovery_installation")? {
            let previous: Value = serde_json::from_str(&previous)?;
            ensure!(
                previous["completion_hash"] == completion_hash.as_str()
                    && previous["principal"] == principal,
                "target Node already installed another recovery completion"
            );
            let response = bounded(previous["response"].clone())?;
            let mut usage = RecoveryProofUsage::new(carrier_bytes(params, &response)?)?;
            usage.record_cross_committed_carrier(&committed)?;
            usage.finish()?;
            return Ok(response);
        }
        check_claim_in(&tx, &original.payload.target_claim, &local)?;
        let cache = UntrustedTargetRecoveryCache::new(
            original,
            &committed.request.possession.context.snapshot_id,
        )?;
        let approved = cache
            .load_approval(&tx)?
            .context("original target approval is missing")?;
        let header = cache
            .load_header(&tx)?
            .context("target recovery header is missing")?;
        require_approval_binding_in(&tx, principal, &header.snapshot, &approved)?;
        ensure!(
            approved.possession == committed.request.possession,
            "target installation changed its original approved proposal"
        );
        cache.store_committed(&tx, &committed)?;
        let active = self
            .target_recovery_import
            .take()
            .context("target recovery import is missing")?;
        active.check(principal, &committed.request.possession.context)?;
        ensure!(
            active
                .historical_completion
                .as_ref()
                .is_none_or(|hash| hash == &completion_hash),
            "historical target installation changed its original committed input"
        );
        let TargetImportState::Ready(context) = active.state else {
            bail!("target recovery import is incomplete");
        };
        let verified = stream::verify_cross_node_recovery_stream_installation(
            context,
            &approved.approval,
            &committed,
            &local,
            &original.payload.target_claim.payload.claim_nonce,
            now,
        )?;
        let response = bounded(json!({"installed":true,"account_id":verified.account(),
            "node_public_key":verified.target_node_public_key(),"pins":committed.request.pins}))?;
        let mut usage = RecoveryProofUsage::new(carrier_bytes(params, &response)?)?;
        verified.record_proof_usage_into(&mut usage);
        record_cached_completion_binding(&cache, &tx, &mut usage)?;
        usage.finish()?;
        let update = self.policy.prepare_cross_recovery_install(verified, now)?;
        let state = journal::commit_in(&tx, &update)?;
        project_policy_in(&tx, &state)?;
        ensure!(tx.execute("UPDATE identity_meta SET value='consumed:'||value WHERE key='bootstrap_capability_hash' AND value=?",
            [crate::crypto::digest(original.payload.target_claim.payload.claim_nonce.as_str().as_bytes())])? == 1,
            "bootstrap capability was already consumed");
        tx.execute("INSERT INTO identity_meta(key,value) VALUES('cross_node_recovery_installation',?)",
            [serde_json::to_string(&json!({"completion_hash":completion_hash,"principal":principal,"response":response}))?])?;
        tx.commit()?;
        self.policy = state;
        Ok(response)
    }
}
