use super::*;
use crate::authorization::enrollment_policy_import::{
    EnrollmentPolicyImport, EnrollmentPolicyPage, EnrollmentPolicySnapshot, EnrollmentPolicyWitness,
};

#[derive(Clone, Debug)]
#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen)]
pub struct AccountNodeEnrollmentImport {
    credential: AccountNodeEnrollmentCredential,
    inner: EnrollmentPolicyImport,
    completed: Option<VerifiedAccountNodeEnrollmentContext>,
    accepted_pages: usize,
    proof_usage: ExecutionProofUsage,
}

#[derive(Clone, Debug)]
#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen)]
pub struct VerifiedAccountNodeEnrollmentContext {
    pub(super) credential: AccountNodeEnrollmentCredential,
    pub(super) policy: VerifiedPolicySnapshot,
    pub(super) reader: PublicKey,
    pub(super) approved_at_ms: u64,
    pub(super) context_hash: ObjectHash,
    pub(super) witness_hash: ObjectHash,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AccountNodeEnrollmentImportApproval {
    pub version: u16,
    pub context_hash: ObjectHash,
    pub witness_hash: ObjectHash,
    pub credential_hash: ObjectHash,
    pub node_public_key: PublicKey,
    pub reader_public_key: PublicKey,
    pub proof_hash: ObjectHash,
    pub approved_at_ms: u64,
}
impl sealed::Sealed for AccountNodeEnrollmentImportApproval {}
impl SignedObject for AccountNodeEnrollmentImportApproval {
    const DOMAIN: &'static str = "account-node-enrollment-import-approval-v2";
}

impl VerifiedAccountNodeEnrollmentContext {
    pub fn approved_at_ms(&self) -> u64 {
        self.approved_at_ms
    }
}

pub fn sign_account_node_enrollment_import_approval(
    context: &VerifiedAccountNodeEnrollmentContext,
    node: &Identity,
    proof: &str,
) -> Result<Signed<AccountNodeEnrollmentImportApproval>> {
    require(
        PublicKey::of(node)
            == context
                .credential
                .intent
                .payload
                .claim
                .payload
                .node_public_key,
        "node enrollment approval belongs to another local Node",
    )?;
    let mut certificate = context.credential.intent.payload.certificate.clone();
    certificate.prove_possession(node)?;
    require(
        certificate
            .possession
            .as_ref()
            .is_some_and(|signature| signature.as_str() == proof),
        "node enrollment approval changed its independent possession proof",
    )?;
    Signed::sign(
        AccountNodeEnrollmentImportApproval {
            version: AUTH_VERSION,
            context_hash: context.context_hash.clone(),
            witness_hash: context.witness_hash.clone(),
            credential_hash: ObjectHash::body(&context.credential)?,
            node_public_key: PublicKey::of(node),
            reader_public_key: context.reader.clone(),
            proof_hash: ObjectHash::body(&proof)?,
            approved_at_ms: context.approved_at_ms,
        },
        node,
    )
}

fn credential_usage(
    credential: &AccountNodeEnrollmentCredential,
    mut usage: ExecutionProofUsage,
) -> Result<ExecutionProofUsage> {
    usage.record_signed(&credential.intent)?;
    usage.record_signed(&credential.intent.payload.claim)?;
    usage.record_metadata(&credential.descriptor)?;
    usage.record_metadata(credential)?;
    if let Some(counts) = crate::signature_verification::SignatureVerification::current_counts() {
        usage.record_actual_signature_counts(counts);
    }
    usage.enforce_limits()?;
    if crate::signature_verification::SignatureVerification::current_counts()
        .is_some_and(|counts| counts.actual_verifications > MAX_PROOF_SIGNATURES)
    {
        return Err(AuthorizationError::BudgetExceeded);
    }
    Ok(usage)
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = beginAccountNodeEnrollmentImport))]
pub fn begin_account_node_enrollment_import(
    credential_json: &str,
    request_json: &str,
    snapshot_json: &str,
    node: &Identity,
    expected_claim_nonce: &str,
    noise_public_key: &str,
    now_ms: f64,
) -> crate::Result<AccountNodeEnrollmentImport> {
    let _scope = crate::signature_verification::ScopedSignatureVerification::new();
    let begin = || -> Result<_> {
        require(
            credential_json.len() + request_json.len() + snapshot_json.len() <= MAX_PROOF_BYTES,
            "node enrollment import carrier exceeds its byte limit",
        )?;
        let credential: AccountNodeEnrollmentCredential =
            decode_bounded(credential_json, MAX_PROOF_BYTES)?;
        let request: Signed<EnrollmentPolicyReadRequest> =
            decode_bounded(request_json, MAX_PROOF_BYTES)?;
        let snapshot: Signed<EnrollmentPolicySnapshot> =
            decode_bounded(snapshot_json, MAX_PROOF_BYTES)?;
        let now = timestamp(now_ms)?;
        let target = PublicKey::of(node);
        let reader = PublicKey::parse(noise_public_key)?;
        let intent = &credential.intent;
        intent.verify_signature()?;
        validate_claim(
            &intent.payload.claim,
            &target,
            &Nonce::parse(expected_claim_nonce)?,
            now,
        )?;
        require(
            intent.payload.context_hash == ObjectHash::body(&credential.descriptor)?
                && credential.descriptor.purpose == EnrollmentPolicyPurpose::AccountNode
                && credential.descriptor.account == intent.payload.account
                && credential.descriptor.requesting_device == intent.payload.requesting_device
                && DeviceId::from_key(&reader) == intent.payload.requesting_device
                && credential.descriptor.claim_hash.as_ref() == Some(&intent.payload.claim.hash()?)
                && request.payload.target_public_key == target,
            "node enrollment import changed its Root approval, local claim or requesting client",
        )?;
        let inner = EnrollmentPolicyImport::begin(
            credential.descriptor.clone(),
            &ObjectHash::body(&credential)?,
            request,
            snapshot,
            &reader,
            &target,
            &credential.descriptor.source_node_public_key,
            now,
        )?;
        let proof_usage = credential_usage(&credential, inner.last_proof_usage().clone())?;
        Ok(AccountNodeEnrollmentImport {
            credential,
            inner,
            completed: None,
            accepted_pages: 0,
            proof_usage,
        })
    };
    begin().map_err(|error| error.to_string())
}

impl AccountNodeEnrollmentImport {
    pub fn context_hash(&self) -> Result<ObjectHash> {
        ObjectHash::body(&(
            ObjectHash::body(&self.credential)?,
            self.inner.request().hash()?,
            self.inner.snapshot().hash()?,
        ))
    }
    pub fn completed_context(&self) -> Option<&VerifiedAccountNodeEnrollmentContext> {
        self.completed.as_ref()
    }
    pub fn accepted_page_count(&self) -> usize {
        self.accepted_pages
    }
    pub fn last_proof_usage(&self) -> &ExecutionProofUsage {
        &self.proof_usage
    }
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen)]
impl AccountNodeEnrollmentImport {
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = proofUsageJson))]
    pub fn proof_usage_json(&self) -> crate::Result<String> {
        encode(&self.proof_usage.counts())
    }
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = pageRequestJson))]
    pub fn page_request_json(&self) -> crate::Result<String> {
        encode(&self.inner.page_request())
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen::prelude::wasm_bindgen(js_name = appendPage))]
    pub fn append_page(&mut self, page_json: &str, now_ms: f64) -> crate::Result<()> {
        let _scope = crate::signature_verification::ScopedSignatureVerification::new();
        let mut append = || -> Result<_> {
            require(
                self.completed.is_none(),
                "node enrollment import is already complete",
            )?;
            let page: Signed<EnrollmentPolicyPage> = decode_bounded(page_json, MAX_PROOF_BYTES)?;
            let mut staged = self.inner.clone();
            staged.append_page(page, timestamp(now_ms)?)?;
            let proof_usage =
                credential_usage(&self.credential, staged.last_proof_usage().clone())?;
            let accepted_pages = self
                .accepted_pages
                .checked_add(1)
                .ok_or(AuthorizationError::BudgetExceeded)?;
            self.inner = staged;
            self.accepted_pages = accepted_pages;
            self.proof_usage = proof_usage;
            Ok(())
        };
        append().map_err(|error| error.to_string())
    }

    pub fn finish(
        &mut self,
        witness_json: &str,
        node: &Identity,
        expected_claim_nonce: &str,
        noise_public_key: &str,
        now_ms: f64,
    ) -> crate::Result<VerifiedAccountNodeEnrollmentContext> {
        let _scope = crate::signature_verification::ScopedSignatureVerification::new();
        let mut finish = || -> Result<_> {
            let witness: Signed<EnrollmentPolicyWitness> =
                decode_bounded(witness_json, MAX_PROOF_BYTES)?;
            let now = timestamp(now_ms)?;
            let reader = PublicKey::parse(noise_public_key)?;
            require(
                self.inner.request().payload.reader_public_key == reader
                    && self.inner.request().payload.target_public_key == PublicKey::of(node),
                "node enrollment finish belongs to another local Node or Noise client",
            )?;
            validate_claim(
                &self.credential.intent.payload.claim,
                &PublicKey::of(node),
                &Nonce::parse(expected_claim_nonce)?,
                now,
            )?;
            let mut staged = self.inner.clone();
            let policy = staged.finish(&witness, now)?;
            let usage = verify_account_node_enrollment_read(
                policy.state(),
                &self.credential.intent,
                &self.credential.descriptor,
                &self.credential.descriptor.source_node_public_key,
                &reader,
                now,
            )?;
            let mut total = staged.last_proof_usage().clone();
            total.merge(&usage);
            let proof_usage = credential_usage(&self.credential, total)?;
            let completed = VerifiedAccountNodeEnrollmentContext {
                credential: self.credential.clone(),
                policy,
                reader,
                approved_at_ms: now,
                context_hash: self.context_hash()?,
                witness_hash: witness.hash()?,
            };
            self.inner = staged;
            self.completed = Some(completed.clone());
            self.proof_usage = proof_usage;
            Ok(completed)
        };
        finish().map_err(|error| error.to_string())
    }
}
