//! Health checks a Node runs on itself every minute: a running work with no progress, an App
//! Server that stops answering, a reachable peer whose sync stalls. A problem begins once two
//! rounds in a row find it; it is logged when it begins and when it ends, never in between, and
//! a new one goes to the primary bot once when this Node schedules that bot. Nothing here
//! restarts a process or sends a write again.
use super::*;
use crate::logging::{Filter, Level, Source};
use crate::store::bot_settings::PrimaryReminder;
use crate::work::WorkState;

const INTERVAL: Duration = Duration::from_secs(60);
/// A longer gap between rounds means the computer slept: that round only resets the baseline.
const ASLEEP_MS: i64 = 3 * INTERVAL.as_millis() as i64;
/// ponytail: one fixed limit; per-provider limits if long silent turns trip it.
const WORK_STALL_MS: i64 = 10 * 60_000;
/// Ten hello rounds (`peers::HELLO_INTERVAL`) without a finished sync.
const SYNC_STALL_MS: i64 = 10 * 60_000;
/// ponytail: at most 10 s per probe, so one round stays under its minute.
const APP_SERVER_TIMEOUT: Duration = Duration::from_secs(10);
const HOUR_MS: i64 = 3_600_000;
const DAY_MS: i64 = 24 * HOUR_MS;
/// New problems handed to the primary bot in any hour and any day; each key at most once an hour.
const HANDOFFS_PER_HOUR: usize = 6;
const HANDOFFS_PER_DAY: usize = 24;
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
    /// When this Node started or last woke from sleep; nothing stalls from before it.
    baseline_ms: i64,
    checked_ms: Option<i64>,
    problems: BTreeMap<ProblemKey, Problem>,
    /// Found in the last round only; a second round in a row opens them.
    pending: BTreeSet<ProblemKey>,
    /// The last progress event of each running work.
    progress: HashMap<String, i64>,
    /// When the App Server's current run of missed probes began.
    app_server_missed: Option<i64>,
    /// Problems handed to the primary bot within the last day.
    handed: Vec<(ProblemKey, i64)>,
}

impl HealthState {
    pub(crate) fn new(started_ms: i64) -> Self {
        Self {
            baseline_ms: started_ms,
            checked_ms: None,
            problems: BTreeMap::new(),
            pending: BTreeSet::new(),
            progress: HashMap::new(),
            app_server_missed: None,
            handed: Vec::new(),
        }
    }

    pub(crate) fn progressed(&mut self, work_id: &str, at_ms: i64) {
        self.progress.insert(work_id.to_owned(), at_ms);
    }

    /// After a gap of over `ASLEEP_MS` since the last round, as when the computer slept, moves
    /// the baseline to `now` and forgets what the last round found; that round judges nothing.
    fn woke(&mut self, now: i64) -> bool {
        let asleep = self.checked_ms.is_some_and(|last| now - last > ASLEEP_MS);
        if asleep {
            self.checked_ms = Some(now);
            self.baseline_ms = now;
            self.pending.clear();
            self.app_server_missed = None;
        }
        asleep
    }

    /// Takes what the checks in `ran` found now; those that did not run keep their problems.
    /// A problem begins when two rounds in a row find it. Returns the problems that began and
    /// those that ended. An open problem keeps its start.
    fn update(
        &mut self,
        ran: &[Check],
        found: Vec<Problem>,
        now: i64,
    ) -> (Vec<Problem>, Vec<Problem>) {
        self.checked_ms = Some(now);
        let mut began = Vec::new();
        let mut open = BTreeMap::new();
        let last_round = std::mem::take(&mut self.pending);
        // What a check that did not run found waits for its next round.
        let mut pending: BTreeSet<_> = last_round
            .iter()
            .filter(|key| !ran.contains(&key.check))
            .cloned()
            .collect();
        for problem in found {
            let key = problem.key();
            let problem = match self.problems.remove(&key) {
                Some(known) => Problem {
                    since_ms: known.since_ms,
                    ..problem
                },
                None if last_round.contains(&key) => {
                    began.push(problem.clone());
                    problem
                }
                None => {
                    pending.insert(key);
                    continue;
                }
            };
            open.insert(key, problem);
        }
        self.pending = pending;
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

    /// Whether a new problem may go to the primary bot now: each key once an hour, at most
    /// `HANDOFFS_PER_HOUR` an hour and `HANDOFFS_PER_DAY` a day.
    fn may_hand(&mut self, key: &ProblemKey, now: i64) -> bool {
        self.handed.retain(|(_, at)| now - at < DAY_MS);
        let hour = || self.handed.iter().filter(|(_, at)| now - at < HOUR_MS);
        self.handed.len() < HANDOFFS_PER_DAY
            && hour().count() < HANDOFFS_PER_HOUR
            && !hour().any(|(k, _)| k == key)
    }

    /// Counts a problem the primary bot got.
    fn handed(&mut self, key: &ProblemKey, now: i64) {
        self.handed.push((key.clone(), now));
    }

    /// Records one answered (`true`) or missed probe; returns when the run of misses began.
    fn app_server_probe(&mut self, answered: bool, now: i64) -> Option<i64> {
        if answered {
            self.app_server_missed = None;
            return None;
        }
        Some(*self.app_server_missed.get_or_insert(now))
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
                .max(state.baseline_ms);
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

/// Peers that answer but finished no sync for `SYNC_STALL_MS`, counted from `baseline_ms` at
/// the earliest.
fn stalled_peers(
    peers: &[crate::maintenance::PeerHealth],
    baseline_ms: i64,
    now: i64,
) -> Vec<Problem> {
    peers
        .iter()
        .filter_map(|peer| {
            let synced = peer.synced_ms?;
            (peer.reachable == Some(true) && now - synced.max(baseline_ms) > SYNC_STALL_MS).then(
                || Problem {
                    check: Check::SyncStalled,
                    subject: peer.device_id.clone(),
                    since_ms: synced,
                    detail: format!("{} answers, but no sync has finished since", peer.name),
                    conversation_id: None,
                },
            )
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
        "设备 {device} 的 Node 健康检查发现新问题：{}（{}）。\n发现问题的设备：{device}\n对象：{}{conversation}\n开始于：{}（Unix 毫秒）\n详情：{}\n\n先用 read_health 和 read_logs 排查原因。这两个工具只读取执行这一轮的设备自己的日志和健康状态，这一轮不在设备 {device} 上执行时不可用。这一轮不能派发或控制工作，也不能设提醒或给其他 bot 发消息；能自行处理的就处理，例如用 propose_provider_config 提议配置；需要本人决定时按现有拍板流程发卡片。这条提示来自 Node 的健康检查，不是本人的消息。",
        problem.check.title(),
        problem.check.began(),
        problem.subject,
        problem.since_ms,
        problem.detail.chars().take(500).collect::<String>(),
    )
}

/// The same problem on the same device in the same hour gets the same key, so a retried write
/// finds the first.
fn handoff_key(problem: &Problem, device: &str, now: i64) -> String {
    let subject = crate::crypto::digest(
        serde_json::to_string(&(problem.check, &problem.subject, device))
            .expect("strings")
            .as_bytes(),
    );
    format!("health-{}-{}", &subject[..16], now / HOUR_MS)
}

/// Whether a health reminder started the turn of `trigger`; such a turn dispatches no work.
pub(crate) fn health_turn(trigger: &str) -> bool {
    matches!(crate::scheduling::Trigger::decode(trigger),
        Ok(crate::scheduling::Trigger::Schedule { id, .. })
            if id.starts_with(crate::store::bot_settings::HEALTH_SCHEDULE_PREFIX))
}

/// The tools a health reminder's turn is offered: the tools that only read this network's
/// sessions, nodes, logs and health, and the provider-config card, which changes nothing until
/// the user allows it. No work, wake or settings tool, and no `fetch_url`: a turn a log line
/// started does not reach outside the network.
pub(crate) fn health_turn_tool(tool: crate::tools::Tool) -> bool {
    use crate::tools::{Effect, Tool};
    (tool.effect() == Effect::Read && tool != Tool::FetchUrl) || tool == Tool::ProposeProviderConfig
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
    until_ms: Option<i64>,
    level: Option<Level>,
    source: Option<Source>,
    event: Option<String>,
    conversation_id: Option<String>,
    grep: Option<String>,
    cursor: Option<String>,
    limit: Option<usize>,
}

/// Where a `read_logs` page stopped: the records at or before `until_ms`, less the newest `skip`
/// of those at `until_ms`, which earlier pages returned. Opaque to the Agent.
#[derive(serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct LogCursor {
    until_ms: i64,
    skip: usize,
}

impl LogCursor {
    fn encode(&self) -> String {
        use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
        URL_SAFE_NO_PAD.encode(serde_json::to_vec(self).expect("cursor JSON"))
    }

    fn decode(text: &str) -> Result<Self> {
        use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
        let bytes = URL_SAFE_NO_PAD
            .decode(text)
            .ok()
            .context("invalid read_logs cursor")?;
        serde_json::from_slice(&bytes).context("invalid read_logs cursor")
    }
}

/// Characters of one record's message, and bytes of its fields as JSON, a `read_logs` page keeps.
const LOG_MESSAGE_CHARS: usize = 2_000;
const LOG_FIELDS_BYTES: usize = 2_000;

/// `read_logs`: the newest matching records of `node.log` by `ts_ms`, redacted, at most
/// `MAX_LOG_READ` and `PAGE_BYTES` of JSON. When older records did not fit, the page says
/// `truncated` and gives the `cursor` that reads on from where it stopped.
pub(crate) fn read_logs(dir: &Path, args: &Value) -> Result<Value> {
    let query: LogQuery =
        serde_json::from_value(args.clone()).context("invalid read_logs arguments")?;
    let limit = query.limit.unwrap_or(50).clamp(1, MAX_LOG_READ);
    let cursor = query.cursor.as_deref().map(LogCursor::decode).transpose()?;
    let (until_ms, skip) = match (cursor, query.until_ms) {
        (Some(cursor), until) if until.is_none_or(|until| cursor.until_ms <= until) => {
            (Some(cursor.until_ms), cursor.skip)
        }
        (_, until) => (until, 0),
    };
    let filter = Filter {
        since_ms: query.since_ms,
        until_ms,
        level: query.level,
        sources: query.source.into_iter().collect(),
        event: query.event,
        conversation_id: query.conversation_id,
        text: query.grep,
        limit: Some(limit.saturating_add(skip).min(crate::logging::MAX_READ)),
        ..Default::default()
    };
    let mut records = crate::logging::read(dir, &filter)?;
    // A clock set back writes older times after newer ones; pages follow the times.
    records.sort_by_key(|record| record.ts_ms);
    if let Some(until) = until_ms {
        let at_until = records
            .iter()
            .rev()
            .take_while(|record| record.ts_ms == until)
            .count();
        records.truncate(records.len() - skip.min(at_until));
    }
    if records.len() > limit {
        records.drain(..records.len() - limit);
    }
    // Room for the envelope, `truncated` and `cursor`.
    let mut room = super::agent_tools::PAGE_BYTES - 200;
    let mut page: Vec<crate::logging::Record> = Vec::new();
    while let Some(record) = records.pop() {
        let mut record = shortened(record);
        let mut size = serde_json::to_string(&record)?.len() + 1;
        if size > room && page.is_empty() {
            // Not even one record fits: cut it further, so every page moves on.
            record = squeezed(record, room);
            size = serde_json::to_string(&record)?.len() + 1;
        }
        if size > room && !page.is_empty() {
            records.push(record);
            break;
        }
        room = room.saturating_sub(size);
        page.push(record);
    }
    let next = page.last().filter(|_| !records.is_empty()).map(|oldest| {
        let returned = page
            .iter()
            .rev()
            .take_while(|record| record.ts_ms == oldest.ts_ms)
            .count();
        let earlier = if until_ms == Some(oldest.ts_ms) {
            skip
        } else {
            0
        };
        LogCursor {
            until_ms: oldest.ts_ms,
            skip: earlier + returned,
        }
        .encode()
    });
    page.reverse();
    Ok(match next {
        Some(cursor) => json!({"records":page,"truncated":true,"cursor":cursor}),
        None => json!({"records":page}),
    })
}

/// `record` cut until its JSON fits in `room` bytes: no fields, then ever shorter messages.
fn squeezed(mut record: crate::logging::Record, room: usize) -> crate::logging::Record {
    record.fields = serde_json::Map::from_iter([("truncated".to_owned(), json!(true))]);
    let mut chars = record.message.chars().count();
    while chars > 0 && serde_json::to_string(&record).map_or(0, |text| text.len()) + 1 > room {
        chars /= 2;
        record.message = record.message.chars().take(chars).collect::<String>() + "…";
    }
    record
}

/// `record` with its message and its fields cut to a fixed prefix.
fn shortened(mut record: crate::logging::Record) -> crate::logging::Record {
    if record.message.chars().count() > LOG_MESSAGE_CHARS {
        record.message = record
            .message
            .chars()
            .take(LOG_MESSAGE_CHARS)
            .collect::<String>()
            + "…";
    }
    let fields = serde_json::to_string(&record.fields).expect("JSON fields");
    if fields.len() > LOG_FIELDS_BYTES {
        let prefix: String = fields
            .char_indices()
            .take_while(|(at, _)| *at < LOG_FIELDS_BYTES)
            .map(|(_, char)| char)
            .collect();
        record.fields = serde_json::Map::from_iter([
            ("truncated".to_owned(), json!(true)),
            ("prefix".to_owned(), json!(prefix)),
        ]);
    }
    record
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
        if self.health.lock().expect("health").woke(now) {
            tracing::debug!("the health checks start over after a gap, as after sleep");
            return;
        }
        let mut ran = vec![Check::SyncStalled];
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
        // Not connected is no probe: the check keeps what it had.
        if let Some(answered) = self.probe_app_server().await {
            ran.push(Check::AppServerUnresponsive);
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
                        "probes since then got no answer within {} s",
                        APP_SERVER_TIMEOUT.as_secs()
                    ),
                    conversation_id: None,
                });
            }
        }
        let baseline = self.health.lock().expect("health").baseline_ms;
        found.extend(stalled_peers(&self.peer_health().await, baseline, now));
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

    /// One read the App Server answers at once; `None` while it is not connected, or when the
    /// connection dropped the read before an answer. An error it returns is an answer.
    async fn probe_app_server(&self) -> Option<bool> {
        let client = self.codex.client();
        if client.status().await["connection"]["state"] != "connected" {
            return None;
        }
        let read = client.request("account/read", json!({"refreshToken":false}));
        match tokio::time::timeout(APP_SERVER_TIMEOUT, read).await {
            Ok(Ok(_)) => Some(true),
            Ok(Err(error)) => error.answered.then_some(true),
            Err(_) => Some(false),
        }
    }

    /// Sets the primary bot a one-time reminder about a new problem, within the hourly and
    /// daily limits. Only the Node that schedules the primary bot may; no Node path lets another
    /// one start that bot's turn without the user's own message, so there only the log has it,
    /// as it does without a running bot or a primary bot.
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
                subject = problem.subject, "the hourly or daily limit holds this problem back from the primary bot");
            return Ok(());
        }
        let device = self.identity.device_id()?;
        let (key, note) = (
            handoff_key(problem, &device, now),
            handoff_note(problem, &device),
        );
        let identity = self.identity.clone();
        let reminded = {
            let network = network.clone();
            self.store
                .call("health.remind_primary_bot", move |store| {
                    store.remind_primary_bot(&identity, &network, &key, &note, now)
                })
                .await?
        };
        match reminded {
            PrimaryReminder::NoPrimaryBot => {
                tracing::info!(event = "health.no_primary_bot", check = ?problem.check,
                    "no primary bot to hand the problem to");
            }
            PrimaryReminder::NotOwner {
                conversation_id,
                owner,
            } => {
                tracing::info!(event = "health.not_owner", conversation_id, owner,
                    check = ?problem.check, subject = problem.subject,
                    "another Node schedules the primary bot, so only this log has the problem");
            }
            PrimaryReminder::Existing(change) => {
                tracing::debug!(conversation_id = change.conversation_id, check = ?problem.check,
                    "the primary bot already has this problem");
            }
            PrimaryReminder::Written(change) => {
                self.health
                    .lock()
                    .expect("health")
                    .handed(&problem.key(), now);
                self.publish_changed(
                    &network,
                    &change.conversation_id,
                    Changed::Event(&change.event_hash),
                );
                tracing::info!(event = "health.handed", conversation_id = change.conversation_id,
                    check = ?problem.check, subject = problem.subject, "a new problem went to the primary bot");
            }
        }
        Ok(())
    }

    /// Whether this agent turn may call `read_logs` or `read_health`; see
    /// `Store::diagnostics_turn`.
    pub(super) async fn diagnostics_turn(
        &self,
        network: &str,
        scope: &str,
        trigger: &str,
    ) -> Result<bool> {
        let (network, scope, trigger) = (network.to_owned(), scope.to_owned(), trigger.to_owned());
        let local = self.identity.device_id()?;
        self.store
            .read("health.diagnostics_turn", move |store| {
                store.diagnostics_turn(&network, &local, &scope, &trigger)
            })
            .await
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

    /// `read_health`: the health checks, each peer's sync state and the triage summary. As the
    /// `nodes` tool gives an agent no connection observation, the path, round-trip time and
    /// link counts stay out.
    pub(super) async fn read_health(&self) -> Result<Value> {
        let peers: Vec<Value> = self
            .peer_health()
            .await
            .into_iter()
            .map(|peer| {
                json!({"device_id":peer.device_id,"name":peer.name,"reachable":peer.reachable,
                    "synced_ms":peer.synced_ms})
            })
            .collect();
        Ok(json!({"health":self.health_status().await,"peers":peers,
            "triage":self.triage_status().await?}))
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

    const ALL: [Check; 3] = [
        Check::WorkStalled,
        Check::AppServerUnresponsive,
        Check::SyncStalled,
    ];

    #[test]
    fn a_problem_begins_after_two_rounds_and_ends_once() {
        let mut state = HealthState::new(0);
        let (began, _) = state.update(&ALL, vec![problem(Check::SyncStalled, "p", 5)], 10);
        assert!(began.is_empty(), "one round is not enough");
        let (began, ended) = state.update(&ALL, vec![problem(Check::SyncStalled, "p", 5)], 20);
        assert_eq!((began.len(), ended.len()), (1, 0));
        let (began, ended) = state.update(&ALL, vec![problem(Check::SyncStalled, "p", 9)], 30);
        assert!(began.is_empty() && ended.is_empty());
        assert_eq!(state.problems.values().next().unwrap().since_ms, 5);
        // A check that could not run keeps what it found.
        let (_, ended) = state.update(&[Check::WorkStalled], vec![], 40);
        assert!(ended.is_empty());
        let (began, ended) = state.update(&ALL, vec![], 50);
        assert_eq!((began.len(), ended.len()), (0, 1));
        assert!(state.update(&ALL, vec![], 60).1.is_empty());
        // A round in between that does not find it starts the count over.
        state.update(&ALL, vec![problem(Check::WorkStalled, "w", 0)], 70);
        state.update(&ALL, vec![], 80);
        assert!(
            state
                .update(&ALL, vec![problem(Check::WorkStalled, "w", 0)], 90)
                .0
                .is_empty()
        );
        // A check that did not run keeps its candidate for its next round.
        state.update(&[Check::SyncStalled], vec![], 100);
        assert_eq!(
            state
                .update(&ALL, vec![problem(Check::WorkStalled, "w", 0)], 110)
                .0
                .len(),
            1
        );
    }

    #[test]
    fn a_round_after_sleep_judges_nothing() {
        let mut state = HealthState::new(0);
        state.update(&ALL, vec![problem(Check::SyncStalled, "p", 5)], 60_000);
        assert!(!state.woke(120_000));
        let after_sleep = 60_000 + ASLEEP_MS + 1;
        assert!(state.woke(after_sleep));
        assert_eq!(state.baseline_ms, after_sleep);
        // What the round before the sleep found does not count toward the next one.
        assert!(
            state
                .update(
                    &ALL,
                    vec![problem(Check::SyncStalled, "p", 5)],
                    after_sleep + 60_000
                )
                .0
                .is_empty()
        );
        let peer = crate::maintenance::PeerHealth {
            device_id: "d".into(),
            path: None,
            name: "Mac".into(),
            reachable: Some(true),
            synced_ms: Some(1),
            rtt_ms: None,
            path_changes: 0,
            disconnects: 0,
        };
        let peers = [peer];
        assert!(stalled_peers(&peers, after_sleep, after_sleep + SYNC_STALL_MS).is_empty());
        assert_eq!(
            stalled_peers(&peers, after_sleep, after_sleep + SYNC_STALL_MS + 1)[0].since_ms,
            1
        );
    }

    #[test]
    fn handoffs_are_once_an_hour_per_problem_six_an_hour_and_24_a_day() {
        let mut state = HealthState::new(0);
        let key = |subject: &str| problem(Check::WorkStalled, subject, 0).key();
        // Only a written handoff counts.
        assert!(state.may_hand(&key("a"), 0));
        assert!(state.may_hand(&key("a"), 0));
        state.handed(&key("a"), 0);
        assert!(!state.may_hand(&key("a"), HOUR_MS - 1));
        assert!(state.may_hand(&key("a"), HOUR_MS));
        for subject in ["a", "b", "c", "d", "e", "f"] {
            assert!(state.may_hand(&key(subject), HOUR_MS));
            state.handed(&key(subject), HOUR_MS);
        }
        assert!(!state.may_hand(&key("g"), HOUR_MS));
        let mut at = 2 * HOUR_MS;
        while state.handed.len() < HANDOFFS_PER_DAY {
            let subject = format!("s{at}");
            assert!(state.may_hand(&key(&subject), at));
            state.handed(&key(&subject), at);
            at += HOUR_MS / 6;
        }
        assert!(at < DAY_MS);
        assert!(!state.may_hand(&key("late"), at));
        assert!(state.may_hand(&key("late"), DAY_MS + 1));
        let device = "device-1";
        assert_eq!(
            handoff_key(&problem(Check::WorkStalled, "a", 0), device, 1),
            handoff_key(&problem(Check::WorkStalled, "a", 9), device, HOUR_MS - 1)
        );
        assert_ne!(
            handoff_key(&problem(Check::WorkStalled, "a", 0), device, 1),
            handoff_key(&problem(Check::WorkStalled, "a", 0), device, HOUR_MS)
        );
        assert_ne!(
            handoff_key(&problem(Check::WorkStalled, "a", 0), device, 1),
            handoff_key(&problem(Check::WorkStalled, "a", 0), "device-2", 1)
        );
        assert!(handoff_note(&problem(Check::WorkStalled, "a", 0), device).contains(device));
    }

    #[test]
    fn the_app_server_misses_since_its_first_miss() {
        let mut state = HealthState::new(0);
        assert_eq!(state.app_server_probe(false, 1), Some(1));
        assert_eq!(state.app_server_probe(false, 2), Some(1));
        assert_eq!(state.app_server_probe(true, 3), None);
        assert_eq!(state.app_server_probe(false, 4), Some(4));
    }

    #[test]
    fn a_health_turn_dispatches_no_work() {
        use crate::tools::Tool;
        let health = crate::scheduling::schedule_trigger("health-abc", 1);
        assert!(health_turn(&health));
        assert!(!health_turn(&crate::scheduling::schedule_trigger(
            "schedule-abc",
            1
        )));
        assert!(!health_turn("message:health-abc"));
        assert!(!health_turn_tool(Tool::DispatchWork));
        assert!(!health_turn_tool(Tool::InterruptWork));
        assert!(!health_turn_tool(Tool::Remind));
        assert!(!health_turn_tool(Tool::MessageBot));
        assert!(!health_turn_tool(Tool::FetchUrl));
        assert!(!health_turn_tool(Tool::SaveSchedule));
        assert!(!health_turn_tool(Tool::ApplyProviderConfig));
        assert!(health_turn_tool(Tool::ReadLogs));
        assert!(health_turn_tool(Tool::ReadHealth));
        assert!(health_turn_tool(Tool::Context));
        assert!(health_turn_tool(Tool::Nodes));
        assert!(health_turn_tool(Tool::ProposeProviderConfig));
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

    /// A log directory holding `count` records, record `index` at `ts(index)` and built by `fill`.
    fn log_dir(
        name: &str,
        count: i64,
        ts: impl Fn(i64) -> i64,
        fill: impl Fn(&mut crate::logging::Record),
    ) -> Result<PathBuf> {
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../.artifacts")
            .join(format!("{name}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir)?;
        let mut lines = String::new();
        for index in 0..count {
            let mut record = crate::logging::Record {
                ts_ms: ts(index),
                level: Level::Error,
                source: Source::Mobile,
                event: "mobile.uncaught".into(),
                message: format!("{index} "),
                conversation_id: None,
                work_id: None,
                thread_id: None,
                fields: Default::default(),
            };
            fill(&mut record);
            lines.push_str(&serde_json::to_string(&record)?);
            lines.push('\n');
        }
        std::fs::write(dir.join(crate::logging::FILE), lines)?;
        Ok(dir)
    }

    /// Every page from the newest on, following `cursor`: the indexes the messages start with,
    /// oldest first, and the number of pages.
    fn read_every_page(dir: &Path) -> Result<(Vec<i64>, usize)> {
        let (mut seen, mut cursor, mut pages) = (Vec::new(), None::<String>, 0);
        loop {
            let page = read_logs(dir, &json!({"limit":200,"cursor":cursor}))?;
            assert!(page.to_string().len() <= super::super::agent_tools::PAGE_BYTES);
            pages += 1;
            assert!(pages <= 200, "the pages move on");
            let records = page["records"].as_array().unwrap();
            assert!(!records.is_empty());
            let mut indexes: Vec<i64> = records
                .iter()
                .map(|record| {
                    let message = record["message"].as_str().unwrap();
                    assert!(message.chars().count() <= LOG_MESSAGE_CHARS + 1);
                    message.split(' ').next().unwrap().parse().unwrap()
                })
                .collect();
            indexes.append(&mut seen);
            seen = indexes;
            if page["truncated"] != true {
                assert!(page.get("cursor").is_none());
                break;
            }
            cursor = Some(page["cursor"].as_str().unwrap().to_owned());
        }
        Ok((seen, pages))
    }

    fn large(record: &mut crate::logging::Record) {
        record.message.push_str(&"x".repeat(5_000));
        record.fields =
            serde_json::Map::from_iter([("stack".to_owned(), json!("y".repeat(5_000)))]);
    }

    #[test]
    fn read_logs_pages_large_records_by_bytes() -> Result<()> {
        let dir = log_dir("health-log-pages", 100, |index| index, large)?;
        let (seen, pages) = read_every_page(&dir)?;
        assert!(pages > 1, "a page of 100 large records is split");
        assert_eq!(seen, (0..100).collect::<Vec<_>>());
        let first = read_logs(&dir, &json!({"limit":200}))?;
        assert!(first["records"][0]["fields"]["truncated"] == true);
        std::fs::remove_dir_all(dir)?;
        Ok(())
    }

    #[test]
    fn read_logs_pages_records_of_one_millisecond() -> Result<()> {
        let dir = log_dir("health-log-same-ms", 100, |_| 5, large)?;
        let (seen, pages) = read_every_page(&dir)?;
        assert!(pages > 1);
        assert_eq!(seen, (0..100).collect::<Vec<_>>());
        std::fs::remove_dir_all(dir)?;
        Ok(())
    }

    #[test]
    fn read_logs_follows_times_when_the_clock_went_back() -> Result<()> {
        // Written 0..50 at 100.. and then 50..100 at 0.. after the clock went back.
        let dir = log_dir(
            "health-log-clock",
            100,
            |index| if index < 50 { 100 + index } else { index - 50 },
            large,
        )?;
        let (seen, _) = read_every_page(&dir)?;
        let mut expected: Vec<i64> = (50..100).collect();
        expected.extend(0..50);
        assert_eq!(seen, expected);
        std::fs::remove_dir_all(dir)?;
        Ok(())
    }

    #[test]
    fn read_logs_cuts_a_record_larger_than_a_page() -> Result<()> {
        let dir = log_dir(
            "health-log-huge",
            3,
            |index| index,
            |record| {
                record.message.push_str(&"\u{1}".repeat(5_000));
                record.work_id = Some("w".repeat(31_000));
            },
        )?;
        let (seen, pages) = read_every_page(&dir)?;
        assert_eq!((seen, pages), (vec![0, 1, 2], 3));
        assert!(read_logs(&dir, &json!({"cursor":"not a cursor"})).is_err());
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
        let stalled = stalled_peers(&[peer], 0, 1 + SYNC_STALL_MS + 1);
        assert_eq!(stalled.len(), 1);
        assert_eq!(
            (stalled[0].check, stalled[0].since_ms),
            (Check::SyncStalled, 1)
        );
    }
}
