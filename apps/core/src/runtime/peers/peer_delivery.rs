use super::*;

impl Runtime {
    pub(super) async fn outbox_loop(self: Arc<Self>) {
        loop {
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_secs(3)) => {},
                _ = self.outbox_wakeup.notified() => {},
            }
            let Ok(state) = self.membership_state().await else {
                continue;
            };
            let network = &state.genesis.network_id;
            let (outbox, works, commands) = tokio::join!(
                self.deliver_outbox(),
                self.deliver_work_requests(network),
                self.drive_peer_commands(network)
            );
            if let Err(error) = commands {
                tracing::debug!(%error, "peer command admission pending");
            }
            if let Err(error) = outbox {
                tracing::debug!(%error, "scheduler delivery pending");
            }
            if let Err(error) = works {
                tracing::debug!(%error, "work requests wait for their devices");
            }
            if let Some(guard) = self.peer_delivery_locks.try_lock("reconcile_works") {
                let (node, network) = (self.clone(), network.to_owned());
                if let Err(error) = self.background_tasks.spawn(async move {
                    let _guard = guard;
                    if let Err(error) = node.reconcile_works(&network).await {
                        tracing::debug!(%error, "work reconciliation pending");
                    }
                    if let Err(error) = node.reconcile_checks(&network).await {
                        tracing::debug!(%error, "pending check reconciliation pending");
                    }
                }) {
                    tracing::debug!(%error, "work reconciliation stopped");
                }
            }
        }
    }

    pub(super) async fn deliver_outbox(self: &Arc<Self>) -> Result<()> {
        let snapshot = self.store.call("peers.outbox_lanes", move |store| {
            let Some(network) = store.network()? else {
                return Ok(None);
            };
            let mut lanes: BTreeMap<String, Vec<_>> = BTreeMap::new();
            for entry in store.scheduler_outbox()? {
                let metadata = (|| -> Result<_> {
                    let target = store.peer_delivery_target(
                        &network,
                        &entry.conversation_id,
                        &entry.trigger,
                    )?;
                    let lane = serde_json::to_string(&(&network, &entry.conversation_id, target))?;
                    let (_, sequence, _) =
                        store.peer_source(&network, &entry.conversation_id, &entry.trigger)?;
                    Ok((lane, sequence))
                })();
                match metadata {
                    Ok((lane, sequence)) => {
                        lanes.entry(lane).or_default().push((sequence, entry));
                    }
                    Err(error) => {
                        tracing::debug!(%error, conversation_id = entry.conversation_id, trigger = entry.trigger, "delivery source pending")
                    }
                }
            }
            Ok(Some((network, lanes)))
                }).await?;
        let Some((network, lanes)) = snapshot else {
            return Ok(());
        };
        for (lane, mut entries) in lanes {
            let Some(guard) = self
                .peer_delivery_locks
                .try_lock(&format!("outbound:{lane}"))
            else {
                continue;
            };
            entries.sort_by_key(|(sequence, _)| *sequence);
            let (node, network) = (self.clone(), network.clone());
            self.background_tasks.spawn(async move {
                let _guard = guard;
                let mut progressed = false;
                for (_, entry) in entries {
                    match node.deliver_outbox_entry(&network, &entry).await {
                        Ok(attempted) => progressed |= attempted,
                        Err(error) => {
                            tracing::debug!(%error, "target delivery pending");
                            break;
                        }
                    }
                }
                drop(_guard);
                if progressed {
                    node.outbox_wakeup.notify_one();
                }
            })?;
        }
        Ok(())
    }

    pub(super) async fn deliver_outbox_entry(
        self: &Arc<Self>,
        network: &str,
        entry: &crate::scheduling::OutboxEntry,
    ) -> Result<bool> {
        let command = {
            let network = network.to_owned();
            let entry = entry.clone();
            self.store
                .call("peers.delivery_attempt", move |store| {
                    if !store.mailbox_before_attempt(
                        &network,
                        &entry.conversation_id,
                        &entry.trigger,
                        now_ms(),
                    )? {
                        ensure!(
                            store.peer_delivery_saved(
                                &network,
                                &entry.conversation_id,
                                &entry.trigger
                            )?,
                            "target waits for earlier delivery reconciliation"
                        );
                        return Ok(None);
                    }
                    Ok(Some(store.peer_delivery_command(
                        &network,
                        &entry.conversation_id,
                        &entry.trigger,
                    )?))
                })
                .await?
        };
        let Some(command) = command else {
            return Ok(false);
        };
        let command_id = command.envelope.command_id.clone();
        let local = self.identity.device_id()?;
        let delivered = if command.target_node == local {
            self.accept_peer_envelope(network, &local, command.envelope)
                .await
        } else {
            self.peer_request(
                network,
                &command.target_node,
                json!({"type":"deliver","command":command.envelope}),
            )
            .await
        };
        let receipt_matches = match &delivered {
            Ok(reply) if reply["admission"]["state"] == "owner_accepted" => {
                let arg0 = (network).to_owned();
                let arg1 = (entry.conversation_id).to_owned();
                let arg2 = (entry.trigger).to_owned();
                let arg3 = (reply["receipt"]["trigger"].as_str().unwrap_or_default()).to_owned();
                self.store
                    .call("peers.peer_delivery_receipt_matches", move |store| {
                        store.peer_delivery_receipt_matches(&arg0, &arg1, &arg2, &arg3)
                    })
                    .await
            }
            _ => Ok(true),
        };
        let delivered = delivered.and_then(|reply| {
            ensure!(
                reply["admission"]["command_id"] == command_id,
                "delivery answer belongs to another command"
            );
            let state = reply["admission"]["state"]
                .as_str()
                .context("delivery answer had no admission state")?;
            ensure!(
                matches!(
                    state,
                    "saved" | "waiting_dependencies" | "owner_accepted" | "rejected"
                ),
                "invalid owner admission state"
            );
            if state == "owner_accepted" {
                let receipt = &reply["receipt"];
                ensure!(
                    receipt["network_id"] == network
                        && receipt["conversation_id"] == entry.conversation_id
                        && receipt_matches?,
                    "delivery answer had no matching owner receipt"
                );
            }
            Ok(reply)
        });
        match delivered {
            Ok(delivered) if delivered["admission"]["state"] == "owner_accepted" => {
                {
                    let arg0 = (network).to_owned();
                    let arg1 = (entry.conversation_id).to_owned();
                    let arg2 = (entry.trigger).to_owned();
                    self.store
                        .call("peers.mailbox_delivered", move |store| {
                            store.mailbox_delivered(&arg0, &arg1, &arg2, now_ms())
                        })
                        .await
                }?;
                self.publish_changed(
                    network,
                    &entry.conversation_id,
                    Changed::Job(&entry.trigger),
                );
                Ok(true)
            }
            Ok(delivered) => {
                let state = delivered["admission"]["state"]
                    .as_str()
                    .context("delivery answer had no admission state")?;
                let detail = delivered["rejected"].as_str().unwrap_or(state);
                if state == "rejected" {
                    {
                        let arg0 = (network).to_owned();
                        let arg1 = (entry.conversation_id).to_owned();
                        let arg2 = (entry.trigger).to_owned();
                        let arg3 = (detail).to_owned();
                        self.store
                            .call("peers.mailbox_rejected", move |store| {
                                store.mailbox_rejected(&arg0, &arg1, &arg2, &arg3, now_ms())
                            })
                            .await
                    }?;
                    return Ok(true);
                }
                {
                    let arg0 = (network).to_owned();
                    let arg1 = (entry.conversation_id).to_owned();
                    let arg2 = (entry.trigger).to_owned();
                    let arg3 = false;
                    let arg4 = (detail).to_owned();
                    self.store
                        .call("peers.mailbox_failed", move |store| {
                            store.mailbox_failed(&arg0, &arg1, &arg2, arg3, &arg4, now_ms())
                        })
                        .await
                }?;
                {
                    let arg0 = (network).to_owned();
                    let arg1 = (command_id).to_owned();
                    self.store
                        .call("peers.mark_peer_saved", move |store| {
                            store.mark_peer_saved(&arg0, &arg1)
                        })
                        .await
                }?;
                Ok(true)
            }
            Err(error) => {
                {
                    let arg0 = (network).to_owned();
                    let arg1 = (entry.conversation_id).to_owned();
                    let arg2 = (entry.trigger).to_owned();
                    let arg3 = net::Unanswered::is(&error);
                    let arg4 = (error.to_string()).to_owned();
                    self.store
                        .call("peers.mailbox_failed", move |store| {
                            store.mailbox_failed(&arg0, &arg1, &arg2, arg3, &arg4, now_ms())
                        })
                        .await
                }?;
                Err(error)
            }
        }
    }

    pub(super) async fn accept_peer_command(
        self: &Arc<Self>,
        network: &str,
        origin: &str,
        input: &Value,
    ) -> Result<Value> {
        let envelope: PeerEnvelope = serde_json::from_value(input["command"].clone())?;
        ensure!(
            matches!(
                (field(input, "type")?, &envelope.payload),
                ("deliver", PeerCommand::Deliver { .. })
                    | ("dispatch_work", PeerCommand::Dispatch { .. })
                    | ("interrupt_work", PeerCommand::Stop { .. })
            ),
            "RPC does not match its command"
        );
        self.accept_peer_envelope(network, origin, envelope).await
    }

    pub(super) async fn accept_peer_envelope(
        self: &Arc<Self>,
        network: &str,
        origin: &str,
        envelope: PeerEnvelope,
    ) -> Result<Value> {
        envelope.validate(network, origin, &self.identity.device_id()?)?;
        let admission = {
            let arg0 = (network).to_owned();
            let arg1 = (origin).to_owned();
            let arg2 = (envelope).to_owned();
            self.store
                .call("peers.save_peer_command", move |store| {
                    store.save_peer_command(&arg0, &arg1, &arg2, now_ms())
                })
                .await
        }?;
        self.drive_peer_commands(network).await?;
        Ok(admission.reply())
    }

    pub(super) async fn drive_peer_commands(self: &Arc<Self>, network: &str) -> Result<()> {
        let pending = {
            let arg0 = (network).to_owned();
            self.store
                .call("peers.pending_peer_commands", move |store| {
                    store.pending_peer_commands(&arg0)
                })
                .await
        }?;
        let mut lanes = BTreeSet::new();
        for command in pending {
            lanes.insert(command.envelope.lane(network));
        }
        for lane in lanes {
            let Some(guard) = self
                .peer_delivery_locks
                .try_lock(&format!("inbound:{lane}"))
            else {
                continue;
            };
            let (node, network) = (self.clone(), network.to_owned());
            self.background_tasks.spawn(async move {
                let _guard = guard;
                let mut drained = false;
                let mut progressed = false;
                loop {
                    let pending = {
                        let arg0 = (network).to_owned();
                        node.store.call("peers.pending_peer_commands", move |store| store.pending_peer_commands(&arg0)).await
                    };
                    let command = match pending {
                        Ok(pending) => pending
                            .into_iter()
                            .find(|command| command.envelope.lane(&network) == lane),
                        Err(error) => {
                            tracing::warn!(%error, "peer command scan pending");
                            break;
                        }
                    };
                    let Some(command) = command else {
                        drained = true;
                        break;
                    };
                    match node.process_peer_command(&network, &command).await {
                        Ok(true) => progressed = true,
                        Ok(false) => break,
                        Err(error) => {
                            tracing::warn!(%error, command_id = command.envelope.command_id, "peer command persistence pending");
                            break;
                        }
                    }
                }
                drop(_guard);
                if drained && progressed {
                    node.outbox_wakeup.notify_one();
                }
            })?;
        }
        Ok(())
    }

    pub(super) async fn process_peer_command(
        self: &Arc<Self>,
        network: &str,
        command: &PeerAdmission,
    ) -> Result<bool> {
        let envelope = &command.envelope;
        let result: Result<Option<Value>> = async {
            let checked = {
                let identity = self.identity.clone();
                let network = network.to_owned();
                let envelope = envelope.clone();
                let command = command.clone();
                self.store.call("peers.check_command", move |store| {
                let local = identity.device_id()?;
                authorize_peer_node_from(&store.connection, &network, &local)?;
                if let Some(proof) = store.peer_owner_proof(&network, &local, &envelope)? {
                    return Ok(Some(Some(proof)));
                }
                authorize_peer_node_from(&store.connection, &network, &command.origin)?;
                if !store.peer_command_dependencies(&network, &envelope)? { return Ok(Some(None)); }

                    Ok(None)
                }).await?
            };
            if let Some(result) = checked { return Ok(result); }

            match &envelope.payload {
                PeerCommand::Deliver { conversation_id, trigger } => {
                    if let Ok(crate::scheduling::Trigger::Message(message_id)) = crate::scheduling::Trigger::decode(trigger)
                        && let Some(receipt) = self.accept_held_answer_delivery(network, conversation_id, message_id).await? {
                            return Ok(Some(json!({"receipt":receipt})));
                        }
                    let receipt = {
                        let local = self.identity.device_id()?;
                        let network = network.to_owned();
                        let command = command.clone();
                        let envelope = envelope.clone();
                        let conversation_id = conversation_id.clone();
                        let trigger = trigger.clone();
                        self.store.call("peers.admit_delivery", move |store| {
                        authorize_peer_node_from(&store.connection, &network, &command.origin)?;
                        if !store.peer_command_dependencies(&network, &envelope)? { return Ok(None); }
                        store.admit_delivery(&local, &network, &conversation_id, &trigger)
                        }).await?
                    };
                    if let Some(receipt) = receipt {
                        tracing::info!(event = "input.delivered", conversation_id, trigger, origin = command.origin, state = receipt.state, "another Node delivered an input");
                        self.schedule_wakeup.notify_one();
                        self.publish_changed(network, conversation_id, Changed::Job(trigger));
                        Ok(Some(json!({"receipt":receipt})))
                    } else { Ok(None) }
                }
                PeerCommand::Dispatch { spec } => {
                    let admitted = self.accept_dispatch(network, spec.as_ref().clone(), &Value::Null).await?;
                    Ok(Some(json!({"work_id":admitted.record.work_id,"state":admitted.record.state.name()})))
                }
                PeerCommand::Stop { conversation_id, work_id } => {
                    let record = {
                        let arg0 = (work_id).to_owned();
                        self.store.call("peers.work_record", move |store| store.work_record(&arg0)).await
                    }?;
                    let Some(record) = record else {
                        return Ok(Some(json!({"work_id":work_id,"interrupt_requested":true,"confirmed":false,"waiting_for_start":true})));
                    };
                    ensure!(record.conversation == *conversation_id, AdmissionRejected::new("work belongs to another conversation"));
                    self.interrupt_work(network, work_id).await.map(Some)
                }
            }
        }.await;
        let (state, response) = match result {
            Ok(Some(response)) => (PeerAdmissionState::OwnerAccepted, response),
            Ok(None) => {
                self.wake_sync(&command.origin);
                (PeerAdmissionState::WaitingDependencies, json!({}))
            }
            Err(error)
                if error.is::<AdmissionRejected>()
                    || error
                        .downcast_ref::<vibeface_bridge::failure::ExecFailure>()
                        .is_some_and(|failure| {
                            failure.kind == vibeface_bridge::failure::FailureKind::NoBot
                        }) =>
            {
                (
                    PeerAdmissionState::Rejected,
                    json!({"rejected":error.to_string(),
                "failure":error.downcast_ref::<vibeface_bridge::failure::ExecFailure>().map(|failure| failure.kind)}),
                )
            }
            Err(error) => return Err(error),
        };
        {
            let arg0 = (network).to_owned();
            let arg1 = (envelope.command_id).to_owned();
            let arg2 = state;
            let arg3 = response;
            self.store
                .call("peers.finish_peer_command", move |store| {
                    store.finish_peer_command(&arg0, &arg1, arg2, arg3)
                })
                .await
        }?;
        Ok(state != PeerAdmissionState::WaitingDependencies)
    }
}
