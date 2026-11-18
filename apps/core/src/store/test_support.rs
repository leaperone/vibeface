use super::bot_settings::ChangeRequest;
use super::*;
use crate::providers::Provider;
use crate::scheduling::{JobState, SchedulerJob, Shown, TriggerKind, message_trigger};
use crate::work::{Signal, WorkSpec};
use vibeface_protocol::history::HISTORY_RESPONSE_BYTES;

impl Store {
    /// Feed an earlier-ranked answer through the message projector, as on sync.
    pub(crate) fn project_earlier_answer_fixture(
        &self,
        network: &str,
        scope: &str,
        question: &str,
        replacement: &str,
    ) -> Result<()> {
        let message = self
            .message(network, scope, &questions::answer_id(question))?
            .context("prior answer")?;
        let mut event = log::event_from(&self.connection, network, &message.event_hash)?
            .context("prior answer event")?;
        let EventBody::MessagePosted { text, .. } = &mut event.body else {
            bail!("expected answer")
        };
        *text = replacement.to_owned();
        let writer = LocalIdentity::generate();
        event.author = writer.device_id()?;
        event.seq = 1;
        event.prev = None;
        let hash = writer.sign_event(event.clone())?.hash;
        reduce::apply(
            &self.connection,
            network,
            &hash,
            &event,
            event.created_at_ms,
            &rank(1, event.created_at_ms, &hash),
            false,
        )
    }

    pub(crate) fn post_input_fixture(
        &mut self,
        identity: &LocalIdentity,
        network: &str,
        body: EventBody,
        at: i64,
    ) -> Result<(Message, crate::scheduling::AdmissionReceipt)> {
        let EventBody::InputPosted {
            message_id,
            target,
            intent,
            sequence,
            expected_revision,
            text,
            attachments,
            report_results,
            attestation: Some(attestation),
        } = body
        else {
            bail!("fixture requires a signed input");
        };
        let input = crate::workspace::InputRequest {
            message_id,
            route: crate::workspace::InputRoute {
                target,
                intent,
                sequence,
                expected_revision,
            },
            text,
            attachments,
            report_results: report_results
                .context("signed input fixture requires report_results")?,
        };
        let stored = self.post_workspace_input(identity, network, input, attestation, at)?;
        let receipt =
            self.admit_workspace_input(network, &identity.device_id()?, &stored.input.message_id)?;
        let message = self
            .message(network, &stored.conversation_id, &stored.input.message_id)?
            .context("input projection")?;
        Ok((message, receipt))
    }

    pub(crate) fn accept_work_fixture(
        &mut self,
        identity: &LocalIdentity,
        network: &str,
        spec: WorkSpec,
        at: i64,
    ) -> Result<(crate::work::WorkRecord, bool)> {
        let WorkSpec {
            work_id,
            conversation,
            trigger,
            target,
            dispatcher,
            call_ref,
            kind,
            model,
            resolved,
            cwd,
            title,
            prompt,
            continues,
            parent,
            thread,
        } = spec;
        let kind = match kind {
            WorkKind::Agent(provider) => crate::work::RequestedWorkKind::Agent(Some(provider)),
            WorkKind::Task => crate::work::RequestedWorkKind::Task,
        };
        let request = crate::work::WorkRequest {
            work_id,
            conversation,
            trigger,
            target,
            dispatcher,
            call_ref,
            kind,
            model,
            cwd,
            title,
            prompt,
            continues,
            parent,
            thread,
        };
        let admitted = self.accept_work(identity, network, request, resolved, at)?;
        Ok((admitted.record, admitted.created))
    }

    pub(crate) fn claim_next_fixture(
        &mut self,
        identity: &LocalIdentity,
        network: &str,
        conversation: &str,
        boot: &str,
        fingerprint: &str,
    ) -> Result<Option<crate::scheduling::ClaimedJob>> {
        self.recover_scheduler_jobs(identity, network, boot)?;
        let mut queued = self.scheduler_jobs(conversation)?;
        queued.retain(|job| matches!(job.state, JobState::Queued));
        queued.sort_by(|left, right| left.priority().cmp(&right.priority()));
        for job in queued {
            let resolved = if job.kind == TriggerKind::AgentTask {
                self.works(network, conversation)?
                    .into_iter()
                    .find(|work| work.work_id == job.reply_to)
                    .context("accepted task")?
                    .resolved
            } else {
                ResolvedModel {
                    provider: Provider::Codex,
                    model: None,
                }
            };
            if let Some(mut claimed) = self.claim_next_in_session(
                identity,
                network,
                &job.session_key(),
                boot,
                &crate::scheduling::SessionConfig {
                    stable_fingerprint: fingerprint.into(),
                    runtime_fingerprint: fingerprint.into(),
                    provider: resolved.provider,
                },
                &crate::config::SchedulingLimits::default(),
            )? {
                let attempt = claimed
                    .job
                    .state
                    .attempt()
                    .context("claimed attempt")?
                    .id
                    .clone();
                let runtime = crate::scheduling::TurnRuntime {
                    provider: resolved.provider,
                    model: resolved.model,
                    reasoning_effort: None,
                    generation: "fixture-generation".into(),
                    session_generation: claimed.session_generation,
                    delivery_id: attempt.clone(),
                    stage: vibeface_bridge::failure::ProviderStage::Validate,
                    execution: crate::scheduling::TurnExecution::NotStarted,
                    attempted_at_ms: 0,
                    switched_from: None,
                    switch_reason: None,
                };
                if claimed.job.runtime.as_ref().is_none_or(|saved| {
                    saved.execution == crate::scheduling::TurnExecution::NotStarted
                }) {
                    claimed.job = self.record_turn_runtime(
                        conversation,
                        &claimed.job.trigger,
                        &attempt,
                        &runtime,
                    )?;
                }
                return Ok(Some(claimed));
            }
        }
        Ok(None)
    }
}

pub(super) struct Net {
    pub(super) network: String,
    pub(super) a: LocalIdentity,
    pub(super) b: LocalIdentity,
    pub(super) state: NetworkState,
}

pub(super) fn net() -> Result<Net> {
    let a = LocalIdentity::generate();
    let b = LocalIdentity::generate();
    let state = a.sign_network_fixture(1, vec![a.device_id()?, b.device_id()?])?;
    Ok(Net {
        network: a.fixture_network_id()?,
        a,
        b,
        state,
    })
}

pub(super) fn store(net: &Net) -> Result<Store> {
    let mut store = Store::open(":memory:")?;
    store.install_trust_bundle(&net.network, &net.state)?;
    Ok(store)
}

pub(super) fn work_spec(
    local: &DeviceId,
    trigger: &str,
    call_ref: &str,
    provider: Provider,
) -> WorkSpec {
    WorkSpec {
        work_id: WorkSpec::id("c", trigger, call_ref),
        conversation: "c".into(),
        trigger: trigger.into(),
        target: local.clone(),
        dispatcher: local.clone(),
        call_ref: call_ref.into(),
        kind: WorkKind::Agent(provider),
        model: None,
        resolved: ResolvedModel {
            provider,
            model: None,
        },
        cwd: "/tmp".into(),
        title: "t".into(),
        prompt: "p".into(),
        continues: None,
        parent: None,
        thread: None,
    }
}

pub(super) fn start_work(
    store: &mut Store,
    net: &Net,
    author: &LocalIdentity,
    work_id: &str,
    thread: (&str, i64),
    turn: (&str, i64),
) -> Result<()> {
    store.apply_work_signal(
        author,
        &net.network,
        work_id,
        Signal::ThreadCreated(thread.0.into()),
        thread.1,
    )?;
    store.apply_work_signal(
        author,
        &net.network,
        work_id,
        Signal::TurnStarted {
            turn_id: turn.0.into(),
            epoch: 1,
        },
        turn.1,
    )?;
    Ok(())
}

pub(super) fn complete_work(
    store: &mut Store,
    net: &Net,
    author: &LocalIdentity,
    work_id: &str,
    turn: &str,
    text: &str,
    at: i64,
) -> Result<Option<crate::scheduling::AdmissionReceipt>> {
    Ok(store
        .apply_work_signal(
            author,
            &net.network,
            work_id,
            Signal::TurnCompleted {
                turn_id: turn.into(),
                status: vibeface_bridge::turn::TurnStatus::Completed,
                text: text.into(),
                error: None,
            },
            at,
        )?
        .1)
}

pub(super) fn task_state(
    store: &Store,
    net: &Net,
    id: &str,
) -> Result<crate::workspace::TaskState> {
    Ok(store.workspace_task(&net.network, id)?.unwrap().state)
}

pub(super) fn assignment_state(
    store: &Store,
    net: &Net,
    id: &str,
) -> Result<crate::workspace::AssignmentState> {
    Ok(store.workspace_assignment(&net.network, id)?.unwrap().state)
}

pub(super) fn adopt_initial_assignment(
    store: &mut Store,
    net: &Net,
    author: &LocalIdentity,
    assignment_id: &str,
    source: String,
    accepted: (&str, i64),
    adopted: (&str, i64),
) -> Result<()> {
    use crate::workspace::{AssignmentState, WorkspaceCommand};
    for ((command_id, at), state, input_event) in [
        (accepted, AssignmentState::Accepted, None),
        (adopted, AssignmentState::Adopted, Some(source)),
    ] {
        store.apply_workspace_command(
            author,
            &net.network,
            command_id,
            WorkspaceCommand::AssignmentProgress {
                execution: None,
                assignment_id: assignment_id.into(),
                revision: 1,
                state,
                input_event,
                evidence_event: None,
                result: None,
            },
            None,
            None,
            None,
            at,
        )?;
    }
    Ok(())
}

pub(super) fn user(net: &Net, id: &str, text: &str) -> EventBody {
    signed_user(net, id, text, true).expect("fixture client signs")
}

pub(super) fn signed_user(
    net: &Net,
    id: &str,
    text: &str,
    report_results: bool,
) -> Result<EventBody> {
    net.a.fixture_admin().attest(
        &net.network,
        "c",
        EventBody::InputPosted {
            message_id: id.into(),
            target: crate::workspace::InputTarget::BotMain {
                bot_id: "bot".into(),
            },
            intent: crate::workspace::InputIntent::Continue,
            sequence: 1,
            expected_revision: None,
            text: text.into(),
            attachments: vec![],
            report_results: Some(report_results),
            attestation: None,
        },
    )
}

pub(super) fn conversation(store: &mut Store, net: &Net) -> Result<()> {
    let owner = net
        .a
        .owner_fixture(&net.network, "c", &net.a.device_id()?)?;
    store.create_conversation(&net.a, &net.network, "c", "t", owner, 1)?;
    Ok(())
}

pub(super) fn bot_conversation(store: &mut Store, net: &Net) -> Result<()> {
    conversation(store, net)?;
    store.append_event(&net.a, &net.network, "c", bot(net, "Helper")?, vec![], 2)?;
    Ok(())
}

pub(super) fn say(store: &mut Store, net: &Net, id: &str, report_results: bool) -> Result<()> {
    let signed = signed_user(net, id, id, report_results)?;
    store.post_input_fixture(&net.a, &net.network, signed, 2)?;
    Ok(())
}

pub(super) fn owner(net: &Net, node: &LocalIdentity, epoch: u64) -> Result<EventBody> {
    net.a.fixture_admin().attest(
        &net.network,
        "c",
        EventBody::ConversationOwner {
            scheduler_node: node.device_id()?,
            epoch,
            attestation: None,
            cause: None,
        },
    )
}

pub(super) fn bot(net: &Net, name: &str) -> Result<EventBody> {
    net.a.fixture_admin().attest(
        &net.network,
        "c",
        EventBody::BotCreated {
            bot_id: "bot".into(),
            name: name.into(),
            instructions: "be brief".into(),
            project: None,
            attestation: None,
            cause: None,
        },
    )
}

pub(super) fn bot_reply(id: &str, to: &str) -> EventBody {
    EventBody::MessagePosted {
        message_id: id.into(),
        role: Role::Assistant,
        text: "ok".into(),
        attachments: vec![],
        report_results: None,
        in_reply_to: Some(to.into()),
        status: Some(ReplyStatus::Completed),
        answers: None,
        attestation: None,
        failure: None,
    }
}

pub(super) fn save_skill(skill_id: Option<&str>, name: &str, body: &str) -> ChangeRequest {
    ChangeRequest::SaveSkill {
        skill_id: skill_id.map(Into::into),
        name: name.into(),
        description: format!("{name} in short"),
        body: body.into(),
    }
}

pub(super) fn save_schedule(
    schedule_id: Option<&str>,
    name: &str,
    every_minutes: u32,
) -> ChangeRequest {
    ChangeRequest::SaveSchedule {
        schedule_id: schedule_id.map(Into::into),
        name: name.into(),
        prompt: "Summarize yesterday's commits; say so if there are none.".into(),
        first_run_at_ms: 1_000,
        every_minutes: Some(every_minutes),
        enabled: None,
    }
}

pub(super) fn rename(name: &str) -> ChangeRequest {
    ChangeRequest::UpdateProfile {
        name: Some(name.into()),
        instructions: None,
        avatar: None,
        default_avatar: None,
    }
}

pub(super) fn bot_with_message(net: &Net) -> Result<(Store, String)> {
    let mut store = store(net)?;
    conversation(&mut store, net)?;
    store.append_event(&net.a, &net.network, "c", bot(net, "Helper")?, vec![], 2)?;
    let m1 = store.append_event(&net.a, &net.network, "c", user(net, "m1", "x"), vec![], 3)?;
    Ok((store, m1.hash))
}

pub(super) fn second_bot(store: &mut Store, net: &Net, name: &str) -> Result<()> {
    let owner = net
        .a
        .owner_fixture(&net.network, "d", &net.a.device_id()?)?;
    store.create_conversation(&net.a, &net.network, "d", "t", owner, 1)?;
    let created = net.a.fixture_admin().attest(
        &net.network,
        "d",
        EventBody::BotCreated {
            bot_id: "bot-d".into(),
            name: name.into(),
            instructions: String::new(),
            project: None,
            attestation: None,
            cause: None,
        },
    )?;
    store.append_event(&net.a, &net.network, "d", created, vec![], 2)?;
    Ok(())
}

pub(super) fn all_accepted(target: &mut Store, net: &Net, source: &Store) -> Result<()> {
    for author in [&net.a, &net.b] {
        let events = source.events_from(&net.network, &author.device_id()?, 1, 500)?;
        let statuses = target.ingest_events(&net.network, &events)?.statuses;
        assert!(
            statuses
                .iter()
                .all(|status| *status == IngestStatus::Accepted),
            "{statuses:?}"
        );
    }
    Ok(())
}

pub(super) fn scheduled_on_both(net: &Net, every_minutes: u32) -> Result<(Store, Store, String)> {
    let mut a_side = store(net)?;
    bot_conversation(&mut a_side, net)?;
    a_side.append_event(&net.a, &net.network, "c", user(net, "m1", "x"), vec![], 2)?;
    let (saved, _) = a_side.change_bot(
        &net.a,
        &net.network,
        "c",
        "message:m1",
        "r1",
        save_schedule(None, "Digest", every_minutes),
        Provider::Codex,
        3,
    )?;
    let BotChange::Schedule { schedule_id, .. } = saved.change else {
        panic!("schedule change")
    };
    let mut b_side = store(net)?;
    sync_at(&a_side, &mut b_side, net, 3)?;
    Ok((a_side, b_side, schedule_id))
}

pub(super) fn memory_test_shown(store: &Store) -> Result<Shown> {
    let shown: String = store.connection.query_row(
        "SELECT shown FROM scheduler_sessions WHERE conversation_id='c'",
        [],
        |row| row.get(0),
    )?;
    Ok(serde_json::from_str(&shown)?)
}

pub(super) struct Fleet {
    pub(super) net: Net,
    pub(super) x: LocalIdentity,
    pub(super) admin: LocalIdentity,
}

pub(super) fn fleet() -> Result<Fleet> {
    let a = LocalIdentity::generate();
    let b = LocalIdentity::generate();
    let x = LocalIdentity::generate();
    let state = a.sign_network_fixture(1, vec![a.device_id()?, b.device_id()?, x.device_id()?])?;
    Ok(Fleet {
        admin: a.fixture_admin(),
        net: Net {
            network: a.fixture_network_id()?,
            a,
            b,
            state,
        },
        x,
    })
}

pub(super) fn seq_of(envelope: &SignedEvent) -> Result<u64> {
    Ok(serde_json::from_str::<Event>(&envelope.canonical_body)?.seq)
}

pub(super) fn revoke(
    fleet: &Fleet,
    state: &NetworkState,
    device: &LocalIdentity,
    last: Option<&SignedEvent>,
) -> Result<NetworkState> {
    use crate::identity::{GovernanceEvent, Revocation};
    let cutoff = last.map(|event| Ok::<_, anyhow::Error>((seq_of(event)?, event.hash.as_str())));
    let revocation = Revocation::new(&device.device_id()?, cutoff.transpose()?)?;
    let mut next = state.clone();
    next.append(GovernanceEvent::revoke(
        &fleet.admin,
        &state.project()?,
        revocation,
    )?)?;
    Ok(next)
}

pub(super) fn write(
    store: &mut Store,
    net: &Net,
    author: &LocalIdentity,
    id: &str,
    at: i64,
) -> Result<SignedEvent> {
    store.append_event(author, &net.network, "c", user(net, id, id), vec![], at)
}

pub(super) fn created(
    store: &mut Store,
    net: &Net,
    author: &LocalIdentity,
) -> Result<Vec<SignedEvent>> {
    let owner = net
        .a
        .owner_fixture(&net.network, "c", &author.device_id()?)?;
    store.create_conversation(author, &net.network, "c", "t", owner, 1)?;
    store.append_event(author, &net.network, "c", bot(net, "Helper")?, vec![], 2)?;
    store.events_from(&net.network, &author.device_id()?, 1, 100)
}

pub(super) enum Piece {
    Governance(NetworkState),
    Events(Vec<SignedEvent>),
}

pub(super) fn replay(net: &Net, pieces: &[Piece], order: &[usize]) -> Result<Store> {
    let mut replica = store(net)?;
    let events = pieces
        .iter()
        .filter(|piece| matches!(piece, Piece::Events(_)));
    for piece in order.iter().map(|index| &pieces[*index]).chain(events) {
        match piece {
            Piece::Governance(state) => {
                replica.install_trust_bundle(&net.network, state)?;
            }
            Piece::Events(events) => {
                replica.ingest_events(&net.network, events)?;
            }
        }
        assert_full_recompute_oracle(&replica, &net.network)?;
    }
    Ok(replica)
}

pub(super) fn dump(store: &Store, network: &str) -> Result<Vec<String>> {
    let mut dump = Vec::new();
    for table in reduce::TABLES {
        let mut statement = store
            .connection
            .prepare(&format!("SELECT * FROM {table} WHERE network_id=?"))?;
        let columns = statement.column_count();
        let mut rows = statement
            .query_map([network], |r| {
                (0..columns)
                    .map(|i| r.get::<_, rusqlite::types::Value>(i))
                    .collect::<rusqlite::Result<Vec<_>>>()
            })?
            .map(|row| Ok(format!("{table}: {:?}", row?)))
            .collect::<Result<Vec<_>>>()?;
        rows.sort();
        dump.extend(rows);
    }
    let mut statement = store
        .connection
        .prepare("SELECT hash FROM events WHERE network_id=? AND effective=1 ORDER BY hash")?;
    for hash in statement.query_map([network], |r| r.get::<_, String>(0))? {
        dump.push(format!("effective: {}", hash?));
    }
    Ok(dump)
}

pub(super) fn permutations(n: usize) -> Vec<Vec<usize>> {
    if n == 0 {
        return vec![vec![]];
    }
    let mut all = Vec::new();
    for rest in permutations(n - 1) {
        for at in 0..=rest.len() {
            let mut order = rest.clone();
            order.insert(at, n - 1);
            all.push(order);
        }
    }
    all
}

pub(super) fn converges(net: &Net, pieces: &[Piece]) -> Result<Store> {
    let orders = permutations(pieces.len());
    let dumps = orders
        .iter()
        .map(|order| dump(&replay(net, pieces, order)?, &net.network))
        .collect::<Result<Vec<_>>>()?;
    for (order, found) in orders.iter().zip(&dumps) {
        assert_eq!(
            found, &dumps[0],
            "arrival order {order:?} against {:?}",
            orders[0]
        );
    }
    let reference = replay(net, pieces, &orders[0])?;
    reference.connection.execute(
        "UPDATE events SET effective=0 WHERE network_id=?",
        [&net.network],
    )?;
    reduce::clear(&reference.connection, &net.network)?;
    log::recompute(
        &reference.connection,
        &membership_from(&reference.connection, &net.network)?,
    )?;
    assert_eq!(
        dump(&reference, &net.network)?,
        dumps[0],
        "rebuild from scratch"
    );
    Ok(reference)
}

pub(super) fn effective(store: &Store, net: &Net, events: &[&SignedEvent]) -> Result<Vec<bool>> {
    events
        .iter()
        .map(|event| {
            Ok(store
                .connection
                .query_row(
                    "SELECT effective FROM events WHERE network_id=? AND hash=?",
                    params![net.network, event.hash],
                    |r| r.get(0),
                )
                .optional()?
                .unwrap_or(false))
        })
        .collect()
}

pub(super) fn question_conversation(store: &mut Store, net: &Net) -> Result<()> {
    conversation(store, net)?;
    store.append_event(
        &net.a,
        &net.network,
        "c",
        bot(net, "Question Bot")?,
        vec![],
        1,
    )?;
    Ok(())
}

pub(super) fn question(
    store: &Store,
    net: &Net,
    scope: &str,
    id: &str,
    session: &str,
) -> Result<EventBody> {
    Ok(EventBody::QuestionAsked {
        question_id: id.into(),
        target: Some(crate::workspace::InputTarget::BotMain {
            bot_id: store
                .bot_for_conversation(&net.network, scope)?
                .context("question fixture requires a real bot")?
                .bot_id,
        }),
        severity: Severity::Attention,
        text: "要继续吗？".into(),
        choices: vec![Choice {
            label: "继续".into(),
            action: None,
        }],
        about: Subject {
            node: net.a.device_id()?,
            agent: Some(crate::providers::Provider::Codex),
            session_id: session.into(),
            cwd: "/w".into(),
            summary: "tests fail".into(),
            request_id: None,
        },

        attention: AttentionRequest {
            kind: AttentionKind::Question,

            device_id: Some(net.a.device_id()?),
        },
    })
}

pub(super) fn signed_answer(
    net: &Net,
    scope: &str,
    question: &str,
    text: &str,
) -> Result<EventBody> {
    net.a.fixture_admin().attest(
        &net.network,
        scope,
        EventBody::MessagePosted {
            message_id: questions::answer_id(question),
            role: Role::User,
            text: text.into(),
            attachments: vec![],
            report_results: Some(true),
            in_reply_to: None,
            status: None,
            answers: Some(question.into()),
            attestation: None,
            failure: None,
        },
    )
}

pub(super) fn answer_signature(
    net: &Net,
    scope: &str,
    question: &str,
    text: &str,
) -> Result<Attestation> {
    signed_answer(net, scope, question, text)?
        .attestation()
        .cloned()
        .context("signed answer")
}

pub(super) fn sync(from: &Store, to: &mut Store, net: &Net) -> Result<()> {
    sync_at(from, to, net, crate::clock::now_ms())
}

pub(super) fn sync_at(from: &Store, to: &mut Store, net: &Net, seen_at_ms: i64) -> Result<()> {
    let envelopes: Vec<SignedEvent> = from
        .connection
        .prepare("SELECT envelope FROM events WHERE network_id=? ORDER BY author,seq")?
        .query_map([&net.network], |r| r.get::<_, String>(0))?
        .map(|row| Ok(serde_json::from_str(&row?)?))
        .collect::<Result<_>>()?;
    to.ingest_events_at(&net.network, &envelopes, seen_at_ms)?;
    Ok(())
}

pub(super) fn session_version(store: &Store, net: &Net, session: &str) -> Result<String> {
    for scope in ["c", "d"] {
        if let Some(row) = store
            .attributed_sessions(&net.network, scope)?
            .into_iter()
            .find(|row| row["session_id"] == session)
        {
            return Ok(row["version"].as_str().context("version")?.to_owned());
        }
    }
    Ok(String::new())
}

pub(super) fn prompt_data(prompt: &str) -> Result<Value> {
    let start = prompt.rfind("\n\n{").context("prompt data")?;
    Ok(serde_json::from_str(&prompt[start + 2..])?)
}

pub(super) fn listed(data: &Value, pointer: &str, key: &str) -> Vec<Value> {
    data.pointer(pointer)
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|row| row[key].clone())
        .collect()
}

pub(super) fn message_seq(store: &Store, net: &Net, message: &str) -> Result<u64> {
    Ok(super::event_about_from(
        &store.connection,
        &net.network,
        "c",
        "input_posted",
        message,
    )?
    .context("message")?
    .event
    .seq)
}

pub(super) fn isolated_task(store: &mut Store, net: &Net, work_id: &str) -> Result<WorkSpec> {
    let spec = WorkSpec {
        work_id: work_id.into(),
        kind: WorkKind::Task,
        title: work_id.into(),
        prompt: "read the task".into(),
        ..work_spec(
            &net.a.device_id()?,
            &message_trigger("m1"),
            work_id,
            Provider::Codex,
        )
    };
    assert!(
        store
            .accept_work_fixture(&net.a, &net.network, spec.clone(), 4)?
            .1
    );
    Ok(spec)
}

pub(super) fn workspace_fixture() -> Result<(Net, Store)> {
    let net = net()?;
    let mut origin = store(&net)?;
    origin.create_conversation(
        &net.a,
        &net.network,
        "c",
        "workspace",
        net.a
            .owner_fixture(&net.network, "c", &net.a.device_id()?)?,
        1,
    )?;
    origin.append_event(
        &net.a,
        &net.network,
        "c",
        bot(&net, "Workspace Bot")?,
        vec![],
        2,
    )?;
    Ok((net, origin))
}

pub(super) fn workspace_command_fixture(
    net: &Net,
    origin: &mut Store,
    id: &str,
    command: crate::workspace::WorkspaceCommand,
    at: i64,
) -> Result<crate::workspace::WorkspaceCommandReceipt> {
    let signed = net.a.fixture_admin().attest(
        &net.network,
        crate::workspace::WORKSPACE_SCOPE,
        EventBody::WorkspaceCommanded {
            acting_assignment: None,
            command_id: id.into(),
            command: command.clone(),
            attestation: None,
            cause: None,
        },
    )?;
    origin.apply_workspace_command(
        &net.a,
        &net.network,
        id,
        command,
        signed.attestation().cloned(),
        None,
        None,
        at,
    )
}

pub(super) fn workspace_task_command(
    id: &str,
    parent: Option<&str>,
) -> crate::workspace::WorkspaceCommand {
    crate::workspace::WorkspaceCommand::CreateTask {
        task_id: id.into(),
        channel_id: None,
        parent_task_id: parent.map(str::to_owned),
        owner_bot: "bot".into(),
        title: format!("task {id}"),
        request: format!("perform {id}"),
        expected_result: "verified result".into(),
        source_events: vec![],
        dependencies: vec![],
        reply_only: false,
    }
}

pub(super) fn workspace_assigned_task_fixture(
    net: &Net,
    origin: &mut Store,
    command_id: &str,
    task_id: &str,
    input_id: &str,
    roots: &[String],
    at: i64,
) -> Result<(
    crate::workspace::WorkspaceInput,
    crate::workspace::Assignment,
)> {
    workspace_command_fixture(
        net,
        origin,
        command_id,
        workspace_task_command(task_id, None),
        at,
    )?;
    let input = workspace_input_fixture(
        net,
        origin,
        input_id,
        Some(task_id),
        crate::workspace::InputIntent::Continue,
        at + 1,
    )?;
    let assignment = origin
        .ensure_workspace_assignment(&net.a, &net.network, task_id, roots, at + 2)?
        .context("assignment")?;
    Ok((input, assignment))
}

pub(super) fn workspace_input_fixture(
    net: &Net,
    origin: &mut Store,
    id: &str,
    task: Option<&str>,
    intent: crate::workspace::InputIntent,
    at: i64,
) -> Result<crate::workspace::WorkspaceInput> {
    use crate::workspace::*;
    let target = task.map_or_else(
        || InputTarget::BotMain {
            bot_id: "bot".into(),
        },
        |id| InputTarget::Task { task_id: id.into() },
    );
    let expected_revision = task
        .map(|id| {
            origin
                .workspace_task(&net.network, id)?
                .map(|task| task.revision)
                .context("task exists")
        })
        .transpose()?;
    let input = InputRequest {
        message_id: id.into(),
        route: InputRoute {
            target,
            intent,
            sequence: at as u64,
            expected_revision,
        },
        text: format!("input {id}"),
        attachments: vec![],
        report_results: true,
    };
    workspace_signed_input_fixture(net, origin, &net.a, input, at)
}

pub(super) fn workspace_signed_input_fixture(
    net: &Net,
    origin: &mut Store,
    author: &LocalIdentity,
    input: crate::workspace::InputRequest,
    at: i64,
) -> Result<crate::workspace::WorkspaceInput> {
    let conversation = origin
        .resolve_input_target(&net.network, &input.route.target)?
        .conversation_id;
    let signed = net.a.fixture_admin().attest(
        &net.network,
        &conversation,
        crate::workspace::input_event_body(&input, None),
    )?;
    origin.post_workspace_input(
        author,
        &net.network,
        input,
        signed.attestation().cloned().context("input signature")?,
        at,
    )
}

pub(super) fn workspace_started_job_fixture(
    net: &Net,
    origin: &mut Store,
    at: i64,
) -> Result<SchedulerJob> {
    let job = origin
        .claim_next_fixture(&net.a, &net.network, "c", "boot", "task-lifecycle")?
        .context("claimable workspace job")?
        .job;
    let attempt = job.state.attempt().context("attempt")?.id.clone();
    origin.mark_started("c", &job.trigger, &attempt)?;
    origin.update_turn_runtime(
        "c",
        &job.trigger,
        &attempt,
        vibeface_bridge::failure::ProviderStage::Run,
        crate::scheduling::TurnExecution::Started,
    )?;
    origin.settle_workspace_assignment(&net.a, &net.network, &job, at)?;
    origin
        .scheduler_job("c", &job.trigger)?
        .context("started job")
}

pub(super) fn paged(store: &Store, network: &str, limit: usize) -> Result<Vec<Message>> {
    let mut cursor = None;
    let mut all = Vec::new();
    for _ in 0..10_000 {
        let page = store.conversation_messages_page(network, "c", cursor.as_deref(), limit)?;
        assert!(serde_json::to_vec(&page)?.len() < HISTORY_RESPONSE_BYTES);
        assert!(page.items.len() <= limit);
        assert_eq!(page.complete, page.next_cursor.is_none());
        all.splice(0..0, page.items);
        cursor = page.next_cursor;
        if cursor.is_none() {
            return Ok(all);
        }
    }
    bail!("history cursor did not terminate")
}

pub(super) fn shared_state(store: &Store, network: &str) -> Result<Vec<String>> {
    let mut result = dump(store, network)?;
    for table in ["events", "log_heads", "pending_events"] {
        let mut statement = store
            .connection
            .prepare(&format!("SELECT * FROM {table} WHERE network_id=?"))?;
        let columns = statement.column_count();
        let mut rows = statement
            .query_map([network], |row| {
                (0..columns)
                    .map(|index| row.get::<_, rusqlite::types::Value>(index))
                    .collect::<rusqlite::Result<Vec<_>>>()
            })?
            .map(|row| Ok(format!("{table}: {:?}", row?)))
            .collect::<Result<Vec<_>>>()?;
        rows.sort();
        result.extend(rows);
    }
    Ok(result)
}

/// Rebuild every shared projection, even when the effective flags would not change.
pub(super) fn assert_full_recompute_oracle(store: &Store, network: &str) -> Result<()> {
    let before = shared_state(store, network)?;
    store.connection.execute(
        "UPDATE events SET effective=0 WHERE network_id=?",
        [network],
    )?;
    reduce::clear(&store.connection, network)?;
    log::recompute(
        &store.connection,
        &membership_from(&store.connection, network)?,
    )?;
    assert_eq!(shared_state(store, network)?, before);
    Ok(())
}
