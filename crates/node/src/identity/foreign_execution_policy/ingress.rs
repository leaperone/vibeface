use super::*;
use std::collections::BTreeMap;
use vibeface_secure::authorization::{
    PolicyPin, PolicyStamp,
    node_execution_policy::{
        PolicyCarrierOrigin, TaskExecutionPolicyImport, VerifiedTaskCurrentPolicy,
        VerifiedTaskHistoryPolicy, VerifiedTaskPolicySource, begin_task_policy_import,
        task_policy_source_from_current,
    },
    remote_execution_policy::{
        RemoteExecutionPolicyPage, RemoteExecutionPolicyReadRequest, RemoteExecutionPolicySnapshot,
        RemoteExecutionPolicyWitness,
    },
};

#[derive(Default)]
pub(crate) struct ForeignExecutionPolicyCache {
    pending: BTreeMap<ObjectHash, CachedImport>,
}

struct CachedImport {
    importer: TaskExecutionPolicyImport,
    page_hashes: Vec<ObjectHash>,
    ready_for_finish: bool,
}

enum CompletedPolicy {
    Current(VerifiedTaskCurrentPolicy),
    History(VerifiedTaskHistoryPolicy),
}

#[derive(Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub(super) struct ImportRecord {
    pub(super) approval_ref: ObjectHash,
    pub(super) request_ref: ObjectHash,
    pub(super) snapshot_ref: ObjectHash,
    pub(super) page_count: u64,
    pub(super) ready_for_finish: bool,
}

#[derive(Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub(super) struct CompletionRecord {
    pub(super) import_ref: ObjectHash,
    pub(super) witness_ref: ObjectHash,
    pub(super) completed_at_ms: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Begin {
    request: RemoteExecutionPolicyReadRequest,
    snapshot: Signed<RemoteExecutionPolicySnapshot>,
    approval_ref: Option<ObjectHash>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Page {
    import_ref: ObjectHash,
    page: Signed<RemoteExecutionPolicyPage>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Finish {
    import_ref: ObjectHash,
    witness: Signed<RemoteExecutionPolicyWitness>,
}

pub(super) fn page_key(import: &ObjectHash, index: u64) -> String {
    format!("{PREFIX}:import-page:{import}:{index}")
}

fn progress(reference: &ObjectHash, cached: &CachedImport) -> Value {
    json!({"import_ref":reference,"next_page_index":cached.page_hashes.len(),"ready_for_finish":cached.ready_for_finish})
}

pub(super) fn response(
    input: &Value,
    output: Value,
    mut usage: ExecutionProofUsage,
) -> Result<Value> {
    ensure!(
        serde_json::to_vec(input)?.len() + serde_json::to_vec(&output)?.len() <= MAX_PROOF_BYTES,
        "foreign execution policy input and output exceed 64 KiB"
    );
    usage.record_metadata(&output)?;
    if let Some(counts) = SignatureVerification::current_counts() {
        ensure!(
            counts.actual_verifications <= 32,
            "foreign policy crypto budget exceeded"
        );
        usage.record_actual_signature_counts(counts);
    }
    usage.enforce_limits()?;
    Ok(output)
}

fn current_source_in(
    connection: &Connection,
    policy: &super::super::journal::PolicyState,
    identity: &LocalIdentity,
    principal: &str,
    intent: &Signed<RemoteExecutionPolicyReadIntent>,
    now: u64,
) -> Result<(VerifiedTaskPolicySource, ExecutionProofUsage)> {
    let local = PublicKey::parse(&identity.public_key_hex()?)?;
    let source = verify_remote_execution_policy_delegation_source(
        policy.verified(),
        intent,
        &local,
        &PublicKey::parse(principal)?,
        now,
    )?;
    check_heads_in(connection, policy, source.source_heads())?;
    policy.accepted_journal_snapshot(connection)?;
    let mut usage = source.proof_usage().clone();
    if intent.payload.purpose == RemoteExecutionPolicyPurpose::History {
        let reference = intent
            .payload
            .history
            .as_ref()
            .context("History has no original artifact")?;
        usage.merge(
            &super::super::execution_dispatch::retained_execution_history_reference_in(
                connection, identity, reference,
            )?,
        );
    } else {
        super::super::remote_execution_policy::check_source_task(
            connection, intent, &local, &mut usage,
        )?;
    }
    let pins: Vec<_> = policy
        .pins()
        .iter()
        .filter(|pin| {
            source
                .source_heads()
                .iter()
                .any(|head| head.domain == pin.domain)
        })
        .cloned()
        .collect();
    let native = task_policy_source_from_current(&source, &pins, now)?;
    Ok((native, usage))
}

pub(super) fn floor_key(pin: &PolicyPin) -> Result<String> {
    Ok(key(
        "foreign-floor",
        &ObjectHash::body(&(&pin.domain, &pin.genesis_root))?,
    ))
}

pub(super) fn floors_in(connection: &Connection, pins: &[PolicyPin]) -> Result<Vec<PolicyPin>> {
    let mut floors = Vec::new();
    for pin in pins {
        if let Some(saved) = read_in::<PolicyPin>(connection, &floor_key(pin)?)? {
            ensure!(
                saved.domain == pin.domain && saved.genesis_root == pin.genesis_root,
                "foreign policy floor changed its approved domain or genesis"
            );
            floors.push(saved);
        }
    }
    Ok(floors)
}

fn advance_floors_in(
    connection: &Connection,
    pins: &[PolicyPin],
    heads: &[PolicyStamp],
) -> Result<()> {
    for head in heads {
        let pin = pins
            .iter()
            .find(|pin| pin.domain == head.domain)
            .context("verified foreign head has no independent pin")?;
        let name = floor_key(pin)?;
        let old: Option<PolicyPin> = read_in(connection, &name)?;
        if let Some(old) = &old {
            ensure!(
                old.domain == pin.domain && old.genesis_root == pin.genesis_root,
                "foreign floor changed its independent genesis"
            );
            if let Some(prior) = &old.minimum_head {
                ensure!(
                    (head.authority_epoch, head.sequence)
                        >= (prior.authority_epoch, prior.sequence),
                    "foreign policy tried to roll back a verified floor"
                );
                ensure!(
                    (head.authority_epoch, head.sequence)
                        != (prior.authority_epoch, prior.sequence)
                        || head == prior,
                    "foreign policy conflicts at the same verified floor"
                );
            }
        }
        let value = serde_json::to_string(&PolicyPin {
            domain: head.domain.clone(),
            genesis_root: pin.genesis_root.clone(),
            minimum_head: Some(head.clone()),
        })?;
        match old {
            Some(old) => {
                let count = connection.execute(
                    "UPDATE identity_meta SET value=? WHERE key=? AND value=?",
                    params![value, name, serde_json::to_string(&old)?],
                )?;
                ensure!(count == 1, "foreign policy floor changed during completion");
            }
            None => {
                connection.execute(
                    "INSERT INTO identity_meta(key,value) VALUES(?,?)",
                    params![name, value],
                )?;
            }
        }
    }
    Ok(())
}

fn import_record_in(connection: &Connection, reference: &ObjectHash) -> Result<ImportRecord> {
    read_in(connection, &key("import", reference))?.context("foreign policy import is missing")
}

fn request_in(
    connection: &Connection,
    record: &ImportRecord,
) -> Result<RemoteExecutionPolicyReadRequest> {
    let request: RemoteExecutionPolicyReadRequest =
        read_in(connection, &key("request", &record.request_ref))?
            .context("foreign policy original request is missing")?;
    ensure!(
        ObjectHash::body(&request)? == record.request_ref,
        "foreign policy original request changed"
    );
    Ok(request)
}

impl Store {
    pub(crate) fn foreign_execution_policy_begin(
        &mut self,
        identity: &LocalIdentity,
        principal: &str,
        params: &Value,
        now: u64,
    ) -> Result<Value> {
        let _signatures = ScopedSignatureVerification::new();
        ensure!(
            serde_json::to_vec(params)?.len() <= MAX_PROOF_BYTES,
            "foreign policy begin exceeds 64 KiB"
        );
        let input: Begin = serde_json::from_value(params.clone())?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let (source, mut usage) = current_source_in(
            &tx,
            &self.policy,
            identity,
            principal,
            &input.request.intent,
            now,
        )?;
        let trust = super::trust_for_read_in(
            &tx,
            &self.policy,
            identity,
            &input.request.intent,
            input.approval_ref.as_ref(),
            &mut usage,
        )?;
        let approval_ref = trust.approval().hash()?;
        let request_ref = ObjectHash::body(&input.request)?;
        let snapshot_ref = ObjectHash::body(&input.snapshot)?;
        let reference = ObjectHash::body(&(
            "task-policy-import-v2",
            &approval_ref,
            &request_ref,
            &snapshot_ref,
        ))?;
        let floors = floors_in(&tx, &input.request.intent.payload.execution_pins)?;
        let importer = begin_task_policy_import(
            &trust,
            &source,
            &input.request,
            &input.snapshot,
            &floors,
            PolicyCarrierOrigin::SignedTranscript,
            now,
        )?;
        usage.merge(importer.proof_usage());
        let fresh = CachedImport {
            importer,
            page_hashes: Vec::new(),
            ready_for_finish: false,
        };
        let cached = self
            .foreign_execution_policy
            .pending
            .get(&reference)
            .unwrap_or(&fresh);
        let output = response(params, progress(&reference, cached), usage)?;
        let initial = ImportRecord {
            approval_ref,
            request_ref,
            snapshot_ref,
            page_count: 0,
            ready_for_finish: false,
        };
        if let Some(saved) = read_in::<ImportRecord>(&tx, &key("import", &reference))? {
            ensure!(
                saved.approval_ref == initial.approval_ref
                    && saved.request_ref == initial.request_ref
                    && saved.snapshot_ref == initial.snapshot_ref,
                "foreign import changed its original carrier"
            );
        } else {
            save_exact_in(&tx, &key("import", &reference), &initial)?;
        }
        save_exact_in(&tx, &key("request", &initial.request_ref), &input.request)?;
        save_exact_in(
            &tx,
            &key("snapshot", &initial.snapshot_ref),
            &input.snapshot,
        )?;
        tx.commit()?;
        self.foreign_execution_policy
            .pending
            .entry(reference)
            .or_insert(fresh);
        Ok(output)
    }

    pub(crate) fn foreign_execution_policy_page(
        &mut self,
        identity: &LocalIdentity,
        principal: &str,
        params: &Value,
        now: u64,
    ) -> Result<Value> {
        let _signatures = ScopedSignatureVerification::new();
        ensure!(
            serde_json::to_vec(params)?.len() <= MAX_PROOF_BYTES,
            "foreign policy page exceeds 64 KiB"
        );
        let input: Page = serde_json::from_value(params.clone())?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let saved = import_record_in(&tx, &input.import_ref)?;
        let request = request_in(&tx, &saved)?;
        let (_, mut usage) =
            current_source_in(&tx, &self.policy, identity, principal, &request.intent, now)?;
        super::trust_for_read_in(
            &tx,
            &self.policy,
            identity,
            &request.intent,
            Some(&saved.approval_ref),
            &mut usage,
        )?;
        let cached = self
            .foreign_execution_policy
            .pending
            .get(&input.import_ref)
            .context("NeedPolicyResume: begin and replay each saved page after restart")?;
        let hash = ObjectHash::body(&input.page)?;
        let index = input.page.payload.page_index;
        if index < cached.page_hashes.len() as u64 {
            ensure!(
                cached.page_hashes.get(index as usize) == Some(&hash),
                "foreign policy retry changed its signed page"
            );
            return response(params, progress(&input.import_ref, cached), usage);
        }
        ensure!(
            index == cached.page_hashes.len() as u64,
            "foreign policy page is out of order"
        );
        let mut next = CachedImport {
            importer: cached.importer.clone(),
            page_hashes: cached.page_hashes.clone(),
            ready_for_finish: cached.ready_for_finish,
        };
        next.importer.append_page(&input.page, now)?;
        next.page_hashes.push(hash);
        next.ready_for_finish = input.page.payload.complete;
        usage.merge(next.importer.proof_usage());
        let output = response(params, progress(&input.import_ref, &next), usage)?;
        save_exact_in(&tx, &page_key(&input.import_ref, index), &input.page)?;
        if index >= saved.page_count {
            ensure!(
                index == saved.page_count && !saved.ready_for_finish,
                "foreign policy persisted cursor changed"
            );
            let mut changed = saved.clone();
            changed.page_count += 1;
            changed.ready_for_finish = next.ready_for_finish;
            ensure!(
                tx.execute(
                    "UPDATE identity_meta SET value=? WHERE key=? AND value=?",
                    params![
                        serde_json::to_string(&changed)?,
                        key("import", &input.import_ref),
                        serde_json::to_string(&saved)?
                    ]
                )? == 1,
                "foreign policy persisted cursor changed during append"
            );
        }
        tx.commit()?;
        self.foreign_execution_policy
            .pending
            .insert(input.import_ref, next);
        Ok(output)
    }

    pub(crate) fn foreign_execution_policy_finish(
        &mut self,
        identity: &LocalIdentity,
        principal: &str,
        params: &Value,
        now: u64,
    ) -> Result<Value> {
        let _signatures = ScopedSignatureVerification::new();
        ensure!(
            serde_json::to_vec(params)?.len() <= MAX_PROOF_BYTES,
            "foreign policy witness exceeds 64 KiB"
        );
        let input: Finish = serde_json::from_value(params.clone())?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let saved = import_record_in(&tx, &input.import_ref)?;
        let request = request_in(&tx, &saved)?;
        let (_, mut usage) =
            current_source_in(&tx, &self.policy, identity, principal, &request.intent, now)?;
        super::trust_for_read_in(
            &tx,
            &self.policy,
            identity,
            &request.intent,
            Some(&saved.approval_ref),
            &mut usage,
        )?;
        let cached = self
            .foreign_execution_policy
            .pending
            .get(&input.import_ref)
            .context("NeedPolicyResume: begin and replay each saved page after restart")?;
        ensure!(
            cached.ready_for_finish
                && saved.ready_for_finish
                && cached.page_hashes.len() as u64 == saved.page_count,
            "foreign policy has not consumed every original page"
        );
        let complete_policy =
            if request.intent.payload.purpose == RemoteExecutionPolicyPurpose::History {
                let policy: VerifiedTaskHistoryPolicy = cached
                    .importer
                    .clone()
                    .finish_history(&input.witness, now)?;
                usage.merge(policy.proof_usage());
                CompletedPolicy::History(policy)
            } else {
                let policy: VerifiedTaskCurrentPolicy = cached
                    .importer
                    .clone()
                    .finish_current(&input.witness, now)?;
                usage.merge(policy.proof_usage());
                CompletedPolicy::Current(policy)
            };
        let (pins, heads) = match &complete_policy {
            CompletedPolicy::Current(policy) => (policy.pins(), policy.foreign_heads()),
            CompletedPolicy::History(policy) => (policy.pins(), policy.foreign_heads()),
        };
        let witness_ref = ObjectHash::body(&input.witness)?;
        let complete = CompletionRecord {
            import_ref: input.import_ref.clone(),
            witness_ref,
            completed_at_ms: now,
        };
        let policy_ref = ObjectHash::body(&(
            "task-policy-completion-v2",
            &input.import_ref,
            &complete.witness_ref,
        ))?;
        let output = response(
            params,
            json!({"policy_ref":policy_ref,"approval_ref":saved.approval_ref,"purpose":request.intent.payload.purpose}),
            usage,
        )?;
        if let Some(old) = read_in::<CompletionRecord>(&tx, &key("completion", &policy_ref))? {
            ensure!(
                old.import_ref == complete.import_ref && old.witness_ref == complete.witness_ref,
                "foreign policy completion changed its original witness"
            );
        } else {
            save_exact_in(&tx, &key("completion", &policy_ref), &complete)?;
        }
        save_exact_in(&tx, &key("witness", &complete.witness_ref), &input.witness)?;
        let foreign_heads: Vec<_> = match &complete_policy {
            CompletedPolicy::Current(policy) => heads
                .iter()
                .filter(|head| {
                    !policy
                        .primary_heads()
                        .iter()
                        .any(|primary| primary.domain == head.domain)
                })
                .cloned()
                .collect(),
            CompletedPolicy::History(policy) => heads
                .iter()
                .filter(|head| {
                    !policy
                        .primary_heads()
                        .iter()
                        .any(|primary| primary.domain == head.domain)
                })
                .cloned()
                .collect(),
        };
        advance_floors_in(&tx, pins, &foreign_heads)?;
        save_exact_in(
            &tx,
            &key("completed-import", &input.import_ref),
            &policy_ref,
        )?;
        let task_key = match &complete_policy {
            CompletedPolicy::Current(policy) => Some(
                super::provenance::TaskExecutionPolicyProvenance::task_key(policy)?,
            ),
            CompletedPolicy::History(_) => None,
        };
        tx.commit()?;
        match complete_policy {
            CompletedPolicy::Current(policy) => {
                self.policy.task_execution_policy.insert_current(
                    task_key.context("current policy lost its Task index")?,
                    policy_ref,
                    policy,
                );
            }
            CompletedPolicy::History(policy) => {
                self.policy
                    .task_execution_policy
                    .history
                    .insert(policy_ref, policy);
            }
        }
        Ok(output)
    }
}
