use super::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KeyAction {
    Register,
    Rotate,
    Revoke,
}

#[derive(Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum KeyCompletion {
    Confirmed,
    Failed,
    Superseded,
}
impl KeyCompletion {
    fn phase(self) -> &'static str {
        match self {
            Self::Confirmed => "confirmed",
            Self::Failed => "failed",
            Self::Superseded => "superseded",
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct KeyOperation {
    intent_id: String,
    action: KeyAction,
    gas_cap: String,
    profile_id: Word,
    binding: SignedKeyBinding,
    material_id: String,
    completed: Option<KeyCompletion>,
}
impl KeyOperation {
    fn storage_id(intent: &str) -> String {
        format!("receiving-operation:{intent}")
    }
    fn transaction_id(&self) -> String {
        format!("key-{}/register", self.intent_id)
    }
    fn matches(&self, action: KeyAction, gas_cap: &str, config: &DeliveryConfig) -> Result<()> {
        self.binding.verify()?;
        ensure!(
            self.action == action
                && self.gas_cap == gas_cap
                && self.profile_id
                    == config
                        .chain
                        .as_ref()
                        .context("paid deployment missing")?
                        .profile
                        .digest()?,
            "receiving key intent was already used for another action, budget or profile"
        );
        let (chain, escrow) = deployment(config, Mode::Paid)?;
        ensure!(
            self.binding.binding.chain_id == chain && self.binding.binding.escrow == escrow,
            "receiving key intent belongs to another deployment"
        );
        Ok(())
    }
}

pub(super) fn current_key_label(config: &DeliveryConfig) -> Result<String> {
    let (chain, escrow) = deployment(config, Mode::Paid)?;
    Ok(format!(
        "receiving-current:{}:{}",
        hex::encode(chain),
        hex::encode(escrow)
    ))
}
fn pending_label(config: &DeliveryConfig) -> Result<String> {
    let (chain, escrow) = deployment(config, Mode::Paid)?;
    let mut bytes = chain.to_vec();
    bytes.extend_from_slice(&escrow);
    Ok(format!(
        "receiving-operation-current:{}",
        hex::encode(hash(&bytes))
    ))
}
fn material_id(binding: &KeyBinding, intent: &str) -> String {
    if binding.action == BindingAction::Active {
        format!("receiving-key:{}", hex::encode(binding.key_id))
    } else {
        format!("receiving-revoked:{intent}")
    }
}

impl Delivery {
    pub(super) fn new_receiving_material(
        &self,
        serial: u64,
        action: BindingAction,
    ) -> Result<ReceivingMaterial> {
        let (chain_id, escrow) = deployment(&self.config(), Mode::Paid)?;
        let secret = if action == BindingAction::Active {
            Some(Secret32::new(random_word())?)
        } else {
            None
        };
        let receiver_public = match &secret {
            Some(secret) => receiving_public(secret)?,
            None => [0; 32],
        };
        let binding = KeyBinding {
            chain_id,
            escrow,
            receiver_peer: parse_word(&self.identity.device_id()?)?,
            receiver_public,
            key_id: if action == BindingAction::Active {
                random_word()
            } else {
                [0; 32]
            },
            serial,
            valid_from: now().saturating_sub(60),
            valid_until: now() + 7 * 86400,
            action,
        };
        let signature = signature(
            self.identity
                .sign_payload("delivery-key", &binding.signing_payload()?)?,
        )?;
        Ok(ReceivingMaterial {
            binding: SignedKeyBinding { binding, signature },
            secret,
        })
    }
    fn pending_key_operation(&self) -> Result<Option<KeyOperation>> {
        let label = pending_label(&self.config())?;
        self.with_store(|store| {
            let Some(intent) = store.value(&label)? else {
                return Ok(None);
            };
            let intent = std::str::from_utf8(&intent)?;
            let bytes = store
                .value(&KeyOperation::storage_id(intent))?
                .context("pending receiving key intent is missing")?;
            Ok(Some(serde_json::from_slice(&bytes)?))
        })
    }
    fn reserve_key_operation(
        &self,
        intent: &str,
        action: KeyAction,
        gas_cap: &str,
        registry: &chain::KeyRecord,
    ) -> Result<KeyOperation> {
        let config = self.config();
        let id = KeyOperation::storage_id(intent);
        self.with_store(|store| {
            if let Some(bytes)=store.value(&id)? {
                let operation:KeyOperation=serde_json::from_slice(&bytes)?;
                operation.matches(action,gas_cap,&config)?;
                ensure!(operation.binding.binding.receiver_peer==parse_word(&self.identity.device_id()?)?,"receiving key intent belongs to another Node");
                return Ok(operation);
            }
            let pending=pending_label(&config)?;
            if let Some(prior)=store.value(&pending)? {
                let prior=std::str::from_utf8(&prior)?;
                let bytes=store.value(&KeyOperation::storage_id(prior))?.context("pending receiving key intent is missing")?;
                let prior:KeyOperation=serde_json::from_slice(&bytes)?;
                ensure!(prior.completed.is_some(),"another receiving key operation is unresolved; reconcile its original intent first");
            }
            let current_label=current_key_label(&config)?;
            let current=store.value(&current_label)?.map(|bytes|serde_json::from_slice::<ReceivingMaterial>(&bytes)).transpose()?;
            let expected=if action==KeyAction::Revoke {BindingAction::Revoked}else{BindingAction::Active};
            let reusable=current.as_ref().is_some_and(|material|action==KeyAction::Register && material.binding.binding.action==BindingAction::Active
                && material.binding.binding.serial>registry.serial
                && material.binding.binding.valid_until>=paid_deadlines(now(),&config.chain.as_ref().expect("validated chain").profile).retain_until);
            let no_change=current.as_ref().is_some_and(|material|action!=KeyAction::Rotate && material.binding.binding.action==expected
                && material.binding.binding.serial==registry.serial && material.binding.binding.digest().ok()==Some(registry.binding_hash)
                && registry.active==(expected==BindingAction::Active)
                && (expected==BindingAction::Revoked || material.binding.binding.valid_until>=paid_deadlines(now(),&config.chain.as_ref().expect("validated chain").profile).retain_until));
            let material=if reusable || no_change {current.as_ref().context("receiving key material is unavailable")?} else {
                let serial=current.as_ref().map(|material|material.binding.binding.serial).unwrap_or(0).max(registry.serial).checked_add(1).context("receiving key serial exhausted")?;
                let material=self.new_receiving_material(serial,expected)?;
                let binding=material.binding.clone();
                let material_id=material_id(&binding.binding,intent);
                let operation=KeyOperation{intent_id:intent.into(),action,gas_cap:gas_cap.into(),profile_id:config.chain.as_ref().expect("validated chain").profile.digest()?,binding,material_id:material_id.clone(),completed:None};
                let encoded=Zeroizing::new(serde_json::to_vec(&material)?);
                let operation_bytes=serde_json::to_vec(&operation)?;
                let mut values=vec![(material_id.as_str(),encoded.as_slice()),(id.as_str(),operation_bytes.as_slice()),(pending.as_str(),intent.as_bytes())];
                if current.is_none() && action==KeyAction::Register {values.push((current_label.as_str(),encoded.as_slice()));}
                store.put_values(&values)?;
                return Ok(operation);
            };
            let operation=KeyOperation{intent_id:intent.into(),action,gas_cap:gas_cap.into(),profile_id:config.chain.as_ref().expect("validated chain").profile.digest()?,
                binding:material.binding.clone(),material_id:material_id(&material.binding.binding,intent),completed:if no_change {Some(KeyCompletion::Confirmed)}else{None}};
            let bytes=serde_json::to_vec(&operation)?;
            let material_bytes=Zeroizing::new(serde_json::to_vec(material)?);
            store.put_values(&[(id.as_str(),&bytes),(pending.as_str(),intent.as_bytes()),(operation.material_id.as_str(),material_bytes.as_slice())])?;
            Ok(operation)
        })
    }
    pub async fn local_key_operation(
        &self,
        action: KeyAction,
        intent: &str,
        gas_cap: &str,
    ) -> Result<Value> {
        ensure!(
            uuid::Uuid::parse_str(intent)?.to_string() == intent,
            "receiving key intent must be a canonical UUID"
        );
        ensure!(
            Amount::from_decimal_str(gas_cap)? != Amount::ZERO,
            "receiving key operation requires a positive gas cap"
        );
        self.ensure_mode(Mode::Paid)?;
        self.changes
            .send_modify(|generation| *generation = generation.wrapping_add(1));
        let _operation =
            tokio::time::timeout(std::time::Duration::from_secs(2), self.operations.lock())
                .await
                .context("another delivery operation is in progress; retry this same key intent")?;
        ensure!(
            matches!(
                self.wallet.lock().expect("delivery wallet").status(),
                wallet::WalletStatus::Unlocked { .. }
            ),
            "unlock the independent delivery wallet locally first"
        );
        let chain = self.chain().await?;
        if let Some(prior) = self.pending_key_operation()?
            && let Some(transaction) =
                self.with_store(|store| store.find_transaction(&prior.transaction_id()))?
        {
            let observed = chain.observe(&transaction).await?;
            self.with_store(|store| {
                store.observe_transaction(
                    &transaction.intent.id,
                    &observed.phase,
                    observed.block_number,
                    observed.block_hash.as_deref(),
                    observed.error.as_deref(),
                )
            })?;
            self.finish_key_operation(&chain, &transaction.intent.id, &observed.phase)
                .await?;
        }
        let peer = parse_word(&self.identity.device_id()?)?;
        let registry = chain.current_key(peer).await?;
        let operation = self.reserve_key_operation(intent, action, gas_cap, &registry)?;
        if registry_matches(&operation.binding.binding, &registry)? {
            self.apply_key_confirmation(operation.clone(), "confirmed", &registry)?;
            let transaction =
                self.with_store(|store| store.find_transaction(&operation.transaction_id()))?;
            let mut result = key_result(
                &operation,
                "confirmed",
                transaction.as_ref().and_then(|tx| tx.tx_hash.clone()),
                true,
            );
            result["transaction_phase"] = json!(transaction.map(|tx| tx.phase));
            return Ok(result);
        }
        if let Some(completion) = operation.completed {
            return Ok(key_result(
                &operation,
                if matches!(completion, KeyCompletion::Confirmed) {
                    "superseded"
                } else {
                    completion.phase()
                },
                None,
                false,
            ));
        }
        let config = self
            .config()
            .prover
            .context("local receiving key prover is not configured")?;
        let statement = Statement::key_binding(&config.profile, &operation.binding)?;
        let digest = hex::encode(statement.digest());
        let proof_id = format!("proof-seal:{digest}");
        self.with_store(|store| {
            store.put_value(&format!("local-spend:key-{intent}"), gas_cap.as_bytes())
        })?;
        let seal = if let Some(seal) = self.with_store(|store| store.value(&proof_id))? {
            seal
        } else {
            let input = ProofInput {
                profile: config.profile.clone(),
                statement: statement.clone(),
                witness: Witness::KeyBinding {
                    binding: operation.binding.clone(),
                },
            };
            let path = self.with_store(|store| store.reserve_proof(&digest))?;
            let seal = self.proofs.prove(&config, &input, &path).await?;
            self.with_store(|store| store.put_value(&proof_id, &seal))?;
            seal
        };
        let id = operation.transaction_id();
        let observed = chain
            .execute(
                self,
                &id,
                chain::Call::RegisterKey { statement, seal },
                gas_cap,
            )
            .await?;
        self.finish_key_operation(&chain, &id, &observed.phase)
            .await?;
        let registry = chain.current_key(peer).await?;
        let registered = registry_matches(&operation.binding.binding, &registry)?;
        let mut result = key_result(
            &operation,
            if registered {
                "confirmed"
            } else {
                &observed.phase
            },
            observed.tx_hash,
            registered,
        );
        result["transaction_phase"] = json!(observed.phase);
        Ok(result)
    }
    pub(super) async fn finish_key_operation(
        &self,
        chain: &chain::Chain,
        transaction: &str,
        phase: &str,
    ) -> Result<()> {
        let Some(intent) = transaction
            .strip_prefix("key-")
            .and_then(|id| id.strip_suffix("/register"))
        else {
            return Ok(());
        };
        if uuid::Uuid::parse_str(intent).is_err() {
            return Ok(());
        }
        let id = KeyOperation::storage_id(intent);
        let Some(bytes) = self.with_store(|store| store.value(&id))? else {
            return Ok(());
        };
        let mut operation: KeyOperation = serde_json::from_slice(&bytes)?;
        if !matches!(phase, "confirmed" | "reverted" | "conflicted" | "failed") {
            operation.completed = None;
            self.with_store(|store| store.put_value(&id, &serde_json::to_vec(&operation)?))?;
            return Ok(());
        }
        let current = chain
            .current_key(operation.binding.binding.receiver_peer)
            .await?;
        self.apply_key_confirmation(operation, phase, &current)
    }
    fn apply_key_confirmation(
        &self,
        mut operation: KeyOperation,
        phase: &str,
        current: &chain::KeyRecord,
    ) -> Result<()> {
        ensure!(
            operation.binding.binding.receiver_peer == parse_word(&self.identity.device_id()?)?,
            "receiving key operation belongs to another Node"
        );
        let id = KeyOperation::storage_id(&operation.intent_id);
        let confirmed = registry_matches(&operation.binding.binding, current)?;
        operation.completed = Some(if confirmed {
            KeyCompletion::Confirmed
        } else if phase == "confirmed" {
            KeyCompletion::Superseded
        } else {
            KeyCompletion::Failed
        });
        self.with_store(|store| {
            let bytes = serde_json::to_vec(&operation)?;
            if confirmed {
                let material = Zeroizing::new(
                    store
                        .value(&operation.material_id)?
                        .context("confirmed receiving key material is unavailable")?,
                );
                store.put_values(&[
                    (id.as_str(), &bytes),
                    (
                        current_key_label(&self.config())?.as_str(),
                        material.as_slice(),
                    ),
                ])?;
            } else {
                store.put_value(&id, &bytes)?;
            }
            Ok(())
        })
    }
}
fn registry_matches(binding: &KeyBinding, current: &chain::KeyRecord) -> Result<bool> {
    Ok(current.binding_hash == binding.digest()?
        && current.serial == binding.serial
        && current.active == (binding.action == BindingAction::Active)
        && current.valid_from == binding.valid_from
        && current.valid_until == binding.valid_until)
}
fn key_result(
    operation: &KeyOperation,
    phase: &str,
    tx_hash: Option<String>,
    registry_confirmed: bool,
) -> Value {
    json!({"intent_id":operation.intent_id,"action":operation.action,"phase":phase,
        "serial":operation.binding.binding.serial,"binding_hash":hex::encode(operation.binding.binding.digest().expect("verified binding")),
        "registry_confirmed":registry_confirmed,"tx_hash":tx_hash})
}

#[cfg(test)]
mod tests {
    use super::*;
    fn node() -> (Delivery, LocalIdentity, PathBuf) {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../.artifacts")
            .join(format!("receiving-key-{}", uuid::Uuid::new_v4()));
        crate::config::private_dir(&dir).unwrap();
        let identity = LocalIdentity::generate();
        let node = Delivery::open(&dir, identity.clone()).unwrap();
        node.configure_local(DeliveryConfig {
            enabled: true,
            paid_enabled: true,
            charity_enabled: false,
            chain: Some(chain::tests::config()),
            ..Default::default()
        })
        .unwrap();
        (node, identity, dir)
    }
    fn record(binding: &KeyBinding) -> chain::KeyRecord {
        chain::KeyRecord {
            binding_hash: binding.digest().unwrap(),
            serial: binding.serial,
            valid_from: binding.valid_from,
            valid_until: binding.valid_until,
            active: binding.action == BindingAction::Active,
        }
    }
    #[test]
    fn receiving_key_intents_keep_the_same_binding_across_retries_and_restarts() {
        let (node, identity, dir) = node();
        let original = node.key_binding(Mode::Paid).unwrap();
        let current = record(&original.binding);
        let intent = uuid::Uuid::new_v4().to_string();
        let operation = node
            .reserve_key_operation(&intent, KeyAction::Rotate, "1000000000000000", &current)
            .unwrap();
        assert_eq!(
            operation.binding.binding.serial,
            original.binding.serial + 1
        );
        operation.binding.verify().unwrap();
        let profile = node.config().chain.unwrap().profile;
        let statement = Statement::key_binding(&profile, &operation.binding).unwrap();
        vibeface_delivery::verify_relation(
            &profile,
            &statement,
            &Witness::KeyBinding {
                binding: operation.binding.clone(),
            },
        )
        .unwrap();
        assert!(
            node.reserve_key_operation(&intent, KeyAction::Revoke, "1000000000000000", &current)
                .is_err()
        );
        assert!(
            node.reserve_key_operation(&intent, KeyAction::Rotate, "2000000000000000", &current)
                .is_err()
        );
        assert!(
            node.reserve_key_operation(
                &uuid::Uuid::new_v4().to_string(),
                KeyAction::Rotate,
                "1000000000000000",
                &current
            )
            .is_err()
        );
        drop(node);
        let node = Delivery::open(&dir, identity).unwrap();
        let retry = node
            .reserve_key_operation(&intent, KeyAction::Rotate, "1000000000000000", &current)
            .unwrap();
        assert_eq!(retry.binding, operation.binding);
        assert_eq!(retry.transaction_id(), operation.transaction_id());
        assert_eq!(
            Statement::key_binding(&profile, &retry.binding).unwrap(),
            statement
        );
        assert_eq!(
            receiving_public(&node.receiver_secret(original.binding.key_id).unwrap()).unwrap(),
            original.binding.receiver_public
        );
        assert!(!dir.join("delivery-wallet.json").exists());
        drop(node);
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn key_confirmation_projects_rotation_and_revocation_without_erasing_held_keys() {
        let (node, _, dir) = node();
        let old = node.key_binding(Mode::Paid).unwrap();
        let rotate = node
            .reserve_key_operation(
                &uuid::Uuid::new_v4().to_string(),
                KeyAction::Rotate,
                "1000000000000000",
                &record(&old.binding),
            )
            .unwrap();
        assert_eq!(node.key_binding(Mode::Paid).unwrap(), old);
        node.apply_key_confirmation(
            rotate.clone(),
            "confirmed",
            &record(&rotate.binding.binding),
        )
        .unwrap();
        assert_eq!(node.key_binding(Mode::Paid).unwrap(), rotate.binding);
        let revoke = node
            .reserve_key_operation(
                &uuid::Uuid::new_v4().to_string(),
                KeyAction::Revoke,
                "1000000000000000",
                &record(&rotate.binding.binding),
            )
            .unwrap();
        assert_eq!(
            revoke.binding.binding.serial,
            rotate.binding.binding.serial + 1
        );
        assert_eq!(revoke.binding.binding.key_id, [0; 32]);
        assert_eq!(revoke.binding.binding.receiver_public, [0; 32]);
        revoke.binding.verify().unwrap();
        assert_eq!(node.key_binding(Mode::Paid).unwrap(), rotate.binding);
        node.apply_key_confirmation(
            revoke.clone(),
            "confirmed",
            &record(&revoke.binding.binding),
        )
        .unwrap();
        assert_eq!(
            node.key_binding(Mode::Paid).unwrap().binding.action,
            BindingAction::Revoked
        );
        assert!(node.status().unwrap()["receiver_key"].is_null());
        assert_eq!(
            node.key_binding(Mode::Paid).unwrap().binding.serial,
            revoke.binding.binding.serial
        );
        for binding in [&old.binding, &rotate.binding.binding] {
            assert_eq!(
                receiving_public(&node.receiver_secret(binding.key_id).unwrap()).unwrap(),
                binding.receiver_public
            );
        }
        let reactivate = node
            .reserve_key_operation(
                &uuid::Uuid::new_v4().to_string(),
                KeyAction::Register,
                "1000000000000000",
                &record(&revoke.binding.binding),
            )
            .unwrap();
        assert_eq!(reactivate.binding.binding.action, BindingAction::Active);
        assert_eq!(
            reactivate.binding.binding.serial,
            revoke.binding.binding.serial + 1
        );
        assert!(node.status().unwrap()["receiver_key"].is_null());
        drop(node);
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn another_keeper_registering_the_exact_key_does_not_hide_confirmed_registry_state() {
        let (node, _, dir) = node();
        let old = node.key_binding(Mode::Paid).unwrap();
        let operation = node
            .reserve_key_operation(
                &uuid::Uuid::new_v4().to_string(),
                KeyAction::Rotate,
                "1000000000000000",
                &record(&old.binding),
            )
            .unwrap();
        node.apply_key_confirmation(
            operation.clone(),
            "reverted",
            &record(&operation.binding.binding),
        )
        .unwrap();
        assert_eq!(node.key_binding(Mode::Paid).unwrap(), operation.binding);
        assert_eq!(
            node.pending_key_operation()
                .unwrap()
                .unwrap()
                .completed
                .unwrap()
                .phase(),
            "confirmed"
        );
        drop(node);
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn failed_or_superseded_key_actions_do_not_publish_a_new_local_key() {
        let (node, _, dir) = node();
        let old = node.key_binding(Mode::Paid).unwrap();
        let operation = node
            .reserve_key_operation(
                &uuid::Uuid::new_v4().to_string(),
                KeyAction::Rotate,
                "1000000000000000",
                &record(&old.binding),
            )
            .unwrap();
        node.apply_key_confirmation(operation.clone(), "reverted", &record(&old.binding))
            .unwrap();
        assert_eq!(node.key_binding(Mode::Paid).unwrap(), old);
        let pending = node.pending_key_operation().unwrap().unwrap();
        assert_eq!(pending.completed.unwrap().phase(), "failed");
        let mut foreign = record(&operation.binding.binding);
        foreign.serial += 1;
        node.apply_key_confirmation(operation, "confirmed", &foreign)
            .unwrap();
        assert_eq!(node.key_binding(Mode::Paid).unwrap(), old);
        assert_eq!(
            node.pending_key_operation()
                .unwrap()
                .unwrap()
                .completed
                .unwrap()
                .phase(),
            "superseded"
        );
        drop(node);
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[tokio::test]
    async fn locked_key_actions_fail_before_creating_material_or_reserving_a_nonce() {
        let (node, _, dir) = node();
        let error = node
            .local_key_operation(
                KeyAction::Rotate,
                &uuid::Uuid::new_v4().to_string(),
                "1000000000000000",
            )
            .await
            .unwrap_err();
        assert!(error.to_string().contains("unlock"));
        assert!(!dir.join("delivery/service.sqlite3").exists());
        assert!(!dir.join("delivery-wallet.json").exists());
        drop(node);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
