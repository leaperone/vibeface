use super::super::test_support::{
    all_accepted, answer_signature, bot_reply, bot_with_message, isolated_task, net, paged,
    permutations, question, second_bot, session_version, signed_answer, signed_user, start_work,
    store, sync, work_spec, workspace_command_fixture, workspace_fixture, workspace_task_command,
};
use super::super::*;
use crate::providers::Provider;
use crate::scheduling::{SessionScope, Shown, TriggerKind, message_trigger, work_trigger};
use crate::work::{Signal, WorkSpec};
use serde_json::json;

#[test]
fn main_projection_excludes_task_channel_and_unknown_replies() -> Result<()> {
    use crate::workspace::InputTarget;
    let net = net()?;
    let (mut origin, source) = bot_with_message(&net)?;
    let append_signed =
        |store: &mut Store, scope: &str, body: serde_json::Value, at| -> Result<()> {
            let body =
                net.a
                    .fixture_admin()
                    .attest(&net.network, scope, serde_json::from_value(body)?)?;
            store.append_event(&net.a, &net.network, scope, body, vec![], at)?;
            Ok(())
        };
    append_signed(
        &mut origin,
        "workspace",
        json!({
            "type":"workspace_commanded","command_id":"projection-channel",
            "command":{"kind":"create_channel","channel_id":"projection-channel","title":"Channel",
                "purpose":"Scope regression","members":["bot"],"coordinator_bot":"bot",
                "coordinator_node":net.a.device_id()?}
        }),
        4,
    )?;
    append_signed(
        &mut origin,
        "workspace",
        json!({
            "type":"workspace_commanded","command_id":"projection-task",
            "command":{"kind":"create_task","task_id":"projection-task","channel_id":"projection-channel",
                "owner_bot":"bot","title":"Task","request":"Keep this task private",
                "expected_result":"Result","source_events":[source],"dependencies":[],"reply_only":false}
        }),
        5,
    )?;
    let mut main_reply = bot_reply("main-reply", "m1");
    if let EventBody::MessagePosted { text, .. } = &mut main_reply {
        *text = "main visible".into();
    }
    origin.append_event(&net.a, &net.network, "c", main_reply, vec![], 6)?;
    let main_work = isolated_task(&mut origin, &net, "projection-main-work")?;
    assert!(crate::store::scope_projection::work_is_main_from(
        &origin.connection,
        &net.network,
        "c",
        &main_work.work_id
    )?);
    assert!(
        !crate::store::scope_projection::trigger_is_main_from(
            &origin.connection,
            &net.network,
            "c",
            &crate::scheduling::task_trigger(&main_work.work_id)
        )?,
        "Main work facts do not make its native task session a Main reply"
    );
    let assignment_target = InputTarget::ChannelTask {
        channel_id: "projection-channel".into(),
        task_id: "projection-task".into(),
        bot_id: "bot".into(),
    };
    let resolved = origin.resolve_input_target(&net.network, &assignment_target)?;
    workspace_command_fixture(
        &net,
        &mut origin,
        "projection-assign",
        crate::workspace::WorkspaceCommand::AssignTask {
            task_id: "projection-task".into(),
            expected_revision: 1,
            assignment_id: "projection-assignment".into(),
            bot_id: resolved.bot_id,
            execution_node: resolved.owner_node,
            bot_owner_epoch: resolved.owner_epoch,
            channel_owner_epoch: resolved.channel_owner_epoch,
            work_id: None,
            resource_roots: vec![],
            automatic: false,
        },
        7,
    )?;
    let assignment_trigger = "assignment:projection-assignment";
    assert_eq!(
        origin.trigger_input_target(&net.network, "c", assignment_trigger)?,
        Some(assignment_target.clone())
    );
    let derived = WorkSpec {
        work_id: "projection-assignment-work".into(),
        trigger: assignment_trigger.into(),
        call_ref: "assignment-derived".into(),
        kind: WorkKind::Agent(Provider::Codex),
        ..main_work.clone()
    };
    origin.accept_work_fixture(&net.a, &net.network, derived.clone(), 8)?;
    assert!(
        origin
            .workspace_assignment(&net.network, "projection-assignment")?
            .context("source assignment")?
            .work_id
            .is_none()
    );
    assert_eq!(
        origin.work_input_target(&net.network, "c", &derived.work_id)?,
        Some(assignment_target.clone())
    );
    assert!(!crate::store::scope_projection::work_is_main_from(
        &origin.connection,
        &net.network,
        "c",
        &derived.work_id
    )?);
    second_bot(&mut origin, &net, "Other Projection")?;
    assert_eq!(
        origin.trigger_input_target(&net.network, "d", assignment_trigger)?,
        None
    );
    assert_eq!(
        origin.trigger_input_target(&net.network, "c", "assignment:missing")?,
        None
    );
    start_work(
        &mut origin,
        &net,
        &net.a,
        &derived.work_id,
        ("assignment-native-thread", 8),
        ("assignment-native-turn", 9),
    )?;
    assert_eq!(
        origin.native_input_target(&net.network, "c", "assignment-native-thread", "")?,
        Some(assignment_target.clone())
    );
    let assignment = origin
        .workspace_assignment(&net.network, "projection-assignment")?
        .context("source assignment")?;
    let admission = crate::store::scheduler::admit_assignment_from(
        &origin.connection,
        &net.network,
        &net.a.device_id()?,
        &assignment,
    )?;
    let task_job = origin
        .scheduler_job("c", &admission.trigger)?
        .context("Task job")?;
    crate::store::scheduler::admit_from(
        &origin.connection,
        &net.network,
        "c",
        "message:m1",
        TriggerKind::Human,
        "m1",
    )?;
    let main_job = origin
        .scheduler_job("c", "message:m1")?
        .context("Main job")?;
    assert!(!origin.scheduler_native_in_scope(
        &net.network,
        &main_job,
        "assignment-native-thread"
    )?);
    assert!(origin.scheduler_native_in_scope(
        &net.network,
        &task_job,
        "assignment-native-thread"
    )?);
    assert!(
        origin
            .native_input_target(&net.network, "d", "assignment-native-thread", "")
            .is_err()
    );
    assert_eq!(
        origin.native_input_target(&net.network, "c", "unknown-external-thread", "")?,
        None
    );
    assert!(origin.scheduler_native_in_scope(
        &net.network,
        &main_job,
        "unknown-external-thread"
    )?);
    assert!(!origin.scheduler_native_in_scope(
        &net.network,
        &task_job,
        "unknown-external-thread"
    )?);
    for index in 0..2 {
        let work = WorkSpec {
            work_id: format!("main-native-work-{index}"),
            call_ref: format!("main-native-{index}"),
            kind: WorkKind::Agent(Provider::Codex),
            ..main_work.clone()
        };
        origin.accept_work_fixture(&net.a, &net.network, work.clone(), 10)?;
        start_work(
            &mut origin,
            &net,
            &net.a,
            &work.work_id,
            ("main-native-thread", 11),
            (&format!("main-native-turn-{index}"), 12),
        )?;
        assert_eq!(
            origin.native_input_target(
                &net.network,
                "c",
                "main-native-thread",
                &format!("main-native-turn-{index}")
            )?,
            Some(InputTarget::Work {
                work_id: work.work_id.clone()
            })
        );
        if index == 0 {
            let mut own_job = main_job.clone();
            own_job.scope = SessionScope::Task {
                work_id: work.work_id,
            };
            assert!(origin.scheduler_native_in_scope(
                &net.network,
                &own_job,
                "main-native-thread"
            )?);
        }
    }
    assert!(origin.scheduler_native_in_scope(&net.network, &main_job, "main-native-thread")?);
    assert!(!origin.scheduler_native_in_scope(&net.network, &task_job, "main-native-thread")?);
    let mut own_job = main_job.clone();
    own_job.scope = SessionScope::Task {
        work_id: "main-native-work-0".into(),
    };
    assert!(!origin.scheduler_native_in_scope(&net.network, &own_job, "main-native-thread")?);
    assert!(
        origin
            .native_input_target(&net.network, "c", "main-native-thread", "")
            .unwrap_err()
            .to_string()
            .contains("conflicting input targets")
    );
    let conflicting = WorkSpec {
        work_id: "conflicting-native-work".into(),
        trigger: "message:m1".into(),
        call_ref: "conflicting-native".into(),
        kind: WorkKind::Agent(Provider::Codex),
        ..main_work.clone()
    };
    origin.accept_work_fixture(&net.a, &net.network, conflicting.clone(), 10)?;
    start_work(
        &mut origin,
        &net,
        &net.a,
        &conflicting.work_id,
        ("assignment-native-thread", 11),
        ("conflicting-native-turn", 12),
    )?;
    assert!(
        origin
            .scheduler_native_in_scope(&net.network, &main_job, "assignment-native-thread")
            .is_err()
    );
    assert!(
        origin
            .scheduler_native_in_scope(&net.network, &task_job, "assignment-native-thread")
            .is_err()
    );
    let targets = [
        InputTarget::Task {
            task_id: "projection-task".into(),
        },
        assignment_target,
    ];
    for (index, target) in targets.iter().enumerate() {
        let input_id = format!("private-input-{index}");
        append_signed(
            &mut origin,
            "c",
            json!({
                "type":"input_posted","message_id":input_id,"target":target,"intent":"continue",
                "sequence":index + 1,"expected_revision":1,"text":"private input","report_results":true
            }),
            7 + index as i64 * 2,
        )?;
        let private_work = WorkSpec {
            work_id: format!("projection-private-work-{index}"),
            trigger: message_trigger(&input_id),
            call_ref: format!("private-call-{index}"),
            ..main_work.clone()
        };
        origin.accept_work_fixture(
            &net.a,
            &net.network,
            private_work.clone(),
            8 + index as i64 * 2,
        )?;
        assert_eq!(
            origin.trigger_input_target(&net.network, "c", &work_trigger(&private_work.work_id))?,
            Some(target.clone())
        );
        assert!(!crate::store::scope_projection::work_is_main_from(
            &origin.connection,
            &net.network,
            "c",
            &private_work.work_id
        )?);
        let reply = origin.append_event(
            &net.a,
            &net.network,
            "c",
            bot_reply(&format!("private-reply-{index}"), &input_id),
            vec![],
            8 + index as i64 * 2,
        )?;
        assert_eq!(
            origin
                .pending_notifications(&net.network, "web")?
                .into_iter()
                .find(|notification| notification.notification_id == reply.hash)
                .and_then(|notification| notification.target),
            Some(target.clone())
        );
    }
    let unknown = origin.append_event(
        &net.a,
        &net.network,
        "c",
        bot_reply("unknown-reply", "missing-source"),
        vec![],
        12,
    )?;
    assert_eq!(
        origin.trigger_input_target(&net.network, "c", "work:missing-work")?,
        None
    );
    assert!(!crate::store::scope_projection::trigger_is_main_from(
        &origin.connection,
        &net.network,
        "c",
        "work:missing-work"
    )?);
    assert_eq!(
        origin
            .main_messages(&net.network, "c")?
            .iter()
            .map(|message| message.message_id.as_str())
            .collect::<Vec<_>>(),
        ["m1", "main-reply"]
    );
    assert_eq!(
        origin.last_messages(&net.network)?["c"].text,
        "main visible"
    );
    assert_eq!(
        paged(&origin, &net.network, 1)?,
        origin.main_messages(&net.network, "c")?
    );
    let notifications = origin.pending_notifications(&net.network, "web")?;
    assert_eq!(
        notifications
            .iter()
            .find(|note| note.notification_id == unknown.hash)
            .and_then(|note| note.target.as_ref()),
        None
    );
    for notification in notifications
        .into_iter()
        .filter(|notification| matches!(notification.target, Some(InputTarget::BotMain { .. })))
    {
        let body = net.a.fixture_admin().attest(
            &net.network,
            "c",
            EventBody::NotificationAcked {
                notification_id: notification.notification_id,
                client_id: "web".into(),
                attestation: None,
            },
        )?;
        origin.append_event(&net.a, &net.network, "c", body, vec![], 13)?;
    }
    let remaining = origin.pending_notifications(&net.network, "web")?;
    assert_eq!(
        remaining.len(),
        3,
        "Main ack preserves both private scopes and unknown notifications"
    );
    assert!(
        remaining
            .iter()
            .all(|notification| !matches!(notification.target, Some(InputTarget::BotMain { .. })))
    );
    let mut replica = store(&net)?;
    all_accepted(&mut replica, &net, &origin)?;
    assert_eq!(
        replica.pending_notifications(&net.network, "web")?,
        remaining
    );
    assert_eq!(
        replica.last_messages(&net.network)?,
        origin.last_messages(&net.network)?
    );
    assert!(
        replica.scheduler_jobs("c")?.is_empty(),
        "sync never schedules a private input"
    );
    Ok(())
}

#[test]
fn native_targets_require_one_exact_work_or_one_shared_task_target() -> Result<()> {
    use crate::workspace::{InputIntent, InputTarget, WorkspaceCommand};
    for case in ["main", "lineage", "task", "channel", "unknown", "foreign"] {
        let (net, mut origin) = workspace_fixture()?;
        let local = net.a.device_id()?;
        let scoped = matches!(case, "task" | "channel");
        if case == "channel" {
            workspace_command_fixture(
                &net,
                &mut origin,
                "channel",
                WorkspaceCommand::CreateChannel {
                    channel_id: "native-channel".into(),
                    title: "native channel".into(),
                    purpose: "shared native target".into(),
                    members: vec!["bot".into()],
                    coordinator_bot: "bot".into(),
                    coordinator_node: local.clone(),
                },
                3,
            )?;
        }
        if scoped {
            let mut command = workspace_task_command("native-task", None);
            if case == "channel"
                && let WorkspaceCommand::CreateTask { channel_id, .. } = &mut command
            {
                *channel_id = Some("native-channel".into());
            }
            workspace_command_fixture(&net, &mut origin, "task", command, 4)?;
        }
        let expected = match case {
            "task" => InputTarget::Task {
                task_id: "native-task".into(),
            },
            "channel" => InputTarget::ChannelTask {
                channel_id: "native-channel".into(),
                task_id: "native-task".into(),
                bot_id: "bot".into(),
            },
            _ => InputTarget::Work {
                work_id: "native-first".into(),
            },
        };
        let input = net.a.fixture_admin().attest(
            &net.network,
            "c",
            EventBody::InputPosted {
                message_id: "native-source".into(),
                target: if scoped {
                    expected.clone()
                } else {
                    InputTarget::BotMain {
                        bot_id: "bot".into(),
                    }
                },
                intent: InputIntent::Continue,
                sequence: 1,
                expected_revision: scoped.then_some(1),
                text: "native source".into(),
                attachments: vec![],
                report_results: Some(true),
                attestation: None,
            },
        )?;
        origin.append_event(&net.a, &net.network, "c", input, vec![], 5)?;
        let mut unscoped = question(
            &origin,
            &net,
            "c",
            "q-native-unscoped",
            "exact-native-thread",
        )?;
        if let EventBody::QuestionAsked { target, about, .. } = &mut unscoped {
            *target = None;
            origin.claim_session(&net.a, &net.network, "c", about, 5)?;
        }
        crate::store::scheduler::admit_from(
            &origin.connection,
            &net.network,
            "c",
            "message:native-source",
            TriggerKind::Human,
            "native-source",
        )?;
        let source_job = origin
            .scheduler_job("c", "message:native-source")?
            .context("native source job")?;
        let add = |store: &mut Store, id: &str, conversation: &str, trigger: &str| -> Result<()> {
            store.accept_work_fixture(
                &net.a,
                &net.network,
                WorkSpec {
                    work_id: id.into(),
                    conversation: conversation.into(),
                    cwd: "/workspace/native".into(),
                    title: id.into(),
                    prompt: id.into(),
                    thread: Some("exact-native-thread".into()),
                    ..work_spec(&local, trigger, id, Provider::Codex)
                },
                6,
            )?;
            store.apply_work_signal(
                &net.a,
                &net.network,
                id,
                Signal::TurnStarted {
                    turn_id: "exact-native-turn".into(),
                    epoch: 1,
                },
                7,
            )?;
            assert_eq!(
                store
                    .work_record(id)?
                    .context("registered native work")?
                    .state
                    .handle()
                    .map(|handle| handle.turn_id.as_str()),
                Some("exact-native-turn")
            );
            Ok(())
        };
        add(&mut origin, "native-first", "c", "message:native-source")?;
        assert_eq!(
            origin.native_input_target(
                &net.network,
                "c",
                "exact-native-thread",
                "exact-native-turn"
            )?,
            Some(expected.clone()),
            "{case}: the single genuine binding is visible"
        );
        assert_eq!(
            origin.native_input_target(&net.network, "c", "exact-native-thread", "wrong-turn")?,
            None,
            "{case}: an unknown turn never borrows Main"
        );
        if case == "foreign" {
            second_bot(&mut origin, &net, "Foreign")?;
        }
        add(
            &mut origin,
            "native-second",
            if case == "foreign" { "d" } else { "c" },
            if case == "lineage" {
                "task:native-first"
            } else if case == "unknown" {
                "message:missing"
            } else {
                "message:native-source"
            },
        )?;
        let target = origin.native_input_target(
            &net.network,
            "c",
            "exact-native-thread",
            "exact-native-turn",
        );
        if scoped || case == "lineage" {
            assert_eq!(
                target?,
                Some(expected),
                "{case}: only the same exact Task target converges"
            );
            assert!(origin.scheduler_native_in_scope(
                &net.network,
                &source_job,
                "exact-native-thread"
            )?);
            if case == "lineage" {
                let mut own_job = source_job.clone();
                own_job.scope = SessionScope::Task {
                    work_id: "native-first".into(),
                };
                assert!(origin.scheduler_native_in_scope(
                    &net.network,
                    &own_job,
                    "exact-native-thread"
                )?);
                assert!(!crate::store::scope_projection::work_is_main_from(
                    &origin.connection,
                    &net.network,
                    "c",
                    "native-second"
                )?);
            }
        } else {
            let error = target.unwrap_err().to_string();
            assert!(
                error.contains(match case {
                    "unknown" => "unknown input target",
                    "foreign" => "another conversation",
                    _ => "conflicting input targets",
                }),
                "{case}: {error}"
            );
            let in_scope =
                origin.scheduler_native_in_scope(&net.network, &source_job, "exact-native-thread");
            if case == "main" {
                assert!(in_scope?);
            } else {
                assert!(in_scope.is_err(), "{case}: invalid bindings stay rejected");
            }
        }
        origin.ask(&net.a, &net.network, "c", unscoped, 8)?;
        let mut replica = store(&net)?;
        all_accepted(&mut replica, &net, &origin)?;
        for node in [&mut origin, &mut replica] {
            assert!(
                node.question(&net.network, "q-native-unscoped")?
                    .context("unscoped native question")?["target"]
                    .is_null(),
                "{case}: shared native Work bindings cannot become Main through attribution"
            );
            assert!(
                node.accept_answer(
                    &net.a,
                    &net.network,
                    "q-native-unscoped",
                    "continue",
                    Some(answer_signature(
                        &net,
                        "c",
                        "q-native-unscoped",
                        "continue"
                    )?),
                    9,
                )
                .is_err(),
                "{case}: an unknown native target stays rejected on every device"
            );
        }
    }
    Ok(())
}

#[test]
fn a_question_target_is_shared_and_does_not_turn_unknown_into_main() -> Result<()> {
    use crate::workspace::InputTarget;
    let net = net()?;
    let (mut origin, _) = bot_with_message(&net)?;
    let work = isolated_task(&mut origin, &net, "work-question")?;
    let job = origin
        .claim_next_fixture(
            &net.a,
            &net.network,
            "c",
            "projection-boot",
            "projection-cfg",
        )?
        .context("question work")?
        .job;
    origin.record_manager_turn(
        &job.state.attempt().context("attempt")?.id,
        "system",
        "private-thread",
        Some("private-turn"),
        &Shown::default(),
    )?;
    let target = origin
        .native_input_target(&net.network, "c", "private-thread", "private-turn")?
        .context("verified question target")?;
    assert_eq!(
        target,
        InputTarget::Work {
            work_id: work.work_id
        }
    );
    assert_eq!(
        origin.native_input_target(&net.network, "c", "external-thread", "external-turn")?,
        None
    );
    let mut body = question(&origin, &net, "c", "q-scoped", "private-thread")?;
    if let EventBody::QuestionAsked {
        target: question_target,
        ..
    } = &mut body
    {
        *question_target = Some(target.clone());
    }
    origin.ask(&net.a, &net.network, "c", body, 4)?;
    let answer = signed_answer(&net, "c", "q-scoped", "continue")?;
    origin.append_event(&net.a, &net.network, "c", answer, vec![], 5)?;
    origin.append_event(
        &net.a,
        &net.network,
        "c",
        bot_reply("question-reply", "a-q-scoped"),
        vec![],
        6,
    )?;
    let mut unknown = question(&origin, &net, "c", "q-unknown-scope", "external-thread")?;
    if let EventBody::QuestionAsked { target, .. } = &mut unknown {
        *target = None;
    }
    origin.ask(&net.a, &net.network, "c", unknown, 7)?;
    let mut replica = store(&net)?;
    all_accepted(&mut replica, &net, &origin)?;
    for node in [&origin, &replica] {
        assert_eq!(
            node.question(&net.network, "q-scoped")?
                .context("question")?["target"],
            json!(target)
        );
        assert_eq!(
            node.main_messages(&net.network, "c")?
                .iter()
                .map(|message| message.message_id.as_str())
                .collect::<Vec<_>>(),
            ["m1"]
        );
        assert_eq!(
            paged(node, &net.network, 1)?,
            node.main_messages(&net.network, "c")?
        );
        assert_eq!(
            node.trigger_input_target(&net.network, "c", &message_trigger("a-q-scoped"))?,
            Some(target.clone())
        );
        let notifications = node.pending_notifications(&net.network, "web")?;
        assert_eq!(
            notifications
                .iter()
                .filter(|note| note.target == Some(target.clone()))
                .count(),
            2
        );
        assert_eq!(
            notifications
                .iter()
                .filter(|note| note.target.is_none())
                .count(),
            1
        );
    }
    let rejected = origin
        .accept_answer(
            &net.a,
            &net.network,
            "q-unknown-scope",
            "continue",
            Some(answer_signature(&net, "c", "q-unknown-scope", "continue")?),
            8,
        )
        .err()
        .context("an unowned session cannot borrow Main")?;
    assert!(rejected.to_string().contains("trigger target is unknown"));
    assert!(
        origin
            .message(&net.network, "c", "a-q-unknown-scope")?
            .is_none()
    );
    Ok(())
}

#[test]
fn colliding_question_events_keep_their_own_scope_in_history_and_notifications() -> Result<()> {
    use crate::workspace::InputTarget;
    for target in [
        Some(InputTarget::Task {
            task_id: "private-task".into(),
        }),
        Some(InputTarget::Work {
            work_id: "private-work".into(),
        }),
        None,
    ] {
        let net = net()?;
        let (mut origin, _) = bot_with_message(&net)?;
        isolated_task(&mut origin, &net, "private-work")?;
        workspace_command_fixture(
            &net,
            &mut origin,
            "task",
            workspace_task_command("private-task", None),
            5,
        )?;
        let main = question(&origin, &net, "c", "q-collision", "public-thread")?;
        origin.append_event(&net.a, &net.network, "c", main, vec![], 6)?;
        let mut private = question(&origin, &net, "c", "q-collision", "private-thread")?;
        if let EventBody::QuestionAsked {
            target: destination,
            text,
            ..
        } = &mut private
        {
            *destination = target.clone();
            *text = "private question must stay outside Main".into();
        }
        origin.append_event(&net.a, &net.network, "c", private, vec![], 7)?;
        origin.post_input_fixture(
            &net.a,
            &net.network,
            signed_user(&net, "after-collision", "public input", true)?,
            8,
        )?;
        let job = origin
            .scheduler_job("c", "message:after-collision")?
            .context("Main job")?;
        let mut replica = store(&net)?;
        sync(&origin, &mut replica, &net)?;
        for node in [&origin, &replica] {
            let events = context_after(
                &node.connection,
                &net.network,
                "c",
                None,
                &VersionVector::new(),
            )?
            .events;
            let questions: Vec<_> = events
                .iter()
                .filter(|event| {
                    matches!(
                        &event.event.body,
                        EventBody::QuestionAsked { question_id, .. } if question_id == "q-collision"
                    )
                })
                .collect();
            assert_eq!(questions.len(), 2, "both signed events remain effective");
            assert!(node.main_event_allowed(&net.network, "c", questions[0])?);
            assert!(
                !node.main_event_allowed(&net.network, "c", questions[1])?,
                "{target:?}: a losing event cannot borrow the winner's Main target"
            );
            assert_eq!(
                node.question(&net.network, "q-collision")?
                    .context("winning card")?["text"],
                "要继续吗？"
            );
            let history = node.turn_history(&net.network, "c", &job, None)?;
            assert!(!history.recent.events.iter().any(|event| matches!(
                &event.event.body,
                EventBody::QuestionAsked { text, .. } if text == "private question must stay outside Main"
            )), "{target:?}: Main never sees the losing event's private text");
            let notes = node.pending_notifications(&net.network, "web")?;
            let private = notes
                .iter()
                .find(|note| note.text == "private question must stay outside Main")
                .context("private question notification")?;
            assert_eq!(
                private.target, target,
                "a notification's target must describe its own text"
            );
        }
    }
    Ok(())
}

#[test]
fn targetless_questions_require_current_matching_session_attributions() -> Result<()> {
    for case in [
        "attributed",
        "node",
        "provider",
        "session",
        "conversation",
        "muted",
        "deleted",
        "handed-over",
        "late-attribution",
        "other-node-work",
        "other-provider-work",
    ] {
        let net = net()?;
        let (mut origin, _) = bot_with_message(&net)?;
        second_bot(&mut origin, &net, "Other")?;
        let mut card = question(&origin, &net, "c", "q-attributed", "external-thread")?;
        let EventBody::QuestionAsked { target, about, .. } = &mut card else {
            unreachable!()
        };
        *target = None;
        let mut attribution = about.clone();
        match case {
            "node" => attribution.node = net.b.device_id()?,
            "provider" => attribution.agent = Some(Provider::Claude),
            "session" => attribution.session_id = "another-session".into(),
            _ => {}
        }
        if case != "late-attribution" {
            origin.claim_session(
                if case == "node" { &net.b } else { &net.a },
                &net.network,
                if case == "conversation" { "d" } else { "c" },
                &attribution,
                4,
            )?;
        }
        if matches!(case, "other-node-work" | "other-provider-work") {
            let identity = if case == "other-node-work" {
                &net.b
            } else {
                &net.a
            };
            let provider = if case == "other-provider-work" {
                Provider::Claude
            } else {
                Provider::Codex
            };
            origin.accept_work_fixture(
                identity,
                &net.network,
                WorkSpec {
                    work_id: "unrelated-native-work".into(),
                    conversation: "d".into(),
                    cwd: "/w".into(),
                    title: "unrelated".into(),
                    prompt: "unrelated".into(),
                    thread: Some("external-thread".into()),
                    ..work_spec(
                        &identity.device_id()?,
                        "message:unrelated",
                        "unrelated",
                        provider,
                    )
                },
                5,
            )?;
        }
        origin.ask(&net.a, &net.network, "c", card, 6)?;
        match case {
            "muted" | "handed-over" => {
                let version = session_version(&origin, &net, "external-thread")?;
                origin.hand_over_session(
                    &net.a,
                    &net.network,
                    "c",
                    "external-thread",
                    &version,
                    (case == "handed-over").then_some("d"),
                    7,
                )?;
            }
            "deleted" => {
                let deleted = net.a.fixture_admin().attest(
                    &net.network,
                    "c",
                    EventBody::BotDeleted {
                        cause: None,
                        attestation: None,
                    },
                )?;
                origin.append_event(&net.a, &net.network, "c", deleted, vec![], 7)?;
            }
            "late-attribution" => {
                origin.claim_session(&net.a, &net.network, "c", &attribution, 7)?;
            }
            _ => {}
        }
        let expected = matches!(
            case,
            "attributed" | "other-node-work" | "other-provider-work"
        )
        .then(|| crate::workspace::InputTarget::BotMain {
            bot_id: "bot".into(),
        });
        let mut replica = store(&net)?;
        sync(&origin, &mut replica, &net)?;
        for node in [&mut origin, &mut replica] {
            assert_eq!(
                node.question(&net.network, "q-attributed")?
                    .context("attributed question")?["target"],
                json!(expected),
                "{case}: only this question's current signed attribution proves Main"
            );
            let answered = node.accept_answer(
                &net.a,
                &net.network,
                "q-attributed",
                "continue",
                Some(answer_signature(&net, "c", "q-attributed", "continue")?),
                8,
            );
            assert_eq!(answered.is_ok(), expected.is_some(), "{case}");
            if expected.is_none() {
                assert!(node.message(&net.network, "c", "a-q-attributed")?.is_none());
            }
        }
    }
    Ok(())
}

#[test]
fn reply_targets_follow_winning_rows_and_ranked_steers_in_any_order() -> Result<()> {
    use crate::workspace::{InputIntent, InputTarget, WorkspaceCommand};
    for order in permutations(3) {
        let (net, mut origin) = workspace_fixture()?;
        workspace_command_fixture(
            &net,
            &mut origin,
            "task",
            workspace_task_command("task", None),
            3,
        )?;
        workspace_command_fixture(
            &net,
            &mut origin,
            "assign",
            WorkspaceCommand::AssignTask {
                task_id: "task".into(),
                expected_revision: 1,
                assignment_id: "assignment.with.dots".into(),
                bot_id: "bot".into(),
                execution_node: net.a.device_id()?,
                bot_owner_epoch: 1,
                channel_owner_epoch: None,
                work_id: None,
                resource_roots: vec![],
                automatic: false,
            },
            4,
        )?;
        let [early, late, direct] =
            ["early", "late", "direct"].map(|id| InputTarget::Task { task_id: id.into() });
        let apply = |scope: &str, body: EventBody, rank: u64, author: &LocalIdentity| {
            let event = Event {
                version: WIRE_VERSION,
                network_id: net.network.clone(),
                author: author.device_id()?,
                seq: 1,
                prev: None,
                deps: vec![],
                scope_id: scope.into(),
                created_at_ms: rank as i64,
                body,
                blobs: vec![],
            };
            let envelope = author.sign_event(event.clone())?;
            // Reducer fixtures persist the event while controlling its projection rank.
            origin.connection.execute(
                "INSERT INTO events(network_id,author,seq,hash,scope_id,lamport,created_at_ms,seen_at_ms,envelope,main,effective,kind,subject,causal_chain,causal_clock) VALUES(?,?,?,?,?,?,?,?,?,0,1,?,?,?,'{}')",
                params![net.network, event.author, event.seq, envelope.hash, scope, 1000 + rank, event.created_at_ms, rank as i64, serde_json::to_string(&envelope)?, event.body.kind(), event.body.subject(), envelope.hash],
            )?;
            reduce::apply(
                &origin.connection,
                &net.network,
                &envelope.hash,
                &event,
                rank as i64,
                &crate::store::rank(1000 + rank, rank as i64, &envelope.hash),
                false,
            )
        };
        let question_body = |id: &str, target: Option<InputTarget>| -> Result<EventBody> {
            let mut body = question(&origin, &net, "c", id, "session")?;
            if let EventBody::QuestionAsked {
                target: current, ..
            } = &mut body
            {
                *current = target;
            }
            Ok(body)
        };
        let answer = |id: &str| signed_answer(&net, "c", id, "answer");
        let target =
            |id: &str| workspace::message_target_from(&origin.connection, &net.network, "c", id);
        for piece in &order {
            match piece {
                0 | 1 => {
                    let (id, value, rank) = if *piece == 0 {
                        ("q-early", &early, 10)
                    } else {
                        ("q-late", &late, 20)
                    };
                    apply("c", question_body(id, Some(value.clone()))?, rank, &net.a)?;
                    apply("c", answer(id)?, rank + 20, &net.a)?;
                    apply(
                        "c",
                        EventBody::MessageSteered {
                            message_id: format!("a-{id}"),
                            reply: "reply".into(),
                        },
                        rank + 100,
                        &net.a,
                    )?;
                }
                2 => {
                    apply("c", bot_reply("reply", "a-q-direct"), 80, &net.a)?;
                    apply("c", bot_reply("chain", "reply"), 90, &net.a)?;
                }
                _ => unreachable!(),
            }
        }
        assert_eq!(target("reply")?, Some(early.clone()), "{order:?}");
        assert_eq!(target("chain")?, Some(early.clone()), "{order:?}");
        apply("c", answer("q-direct")?, 50, &net.a)?;
        assert_eq!(target("reply")?, Some(early.clone()));
        apply(
            "c",
            question_body("q-direct", Some(direct.clone()))?,
            60,
            &net.a,
        )?;
        assert_eq!(target("reply")?, Some(direct.clone()));
        assert_eq!(target("chain")?, Some(direct.clone()));
        apply(
            "c",
            question_body("q-direct", Some(late.clone()))?,
            70,
            &net.a,
        )?;
        apply("c", bot_reply("reply", "a-q-late"), 85, &net.b)?;
        assert_eq!(
            target("reply")?,
            Some(direct.clone()),
            "losing events keep the winning row"
        );
        apply(
            "other",
            question_body("q-direct", Some(direct.clone()))?,
            55,
            &net.a,
        )?;
        assert_eq!(
            target("a-q-direct")?,
            None,
            "a question cannot cross conversation boundaries"
        );
        assert_eq!(target("reply")?, Some(early.clone()));
        apply("c", bot_reply("a-q-early", "a-q-late"), 25, &net.a)?;
        assert_eq!(target("reply")?, Some(late.clone()));
        let input = |id: &str| -> Result<EventBody> {
            net.a.fixture_admin().attest(
                &net.network,
                "c",
                EventBody::InputPosted {
                    message_id: id.into(),
                    target: direct.clone(),
                    intent: InputIntent::Continue,
                    sequence: 1,
                    expected_revision: None,
                    text: "input".into(),
                    attachments: vec![],
                    report_results: Some(true),
                    attestation: None,
                },
            )
        };
        apply("c", input("a-q-early")?, 24, &net.a)?;
        let row: (String, Option<String>, Option<String>, Option<String>) = origin.connection.query_row(
            "SELECT role,answers,in_reply_to,steered_into FROM messages WHERE network_id=? AND conversation_id='c' AND message_id='a-q-early'",
            [&net.network], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )?;
        assert_eq!(row, ("user".into(), None, None, Some("reply".into())));
        assert_eq!(target("reply")?, Some(direct.clone()));
        assert_eq!(target("chain")?, Some(direct.clone()));
        apply("c", input("assignment.with.dots")?, 140, &net.a)?;
        apply(
            "c",
            bot_reply("assigned", "assignment.with.dots"),
            150,
            &net.a,
        )?;
        apply(
            "c",
            bot_reply("other-author", "assignment.with.dots"),
            151,
            &net.b,
        )?;
        apply(
            "other",
            bot_reply("other-scope", "assignment.with.dots"),
            152,
            &net.a,
        )?;
        assert_eq!(
            target("assigned")?,
            Some(InputTarget::Task {
                task_id: "task".into()
            })
        );
        assert_eq!(target("other-author")?, Some(direct));
        assert_eq!(
            workspace::message_target_from(
                &origin.connection,
                &net.network,
                "other",
                "other-scope"
            )?,
            None
        );
        assert_eq!(
            workspace::workspace_assignment_from(
                &origin.connection,
                "other-network",
                "assignment.with.dots"
            )?,
            None
        );
        assert_eq!(
            workspace::workspace_assignment_from(&origin.connection, &net.network, "assignment")?,
            None,
            "an assignment ID is an exact JSON object key"
        );
    }
    Ok(())
}

#[test]
fn reply_target_refresh_visits_only_related_mixed_history() -> Result<()> {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    let net = net()?;
    let mut store = store(&net)?;
    let target = serde_json::to_string(&crate::workspace::InputTarget::Task {
        task_id: "task".into(),
    })?;
    store.connection.execute(
        "INSERT INTO messages(network_id,conversation_id,message_id,event_hash,rank,author,role,text,created_at_ms,attachments,target) VALUES(?,'c','input','input','0','author','user','input',0,'[]',?)",
        params![net.network, target],
    )?;
    store.connection.execute(
        "INSERT INTO messages(network_id,conversation_id,message_id,event_hash,rank,author,role,text,created_at_ms,attachments,in_reply_to) VALUES(?,'c','reply','reply','1','author','assistant','reply',0,'[]','input')",
        [&net.network],
    )?;
    let mut steps = Vec::new();
    for history in [0, 2000] {
        for i in 0..history {
            store.connection.execute(
                "INSERT INTO messages(network_id,conversation_id,message_id,event_hash,rank,author,role,text,created_at_ms,attachments,in_reply_to,steered_into,target) VALUES(?1,'c',?2,?2,?2,'author',CASE WHEN ?3%2=0 THEN 'user' ELSE 'assistant' END,'unrelated',0,'[]',?2,?2,?4)",
                params![net.network, format!("unrelated-{i}"), i, target],
            )?;
        }
        store.connection.execute(
            "UPDATE messages SET target=NULL WHERE network_id=? AND message_id='reply'",
            [&net.network],
        )?;
        let count = Arc::new(AtomicUsize::new(0));
        let observed = count.clone();
        store.connection.progress_handler(
            1,
            Some(move || {
                observed.fetch_add(1, Ordering::Relaxed);
                false
            }),
        )?;
        let result =
            workspace::refresh_reply_targets(&store.connection, &net.network, "c", "input");
        store.connection.progress_handler(0, None::<fn() -> bool>)?;
        result?;
        steps.push(count.load(Ordering::Relaxed));
        assert_eq!(
            store
                .message(&net.network, "c", "reply")?
                .context("reply")?
                .target,
            Some(crate::workspace::InputTarget::Task {
                task_id: "task".into()
            })
        );
    }
    assert!(
        steps[1] < steps[0] * 2,
        "unrelated mixed history increased projection work: {steps:?}"
    );
    eprintln!("reply target SQLite VM steps with 0/2000 unrelated mixed messages: {steps:?}");
    Ok(())
}
