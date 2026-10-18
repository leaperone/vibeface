use super::*;

fn valid_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || "-_.:".contains(character))
}

fn validate_definition(
    bot_id: &str,
    conversation_id: &str,
    name: &str,
    instructions: &str,
    project: Option<&BotProject>,
) -> Result<()> {
    ensure!(valid_id(bot_id), "invalid bot_id");
    ensure!(valid_id(conversation_id), "invalid conversation_id");
    ensure!(
        !name.trim().is_empty() && name.len() <= 512 && !name.chars().any(char::is_control),
        "invalid Bot name"
    );
    ensure!(instructions.len() <= 65536, "Bot instructions are too long");
    if let Some(project) = project {
        ensure!(
            valid_id(&project.target_node),
            "invalid project target_node"
        );
        let cwd = Path::new(&project.cwd);
        ensure!(
            project.cwd.len() <= 4096
                && cwd.is_absolute()
                && !project.cwd.chars().any(char::is_control)
                && !cwd
                    .components()
                    .any(|part| matches!(part, Component::ParentDir | Component::CurDir)),
            "Bot project must use an absolute directory without parent components"
        );
    }
    Ok(())
}

fn validate_gateway_proof_from(
    connection: &Connection,
    network: &str,
    author: &str,
    proof: &NetworkState,
) -> Result<()> {
    let encoded: String = connection.query_row(
        "SELECT bundle FROM networks WHERE network_id=?",
        [network],
        |row| row.get(0),
    )?;
    let trusted: NetworkState = serde_json::from_str(&encoded)?;
    ensure!(
        proof.genesis.network_id == network && proof.genesis == trusted.genesis,
        "Bot Gateway role proof belongs to another personal network"
    );
    proof
        .project()?
        .authorize_node(author, Some(NodeRole::Gateway))?;
    Ok(())
}

fn project_bot(hash: &str, event: &Event) -> Option<Bot> {
    let EventBody::BotCreated {
        bot_id,
        name,
        instructions,
        project,
        ..
    } = &event.body
    else {
        return None;
    };
    Some(Bot {
        bot_id: bot_id.clone(),
        conversation_id: event.scope_id.clone(),
        name: name.clone(),
        instructions: instructions.clone(),
        project: project.clone(),
        event_hash: hash.into(),
    })
}

fn bots_from(connection: &Connection, network: &str) -> Result<Vec<Bot>> {
    let events = load_events_from(connection, network)?;
    let mut bots = Vec::new();
    let mut identifiers = BTreeSet::new();
    let mut scopes = BTreeSet::new();
    for (hash, stored) in &events {
        if stored.status != "ready" {
            continue;
        }
        let Some(bot) = project_bot(hash, &stored.event) else {
            continue;
        };
        validate_definition(
            &bot.bot_id,
            &bot.conversation_id,
            &bot.name,
            &bot.instructions,
            bot.project.as_ref(),
        )?;
        let EventBody::BotCreated {
            membership_role_proof,
            ..
        } = &stored.event.body
        else {
            unreachable!()
        };
        validate_gateway_proof_from(
            connection,
            network,
            &stored.event.author,
            membership_role_proof,
        )?;
        ensure!(
            identifiers.insert(bot.bot_id.clone()) && scopes.insert(bot.conversation_id.clone()),
            "Bot history contains conflicting identities"
        );
        bots.push(bot);
    }
    bots.sort_by(|left, right| left.bot_id.cmp(&right.bot_id));
    Ok(bots)
}

pub(super) fn validate_bot_projections_from(
    connection: &Connection,
    network: &str,
    events: &mut BTreeMap<String, StoredEvent>,
    ordered: &[String],
) {
    let mut identifiers: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut scopes: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut invalid = BTreeSet::new();
    for hash in ordered {
        let stored = &events[hash];
        if stored.status != "ready" {
            continue;
        }
        let Some(bot) = project_bot(hash, &stored.event) else {
            continue;
        };
        let EventBody::BotCreated {
            membership_role_proof,
            ..
        } = &stored.event.body
        else {
            unreachable!()
        };
        identifiers
            .entry(bot.bot_id.clone())
            .or_default()
            .push(hash.clone());
        scopes
            .entry(bot.conversation_id.clone())
            .or_default()
            .push(hash.clone());
        let ancestors = closure_hashes(events, &stored.event.parents);
        let conversation_exists = ancestors.iter().any(|ancestor| {
            events.get(ancestor).is_some_and(|parent| {
                parent.status == "ready"
                    && parent.event.scope_id == bot.conversation_id
                    && matches!(parent.event.body, EventBody::ConversationCreated { .. })
            })
        });
        if !conversation_exists
            || validate_definition(
                &bot.bot_id,
                &bot.conversation_id,
                &bot.name,
                &bot.instructions,
                bot.project.as_ref(),
            )
            .is_err()
            || validate_gateway_proof_from(
                connection,
                network,
                &stored.event.author,
                membership_role_proof,
            )
            .is_err()
        {
            invalid.insert(hash.clone());
        }
    }
    for hashes in identifiers.values().chain(scopes.values()) {
        if hashes.len() > 1 {
            invalid.extend(hashes.iter().cloned());
        }
    }
    for hash in ordered {
        if invalid.contains(hash)
            || dependencies(&events[hash].event).any(|dependency| invalid.contains(dependency))
        {
            invalid.insert(hash.clone());
            events.get_mut(hash).expect("ordered event exists").status = "invalid".into();
        }
    }
}

impl Store {
    pub fn create_bot(
        &mut self,
        identity: &LocalIdentity,
        network: &str,
        conversation: &str,
        bot_id: &str,
        name: &str,
        instructions: &str,
        project: Option<BotProject>,
        created_at_ms: i64,
    ) -> Result<Bot> {
        validate_definition(bot_id, conversation, name, instructions, project.as_ref())?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        authorize_node_from(&tx, network, &identity.node_id()?, Some(NodeRole::Gateway))?;
        let membership_role_proof: NetworkState = serde_json::from_str(&tx.query_row(
            "SELECT bundle FROM networks WHERE network_id=?",
            [network],
            |row| row.get::<_, String>(0),
        )?)?;
        let bots = bots_from(&tx, network)?;
        if let Some(existing) = bots.iter().find(|bot| bot.bot_id == bot_id) {
            ensure!(
                existing.conversation_id == conversation
                    && existing.name == name
                    && existing.instructions == instructions
                    && serde_json::to_value(&existing.project)? == serde_json::to_value(&project)?,
                "Bot ID already exists with another definition"
            );
            return Ok(existing.clone());
        }
        ensure!(
            !bots.iter().any(|bot| bot.conversation_id == conversation),
            "conversation already belongs to another Bot"
        );
        let events = load_events_from(&tx, network)?;
        let heads = heads_from_events(&events, conversation);
        if heads.is_empty() {
            append_event_from(
                &tx,
                identity,
                network,
                conversation,
                EventBody::ConversationCreated { title: name.into() },
                vec![],
                created_at_ms,
            )?;
        } else {
            let history = closure_hashes(&events, &heads);
            ensure!(
                history
                    .iter()
                    .all(|hash| events.get(hash).is_some_and(|stored| {
                        stored.status == "ready" && stored.event.scope_id == conversation
                    })),
                "conversation has incomplete or conflicting history"
            );
            ensure!(
                history.iter().any(|hash| {
                    matches!(
                        events[hash].event.body,
                        EventBody::ConversationCreated { .. }
                    )
                }),
                "conversation does not exist"
            );
        }
        let event = append_event_from(
            &tx,
            identity,
            network,
            conversation,
            EventBody::BotCreated {
                bot_id: bot_id.into(),
                name: name.into(),
                instructions: instructions.into(),
                project,
                membership_role_proof,
            },
            vec![],
            created_at_ms,
        )?;
        let bot = bots_from(&tx, network)?
            .into_iter()
            .find(|bot| bot.event_hash == event.hash)
            .context("created Bot could not be projected")?;
        tx.commit()?;
        Ok(bot)
    }

    pub fn bots(&self, network: &str) -> Result<Vec<Bot>> {
        bots_from(&self.connection, network)
    }

    pub fn bot_for_conversation(&self, network: &str, scope: &str) -> Result<Option<Bot>> {
        Ok(self
            .bots(network)?
            .into_iter()
            .find(|bot| bot.conversation_id == scope))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn memory(owner: &LocalIdentity) -> Result<Store> {
        let mut store = Store::open(":memory:")?;
        store.install_trust_bundle(
            &owner.fixture_network_id()?,
            &owner.sign_network_fixture(
                1,
                vec![owner.certificate(vec![NodeRole::Gateway, NodeRole::Replica])?],
            )?,
        )?;
        Ok(store)
    }

    #[test]
    fn retries_preserve_the_complete_definition_and_scope_isolation() -> Result<()> {
        let owner = LocalIdentity::generate();
        let network = owner.fixture_network_id()?;
        let mut store = memory(&owner)?;
        let project = Some(BotProject {
            target_node: owner.node_id()?,
            cwd: "/project".into(),
        });
        let first = store.create_bot(
            &owner,
            &network,
            "first-history",
            "first-bot",
            "First",
            "Review changes",
            project.clone(),
            1,
        )?;
        let retry = store.create_bot(
            &owner,
            &network,
            "first-history",
            "first-bot",
            "First",
            "Review changes",
            project.clone(),
            9,
        )?;
        assert_eq!(first.event_hash, retry.event_hash);
        for (scope, name, instructions, project) in [
            (
                "different-history",
                "First",
                "Review changes",
                project.clone(),
            ),
            (
                "first-history",
                "Changed",
                "Review changes",
                project.clone(),
            ),
            ("first-history", "First", "Change changes", project.clone()),
            ("first-history", "First", "Review changes", None),
        ] {
            assert!(
                store
                    .create_bot(
                        &owner,
                        &network,
                        scope,
                        "first-bot",
                        name,
                        instructions,
                        project,
                        10,
                    )
                    .is_err()
            );
        }
        assert!(
            store
                .create_bot(
                    &owner,
                    &network,
                    "first-history",
                    "second-bot",
                    "Second",
                    "",
                    None,
                    11,
                )
                .is_err()
        );
        let second = store.create_bot(
            &owner,
            &network,
            "second-history",
            "second-bot",
            "Second",
            "",
            None,
            12,
        )?;
        assert_ne!(first.conversation_id, second.conversation_id);
        assert_eq!(store.conversations(&network)?.len(), 2);
        assert_eq!(store.bots(&network)?.len(), 2);
        assert!(store.bots("another-network")?.is_empty());
        let count: i64 = store.connection.query_row(
            "SELECT count(*) FROM events WHERE network_id=?",
            [&network],
            |row| row.get(0),
        )?;
        assert_eq!(count, 4);
        Ok(())
    }

    #[test]
    fn explicit_adoption_keeps_the_original_history() -> Result<()> {
        let owner = LocalIdentity::generate();
        let network = owner.fixture_network_id()?;
        let mut store = memory(&owner)?;
        store.append_event(
            &owner,
            &network,
            "existing-history",
            EventBody::ConversationCreated {
                title: "Old discussion".into(),
            },
            vec![],
            1,
        )?;
        let message = store.append_event(
            &owner,
            &network,
            "existing-history",
            EventBody::MessagePosted {
                message_id: "original-message".into(),
                scheduler_node: None,
                role: "user".into(),
                text: "Keep this discussion".into(),
            },
            vec![],
            2,
        )?;
        let bot = store.create_bot(
            &owner,
            &network,
            "existing-history",
            "adopted-bot",
            "New assistant",
            "Continue this discussion",
            None,
            3,
        )?;
        assert_eq!(bot.conversation_id, "existing-history");
        let messages = store.messages(&network, "existing-history")?;
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].event_hash, message.hash);
        assert_eq!(messages[0].message_id, "original-message");
        assert_eq!(store.conversations(&network)?[0].title, "Old discussion");
        Ok(())
    }

    #[test]
    fn signed_gateway_snapshot_preserves_bot_after_recovery_and_import() -> Result<()> {
        let owner = LocalIdentity::generate();
        let replica = LocalIdentity::generate();
        let network = owner.fixture_network_id()?;
        let mut source = memory(&owner)?;
        let original = source.create_bot(
            &owner,
            &network,
            "history",
            "bot",
            "Assistant",
            "Preserve signed responsibilities",
            None,
            1,
        )?;
        let mut retired = owner.certificate(vec![NodeRole::Gateway, NodeRole::Replica])?;
        retired.revoked = true;
        let recovered = owner.sign_network_fixture(
            2,
            vec![retired, replica.certificate(vec![NodeRole::Replica])?],
        )?;
        source.install_trust_bundle(&network, &recovered)?;
        source.rebuild_projections(&network)?;
        assert_eq!(source.bots(&network)?[0].event_hash, original.event_hash);
        assert!(
            source
                .authorize_node(&network, &owner.node_id()?, Some(NodeRole::Gateway))
                .is_err()
        );
        let mut imported = Store::open(":memory:")?;
        imported.install_trust_bundle(&network, &recovered)?;
        for stored in load_events_from(&source.connection, &network)?.into_values() {
            imported.ingest_event(&network, &stored.envelope)?;
        }
        imported.rebuild_projections(&network)?;
        let bot = imported.bot_for_conversation(&network, "history")?.unwrap();
        assert_eq!(bot.event_hash, original.event_hash);
        assert_eq!(bot.instructions, "Preserve signed responsibilities");
        Ok(())
    }

    #[test]
    fn foreign_or_forged_gateway_proof_cannot_create_a_bot() -> Result<()> {
        let owner = LocalIdentity::generate();
        let stranger = LocalIdentity::generate();
        let network = owner.fixture_network_id()?;
        let mut store = memory(&owner)?;
        let valid = store.trust_bundle(&network)?.unwrap();
        let foreign = stranger.sign_network_fixture(
            1,
            vec![owner.certificate(vec![NodeRole::Gateway, NodeRole::Replica])?],
        )?;
        let mut forged = valid.clone();
        forged.checkpoint.as_mut().unwrap().signature = "00".repeat(64);
        for (scope, proof) in [("foreign", foreign), ("forged", forged)] {
            store.append_event(
                &owner,
                &network,
                scope,
                EventBody::ConversationCreated {
                    title: scope.into(),
                },
                vec![],
                1,
            )?;
            let envelope = store.append_event(
                &owner,
                &network,
                scope,
                EventBody::BotCreated {
                    bot_id: scope.into(),
                    name: "Invalid".into(),
                    instructions: "".into(),
                    project: None,
                    membership_role_proof: proof,
                },
                vec![],
                2,
            )?;
            let status: String = store.connection.query_row(
                "SELECT status FROM events WHERE network_id=? AND hash=?",
                params![network, envelope.hash],
                |row| row.get(0),
            )?;
            assert_eq!(status, "invalid");
        }
        assert!(store.bots(&network)?.is_empty());
        Ok(())
    }

    #[test]
    fn replica_cannot_create_a_bot_or_leave_a_partial_conversation() -> Result<()> {
        let replica = LocalIdentity::generate();
        let network = replica.fixture_network_id()?;
        let mut store = Store::open(":memory:")?;
        store.install_trust_bundle(
            &network,
            &replica
                .sign_network_fixture(1, vec![replica.certificate(vec![NodeRole::Replica])?])?,
        )?;
        assert!(
            store
                .create_bot(
                    &replica,
                    &network,
                    "history",
                    "bot",
                    "Assistant",
                    "",
                    None,
                    1
                )
                .is_err()
        );
        assert!(store.conversations(&network)?.is_empty());
        let proof = store.trust_bundle(&network)?.unwrap();
        assert!(
            validate_gateway_proof_from(&store.connection, &network, &replica.node_id()?, &proof)
                .is_err()
        );
        Ok(())
    }
}
