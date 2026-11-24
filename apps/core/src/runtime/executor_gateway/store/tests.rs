use super::*;
use crate::{
    model::{ResolvedModel, WorkKind},
    providers::Provider,
};
use std::path::PathBuf;
use vibeface_bridge::{
    failure::{ExecFailure, FailureKind},
    turn::TurnStatus,
};

mod webhooks;

struct Fixture {
    store: GatewayStore,
    dir: PathBuf,
}

impl Fixture {
    fn new() -> Result<Self> {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../.artifacts/gateway-mvp/store-tests")
            .join(uuid::Uuid::new_v4().to_string());
        let store = GatewayStore::open(&dir)?;
        Ok(Self { store, dir })
    }

    fn caller(&self) -> Result<Caller> {
        let minted = self.store.mint(false, None, None)?;
        self.store.authenticate(minted["key"].as_str().unwrap())
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn gateway_spec(caller: &Caller, call_ref: &str) -> WorkSpec {
    let conversation = format!("gateway:{}", caller.id);
    WorkSpec {
        work_id: WorkSpec::id(&conversation, "gateway", call_ref),
        conversation,
        trigger: "gateway".into(),
        target: "local".into(),
        dispatcher: "local".into(),
        call_ref: call_ref.into(),
        kind: WorkKind::Agent(Provider::Codex),
        model: None,
        resolved: ResolvedModel {
            provider: Provider::Codex,
            model: None,
        },
        cwd: "/fixture".into(),
        title: "t".into(),
        prompt: "hello".into(),
        continues: None,
        parent: None,
        thread: None,
    }
}

fn start(store: &GatewayStore, id: &str, thread: &str) -> Result<()> {
    store.signal(id, Signal::ThreadCreated(thread.into()))?;
    store.signal(
        id,
        Signal::TurnStarted {
            turn_id: "turn-1".into(),
            epoch: 1,
        },
    )?;
    Ok(())
}

fn finish(store: &GatewayStore, id: &str) -> Result<()> {
    store.signal(
        id,
        Signal::Probed(crate::work::Probe::Terminal {
            turn_id: "turn-1".into(),
            status: TurnStatus::Completed,
            text: "done".into(),
            error: None,
        }),
    )?;
    Ok(())
}

#[test]
fn mint_authenticate_and_idempotent_accept() -> Result<()> {
    fn send_sync<T: Send + Sync>() {}
    send_sync::<GatewayStore>();
    let f = Fixture::new()?;
    let caller = f.caller()?;
    let spec = gateway_spec(&caller, "call-1");
    let (first, created) = f.store.accept(&caller, &spec, false, None)?;
    assert!(created);
    let mut defaults_changed = spec.clone();
    defaults_changed.resolved.model = Some("new-provider-default".into());
    let (again, duplicate) = f.store.accept(&caller, &defaults_changed, false, None)?;
    assert!(!duplicate);
    assert_eq!(
        first, again,
        "resolved defaults must not change replay admission"
    );
    let mut changed = spec.clone();
    changed.prompt = "changed".into();
    assert_eq!(
        f.store
            .accept(&caller, &changed, false, None)
            .unwrap_err()
            .downcast_ref::<super::super::GatewayError>()
            .map(|e| e.code),
        Some("idempotency_conflict")
    );
    assert!(f.store.authenticate("bad-key").is_err());
    assert!(f.store.accept(&caller, &spec, true, None).is_err());
    assert_eq!(first.compact()["sandbox"], "read-only");
    assert!(
        f.store
            .mint(false, Some("http://127.0.0.1/hook"), None)
            .is_err()
    );
    assert!(f.store.mint(false, None, Some("secret")).is_err());
    assert!(
        f.store
            .mint(false, Some("file:///tmp/hook"), Some("secret"))
            .is_err()
    );
    let reopened = GatewayStore::open(&f.dir)?;
    assert_eq!(reopened.get(&caller, &spec.work_id)?, first);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(f.dir.join("gateway"))?
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
    }
    Ok(())
}

#[test]
fn caller_and_native_thread_isolation_and_terminal_reuse() -> Result<()> {
    let f = Fixture::new()?;
    let owner = f.caller()?;
    let foreign = f.caller()?;
    let work = gateway_spec(&owner, "first");
    f.store.accept(&owner, &work, false, None)?;
    start(&f.store, &work.work_id, "owned-thread")?;
    assert!(f.store.get(&foreign, &work.work_id).is_err());
    assert!(f.store.find(&foreign, &work.work_id)?.is_none());
    assert!(f.store.by_thread(&foreign, "owned-thread").is_err());
    assert!(
        f.store
            .claim_control(&foreign, &work.work_id, "k", "steer", "hidden")
            .is_err()
    );
    let mut continuation = gateway_spec(&owner, "second");
    continuation.thread = Some("unknown-thread".into());
    assert!(f.store.accept(&owner, &continuation, false, None).is_err());
    continuation.thread = Some("owned-thread".into());
    assert_eq!(
        f.store
            .accept(&owner, &continuation, false, None)
            .unwrap_err()
            .downcast_ref::<super::super::GatewayError>()
            .map(|e| e.code),
        Some("thread_busy")
    );
    f.store.signal(
        &work.work_id,
        Signal::SubmissionUnverified("not proven".into()),
    )?;
    assert!(f.store.accept(&owner, &continuation, false, None).is_err());
    finish(&f.store, &work.work_id)?;
    assert!(f.store.get(&owner, &work.work_id)?.record.state.finished());
    let mut foreign_work = gateway_spec(&foreign, "foreign");
    foreign_work.thread = Some("owned-thread".into());
    assert!(
        f.store
            .accept(&foreign, &foreign_work, false, None)
            .is_err(),
        "terminal ownership is retained"
    );
    let (next, _) = f.store.accept(&owner, &continuation, false, None)?;
    assert_eq!(
        f.store.native_thread("owned-thread")?.unwrap().spec.work_id,
        next.spec.work_id
    );
    let collision = gateway_spec(&foreign, "collision");
    f.store.accept(&foreign, &collision, false, None)?;
    assert!(
        f.store
            .signal(
                &collision.work_id,
                Signal::ThreadCreated("owned-thread".into())
            )
            .is_err()
    );
    assert!(
        f.store
            .signal(
                &work.work_id,
                Signal::ThreadCreated("switched-thread".into())
            )
            .is_err()
    );
    Ok(())
}

#[test]
fn control_replay_conflict_and_unchanged_signals_do_not_write() -> Result<()> {
    let f = Fixture::new()?;
    let caller = f.caller()?;
    let work = gateway_spec(&caller, "controls");
    f.store.accept(&caller, &work, false, None)?;
    assert!(
        f.store
            .claim_control(&caller, &work.work_id, "k", "steer", "hello")?
    );
    assert!(
        !f.store
            .claim_control(&caller, &work.work_id, "k", "steer", "hello")?
    );
    assert_eq!(
        f.store
            .claim_control(&caller, &work.work_id, "k", "steer", "changed")
            .unwrap_err()
            .downcast_ref::<super::super::GatewayError>()
            .map(|e| e.code),
        Some("idempotency_conflict")
    );
    assert!(
        f.store
            .claim_control(&caller, &work.work_id, "k", "stop", "hello")
            .is_err()
    );
    let before: u64 = f
        .store
        .connection()?
        .query_row("SELECT total_changes()", [], |row| row.get(0))?;
    f.store
        .signal(&work.work_id, Signal::Text("ignored before start".into()))?;
    let after: u64 = f
        .store
        .connection()?
        .query_row("SELECT total_changes()", [], |row| row.get(0))?;
    assert_eq!(before, after);
    f.store.signal(
        &work.work_id,
        Signal::StartRejected(ExecFailure::new(
            FailureKind::Rejected,
            "native sandbox refused",
        )),
    )?;
    let compact = f.store.get(&caller, &work.work_id)?.compact();
    assert_eq!(compact["failure_reason"], "native sandbox refused");
    assert_eq!(compact["outcome"], "rejected");
    assert!(f.store.pending()?.is_empty());
    Ok(())
}

#[test]
fn needs_input_is_durable_before_turn_and_distinct_from_unverified() -> Result<()> {
    let f = Fixture::new()?;
    let caller = f.caller()?;
    let work = gateway_spec(&caller, "input");
    f.store.accept(&caller, &work, false, None)?;
    f.store.signal(
        &work.work_id,
        Signal::Progress {
            summary: "needs_input".into(),
            at_ms: now_ms(),
        },
    )?;
    assert_eq!(
        f.store.get(&caller, &work.work_id)?.compact()["state"],
        "accepted",
        "no thread to bind input request"
    );
    f.store
        .signal(&work.work_id, Signal::ThreadCreated("input-thread".into()))?;
    f.store.signal(
        &work.work_id,
        Signal::Progress {
            summary: "needs_input".into(),
            at_ms: now_ms(),
        },
    )?;
    let reopened = GatewayStore::open(&f.dir)?;
    assert_eq!(
        reopened.get(&caller, &work.work_id)?.compact()["state"],
        "needs_input"
    );
    f.store.signal(
        &work.work_id,
        Signal::TurnStarted {
            turn_id: "turn-1".into(),
            epoch: 1,
        },
    )?;
    f.store.signal(
        &work.work_id,
        Signal::Progress {
            summary: "commentary".into(),
            at_ms: now_ms() + 60_000,
        },
    )?;
    assert_eq!(
        f.store.get(&caller, &work.work_id)?.compact()["state"],
        "needs_input"
    );
    f.store.signal(
        &work.work_id,
        Signal::SubmissionUnverified("unknown result".into()),
    )?;
    assert_eq!(
        f.store.get(&caller, &work.work_id)?.compact()["state"],
        "unverified"
    );
    finish(&f.store, &work.work_id)?;
    assert_eq!(
        f.store.get(&caller, &work.work_id)?.compact()["state"],
        "finished"
    );
    Ok(())
}
