use super::*;

fn projection(network: &str, attempt: &str) -> ContextProjection {
    ContextProjection {
        network_id: network.into(),
        scope: SessionScope::Main,
        attempt_id: attempt.into(),
        provider: Provider::Codex,
        stable_fingerprint: "stable".into(),
        dynamic_fingerprint: "dynamic".into(),
        session_generation: 1,
        coverage: Default::default(),
        stable_bytes: 1,
        dynamic_bytes: 2,
        invalidation_reason: None,
        stage: ContextStage::Started,
        session_status: None,
        sources: vec![],
        sources_omitted: 0,
        inputs: vec![],
        inputs_omitted: 0,
    }
}
fn add_job(store: &Store, index: usize, attempt: &str, source: bool, network: &str) -> Result<()> {
    let reply = format!("input-{index}");
    let into = SteerTarget {
        provider: Provider::Codex,
        attempt_id: attempt.into(),
        thread_id: "thread".into(),
        turn_id: format!("turn-{index}"),
    };
    let receipt = Some(SteerReceipt {
        accepted_at_ms: index as i64,
        actual_turn_id: Some(format!("actual-{index}")),
    });
    let state = match index % 5 {
        0 => JobState::Steering {
            into,
            receipt: None,
            uncertain: false,
            unread_since_ms: None,
            running_since_ms: None,
        },
        1 => JobState::Steering {
            into,
            receipt: None,
            uncertain: true,
            unread_since_ms: None,
            running_since_ms: None,
        },
        2 => JobState::Steering {
            into,
            receipt,
            uncertain: false,
            unread_since_ms: None,
            running_since_ms: None,
        },
        3 => JobState::Steered {
            into,
            receipt,
            outcome: SteerOutcome::Answered,
        },
        _ => JobState::Steered {
            into,
            receipt: None,
            outcome: SteerOutcome::Unknown,
        },
    };
    let job = SchedulerJob {
        conversation_id: "c".into(),
        scope: SessionScope::Main,
        trigger: format!("message:{reply}"),
        kind: TriggerKind::Human,
        sequence: index as u64,
        basis: VersionVector::new(),
        reply_to: reply.clone(),
        message_id: format!("reply-{index}"),
        runtime: None,
        pending_check: false,
        tool_calls: false,
        state,
    };
    save_job(&store.connection, &job)?;
    if source {
        store.connection.execute("INSERT INTO messages(network_id,conversation_id,message_id,event_hash,rank,author,role,text,created_at_ms,attachments)
        VALUES(?,'c',?,?,?,'synthetic','user','synthetic',0,'[]')",params![network,reply,format!("source-{index}"),format!("rank-{index}")])?;
    }
    Ok(())
}
fn old_inputs(store: &Store, network: &str, attempt: &str) -> Result<(Vec<Value>, usize)> {
    let mut values = Vec::new();
    let mut missing = 0;
    for job in query_jobs(
        &store.connection,
        "conversation_id=? AND state IN ('steering','steered')",
        ["c"],
    )? {
        let (into, receipt, stage) = match &job.state {
            JobState::Steering {
                into,
                receipt,
                uncertain,
                ..
            } => (
                into,
                receipt,
                if receipt.is_some() {
                    "provider_accepted"
                } else if *uncertain {
                    "unknown"
                } else {
                    "submitting"
                },
            ),
            JobState::Steered {
                into,
                receipt,
                outcome,
            } => (
                into,
                receipt,
                if *outcome == SteerOutcome::Unknown
                    || receipt
                        .as_ref()
                        .and_then(|receipt| receipt.actual_turn_id.as_ref())
                        .is_none()
                {
                    "unknown"
                } else {
                    "associated"
                },
            ),
            _ => continue,
        };
        if into.attempt_id != attempt {
            continue;
        }
        let Some(source) = message_from(&store.connection, network, "c", &job.reply_to)?
            .map(|message| message.event_hash)
        else {
            missing += 1;
            continue;
        };
        let mut value = json!({"message_id":job.reply_to,"source_event":source,"stage":stage,"expected_turn_id":into.turn_id,
            "actual_turn_id":receipt.as_ref().and_then(|receipt|receipt.actual_turn_id.clone())});
        if let Some(receipt) = receipt {
            value["accepted_at_ms"] = json!(receipt.accepted_at_ms);
        }
        values.push(value);
    }
    let omitted = values.len().saturating_sub(64) + missing;
    values.truncate(64);
    Ok((values, omitted))
}

#[test]
fn stored_context_caps_valid_sources_and_exactly_counts_missing_and_omitted_rows() -> Result<()> {
    let store = Store::open(":memory:")?;
    let mut session = SchedulerSession::in_scope("c", SessionScope::Main);
    session.shown.context = Some(projection("n", "current"));
    for index in 0..96 {
        add_job(&store, index, "current", index % 7 != 0, "n")?;
    }
    for index in 96..1096 {
        add_job(&store, index, "other-attempt", true, "n")?;
    }
    add_job(&store, 1096, "current", true, "wrong-network")?;
    let context =
        context_projection_from(&store.connection, &session)?.context("stored context")?;
    let old = old_inputs(&store, "n", "current")?;
    assert_eq!((context.inputs.clone(), context.inputs_omitted), old);
    assert_eq!(context.inputs.len(), 64);
    assert_eq!(context.inputs_omitted, 33);
    assert_eq!(context.inputs[0]["message_id"], "input-1");
    assert_eq!(context.inputs[0]["stage"], "unknown");
    assert_eq!(context.inputs[1]["stage"], "provider_accepted");
    assert_eq!(context.inputs[1]["accepted_at_ms"], 2);
    assert_eq!(context.inputs[2]["stage"], "associated");
    let unchanged = store
        .connection
        .query_row("SELECT total_changes()", [], |row| row.get::<_, u64>(0))?;
    let mut other_network = session.clone();
    other_network.shown.context = Some(projection("missing-network", "current"));
    let missing = context_projection_from(&store.connection, &other_network)?.context("context")?;
    assert!(missing.inputs.is_empty());
    assert_eq!(missing.inputs_omitted, 97);
    assert_eq!(
        store
            .connection
            .query_row("SELECT total_changes()", [], |row| row.get::<_, u64>(0))?,
        unchanged
    );
    let plan=store.connection.prepare("EXPLAIN QUERY PLAN SELECT j.reply_to,j.detail,m.event_hash FROM scheduler_jobs j JOIN messages m ON m.network_id='n' AND m.conversation_id=j.conversation_id AND m.message_id=j.reply_to WHERE j.conversation_id='c' AND j.state IN ('steering','steered') AND json_extract(j.detail,'$.into.attempt_id')='current' ORDER BY j.sequence,j.rowid LIMIT 64")?
        .query_map([],|row|row.get::<_,String>(3))?.collect::<rusqlite::Result<Vec<_>>>()?.join("\n");
    assert!(plan.contains("scheduler_jobs_context"), "{plan}");
    assert!(!plan.contains("TEMP B-TREE"), "{plan}");
    Ok(())
}

#[test]
fn stored_context_empty_and_short_attempts_keep_literal_receipt_stages() -> Result<()> {
    let store = Store::open(":memory:")?;
    let mut session = SchedulerSession::in_scope("c", SessionScope::Main);
    assert!(context_projection_from(&store.connection, &session)?.is_none());
    session.shown.context = Some(projection("n", "current"));
    let empty = context_projection_from(&store.connection, &session)?.context("empty")?;
    assert!(empty.inputs.is_empty());
    assert_eq!(empty.inputs_omitted, 0);
    for index in 0..5 {
        add_job(&store, index, "current", true, "n")?;
    }
    let short = context_projection_from(&store.connection, &session)?.context("short")?;
    assert_eq!(
        short
            .inputs
            .iter()
            .map(|input| input["stage"].as_str().unwrap_or_default())
            .collect::<Vec<_>>(),
        vec![
            "submitting",
            "unknown",
            "provider_accepted",
            "associated",
            "unknown"
        ]
    );
    assert_eq!(short.inputs_omitted, 0);
    assert_eq!(
        (short.inputs, short.inputs_omitted),
        old_inputs(&store, "n", "current")?
    );
    Ok(())
}
