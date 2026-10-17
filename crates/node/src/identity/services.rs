use super::event_authorization::{current_event_authorization, verify_event_envelope};
use crate::{
    model::{AgentSession, EventBody, SignedEvent},
    store::{Store, load_events_from},
};
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use vibeface_secure::authorization::{
    AUTH_VERSION, Action, Actor, Audience, Grantee, Invocation, InvocationContext, Nonce,
    ObjectHash, PolicyDomain, PolicyStamp, RequestFacts, ResourceId, ResourceRef, ResourceScope,
    ServiceId, SessionRef, SpaceId,
};

fn session_key(session: &SessionRef) -> String {
    format!("agent-session:{}", session.session_id)
}

fn check_heads(connection: &Connection, heads: &[PolicyStamp]) -> Result<()> {
    for head in heads {
        let (kind, id) = match &head.domain {
            PolicyDomain::Account(id) => ("account", id.as_str()),
            PolicyDomain::Space(id) => ("space", id.as_str()),
        };
        let actual: Option<(u64, u64, String)> = connection.query_row(
            "SELECT authority_epoch,sequence,head_hash FROM current_policy_heads WHERE domain_kind=? AND domain_id=?",
            params![kind, id], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        ).optional()?;
        ensure!(
            actual == Some((head.authority_epoch, head.sequence, head.head.to_string())),
            "service authorization policy changed before commit"
        );
    }
    Ok(())
}

impl Store {
    fn saved_agent_session(
        &self,
        space: &str,
        session: &SessionRef,
    ) -> Result<Option<AgentSession>> {
        let encoded: Option<String> = self
            .connection
            .query_row(
                "SELECT value FROM local_values WHERE space_id=? AND key=?",
                params![space, session_key(session)],
                |row| row.get(0),
            )
            .optional()?;
        encoded
            .map(|value| Ok(serde_json::from_str(&value)?))
            .transpose()
    }

    fn prepare_agent_session(
        &self,
        space: &str,
        conversation: &str,
        service: &ServiceId,
        session: &SessionRef,
        trigger: &str,
        selected: Option<&ObjectHash>,
        now_ms: u64,
    ) -> Result<(AgentSession, Vec<PolicyStamp>)> {
        ensure!(
            session.session_id == trigger,
            "agent session does not identify its triggering event"
        );
        let space_id = SpaceId::parse(space)?;
        let encoded: String = self
            .connection
            .query_row(
                "SELECT envelope FROM events WHERE space_id=? AND hash=? AND status='ready'",
                params![space, trigger],
                |row| row.get(0),
            )
            .context("agent trigger is not a ready event")?;
        let envelope: SignedEvent = serde_json::from_str(&encoded)?;
        ensure!(
            envelope.hash == trigger,
            "agent trigger envelope differs from its stored key"
        );
        let event = verify_event_envelope(&self.policy, space, &envelope)?;
        ensure!(
            event.network_id == space && event.scope_id == conversation,
            "agent trigger belongs to another space or conversation"
        );
        ensure!(
            matches!(&event.body, EventBody::MessagePosted { role, text, .. } if role == "user" && !text.trim().is_empty()),
            "agent trigger is not an admitted user message"
        );
        let events = load_events_from(&self.connection, space)?;
        let parent =
            current_event_authorization(&self.policy, &self.connection, &event, &events, now_ms)?;
        ensure!(
            matches!(parent.actor(), Actor::Account(_))
                && parent.resource().resource == event.resource_id,
            "agent trigger has no matching account resource authorization"
        );
        let facts = RequestFacts {
            resource: ResourceRef {
                space: space_id.clone(),
                resource: event.resource_id.clone(),
            },
            action: Action::AgentStart,
            audience: parent.audience().clone(),
            operation_id: format!("agent:{trigger}"),
            body_hash: ObjectHash::body(&envelope)?,
            context: InvocationContext {
                conversation: Some(conversation.into()),
                sdk_session: Some(session.clone()),
                workspace_resource: Some(event.resource_id.clone()),
                ..Default::default()
            },
        };
        let document = self
            .policy
            .verified()
            .space(&space_id)
            .context("agent space policy is missing")?;
        for leaf in &document.grants {
            if selected.is_some_and(|selected| selected != leaf) {
                continue;
            }
            let Ok(proof) = self
                .policy
                .verified()
                .verify_service_delegation(&parent, service, leaf, &facts, now_ms)
            else {
                continue;
            };
            ensure!(
                session.target == *proof.device(),
                "agent SDK session targets another service device"
            );
            return Ok((
                AgentSession {
                    space_id,
                    resource_id: event.resource_id,
                    service: service.clone(),
                    device: proof.device().clone(),
                    conversation_id: conversation.into(),
                    sdk_session: session.clone(),
                    grant_leaf: proof.grant_leaf().clone(),
                    initiator: proof.initiator().clone(),
                    initiating_grant: proof.initiating_grant().cloned(),
                    parent_invocation: parent.invocation_hash().clone(),
                    expected_policies: proof.heads().to_vec(),
                    expires_at_ms: proof.expires_at_ms(),
                },
                parent.dependencies().to_vec(),
            ));
        }
        anyhow::bail!("no explicit service grant authorizes this admitted account message")
    }

    pub fn agent_grant(
        &mut self,
        space: &str,
        conversation: &str,
        service: &ServiceId,
        session: &SessionRef,
        trigger: &str,
        now_ms: u64,
    ) -> Result<AgentSession> {
        if let Some(saved) = self.saved_agent_session(space, session)? {
            ensure!(
                saved.space_id.as_str() == space
                    && saved.conversation_id == conversation
                    && saved.service == *service
                    && saved.sdk_session == *session
                    && session.session_id == trigger,
                "agent session descriptor was reused for another request"
            );
            return self.refresh_agent_session(&saved, now_ms);
        }
        let (descriptor, parent_heads) = self.prepare_agent_session(
            space,
            conversation,
            service,
            session,
            trigger,
            None,
            now_ms,
        )?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        check_heads(&tx, &descriptor.expected_policies)?;
        check_heads(&tx, &parent_heads)?;
        tx.execute(
            "INSERT INTO local_values(space_id,key,value) VALUES(?,?,?)",
            params![
                space,
                session_key(session),
                serde_json::to_string(&descriptor)?
            ],
        )?;
        tx.commit()?;
        Ok(descriptor)
    }

    pub fn refresh_agent_session(
        &mut self,
        descriptor: &AgentSession,
        now_ms: u64,
    ) -> Result<AgentSession> {
        let saved = self
            .saved_agent_session(descriptor.space_id.as_str(), &descriptor.sdk_session)?
            .context("agent session was not persisted by verified admission")?;
        ensure!(
            serde_json::to_value(&saved)? == serde_json::to_value(descriptor)?,
            "agent session differs from its persisted descriptor"
        );
        ensure!(now_ms < saved.expires_at_ms, "agent session expired");
        let (mut current, parent_heads) = self.prepare_agent_session(
            saved.space_id.as_str(),
            &saved.conversation_id,
            &saved.service,
            &saved.sdk_session,
            &saved.sdk_session.session_id,
            Some(&saved.grant_leaf),
            now_ms,
        )?;
        current.expires_at_ms = current.expires_at_ms.min(saved.expires_at_ms);
        let mut expected = saved.clone();
        expected.expected_policies = current.expected_policies.clone();
        expected.expires_at_ms = current.expires_at_ms;
        ensure!(
            serde_json::to_value(&expected)? == serde_json::to_value(&current)?,
            "agent session no longer has its admitted resource, parent, or delegation"
        );
        let previous = serde_json::to_string(&saved)?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        check_heads(&tx, &current.expected_policies)?;
        check_heads(&tx, &parent_heads)?;
        ensure!(
            tx.execute(
                "UPDATE local_values SET value=? WHERE space_id=? AND key=? AND value=?",
                params![
                    serde_json::to_string(&current)?,
                    current.space_id.as_str(),
                    session_key(&current.sdk_session),
                    previous
                ]
            )? == 1,
            "agent session changed before refresh"
        );
        tx.commit()?;
        Ok(current)
    }

    pub fn service_grant(
        &self,
        body: &Value,
        audience: &Audience,
        service: &ServiceId,
        now_ms: u64,
    ) -> Result<Invocation> {
        ensure!(
            matches!(
                body["method"].as_str(),
                Some("replica_inventory" | "replica_event" | "replica_blob")
            ),
            "service replication requires an explicit replica request"
        );
        let facts = self.request_facts(body, audience)?;
        ensure!(
            facts.action == Action::ReplicaRead,
            "service replication action mismatch"
        );
        let document = self
            .policy
            .verified()
            .space(&facts.resource.space)
            .context("replica space policy is missing")?;
        for leaf in &document.grants {
            let Ok(proof) = self
                .policy
                .verified()
                .verify_service_grant(service, leaf, &facts, now_ms)
            else {
                continue;
            };
            let expires_at_ms = now_ms
                .checked_add(vibeface_secure::authorization::INVOCATION_MAX_AGE_MS)
                .context("service invocation expiry overflow")?
                .min(proof.expires_at_ms());
            ensure!(now_ms < expires_at_ms, "service grant expired");
            return Ok(Invocation {
                version: AUTH_VERSION,
                actor: Actor::Service(service.clone()),
                device: proof.device().clone(),
                initiator: proof.initiator().clone(),
                initiating_grant: proof.initiating_grant().cloned(),
                resource: facts.resource,
                action: facts.action,
                audience: facts.audience,
                presenter: proof.device().clone(),
                operation_id: facts.operation_id,
                body_hash: facts.body_hash,
                grant_leaf: proof.grant_leaf().clone(),
                expected_policies: proof.heads().to_vec(),
                context: facts.context,
                nonce: Nonce::random()?,
                not_before_ms: now_ms,
                expires_at_ms,
            });
        }
        anyhow::bail!(
            "service has no explicit current replica grant for this resource and audience"
        )
    }

    pub fn replication_targets(
        &self,
        space: &str,
        service: &ServiceId,
        now_ms: u64,
    ) -> Result<Vec<(ResourceId, Audience, String)>> {
        let space = SpaceId::parse(space)?;
        let document = self
            .policy
            .verified()
            .space(&space)
            .context("replica space policy is missing")?;
        let mut targets = BTreeMap::new();
        for hash in &document.grants {
            let Some(grant) = self.policy.verified().grant(hash) else {
                continue;
            };
            if grant.payload.grantee != Grantee::Actor(Actor::Service(service.clone()))
                || !grant.payload.actions.contains(&Action::ReplicaRead)
            {
                continue;
            }
            let resources = match &grant.payload.scope {
                ResourceScope::Exact(resource) => vec![resource.resource.clone()],
                ResourceScope::WholeSpace(_) => {
                    let mut query = self.connection.prepare("SELECT resource_id FROM resource_records WHERE space_id=? AND host_service_id=? ORDER BY resource_id")?;
                    query
                        .query_map(
                            params![space.as_str(), grant.payload.audience.service.as_str()],
                            |row| row.get::<_, String>(0),
                        )?
                        .map(|row| Ok(ResourceId::parse(&row?)?))
                        .collect::<Result<Vec<_>>>()?
                }
            };
            for id in resources {
                let Some(record) = self.policy.verified().resource(&space, &id) else {
                    continue;
                };
                let resource = ResourceRef {
                    space: space.clone(),
                    resource: record.payload.resource.clone(),
                };
                let covered = match &grant.payload.scope {
                    ResourceScope::WholeSpace(id) => id == &resource.space,
                    ResourceScope::Exact(exact) => exact == &resource,
                };
                if !covered {
                    continue;
                }
                let facts = RequestFacts {
                    resource: resource.clone(),
                    action: Action::ReplicaRead,
                    audience: grant.payload.audience.clone(),
                    operation_id: "replica-target".into(),
                    body_hash: ObjectHash::body(&json!({"replica_target":resource}))?,
                    context: InvocationContext {
                        workspace_resource: Some(resource.resource.clone()),
                        ..Default::default()
                    },
                };
                if self
                    .policy
                    .verified()
                    .verify_service_grant(service, hash, &facts, now_ms)
                    .is_err()
                {
                    continue;
                }
                let device = self.policy.verified().verify_service_device(
                    &space,
                    &grant.payload.audience.device,
                    now_ms,
                )?;
                let node_id = crate::crypto::fingerprint(device.payload.public_key.as_str())?;
                targets.insert(
                    (
                        resource.resource.to_string(),
                        grant.payload.audience.service.to_string(),
                    ),
                    (resource.resource, grant.payload.audience.clone(), node_id),
                );
            }
        }
        Ok(targets.into_values().collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        crypto::LocalIdentity,
        identity::{admission::tests::Fixture, journal::PolicyState, policy::FreshnessProof},
        model::EventBody,
    };
    use vibeface_secure::{
        Identity,
        authorization::{
            CheckStage, Grant, GrantConstraints, JournalEntry, PublicKey, Signed, TransportEvidence,
        },
    };

    const NOW: u64 = 1_000;

    struct AgentFixture {
        store: Store,
        node: LocalIdentity,
        authority: Identity,
        user: Identity,
        space: String,
        audience: Audience,
        parent_grant: ObjectHash,
        service_grant: ObjectHash,
        trigger: String,
    }

    impl AgentFixture {
        fn new() -> Result<Self> {
            let fixture = Fixture::new()?;
            let freshness = fixture.proof()?;
            let audience = fixture.auth.audience().clone();
            let resource = fixture.auth.resource().clone();
            let actor = fixture.auth.actor().clone();
            let mut store = Store {
                connection: fixture.connection,
                policy: fixture.policy,
            };
            store.append_authorized_event(
                &fixture.node,
                &fixture.auth,
                &freshness,
                &fixture.transport,
                &fixture.body,
                "chat",
                EventBody::ConversationCreated {
                    title: "test".into(),
                },
                vec![],
                NOW as i64,
            )?;
            let root =
                Identity::from_secret_hex(&hex::encode([1; 32])).map_err(anyhow::Error::msg)?;
            let user =
                Identity::from_secret_hex(&hex::encode([4; 32])).map_err(anyhow::Error::msg)?;
            let parent = Signed::sign(
                Grant {
                    version: AUTH_VERSION,
                    owner_space: resource.space.clone(),
                    scope: ResourceScope::Exact(resource.clone()),
                    grantee: Grantee::Actor(actor.clone()),
                    actions: vec![
                        Action::ConversationRead,
                        Action::MessageAppend,
                        Action::ReplicaRead,
                        Action::AgentStart,
                    ],
                    audience: audience.clone(),
                    constraints: GrantConstraints::default(),
                    parent: None,
                    remaining_delegations: 1,
                    issued_under: store
                        .authorization_state()
                        .head(&PolicyDomain::Space(resource.space.clone()))
                        .unwrap()
                        .clone(),
                    not_before_ms: 0,
                    expires_at_ms: 1_000_000,
                    nonce: Nonce::random()?,
                },
                &root,
            )?;
            let parent_grant = parent.hash()?;
            let mut document = store
                .authorization_state()
                .space(&resource.space)
                .unwrap()
                .clone();
            document.previous = store
                .authorization_state()
                .head(&PolicyDomain::Space(resource.space.clone()))
                .cloned();
            document.sequence += 1;
            document.grants.push(parent_grant.clone());
            document.grants.sort();
            store.install_policy_journal(
                &[
                    JournalEntry::Grant(parent),
                    JournalEntry::Space(Signed::sign(document, &root)?),
                ],
                NOW,
            )?;
            let mut result = Self {
                store,
                node: fixture.node,
                authority: fixture.authority,
                user,
                space: resource.space.to_string(),
                audience,
                parent_grant: parent_grant.clone(),
                service_grant: parent_grant,
                trigger: String::new(),
            };
            result.service_grant = result.add_service_grant(vec![Action::AgentStart])?;
            let body = json!({"method":"message","space_id":result.space,"resource_id":resource.resource,"conversation_id":"chat",
                "message_id":"message_1","text":"please work on this","scheduler_node":result.node.node_id()?,"authorization_id":"message_1"});
            let auth = result.authorize(&body, &actor)?;
            let freshness = result.freshness(&auth)?;
            let event = result.store.append_authorized_event(
                &result.node,
                &auth,
                &freshness,
                &TransportEvidence::DirectNoise {
                    device: auth.device().clone(),
                },
                &body,
                "chat",
                EventBody::MessagePosted {
                    message_id: "message_1".into(),
                    role: "user".into(),
                    text: "please work on this".into(),
                    scheduler_node: Some(result.node.node_id()?),
                },
                vec![],
                NOW as i64,
            )?;
            result.trigger = event.hash;
            Ok(result)
        }

        fn add_service_grant(&mut self, actions: Vec<Action>) -> Result<ObjectHash> {
            let space = SpaceId::parse(&self.space)?;
            let mut grant = self
                .store
                .authorization_state()
                .grant(&self.parent_grant)
                .unwrap()
                .payload
                .clone();
            grant.grantee = Grantee::Actor(Actor::Service(self.audience.service.clone()));
            grant.parent = Some(self.parent_grant.clone());
            grant.remaining_delegations = 0;
            grant.actions = actions;
            grant.issued_under = self
                .store
                .authorization_state()
                .head(&PolicyDomain::Space(space.clone()))
                .unwrap()
                .clone();
            grant.nonce = Nonce::random()?;
            let signed = Signed::sign(grant, &self.user)?;
            let hash = signed.hash()?;
            let mut document = self
                .store
                .authorization_state()
                .space(&space)
                .unwrap()
                .clone();
            document.previous = self
                .store
                .authorization_state()
                .head(&PolicyDomain::Space(space))
                .cloned();
            document.sequence += 1;
            document.grants.push(hash.clone());
            document.grants.sort();
            let root =
                Identity::from_secret_hex(&hex::encode([1; 32])).map_err(anyhow::Error::msg)?;
            self.store.install_policy_journal(
                &[
                    JournalEntry::Grant(signed),
                    JournalEntry::Space(Signed::sign(document, &root)?),
                ],
                NOW,
            )?;
            Ok(hash)
        }

        fn authorize(
            &self,
            body: &Value,
            actor: &Actor,
        ) -> Result<vibeface_secure::authorization::VerifiedAuthorization> {
            let facts = self.store.request_facts(body, &self.audience)?;
            let device =
                vibeface_secure::authorization::DeviceId::from_key(&PublicKey::of(&self.user));
            let pins = self.store.trusted_policy_pins();
            let heads = pins
                .iter()
                .filter_map(|pin| self.store.authorization_state().head(&pin.domain).cloned())
                .collect();
            let invocation = Signed::sign(
                Invocation {
                    version: AUTH_VERSION,
                    actor: actor.clone(),
                    device: device.clone(),
                    initiator: actor.clone(),
                    initiating_grant: None,
                    resource: facts.resource,
                    action: facts.action,
                    audience: facts.audience,
                    presenter: device.clone(),
                    operation_id: facts.operation_id,
                    body_hash: facts.body_hash,
                    grant_leaf: self.parent_grant.clone(),
                    expected_policies: heads,
                    context: facts.context,
                    nonce: Nonce::random()?,
                    not_before_ms: NOW,
                    expires_at_ms: NOW + 10_000,
                },
                &self.user,
            )?;
            self.store.authorize_request(
                &invocation,
                &self.store.request_facts(body, &self.audience)?,
                &TransportEvidence::DirectNoise { device },
                NOW,
            )
        }

        fn freshness(
            &mut self,
            auth: &vibeface_secure::authorization::VerifiedAuthorization,
        ) -> Result<FreshnessProof> {
            let challenge = self.store.issue_freshness(auth, CheckStage::Admit, NOW)?;
            let receipts = self.store.sign_current_receipts(
                &challenge,
                &LocalIdentity {
                    secret_key_hex: self.authority.secret_hex(),
                },
                NOW,
            )?;
            Ok(FreshnessProof {
                challenge,
                receipts,
            })
        }

        fn session(&self) -> SessionRef {
            SessionRef {
                target: self.audience.device.clone(),
                session_id: self.trigger.clone(),
            }
        }
    }

    #[test]
    fn agent_descriptor_persists_revalidates_and_rejects_context_tampering() -> Result<()> {
        let mut fixture = AgentFixture::new()?;
        let descriptor = fixture.store.agent_grant(
            &fixture.space,
            "chat",
            &fixture.audience.service,
            &fixture.session(),
            &fixture.trigger,
            NOW,
        )?;
        assert_eq!(descriptor.grant_leaf, fixture.service_grant);
        let ResourceScope::Exact(resource) = &fixture
            .store
            .authorization_state()
            .grant(&fixture.service_grant)
            .unwrap()
            .payload
            .scope
        else {
            panic!("fixture needs an exact resource grant")
        };
        assert_eq!(descriptor.resource_id, resource.resource);
        let artifact = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../.artifacts/services-restart")
            .join(uuid::Uuid::new_v4().to_string());
        std::fs::create_dir_all(&artifact)?;
        let database = artifact.join("node.sqlite3");
        fixture.store.connection.execute(
            "VACUUM INTO ?",
            [database
                .to_str()
                .context("test database path is not UTF-8")?],
        )?;
        let reopened = rusqlite::Connection::open(&database)?;
        reopened.pragma_update(None, "foreign_keys", true)?;
        let policy = PolicyState::load(&reopened)?;
        fixture.store = Store {
            connection: reopened,
            policy,
        };
        let refreshed = fixture
            .store
            .refresh_agent_session(&descriptor, NOW + 100_000)?;
        assert_eq!(refreshed.parent_invocation, descriptor.parent_invocation);
        let mut changed = refreshed.clone();
        changed.conversation_id = "other-chat".into();
        assert!(
            fixture
                .store
                .refresh_agent_session(&changed, NOW + 100_000)
                .is_err()
        );
        changed = refreshed.clone();
        changed.parent_invocation = ObjectHash::body(&"another-parent")?;
        fixture.store.local_set(
            &fixture.space,
            &session_key(&changed.sdk_session),
            &serde_json::to_value(&changed)?,
        )?;
        assert!(
            fixture
                .store
                .refresh_agent_session(&changed, NOW + 100_000)
                .is_err()
        );
        drop(fixture);
        std::fs::remove_dir_all(artifact)?;
        Ok(())
    }

    #[test]
    fn revoked_service_session_and_changed_policy_cannot_be_persisted_or_refreshed() -> Result<()> {
        let mut fixture = AgentFixture::new()?;
        let descriptor = fixture.store.agent_grant(
            &fixture.space,
            "chat",
            &fixture.audience.service,
            &fixture.session(),
            &fixture.trigger,
            NOW,
        )?;
        let space = SpaceId::parse(&fixture.space)?;
        let mut document = fixture
            .store
            .authorization_state()
            .space(&space)
            .unwrap()
            .clone();
        document.previous = fixture
            .store
            .authorization_state()
            .head(&PolicyDomain::Space(space))
            .cloned();
        document.sequence += 1;
        document
            .grants
            .retain(|hash| hash != &fixture.service_grant);
        let root = Identity::from_secret_hex(&hex::encode([1; 32])).map_err(anyhow::Error::msg)?;
        fixture
            .store
            .install_policy_journal(&[JournalEntry::Space(Signed::sign(document, &root)?)], NOW)?;
        assert!(
            fixture
                .store
                .refresh_agent_session(&descriptor, NOW)
                .is_err()
        );

        let mut raced = AgentFixture::new()?;
        raced
            .store
            .connection
            .execute("UPDATE current_policy_heads SET sequence=sequence+1", [])?;
        assert!(
            raced
                .store
                .agent_grant(
                    &raced.space,
                    "chat",
                    &raced.audience.service,
                    &raced.session(),
                    &raced.trigger,
                    NOW
                )
                .is_err()
        );
        assert!(
            raced
                .store
                .local_value(&raced.space, &session_key(&raced.session()))?
                .is_none()
        );
        Ok(())
    }

    #[test]
    fn service_replication_requires_a_real_replica_child_and_builds_a_verifiable_invocation()
    -> Result<()> {
        let mut fixture = AgentFixture::new()?;
        assert!(
            fixture
                .store
                .replication_targets(&fixture.space, &fixture.audience.service, NOW)?
                .is_empty()
        );
        let replica = fixture.add_service_grant(vec![Action::ReplicaRead])?;
        let targets =
            fixture
                .store
                .replication_targets(&fixture.space, &fixture.audience.service, NOW)?;
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].2, fixture.node.node_id()?);
        let body = json!({"method":"replica_inventory","space_id":fixture.space,"resource_id":targets[0].0,"authorization_id":"replica_1","limit":32});
        let invocation = fixture.store.service_grant(
            &body,
            &fixture.audience,
            &fixture.audience.service,
            NOW,
        )?;
        assert_eq!(invocation.grant_leaf, replica);
        let signer =
            Identity::from_secret_hex(&fixture.node.secret_key_hex).map_err(anyhow::Error::msg)?;
        let signed = Signed::sign(invocation, &signer)?;
        let facts = fixture.store.request_facts(&body, &fixture.audience)?;
        let verified = fixture.store.authorize_request(
            &signed,
            &facts,
            &TransportEvidence::VerifiedPeer {
                origin: fixture.audience.device.clone(),
            },
            NOW,
        )?;
        assert_eq!(
            verified.actor(),
            &Actor::Service(fixture.audience.service.clone())
        );
        assert_eq!(verified.action(), Action::ReplicaRead);
        Ok(())
    }
}
