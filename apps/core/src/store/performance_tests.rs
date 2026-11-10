//! Synthetic workloads only. Fixture signing is deliberately outside each timed sample.
use super::*;
use crate::work::{StopRequest, WorkRecord, RECONCILE_GRACE_MS};
use std::time::Instant;

fn plan(store: &Store, sql: &str) -> Result<String> {
    Ok(store.connection.prepare(&format!("EXPLAIN QUERY PLAN {sql}"))?
        .query_map([], |row| row.get::<_, String>(3))?
        .collect::<rusqlite::Result<Vec<_>>>()?.join("\n"))
}

fn shared_state(store: &Store, network: &str) -> Result<Vec<String>> {
    let mut result = dump(store, network)?;
    for table in ["events", "log_heads", "pending_events"] {
        let mut statement = store.connection.prepare(&format!(
            "SELECT * FROM {table} WHERE network_id=?"
        ))?;
        let columns = statement.column_count();
        let mut rows = statement.query_map([network], |row| {
            (0..columns).map(|index| row.get::<_, rusqlite::types::Value>(index))
                .collect::<rusqlite::Result<Vec<_>>>()
        })?.map(|row| Ok(format!("{table}: {:?}", row?)))
            .collect::<Result<Vec<_>>>()?;
        rows.sort();
        result.extend(rows);
    }
    Ok(result)
}

/// Rebuild every shared projection, even when the effective flags would not change.
pub(super) fn assert_full_recompute_oracle(store: &Store, network: &str) -> Result<()> {
    let before = shared_state(store, network)?;
    store.connection.execute("UPDATE events SET effective=0 WHERE network_id=?", [network])?;
    reduce::clear(&store.connection, network)?;
    log::recompute(&store.connection, &membership_from(&store.connection, network)?)?;
    assert_eq!(shared_state(store, network)?, before);
    Ok(())
}

fn sign_after(net: &Net, author: &LocalIdentity, previous: &SignedEvent, body: EventBody, deps: Vec<String>) -> Result<SignedEvent> {
    let previous_event: Event = serde_json::from_str(&previous.canonical_body)?;
    author.sign_event(Event {
        version: WIRE_VERSION, network_id: net.network.clone(), author: author.device_id()?,
        seq: previous_event.seq + 1, prev: Some(previous.hash.clone()), deps,
        scope_id: "c".into(), created_at_ms: previous_event.created_at_ms + 1, body, blobs: vec![],
    })
}

#[test]
fn stop_lookup_is_exact_and_hold_preserves_the_first_request() -> Result<()> {
    let net = net()?;
    let mut store = store(&net)?;
    for index in 0..1000 {
        assert!(store.hold_stop_request(&StopRequest {
            work_id: format!("noise-{index}"), conversation: "elsewhere".into(), target: "remote".into(),
            requested_at_ms: 1, deadline_ms: 2, timed_out: false,
        })?);
    }
    let first = StopRequest { work_id: "target".into(), conversation: "c".into(), target: "local".into(),
        requested_at_ms: 17, deadline_ms: 31, timed_out: false };
    assert!(store.hold_stop_request(&first)?);
    let mut repeated = first.clone();
    repeated.conversation = "wrong-scope".into(); repeated.target = "other".into();
    repeated.requested_at_ms = 99; repeated.deadline_ms = 999; repeated.timed_out = true;
    assert!(!store.hold_stop_request(&repeated)?);
    let held = store.stop_request("target")?.context("held stop")?;
    assert_eq!((held.work_id.as_str(), held.conversation.as_str(), held.target.as_str(), held.requested_at_ms, held.deadline_ms, held.timed_out),
        ("target", "c", "local", 17, 31, false));
    assert!(store.stop_request("absent")?.is_none());
    assert_eq!(store.stop_requests_for_target("local")?.len(), 1);
    repeated.deadline_ms = 37;
    store.save_stop_request(&repeated)?;
    assert_eq!(store.stop_request("target")?.context("updated stop")?.deadline_ms, 37);
    let details = plan(&store, "SELECT * FROM work_stops WHERE work_id='target'")?;
    assert!(details.contains("SEARCH work_stops USING INDEX sqlite_autoindex_work_stops_1"), "{details}");
    let details = plan(&store, "SELECT * FROM works WHERE network_id='n' AND work_id='target'")?;
    assert!(details.contains("SEARCH works USING INDEX sqlite_autoindex_works_1"), "{details}");
    Ok(())
}

fn insert_run(store: &Store, id: &str, kind: &str, state: &str, send_by: i64, probe: Option<bool>) -> Result<()> {
    store.connection.execute("INSERT INTO work_runs(work_id,conversation_id,kind,state,thread_id,turn_id,codex_epoch,probe,result_text,send_by_ms,progress,progress_at_ms,outcome)
        VALUES(?,'c',?,?,'thread','turn',1,?,'',?,'',0,?)", params![id, kind, state, probe, send_by,
            (state == "finished").then_some("completed")])?;
    Ok(())
}

#[test]
fn eligible_journal_preserves_the_exact_grace_boundary_and_probe_rules() -> Result<()> {
    let net = net()?;
    let store = store(&net)?;
    for index in 0..1000 { insert_run(&store, &format!("terminal-{index}"), "codex", "finished", 0, None)?; }
    for (id, state, send_by, probe) in [("due", "accepted", 100, None), ("starting", "starting", 100, None),
        ("future", "accepted", 101, None), ("running", "running", 0, None),
        ("probe", "unverified", 0, Some(true)), ("no-probe", "unverified", 0, Some(false)),
        ("interrupting", "interrupting", 0, None)] {
        insert_run(&store, id, "codex", state, send_by, probe)?;
    }
    insert_run(&store, "task", "task", "accepted", 0, None)?;
    let ids = |records: Vec<WorkRecord>| records.into_iter().map(|record| record.work_id).collect::<Vec<_>>();
    assert_eq!(ids(store.reconcile_work_records(100 + RECONCILE_GRACE_MS, false)?), vec!["due", "starting"]);
    assert_eq!(ids(store.reconcile_work_records(100 + RECONCILE_GRACE_MS, true)?), vec!["due", "probe", "running", "starting"]);
    assert!(store.reconcile_work_records(99 + RECONCILE_GRACE_MS, false)?.is_empty());
    let details = plan(&store, "SELECT work_id FROM work_runs WHERE kind<>'task' AND state IN ('accepted','starting') AND send_by_ms<=100")?;
    assert!(details.contains("SEARCH work_runs USING INDEX work_runs_reconcile"), "{details}");
    assert!(!details.contains("SCAN work_runs"), "{details}");
    Ok(())
}

#[test]
fn assistant_fork_tail_skips_recompute_but_first_and_earlier_forks_do_not() -> Result<()> {
    let net = net()?;
    let mut origin = store(&net)?;
    let details = plan(&origin, "SELECT min(seq) FROM events WHERE network_id='n' AND author='a' AND main=0")?;
    assert!(details.contains("events_forks"), "{details}");
    let base = created(&mut origin, &net, &net.a)?;
    let tip = base.last().context("prelude")?;
    let main = sign_after(&net, &net.a, tip, bot_reply("main", "seed"), vec![])?;
    let fork = sign_after(&net, &net.a, tip, bot_reply("fork", "seed"), vec![])?;
    let tail = sign_after(&net, &net.a, &main, bot_reply("tail", "seed"), vec![])?;
    let future = sign_after(&net, &net.a, &tail, bot_reply("future", "seed"), vec![])?;
    for order in [[0, 1, 2, 3], [1, 0, 3, 2], [3, 2, 0, 1]] {
        let mut replica = store(&net)?;
        replica.ingest_events_at(&net.network, &base, 50)?;
        for index in order {
            let event = [&main, &fork, &tail, &future][index];
            replica.ingest_events_at(&net.network, std::slice::from_ref(event), 50)?;
            assert_full_recompute_oracle(&replica, &net.network)?;
        }
    }
    let mut replica = store(&net)?;
    replica.ingest_events_at(&net.network, &base, 50)?;
    replica.ingest_events_at(&net.network, std::slice::from_ref(&main), 50)?;
    log::test_metrics::reset();
    replica.ingest_events_at(&net.network, std::slice::from_ref(&fork), 50)?;
    assert_eq!(log::test_metrics::snapshot().recomputes, 1);
    let mut batch_replica = store(&net)?;
    batch_replica.ingest_events_at(&net.network, &base, 50)?;
    batch_replica.ingest_events_at(&net.network, std::slice::from_ref(&main), 50)?;
    log::test_metrics::reset();
    batch_replica.ingest_events_at(&net.network, &[fork.clone(), tail.clone(), future.clone()], 50)?;
    assert_eq!(log::test_metrics::snapshot().recomputes, 1, "a fast tail must not clear another row's stale flag");
    assert_full_recompute_oracle(&batch_replica, &net.network)?;
    log::test_metrics::reset();
    replica.ingest_events_at(&net.network, &[tail, future], 50)?;
    let metrics = log::test_metrics::snapshot();
    assert_eq!(metrics.recomputes, 0);
    assert_eq!(metrics.assistant_tail_skips, 2);
    assert_full_recompute_oracle(&replica, &net.network)?;
    // A late earlier branch is deliberately an assistant too; it must move the fork bound.
    let previous = &base[base.len()-2];
    let earlier = sign_after(&net, &net.a, previous, bot_reply("earlier", "seed"), vec![])?;
    log::test_metrics::reset();
    replica.ingest_events_at(&net.network, &[earlier], 50)?;
    assert_eq!(log::test_metrics::snapshot().recomputes, 1);
    assert_full_recompute_oracle(&replica, &net.network)?;
    Ok(())
}

#[test]
fn input_projection_omits_basis_walks_in_arrival_order_and_full_rebuild() -> Result<()> {
    let net = net()?;
    let mut origin = store(&net)?;
    let mut events = created(&mut origin, &net, &net.a)?;
    let mut previous = events.last().context("prelude")?.clone();
    for index in 0..6 {
        let event = sign_after(&net, &net.a, &previous, signed_user(&net, &format!("input-{index}"), "synthetic", false)?, vec![])?;
        previous = event.clone(); events.push(event);
    }
    for legacy in [true, false] {
        for reversed in [false, true] {
            let mut replica = store(&net)?;
            let base_len = events.len() - 6;
            replica.ingest_events_at(&net.network, &events[..base_len], 50)?;
            let mut inputs = events[base_len..].to_vec();
            if reversed { inputs.reverse(); }
            workspace::reset_projection_ancestor_walks();
            log::test_metrics::with_legacy(legacy, || -> Result<()> {
                replica.ingest_events_at(&net.network, &inputs, 50)?;
                if reversed { replica.ingest_events_at(&net.network, &events[base_len..], 50)?; }
                Ok(())
            })?;
            if legacy { assert!(workspace::projection_ancestor_walks() >= 6); }
            else { assert_eq!(workspace::projection_ancestor_walks(), 0); }
            assert!(workspace::authorization_ancestor_walks() >= 6);
            workspace::reset_projection_ancestor_walks();
            assert_full_recompute_oracle(&replica, &net.network)?;
            assert_eq!(workspace::projection_ancestor_walks(), 0);
            let mut reference = store(&net)?;
            log::test_metrics::with_legacy(true, || reference.ingest_events_at(&net.network, &events, 50))?;
            assert_eq!(shared_state(&replica, &net.network)?, shared_state(&reference, &net.network)?);
        }
    }
    Ok(())
}

fn percentile(values: &[u128], percentile: usize) -> u128 {
    if values.is_empty() { return 0; }
    let mut values = values.to_vec(); values.sort_unstable();
    values[((values.len()-1)*percentile)/100]
}
fn report(label: &str, values: &[u128]) {
    eprintln!("{label} samples={} ns p50={} p95={} p99={} max={}", values.len(),
        percentile(values, 50), percentile(values, 95), percentile(values, 99), values.iter().max().unwrap_or(&0));
}

/// Run explicitly with `cargo test -p vibeface-core store::tests::performance_tests::signed_history_scale -- --ignored --nocapture`.
/// VIBEFACE_PERF_SIZES and VIBEFACE_PERF_KINDS may narrow an exploratory run; defaults cover the full matrix.
#[test]
#[ignore = "signed synthetic 1k/10k/100k baseline and optimized measurements"]
fn signed_history_scale() -> Result<()> {
    let sizes = std::env::var("VIBEFACE_PERF_SIZES").unwrap_or_else(|_| "1000,10000,100000".into());
    let kinds = std::env::var("VIBEFACE_PERF_KINDS").unwrap_or_else(|_| "assistant,input,fork,multi".into());
    for size in sizes.split(',').map(str::parse::<usize>) {
        let size = size?;
        for kind in kinds.split(',') {
            ensure!(["assistant", "input", "fork", "multi"].contains(&kind), "unknown synthetic workload");
            let net = net()?;
            let mut origin = store(&net)?;
            let base = created(&mut origin, &net, &net.a)?;
            let mut previous = base.last().context("prelude")?.clone();
            let mut fixtures: Vec<SignedEvent> = Vec::with_capacity(size);
            for index in 0..size {
                let body = if kind == "input" || kind == "multi" {
                    signed_user(&net, &format!("input-{index}"), "synthetic input", false)?
                } else { bot_reply(&format!("assistant-{index}"), "seed") };
                let mut event = sign_after(&net, &net.a, &previous, body, vec![])?;
                if kind == "multi" && index % 2 == 1 {
                    // A second author's independent chain depends on the other author's latest input.
                    let prev_b = fixtures.iter().rev().find(|event: &&SignedEvent| {
                        serde_json::from_str::<Event>(&event.canonical_body).is_ok_and(|event| event.author == net.b.device_id().unwrap_or_default())
                    });
                    let dep = previous.hash.clone();
                    let mut body: Event = serde_json::from_str(&event.canonical_body)?;
                    body.author = net.b.device_id()?;
                    body.seq = prev_b.map(|event| seq_of(event)).transpose()?.unwrap_or(0)+1;
                    body.prev = prev_b.map(|event| event.hash.clone()); body.deps = vec![dep];
                    event = net.b.sign_event(body)?;
                } else { previous = event.clone(); }
                fixtures.push(event);
            }
            let fork = if kind == "fork" {
                Some(sign_after(&net, &net.a, base.last().context("prelude")?, bot_reply("conflict", "seed"), vec![])?)
            } else { None };
            for page in [64, 256, 1024] {
                for legacy in [true, false] {
                    for repeat in 0..3 {
                        let mut replica = store(&net)?;
                        replica.ingest_events_at(&net.network, &base, 50)?;
                        let mut offset = 0;
                        if let Some(fork) = &fork {
                            replica.ingest_events_at(&net.network, &fixtures[..1], 50)?;
                            replica.ingest_events_at(&net.network, std::slice::from_ref(fork), 50)?;
                            offset = 1;
                        }
                        log::test_metrics::reset(); workspace::reset_projection_ancestor_walks();
                        let mut pages = Vec::new();
                        let started = Instant::now();
                        log::test_metrics::with_legacy(legacy, || -> Result<()> {
                            for batch in fixtures[offset..].chunks(page) {
                                let start = Instant::now();
                                let ingested = replica.ingest_events_at(&net.network, batch, 50)?;
                                ensure!(ingested.statuses.iter().all(|status| matches!(status, IngestStatus::Accepted | IngestStatus::Conflict)), "synthetic fixture rejected");
                                pages.push(start.elapsed().as_nanos());
                            }
                            Ok(())
                        })?;
                        let elapsed = started.elapsed();
                        let metrics = log::test_metrics::snapshot();
                        eprintln!("workload={kind} size={size} page={page} legacy={legacy} repeat={repeat} elapsed_ns={} events_per_second={} recomputes={} rebuilds={} projection_ancestor_walks={}", elapsed.as_nanos(), (size-offset) as f64 / elapsed.as_secs_f64(), metrics.recomputes, metrics.rebuilds, workspace::projection_ancestor_walks());
                        #[cfg(unix)]
                        {
                            let mut usage = std::mem::MaybeUninit::<libc::rusage>::uninit();
                            // getrusage writes the complete output structure when it succeeds.
                            if unsafe { libc::getrusage(libc::RUSAGE_SELF, usage.as_mut_ptr()) } == 0 {
                                let usage = unsafe { usage.assume_init() };
                                let bytes = if cfg!(target_os = "macos") { usage.ru_maxrss as u64 } else { usage.ru_maxrss as u64 * 1024 };
                                eprintln!("process_peak_resident_bytes={bytes}");
                            }
                        }
                        report("page", &pages); report("ingest_including_validation", &metrics.ingest_ns); report("validation", &metrics.validation_ns);
                        report("recompute", &metrics.recompute_ns); report("projection_rebuild", &metrics.rebuild_ns);
                        assert_full_recompute_oracle(&replica, &net.network)?;
                    }
                }
            }
        }
    }
    Ok(())
}

#[test]
fn scoped_lists_match_the_complete_previous_output_including_accepted_dispatches() -> Result<()> {
    let net = net()?;
    let mut store = store(&net)?;
    for (index, scope, state) in [(0, "c", "running"), (1, "elsewhere", "queued"),
        (2, "c", "completed"), (3, "c", "queued"), (4, "c", "cancelled")] {
        store.connection.execute("INSERT INTO scheduler_jobs(conversation_id,trigger,kind,sequence,basis,reply_to,message_id,state,attempt_id,attempt_boot,attempt_permission,tool_calls)
            VALUES(?,?,'human',1,'{}','seed',?,?,?,'boot',1,0)",
            params![scope, format!("trigger-{index}"), format!("message-{index}"), state, format!("attempt-{index}")])?;
    }
    let scopes = vec!["c".to_string()];
    for include_finished in [false, true] {
        let old: Vec<_> = store.scheduler_jobs_all()?.into_iter().filter(|job|
            scopes.contains(&job.conversation_id) && (include_finished || !job.state.terminal())).collect();
        let new = store.scheduler_jobs_for_scopes(&scopes, include_finished)?;
        assert_eq!(serde_json::to_value(new)?, serde_json::to_value(old)?);
    }
    assert!(store.scheduler_jobs_for_scopes(&[], true)?.is_empty());
    let details = plan(&store, "SELECT * FROM scheduler_jobs WHERE state IN ('queued','steering','claimed','running','cancelling','reconciling') AND conversation_id IN (SELECT value FROM json_each('[\"c\"]'))")?;
    assert!(details.contains("scheduler_jobs_state"), "{details}");
    for (index, state, scope) in [(0, "accepted", "c"), (1, "pending", "c"), (2, "accepted", "elsewhere")] {
        let mut spec = work_spec(&net.a.device_id()?, "trigger", &format!("call-{index}"), Provider::Codex);
        spec.conversation = scope.into();
        store.connection.execute("INSERT INTO work_dispatches(work_id,spec,state) VALUES(?,?,?)",
            params![spec.work_id, serde_json::to_string(&spec)?, state])?;
    }
    let old: Vec<_> = store.dispatches()?.into_iter().filter(|dispatch| dispatch.spec.conversation == "c").collect();
    assert_eq!(serde_json::to_value(store.dispatches_for_scopes(&scopes)?)?, serde_json::to_value(&old)?);
    store.connection.execute("INSERT INTO works(network_id,work_id,conversation_id) VALUES(?,?,'c')",
        params![net.network, old[0].spec.work_id])?;
    assert_eq!(store.undelivered_dispatches(&net.network, "c")?.len(), 2);
    assert!(store.undelivered_dispatches(&net.network, "c")?.iter().any(|dispatch| dispatch.state == crate::work::DispatchState::Accepted));
    assert_eq!(store.pending_dispatches()?.len(), 1);
    Ok(())
}

#[test]
fn settlement_only_transitions_matching_local_states_with_shared_final_outcomes() -> Result<()> {
    let net = net()?;
    let mut store = store(&net)?;
    for (id, state, outcome, dispatched, network) in [
        ("accepted", "accepted", Some(WorkOutcome::Completed), true, net.network.as_str()),
        ("starting", "starting", Some(WorkOutcome::Rejected), true, net.network.as_str()),
        ("unverified", "unverified", Some(WorkOutcome::Interrupted), true, net.network.as_str()),
        ("running", "running", Some(WorkOutcome::Completed), true, net.network.as_str()),
        ("unknown", "accepted", Some(WorkOutcome::Unverified), true, net.network.as_str()),
        ("no-result", "accepted", None, true, net.network.as_str()),
        ("no-dispatch", "accepted", Some(WorkOutcome::Completed), false, net.network.as_str()),
        ("foreign", "accepted", Some(WorkOutcome::Completed), true, "other-network"),
    ] {
        insert_run(&store, id, "codex", state, 100, Some(false))?;
        let mut spec = work_spec(&net.a.device_id()?, "trigger", id, Provider::Codex);
        spec.work_id = id.into();
        let (_, dispatch) = crate::work::accept(spec, 1);
        let finish = outcome.map(|outcome| EventBody::WorkFinished { work_id: id.into(), outcome,
            result_text: "shared result".into(), failure_reason: None, attestation: None, failure: None });
        store.connection.execute("INSERT INTO works(network_id,work_id,conversation_id,target_node,dispatch,finish) VALUES(?,?,'c',?,?,?)",
            params![network, id, net.a.device_id()?, dispatched.then(|| serde_json::to_string(&dispatch)).transpose()?,
                finish.map(|finish| serde_json::to_string(&finish)).transpose()?])?;
    }
    store.settle_local_works(&net.network)?;
    for (id, expected) in [("accepted", "finished"), ("starting", "finished"), ("unverified", "finished"),
        ("running", "running"), ("unknown", "accepted"), ("no-result", "accepted"), ("no-dispatch", "accepted"), ("foreign", "accepted")] {
        assert_eq!(store.work_record(id)?.context("local run")?.state.name(), expected, "{id}");
    }
    assert!(matches!(store.work_record("accepted")?.context("settled")?.state,
        crate::work::WorkState::Finished { outcome: WorkOutcome::Completed }));
    let details = plan(&store, "SELECT r.work_id FROM work_runs r CROSS JOIN works w WHERE w.network_id='n' AND w.work_id=r.work_id AND r.state IN ('unverified','accepted','starting') AND w.dispatch IS NOT NULL AND w.finish IS NOT NULL AND json_extract(w.finish,'$.outcome')<>'unverified'")?;
    assert!(details.contains("work_runs_reconcile"), "{details}");
    assert!(details.contains("sqlite_autoindex_works_1"), "{details}");
    Ok(())
}

#[test]
fn pending_cascade_preserves_the_assistant_tail_fast_path() -> Result<()> {
    let net = net()?;
    let mut origin = store(&net)?;
    let base = created(&mut origin, &net, &net.a)?;
    let previous = base.last().context("prelude")?;
    let main = sign_after(&net, &net.a, previous, bot_reply("main", "seed"), vec![])?;
    let fork = sign_after(&net, &net.a, previous, bot_reply("fork", "seed"), vec![])?;
    let dependency = net.b.sign_event(Event { version: WIRE_VERSION, network_id: net.network.clone(), author: net.b.device_id()?,
        seq: 1, prev: None, deps: vec![previous.hash.clone()], scope_id: "c".into(), created_at_ms: 10,
        body: signed_user(&net, "dependency", "synthetic", false)?, blobs: vec![] })?;
    let tail = sign_after(&net, &net.a, &main, bot_reply("tail", "dependency"), vec![dependency.hash.clone()])?;
    let future = sign_after(&net, &net.a, &tail, bot_reply("future", "dependency"), vec![])?;
    let mut replica = store(&net)?;
    replica.ingest_events_at(&net.network, &base, 50)?;
    replica.ingest_events_at(&net.network, &[main, fork], 50)?;
    assert_eq!(replica.ingest_events_at(&net.network, &[tail, future], 50)?.statuses,
        vec![IngestStatus::Pending, IngestStatus::Pending]);
    assert_full_recompute_oracle(&replica, &net.network)?;
    log::test_metrics::reset();
    replica.ingest_events_at(&net.network, &[dependency], 50)?;
    assert_eq!(log::test_metrics::snapshot().assistant_tail_skips, 2);
    assert_eq!(log::test_metrics::snapshot().recomputes, 0);
    let pending: u64 = replica.connection.query_row("SELECT count(*) FROM pending_events", [], |row| row.get(0))?;
    assert_eq!(pending, 0);
    assert_full_recompute_oracle(&replica, &net.network)?;
    Ok(())
}

#[test]
fn cutoff_hash_arriving_after_governance_still_recomputes_and_selects_its_branch() -> Result<()> {
    let fleet = fleet()?;
    let net = &fleet.net;
    let mut origin = store(net)?;
    let base = created(&mut origin, net, &fleet.x)?;
    let tip = base.last().context("prelude")?;
    let main = sign_after(net, &fleet.x, tip, bot_reply("main", "seed"), vec![])?;
    let fork = sign_after(net, &fleet.x, tip, bot_reply("fork", "seed"), vec![])?;
    let cutoff = sign_after(net, &fleet.x, &fork, bot_reply("cutoff", "seed"), vec![])?;
    let governance = revoke(&fleet, &net.state, &fleet.x, Some(&cutoff))?;
    let pieces = [Piece::Events(base), Piece::Events(vec![main.clone(), fork.clone()]),
        Piece::Events(vec![cutoff.clone()]), Piece::Governance(governance.clone())];
    converges(net, &pieces)?;
    let mut replica = store(net)?;
    if let Piece::Events(base) = &pieces[0] { replica.ingest_events_at(&net.network, base, 50)?; }
    replica.ingest_events_at(&net.network, &[main.clone(), fork.clone()], 50)?;
    log::test_metrics::reset();
    replica.install_trust_bundle(&net.network, &governance)?;
    assert!(log::test_metrics::snapshot().recomputes >= 1);
    log::test_metrics::reset();
    replica.ingest_events_at(&net.network, std::slice::from_ref(&cutoff), 50)?;
    assert_eq!(log::test_metrics::snapshot().recomputes, 1);
    assert_eq!(log::test_metrics::snapshot().assistant_tail_skips, 0);
    assert_eq!(effective(&replica, net, &[&main, &fork, &cutoff])?, vec![false, true, true]);
    assert_full_recompute_oracle(&replica, &net.network)?;
    Ok(())
}

#[test]
#[ignore = "controlled SQL journal noise, separate from signed ingest measurements"]
fn local_journal_scale() -> Result<()> {
    for size in [1000, 10000, 100000] {
        let net = net()?;
        let mut store = store(&net)?;
        store.connection.execute_batch("BEGIN")?;
        for index in 0..size {
            insert_run(&store, &format!("terminal-{index}"), "codex", "finished", 0, None)?;
            store.hold_stop_request(&StopRequest { work_id: format!("stop-{index}"), conversation: "elsewhere".into(),
                target: "remote".into(), requested_at_ms: 1, deadline_ms: 2, timed_out: false })?;
        }
        insert_run(&store, "eligible", "codex", "accepted", 100, None)?;
        store.hold_stop_request(&StopRequest { work_id: "target".into(), conversation: "c".into(), target: "local".into(),
            requested_at_ms: 1, deadline_ms: 2, timed_out: false })?;
        store.connection.execute_batch("COMMIT")?;
        for legacy in [true, false] {
            let mut stop_samples = Vec::new(); let mut journal_samples = Vec::new();
            for _ in 0..3 {
                let start = Instant::now();
                let stop = if legacy { store.stop_requests()?.into_iter().find(|stop| stop.work_id == "target") }
                    else { store.stop_request("target")? };
                stop_samples.push(start.elapsed().as_nanos());
                assert_eq!(stop.context("target stop")?.conversation, "c");
                let start = Instant::now();
                let eligible: Vec<_> = if legacy { store.work_records()?.into_iter().filter(|record|
                    record.kind != WorkKind::Task && matches!(record.state, crate::work::WorkState::Accepted | crate::work::WorkState::Starting { .. })
                        && record.send_window_over(100 + RECONCILE_GRACE_MS)).collect() }
                    else { store.reconcile_work_records(100 + RECONCILE_GRACE_MS, false)? };
                journal_samples.push(start.elapsed().as_nanos());
                assert_eq!(eligible.iter().map(|record| record.work_id.as_str()).collect::<Vec<_>>(), vec!["eligible"]);
            }
            eprintln!("controlled_sql_rows={size} legacy={legacy}");
            report("single_stop", &stop_samples); report("eligible_journal", &journal_samples);
        }
    }
    Ok(())
}

#[test]
fn a_late_owner_branch_still_recomputes_other_authors_authorization() -> Result<()> {
    let net = net()?;
    let mut origin = store(&net)?;
    let base = created(&mut origin, &net, &net.a)?;
    let tip = base.last().context("prelude")?;
    let owner_body = |node: &LocalIdentity, epoch| net.a.fixture_admin().attest(&net.network, "c",
        EventBody::ConversationOwner { scheduler_node: node.device_id()?, epoch, attestation: None, cause: None });
    let owner_b = sign_after(&net, &net.a, tip, owner_body(&net.b, 2)?, vec![])?;
    let fork = sign_after(&net, &net.a, tip, bot_reply("fork", "seed"), vec![])?;
    let tail = sign_after(&net, &net.a, &owner_b, bot_reply("tail", "seed"), vec![])?;
    let claim = net.b.sign_event(Event { version: WIRE_VERSION, network_id: net.network.clone(), author: net.b.device_id()?,
        seq: 1, prev: None, deps: vec![tail.hash.clone()], scope_id: "c".into(), created_at_ms: 20,
        body: EventBody::ScheduleRunClaimed { schedule_id: "synthetic-schedule".into(), slot_ms: 20, owner_epoch: 2 }, blobs: vec![] })?;
    let late_owner = sign_after(&net, &net.a, tip, owner_body(&net.a, 3)?, vec![])?;
    let mut replica = store(&net)?;
    replica.ingest_events_at(&net.network, &base, 50)?;
    replica.ingest_events_at(&net.network, &[owner_b, fork, tail], 50)?;
    replica.ingest_events_at(&net.network, std::slice::from_ref(&claim), 50)?;
    assert_eq!(effective(&replica, &net, &[&claim])?, vec![true]);
    assert_full_recompute_oracle(&replica, &net.network)?;
    log::test_metrics::reset();
    replica.ingest_events_at(&net.network, &[late_owner], 50)?;
    assert_eq!(log::test_metrics::snapshot().recomputes, 1);
    assert_eq!(effective(&replica, &net, &[&claim])?, vec![false]);
    assert_full_recompute_oracle(&replica, &net.network)?;
    Ok(())
}

#[test]
#[ignore = "signed pending cascade can exceed the one-event outer ingest page"]
fn signed_pending_cascade_scale() -> Result<()> {
    for size in [1000usize, 10000, 100000] {
        let net = net()?;
        let mut origin = store(&net)?;
        let base = created(&mut origin, &net, &net.a)?;
        let mut previous = base.last().context("prelude")?.clone();
        let mut history = Vec::with_capacity(size);
        for index in 0..size {
            let event = sign_after(&net, &net.a, &previous, bot_reply(&format!("history-{index}"), "seed"), vec![])?;
            previous = event.clone(); history.push(event);
        }
        let dependency = net.b.sign_event(Event { version: WIRE_VERSION, network_id: net.network.clone(), author: net.b.device_id()?,
            seq: 1, prev: None, deps: vec![previous.hash.clone()], scope_id: "c".into(), created_at_ms: 10,
            body: bot_reply("dependency", "seed"), blobs: vec![] })?;
        // Respect the existing pending-event fair share instead of fabricating an inadmissible queue.
        let view = membership_from(&origin.connection, &net.network)?;
        let authors: std::collections::BTreeSet<_> = view.members.iter().map(|member| member.device_id.as_str())
            .chain(view.revoked.iter().map(|revoked| revoked.device_id.as_str()))
            .filter(|author| view.write_bound(author).is_some()).collect();
        let pending_count = size.min(log::MAX_PENDING_EVENTS as usize / authors.len().max(1)).min(4096);
        let mut pending = Vec::with_capacity(pending_count);
        for index in 0..pending_count {
            let deps = if index == 0 { vec![dependency.hash.clone()] } else { vec![] };
            let event = sign_after(&net, &net.a, &previous, bot_reply(&format!("pending-{index}"), "seed"), deps)?;
            previous = event.clone(); pending.push(event);
        }
        for page in [64, 256, 1024] {
            let mut samples = Vec::new();
            for repeat in 0..3 {
                let mut replica = store(&net)?;
                replica.ingest_events_at(&net.network, &base, 50)?;
                for batch in history.chunks(page) { replica.ingest_events_at(&net.network, batch, 50)?; }
                for batch in pending.chunks(page) {
                    let result = replica.ingest_events_at(&net.network, batch, 50)?;
                    ensure!(result.statuses.iter().all(|status| *status == IngestStatus::Pending), "cascade setup did not hold pending tails");
                }
                log::test_metrics::reset();
                let start = Instant::now();
                let result = replica.ingest_events_at(&net.network, std::slice::from_ref(&dependency), 50)?;
                samples.push(start.elapsed().as_nanos());
                assert_eq!(result.statuses, vec![IngestStatus::Accepted]);
                assert_eq!(result.stored.len(), pending_count + 1);
                let metrics = log::test_metrics::snapshot();
                eprintln!("cascade history={size} pending={pending_count} setup_page={page} outer_page=1 repeat={repeat}");
                report("cascade_ingest", &metrics.ingest_ns); report("cascade_validation", &metrics.validation_ns);
                assert_full_recompute_oracle(&replica, &net.network)?;
            }
            report("cascade_admission", &samples);
        }
    }
    Ok(())
}
