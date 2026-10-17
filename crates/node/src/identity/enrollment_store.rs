use super::{journal, policy::project_policy_in};
use crate::{crypto::LocalIdentity, store::Store};
use anyhow::{Context, Result, bail, ensure};
use rusqlite::{OptionalExtension, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use vibeface_secure::authorization::{
    AUTH_VERSION, AccountDocument, AccountId, AccountRight, Committed, DeviceCertificate, DeviceId,
    FreshnessReceipt, JournalEntry, Nonce, ObjectHash, PolicyCommit, PolicyDomain, PublicKey,
    Signed, SpaceId,
    enrollment::{
        self, AccountDeviceChange, AccountDevicePolicyRequest, DeviceEnrollmentIntent,
        offer::{self, EnrollmentOffer},
    },
};

#[derive(Clone, Serialize, Deserialize)]
struct EnrollmentRecord {
    request_id: ObjectHash,
    offer_hash: ObjectHash,
    nonce: Nonce,
    new_device_public_key: PublicKey,
    unsigned_intent: DeviceEnrollmentIntent,
    expires_at_ms: u64,
    #[serde(flatten)]
    phase: EnrollmentPhase,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
enum EnrollmentPhase {
    Requested,
    Approved {
        intent: Signed<DeviceEnrollmentIntent>,
    },
    Possessed {
        intent: Signed<DeviceEnrollmentIntent>,
        certificate: DeviceCertificate,
    },
    Completed {
        intent: Signed<DeviceEnrollmentIntent>,
        certificate: DeviceCertificate,
        completion: Value,
    },
}

fn text<'a>(params: &'a Value, name: &str) -> Result<&'a str> {
    params[name]
        .as_str()
        .with_context(|| format!("{name} is required"))
}

fn offer_key(hash: &ObjectHash) -> String {
    format!("enrollment-offer:{hash}")
}
fn request_key(hash: &ObjectHash) -> String {
    format!("enrollment-request:{hash}")
}

impl Store {
    fn enrollment_offer(
        &self,
        account: &AccountId,
        hash: &ObjectHash,
    ) -> Result<Signed<EnrollmentOffer>> {
        let encoded: String = self.connection.query_row(
            "SELECT value FROM local_values WHERE space_id=? AND key=?",
            params![SpaceId::personal(account).as_str(), offer_key(hash)],
            |row| row.get(0),
        )?;
        let signed = offer::decode_enrollment_offer(encoded.as_bytes())?;
        ensure!(
            signed.payload.account == *account && offer::enrollment_offer_hash(&signed)? == *hash,
            "stored enrollment offer differs from its reference"
        );
        Ok(signed)
    }

    fn current_enrollment_offer(
        &self,
        identity: &LocalIdentity,
        signed: &Signed<EnrollmentOffer>,
        now: u64,
    ) -> Result<()> {
        offer::verify_enrollment_offer(signed, now)?;
        ensure!(
            signed.payload.entry.node_public_key.as_str() == identity.public_key_hex()?,
            "enrollment offer targets another Node"
        );
        for pin in &signed.payload.pins {
            let trusted = self
                .policy
                .pins()
                .iter()
                .find(|p| p.domain == pin.domain)
                .context("enrollment account is not trusted by this Node")?;
            ensure!(
                pin.genesis_root == trusted.genesis_root
                    && pin.minimum_head.as_ref() == self.policy.verified().head(&pin.domain),
                "enrollment offer policy changed"
            );
        }
        Ok(())
    }

    fn current_enrollment_manager(
        &self,
        account: &AccountId,
        principal: &str,
        now: u64,
    ) -> Result<()> {
        let state = self.policy.verified();
        let document = state
            .account(account)
            .context("account policy is unavailable")?;
        let key = PublicKey::parse(principal)?;
        let device = DeviceId::from_key(&key);
        ensure!(
            document
                .enrollment
                .iter()
                .any(|entry| entry.device == device
                    && entry.rights.contains(&AccountRight::EnrollDevice)
                    && now < entry.expires_at_ms),
            "current account enrollment authority is required"
        );
        ensure!(
            !document.revoked_devices.contains(&device)
                && document
                    .devices
                    .iter()
                    .filter_map(|hash| state.device(hash))
                    .any(|cert| cert.payload.public_key == key
                        && cert.payload.not_before_ms <= now
                        && now < cert.payload.expires_at_ms),
            "account manager device is unavailable"
        );
        Ok(())
    }

    fn enrollment_record(
        &self,
        account: &AccountId,
        hash: &ObjectHash,
    ) -> Result<(String, EnrollmentRecord)> {
        let encoded: String = self.connection.query_row(
            "SELECT value FROM local_values WHERE space_id=? AND key=?",
            params![SpaceId::personal(account).as_str(), request_key(hash)],
            |row| row.get(0),
        )?;
        let record: EnrollmentRecord = serde_json::from_str(&encoded)?;
        ensure!(
            record.request_id == *hash && record.unsigned_intent.account == *account,
            "enrollment request scope changed"
        );
        Ok((encoded, record))
    }

    fn save_enrollment_record(
        &mut self,
        account: &AccountId,
        previous: Option<&str>,
        record: &EnrollmentRecord,
        now: u64,
    ) -> Result<()> {
        let update = self.policy.prepare_network_update(&[], now)?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        journal::commit_in(&tx, &update)?;
        let encoded = serde_json::to_string(record)?;
        let space = SpaceId::personal(account);
        if let Some(previous) = previous {
            ensure!(
                tx.execute(
                    "UPDATE local_values SET value=? WHERE space_id=? AND key=? AND value=?",
                    params![
                        encoded,
                        space.as_str(),
                        request_key(&record.request_id),
                        previous
                    ]
                )? == 1,
                "enrollment request changed concurrently"
            );
        } else {
            let active: u64 = tx.query_row("SELECT count(*) FROM local_values WHERE space_id=? AND key GLOB 'enrollment-request:*' AND json_extract(value,'$.status')!='completed' AND json_extract(value,'$.expires_at_ms')>?",
                params![space.as_str(), now], |row| row.get(0))?;
            ensure!(active < 32, "too many pending device enrollments");
            tx.execute(
                "INSERT INTO local_values(space_id,key,value) VALUES(?,?,?)",
                params![space.as_str(), request_key(&record.request_id), encoded],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    pub(crate) fn enrollment_command(
        &mut self,
        identity: &LocalIdentity,
        principal: &str,
        method: &str,
        params: &Value,
        now: u64,
    ) -> Result<Value> {
        if method == "account_device_commit" {
            return self.commit_account_device(identity, principal, params, now);
        }
        if method == "enrollment_offer_publish" {
            let encoded = serde_json::to_vec(&params["offer"])?;
            let signed = offer::decode_enrollment_offer(&encoded)?;
            ensure!(
                signed.issuer.as_str() == principal,
                "enrollment offer belongs to another Noise manager"
            );
            self.current_enrollment_offer(identity, &signed, now)?;
            let hash = offer::enrollment_offer_hash(&signed)?;
            let space = SpaceId::personal(&signed.payload.account);
            let encoded = serde_json::to_string(&signed)?;
            let update = self.policy.prepare_network_update(&[], now)?;
            let tx = self
                .connection
                .transaction_with_behavior(TransactionBehavior::Immediate)?;
            journal::commit_in(&tx, &update)?;
            let old: Option<String> = tx
                .query_row(
                    "SELECT value FROM local_values WHERE space_id=? AND key=?",
                    params![space.as_str(), offer_key(&hash)],
                    |row| row.get(0),
                )
                .optional()?;
            if let Some(old) = old {
                ensure!(old == encoded, "enrollment offer reference was reused");
            } else {
                let active: u64 = tx.query_row("SELECT count(*) FROM local_values WHERE space_id=? AND key GLOB 'enrollment-offer:*' AND json_extract(value,'$.payload.expires_at_ms')>?", params![space.as_str(),now], |row| row.get(0))?;
                ensure!(active < 32, "too many published enrollment offers");
                tx.execute(
                    "INSERT INTO local_values(space_id,key,value) VALUES(?,?,?)",
                    params![space.as_str(), offer_key(&hash), encoded],
                )?;
            }
            tx.commit()?;
            return Ok(json!({"offer_hash":hash}));
        }
        let account = AccountId::parse(text(params, "account_id")?)?;
        let hash = ObjectHash::parse(text(params, "offer_hash")?)?;
        let signed = self.enrollment_offer(&account, &hash)?;
        if method == "enrollment_status" {
            let id = ObjectHash::parse(text(params, "request_id")?)?;
            let (_, record) = self.enrollment_record(&account, &id)?;
            ensure!(
                record.offer_hash == hash,
                "enrollment request belongs to another offer"
            );
            if record.new_device_public_key.as_str() != principal {
                self.current_enrollment_manager(&account, principal, now)?;
            } else if matches!(record.phase, EnrollmentPhase::Completed { .. }) {
                let state = self.policy.verified();
                let document = state
                    .account(&account)
                    .context("account policy is unavailable")?;
                let device = DeviceId::from_key(&record.new_device_public_key);
                ensure!(
                    !document.revoked_devices.contains(&device)
                        && document
                            .devices
                            .iter()
                            .filter_map(|hash| state.device(hash))
                            .any(
                                |cert| cert.payload.public_key == record.new_device_public_key
                                    && cert.payload.not_before_ms <= now
                                    && now < cert.payload.expires_at_ms
                            ),
                    "enrolled device is no longer current"
                );
            }
            if !matches!(record.phase, EnrollmentPhase::Completed { .. }) {
                self.current_enrollment_offer(identity, &signed, now)?;
            }
            return Ok(serde_json::to_value(record)?);
        }
        self.current_enrollment_offer(identity, &signed, now)?;
        match method {
            "enrollment_offer_get" => Ok(json!({"offer":signed})),
            "enrollment_request" => {
                let key = PublicKey::parse(text(params, "new_device_public_key")?)?;
                ensure!(
                    key.as_str() == principal,
                    "enrollment request belongs to another Noise device"
                );
                let intent: DeviceEnrollmentIntent =
                    serde_json::from_value(params["intent"].clone())?;
                let verified = offer::verify_device_enrollment_offer(
                    &serde_json::to_string(&signed)?,
                    now as f64,
                )
                .map_err(anyhow::Error::msg)?;
                offer::verify_device_enrollment_intent_for_offer(
                    &verified,
                    &serde_json::to_string(&intent)?,
                    key.as_str(),
                    now as f64,
                )
                .map_err(anyhow::Error::msg)?;
                let id = ObjectHash::body(&(hash.clone(), &intent))?;
                if let Ok((_, old)) = self.enrollment_record(&account, &id) {
                    return Ok(serde_json::to_value(old)?);
                }
                let record = EnrollmentRecord {
                    request_id: id,
                    offer_hash: hash,
                    nonce: Nonce::random()?,
                    new_device_public_key: key,
                    unsigned_intent: intent,
                    expires_at_ms: signed.payload.expires_at_ms,
                    phase: EnrollmentPhase::Requested,
                };
                self.save_enrollment_record(&account, None, &record, now)?;
                Ok(serde_json::to_value(record)?)
            }
            "enrollment_pending" => {
                ensure!(
                    signed.issuer.as_str() == principal,
                    "offer manager Noise identity is required"
                );
                self.current_enrollment_manager(&account, principal, now)?;
                let mut statement = self.connection.prepare("SELECT value FROM local_values WHERE space_id=? AND key GLOB 'enrollment-request:*' ORDER BY key")?;
                let rows = statement.query_map([SpaceId::personal(&account).as_str()], |row| {
                    row.get::<_, String>(0)
                })?;
                let mut records = Vec::new();
                for row in rows {
                    let record: EnrollmentRecord = serde_json::from_str(&row?)?;
                    if record.offer_hash == hash {
                        records.push(record);
                    }
                }
                Ok(json!({"requests":records}))
            }
            "enrollment_approve" | "enrollment_possession" => {
                let id = ObjectHash::parse(text(params, "request_id")?)?;
                let (previous, mut record) = self.enrollment_record(&account, &id)?;
                ensure!(
                    record.offer_hash == hash,
                    "enrollment request belongs to another offer"
                );
                if method == "enrollment_approve" {
                    ensure!(
                        signed.issuer.as_str() == principal,
                        "offer manager Noise identity is required"
                    );
                    let intent: Signed<DeviceEnrollmentIntent> =
                        serde_json::from_value(params["intent"].clone())?;
                    ensure!(
                        intent.issuer.as_str() == principal
                            && intent.payload == record.unsigned_intent,
                        "approval differs from the requested device enrollment"
                    );
                    enrollment::verify_account_enrollment(self.policy.verified(), &intent, now)?;
                    match &record.phase {
                        EnrollmentPhase::Requested => {
                            record.phase = EnrollmentPhase::Approved { intent }
                        }
                        EnrollmentPhase::Approved { intent: old } if old == &intent => {
                            return Ok(serde_json::to_value(record)?);
                        }
                        _ => bail!("enrollment is already approved or completed"),
                    }
                } else {
                    ensure!(
                        record.new_device_public_key.as_str() == principal,
                        "new device Noise identity is required"
                    );
                    let certificate: DeviceCertificate =
                        serde_json::from_value(params["certificate"].clone())?;
                    let intent = match &record.phase {
                        EnrollmentPhase::Approved { intent } => intent.clone(),
                        EnrollmentPhase::Possessed {
                            certificate: old, ..
                        } if old == &certificate => return Ok(serde_json::to_value(record)?),
                        _ => bail!("enrollment manager approval is required"),
                    };
                    let i = &record.unsigned_intent;
                    let mut uses = vec![
                        vibeface_secure::authorization::DeviceUse::Noise,
                        vibeface_secure::authorization::DeviceUse::AccountInvocation,
                    ];
                    if i.requested_rights.contains(&AccountRight::EnrollDevice) {
                        uses.push(vibeface_secure::authorization::DeviceUse::EnrollDevice);
                    }
                    ensure!(
                        certificate.version == AUTH_VERSION
                            && certificate.uses == uses
                            && certificate.enrollment_grant.is_none()
                            && certificate.device == DeviceId::from_key(&i.new_device_public_key)
                            && certificate.public_key == i.new_device_public_key
                            && certificate.owner == PolicyDomain::Account(account.clone())
                            && certificate.subject
                                == vibeface_secure::authorization::Actor::Account(account.clone())
                            && certificate.nonce == record.nonce
                            && certificate.not_before_ms == i.certificate_not_before_ms
                            && certificate.expires_at_ms == i.certificate_expires_at_ms
                            && i.expected_heads.contains(&certificate.issued_under),
                        "device possession differs from approved intent"
                    );
                    vibeface_secure::verify(
                        principal,
                        "device-possession-v2",
                        &certificate.possession_bytes()?,
                        certificate
                            .possession
                            .as_ref()
                            .context("device possession proof is missing")?
                            .as_str(),
                    )
                    .map_err(anyhow::Error::msg)?;
                    record.phase = EnrollmentPhase::Possessed {
                        intent,
                        certificate,
                    };
                }
                self.save_enrollment_record(&account, Some(&previous), &record, now)?;
                Ok(serde_json::to_value(record)?)
            }
            _ => bail!("unknown enrollment method"),
        }
    }

    fn commit_account_device(
        &mut self,
        identity: &LocalIdentity,
        principal: &str,
        params: &Value,
        now: u64,
    ) -> Result<Value> {
        let request: AccountDevicePolicyRequest =
            serde_json::from_value(params["request"].clone())?;
        ensure!(
            request.proposal.issuer.as_str() == principal,
            "device policy request belongs to another Noise manager"
        );
        ensure!(
            !matches!(request.change, AccountDeviceChange::Passkey(_)),
            "Passkey device changes require the pending ledger transaction"
        );
        let account = &request.proposal.payload.account;
        let input_hash = ObjectHash::body(&request)?;
        let receipt_key = format!("device-policy-receipt:{input_hash}");
        let space = SpaceId::personal(account);
        let receipt: Option<String> = self
            .connection
            .query_row(
                "SELECT value FROM local_values WHERE space_id=? AND key=?",
                params![space.as_str(), receipt_key],
                |row| row.get(0),
            )
            .optional()?;
        if let Some(receipt) = receipt {
            return Ok(serde_json::from_str(&receipt)?);
        }
        let mut pending = None;
        if let AccountDeviceChange::Enrollment {
            intent,
            certificate,
        } = &request.change
        {
            let id = ObjectHash::parse(text(params, "request_id")?)?;
            let (encoded, record) = self.enrollment_record(account, &id)?;
            let offer = self.enrollment_offer(account, &record.offer_hash)?;
            self.current_enrollment_offer(identity, &offer, now)?;
            ensure!(
                offer.issuer == request.proposal.issuer && record.nonce == request.nonce,
                "device policy request does not belong to this manager and nonce"
            );
            ensure!(
                matches!(&record.phase, EnrollmentPhase::Possessed {intent:approved,certificate:possession}
                if approved == intent && possession == &certificate.payload),
                "device policy request differs from approved possession"
            );
            pending = Some((encoded, record));
        }
        let snapshot = self.authenticate_account(
            principal,
            &json!({"account_id":account,"space_id":space,"device_public_key":principal}),
            now,
        )?;
        let signer = vibeface_secure::Identity::from_secret_hex(&identity.secret_key_hex)
            .map_err(anyhow::Error::msg)?;
        let request_json = serde_json::to_string(&request)?;
        let pins = serde_json::to_string(&snapshot["pins"])?;
        let bundle = serde_json::to_string(&snapshot["authorization"])?;
        let challenge: Signed<PolicyCommit> = serde_json::from_str(
            &enrollment::issue_account_device_policy_challenge(
                &request_json,
                &signer,
                &pins,
                &bundle,
                now as f64,
            )
            .map_err(anyhow::Error::msg)?,
        )?;
        let mut receipts = Vec::new();
        for head in &challenge.payload.dependencies {
            let authority = match &head.domain {
                PolicyDomain::Account(account) => {
                    &self
                        .policy
                        .verified()
                        .account(account)
                        .context("account authority is unavailable")?
                        .authority
                }
                PolicyDomain::Space(space) => {
                    &self
                        .policy
                        .verified()
                        .space(space)
                        .context("space authority is unavailable")?
                        .authority
                }
            };
            ensure!(
                authority.public_key.as_str() == identity.public_key_hex()?,
                "device policy requires another current authority"
            );
            receipts.push(Signed::sign(
                FreshnessReceipt {
                    version: AUTH_VERSION,
                    current: head.clone(),
                    challenge_hash: challenge.challenge_hash()?,
                    issued_at_ms: now,
                    expires_at_ms: challenge.payload.expires_at_ms,
                },
                &signer,
            )?);
        }
        let committed: Committed<AccountDocument> = serde_json::from_str(
            &enrollment::accept_account_device_policy_change(
                &request_json,
                &serde_json::to_string(&challenge)?,
                &serde_json::to_string(&receipts)?,
                &signer,
                &pins,
                &bundle,
                now as f64,
            )
            .map_err(anyhow::Error::msg)?,
        )?;
        let mut delta = Vec::new();
        if let AccountDeviceChange::Enrollment { certificate, .. } = &request.change {
            delta.push(JournalEntry::Device(certificate.clone()));
        }
        delta.push(JournalEntry::AccountCommit(committed.clone()));
        let response = json!({"committed":committed,"journal_delta":delta});
        let update = self.policy.prepare_network_update(&delta, now)?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let state = journal::commit_in(&tx, &update)?;
        project_policy_in(&tx, &state)?;
        if let Some((previous, mut record)) = pending {
            let EnrollmentPhase::Possessed {
                intent,
                certificate,
            } = record.phase
            else {
                bail!("device possession changed")
            };
            record.phase = EnrollmentPhase::Completed {
                intent,
                certificate,
                completion: response.clone(),
            };
            ensure!(
                tx.execute(
                    "UPDATE local_values SET value=? WHERE space_id=? AND key=? AND value=?",
                    params![
                        serde_json::to_string(&record)?,
                        space.as_str(),
                        request_key(&record.request_id),
                        previous
                    ]
                )? == 1,
                "device enrollment changed before commit"
            );
        }
        tx.execute(
            "INSERT INTO local_values(space_id,key,value) VALUES(?,?,?)",
            params![
                space.as_str(),
                receipt_key,
                serde_json::to_string(&response)?
            ],
        )?;
        tx.commit()?;
        self.policy = state;
        Ok(response)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::identity::test_support::{TestNetwork, now};
    use std::collections::BTreeMap;
    use vibeface_secure::Identity;

    struct Flow {
        network: TestNetwork,
        store: Store,
        device: Identity,
        offer: String,
        hash: Value,
        intent: String,
        record: Value,
    }

    impl Flow {
        fn new(persistent: bool) -> Result<Self> {
            let network = TestNetwork::new()?;
            let store = if persistent {
                network.at(&network.directory.join("enrollment.sqlite3"))?
            } else {
                network.memory()?
            };
            let offer = offer::create_device_enrollment_offer(
                &network.client,
                network.account.as_str(),
                "https://node.example.test",
                &network.node.public_key_hex()?,
                "[]",
                (now() + 60_000) as f64,
                &serde_json::to_string(&network.pins)?,
                &serde_json::to_string(&network.bundle)?,
                now() as f64,
            )
            .map_err(anyhow::Error::msg)?;
            let device = Identity::generate().map_err(anyhow::Error::msg)?;
            let verified = offer::verify_device_enrollment_offer(&offer, now() as f64)
                .map_err(anyhow::Error::msg)?;
            let intent =
                offer::create_device_enrollment_intent_from_offer(&verified, &device, now() as f64)
                    .map_err(anyhow::Error::msg)?;
            let mut flow = Self {
                network,
                store,
                device,
                offer,
                hash: Value::Null,
                intent,
                record: Value::Null,
            };
            let published = flow.call(
                &flow.network.client.public_key_hex(),
                "enrollment_offer_publish",
                &json!({"offer":serde_json::from_str::<Value>(&flow.offer)?}),
                now(),
            )?;
            flow.hash = published["offer_hash"].clone();
            Ok(flow)
        }

        fn call(
            &mut self,
            principal: &str,
            method: &str,
            params: &Value,
            at: u64,
        ) -> Result<Value> {
            self.store
                .enrollment_command(&self.network.node, principal, method, params, at)
        }

        fn reference(&self) -> Value {
            json!({"account_id":self.network.account,"offer_hash":self.hash,"request_id":self.record["request_id"]})
        }

        fn request_params(&self) -> Result<Value> {
            Ok(
                json!({"account_id":self.network.account,"offer_hash":self.hash,
                "new_device_public_key":self.device.public_key_hex(),"intent":serde_json::from_str::<Value>(&self.intent)?}),
            )
        }

        fn request(&mut self) -> Result<Value> {
            self.record = self.call(
                &self.device.public_key_hex(),
                "enrollment_request",
                &self.request_params()?,
                now(),
            )?;
            Ok(self.record.clone())
        }

        fn approval(&self) -> Result<String> {
            enrollment::sign_device_enrollment_intent(
                &self.intent,
                &self.network.client,
                &serde_json::to_string(&self.network.pins)?,
                &serde_json::to_string(&self.network.bundle)?,
                now() as f64,
            )
            .map_err(anyhow::Error::msg)
        }

        fn approve(&mut self) -> Result<Value> {
            let mut params = self.reference();
            params["intent"] = serde_json::from_str(&self.approval()?)?;
            self.record = self.call(
                &self.network.client.public_key_hex(),
                "enrollment_approve",
                &params,
                now(),
            )?;
            Ok(self.record.clone())
        }

        fn qualification(&self) -> Result<enrollment::VerifiedAccountEnrollment> {
            let verified = offer::verify_device_enrollment_offer(&self.offer, now() as f64)
                .map_err(anyhow::Error::msg)?;
            offer::verify_account_enrollment_from_offer(&self.approval()?, &verified, now() as f64)
                .map_err(anyhow::Error::msg)
        }

        fn certificate(&self) -> Result<String> {
            enrollment::sign_device_enrollment_possession(
                &self.qualification()?,
                &self.device,
                self.record["nonce"]
                    .as_str()
                    .context("pending nonce missing")?,
                now() as f64,
            )
            .map_err(anyhow::Error::msg)
        }

        fn possess(&mut self) -> Result<Value> {
            let mut params = self.reference();
            params["certificate"] = serde_json::from_str(&self.certificate()?)?;
            self.record = self.call(
                &self.device.public_key_hex(),
                "enrollment_possession",
                &params,
                now(),
            )?;
            Ok(self.record.clone())
        }

        fn prepared(&self) -> Result<enrollment::PreparedAccountDeviceChange> {
            enrollment::prepare_account_device_enrollment(
                &self.qualification()?,
                &self.certificate()?,
                &self.network.client,
                &serde_json::to_string(&self.network.pins)?,
                &serde_json::to_string(&self.network.bundle)?,
                now() as f64,
            )
            .map_err(anyhow::Error::msg)
        }

        fn commit_params(&self) -> Result<Value> {
            Ok(json!({"request_id":self.record["request_id"],
                "request":serde_json::from_str::<Value>(&self.prepared()?.request_json().map_err(anyhow::Error::msg)?)?}))
        }

        fn restart(self) -> Result<Self> {
            let Self {
                network,
                store,
                device,
                offer,
                hash,
                intent,
                record,
            } = self;
            drop(store);
            let store = Store::open(network.directory.join("enrollment.sqlite3"))?;
            Ok(Self {
                network,
                store,
                device,
                offer,
                hash,
                intent,
                record,
            })
        }

        fn snapshot(&self) -> Result<(BTreeMap<String, Vec<Vec<rusqlite::types::Value>>>, Value)> {
            let tables = self.store.connection.prepare("SELECT name FROM sqlite_schema WHERE type='table' AND name NOT LIKE 'sqlite_%' ORDER BY name")?
                .query_map([],|row| row.get::<_,String>(0))?.collect::<std::result::Result<Vec<_>,_>>()?;
            let mut database = BTreeMap::new();
            for table in tables {
                let mut statement = self
                    .store
                    .connection
                    .prepare(&format!("SELECT * FROM \"{}\"", table.replace('"', "\"\"")))?;
                let columns = statement.column_count();
                let rows = statement
                    .query_map([], |row| {
                        (0..columns)
                            .map(|column| row.get(column))
                            .collect::<std::result::Result<Vec<rusqlite::types::Value>, _>>()
                    })?
                    .collect::<std::result::Result<Vec<_>, _>>()?;
                database.insert(table, rows);
            }
            Ok((
                database,
                json!({"account":self.store.authorization_state().account(&self.network.account),
                "account_head":self.store.authorization_state().head(&PolicyDomain::Account(self.network.account.clone())),
                "space_head":self.store.authorization_state().head(&PolicyDomain::Space(self.network.space.clone()))}),
            ))
        }
    }

    #[test]
    fn failed_enrollment_writes_roll_back_database_and_memory_and_allow_exact_retry() -> Result<()>
    {
        for (trigger, commit) in [
            (
                "CREATE TEMP TRIGGER enrollment_failure BEFORE INSERT ON main.local_values WHEN NEW.key GLOB 'enrollment-request:*' BEGIN SELECT RAISE(ABORT,'request failure'); END",
                false,
            ),
            (
                "CREATE TEMP TRIGGER enrollment_failure BEFORE UPDATE ON main.local_values WHEN NEW.key GLOB 'enrollment-request:*' AND json_extract(NEW.value,'$.status')='completed' BEGIN SELECT RAISE(ABORT,'completion failure'); END",
                true,
            ),
            (
                "CREATE TEMP TRIGGER enrollment_failure BEFORE INSERT ON main.local_values WHEN NEW.key GLOB 'device-policy-receipt:*' BEGIN SELECT RAISE(ABORT,'receipt failure'); END",
                true,
            ),
        ] {
            let mut flow = Flow::new(false)?;
            let params = if commit {
                flow.request()?;
                flow.approve()?;
                flow.possess()?;
                flow.commit_params()?
            } else {
                flow.request_params()?
            };
            flow.store.connection.execute_batch(trigger)?;
            let before = flow.snapshot()?;
            let principal = if commit {
                flow.network.client.public_key_hex()
            } else {
                flow.device.public_key_hex()
            };
            let method = if commit {
                "account_device_commit"
            } else {
                "enrollment_request"
            };
            assert!(flow.call(&principal, method, &params, now()).is_err());
            assert_eq!(flow.snapshot()?, before);
            flow.store
                .connection
                .execute_batch("DROP TRIGGER enrollment_failure")?;
            let result = flow.call(&principal, method, &params, now())?;
            if commit {
                assert!(result["committed"]["proposal"].is_object());
                assert_eq!(
                    flow.call(
                        &flow.device.public_key_hex(),
                        "enrollment_status",
                        &flow.reference(),
                        now()
                    )?["status"],
                    "completed"
                );
            } else {
                assert_eq!(result["status"], "requested");
            }
        }
        Ok(())
    }

    #[test]
    fn enrollment_binds_each_phase_to_noise_offer_intent_and_possession() -> Result<()> {
        let mut flow = Flow::new(false)?;
        let outsider = Identity::generate()
            .map_err(anyhow::Error::msg)?
            .public_key_hex();
        let before = flow.snapshot()?;
        assert!(
            flow.call(
                &outsider,
                "enrollment_request",
                &flow.request_params()?,
                now()
            )
            .is_err()
        );
        assert_eq!(flow.snapshot()?, before);
        flow.request()?;
        let mut approve = flow.reference();
        approve["intent"] = serde_json::from_str(&flow.approval()?)?;
        let before = flow.snapshot()?;
        assert!(
            flow.call(&outsider, "enrollment_approve", &approve, now())
                .is_err()
        );
        let mut changed = approve.clone();
        changed["intent"]["payload"]["requested_rights"] = json!(["enroll_device"]);
        assert!(
            flow.call(
                &flow.network.client.public_key_hex(),
                "enrollment_approve",
                &changed,
                now()
            )
            .is_err()
        );
        let mut cross = approve.clone();
        cross["offer_hash"] = json!(ObjectHash::body(&"another offer")?);
        assert!(
            flow.call(
                &flow.network.client.public_key_hex(),
                "enrollment_approve",
                &cross,
                now()
            )
            .is_err()
        );
        assert_eq!(flow.snapshot()?, before);
        flow.approve()?;
        let mut possession = flow.reference();
        possession["certificate"] = serde_json::from_str(&flow.certificate()?)?;
        let before = flow.snapshot()?;
        assert!(
            flow.call(
                &flow.network.client.public_key_hex(),
                "enrollment_possession",
                &possession,
                now()
            )
            .is_err()
        );
        for field in ["nonce", "possession", "uses"] {
            let mut changed = possession.clone();
            changed["certificate"][field] = match field {
                "nonce" => json!(Nonce::random()?),
                "possession" => json!("00".repeat(64)),
                _ => json!([]),
            };
            assert!(
                flow.call(
                    &flow.device.public_key_hex(),
                    "enrollment_possession",
                    &changed,
                    now()
                )
                .is_err()
            );
            assert_eq!(flow.snapshot()?, before);
        }
        flow.possess()?;
        let params = flow.commit_params()?;
        let before = flow.snapshot()?;
        assert!(
            flow.call(
                &flow.device.public_key_hex(),
                "account_device_commit",
                &params,
                now()
            )
            .is_err()
        );
        let mut wrong_request = params.clone();
        wrong_request["request_id"] = json!(ObjectHash::body(&"another pending request")?);
        assert!(
            flow.call(
                &flow.network.client.public_key_hex(),
                "account_device_commit",
                &wrong_request,
                now()
            )
            .is_err()
        );
        assert!(
            flow.call(&outsider, "enrollment_status", &flow.reference(), now())
                .is_err()
        );
        assert_eq!(flow.snapshot()?, before);
        Ok(())
    }

    #[test]
    fn pending_enrollment_cas_never_overwrites_a_newer_approval() -> Result<()> {
        let mut flow = Flow::new(false)?;
        flow.request()?;
        let id = ObjectHash::parse(flow.record["request_id"].as_str().unwrap())?;
        let (previous, record) = flow.store.enrollment_record(&flow.network.account, &id)?;
        flow.approve()?;
        let before = flow.snapshot()?;
        assert!(
            flow.store
                .save_enrollment_record(&flow.network.account, Some(&previous), &record, now())
                .is_err()
        );
        assert_eq!(flow.snapshot()?, before);
        Ok(())
    }

    #[test]
    fn revocation_and_expiry_stop_every_unfinished_enrollment_phase() -> Result<()> {
        for phase in 0..3 {
            let mut flow = Flow::new(false)?;
            flow.request()?;
            if phase >= 1 {
                flow.approve()?;
            }
            if phase >= 2 {
                flow.possess()?;
            }
            let (method, params, principal) = match phase {
                0 => {
                    let mut params = flow.reference();
                    params["intent"] = serde_json::from_str(&flow.approval()?)?;
                    (
                        "enrollment_approve",
                        params,
                        flow.network.client.public_key_hex(),
                    )
                }
                1 => {
                    let mut params = flow.reference();
                    params["certificate"] = serde_json::from_str(&flow.certificate()?)?;
                    (
                        "enrollment_possession",
                        params,
                        flow.device.public_key_hex(),
                    )
                }
                _ => (
                    "account_device_commit",
                    flow.commit_params()?,
                    flow.network.client.public_key_hex(),
                ),
            };
            let before = flow.snapshot()?;
            assert!(
                flow.call(&principal, method, &params, now() + 60_001)
                    .is_err()
            );
            assert_eq!(flow.snapshot()?, before);
            flow.network.revoke_client(&mut flow.store)?;
            let before = flow.snapshot()?;
            assert!(flow.call(&principal, method, &params, now()).is_err());
            assert_eq!(flow.snapshot()?, before);
        }
        Ok(())
    }

    #[test]
    fn pending_enrollment_limit_counts_active_distinct_requests_only() -> Result<()> {
        let mut flow = Flow::new(false)?;
        let verified = offer::verify_device_enrollment_offer(&flow.offer, now() as f64)
            .map_err(anyhow::Error::msg)?;
        for _ in 0..32 {
            flow.device = Identity::generate().map_err(anyhow::Error::msg)?;
            flow.intent = offer::create_device_enrollment_intent_from_offer(
                &verified,
                &flow.device,
                now() as f64,
            )
            .map_err(anyhow::Error::msg)?;
            let requested = flow.request()?;
            assert_eq!(flow.request()?, requested);
        }
        flow.device = Identity::generate().map_err(anyhow::Error::msg)?;
        flow.intent = offer::create_device_enrollment_intent_from_offer(
            &verified,
            &flow.device,
            now() as f64,
        )
        .map_err(anyhow::Error::msg)?;
        let before = flow.snapshot()?;
        assert!(flow.request().is_err());
        assert_eq!(flow.snapshot()?, before);
        assert_eq!(
            flow.store.connection.query_row(
                "SELECT count(*) FROM local_values WHERE key GLOB 'enrollment-request:*'",
                [],
                |row| row.get::<_, u64>(0)
            )?,
            32
        );
        Ok(())
    }

    #[test]
    fn real_enrollment_survives_each_restart_and_recovers_only_the_original_receipt() -> Result<()>
    {
        let mut flow = Flow::new(true)?;
        flow = flow.restart()?;
        let requested = flow.request()?;
        assert_eq!(requested["status"], "requested");
        assert_eq!(flow.request()?, requested);
        flow = flow.restart()?;
        let approved = flow.approve()?;
        assert_eq!(approved["status"], "approved");
        assert_eq!(flow.approve()?, approved);
        flow = flow.restart()?;
        let possessed = flow.possess()?;
        assert_eq!(possessed["status"], "possessed");
        assert_eq!(flow.possess()?, possessed);
        flow = flow.restart()?;
        let prepared = flow.prepared()?;
        let params = flow.commit_params()?;
        let manager = flow.network.client.public_key_hex();
        let receipt = flow.call(&manager, "account_device_commit", &params, now())?;
        let finished = enrollment::finish_account_device_change(
            &prepared,
            &serde_json::to_string(&receipt["committed"])?,
            now() as f64,
        )
        .map_err(anyhow::Error::msg)?;
        let scope: Option<vibeface_secure::authorization::ClientScope> =
            serde_json::from_str(&finished.scope_json().map_err(anyhow::Error::msg)?)?;
        assert_eq!(
            scope.as_ref().map(|scope| &scope.device_id),
            Some(&DeviceId::from_key(&PublicKey::of(&flow.device)))
        );
        let completed = flow.call(
            &flow.device.public_key_hex(),
            "enrollment_status",
            &flow.reference(),
            now(),
        )?;
        assert_eq!(completed["status"], "completed");
        assert_eq!(completed["completion"], receipt);
        let key = flow.device.public_key_hex();
        let authenticated = flow.store.authenticate_account(
            &key,
            &json!({"account_id":flow.network.account,
            "space_id":flow.network.space,"device_public_key":key}),
            now(),
        )?;
        assert_eq!(
            authenticated["scope"]["device_id"],
            json!(DeviceId::from_key(&PublicKey::of(&flow.device)))
        );
        flow = flow.restart()?;
        let before = flow.snapshot()?;
        assert_eq!(
            flow.call(&manager, "account_device_commit", &params, now() + 120_001)?,
            receipt
        );
        assert!(
            flow.call(&key, "account_device_commit", &params, now())
                .is_err()
        );
        assert_eq!(flow.snapshot()?, before);
        flow.network.revoke_client(&mut flow.store)?;
        let before = flow.snapshot()?;
        assert_eq!(
            flow.call(&manager, "account_device_commit", &params, now())?,
            receipt
        );
        assert_eq!(flow.snapshot()?, before);
        Ok(())
    }
}
