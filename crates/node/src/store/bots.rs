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
    ensure!(
        instructions.len() <= 8192,
        "Bot instructions must be at most 8192 bytes"
    );
    ensure!(
        serde_json::to_vec(&(name, instructions, project))?.len() <= 12000,
        "Bot definition is too large for the assistant context"
    );
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

fn bots_from(
    connection: &Connection,
    policy: &crate::identity::journal::PolicyState,
    network: &str,
) -> Result<Vec<Bot>> {
    let events = load_events_from(connection, policy, network)?;
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
        ensure!(
            bot_admission(&stored.event).is_ok(),
            "Bot has no Account admission"
        );
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
    _connection: &Connection,
    _network: &str,
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
        if dependencies(&stored.event).any(|dependency| invalid.contains(dependency)) {
            invalid.insert(hash.clone());
            continue;
        }
        let Some(bot) = project_bot(hash, &stored.event) else {
            continue;
        };
        let dependencies = dependencies(&stored.event).cloned().collect::<Vec<_>>();
        let ancestors = closure_hashes(events, &dependencies);
        let conversation_exists = ancestors.iter().any(|ancestor| {
            events.get(ancestor).is_some_and(|parent| {
                parent.status == "ready"
                    && parent.event.scope_id == bot.conversation_id
                    && parent.event.resource_id == stored.event.resource_id
                    && matches!(parent.event.body, EventBody::ConversationCreated { .. })
                    && bot_admission(&stored.event).ok().is_some_and(|admission| {
                        matches!(&parent.event.cause, EventCause::Invocation { admission: original }
                            if original == admission)
                    })
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
            || bot_admission(&stored.event).is_err()
        {
            invalid.insert(hash.clone());
            continue;
        }
        identifiers
            .entry(bot.bot_id.clone())
            .or_default()
            .push(hash.clone());
        scopes
            .entry(bot.conversation_id.clone())
            .or_default()
            .push(hash.clone());
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

pub(crate) fn bot_definition_from_body(body: &Value) -> Result<(String, EventBody)> {
    ensure!(
        body["method"] == "create_bot",
        "Bot creation request required"
    );
    let text = |key: &str| -> Result<&str> {
        body[key]
            .as_str()
            .with_context(|| format!("{key} is required"))
    };
    let conversation = text("conversation_id")?;
    let bot = text("bot_id")?;
    let name = text("name")?;
    let instructions = body
        .get("instructions")
        .map(|value| value.as_str().context("instructions must be a string"))
        .transpose()?
        .unwrap_or("");
    let project: Option<BotProject> = body
        .get("project")
        .filter(|v| !v.is_null())
        .map(|value| serde_json::from_value(value.clone()))
        .transpose()?;
    validate_definition(bot, conversation, name, instructions, project.as_ref())?;
    Ok((
        conversation.into(),
        EventBody::BotCreated {
            bot_id: bot.into(),
            name: name.into(),
            instructions: instructions.into(),
            project,
        },
    ))
}

fn bot_admission(event: &Event) -> Result<&AdmissionProof> {
    let EventCause::Invocation { admission } = &event.cause else {
        anyhow::bail!("Bot must be created by an Account invocation")
    };
    ensure!(
        matches!(
            admission.invocation.payload.actor,
            vibeface_secure::authorization::Actor::Account(_)
        ),
        "only an Account creates Bots"
    );
    let (scope, expected) = bot_definition_from_body(&admission.source_body)?;
    ensure!(
        scope == event.scope_id && expected == event.body,
        "Bot definition differs from its Account request"
    );
    Ok(admission)
}

impl Store {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn create_bot(
        &mut self,
        identity: &LocalIdentity,
        auth: &vibeface_secure::authorization::VerifiedAuthorization,
        freshness: &crate::identity::policy::FreshnessProof,
        transport: &crate::identity::policy::RequestTransport,
        source_body: &Value,
        at: i64,
    ) -> Result<Bot> {
        ensure!(
            matches!(
                auth.actor(),
                vibeface_secure::authorization::Actor::Account(_)
            ),
            "only an Account creates Bots"
        );
        let (conversation, body) = bot_definition_from_body(source_body)?;
        let EventBody::BotCreated {
            bot_id,
            name,
            instructions,
            project,
        } = &body
        else {
            unreachable!()
        };
        let network = auth.resource().space.as_str();
        let now = u64::try_from(at).context("event creation time is negative")?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let bots = bots_from(&tx, &self.policy, network)?;
        if let Some(existing) = bots.iter().find(|bot| bot.bot_id == *bot_id) {
            let events = load_events_from(&tx, &self.policy, network)?;
            let original = events
                .get(&existing.event_hash)
                .context("Bot definition is missing")?;
            ensure!(
                original.event.resource_id == auth.resource().resource,
                "Bot belongs to another resource"
            );
            ensure!(
                existing.conversation_id == conversation
                    && existing.name == *name
                    && existing.instructions == *instructions
                    && existing.project == *project,
                "Bot ID already exists with another definition"
            );
            ensure!(
                !bots
                    .iter()
                    .any(|bot| bot.conversation_id == conversation && bot.bot_id != *bot_id),
                "conversation already belongs to another Bot"
            );
            let admission = bot_admission(&original.event)?;
            if admission.invocation == *auth.signed_invocation() {
                ensure!(
                    admission.source_body == *source_body
                        && admission.transport == transport.stored(),
                    "Bot retry changed its original authorization"
                );
                crate::identity::admission::validate_persisted_admission(&tx, admission)?;
                return Ok(existing.clone());
            }
        }
        let admission = crate::identity::admission::consume_request_authorization_in(
            &tx,
            &self.policy,
            auth,
            freshness,
            transport,
            source_body,
            None,
            now,
        )?;
        if let Some(existing) = bots.iter().find(|bot| bot.bot_id == *bot_id) {
            return Ok(existing.clone());
        }
        ensure!(
            !bots.iter().any(|bot| bot.conversation_id == conversation),
            "conversation already belongs to another Bot"
        );
        let events = load_events_from(&tx, &self.policy, network)?;
        ensure!(
            heads_from_events(&events, &conversation).is_empty(),
            "a new Bot requires a new conversation scope"
        );
        let signed = crate::identity::event_authorization::append_admitted_events_in(
            &tx,
            &self.policy,
            identity,
            &admission,
            vec![
                (
                    conversation.clone(),
                    EventBody::ConversationCreated {
                        title: name.clone(),
                    },
                    vec![],
                ),
                (conversation, body, vec![]),
            ],
            now,
        )?;
        let bot = bots_from(&tx, &self.policy, network)?
            .into_iter()
            .find(|bot| {
                signed
                    .last()
                    .is_some_and(|event| bot.event_hash == event.hash)
            })
            .context("created Bot could not be projected")?;
        let checkpoint = crate::identity::admission::prepare_local_admission_checkpoint_in(
            &tx,
            &self.policy,
            &admission,
        )?;
        tx.commit()?;
        self.policy.local_admissions.insert(checkpoint);
        Ok(bot)
    }

    pub fn bots(&self, network: &str) -> Result<Vec<Bot>> {
        bots_from(&self.connection, &self.policy, network)
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
    use crate::identity::test_support::{TestNetwork, now};
    use serde_json::json;
    use vibeface_secure::authorization::CheckStage;

    fn create(network: &TestNetwork, store: &mut Store, body: &Value) -> Result<Bot> {
        let (auth, transport) = network.authorization(store, body, now())?;
        let proof = network.freshness(store, &auth, CheckStage::Admit, now())?;
        store.create_bot(
            &network.node,
            &auth,
            &proof,
            &crate::identity::policy::RequestTransport::Existing(transport),
            body,
            now() as i64,
        )
    }

    fn body(network: &TestNetwork, scope: &str, bot: &str) -> Value {
        let mut body = network.body("create_bot", scope);
        body["bot_id"] = json!(bot);
        body["name"] = json!("Assistant");
        body["instructions"] = json!("Review changes");
        body
    }

    #[test]
    fn retries_preserve_the_complete_definition_and_scope_isolation() -> Result<()> {
        let network = TestNetwork::new()?;
        let mut store = network.memory()?;
        let original = body(&network, "bot-history", "bot");
        let bot = create(&network, &mut store, &original)?;
        assert_eq!(create(&network, &mut store, &original)?, bot);
        let mut retry = original.clone();
        retry["authorization_id"] = json!("same-definition");
        assert_eq!(create(&network, &mut store, &retry)?, bot);
        let mut changed = original.clone();
        changed["authorization_id"] = json!("different-definition");
        changed["instructions"] = json!("Run everything");
        assert!(create(&network, &mut store, &changed).is_err());
        assert!(
            create(
                &network,
                &mut store,
                &body(&network, "other-history", "bot")
            )
            .is_err()
        );
        assert!(
            create(
                &network,
                &mut store,
                &body(&network, "bot-history", "other-bot")
            )
            .is_err()
        );
        assert_eq!(store.bots(network.space.as_str())?, vec![bot]);
        let count: u64 = store.connection.query_row(
            "SELECT count(*) FROM events WHERE space_id=?",
            [network.space.as_str()],
            |row| row.get(0),
        )?;
        assert_eq!(count, 2);
        Ok(())
    }

    #[test]
    fn bot_creation_rejects_an_existing_conversation_scope() -> Result<()> {
        let network = TestNetwork::new()?;
        let mut store = network.memory()?;
        network.conversation(&mut store, "existing-history")?;
        let original = network.message(&mut store, "existing-history", "Keep this discussion")?;
        assert!(
            create(
                &network,
                &mut store,
                &body(&network, "existing-history", "bot")
            )
            .is_err()
        );
        assert!(store.bots(network.space.as_str())?.is_empty());
        assert_eq!(
            store.messages(network.space.as_str(), "existing-history")?[0].event_hash,
            original.hash
        );
        Ok(())
    }

    #[test]
    fn malformed_bot_definition_leaves_no_partial_conversation() -> Result<()> {
        let network = TestNetwork::new()?;
        let mut store = network.memory()?;
        let mut request = body(&network, "history", "bot");
        request["name"] = json!(" ");
        assert!(create(&network, &mut store, &request).is_err());
        assert!(store.conversations(network.space.as_str())?.is_empty());
        request["name"] = json!("Assistant");
        request["project"] =
            json!({"target_node":network.node.node_id()?, "cwd":"/tmp/../private"});
        assert!(create(&network, &mut store, &request).is_err());
        assert!(store.conversations(network.space.as_str())?.is_empty());
        Ok(())
    }
}
