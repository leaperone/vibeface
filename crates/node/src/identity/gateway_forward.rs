use super::journal::PolicyState;
use crate::{crypto::LocalIdentity, store::Store};
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::collections::{BTreeMap, BTreeSet};
use vibeface_secure::authorization::{
    AUTH_VERSION, AccountId, DeviceCertificate, DeviceId, DeviceUse, JournalEntry,
    MAX_POLICY_DOMAINS, MAX_PROOF_BYTES, Nonce, ObjectHash, PolicyDomain, PolicyPin, PublicKey,
    Signed, SpaceId, SpaceRoot,
    client_policy_import::{
        ClientPolicyAuthConfirmRequest, ClientPolicyAuthConfirmation, ClientPolicyPage,
        ClientPolicyReadRequest, ClientPolicySnapshot, ClientPolicyWitness,
    },
    execution_handoff::ExecutionProofUsage,
    gateway_forward::{
        GatewayClientPolicyImport, GatewayForwardHeader, GatewayOriginal, GatewayRouteApproval,
        GatewaySourceFrame, VerifiedGatewayClientSource, VerifiedGatewayRoute,
        begin_gateway_client_import, verify_gateway_original, verify_gateway_route,
        verify_gateway_route_approval,
    },
    policy_proof::{policy_entry_payload_usage, policy_entry_usage},
};
use vibeface_secure::signature_verification::{ScopedSignatureVerification, SignatureVerification};

const PREFIX: &str = "gateway-policy-v2";

#[derive(Clone, Default)]
pub(crate) struct GatewayPolicyCache {
    sources: BTreeMap<ObjectHash, CachedSource>,
}

#[derive(Clone)]
enum CachedSource {
    Pending(SourceRecord, GatewayClientPolicyImport),
    Complete(SourceRecord, VerifiedGatewayClientSource),
}

impl CachedSource {
    fn record(&self) -> &SourceRecord {
        match self {
            Self::Pending(record, _) | Self::Complete(record, _) => record,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct RouteRecord {
    approval: ObjectHash,
    generation: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct SourceRecord {
    route_id: Nonce,
    approval: ObjectHash,
    begin: ObjectHash,
    pages: u64,
    finish: Option<ObjectHash>,
}

#[derive(Debug, Serialize)]
pub(crate) struct GatewayRouteCommit {
    route_id: Nonce,
    approval: ObjectHash,
    generation: u64,
}

#[derive(Debug, Serialize)]
pub(crate) struct GatewayRouteStatus {
    route_id: Nonce,
    approval: ObjectHash,
    generation: u64,
    checked_at_ms: u64,
    #[serde(flatten)]
    result: GatewayRouteStatusResult,
}

#[derive(Debug, Serialize, PartialEq, Eq)]
#[serde(tag = "status", rename_all = "snake_case")]
enum GatewayRouteStatusResult {
    Installed,
    Superseded {
        current_approval: ObjectHash,
        current_generation: u64,
    },
    NotInstalled,
}

#[derive(Debug, Serialize)]
pub(crate) struct GatewaySourceProgress {
    reference: ObjectHash,
    pages: u64,
    complete: bool,
}

pub(crate) struct GatewayForwardRequest {
    pub(crate) header: Signed<GatewayForwardHeader>,
    pub(crate) target_public_key: PublicKey,
}

fn key(kind: &str, id: &impl std::fmt::Display) -> String {
    format!("{PREFIX}:{kind}:{id}")
}

fn encode<T: Serialize>(value: &T) -> Result<String> {
    let encoded = serde_json::to_string(value)?;
    ensure!(
        encoded.len() <= MAX_PROOF_BYTES,
        "gateway metadata exceeds its byte limit"
    );
    Ok(encoded)
}

fn read<T: DeserializeOwned>(connection: &Connection, name: &str) -> Result<Option<T>> {
    let row: Option<(usize, String)> = connection.query_row(
        "SELECT length(CAST(value AS BLOB)),CASE WHEN length(CAST(value AS BLOB))<=? THEN value ELSE '' END FROM identity_meta WHERE key=?",
        params![MAX_PROOF_BYTES, name], |row| Ok((row.get(0)?, row.get(1)?)),
    ).optional()?;
    row.map(|(length, value)| {
        ensure!(
            length <= MAX_PROOF_BYTES,
            "stored gateway metadata exceeds its byte limit"
        );
        serde_json::from_str(&value).context("invalid stored gateway metadata")
    })
    .transpose()
}

fn write<T: Serialize>(connection: &Connection, name: &str, value: &T) -> Result<()> {
    connection.execute(
        "INSERT INTO identity_meta(key,value) VALUES(?,?) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
        params![name, encode(value)?],
    )?;
    Ok(())
}

fn immutable<T: Serialize + DeserializeOwned + PartialEq>(
    connection: &Connection,
    name: &str,
    value: &T,
) -> Result<()> {
    if let Some(previous) = read::<T>(connection, name)? {
        ensure!(previous == *value, "gateway immutable original changed");
        return Ok(());
    }
    connection.execute(
        "INSERT INTO identity_meta(key,value) VALUES(?,?)",
        params![name, encode(value)?],
    )?;
    Ok(())
}

fn frame(connection: &Connection, reference: &ObjectHash) -> Result<GatewaySourceFrame> {
    let frame = read(connection, &key("frame", reference))?
        .context("gateway source original is missing")?;
    ensure!(
        ObjectHash::body(&frame)? == *reference,
        "gateway source original hash changed"
    );
    Ok(frame)
}

fn floor_key(pin: &PolicyPin) -> Result<String> {
    Ok(key(
        "floor",
        &ObjectHash::body(&(&pin.domain, &pin.genesis_root))?,
    ))
}

fn current_floors(connection: &Connection, approved: &[PolicyPin]) -> Result<Vec<PolicyPin>> {
    approved
        .iter()
        .map(|pin| {
            let mut current: PolicyPin =
                read(connection, &floor_key(pin)?)?.unwrap_or_else(|| pin.clone());
            ensure!(
                current.domain == pin.domain && current.genesis_root == pin.genesis_root,
                "gateway floor differs from approved roots"
            );
            if let Some(approved) = &pin.minimum_head {
                if let Some(saved) = &current.minimum_head {
                    let approved_order = (approved.authority_epoch, approved.sequence);
                    let saved_order = (saved.authority_epoch, saved.sequence);
                    ensure!(
                        approved_order != saved_order || approved == saved,
                        "gateway minimum heads conflict at the same generation"
                    );
                    if approved_order > saved_order {
                        current.minimum_head = Some(approved.clone());
                    }
                } else {
                    current.minimum_head = Some(approved.clone());
                }
            }
            Ok(current)
        })
        .collect()
}

fn local_account(primary: &PolicyState, local: &PublicKey) -> Result<AccountId> {
    let state = primary.verified();
    let device = DeviceId::from_key(local);
    primary
        .pins()
        .iter()
        .find_map(|pin| {
            let PolicyDomain::Account(account) = &pin.domain else {
                return None;
            };
            let document = state.account(account)?;
            (!document.revoked_devices.contains(&device)
                && document.devices.iter().any(|hash| {
                    state.device(hash).is_some_and(|certificate| {
                        certificate.payload.public_key == *local
                            && certificate.payload.device == device
                            && certificate.payload.owner == PolicyDomain::Account(account.clone())
                    })
                }))
            .then(|| account.clone())
        })
        .context("gateway route requires a claimed Account Node")
}

fn active_device<'a>(
    primary: &'a PolicyState,
    device: &DeviceId,
) -> Option<&'a Signed<DeviceCertificate>> {
    let state = primary.verified();
    primary.pins().iter().find_map(|pin| {
        let (devices, revoked) = match &pin.domain {
            PolicyDomain::Account(account) => {
                let document = state.account(account)?;
                (&document.devices, &document.revoked_devices)
            }
            PolicyDomain::Space(space) => {
                let document = state.space(space)?;
                (&document.devices, &document.revoked_devices)
            }
        };
        if revoked.contains(device) {
            return None;
        }
        devices.iter().find_map(|hash| {
            state.device(hash).filter(|certificate| {
                certificate.payload.device == *device && certificate.payload.owner == pin.domain
            })
        })
    })
}

fn verify_route_status_request(
    primary: &PolicyState,
    local: &PublicKey,
    actual_client: &PublicKey,
    approval: &Signed<GatewayRouteApproval>,
    now: u64,
) -> Result<ExecutionProofUsage> {
    let state = primary.verified();
    let account = local_account(primary, local)?;
    let space = SpaceId::personal(&account);
    let client =
        state.verify_account_client_device(&account, &DeviceId::from_key(actual_client), now)?;
    let service = state.verify_service_device(&space, &DeviceId::from_key(local), now)?;
    ensure!(
        service.payload.public_key == *local
            && service.payload.owner == PolicyDomain::Account(account.clone())
            && service.payload.uses.contains(&DeviceUse::ServiceInvocation),
        "gateway route status requires the current local Account service"
    );
    let history = state
        .account_history(&account)
        .context("gateway account history is missing")?;
    let signer = history
        .iter()
        .rev()
        .find(|document| document.payload.current_root == approval.issuer)
        .context("gateway route approval was not signed by a verified account root")?;
    approval.verify_signature()?;
    let route = &approval.payload;
    ensure!(
        route.version == AUTH_VERSION
            && route.gateway == *local
            && route.gateway != route.client_key
            && route.gateway != route.target_key
            && route.client_key != route.target_key
            && route.generation > 0
            && !route.allowed_actions.is_empty()
            && route
                .allowed_actions
                .windows(2)
                .all(|pair| pair[0] < pair[1])
            && route.issued_at_ms < route.expires_at_ms
            && route.issued_at_ms <= now
            && !route.source_pins.is_empty()
            && route.source_pins.len() <= MAX_POLICY_DOMAINS
            && route
                .source_pins
                .windows(2)
                .all(|pair| pair[0].domain < pair[1].domain)
            && route.source_pins.iter().all(|pin| pin
                .minimum_head
                .as_ref()
                .is_none_or(|head| head.domain == pin.domain)),
        "invalid gateway route status original"
    );
    let mut usage = ExecutionProofUsage::default();
    usage.record_signed(approval)?;
    for certificate in [client, service] {
        usage.merge(&policy_entry_usage(
            state,
            &JournalEntry::Device(certificate.clone()),
        )?);
    }
    usage.merge(&policy_entry_payload_usage(&JournalEntry::Account(
        signer.clone(),
    ))?);
    let mut pending = vec![PolicyDomain::Account(account), PolicyDomain::Space(space)];
    let mut seen = BTreeSet::new();
    while let Some(domain) = pending.pop() {
        if !seen.insert(domain.clone()) {
            continue;
        }
        ensure!(
            seen.len() <= MAX_POLICY_DOMAINS,
            "too many gateway status policy domains"
        );
        let authority = match &domain {
            PolicyDomain::Account(account) => {
                let document = state
                    .account_history(account)
                    .and_then(|history| history.last())
                    .context("gateway status account dependency is missing")?;
                usage.merge(&policy_entry_payload_usage(&JournalEntry::Account(
                    document.clone(),
                ))?);
                &document.payload.authority
            }
            PolicyDomain::Space(space) => {
                let document = state
                    .space_history(space)
                    .and_then(|history| history.last())
                    .context("gateway status space dependency is missing")?;
                if let SpaceRoot::Personal { owner, .. } = &document.payload.root {
                    pending.push(PolicyDomain::Account(owner.clone()));
                }
                usage.merge(&policy_entry_payload_usage(&JournalEntry::Space(
                    document.clone(),
                ))?);
                &document.payload.authority
            }
        };
        if let Some(certificate) = active_device(primary, &authority.device) {
            usage.merge(&policy_entry_usage(
                state,
                &JournalEntry::Device(certificate.clone()),
            )?);
            pending.push(certificate.payload.owner.clone());
        }
    }
    usage.enforce_limits()?;
    Ok(usage)
}

fn route_in(
    connection: &Connection,
    primary: &PolicyState,
    local: &PublicKey,
    route_id: &Nonce,
    now: u64,
) -> Result<VerifiedGatewayRoute> {
    let record: RouteRecord =
        read(connection, &key("route", route_id))?.context("gateway route is not approved")?;
    let approval: Signed<GatewayRouteApproval> =
        read(connection, &key("approval", &record.approval))?
            .context("gateway route approval is missing")?;
    ensure!(
        approval.hash()? == record.approval && approval.payload.route_id == *route_id,
        "gateway route differs from its original approval"
    );
    Ok(verify_gateway_route(
        primary.verified(),
        &local_account(primary, local)?,
        local,
        &approval,
        record.generation,
        &current_floors(connection, &approval.payload.source_pins)?,
        now,
    )?)
}

fn source_in(connection: &Connection, reference: &ObjectHash) -> Result<SourceRecord> {
    let record: SourceRecord =
        read(connection, &key("source", reference))?.context("gateway source is missing")?;
    ensure!(
        ObjectHash::body(&(&record.approval, &record.begin))? == *reference,
        "gateway source reference changed"
    );
    Ok(record)
}

fn current_source<'a>(
    connection: &Connection,
    cache: &'a GatewayPolicyCache,
    reference: &ObjectHash,
) -> Result<&'a CachedSource> {
    let record = source_in(connection, reference)?;
    let source = cache
        .sources
        .get(reference)
        .context("NeedPolicySync: gateway source requires explicit rehydration")?;
    ensure!(
        *source.record() == record,
        "NeedPolicySync: gateway source originals changed"
    );
    Ok(source)
}

fn progress(reference: ObjectHash, record: &SourceRecord) -> GatewaySourceProgress {
    GatewaySourceProgress {
        reference,
        pages: record.pages,
        complete: record.finish.is_some(),
    }
}

fn check_client(route: &VerifiedGatewayRoute, actual_client: &PublicKey) -> Result<()> {
    ensure!(
        route.approval().payload.client_key == *actual_client,
        "gateway source belongs to another Noise client"
    );
    Ok(())
}

impl Store {
    pub(crate) fn gateway_route_status(
        &mut self,
        identity: &LocalIdentity,
        actual_client: &PublicKey,
        approval: &Signed<GatewayRouteApproval>,
        now: u64,
    ) -> Result<GatewayRouteStatus> {
        let _signatures = ScopedSignatureVerification::new();
        encode(approval)?;
        let local = PublicKey::parse(&identity.public_key_hex()?)?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Deferred)?;
        let primary = PolicyState::reload_verified(&tx, &self.policy)?;
        let mut usage =
            verify_route_status_request(&primary, &local, actual_client, approval, now)?;
        let hash = approval.hash()?;
        let result = match read::<RouteRecord>(&tx, &key("route", &approval.payload.route_id))? {
            Some(record) => {
                let saved: Signed<GatewayRouteApproval> =
                    read(&tx, &key("approval", &record.approval))?
                        .context("gateway route original is missing")?;
                ensure!(
                    saved.hash()? == record.approval
                        && saved.payload.route_id == approval.payload.route_id
                        && saved.payload.generation == record.generation
                        && saved.payload.gateway == local,
                    "gateway route status differs from its stored original"
                );
                let account = local_account(&primary, &local)?;
                let signer = primary
                    .verified()
                    .account_history(&account)
                    .and_then(|history| {
                        history
                            .iter()
                            .find(|document| document.payload.current_root == saved.issuer)
                    })
                    .context("stored gateway route has no verified account root")?;
                saved.verify_signature()?;
                usage.record_signed(&saved)?;
                usage.merge(&policy_entry_payload_usage(&JournalEntry::Account(
                    signer.clone(),
                ))?);
                if record.approval == hash {
                    ensure!(saved == *approval, "gateway route status original changed");
                    GatewayRouteStatusResult::Installed
                } else if record.generation >= approval.payload.generation {
                    GatewayRouteStatusResult::Superseded {
                        current_approval: record.approval,
                        current_generation: record.generation,
                    }
                } else {
                    GatewayRouteStatusResult::NotInstalled
                }
            }
            None => GatewayRouteStatusResult::NotInstalled,
        };
        let status = GatewayRouteStatus {
            route_id: approval.payload.route_id.clone(),
            approval: hash,
            generation: approval.payload.generation,
            checked_at_ms: now,
            result,
        };
        usage.record_metadata(&status)?;
        if let Some(counts) = SignatureVerification::current_counts() {
            usage.record_actual_signature_counts(counts);
        }
        usage.enforce_limits()?;
        tx.commit()?;
        Ok(status)
    }

    pub(crate) fn install_gateway_route(
        &mut self,
        identity: &LocalIdentity,
        approval: &Signed<GatewayRouteApproval>,
        now: u64,
    ) -> Result<GatewayRouteCommit> {
        let _signatures = ScopedSignatureVerification::new();
        encode(approval)?;
        let local = PublicKey::parse(&identity.public_key_hex()?)?;
        let hash = approval.hash()?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut primary = PolicyState::reload_verified(&tx, &self.policy)?;
        let previous: Option<RouteRecord> = read(&tx, &key("route", &approval.payload.route_id))?;
        if previous
            .as_ref()
            .is_some_and(|record| record.approval == hash)
        {
            let original: Signed<GatewayRouteApproval> =
                read(&tx, &key("approval", &hash))?.context("gateway route original is missing")?;
            ensure!(
                original == *approval,
                "gateway route retry changed its original"
            );
            tx.commit()?;
            return Ok(GatewayRouteCommit {
                route_id: approval.payload.route_id.clone(),
                approval: hash,
                generation: approval.payload.generation,
            });
        }
        let generation = previous.map_or(Ok(1), |record| {
            record
                .generation
                .checked_add(1)
                .context("gateway route generation overflow")
        })?;
        let floors = current_floors(&tx, &approval.payload.source_pins)?;
        verify_gateway_route_approval(
            primary.verified(),
            &local_account(&primary, &local)?,
            &local,
            approval,
            generation,
            &floors,
            now,
        )?;
        immutable(&tx, &key("approval", &hash), approval)?;
        for pin in &floors {
            write(&tx, &floor_key(pin)?, pin)?;
        }
        write(
            &tx,
            &key("route", &approval.payload.route_id),
            &RouteRecord {
                approval: hash.clone(),
                generation,
            },
        )?;
        tx.commit()?;
        primary
            .gateway_policy
            .sources
            .retain(|_, source| source.record().route_id != approval.payload.route_id);
        self.policy = primary;
        Ok(GatewayRouteCommit {
            route_id: approval.payload.route_id.clone(),
            approval: hash,
            generation,
        })
    }

    pub(crate) fn begin_gateway_source(
        &mut self,
        identity: &LocalIdentity,
        actual_client: &PublicKey,
        route_id: &Nonce,
        request: Signed<ClientPolicyReadRequest>,
        snapshot: Signed<ClientPolicySnapshot>,
        now: u64,
    ) -> Result<GatewaySourceProgress> {
        let _signatures = ScopedSignatureVerification::new();
        let original = GatewaySourceFrame::Begin {
            request: request.clone(),
            snapshot: snapshot.clone(),
        };
        encode(&original)?;
        let local = PublicKey::parse(&identity.public_key_hex()?)?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut primary = PolicyState::reload_verified(&tx, &self.policy)?;
        let route = route_in(&tx, &primary, &local, route_id, now)?;
        let importer = begin_gateway_client_import(
            &route,
            actual_client,
            request.clone(),
            snapshot.clone(),
            now,
        )?;
        let begin = ObjectHash::body(&original)?;
        let approval = route.approval_hash()?;
        let reference = ObjectHash::body(&(&approval, &begin))?;
        if let Some(previous) = read::<SourceRecord>(&tx, &key("source", &reference))? {
            ensure!(
                previous.route_id == *route_id
                    && previous.approval == approval
                    && previous.begin == begin,
                "gateway begin retry changed its source"
            );
            ensure!(
                frame(&tx, &begin)? == original,
                "gateway begin original changed"
            );
            tx.commit()?;
            return Ok(progress(reference, &previous));
        }
        let record = SourceRecord {
            route_id: route_id.clone(),
            approval,
            begin: begin.clone(),
            pages: 0,
            finish: None,
        };
        immutable(&tx, &key("frame", &begin), &original)?;
        write(&tx, &key("source", &reference), &record)?;
        tx.commit()?;
        primary.gateway_policy.sources.insert(
            reference.clone(),
            CachedSource::Pending(record.clone(), importer),
        );
        self.policy = primary;
        Ok(progress(reference, &record))
    }

    pub(crate) fn append_gateway_source(
        &mut self,
        identity: &LocalIdentity,
        actual_client: &PublicKey,
        reference: &ObjectHash,
        page: Signed<ClientPolicyPage>,
        now: u64,
    ) -> Result<GatewaySourceProgress> {
        let _signatures = ScopedSignatureVerification::new();
        let local = PublicKey::parse(&identity.public_key_hex()?)?;
        let original = GatewaySourceFrame::Page { page: page.clone() };
        encode(&original)?;
        let hash = ObjectHash::body(&original)?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut primary = PolicyState::reload_verified(&tx, &self.policy)?;
        let saved = current_source(&tx, &primary.gateway_policy, reference)?.clone();
        let route = route_in(&tx, &primary, &local, &saved.record().route_id, now)?;
        check_client(&route, actual_client)?;
        ensure!(
            route.approval_hash()? == saved.record().approval,
            "gateway source route was replaced"
        );
        let page_key = key("page", &format!("{reference}:{}", page.payload.page_index));
        if page.payload.page_index < saved.record().pages {
            ensure!(
                read::<ObjectHash>(&tx, &page_key)? == Some(hash.clone())
                    && frame(&tx, &hash)? == original,
                "gateway page retry changed its original"
            );
            tx.commit()?;
            return Ok(progress(reference.clone(), saved.record()));
        }
        let CachedSource::Pending(mut record, mut importer) = saved else {
            anyhow::bail!("gateway source is already complete");
        };
        ensure!(
            page.payload.page_index == record.pages,
            "gateway page is out of order"
        );
        importer.append_page(page, now)?;
        record.pages = record
            .pages
            .checked_add(1)
            .context("gateway page count overflow")?;
        immutable(&tx, &key("frame", &hash), &original)?;
        immutable(&tx, &page_key, &hash)?;
        write(&tx, &key("source", reference), &record)?;
        tx.commit()?;
        primary.gateway_policy.sources.insert(
            reference.clone(),
            CachedSource::Pending(record.clone(), importer),
        );
        self.policy = primary;
        Ok(progress(reference.clone(), &record))
    }

    pub(crate) fn finish_gateway_source(
        &mut self,
        identity: &LocalIdentity,
        actual_client: &PublicKey,
        reference: &ObjectHash,
        witness: Signed<ClientPolicyWitness>,
        confirm_request: Signed<ClientPolicyAuthConfirmRequest>,
        confirmation: Signed<ClientPolicyAuthConfirmation>,
        now: u64,
    ) -> Result<GatewaySourceProgress> {
        let _signatures = ScopedSignatureVerification::new();
        let local = PublicKey::parse(&identity.public_key_hex()?)?;
        let original = GatewaySourceFrame::Finish {
            witness: witness.clone(),
            confirm_request: confirm_request.clone(),
            confirmation: confirmation.clone(),
        };
        encode(&original)?;
        let hash = ObjectHash::body(&original)?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut primary = PolicyState::reload_verified(&tx, &self.policy)?;
        let saved = current_source(&tx, &primary.gateway_policy, reference)?.clone();
        let route = route_in(&tx, &primary, &local, &saved.record().route_id, now)?;
        check_client(&route, actual_client)?;
        ensure!(
            route.approval_hash()? == saved.record().approval,
            "gateway source route was replaced"
        );
        let (mut record, importer) = match saved {
            CachedSource::Complete(record, source) => {
                ensure!(
                    record.finish == Some(hash.clone()) && frame(&tx, &hash)? == original,
                    "gateway finish retry changed its original"
                );
                source.check_route(&route, now)?;
                tx.commit()?;
                return Ok(progress(reference.clone(), &record));
            }
            CachedSource::Pending(record, importer) => (record, importer),
        };
        let source = importer.finish(witness, confirm_request, confirmation, now)?;
        source.check_route(&route, now)?;
        for pin in source.floors() {
            write(&tx, &floor_key(pin)?, pin)?;
        }
        record.finish = Some(hash.clone());
        immutable(&tx, &key("frame", &hash), &original)?;
        write(&tx, &key("source", reference), &record)?;
        tx.commit()?;
        primary.gateway_policy.sources.insert(
            reference.clone(),
            CachedSource::Complete(record.clone(), source),
        );
        self.policy = primary;
        Ok(progress(reference.clone(), &record))
    }

    pub(crate) fn prepare_gateway_forward(
        &self,
        identity: &LocalIdentity,
        actual_client: &PublicKey,
        reference: &ObjectHash,
        original: &GatewayOriginal,
        now: u64,
    ) -> Result<GatewayForwardRequest> {
        let _signatures = ScopedSignatureVerification::new();
        let local = PublicKey::parse(&identity.public_key_hex()?)?;
        let signer = super::client_policy_read::signer(identity)?;
        let tx = self.connection.unchecked_transaction()?;
        let primary = PolicyState::reload_verified(&tx, &self.policy)?;
        let CachedSource::Complete(record, source) =
            current_source(&tx, &primary.gateway_policy, reference)?
        else {
            anyhow::bail!("gateway source is incomplete");
        };
        let route = route_in(&tx, &primary, &local, &record.route_id, now)?;
        ensure!(
            route.approval_hash()? == record.approval,
            "gateway source route was replaced"
        );
        let verified = verify_gateway_original(&route, source, original, actual_client, now)?;
        let header = verified.sign_header(original, &signer, now)?;
        tx.commit()?;
        Ok(GatewayForwardRequest {
            header,
            target_public_key: route.approval().payload.target_key.clone(),
        })
    }
}

pub(crate) fn rehydrate_gateway_sources_in(
    connection: &Connection,
    primary: &mut PolicyState,
    local: &PublicKey,
    now: u64,
) -> Result<()> {
    let mut query =
        connection.prepare("SELECT key FROM identity_meta WHERE key LIKE ? ORDER BY key")?;
    let names = query
        .query_map([format!("{PREFIX}:source:%")], |row| {
            row.get::<_, String>(0)
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    for name in names {
        let reference = ObjectHash::parse(
            name.strip_prefix(&format!("{PREFIX}:source:"))
                .context("invalid gateway source key")?,
        )?;
        let record = source_in(connection, &reference)?;
        let route = match route_in(connection, primary, local, &record.route_id, now) {
            Ok(route) if route.approval_hash()? == record.approval => route,
            _ => continue,
        };
        let GatewaySourceFrame::Begin { request, snapshot } = frame(connection, &record.begin)?
        else {
            anyhow::bail!("gateway source does not start with its original begin");
        };
        if request.payload.expires_at_ms <= now || snapshot.payload.expires_at_ms <= now {
            continue;
        }
        let restored = (|| -> Result<CachedSource> {
            let mut importer = begin_gateway_client_import(
                &route,
                &route.approval().payload.client_key,
                request,
                snapshot,
                now,
            )?;
            for index in 0..record.pages {
                let hash: ObjectHash =
                    read(connection, &key("page", &format!("{reference}:{index}")))?
                        .context("gateway page reference is missing")?;
                let GatewaySourceFrame::Page { page } = frame(connection, &hash)? else {
                    anyhow::bail!("gateway source page has another purpose");
                };
                ensure!(
                    page.payload.page_index == index,
                    "gateway stored page order changed"
                );
                importer.append_page(page, now)?;
            }
            if let Some(hash) = &record.finish {
                let GatewaySourceFrame::Finish {
                    witness,
                    confirm_request,
                    confirmation,
                } = frame(connection, hash)?
                else {
                    anyhow::bail!("gateway source finish has another purpose");
                };
                let source = importer.finish(witness, confirm_request, confirmation, now)?;
                source.check_route(&route, now)?;
                Ok(CachedSource::Complete(record.clone(), source))
            } else {
                Ok(CachedSource::Pending(record.clone(), importer))
            }
        })();
        if let Ok(source) = restored {
            primary.gateway_policy.sources.insert(reference, source);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::identity::test_support::{TestNetwork, now};
    use serde_json::json;
    use vibeface_secure::authorization::{
        AUTH_VERSION, Action, Actor, Audience, Invocation, InvocationContext, ResourceRef,
        ServiceId,
        client_policy_import::{
            AuthorizationSelection, ClientPolicyContextReference, ClientPolicyPageRequest,
        },
        gateway_forward::GatewayRouteState,
    };

    struct SourceMaterial {
        request: Signed<ClientPolicyReadRequest>,
        snapshot: Signed<ClientPolicySnapshot>,
        pages: Vec<Signed<ClientPolicyPage>>,
        witness: Signed<ClientPolicyWitness>,
        confirm: Signed<ClientPolicyAuthConfirmRequest>,
        confirmation: Signed<ClientPolicyAuthConfirmation>,
    }

    fn route_approval(a: &TestNetwork, b: &TestNetwork) -> Result<Signed<GatewayRouteApproval>> {
        Ok(Signed::sign(
            GatewayRouteApproval {
                version: AUTH_VERSION,
                route_id: Nonce::random()?,
                generation: 1,
                gateway: PublicKey::parse(&a.node.public_key_hex()?)?,
                client_account: b.account.clone(),
                client_key: PublicKey::of(&b.client),
                target_key: PublicKey::parse(&b.node.public_key_hex()?)?,
                resource: ResourceRef {
                    space: b.space.clone(),
                    resource: b.resource.clone(),
                },
                allowed_actions: vec![Action::MessageAppend],
                source_pins: b.pins.clone(),
                state: GatewayRouteState::Active,
                issued_at_ms: now(),
                expires_at_ms: now() + 90_000,
            },
            &a.root,
        )?)
    }

    #[test]
    fn gateway_route_status_distinguishes_originals_without_mutation() -> Result<()> {
        let a = TestNetwork::new()?;
        let b = TestNetwork::new()?;
        let mut store = a.memory()?;
        let original = route_approval(&a, &b)?;
        let client = PublicKey::of(&a.client);
        let hash = original.hash()?;
        let changes = store.connection.total_changes();
        let missing = store.gateway_route_status(&a.node, &client, &original, now())?;
        assert_eq!(
            serde_json::to_value(missing)?,
            json!({
                "route_id":original.payload.route_id, "approval":hash, "generation":1,
                "checked_at_ms":now(), "status":"not_installed"
            })
        );
        let expired = original.payload.expires_at_ms;
        assert!(
            store
                .install_gateway_route(&a.node, &original, expired)
                .is_err()
        );
        assert_eq!(
            store
                .gateway_route_status(&a.node, &client, &original, expired)?
                .result,
            GatewayRouteStatusResult::NotInstalled
        );
        assert_eq!(store.connection.total_changes(), changes);
        store.install_gateway_route(&a.node, &original, now())?;
        let changes = store.connection.total_changes();
        assert_eq!(
            serde_json::to_value(
                store.gateway_route_status(&a.node, &client, &original, expired)?
            )?,
            json!({
                "route_id":original.payload.route_id, "approval":hash, "generation":1,
                "checked_at_ms":expired, "status":"installed"
            })
        );
        let mut competing = original.payload.clone();
        competing.expires_at_ms += 1;
        let competing = Signed::sign(competing, &a.root)?;
        assert!(
            store
                .install_gateway_route(&a.node, &competing, now())
                .is_err()
        );
        assert_eq!(
            serde_json::to_value(store.gateway_route_status(
                &a.node,
                &client,
                &competing,
                now()
            )?)?,
            json!({
                "route_id":competing.payload.route_id, "approval":competing.hash()?, "generation":1,
                "checked_at_ms":now(), "status":"superseded",
                "current_approval":hash, "current_generation":1
            })
        );
        let mut next = original.payload.clone();
        next.generation = 2;
        next.state = GatewayRouteState::Revoked;
        let next = Signed::sign(next, &a.root)?;
        assert_eq!(
            store
                .gateway_route_status(&a.node, &client, &next, now())?
                .result,
            GatewayRouteStatusResult::NotInstalled
        );
        assert_eq!(store.connection.total_changes(), changes);
        store.install_gateway_route(&a.node, &next, now())?;
        let changes = store.connection.total_changes();
        assert_eq!(
            store
                .gateway_route_status(&a.node, &client, &next, expired)?
                .result,
            GatewayRouteStatusResult::Installed
        );
        assert_eq!(
            store
                .gateway_route_status(&a.node, &client, &original, now())?
                .result,
            GatewayRouteStatusResult::Superseded {
                current_approval: next.hash()?,
                current_generation: 2,
            }
        );
        assert_eq!(store.connection.total_changes(), changes);
        assert!(store.policy.gateway_policy.sources.is_empty());
        Ok(())
    }

    #[test]
    fn gateway_route_status_rejects_other_signers_clients_and_invalid_carriers() -> Result<()> {
        let a = TestNetwork::new()?;
        let b = TestNetwork::new()?;
        let mut store = a.memory()?;
        let original = route_approval(&a, &b)?;
        let client = PublicKey::of(&a.client);
        let foreign = Signed::sign(original.payload.clone(), &b.root)?;
        assert!(
            store
                .gateway_route_status(&a.node, &client, &foreign, now())
                .is_err()
        );
        assert!(
            store
                .gateway_route_status(&a.node, &PublicKey::of(&b.client), &original, now())
                .is_err()
        );
        assert!(
            store
                .gateway_route_status(&b.node, &client, &original, now())
                .is_err()
        );
        let mut forged = original.clone();
        forged.payload.expires_at_ms += 1;
        assert!(
            store
                .gateway_route_status(&a.node, &client, &forged, now())
                .is_err()
        );
        let mut future = original.payload.clone();
        future.issued_at_ms = now() + 1;
        let future = Signed::sign(future, &a.root)?;
        assert!(
            store
                .gateway_route_status(&a.node, &client, &future, now())
                .is_err()
        );
        let mut oversized = original.payload.clone();
        oversized.allowed_actions = vec![Action::MessageAppend; MAX_PROOF_BYTES];
        let oversized = Signed::sign(oversized, &a.root)?;
        assert!(
            store
                .gateway_route_status(&a.node, &client, &oversized, now())
                .is_err()
        );
        let request = json!({"stage":"route_status", "approval":original});
        assert!(matches!(
            serde_json::from_value::<crate::runtime::GatewaySourceRequest>(request.clone())?,
            crate::runtime::GatewaySourceRequest::RouteStatus { approval } if approval == original
        ));
        let mut unexpected = request;
        unexpected["route_id"] = json!(original.payload.route_id);
        assert!(
            serde_json::from_value::<crate::runtime::GatewaySourceRequest>(unexpected).is_err()
        );
        let cert = store.policy.verified().verify_account_client_device(
            &a.account,
            &DeviceId::from_key(&client),
            now(),
        )?;
        let expired = cert.payload.expires_at_ms;
        assert!(
            store
                .gateway_route_status(&a.node, &client, &original, expired)
                .is_err()
        );
        a.revoke_client(&mut store)?;
        assert!(
            store
                .gateway_route_status(&a.node, &client, &original, now() + 1)
                .is_err()
        );
        let mut revoked_service = a.memory()?;
        a.revoke_node(&mut revoked_service)?;
        assert!(
            revoked_service
                .gateway_route_status(&a.node, &client, &original, now() + 1)
                .is_err()
        );
        Ok(())
    }

    #[test]
    fn gateway_route_status_keeps_old_root_queries_readonly_after_rotation() -> Result<()> {
        let a = TestNetwork::new()?;
        let b = TestNetwork::new()?;
        let path = a.directory.join("gateway-route-status.sqlite3");
        let mut store = a.at(&path)?;
        let original = route_approval(&a, &b)?;
        let missing = route_approval(&a, &b)?;
        store.install_gateway_route(&a.node, &original, now())?;
        let next_root = vibeface_secure::Identity::generate().map_err(anyhow::Error::msg)?;
        let mut account = store
            .policy
            .verified()
            .account(&a.account)
            .context("account missing")?
            .clone();
        account.previous = store
            .policy
            .verified()
            .head(&PolicyDomain::Account(a.account.clone()))
            .cloned();
        account.sequence += 1;
        account.root_generation += 1;
        account.current_root = PublicKey::of(&next_root);
        account.accept_root(&next_root)?;
        let mut space = store
            .policy
            .verified()
            .space(&a.space)
            .context("space missing")?
            .clone();
        space.previous = store
            .policy
            .verified()
            .head(&PolicyDomain::Space(a.space.clone()))
            .cloned();
        space.sequence += 1;
        space.root_generation += 1;
        space.current_root = PublicKey::of(&next_root);
        store.install_policy_journal(
            &[
                JournalEntry::Account(Signed::sign(account, &a.root)?),
                JournalEntry::Space(Signed::sign(space, &next_root)?),
            ],
            now() + 1,
        )?;
        drop(store);
        let mut store = Store::open(&path)?;
        let client = PublicKey::of(&a.client);
        assert!(
            store
                .install_gateway_route(&a.node, &missing, now() + 1)
                .is_err()
        );
        let changes = store.connection.total_changes();
        assert_eq!(
            store
                .gateway_route_status(&a.node, &client, &original, now() + 1)?
                .result,
            GatewayRouteStatusResult::Installed
        );
        let expired = missing.payload.expires_at_ms;
        assert_eq!(
            store
                .gateway_route_status(&a.node, &client, &missing, expired)?
                .result,
            GatewayRouteStatusResult::NotInstalled
        );
        assert_eq!(store.connection.total_changes(), changes);
        assert_eq!(
            store
                .policy
                .verified()
                .account(&a.account)
                .context("account missing")?
                .current_root,
            PublicKey::of(&next_root)
        );
        Ok(())
    }

    #[test]
    fn gateway_route_status_rejects_corrupt_saved_generation() -> Result<()> {
        let a = TestNetwork::new()?;
        let b = TestNetwork::new()?;
        let mut store = a.memory()?;
        let original = route_approval(&a, &b)?;
        store.install_gateway_route(&a.node, &original, now())?;
        write(
            &store.connection,
            &key("route", &original.payload.route_id),
            &RouteRecord {
                approval: original.hash()?,
                generation: 2,
            },
        )?;
        assert!(
            store
                .gateway_route_status(&a.node, &PublicKey::of(&a.client), &original, now())
                .is_err()
        );
        Ok(())
    }

    fn source_material(network: &TestNetwork, store: &mut Store) -> Result<SourceMaterial> {
        let node = PublicKey::parse(&network.node.public_key_hex()?)?;
        let selection = AuthorizationSelection {
            resource: ResourceRef {
                space: network.space.clone(),
                resource: network.resource.clone(),
            },
            audience: Audience {
                device: DeviceId::from_key(&node),
                service: ServiceId::from_key(&node),
            },
            grant_leaf: network.bundle.grant_leaf.clone(),
        };
        let request = Signed::sign(
            ClientPolicyReadRequest {
                version: AUTH_VERSION,
                account: network.account.clone(),
                space: network.space.clone(),
                device_public_key: PublicKey::of(&network.client),
                node_public_key: node,
                saved_pins: network.pins.clone(),
                selection: Some(selection.clone()),
                nonce: Nonce::random()?,
                issued_at_ms: now(),
                expires_at_ms: now() + 90_000,
            },
            &network.client,
        )?;
        let reply = store.begin_client_policy_read(
            &network.node,
            &network.client.public_key_hex(),
            request.clone(),
            now(),
        )?;
        let snapshot: Signed<ClientPolicySnapshot> =
            serde_json::from_value(reply["snapshot"].clone())?;
        let mut cursor = None;
        let mut pages = Vec::new();
        loop {
            let params = serde_json::to_value(ClientPolicyPageRequest {
                snapshot_id: snapshot.payload.snapshot_id.clone(),
                request_hash: snapshot.payload.request_hash.clone(),
                cursor,
            })?;
            let reply = store.client_policy_page(
                &network.node,
                &network.client.public_key_hex(),
                &params,
                now(),
            )?;
            let page: Signed<ClientPolicyPage> = serde_json::from_value(reply["page"].clone())?;
            let complete = page.payload.complete;
            cursor = page.payload.next_cursor.clone();
            pages.push(page);
            if complete {
                break;
            }
        }
        let params = serde_json::to_value(ClientPolicyPageRequest {
            snapshot_id: snapshot.payload.snapshot_id.clone(),
            request_hash: snapshot.payload.request_hash.clone(),
            cursor: None,
        })?;
        let reply = store.client_policy_witness(
            &network.node,
            &network.client.public_key_hex(),
            &params,
            now(),
        )?;
        let witness: Signed<ClientPolicyWitness> =
            serde_json::from_value(reply["witness"].clone())?;
        let confirm = Signed::sign(
            ClientPolicyAuthConfirmRequest {
                version: AUTH_VERSION,
                context: ClientPolicyContextReference {
                    snapshot_id: snapshot.payload.snapshot_id.clone(),
                    request_hash: snapshot.payload.request_hash.clone(),
                    snapshot_hash: snapshot.hash()?,
                    witness_hash: witness.hash()?,
                },
                selection,
                target_heads: snapshot.payload.target_heads.clone(),
                device_public_key: PublicKey::of(&network.client),
                issued_at_ms: now(),
                expires_at_ms: now() + 90_000,
            },
            &network.client,
        )?;
        let reply = store.confirm_client_policy_auth(
            &network.node,
            &network.client.public_key_hex(),
            &serde_json::to_value(&confirm)?,
            now(),
        )?;
        let confirmation = serde_json::from_value(reply["confirmation"].clone())?;
        Ok(SourceMaterial {
            request,
            snapshot,
            pages,
            witness,
            confirm,
            confirmation,
        })
    }

    #[test]
    fn gateway_sources_commit_atomically_restore_and_stop_after_route_revocation() -> Result<()> {
        let a = TestNetwork::new()?;
        let b = TestNetwork::new()?;
        let path = a.directory.join("gateway.sqlite3");
        let mut store = a.at(&path)?;
        let original_pins = store.trusted_policy_pins().to_vec();
        let mut remote = b.memory()?;
        let material = source_material(&b, &mut remote)?;
        let client = PublicKey::of(&b.client);
        let mut source_pins = b.pins.clone();
        for pin in &mut source_pins {
            pin.minimum_head = None;
        }
        let mut approval = Signed::sign(
            GatewayRouteApproval {
                version: AUTH_VERSION,
                route_id: Nonce::random()?,
                generation: 1,
                gateway: PublicKey::parse(&a.node.public_key_hex()?)?,
                client_account: b.account.clone(),
                client_key: client.clone(),
                target_key: PublicKey::parse(&b.node.public_key_hex()?)?,
                resource: ResourceRef {
                    space: b.space.clone(),
                    resource: b.resource.clone(),
                },
                allowed_actions: vec![Action::MessageAppend],
                source_pins,
                state: GatewayRouteState::Active,
                issued_at_ms: now(),
                expires_at_ms: now() + 90_000,
            },
            &a.root,
        )?;
        let committed = store.install_gateway_route(&a.node, &approval, now())?;
        assert_eq!(committed.approval, approval.hash()?);
        assert_eq!(
            store
                .install_gateway_route(&a.node, &approval, now())?
                .generation,
            1
        );
        for pin in &approval.payload.source_pins {
            assert_eq!(
                read::<PolicyPin>(&store.connection, &floor_key(pin)?)?,
                Some(pin.clone())
            );
        }
        let mut stricter = approval.payload.clone();
        stricter.generation = 2;
        for pin in &mut stricter.source_pins {
            pin.minimum_head = Some(
                material
                    .snapshot
                    .payload
                    .target_heads
                    .iter()
                    .find(|head| head.domain == pin.domain)
                    .context("source snapshot lacks a route domain")?
                    .clone(),
            );
        }
        assert!(
            stricter
                .source_pins
                .iter()
                .zip(&approval.payload.source_pins)
                .all(|(new, old)| new.minimum_head != old.minimum_head)
        );
        {
            let mut first_strict = stricter.clone();
            first_strict.generation = 1;
            let mut fresh = a.at(&a.directory.join("gateway-minimum.sqlite3"))?;
            let first_strict = Signed::sign(first_strict, &a.root)?;
            fresh.install_gateway_route(&a.node, &first_strict, now())?;
            for pin in &first_strict.payload.source_pins {
                assert_eq!(
                    read::<PolicyPin>(&fresh.connection, &floor_key(pin)?)?,
                    Some(pin.clone())
                );
            }
        }
        approval = Signed::sign(stricter, &a.root)?;
        assert_eq!(
            store
                .install_gateway_route(&a.node, &approval, now())?
                .generation,
            2
        );
        for pin in &approval.payload.source_pins {
            assert_eq!(
                read::<PolicyPin>(&store.connection, &floor_key(pin)?)?,
                Some(pin.clone())
            );
        }
        let begun = store.begin_gateway_source(
            &a.node,
            &client,
            &approval.payload.route_id,
            material.request.clone(),
            material.snapshot.clone(),
            now(),
        )?;
        assert!(!begun.complete);
        assert!(
            store
                .append_gateway_source(
                    &a.node,
                    &PublicKey::of(&a.client),
                    &begun.reference,
                    material.pages[0].clone(),
                    now()
                )
                .is_err()
        );
        store.connection.execute_batch(
            "CREATE TEMP TRIGGER gateway_write_failure BEFORE INSERT ON identity_meta WHEN NEW.key LIKE 'gateway-policy-v2:frame:%' BEGIN SELECT RAISE(ABORT,'injected gateway write failure'); END;"
        )?;
        assert!(
            store
                .append_gateway_source(
                    &a.node,
                    &client,
                    &begun.reference,
                    material.pages[0].clone(),
                    now()
                )
                .is_err()
        );
        assert_eq!(source_in(&store.connection, &begun.reference)?.pages, 0);
        assert_eq!(
            store.policy.gateway_policy.sources[&begun.reference]
                .record()
                .pages,
            0
        );
        store
            .connection
            .execute_batch("DROP TRIGGER gateway_write_failure")?;
        let first = store.append_gateway_source(
            &a.node,
            &client,
            &begun.reference,
            material.pages[0].clone(),
            now(),
        )?;
        assert_eq!(first.pages, 1);
        drop(store);
        let mut store = Store::open(&path)?;
        assert!(store.policy.verified().account(&b.account).is_none());
        assert_eq!(
            store
                .append_gateway_source(
                    &a.node,
                    &client,
                    &begun.reference,
                    material.pages[0].clone(),
                    now()
                )?
                .pages,
            1
        );
        for page in material.pages.iter().skip(1) {
            store.append_gateway_source(&a.node, &client, &begun.reference, page.clone(), now())?;
        }
        let finished = store.finish_gateway_source(
            &a.node,
            &client,
            &begun.reference,
            material.witness.clone(),
            material.confirm.clone(),
            material.confirmation.clone(),
            now(),
        )?;
        assert!(finished.complete);
        assert!(
            store
                .finish_gateway_source(
                    &a.node,
                    &client,
                    &begun.reference,
                    material.witness.clone(),
                    material.confirm.clone(),
                    material.confirmation.clone(),
                    now()
                )?
                .complete
        );
        let body = json!({"method":"message", "space_id":b.space, "resource_id":b.resource,
            "authorization_id":"gateway-message", "conversation_id":"chat", "text":"forwarded"});
        let original = GatewayOriginal::Invoke {
            body: body.clone(),
            invocation: Signed::sign(
                Invocation {
                    version: AUTH_VERSION,
                    actor: Actor::Account(b.account.clone()),
                    device: DeviceId::from_key(&client),
                    initiator: Actor::Account(b.account.clone()),
                    initiating_grant: None,
                    resource: approval.payload.resource.clone(),
                    action: Action::MessageAppend,
                    audience: material.snapshot.payload.selection.audience.clone(),
                    presenter: DeviceId::from_key(&approval.payload.gateway),
                    operation_id: "gateway-message".into(),
                    body_hash: ObjectHash::body(&body)?,
                    grant_leaf: b.bundle.grant_leaf.clone(),
                    expected_policies: material.snapshot.payload.target_heads.clone(),
                    context: InvocationContext::default(),
                    nonce: Nonce::random()?,
                    not_before_ms: now(),
                    expires_at_ms: now() + 10_000,
                },
                &b.client,
            )?,
        };
        let forward =
            store.prepare_gateway_forward(&a.node, &client, &begun.reference, &original, now())?;
        assert_eq!(forward.header.payload.original_hash, original.hash()?);
        assert_eq!(forward.target_public_key, approval.payload.target_key);
        assert!(
            store
                .prepare_gateway_forward(
                    &a.node,
                    &PublicKey::of(&a.client),
                    &begun.reference,
                    &original,
                    now()
                )
                .is_err()
        );
        drop(store);
        let mut store = Store::open(&path)?;
        store.prepare_gateway_forward(&a.node, &client, &begun.reference, &original, now())?;
        assert_eq!(store.trusted_policy_pins(), original_pins);
        let mut revoked = approval.payload.clone();
        revoked.generation = 3;
        revoked.state = GatewayRouteState::Revoked;
        store.install_gateway_route(&a.node, &Signed::sign(revoked, &a.root)?, now())?;
        assert!(
            store
                .prepare_gateway_forward(&a.node, &client, &begun.reference, &original, now())
                .is_err()
        );
        assert!(
            store
                .install_gateway_route(&a.node, &approval, now())
                .is_err()
        );
        drop(store);
        let store = Store::open(&path)?;
        assert!(
            store
                .prepare_gateway_forward(&a.node, &client, &begun.reference, &original, now())
                .is_err()
        );
        assert!(store.policy.verified().account(&b.account).is_none());
        assert_eq!(store.trusted_policy_pins(), original_pins);
        Ok(())
    }
}
