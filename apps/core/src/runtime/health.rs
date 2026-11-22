//! Health checks a Node runs on itself every minute: a running work with no progress, an App
//! Server that stops answering, a reachable peer whose sync stalls. A problem is logged when it
//! begins and when it ends, never in between, and a new one goes to the primary bot once.
//! Nothing here restarts a process or sends a write again.
use super::*;
use crate::logging::{Filter, Level, Source};
use crate::work::WorkState;

const INTERVAL: Duration = Duration::from_secs(60);
/// ponytail: one fixed limit; per-provider limits if long silent turns trip it.
const WORK_STALL_MS: i64 = 10 * 60_000;
/// Ten hello rounds (`peers::HELLO_INTERVAL`) without a finished sync.
const SYNC_STALL_MS: i64 = 10 * 60_000;
/// ponytail: at most 10 s per probe, so one round stays under its minute.
const APP_SERVER_TIMEOUT: Duration = Duration::from_secs(10);
const APP_SERVER_MISSES: u32 = 2;
const HOUR_MS: i64 = 3_600_000;
/// New problems handed to the primary bot in any hour; each key at most once an hour.
const HANDOFFS_PER_HOUR: usize = 6;
pub(crate) const MAX_LOG_READ: usize = 200;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Check {
    WorkStalled,
    AppServerUnresponsive,
    SyncStalled,
}

impl Check {
    fn began(self) -> &'static str {
        match self {
            Self::WorkStalled => "work.stalled",
            Self::AppServerUnresponsive => "app_server.unresponsive",
            Self::SyncStalled => "sync.stalled",
        }
    }

    fn ended(self) -> &'static str {
        match self {
            Self::WorkStalled => "work.recovered",
            Self::AppServerUnresponsive => "app_server.recovered",
            Self::SyncStalled => "sync.recovered",
        }
    }

    fn summary(self) -> &'static str {
        match self {
            Self::WorkStalled => "a running work shows no progress",
            Self::AppServerUnresponsive => "the Codex App Server does not answer",
            Self::SyncStalled => "a reachable peer stopped syncing",
        }
    }

    fn title(self) -> &'static str {
        match self {
            Self::WorkStalled => "运行中的工作长时间没有进展",
            Self::AppServerUnresponsive => "Codex App Server 没有响应",
            Self::SyncStalled => "对端可以连通，但同步停滞",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) struct ProblemKey {
    check: Check,
    subject: String,
}

#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub(crate) struct Problem {
    pub check: Check,
    /// The work ID, the peer's device ID, or `codex` for the App Server.
    pub subject: String,
    pub since_ms: i64,
    pub detail: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub conversation_id: Option<String>,
}

impl Problem {
    fn key(&self) -> ProblemKey {
        ProblemKey {
            check: self.check,
            subject: self.subject.clone(),
        }
    }
}

pub(crate) struct HealthState {
    started_ms: i64,
    checked_ms: Option<i64>,
    problems: BTreeMap<ProblemKey, Problem>,
    /// The last progress event of each running work.
    progress: HashMap<String, i64>,
    /// Probes the App Server missed in a row, and when the first of them was.
    app_server_misses: (u32, i64),
    /// Problems handed to the primary bot within the last hour.
    handed: Vec<(ProblemKey, i64)>,
}

impl HealthState {
    pub(crate) fn new(started_ms: i64) -> Self {
        Self {
            started_ms,
            checked_ms: None,
            problems: BTreeMap::new(),
            progress: HashMap::new(),
            app_server_misses: (0, 0),
            handed: Vec::new(),
        }
    }

    pub(crate) fn progressed(&mut self, work_id: &str, at_ms: i64) {
        self.progress.insert(work_id.to_owned(), at_ms);
    }

    /// Takes what the checks in `ran` found now; those that did not run keep their problems.
    /// Returns the problems that began and those that ended. An open problem keeps its start.
    fn update(
        &mut self,
        ran: &[Check],
        found: Vec<Problem>,
        now: i64,
    ) -> (Vec<Problem>, Vec<Problem>) {
        self.checked_ms = Some(now);
        let mut began = Vec::new();
        let mut open = BTreeMap::new();
        for problem in found {
            let key = problem.key();
            let problem = match self.problems.remove(&key) {
                Some(known) => Problem {
                    since_ms: known.since_ms,
                    ..problem
                },
                None => {
                    began.push(problem.clone());
                    problem
                }
            };
            open.insert(key, problem);
        }
        let (ended, kept): (Vec<_>, Vec<_>) = std::mem::take(&mut self.problems)
            .into_iter()
            .partition(|(key, _)| ran.contains(&key.check));
        open.extend(kept);
        self.problems = open;
        (
            began,
            ended.into_iter().map(|(_, problem)| problem).collect(),
        )
    }

    /// Whether a new problem may go to the primary bot now, and counts it if so.
    fn may_hand(&mut self, key: &ProblemKey, now: i64) -> bool {
        self.handed.retain(|(_, at)| now - at < HOUR_MS);
        if self.handed.len() >= HANDOFFS_PER_HOUR || self.handed.iter().any(|(k, _)| k == key) {
            return false;
        }
        self.handed.push((key.clone(), now));
        true
    }

    /// Records one App Server probe, `None` when it is not connected; returns when it began
    /// missing once it missed `APP_SERVER_MISSES` in a row.
    fn app_server_probe(&mut self, answered: Option<bool>, now: i64) -> Option<i64> {
        let (misses, since) = &mut self.app_server_misses;
        if answered != Some(false) {
            *misses = 0;
            return None;
        }
        if *misses == 0 {
            *since = now;
        }
        *misses += 1;
        (*misses >= APP_SERVER_MISSES).then_some(*since)
    }
}

/// Runs of works that are past `WORK_STALL_MS` since their last progress.
fn stalled_works(
    state: &mut HealthState,
    records: Vec<crate::work::WorkRecord>,
    now: i64,
) -> Vec<Problem> {
    let running: Vec<_> = records
        .into_iter()
        .filter(|record| matches!(record.state, WorkState::Running { .. }))
        .collect();
    state
        .progress
        .retain(|work, _| running.iter().any(|record| &record.work_id == work));
    running
        .into_iter()
        .filter_map(|record| {
            let last = state
                .progress
                .get(&record.work_id)
                .copied()
                .unwrap_or(0)
                .max(record.progress_at_ms)
                .max(state.started_ms);
            (now - last > WORK_STALL_MS).then(|| Problem {
                check: Check::WorkStalled,
                detail: format!(
                    "no progress event for over {} minutes on thread {}",
                    WORK_STALL_MS / 60_000,
                    record.state.thread().unwrap_or_default()
                ),
                subject: record.work_id,
                since_ms: last,
                conversation_id: Some(record.conversation),
            })
        })
        .collect()
}

fn stalled_peers(peers: &[crate::maintenance::PeerHealth], now: i64) -> Vec<Problem> {
    peers
        .iter()
        .filter_map(|peer| {
            let synced = peer.synced_ms?;
            (peer.reachable == Some(true) && now - synced > SYNC_STALL_MS).then(|| Problem {
                check: Check::SyncStalled,
                subject: peer.device_id.clone(),
                since_ms: synced,
                detail: format!("{} answers, but no sync has finished since", peer.name),
                conversation_id: None,
            })
        })
        .collect()
}

fn record(problem: &Problem, began: bool) {
    let subject = problem.subject.as_str();
    let (work_id, peer) = match problem.check {
        Check::WorkStalled => (Some(subject), None),
        Check::SyncStalled => (None, Some(subject)),
        Check::AppServerUnresponsive => (None, None),
    };
    let conversation_id = problem.conversation_id.as_deref();
    if began {
        tracing::warn!(
            event = problem.check.began(),
            work_id,
            conversation_id,
            peer,
            since_ms = problem.since_ms,
            detail = problem.detail,
            "{}",
            problem.check.summary()
        );
    } else {
        tracing::info!(
            event = problem.check.ended(),
            work_id,
            conversation_id,
            peer,
            since_ms = problem.since_ms,
            "{} no longer",
            problem.check.summary()
        );
    }
}

/// The turn the primary bot gets for a new problem.
fn handoff_note(problem: &Problem, device: &str) -> String {
    let conversation = problem
        .conversation_id
        .as_ref()
        .map(|id| format!("，会话 {id}"))
        .unwrap_or_default();
    format!(
        "设备 {device} 的 Node 健康检查发现新问题：{}（{}）。\n对象：{}{conversation}\n开始于：{}（Unix 毫秒）\n详情：{}\n\n先用 read_health 和 read_logs 排查原因。能自行处理的就处理，例如用 propose_provider_config 提议配置；需要本人决定时按现有拍板流程发卡片。这条提示来自 Node 的健康检查，不是本人的消息。",
        problem.check.title(),
        problem.check.began(),
        problem.subject,
        problem.since_ms,
        problem.detail.chars().take(500).collect::<String>(),
    )
}

/// The same problem in the same hour gets the same key, so a retried write finds the first.
fn handoff_key(problem: &Problem, now: i64) -> String {
    let subject = crate::crypto::digest(
        serde_json::to_string(&(problem.check, &problem.subject))
            .expect("strings")
            .as_bytes(),
    );
    format!("health-{}-{}", &subject[..16], now / HOUR_MS)
}

/// Warnings and errors since `since_ms` by event, repeats included.
fn log_counts(dir: &Path, since_ms: i64) -> Value {
    let filter = Filter {
        since_ms: Some(since_ms),
        level: Some(Level::Warn),
        limit: Some(crate::logging::MAX_READ),
        ..Default::default()
    };
    let mut counts: BTreeMap<&str, BTreeMap<String, u64>> =
        [("warn", BTreeMap::new()), ("error", BTreeMap::new())].into();
    for record in crate::logging::read(dir, &filter).unwrap_or_default() {
        let level = if record.level == Level::Error {
            "error"
        } else {
            "warn"
        };
        let times = 1 + record
            .fields
            .get("repeated")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        *counts
            .get_mut(level)
            .expect("both levels")
            .entry(record.event)
            .or_default() += times;
    }
    json!(counts)
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct LogQuery {
    since_ms: Option<i64>,
    level: Option<Level>,
    source: Option<Source>,
    event: Option<String>,
    conversation_id: Option<String>,
    grep: Option<String>,
    limit: Option<usize>,
}

/// `read_logs`: the newest matching records of `node.log`, redacted, at most `MAX_LOG_READ`.
pub(crate) fn read_logs(dir: &Path, args: &Value) -> Result<Value> {
    let query: LogQuery =
        serde_json::from_value(args.clone()).context("invalid read_logs arguments")?;
    let limit = query.limit.unwrap_or(50).clamp(1, MAX_LOG_READ);
    let filter = Filter {
        since_ms: query.since_ms,
        level: query.level,
        sources: query.source.into_iter().collect(),
        event: query.event,
        conversation_id: query.conversation_id,
        text: query.grep,
        limit: Some(limit),
        ..Default::default()
    };
    Ok(json!({"records":crate::logging::read(dir, &filter)?}))
}

impl Runtime {
    pub(super) fn start_health(self: &Arc<Self>) {
        let node = self.clone();
        if let Err(error) = self.background_tasks.spawn(async move {
            loop {
                tokio::time::sleep(INTERVAL).await;
                let _work = node.maintenance.enter_when_open().await;
                node.check_health().await;
            }
        }) {
            tracing::debug!(%error, "health checks skipped while the Node is shutting down");
        }
    }

    /// A provider event of a running work.
    pub(super) fn work_progressed(&self, work_id: &str) {
        self.health
            .lock()
            .expect("health")
            .progressed(work_id, now_ms());
    }

    async fn check_health(self: &Arc<Self>) {
        let now = now_ms();
        let mut ran = vec![Check::AppServerUnresponsive, Check::SyncStalled];
        let mut found = Vec::new();
        match self
            .store
            .read("health.works", |store| store.open_work_records())
            .await
        {
            Ok(records) => {
                ran.push(Check::WorkStalled);
                found.extend(stalled_works(
                    &mut self.health.lock().expect("health"),
                    records,
                    now,
                ));
            }
            Err(error) => tracing::debug!(%error, "the work check waits for the next round"),
        }
        let answered = self.probe_app_server().await;
        if let Some(since_ms) = self
            .health
            .lock()
            .expect("health")
            .app_server_probe(answered, now)
        {
            found.push(Problem {
                check: Check::AppServerUnresponsive,
                subject: "codex".into(),
                since_ms,
                detail: format!(
                    "{APP_SERVER_MISSES} probes in a row got no answer within {} s",
                    APP_SERVER_TIMEOUT.as_secs()
                ),
                conversation_id: None,
            });
        }
        found.extend(stalled_peers(&self.peer_health().await, now));
        let (began, ended) = self.health.lock().expect("health").update(&ran, found, now);
        for problem in &ended {
            record(problem, false);
        }
        for problem in began {
            record(&problem, true);
            if let Err(error) = self.hand_to_primary_bot(&problem, now).await {
                tracing::warn!(event = "health.handoff_failed", %error, check = ?problem.check,
                    subject = problem.subject, "a new problem did not reach the primary bot");
            }
        }
    }

    /// One read the App Server answers at once; `None` while it is not connected. An error
    /// it returns is an answer.
    async fn probe_app_server(&self) -> Option<bool> {
        let client = self.codex.client();
        if client.status().await["connection"]["state"] != "connected" {
            return None;
        }
        let read = client.request("account/read", json!({"refreshToken":false}));
        Some(match tokio::time::timeout(APP_SERVER_TIMEOUT, read).await {
            Ok(Ok(_)) => true,
            Ok(Err(error)) => error.answered,
            Err(_) => false,
        })
    }

    /// Sets the primary bot a one-time reminder about a new problem, within the hourly limits.
    /// Without a running bot or a primary bot only the log has it.
    async fn hand_to_primary_bot(&self, problem: &Problem, now: i64) -> Result<()> {
        if self.manager().is_none() || !self.writable() {
            return Ok(());
        }
        let Some(network) = self.store.call("network", |store| store.network()).await? else {
            return Ok(());
        };
        if !self
            .health
            .lock()
            .expect("health")
            .may_hand(&problem.key(), now)
        {
            tracing::info!(event = "health.handoff_limited", check = ?problem.check,
                subject = problem.subject, "the hourly limit holds this problem back from the primary bot");
            return Ok(());
        }
        let device = self.identity.device_id()?;
        let (key, note) = (handoff_key(problem, now), handoff_note(problem, &device));
        let identity = self.identity.clone();
        let reminded = {
            let network = network.clone();
            self.store
                .call("health.remind_primary_bot", move |store| {
                    store.remind_primary_bot(&identity, &network, &key, &note, now)
                })
                .await?
        };
        let Some((change, written)) = reminded else {
            tracing::info!(event = "health.no_primary_bot", check = ?problem.check,
                "no primary bot to hand the problem to");
            return Ok(());
        };
        if written {
            self.publish_changed(
                &network,
                &change.conversation_id,
                Changed::Event(&change.event_hash),
            );
        }
        tracing::info!(event = "health.handed", conversation_id = change.conversation_id,
            check = ?problem.check, subject = problem.subject, "a new problem went to the primary bot");
        Ok(())
    }

    /// Open problems, when the checks last ran, and the last hour's warnings and errors by event.
    pub(crate) async fn health_status(&self) -> Value {
        let (checked, problems) = {
            let state = self.health.lock().expect("health");
            (
                state.checked_ms,
                state.problems.values().cloned().collect::<Vec<_>>(),
            )
        };
        let dir = self.data_dir.clone();
        let since = now_ms() - HOUR_MS;
        let counts = tokio::task::spawn_blocking(move || log_counts(&dir, since))
            .await
            .unwrap_or_else(|_| json!({}));
        json!({"checked_at_ms":checked,"problems":problems,"last_hour":counts})
    }

    /// `read_health`: the health checks, each peer's link and the triage summary.
    pub(super) async fn read_health(&self) -> Result<Value> {
        Ok(
            json!({"health":self.health_status().await,"peers":self.peer_health().await,
            "triage":self.triage_status().await?}),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn problem(check: Check, subject: &str, since_ms: i64) -> Problem {
        Problem {
            check,
            subject: subject.into(),
            since_ms,
            detail: "detail".into(),
            conversation_id: None,
        }
    }

    #[test]
    fn a_problem_begins_and_ends_once() {
        let mut state = HealthState::new(0);
        let all = [
            Check::WorkStalled,
            Check::AppServerUnresponsive,
            Check::SyncStalled,
        ];
        let (began, ended) = state.update(&all, vec![problem(Check::SyncStalled, "p", 5)], 10);
        assert_eq!((began.len(), ended.len()), (1, 0));
        let (began, ended) = state.update(&all, vec![problem(Check::SyncStalled, "p", 9)], 20);
        assert!(began.is_empty() && ended.is_empty());
        assert_eq!(state.problems.values().next().unwrap().since_ms, 5);
        // A check that could not run keeps what it found.
        let (_, ended) = state.update(&[Check::WorkStalled], vec![], 30);
        assert!(ended.is_empty());
        let (began, ended) = state.update(&all, vec![], 40);
        assert_eq!((began.len(), ended.len()), (0, 1));
        assert!(state.update(&all, vec![], 50).1.is_empty());
    }

    #[test]
    fn handoffs_are_once_an_hour_per_problem_and_six_in_all() {
        let mut state = HealthState::new(0);
        let key = |subject: &str| problem(Check::WorkStalled, subject, 0).key();
        assert!(state.may_hand(&key("a"), 0));
        assert!(!state.may_hand(&key("a"), HOUR_MS - 1));
        assert!(state.may_hand(&key("a"), HOUR_MS));
        for subject in ["b", "c", "d", "e", "f"] {
            assert!(state.may_hand(&key(subject), HOUR_MS));
        }
        assert!(!state.may_hand(&key("g"), HOUR_MS));
        assert_eq!(
            handoff_key(&problem(Check::WorkStalled, "a", 0), 1),
            handoff_key(&problem(Check::WorkStalled, "a", 9), HOUR_MS - 1)
        );
        assert_ne!(
            handoff_key(&problem(Check::WorkStalled, "a", 0), 1),
            handoff_key(&problem(Check::WorkStalled, "a", 0), HOUR_MS)
        );
    }

    #[test]
    fn the_app_server_is_unresponsive_after_two_misses() {
        let mut state = HealthState::new(0);
        assert_eq!(state.app_server_probe(Some(false), 1), None);
        assert_eq!(state.app_server_probe(Some(false), 2), Some(1));
        assert_eq!(state.app_server_probe(None, 3), None);
        assert_eq!(state.app_server_probe(Some(false), 4), None);
        assert_eq!(state.app_server_probe(Some(true), 5), None);
    }

    #[test]
    fn read_logs_filters_and_caps() -> Result<()> {
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../.artifacts")
            .join(format!("health-logs-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir)?;
        let mut lines = String::new();
        for index in 0..300 {
            let record = crate::logging::Record {
                ts_ms: index,
                level: if index % 2 == 0 {
                    Level::Warn
                } else {
                    Level::Info
                },
                source: Source::Node,
                event: if index % 3 == 0 {
                    "work.stalled"
                } else {
                    "other"
                }
                .into(),
                message: format!("line {index} Bearer secret-token-value"),
                conversation_id: Some(if index < 150 { "c1" } else { "c2" }.into()),
                work_id: None,
                thread_id: None,
                fields: Default::default(),
            };
            lines.push_str(&serde_json::to_string(&record)?);
            lines.push('\n');
        }
        std::fs::write(dir.join(crate::logging::FILE), lines)?;
        let all = read_logs(&dir, &json!({"limit":1000}))?;
        assert_eq!(all["records"].as_array().unwrap().len(), MAX_LOG_READ);
        let some = read_logs(
            &dir,
            &json!({"level":"warn","event":"work.stalled","conversation_id":"c1","since_ms":10}),
        )?;
        let records = some["records"].as_array().unwrap();
        // Warn and divisible by 3 means divisible by 6: 12 to 144 in c1.
        assert_eq!(records.len(), 23);
        assert!(records.iter().all(|record| {
            record["event"] == "work.stalled" && !record.to_string().contains("secret-token-value")
        }));
        assert_eq!(
            read_logs(&dir, &json!({"grep":"line 7 "}))?["records"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        assert!(read_logs(&dir, &json!({"unknown":1})).is_err());
        std::fs::remove_dir_all(dir)?;
        Ok(())
    }

    #[test]
    fn peer_health_names_its_link() {
        let peer = crate::maintenance::PeerHealth {
            device_id: "d".into(),
            path: Some(crate::net::PathKind::Relay),
            name: "Mac".into(),
            reachable: Some(true),
            synced_ms: Some(1),
            rtt_ms: Some(42),
            path_changes: 3,
            disconnects: 1,
        };
        assert_eq!(
            serde_json::to_value(&peer).unwrap(),
            json!({"device_id":"d","path":"relay","name":"Mac","reachable":true,"synced_ms":1,
                "rtt_ms":42,"path_changes":3,"disconnects":1})
        );
        let stalled = stalled_peers(&[peer], 1 + SYNC_STALL_MS + 1);
        assert_eq!(stalled.len(), 1);
        assert_eq!(
            (stalled[0].check, stalled[0].since_ms),
            (Check::SyncStalled, 1)
        );
    }
}
