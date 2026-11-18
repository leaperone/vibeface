use super::super::test_support::{
    Net, answer_signature, conversation, net, question, question_conversation, second_bot, store,
    sync, work_spec,
};
use super::super::*;
use crate::providers::Provider;
use crate::scheduling::{SessionScope, TriggerKind, message_trigger};
use crate::work::WorkSpec;
use serde_json::json;

#[test]
fn a_retried_signed_decision_never_changes_the_outcome() -> Result<()> {
    let owner = |node: &LocalIdentity| -> Result<EventBody> {
        Ok(EventBody::ConversationOwner {
            scheduler_node: node.device_id()?,
            epoch: 2,
            attestation: None,
            cause: None,
        })
    };
    let write = |store: &mut Store,
                 net: &Net,
                 at: &mut i64,
                 author: &LocalIdentity,
                 body: EventBody|
     -> Result<String> {
        *at += 1;
        let signed = net.a.fixture_admin().attest(&net.network, "c", body)?;
        Ok(store
            .append_event(author, &net.network, "c", signed, vec![], *at)?
            .hash)
    };
    let outcome = |store: &Store, net: &Net| -> Result<Option<String>> {
        Ok(store
            .conversation(&net.network, "c")?
            .context("conversation")?
            .owner)
    };
    // Hashes are random, so a winner hashed in the upper half is drawn first; each retry then
    // undercuts it with probability at least one half.
    for _ in 0..64 {
        let net = net()?;
        let mut store = store(&net)?;
        conversation(&mut store, &net)?;
        let mut at = 10;
        let mut originals = vec![];
        for body in [owner(&net.a)?, owner(&net.b)?] {
            originals.push(write(&mut store, &net, &mut at, &net.a, body)?);
        }
        let settled = outcome(&store, &net)?;
        let losing_owner = if settled == Some(net.a.device_id()?) {
            &net.b
        } else {
            &net.a
        };
        let winning_owner =
            originals[usize::from(losing_owner.device_id()? == net.a.device_id()?)].clone();
        if winning_owner.as_str() < "8" {
            continue;
        }
        for _ in 0..64 {
            let retry = write(&mut store, &net, &mut at, &net.b, owner(losing_owner)?)?;
            assert_eq!(
                outcome(&store, &net)?,
                settled,
                "a retry changed a settled decision"
            );
            if retry < winning_owner {
                return Ok(());
            }
        }
    }
    panic!("no retry hashed below the winner, so the outer-hash tie-break was never exercised")
}

fn unanswered(store: &Store, net: &Net) -> Result<Vec<String>> {
    let mut ids: Vec<String> = store
        .unanswered_questions(&net.network)?
        .iter()
        .map(|question| {
            question["question_id"]
                .as_str()
                .unwrap_or_default()
                .to_owned()
        })
        .collect();
    ids.sort();
    Ok(ids)
}

#[test]
fn an_answer_reaches_the_agent_once_across_retries_and_restarts() -> Result<()> {
    let net = net()?;
    let dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../.artifacts")
        .join(format!("store-{}", uuid::Uuid::new_v4()));
    crate::config::private_dir(&dir)?;
    let path = dir.join("node.sqlite3");
    let result = (|| -> Result<()> {
        let mut store = Store::open(&path)?;
        store.install_trust_bundle(&net.network, &net.state)?;
        question_conversation(&mut store, &net)?;
        let mut card = question(&store, &net, "c", "q-1", "t1")?;
        if let EventBody::QuestionAsked { target, about, .. } = &mut card {
            *target = None;
            store.claim_session(&net.a, &net.network, "c", about, 2)?;
        }
        store.ask(&net.a, &net.network, "c", card, 2)?;
        let signed = Some(answer_signature(&net, "c", "q-1", "允许")?);
        let first = store.accept_answer(&net.a, &net.network, "q-1", "允许", signed.clone(), 3)?;
        assert!(!first.already_answered && first.receipt.is_some());
        let retry = store.accept_answer(&net.a, &net.network, "q-1", "允许", signed, 4)?;
        assert!(!retry.already_answered && retry.message == first.message);
        let webhook = store.accept_answer(&net.a, &net.network, "q-1", "拒绝", None, 5)?;
        assert!(webhook.already_answered && webhook.message.text == "允许");
        let trigger = message_trigger(&questions::answer_id("q-1"));
        store.accept_work_fixture(
            &net.a,
            &net.network,
            WorkSpec {
                work_id: "answered-external-session".into(),
                cwd: "/w".into(),
                title: "answer".into(),
                prompt: "continue".into(),
                thread: Some("t1".into()),
                ..work_spec(&net.a.device_id()?, &trigger, "answer", Provider::Codex)
            },
            6,
        )?;
        let job = store.scheduler_job("c", &trigger)?.context("answer job")?;
        assert_eq!(job.scope, SessionScope::Main);
        let history = store.turn_history(&net.network, "c", &job, None)?;
        assert!(
            history.recent.events.iter().any(|event| matches!(
                &event.event.body,
                EventBody::QuestionAsked { question_id, text, target: None, .. }
                    if question_id == "q-1" && text == "要继续吗？"
            )),
            "the original question remains in Main after its answer starts Work"
        );
        let expected = crate::workspace::InputTarget::BotMain {
            bot_id: "bot".into(),
        };
        assert_eq!(
            store
                .question(&net.network, "q-1")?
                .context("projected question")?["target"],
            json!(expected)
        );
        assert_eq!(
            store.trigger_input_target(&net.network, "c", &trigger)?,
            Some(expected.clone())
        );
        assert!(
            store
                .pending_notifications(&net.network, "web")?
                .iter()
                .any(|note| note.kind == "question" && note.target.as_ref() == Some(&expected))
        );
        drop(store);
        let mut store = Store::open(&path)?;
        let trigger = message_trigger(&questions::answer_id("q-1"));
        store.admit_delivery(&net.a.device_id()?, &net.network, "c", &trigger)?;
        let jobs = store.scheduler_jobs("c")?;
        assert_eq!(
            jobs.iter()
                .map(|job| (job.trigger.clone(), job.kind))
                .collect::<Vec<_>>(),
            vec![(trigger, TriggerKind::Answer)]
        );
        let answers = context_after(
            &store.connection,
            &net.network,
            "c",
            None,
            &VersionVector::new(),
        )?
        .events
        .iter()
        .filter(|event| {
            matches!(
                &event.event.body,
                EventBody::MessagePosted {
                    answers: Some(_),
                    ..
                }
            )
        })
        .count();
        assert_eq!(answers, 1, "one answer event");
        assert!(unanswered(&store, &net)?.is_empty());
        Ok(())
    })();
    std::fs::remove_dir_all(dir)?;
    result
}

#[test]
fn the_answer_to_a_settings_card_turns_into_apply_provider_config() -> Result<()> {
    let net = net()?;
    let mut store = store(&net)?;
    question_conversation(&mut store, &net)?;
    let mut card = question(&store, &net, "c", "q-settings", "provider-setup-1")?;
    if let EventBody::QuestionAsked { about, .. } = &mut card {
        about.agent = None;
    }
    store.ask(&net.a, &net.network, "c", card, 2)?;
    store.ask(
        &net.a,
        &net.network,
        "c",
        question(&store, &net, "c", "q-session", "t1")?,
        3,
    )?;
    let mut roles = std::collections::BTreeMap::new();
    for (id, time) in [("q-settings", 4), ("q-session", 5)] {
        let signed = Some(answer_signature(&net, "c", id, "继续")?);
        store.accept_answer(&net.a, &net.network, id, "继续", signed, time)?;
        let trigger = message_trigger(&questions::answer_id(id));
        store.admit_delivery(&net.a.device_id()?, &net.network, "c", &trigger)?;
        let job = store
            .scheduler_jobs("c")?
            .into_iter()
            .find(|job| job.trigger == trigger)
            .context("answer job")?;
        let history = store.turn_history(&net.network, "c", &job, None)?;
        let works = store.works(&net.network, "c")?;
        let (prompt, _) =
            crate::scheduler::materialize(&history, &job, &works, &[], json!({}), None, &[], None)?;
        roles.insert(id, prompt.lines().next().unwrap_or_default().to_owned());
    }
    assert!(
        roles["q-settings"].contains("apply_provider_config")
            && !roles["q-settings"].contains("dispatch_work"),
        "a settings card's answer is applied, never sent to its placeholder session: {}",
        roles["q-settings"]
    );
    assert!(
        roles["q-session"].contains("dispatch_work"),
        "an answer about a session still reaches the session"
    );
    Ok(())
}

#[test]
fn an_answer_on_one_device_is_answered_for_every_reader() -> Result<()> {
    let net = net()?;
    let (mut a, mut b) = (store(&net)?, store(&net)?);
    question_conversation(&mut a, &net)?;
    let mut card = question(&a, &net, "c", "q-1", "t1")?;
    if let EventBody::QuestionAsked { target, about, .. } = &mut card {
        *target = None;
        a.claim_session(&net.a, &net.network, "c", about, 2)?;
    }
    a.ask(&net.a, &net.network, "c", card, 2)?;
    sync(&a, &mut b, &net)?;
    assert_eq!(unanswered(&b, &net)?, vec!["q-1".to_owned()]);
    let signed = answer_signature(&net, "c", "q-1", "继续")?;
    let signer = signed.device_id.clone();
    let answered = b.accept_answer(&net.b, &net.network, "q-1", "继续", Some(signed), 3)?;
    assert!(b.scheduler_jobs("c")?.is_empty());
    let delivery = b.scheduler_outbox()?;
    assert_eq!(
        delivery
            .iter()
            .map(|entry| entry.trigger.clone())
            .collect::<Vec<_>>(),
        vec![answered.receipt.context("queued for the owner")?.trigger]
    );
    sync(&b, &mut a, &net)?;
    let trigger = &delivery[0].trigger;
    a.admit_delivery(&net.a.device_id()?, &net.network, "c", trigger)?;
    assert_eq!(a.scheduler_jobs("c")?[0].kind, TriggerKind::Answer);
    for store in [&a, &b] {
        assert!(unanswered(store, &net)?.is_empty());
        let questions = store.conversation_questions(&net.network, "c")?;
        assert_eq!(questions[0]["answer"]["text"], "继续");
        assert_eq!(questions[0]["answer"]["message_id"], "a-q-1");
        assert_eq!(
            questions[0]["answer"]["author"],
            signer.as_str(),
            "the signing device answered, not the entry Node"
        );
    }
    Ok(())
}

fn approval(store: &Store, net: &Net, id: &str, request: &str) -> Result<EventBody> {
    let mut body = question(store, net, "c", id, "t1")?;
    if let EventBody::QuestionAsked { about, .. } = &mut body {
        about.request_id = Some(request.into());
    }
    Ok(body)
}

fn closed(store: &Store, net: &Net, question: &str) -> Result<Value> {
    Ok(store
        .question(&net.network, question)?
        .context("question")?["closed"]
        .clone())
}

fn held(requests: &[&str]) -> std::collections::HashSet<String> {
    requests
        .iter()
        .map(|request| (*request).to_owned())
        .collect()
}

#[test]
fn a_lost_request_closes_its_question_on_every_device_once() -> Result<()> {
    let net = net()?;
    let (mut a, mut b) = (store(&net)?, store(&net)?);
    question_conversation(&mut a, &net)?;
    a.ask(
        &net.a,
        &net.network,
        "c",
        approval(&a, &net, "q-1", "r-1")?,
        2,
    )?;
    a.ask(
        &net.a,
        &net.network,
        "c",
        approval(&a, &net, "q-2", "r-2")?,
        2,
    )?;
    a.ask(
        &net.a,
        &net.network,
        "c",
        question(&a, &net, "c", "q-3", "t3")?,
        2,
    )?;
    let written = a.close_lost_questions(&net.a, &net.network, &held(&["r-2"]), 3)?;
    assert_eq!(
        written
            .iter()
            .map(|(scope, _)| scope.as_str())
            .collect::<Vec<_>>(),
        vec!["c"],
        "only the request no longer held closes its question"
    );
    assert!(
        a.close_lost_questions(&net.a, &net.network, &held(&["r-2"]), 4)?
            .is_empty(),
        "a closed question is closed once"
    );
    sync(&a, &mut b, &net)?;
    for store in [&a, &b] {
        assert_eq!(unanswered(store, &net)?, vec!["q-2", "q-3"]);
        assert_eq!(closed(store, &net, "q-1")?, "request_lost");
        let chat = store.conversation_questions(&net.network, "c")?;
        assert_eq!(chat.len(), 3, "a closed question stays in its chat");
    }
    let signed = Some(answer_signature(&net, "c", "q-1", "允许")?);
    let refused = b
        .accept_answer(&net.b, &net.network, "q-1", "允许", signed, 5)
        .err()
        .context("answering a closed question fails")?;
    assert!(refused.to_string().contains("closed"), "{refused}");
    assert!(
        b.messages(&net.network, "c")?.is_empty(),
        "nothing is posted"
    );
    Ok(())
}

#[test]
fn only_the_asking_device_closes_its_question() -> Result<()> {
    let net = net()?;
    let (mut a, mut b) = (store(&net)?, store(&net)?);
    question_conversation(&mut a, &net)?;
    a.ask(
        &net.a,
        &net.network,
        "c",
        approval(&a, &net, "q-1", "r-1")?,
        2,
    )?;
    sync(&a, &mut b, &net)?;
    assert!(
        b.close_lost_questions(&net.b, &net.network, &held(&[]), 3)?
            .is_empty(),
        "another device holds none of this device's requests"
    );
    let forged = EventBody::QuestionClosed {
        question_id: "q-1".into(),
        reason: CloseReason::RequestLost,
    };
    b.append_event(&net.b, &net.network, "c", forged, vec![], 3)?;
    sync(&b, &mut a, &net)?;
    for store in [&a, &b] {
        assert_eq!(unanswered(store, &net)?, vec!["q-1"]);
        assert_eq!(closed(store, &net, "q-1")?, Value::Null);
    }
    Ok(())
}

#[test]
fn an_answer_wins_over_a_close_in_either_order() -> Result<()> {
    let net = net()?;
    let (mut a, mut b) = (store(&net)?, store(&net)?);
    question_conversation(&mut a, &net)?;
    a.ask(
        &net.a,
        &net.network,
        "c",
        approval(&a, &net, "q-1", "r-1")?,
        2,
    )?;
    a.ask(
        &net.a,
        &net.network,
        "c",
        approval(&a, &net, "q-2", "r-2")?,
        2,
    )?;
    let signed = Some(answer_signature(&net, "c", "q-2", "继续")?);
    a.accept_answer(&net.a, &net.network, "q-2", "继续", signed, 3)?;
    sync(&a, &mut b, &net)?;
    let signed = Some(answer_signature(&net, "c", "q-1", "允许")?);
    b.accept_answer(&net.b, &net.network, "q-1", "允许", signed, 4)?;
    let written = a.close_lost_questions(&net.a, &net.network, &held(&[]), 4)?;
    assert_eq!(written.len(), 1, "an answered question is never closed");
    assert_eq!(closed(&a, &net, "q-1")?, "request_lost");
    sync(&a, &mut b, &net)?;
    sync(&b, &mut a, &net)?;
    for store in [&a, &b] {
        assert!(unanswered(store, &net)?.is_empty());
        let question = store.question(&net.network, "q-1")?.context("question")?;
        assert_eq!(
            (
                question["answer"]["text"].clone(),
                question["closed"].clone()
            ),
            (json!("允许"), Value::Null),
            "the answer stands whether it or the close arrived first"
        );
    }
    Ok(())
}

#[test]
fn the_pending_list_holds_exactly_the_unanswered_questions() -> Result<()> {
    let net = net()?;
    let mut store = store(&net)?;
    question_conversation(&mut store, &net)?;
    second_bot(&mut store, &net, "Other Questions")?;
    for (scope, id) in [("c", "q-1"), ("c", "q-2"), ("d", "q-3")] {
        store.ask(
            &net.a,
            &net.network,
            scope,
            question(&store, &net, scope, id, id)?,
            2,
        )?;
    }
    let signed = Some(answer_signature(&net, "c", "q-2", "继续")?);
    store.accept_answer(&net.a, &net.network, "q-2", "继续", signed, 3)?;
    assert_eq!(unanswered(&store, &net)?, vec!["q-1", "q-3"]);
    let pending = store.unanswered_questions(&net.network)?;
    assert!(pending.iter().all(|q| q["conversation_id"] != Value::Null));
    let chat = store.conversation_questions(&net.network, "c")?;
    assert_eq!(
        chat.iter()
            .map(|q| (q["question_id"].clone(), q["answer"]["text"].clone()))
            .collect::<Vec<_>>(),
        vec![
            (serde_json::json!("q-1"), Value::Null),
            (serde_json::json!("q-2"), serde_json::json!("继续"))
        ]
    );
    Ok(())
}
