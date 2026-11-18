use super::super::bot_settings::ChangeRequest;
use super::super::test_support::{
    Net, all_accepted, answer_signature, bot_conversation, bot_with_message, complete_work, listed,
    memory_test_shown, message_seq, net, prompt_data, question, question_conversation, say,
    second_bot, session_version, store, user, work_spec,
};
use super::super::*;
use crate::providers::Provider;
use crate::scheduling::{
    JobOutcome, JobState, SchedulerJob, SessionScope, Shown, TriggerKind, message_trigger,
};
use crate::work::{Signal, WorkSpec};
use serde_json::json;
use vibeface_bridge::failure::{ExecFailure, FailureKind};

#[test]
fn an_oversized_turn_prompt_keeps_the_newest_context_and_fits() -> Result<()> {
    let net = net()?;
    let mut store = store(&net)?;
    bot_conversation(&mut store, &net)?;
    let text = "x".repeat(3_000);
    for i in 0..40 {
        let message = user(&net, &format!("m{i:02}"), &text);
        store.append_event(&net.a, &net.network, "c", message, vec![], 10 + i)?;
    }
    let no_covers = VersionVector::new();
    let context = context_after(&store.connection, &net.network, "c", None, &no_covers)?;
    let checkpoint = crate::scheduler::checkpoint(&context, &[]).context("over budget")?;
    store.append_event(&net.a, &net.network, "c", checkpoint, vec![], 60)?;
    for i in 40..50 {
        let message = user(&net, &format!("m{i:02}"), &text);
        store.append_event(&net.a, &net.network, "c", message, vec![], 100 + i)?;
    }
    for i in 0..30 {
        let spec = WorkSpec {
            work_id: format!("w{i:02}"),
            prompt: "p".repeat(600),
            ..work_spec(
                &net.a.device_id()?,
                &message_trigger("m49"),
                &format!("call-{i}"),
                Provider::Codex,
            )
        };
        store.accept_work_fixture(&net.a, &net.network, spec, 200 + i)?;
    }
    let context = context_after(&store.connection, &net.network, "c", None, &no_covers)?;
    let works = store.works(&net.network, "c")?;
    let job = SchedulerJob {
        scope: crate::scheduling::SessionScope::Main,
        conversation_id: "c".into(),
        trigger: message_trigger("m49"),
        kind: TriggerKind::Human,
        sequence: 0,
        basis: context.basis.clone(),
        reply_to: "m49".into(),
        message_id: "reply".into(),
        state: JobState::Queued,

        runtime: None,

        pending_check: false,

        tool_calls: false,
    };
    let history = store.turn_history(&net.network, "c", &job, None)?;
    let materialize = |bot: Value| {
        crate::scheduler::materialize(&history, &job, &works, &[], bot, None, &[], None)
            .map(|(prompt, _)| prompt)
    };
    let newest = json!(
        crate::store::event_about_from(
            &store.connection,
            &net.network,
            "c",
            "input_posted",
            "m48"
        )?
        .context("m48")?
        .event
        .seq
    );

    let roomy = prompt_data(&materialize(json!({}))?)?;
    let roomy_works = listed(&roomy, "/works", "work_id");
    assert_eq!(
        roomy_works.last(),
        Some(&json!("w29")),
        "the newest work stays"
    );
    assert!(
        roomy_works
            .windows(2)
            .all(|pair| pair[0].as_str() < pair[1].as_str()),
        "works read oldest to newest"
    );
    assert_eq!(
        roomy["earlier_works_omitted"],
        json!(30 - roomy_works.len())
    );
    assert!(roomy["conversation"]["checkpoint"].is_object());
    assert_eq!(
        listed(&roomy, "/conversation/messages", "seq").last(),
        Some(&newest),
        "the newest message before the trigger closes the history"
    );

    let notes = (0..8)
        .map(|i| json!({"memory_id":format!("n{i}"),"text":"y".repeat(400)}))
        .collect::<Vec<_>>();
    let full_bot = json!({"configured_bot":{"name":"Helper","instructions":"i".repeat(MAX_INSTRUCTIONS_BYTES)},
        "skills":notes,"schedules":notes,"memory":notes,"sessions":notes,"bots":notes});
    let prompt = materialize(full_bot)?;
    assert!(prompt.len() <= crate::scheduler::PROMPT_LIMIT);
    let tight = prompt_data(&prompt)?;
    let seqs = listed(&tight, "/conversation/messages", "seq");
    assert_eq!(seqs.last(), Some(&newest), "the newest messages stay");
    assert!(
        seqs.windows(2)
            .all(|pair| pair[0].as_u64() < pair[1].as_u64())
    );
    assert!(
        listed(&tight, "/works", "work_id").len() < roomy_works.len(),
        "works give way first"
    );
    Ok(())
}

#[test]
fn the_bot_prompt_keeps_its_newest_notes() -> Result<()> {
    let net = net()?;
    let (mut store, _) = bot_with_message(&net)?;
    for i in 0..12 {
        store.change_bot(
            &net.a,
            &net.network,
            "c",
            "message:m1",
            &format!("k{i}"),
            ChangeRequest::Remember {
                memory_id: None,
                text: format!("Note {i:02}: {}", "keep replies short. ".repeat(20)),
            },
            Provider::Codex,
            10 + i,
        )?;
    }
    let bot = crate::scheduler::bot_value(&store, &net.network, "c", 100)?;
    let notes: Vec<String> = listed(&bot, "/memory", "text")
        .iter()
        .filter_map(|text| Some(text.as_str()?[..7].to_owned()))
        .collect();
    let omitted = bot["memory_omitted"].as_u64().context("omitted")? as usize;
    assert!(omitted > 0 && notes.len() + omitted == 12);
    let expected: Vec<String> = (12 - notes.len()..12)
        .map(|i| format!("Note {i:02}"))
        .collect();
    assert_eq!(notes, expected, "the newest notes, oldest first");
    Ok(())
}

#[test]
fn the_bot_prompt_lists_the_newest_sessions_it_follows() -> Result<()> {
    let net = net()?;
    let (mut store, _) = bot_with_message(&net)?;
    second_bot(&mut store, &net, "Other")?;
    let node = net.a.device_id()?;
    let about = |session: &str| Subject {
        node: node.clone(),
        agent: Some(crate::providers::Provider::Codex),
        session_id: session.into(),
        cwd: "/work/project".into(),
        summary: format!("{session}: {}", "tests fail again. ".repeat(6)),
        request_id: None,
    };
    for i in 0..20 {
        store.claim_session(
            &net.a,
            &net.network,
            "c",
            &about(&format!("s{i:02}")),
            10 + i,
        )?;
    }
    store.claim_session(&net.a, &net.network, "d", &about("theirs"), 40)?;
    let seen = session_version(&store, &net, "s19")?;
    let (_, version, _) =
        store.hand_over_session(&net.a, &net.network, "c", "s19", &seen, None, 50)?;
    let bot = crate::scheduler::bot_value(&store, &net.network, "c", 100)?;
    let shown = listed(&bot, "/sessions", "session_id");
    let omitted = bot["sessions_omitted"].as_u64().context("omitted")? as usize;
    assert!(
        omitted > 0 && shown.len() + omitted == 20,
        "the index stays within its budget: {} shown, {omitted} omitted",
        shown.len()
    );
    let expected: Vec<Value> = (20 - shown.len()..20)
        .map(|i| json!(format!("s{i:02}")))
        .collect();
    assert_eq!(shown, expected, "the newest sessions, oldest first");
    let muted = &bot["sessions"][shown.len() - 1];
    assert_eq!(
        (
            muted["following"].clone(),
            muted["cwd"].clone(),
            muted["node"].clone(),
            muted["version"].clone()
        ),
        (
            json!(false),
            json!("/work/project"),
            json!(node),
            json!(version)
        ),
        "a bot names the version it sees"
    );
    assert_eq!(bot["sessions"][0]["following"], json!(true));
    let theirs = crate::scheduler::bot_value(&store, &net.network, "d", 100)?;
    assert_eq!(
        listed(&theirs, "/sessions", "session_id"),
        vec![json!("theirs")]
    );
    Ok(())
}

#[test]
fn a_resumed_codex_turn_sends_only_what_changed() -> Result<()> {
    let net = net()?;
    let (mut store, _) = bot_with_message(&net)?;
    let work = |i: u64| -> Result<WorkSpec> {
        Ok(WorkSpec {
            work_id: format!("w{i}"),
            prompt: "p".repeat(300),
            ..work_spec(
                &net.a.device_id()?,
                &message_trigger("m1"),
                &format!("call-{i}"),
                Provider::Codex,
            )
        })
    };
    for i in 0..3 {
        store.accept_work_fixture(&net.a, &net.network, work(i)?, 10 + i as i64)?;
    }
    let local = net.a.device_id()?;
    let render = |store: &Store, trigger: &str, resumed: Option<&VersionVector>, thread| {
        let job = job_at(store, &net, trigger)?;
        let history = store.turn_history(
            &net.network,
            "c",
            &job,
            resumed.map(|covers| (covers, local.as_str())),
        )?;
        let works = store.works(&net.network, "c")?;
        let bot = crate::scheduler::bot_value(store, &net.network, "c", 100)?;
        let rendered =
            crate::scheduler::materialize(&history, &job, &works, &[], bot, None, &[], thread)?;
        Ok::<_, anyhow::Error>((rendered, job.basis))
    };
    let rule = "Operate only inside this personal network";

    let ((cursor, _), _) = render(&store, "m1", None, None)?;
    assert!(
        cursor.contains(rule),
        "Cursor reads the rules in each prompt"
    );
    let fresh = Shown::default();
    let ((first, shown), covers) = render(&store, "m1", None, Some(&fresh))?;
    assert!(!first.contains(rule), "the rules are thread instructions");
    let data = prompt_data(&first)?;
    assert_eq!(data["bot"]["configured_bot"]["name"], "Helper");
    assert_eq!(listed(&data, "/works", "work_id").len(), 3);
    assert_eq!(data["trigger"]["unavailable_tools"], json!([]));

    store.accept_work_fixture(&net.a, &net.network, work(3)?, 20)?;
    store.append_event(
        &net.a,
        &net.network,
        "c",
        user(&net, "m2", "next"),
        vec![],
        21,
    )?;
    let ((resumed, shown), covers) = render(&store, "m2", Some(&covers), Some(&shown))?;
    let data = prompt_data(&resumed)?;
    assert_eq!(data["bot"], json!({"unchanged":true,"now_ms":100}));
    assert_eq!(listed(&data, "/works", "work_id"), [json!("w3")]);
    assert_eq!(data["works_unchanged"], 3);
    assert_eq!(data["trigger"]["event"]["body"]["text"], "next");
    assert_eq!(
        data["conversation"]["messages"],
        json!([]),
        "the trigger is not repeated among the messages"
    );

    store.change_bot(
        &net.a,
        &net.network,
        "c",
        "message:m2",
        "note",
        ChangeRequest::Remember {
            memory_id: None,
            text: "prefers short replies".into(),
        },
        Provider::Codex,
        22,
    )?;
    store.append_event(
        &net.a,
        &net.network,
        "c",
        user(&net, "m3", "again"),
        vec![],
        23,
    )?;
    let ((changed, _), _) = render(&store, "m3", Some(&covers), Some(&shown))?;
    let data = prompt_data(&changed)?;
    assert_eq!(
        data["memory_context"]["upserts"][0]["text"],
        "prefers short replies"
    );
    assert_eq!(data["works"], json!([]));
    Ok(())
}

#[test]
fn a_new_session_key_opens_a_new_thread_shown_everything() -> Result<()> {
    let net = net()?;
    let mut store = store(&net)?;
    bot_conversation(&mut store, &net)?;
    let shown = Shown {
        bot: Some("bot-digest".into()),
        works: [("w1".to_owned(), "work-digest".to_owned())].into(),
        dynamic: None,
        staged_memories: Default::default(),
        session_generation: 0,
        session_lineage: Vec::new(),
        ..Shown::default()
    };
    let mut turn = |id: &str, key: &str| -> Result<crate::scheduling::ClaimedJob> {
        say(&mut store, &net, id, true)?;
        let claimed = store
            .claim_next_fixture(&net.a, &net.network, "c", "boot", key)?
            .context("job")?;
        let trigger = claimed.job.trigger.clone();
        let attempt = claimed.job.state.attempt().context("attempt")?.id.clone();
        store.mark_started("c", &trigger, &attempt)?;
        store.save_scheduler_native_id("c", &trigger, &attempt, "thread", "token", false)?;
        store.record_manager_turn(&attempt, "system", "thread", Some("turn"), &shown)?;
        store.finish_job(
            &net.a,
            &net.network,
            "c",
            &trigger,
            &attempt,
            JobOutcome::Completed,
            "",
            3,
        )?;
        Ok(claimed)
    };
    assert_eq!(
        turn("m1", "cfg")?.shown,
        Shown {
            session_generation: 1,
            ..Shown::default()
        }
    );
    let resumed = turn("m2", "cfg")?;
    assert_eq!(resumed.previous_native_id.as_deref(), Some("thread"));
    assert_eq!(resumed.shown.session_generation, 1);
    assert!(resumed.shown.session_lineage.is_empty());
    let fresh = turn("m3", "cfg-new-instructions")?;
    assert_eq!(fresh.previous_native_id, None);
    assert_eq!(fresh.shown.session_generation, 2);
    assert_eq!(fresh.shown.session_lineage.len(), 1);
    assert_eq!(fresh.shown.session_lineage[0].generation, 1);
    Ok(())
}

fn job_at(store: &Store, net: &Net, trigger: &str) -> Result<SchedulerJob> {
    Ok(SchedulerJob {
        scope: crate::scheduling::SessionScope::Main,
        conversation_id: "c".into(),
        trigger: message_trigger(trigger),
        kind: TriggerKind::Human,
        sequence: 0,
        basis: store.version_vector(&net.network)?,
        reply_to: trigger.into(),
        message_id: "reply".into(),
        state: JobState::Queued,

        runtime: None,

        pending_check: false,

        tool_calls: false,
    })
}

#[test]
fn compiled_context_tracks_observed_turn_state_without_private_values() -> Result<()> {
    use crate::scheduling::{ContextProjection, ContextStage, QueueHoldReason};
    let net = net()?;
    let mut store = store(&net)?;
    bot_conversation(&mut store, &net)?;
    for (index, outcome, expected) in [
        (1, JobOutcome::Completed, ContextStage::Completed),
        (
            2,
            JobOutcome::Interrupted {
                failure: ExecFailure::new(FailureKind::Lost, "unknown"),
                process_unconfirmed: true,
            },
            ContextStage::Unknown,
        ),
    ] {
        let message = format!("context-{index}");
        say(&mut store, &net, &message, true)?;
        let claimed = store
            .claim_next_fixture(&net.a, &net.network, "c", "boot", "cfg")?
            .context("claim")?;
        let trigger = claimed.job.trigger.clone();
        let attempt = claimed.job.state.attempt().context("attempt")?.id.clone();
        let mut projection = ContextProjection {
            network_id: net.network.clone(),
            scope: SessionScope::Main,
            sources: vec![],
            sources_omitted: 0,
            inputs: vec![],
            inputs_omitted: 0,
            attempt_id: attempt.clone(),
            provider: Provider::Codex,
            stable_fingerprint: crate::crypto::digest(b"stable"),
            dynamic_fingerprint: crate::crypto::digest(b"dynamic"),
            session_generation: claimed.shown.session_generation,
            coverage: Default::default(),
            stable_bytes: 100,
            dynamic_bytes: 200,
            invalidation_reason: None,
            stage: ContextStage::Compiled,
            session_status: None,
        };
        if index == 2 {
            projection.coverage.messages_omitted = 1;
        }
        store.record_compiled_context("c", &trigger, &attempt, &projection)?;
        assert_eq!(
            store
                .scheduler_context_projection("c")?
                .context("compiled")?
                .stage,
            ContextStage::Compiled
        );
        let mut queued = claimed.job.clone();
        queued.state = JobState::Queued;
        let hold = |job: &SchedulerJob| {
            store.scheduler_job_hold(
                &net.network,
                &net.a.device_id()?,
                job,
                Provider::Codex,
                &crate::config::SchedulingLimits::default(),
            )
        };
        assert_eq!(hold(&queued)?, Some(QueueHoldReason::ActiveTurn));
        assert_eq!(hold(&claimed.job)?, None);
        queued.state = JobState::Cancelled {
            attempt: None,
            evidence: "stopped".into(),
        };
        assert_eq!(hold(&queued)?, None);
        let covers_before_native: String = store.connection.query_row(
            "SELECT covers FROM scheduler_sessions WHERE conversation_id='c' AND scope='main'",
            [],
            |row| row.get(0),
        )?;
        store.mark_started("c", &trigger, &attempt)?;
        store.save_scheduler_native_id(
            "c",
            &trigger,
            &attempt,
            "private-native-thread",
            "private-bearer",
            false,
        )?;
        let before = memory_test_shown(&store)?;
        let covers: String = store.connection.query_row(
            "SELECT covers FROM scheduler_sessions WHERE conversation_id='c' AND scope='main'",
            [],
            |row| row.get(0),
        )?;
        assert_eq!(
            covers, covers_before_native,
            "creating a session shows no history"
        );
        store.record_context_started(&attempt)?;
        let observed = memory_test_shown(&store)?;
        assert_eq!(
            observed.context.as_ref().context("started callback")?.stage,
            ContextStage::Started
        );
        assert_eq!(observed.memory, before.memory);
        assert_eq!(observed.works, before.works);
        let covers_started: String = store.connection.query_row(
            "SELECT covers FROM scheduler_sessions WHERE conversation_id='c' AND scope='main'",
            [],
            |row| row.get(0),
        )?;
        if index == 1 {
            assert_eq!(
                serde_json::from_str::<VersionVector>(&covers_started)?,
                claimed.job.basis
            );
        } else {
            assert_eq!(covers_started, covers, "omitted history remains unread");
        }
        store.record_manager_turn(
            &attempt,
            "system",
            "private-native-thread",
            Some("turn"),
            &Shown::default(),
        )?;
        assert_eq!(
            store
                .scheduler_context_projection("c")?
                .context("started")?
                .stage,
            ContextStage::Started
        );
        store.finish_job(
            &net.a,
            &net.network,
            "c",
            &trigger,
            &attempt,
            outcome,
            "reply",
            5,
        )?;
        store.record_context_started(&attempt)?;
        store.record_manager_turn(
            &attempt,
            "system",
            "private-native-thread",
            Some("turn"),
            &Shown::default(),
        )?;
        let context = store
            .scheduler_context_projection("c")?
            .context("terminal")?;
        assert_eq!(context.stage, expected);
        let encoded = serde_json::to_string(&context)?;
        assert!(encoded.len() < 2_048);
        assert!(!encoded.contains("private-native-thread"));
        assert!(!encoded.contains("private-bearer"));
    }
    Ok(())
}

#[test]
fn a_turn_reads_only_past_the_checkpoint_yet_finds_an_older_trigger() -> Result<()> {
    let net = net()?;
    let mut store = store(&net)?;
    bot_conversation(&mut store, &net)?;
    let text = "x".repeat(2_000);
    for i in 0..20 {
        store.append_event(
            &net.a,
            &net.network,
            "c",
            user(&net, &format!("m{i}"), &text),
            vec![],
            10 + i,
        )?;
    }
    let context = context_after(
        &store.connection,
        &net.network,
        "c",
        None,
        &VersionVector::new(),
    )?;
    let checkpoint = crate::scheduler::checkpoint(&context, &[]).context("over budget")?;
    store.append_event(&net.a, &net.network, "c", checkpoint, vec![], 50)?;
    let job = job_at(&store, &net, "m0")?;
    let history = store.turn_history(&net.network, "c", &job, None)?;
    assert!(history.checkpoint.is_some());
    let m0 = message_seq(&store, &net, "m0")?;
    assert!(
        history
            .recent
            .events
            .iter()
            .all(|event| event.event.seq > m0),
        "covered events are not loaded"
    );
    assert_eq!(
        history.trigger.as_ref().map(|event| event.event.seq),
        Some(m0),
        "the trigger is found by its index however old"
    );
    let (prompt, _) =
        crate::scheduler::materialize(&history, &job, &[], &[], json!({}), None, &[], None)?;
    let data = prompt_data(&prompt)?;
    assert_eq!(data["trigger"]["event"]["seq"], json!(m0));
    Ok(())
}

#[test]
fn a_resumed_session_is_not_shown_its_own_replies_or_the_trigger_twice() -> Result<()> {
    let net = net()?;
    let mut store = store(&net)?;
    bot_conversation(&mut store, &net)?;
    store.append_event(
        &net.a,
        &net.network,
        "c",
        user(&net, "m1", "first"),
        vec![],
        10,
    )?;
    let covers = store.version_vector(&net.network)?;
    let reply = |id: &str, status| EventBody::MessagePosted {
        message_id: id.into(),
        role: Role::Assistant,
        text: format!("reply {id}"),
        attachments: vec![],
        report_results: None,
        in_reply_to: Some("m1".into()),
        status: Some(status),
        failure: None,
        answers: None,
        attestation: None,
    };
    store.append_event(
        &net.a,
        &net.network,
        "c",
        reply("r1", ReplyStatus::Completed),
        vec![],
        11,
    )?;
    store.append_event(
        &net.a,
        &net.network,
        "c",
        reply("r2", ReplyStatus::Failed),
        vec![],
        12,
    )?;
    store.append_event(
        &net.a,
        &net.network,
        "c",
        reply("r3", ReplyStatus::Stopped),
        vec![],
        12,
    )?;
    store.append_event(
        &net.a,
        &net.network,
        "c",
        user(&net, "m2", "second"),
        vec![],
        13,
    )?;
    let job = job_at(&store, &net, "m2")?;
    let local = net.a.device_id()?;
    let texts = |resumed: Option<(&VersionVector, &str)>| -> Result<Vec<Value>> {
        let history = store.turn_history(&net.network, "c", &job, resumed)?;
        let (prompt, _) =
            crate::scheduler::materialize(&history, &job, &[], &[], json!({}), None, &[], None)?;
        Ok(listed(
            &prompt_data(&prompt)?,
            "/conversation/messages",
            "text",
        ))
    };
    assert_eq!(
        texts(Some((&covers, &local)))?,
        [json!("reply r2"), json!("reply r3")],
        "the session holds the reply it completed; a failed or stopped one may never have run there"
    );
    assert_eq!(
        texts(None)?,
        [
            json!("first"),
            json!("reply r1"),
            json!("reply r2"),
            json!("reply r3")
        ],
        "a fresh session reads every reply, and the trigger only as the trigger"
    );
    Ok(())
}

#[test]
fn answers_become_checkpoint_decisions_that_later_checkpoints_keep() -> Result<()> {
    let net = net()?;
    let mut store = store(&net)?;
    question_conversation(&mut store, &net)?;
    store.ask(
        &net.a,
        &net.network,
        "c",
        question(&store, &net, "c", "q-1", "t1")?,
        2,
    )?;
    let signed = Some(answer_signature(&net, "c", "q-1", "用方案 B")?);
    store.accept_answer(&net.a, &net.network, "q-1", "用方案 B", signed, 3)?;
    let text = "x".repeat(2_000);
    let mut decisions = Vec::new();
    for round in 0..2 {
        for i in 0..20 {
            let id = format!("m{round}-{i}");
            store.append_event(
                &net.a,
                &net.network,
                "c",
                user(&net, &id, &text),
                vec![],
                10 + 100 * round + i,
            )?;
        }
        let context = crate::store::since_checkpoint_from(&store.connection, &net.network, "c")?;
        let checkpoint = crate::scheduler::checkpoint(&context, &[]).context("over budget")?;
        let EventBody::ContextCheckpoint {
            decisions: found, ..
        } = &checkpoint
        else {
            unreachable!()
        };
        decisions = found.clone();
        store.append_event(
            &net.a,
            &net.network,
            "c",
            checkpoint,
            vec![],
            90 + 100 * round,
        )?;
    }
    assert!(
        decisions
            .iter()
            .any(|decision| decision.contains("q-1") && decision.contains("用方案 B")),
        "the answer survives the rolling summary: {decisions:?}"
    );
    Ok(())
}

#[test]
fn the_prompt_shows_a_pending_attachment_as_pending_download() -> Result<()> {
    let net = net()?;
    let mut store = store(&net)?;
    bot_conversation(&mut store, &net)?;
    let content = b"hello".to_vec();
    let file = Attachment {
        hash: crate::crypto::digest(&content),
        size: content.len() as u64,
        name: "a.txt".into(),
    };
    let message = net.a.fixture_admin().attest(
        &net.network,
        "c",
        EventBody::InputPosted {
            message_id: "m1".into(),
            target: crate::workspace::InputTarget::BotMain {
                bot_id: "bot".into(),
            },
            intent: crate::workspace::InputIntent::Continue,
            sequence: 1,
            expected_revision: None,
            text: "看附件".into(),
            attachments: vec![file.clone()],
            report_results: Some(true),
            attestation: None,
        },
    )?;
    store.put_blob(&file.blob(), &content)?;
    store.append_event(&net.a, &net.network, "c", message, vec![file.blob()], 10)?;
    store.append_event(
        &net.a,
        &net.network,
        "c",
        user(&net, "m2", "再看"),
        vec![],
        11,
    )?;
    let job = job_at(&store, &net, "m2")?;
    let mut history = store.turn_history(&net.network, "c", &job, None)?;
    assert_eq!(
        history
            .shown_events()
            .flat_map(|event| &event.event.blobs)
            .count(),
        1,
        "the attachment the prompt shows is the one a turn fetches"
    );
    history.pending_blobs.insert(file.hash.clone());
    let (prompt, _) =
        crate::scheduler::materialize(&history, &job, &[], &[], json!({}), None, &[], None)?;
    let data = prompt_data(&prompt)?;
    assert_eq!(
        data.pointer("/conversation/messages/0/attachments/0/pending_download"),
        Some(&json!(true))
    );
    Ok(())
}

#[test]
fn a_streamed_event_reads_only_the_runs_on_its_thread() -> Result<()> {
    let net = net()?;
    let (mut store, _) = bot_with_message(&net)?;
    let local = net.a.device_id()?;
    let first = WorkSpec {
        work_id: "w0".into(),
        cwd: "/tmp/t-a".into(),
        thread: Some("t-a".into()),
        ..work_spec(&local, &message_trigger("m1"), "call-0", Provider::Codex)
    };
    store.accept_work_fixture(&net.a, &net.network, first.clone(), 10)?;
    store.accept_work_fixture(
        &net.a,
        &net.network,
        WorkSpec {
            work_id: "w1".into(),
            call_ref: "call-1".into(),
            cwd: "/tmp/t-b".into(),
            thread: None,
            ..first.clone()
        },
        11,
    )?;
    store.apply_work_signal(
        &net.a,
        &net.network,
        "w1",
        Signal::ThreadCreated("t-b".into()),
        12,
    )?;
    let on = |store: &Store, thread| -> Result<Vec<String>> {
        Ok(store
            .work_records_on_thread(thread)?
            .into_iter()
            .map(|record| record.work_id)
            .collect())
    };
    assert_eq!(on(&store, "t-a")?, ["w0"]);
    assert_eq!(on(&store, "t-b")?, ["w1"]);
    assert_eq!(
        store.work_cwd_on_thread("t-b")?.as_deref(),
        Some("/tmp/t-b")
    );
    assert_eq!(store.work_cwd_on_thread("t-c")?, None);
    store.apply_work_signal(
        &net.a,
        &net.network,
        "w0",
        Signal::StartRejected(ExecFailure::new(
            FailureKind::Rejected,
            "native start rejected",
        )),
        13,
    )?;
    store.apply_work_signal(
        &net.a,
        &net.network,
        "w1",
        Signal::TurnStarted {
            turn_id: "u-b".into(),
            epoch: 1,
        },
        14,
    )?;
    complete_work(&mut store, &net, &net.a, "w1", "u-b", "done", 15)?;
    assert!(on(&store, "t-a")?.is_empty());
    assert!(
        on(&store, "t-b")?.is_empty(),
        "stream routing excludes finished journals"
    );
    for thread in ["t-a", "t-b"] {
        assert_eq!(
            store.local_work_provider_on_thread(&net.network, &local, thread)?,
            Some(Provider::Codex)
        );
        assert_eq!(
            store.local_work_provider_on_thread("another-network", &local, thread)?,
            None
        );
        assert_eq!(
            store.local_work_provider_on_thread(&net.network, &net.b.device_id()?, thread)?,
            None
        );
    }
    let mut replica = Store::open(":memory:")?;
    replica.install_trust_bundle(&net.network, &net.state)?;
    all_accepted(&mut replica, &net, &store)?;
    assert_eq!(
        replica.local_work_provider_on_thread(&net.network, &local, "t-b")?,
        None,
        "shared Work evidence without a local journal cannot select a Provider"
    );
    store.accept_work_fixture(
        &net.a,
        &net.network,
        WorkSpec {
            work_id: "conflicting-provider".into(),
            call_ref: "conflicting-provider".into(),
            kind: WorkKind::Agent(Provider::Claude),
            resolved: ResolvedModel {
                provider: Provider::Claude,
                model: None,
            },
            ..first
        },
        16,
    )?;
    assert!(
        store
            .local_work_provider_on_thread(&net.network, &local, "t-a")
            .is_err()
    );
    Ok(())
}

#[test]
fn the_bot_prompt_lists_the_newest_messages_it_sent_other_bots() -> Result<()> {
    let net = net()?;
    let (mut store, _) = bot_with_message(&net)?;
    second_bot(&mut store, &net, "Docs")?;
    for i in 0..6 {
        store.message_bot(
            &net.a,
            &net.network,
            "c",
            "message:m1",
            &format!("ask-{i}"),
            "Docs",
            &format!("Question {i}: {}", "please check the docs. ".repeat(40)),
            10 + i * 3_600_001,
        )?;
    }
    let bot = crate::scheduler::bot_value(&store, &net.network, "c", 100)?;
    let texts: Vec<String> = listed(&bot, "/sent", "text")
        .iter()
        .filter_map(|text| Some(text.as_str()?[..10].to_owned()))
        .collect();
    let omitted = bot["sent_omitted"].as_u64().context("omitted")? as usize;
    assert!(omitted > 0 && texts.len() + omitted == 6, "{texts:?}");
    let expected: Vec<String> = (6 - texts.len()..6)
        .map(|i| format!("Question {i}"))
        .collect();
    assert_eq!(texts, expected, "the newest sent messages, oldest first");
    assert_eq!(bot["sent"][0]["to"]["name"], json!("Docs"));
    assert_eq!(bot["sent"][0]["to"]["conversation_id"], json!("d"));
    let theirs = crate::scheduler::bot_value(&store, &net.network, "d", 100)?;
    assert_eq!(theirs["sent"], json!([]), "only what this bot sent");
    Ok(())
}

#[test]
fn the_bot_prompt_lists_schedules_by_their_next_run() -> Result<()> {
    let net = net()?;
    let (mut store, _) = bot_with_message(&net)?;
    for (call_ref, name, at) in [
        ("a", "alpha", 900_000),
        ("b", "beta", 300_000),
        ("g", "gamma", 600_000),
    ] {
        store.change_bot(
            &net.a,
            &net.network,
            "c",
            "message:m1",
            call_ref,
            ChangeRequest::SaveSchedule {
                schedule_id: None,
                name: name.into(),
                prompt: "Check the deploy and report what changed.".into(),
                first_run_at_ms: at,
                every_minutes: None,
                enabled: None,
            },
            Provider::Codex,
            10,
        )?;
    }
    let bot = crate::scheduler::bot_value(&store, &net.network, "c", 100)?;
    assert_eq!(
        listed(&bot, "/schedules", "name"),
        vec![json!("beta"), json!("gamma"), json!("alpha")],
        "the soonest run first"
    );
    Ok(())
}

#[test]
fn an_oversized_bot_gives_way_instead_of_failing_the_turn() -> Result<()> {
    let net = net()?;
    let mut store = store(&net)?;
    bot_conversation(&mut store, &net)?;
    store.append_event(
        &net.a,
        &net.network,
        "c",
        user(&net, "m1", &format!("go{}", "\n".repeat(4_000))),
        vec![],
        10,
    )?;
    let job = job_at(&store, &net, "m1")?;
    let history = store.turn_history(&net.network, "c", &job, None)?;
    let rows = |key: &str| {
        (0..2)
            .map(|i| json!({"id":format!("{key}{i}"),"text":"\"".repeat(700)}))
            .collect::<Vec<_>>()
    };
    let mut bot = json!({"configured_bot":{"name":"Helper","instructions":"\"".repeat(MAX_INSTRUCTIONS_BYTES)},"now_ms":0});
    for key in ["skills", "schedules", "memory", "sessions", "sent", "bots"] {
        bot[key] = json!(rows(key));
        bot[format!("{key}_omitted")] = json!(0);
    }
    let roots: Vec<std::path::PathBuf> = (0..16)
        .map(|i| format!("/srv/{i}/{}", "deep/".repeat(100)).into())
        .collect();
    let (prompt, shown) =
        crate::scheduler::materialize(&history, &job, &[], &[], bot, None, &roots, None)?;
    assert!(prompt.len() <= crate::scheduler::PROMPT_LIMIT);
    assert_eq!(shown.bot, None, "a bot cut short is shown whole next turn");
    let data = prompt_data(&prompt)?;
    let omitted: u64 = ["skills", "schedules", "memory", "sessions", "sent", "bots"]
        .iter()
        .map(|key| data["bot"][format!("{key}_omitted")].as_u64().unwrap_or(0))
        .sum();
    assert!(omitted > 0, "the bot's indexes give way");
    assert_eq!(
        data["bot"]["configured_bot"]["name"],
        json!("Helper"),
        "the bot itself stays"
    );
    Ok(())
}

#[test]
fn the_prompt_never_carries_the_users_device_signature() -> Result<()> {
    let net = net()?;
    let mut store = store(&net)?;
    bot_conversation(&mut store, &net)?;
    let message = user(&net, "m1", "deploy it");
    let signature = message
        .attestation()
        .context("the user's message is signed")?
        .signature
        .clone();
    store.append_event(&net.a, &net.network, "c", message, vec![], 10)?;
    let job = job_at(&store, &net, "m1")?;
    let history = store.turn_history(&net.network, "c", &job, None)?;
    let (prompt, _) =
        crate::scheduler::materialize(&history, &job, &[], &[], json!({}), None, &[], None)?;
    let data = prompt_data(&prompt)?;
    assert_eq!(data["trigger"]["event"]["body"]["text"], json!("deploy it"));
    assert!(!prompt.contains(&signature), "no signature in the prompt");
    assert!(!prompt.contains("attestation"));
    let seq = message_seq(&store, &net, "m1")?;
    let page = crate::runtime::agent_tools::event_page(
        &store,
        &net.network,
        "c",
        &net.a.device_id()?,
        seq,
        0,
    )?;
    assert!(
        !page.to_string().contains(&signature),
        "nor in what context reads"
    );
    Ok(())
}
