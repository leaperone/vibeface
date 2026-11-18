use super::super::test_support::{
    all_accepted, bot_with_message, net, second_bot, session_version, store, work_spec,
};
use super::super::*;
use crate::providers::Provider;
use crate::work::{Signal, WorkSpec};
use serde_json::json;
use vibeface_bridge::failure::{ExecFailure, FailureKind};

#[test]
fn a_hand_over_from_an_old_version_writes_nothing() -> Result<()> {
    let net = net()?;
    let (mut local, _) = bot_with_message(&net)?;
    second_bot(&mut local, &net, "Other")?;
    let about = Subject {
        node: net.a.device_id()?,
        agent: Some(crate::providers::Provider::Codex),
        session_id: "s1".into(),
        cwd: "/w".into(),
        summary: "tests fail".into(),
        request_id: None,
    };
    local.claim_session(&net.a, &net.network, "c", &about, 3)?;
    let work = WorkSpec {
        work_id: "handoff-history".into(),
        cwd: "/w".into(),
        title: "handoff".into(),
        prompt: "handoff".into(),
        thread: Some("s1".into()),
        ..work_spec(
            &net.a.device_id()?,
            "message:m1",
            "handoff-history",
            Provider::Codex,
        )
    };
    local.accept_work_fixture(&net.a, &net.network, work.clone(), 4)?;
    assert!(
        local
            .native_input_target(&net.network, "d", "s1", "")
            .is_err()
    );
    let seen = session_version(&local, &net, "s1")?;
    let hand = |store: &mut Store, from: &str, seen: &str, target: Option<&str>, at: i64| {
        store.hand_over_session(&net.a, &net.network, from, "s1", seen, target, at)
    };
    let (_, moved, written) = hand(&mut local, "c", &seen, Some("d"), 4)?;
    assert!(written.is_some());
    assert_ne!(moved, seen, "a hand-over gives the session a new version");
    let (_, standing, written) = hand(&mut local, "c", &seen, Some("d"), 4)?;
    assert!(
        written.is_none() && standing == moved,
        "the same call again reads back what stands"
    );
    assert!(
        local
            .native_input_target(&net.network, "d", "s1", "")
            .is_err(),
        "an attribution does not release a Starting foreign Work"
    );
    let (_, back, written) = hand(&mut local, "d", &moved, Some("c"), 5)?;
    assert!(written.is_some(), "the other bot hands it back");
    let stale = hand(&mut local, "c", &seen, Some("d"), 6).expect_err("c's retry is from before");
    assert!(
        stale
            .to_string()
            .contains(&format!("it is at version {back} now")),
        "a retry after the session moved on writes nothing: {stale}"
    );
    assert!(
        hand(&mut local, "c", &seen, None, 6).is_err(),
        "nor does a stale mute"
    );
    assert_eq!(session_version(&local, &net, "s1")?, back);
    assert_eq!(
        local
            .session_owner(&net.network, &net.a.device_id()?, "s1")?
            .as_deref(),
        Some("c")
    );
    local.apply_work_signal(
        &net.a,
        &net.network,
        &work.work_id,
        Signal::StartRejected(ExecFailure::new(
            FailureKind::Rejected,
            "native start rejected",
        )),
        7,
    )?;
    assert!(
        local
            .native_input_target(&net.network, "d", "s1", "")
            .is_err(),
        "a terminal Work needs explicit current attribution before changing Bot"
    );
    let seen = session_version(&local, &net, "s1")?;
    local.hand_over_session(&net.a, &net.network, "c", "s1", &seen, Some("d"), 8)?;
    assert_eq!(
        local.native_input_target(&net.network, "d", "s1", "")?,
        Some(crate::workspace::InputTarget::BotMain {
            bot_id: "bot-d".into()
        })
    );
    assert!(
        local
            .native_input_target(&net.network, "c", "s1", "")
            .is_err(),
        "a completed Work does not retain the old Bot's access after handoff"
    );
    for index in 0..2 {
        local.accept_work_fixture(
            &net.a,
            &net.network,
            WorkSpec {
                work_id: format!("handoff-active-{index}"),
                call_ref: format!("handoff-active-{index}"),
                ..work.clone()
            },
            9 + index,
        )?;
    }
    assert!(
        local
            .native_input_target(&net.network, "d", "s1", "")
            .is_err(),
        "new active foreign references keep the thread closed after an earlier handoff"
    );
    Ok(())
}

#[test]
fn a_session_owner_is_written_once_and_only_its_bot_hands_it_over() -> Result<()> {
    let net = net()?;
    let (mut local, _) = bot_with_message(&net)?;
    second_bot(&mut local, &net, "Other")?;
    let node = net.a.device_id()?;
    let about = Subject {
        node: node.clone(),
        agent: Some(crate::providers::Provider::Codex),
        session_id: "s1".into(),
        cwd: "/w".into(),
        summary: "tests fail".into(),
        request_id: None,
    };
    let owner = |store: &Store| store.session_owner(&net.network, &node, "s1");
    assert!(
        local
            .claim_session(&net.a, &net.network, "c", &about, 3)?
            .is_some()
    );
    let session_notifications = local
        .pending_notifications(&net.network, &net.b.device_id()?)?
        .into_iter()
        .filter(|notification| notification.kind == "session_attributed")
        .collect::<Vec<_>>();
    assert_eq!(session_notifications.len(), 1);
    assert_eq!(session_notifications[0].text, "tests fail");
    assert!(
        local
            .claim_session(&net.a, &net.network, "d", &about, 4)?
            .is_none(),
        "an owned session is claimed once"
    );
    assert!(
        local
            .claim_session(&net.b, &net.network, "c", &about, 4)
            .is_err()
    );
    assert_eq!(owner(&local)?.as_deref(), Some("c"));
    let mut hand = |from: &str, session: &str, target: Option<&str>, at: i64| {
        let seen = session_version(&local, &net, session)?;
        local
            .hand_over_session(&net.a, &net.network, from, session, &seen, target, at)
            .map(|(owner, _, event)| (owner, event))
    };
    assert!(hand("c", "s1", Some("nope"), 5).is_err());
    assert!(hand("c", "s2", None, 5).is_err());
    let theirs = hand("d", "s1", Some("d"), 5).expect_err("d does not follow s1");
    assert!(
        theirs.to_string().contains("Helper follows it"),
        "only the bot that follows a session hands it over: {theirs}"
    );

    let (before, moved) = hand("c", "s1", Some("d"), 5)?;
    assert_eq!((before.as_str(), moved.is_some()), ("c", true));
    let again = hand("c", "s1", Some("d"), 6)?;
    assert!(again.1.is_none(), "a repeated hand-over writes nothing");
    assert!(
        hand("c", "s1", None, 6).is_err(),
        "a session handed over is no longer the sender's"
    );
    assert_eq!(owner(&local)?.as_deref(), Some("d"));
    assert!(local.attributed_sessions(&net.network, "c")?.is_empty());

    let mut hand = |from: &str, session: &str, target: Option<&str>, at: i64| {
        let seen = session_version(&local, &net, session)?;
        local
            .hand_over_session(&net.a, &net.network, from, session, &seen, target, at)
            .map(|(owner, _, event)| (owner, event))
    };
    assert!(hand("d", "s1", None, 7)?.1.is_some());
    assert!(hand("d", "s1", None, 8)?.1.is_none());
    assert!(
        hand("c", "s1", Some("c"), 8).is_err(),
        "a muted session stays with the bot that stopped following it"
    );
    assert!(local.session_muted(&net.network, &node, "s1")?);
    assert!(
        local
            .claim_session(&net.a, &net.network, "c", &about, 9)?
            .is_none(),
        "a muted session is not claimed again"
    );
    let rows = local.attributed_sessions(&net.network, "d")?;
    assert_eq!(rows.len(), 1);
    assert_eq!(
        (
            rows[0]["muted"].clone(),
            rows[0]["summary"].clone(),
            rows[0]["agent"].clone()
        ),
        (json!(true), json!("tests fail"), json!("codex"))
    );

    let mut other = store(&net)?;
    all_accepted(&mut other, &net, &local)?;
    assert_eq!(owner(&other)?.as_deref(), Some("d"));
    assert!(other.session_muted(&net.network, &node, "s1")?);
    other.connection.execute(
        "UPDATE events SET effective=0 WHERE network_id=?",
        [&net.network],
    )?;
    reduce::clear(&other.connection, &net.network)?;
    log::recompute(
        &other.connection,
        &membership_from(&other.connection, &net.network)?,
    )?;
    assert_eq!(
        owner(&other)?.as_deref(),
        Some("d"),
        "a rebuild reads the same"
    );
    assert!(other.session_muted(&net.network, &node, "s1")?);
    assert!(
        other
            .pending_notifications(&net.network, &net.b.device_id()?)?
            .into_iter()
            .all(|notification| notification.kind != "session_attributed"),
        "a projection rebuild does not backfill old session reminders"
    );

    let seen = session_version(&local, &net, "s1")?;
    local.hand_over_session(&net.a, &net.network, "d", "s1", &seen, Some("c"), 10)?;
    assert_eq!(owner(&local)?.as_deref(), Some("c"));
    assert!(
        !local.session_muted(&net.network, &node, "s1")?,
        "handing a muted session to a bot follows it again"
    );
    Ok(())
}
