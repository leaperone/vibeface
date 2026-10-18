use super::journal::PolicyState;
use crate::{crypto::LocalIdentity, store::Store};
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::collections::BTreeMap;
use vibeface_secure::authorization::{
    AccountId, DeviceId, MAX_PROOF_BYTES, Nonce, ObjectHash, PolicyDomain, PolicyPin, PublicKey,
    Signed,
    client_policy_import::{
        ClientPolicyAuthConfirmRequest, ClientPolicyAuthConfirmation, ClientPolicyPage,
        ClientPolicyReadRequest, ClientPolicySnapshot, ClientPolicyWitness,
    },
    gateway_forward::{
        GatewayClientPolicyImport, GatewayForwardHeader, GatewayOriginal, GatewayRouteApproval,
        GatewaySourceFrame, VerifiedGatewayClientSource, VerifiedGatewayRoute,
        begin_gateway_client_import, verify_gateway_original, verify_gateway_route,
        verify_gateway_route_approval,
    },
};
use vibeface_secure::signature_verification::ScopedSignatureVerification;

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
    let certificate = primary.verified().find_device(&DeviceId::from_key(local))?;
    ensure!(
        certificate.payload.public_key == *local,
        "gateway local certificate key differs"
    );
    let PolicyDomain::Account(account) = &certificate.payload.owner else {
        anyhow::bail!("gateway route requires a claimed Account Node");
    };
    Ok(account.clone())
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
