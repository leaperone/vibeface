//! Health checks a Node runs on itself every minute: a running work with no progress or repeated
//! tools, an App Server that stops answering, a reachable peer whose sync stalls. Two rounds in
//! a row open a problem; it is logged when it begins and when it ends, never in between, and
//! a new one goes to the primary bot once when this Node schedules that bot. Nothing here
//! restarts a process or sends a write again.
use super::*;
use crate::logging::{Filter, Level, Source};
use crate::store::bot_settings::PrimaryReminder;
use crate::work::WorkState;
use std::collections::VecDeque;

const INTERVAL: Duration = Duration::from_secs(60);
/// A longer gap between rounds means the computer slept: that round only resets the baseline.
const ASLEEP_MS: i64 = 3 * INTERVAL.as_millis() as i64;
/// ponytail: one fixed limit; per-provider limits if long silent turns trip it.
const WORK_STALL_MS: i64 = 10 * 60_000;
const TOOL_WINDOW_MS: i64 = 5 * 60_000;
const TOOL_REPEAT_LIMIT: usize = 6;
const MAX_TOOL_WORKS: usize = 128;
const MAX_TOOL_BYTES: usize = 64 * 1024;
/// Ten hello rounds (`peers::HELLO_INTERVAL`) without a finished sync.
const SYNC_STALL_MS: i64 = 10 * 60_000;
/// ponytail: one limit for every waiting state; a long but legitimate active turn can trip it
/// for the inputs queued behind it, which costs one reminder.
const JOB_STALL_MS: i64 = 30 * 60_000;
/// ponytail: at most 10 s per probe, so one round stays under its minute.
const APP_SERVER_TIMEOUT: Duration = Duration::from_secs(10);
const HOUR_MS: i64 = 3_600_000;
const DAY_MS: i64 = 24 * HOUR_MS;
/// New problems handed to the primary bot in any hour and any day; each key at most once an hour.
const HANDOFFS_PER_HOUR: usize = 6;
const HANDOFFS_PER_DAY: usize = 24;
pub(crate) const MAX_LOG_READ: usize = 200;
/// Open problems one member Node shares, and how much of each.
const MAX_SHARED_PROBLEMS: usize = 32;
const MAX_SUBJECT_BYTES: usize = 256;
const MAX_DETAIL_CHARS: usize = 500;
/// ponytail: one fixed wait per member, asked all at once, so a round stays under its minute.
const PEER_HEALTH_TIMEOUT: Duration = Duration::from_secs(3);

#[derive(
    Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Check {
    WorkStalled,
    WorkLooping,
    AppServerUnresponsive,
    SyncStalled,
    JobStalled,
}

impl Check {
    fn began(self) -> &'static str {
        match self {
            Self::WorkStalled => "work.stalled",
            Self::WorkLooping => "work.looping",
            Self::AppServerUnresponsive => "app_server.unresponsive",
            Self::SyncStalled => "sync.stalled",
            Self::JobStalled => "job.stalled",
        }
    }

    fn ended(self) -> &'static str {
        match self {
            Self::WorkStalled => "work.recovered",
            Self::WorkLooping => "work.loop_ended",
            Self::AppServerUnresponsive => "app_server.recovered",
            Self::SyncStalled => "sync.recovered",
            Self::JobStalled => "job.recovered",
        }
    }

    fn summary(self) -> &'static str {
        match self {
            Self::WorkStalled => "a running work shows no progress",
            Self::WorkLooping => "a running work repeats tool results or explicit failures",
            Self::AppServerUnresponsive => "the Codex App Server does not answer",
            Self::SyncStalled => "a connected peer stopped syncing",
            Self::JobStalled => "a scheduled turn waits in the same state",
        }
    }

    fn title(self) -> &'static str {
        match self {
            Self::WorkStalled => "运行中的工作长时间没有进展",
            Self::WorkLooping => "运行中的工作疑似重复调用工具或连续失败",
            Self::AppServerUnresponsive => "Codex App Server 没有响应",
            Self::SyncStalled => "对端可以连通，但同步停滞",
            Self::JobStalled => "一轮回复长时间停在同一状态",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) struct ProblemKey {
    check: Check,
    subject: String,
    /// The member Node that found the problem; `None` for this one.
    device: Option<String>,
}

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub(crate) struct Problem {
    pub check: Check,
    /// The work ID, the peer's device ID, or `codex` for the App Server.
    pub subject: String,
    pub since_ms: i64,
    pub detail: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conversation_id: Option<String>,
}

impl Problem {
    fn key(&self) -> ProblemKey {
        ProblemKey {
            check: self.check,
            subject: self.subject.clone(),
            device: None,
        }
    }

    fn found_on(&self, device: &str) -> ProblemKey {
        ProblemKey {
            device: Some(device.to_owned()),
            ..self.key()
        }
    }

    /// A problem a member Node answered with, held to what a reminder note and a log line take.
    fn bounded(mut self) -> Option<Self> {
        if self.subject.is_empty() || self.subject.len() > MAX_SUBJECT_BYTES {
            return None;
        }
        self.detail = self.detail.chars().take(MAX_DETAIL_CHARS).collect();
        self.conversation_id = self
            .conversation_id
            .filter(|id| vibeface_protocol::validate_id(id).is_ok());
        Some(self)
    }
}

struct ToolCompletion {
    id: String,
    fingerprint: Option<String>,
    failed: bool,
    at_ms: i64,
}

impl ToolCompletion {
    /// Only complete native results count; progress, reasoning and tool labels prove no loop.
    fn codex(event: &Value, at_ms: i64) -> Option<Self> {
        if event["method"] != "item/completed" {
            return None;
        }
        let item = &event["params"]["item"];
        let kind = item["type"].as_str()?;
        if !matches!(
            kind,
            "commandExecution" | "mcpToolCall" | "dynamicToolCall" | "fileChange"
        ) {
            return None;
        }
        let id = item["id"].as_str().filter(|id| !id.is_empty())?;
        let failed = item["status"] == "failed"
            || item["exitCode"].as_i64().is_some_and(|code| code != 0)
            || item["success"] == false
            || item["result"]["isError"] == true
            || !item["error"].is_null();
        let evidence = match kind {
            "commandExecution"
                if item["command"].is_string()
                    && item["aggregatedOutput"].is_string()
                    && item["exitCode"].is_i64() =>
            {
                Some((
                    &item["command"],
                    &item["cwd"],
                    &item["aggregatedOutput"],
                    &item["exitCode"],
                ))
            }
            "mcpToolCall"
                if item["tool"].is_string()
                    && !item["arguments"].is_null()
                    && !item["result"].is_null() =>
            {
                Some((
                    &item["tool"],
                    &item["server"],
                    &item["arguments"],
                    &item["result"],
                ))
            }
            "dynamicToolCall"
                if item["tool"].is_string()
                    && !item["arguments"].is_null()
                    && item["success"].is_boolean()
                    && item["contentItems"].is_array() =>
            {
                Some((
                    &item["tool"],
                    &item["arguments"],
                    &item["contentItems"],
                    &item["success"],
                ))
            }
            _ => None,
        };
        // Large or incomplete results can prove failure, but never equality by truncation.
        let fingerprint = evidence
            .filter(|_| vibeface_bridge::transport::value_bytes(item) <= MAX_TOOL_BYTES)
            .map(|evidence| {
                crate::crypto::digest(
                    &serde_json::to_vec(&(kind, failed, evidence)).expect("tool evidence"),
                )
            });
        Some(Self {
            id: crate::crypto::digest(id.as_bytes()),
            fingerprint,
            failed,
            at_ms,
        })
    }
}

struct ToolHistory {
    turn: String,
    completions: VecDeque<ToolCompletion>,
}

impl ToolHistory {
    fn prune(&mut self, now: i64) {
        self.completions
            .retain(|completion| now - completion.at_ms <= TOOL_WINDOW_MS);
    }

    fn repeated(&self) -> Option<(i64, &'static str)> {
        if self.completions.len() < TOOL_REPEAT_LIMIT {
            return None;
        }
        let last = self.completions.back()?;
        let repeated = last.fingerprint.as_ref().is_some_and(|fingerprint| {
            self.completions
                .iter()
                .all(|completion| completion.fingerprint.as_ref() == Some(fingerprint))
        });
        let failures = self.completions.iter().all(|completion| completion.failed);
        (repeated || failures).then(|| {
            (
                self.completions.front().expect("completions").at_ms,
                if failures {
                    "consecutive explicit tool failures"
                } else {
                    "identical tool calls and results"
                },
            )
        })
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
    /// Private digests only; no arguments or outputs enter health, logs or shared history.
    tools: HashMap<String, ToolHistory>,
    /// When the App Server's current run of missed probes began.
    app_server_missed: Option<i64>,
    /// Problems handed to the primary bot within the last day.
    handed: Vec<(ProblemKey, i64)>,
    /// Open problems the primary bot has not got yet; each round tries them again.
    unhanded: BTreeSet<ProblemKey>,
    /// Open problems other member Nodes reported to this one, which schedules the primary bot.
    remote: BTreeMap<ProblemKey, Problem>,
    /// Each open scheduler job's state and since when this Node has seen it in that state.
    jobs: HashMap<String, (&'static str, i64)>,
    /// Since when each connected peer has been seen connected, and its last attachment progress.
    /// The flag says whether that round had an open connection; a new one starts the time over.
    peer_connected: HashMap<String, (i64, bool)>,
    peer_progress: HashMap<String, i64>,
    /// Attachments whose source went idle; each is logged at warn once until it downloads.
    stalled_blobs: BTreeSet<String>,
}

impl HealthState {
    pub(crate) fn new(started_ms: i64) -> Self {
        Self {
            baseline_ms: started_ms,
            checked_ms: None,
            problems: BTreeMap::new(),
            pending: BTreeSet::new(),
            progress: HashMap::new(),
            tools: HashMap::new(),
            app_server_missed: None,
            handed: Vec::new(),
            unhanded: BTreeSet::new(),
            remote: BTreeMap::new(),
            jobs: HashMap::new(),
            peer_connected: HashMap::new(),
            peer_progress: HashMap::new(),
            stalled_blobs: BTreeSet::new(),
        }
    }

    pub(crate) fn progressed(&mut self, work_id: &str, at_ms: i64) {
        self.progress.insert(work_id.to_owned(), at_ms);
    }

    /// An attachment from `peer` moved on, so a long download is no stalled sync.
    pub(crate) fn blob_progressed(&mut self, peer: &str, hash: &str, at_ms: i64) {
        self.peer_progress.insert(peer.to_owned(), at_ms);
        self.stalled_blobs.remove(hash);
    }

    /// Whether this is the first idle stall of `hash` since it last moved on.
    pub(crate) fn blob_stalled(&mut self, hash: &str) -> bool {
        self.stalled_blobs.insert(hash.to_owned())
    }

    fn observed_tool(&mut self, work_id: &str, event: &Value, now: i64) {
        let Some(completion) = ToolCompletion::codex(event, now) else {
            return;
        };
        let Some(turn) = event["params"]["turnId"]
            .as_str()
            .filter(|turn| !turn.is_empty())
        else {
            return;
        };
        self.tools.retain(|_, history| {
            history.prune(now);
            !history.completions.is_empty()
        });
        if !self.tools.contains_key(work_id) && self.tools.len() >= MAX_TOOL_WORKS {
            return;
        }
        let turn = crate::crypto::digest(turn.as_bytes());
        let history = self
            .tools
            .entry(work_id.to_owned())
            .or_insert_with(|| ToolHistory {
                turn: turn.clone(),
                completions: VecDeque::new(),
            });
        if history.turn != turn {
            history.turn = turn;
            history.completions.clear();
        }
        if history
            .completions
            .iter()
            .any(|known| known.id == completion.id)
        {
            return;
        }
        history.completions.push_back(completion);
        if history.completions.len() > TOOL_REPEAT_LIMIT {
            history.completions.pop_front();
        }
    }

    /// After a gap of over `ASLEEP_MS` since the last round, as when the computer slept, moves
    /// the baseline to `now` and forgets what the last round found; that round judges nothing.
    fn woke(&mut self, now: i64) -> bool {
        let asleep = self.checked_ms.is_some_and(|last| now - last > ASLEEP_MS);
        if asleep {
            self.checked_ms = Some(now);
            self.baseline_ms = now;
            self.pending.clear();
            self.tools.clear();
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
        self.unhanded.extend(began.iter().map(Problem::key));
        self.unhanded
            .retain(|key| self.problems.contains_key(key) || self.remote.contains_key(key));
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
        self.unhanded.remove(key);
    }

    /// Open problems the primary bot has not got, with their keys.
    fn unhanded(&self) -> Vec<(ProblemKey, Problem)> {
        self.unhanded
            .iter()
            .filter_map(|key| {
                let problem = self.problems.get(key).or_else(|| self.remote.get(key))?;
                Some((key.clone(), problem.clone()))
            })
            .collect()
    }

    /// Takes what each member Node in `answers` reported: `Some` replaces its open problems,
    /// `None`, a member that did not answer, keeps them. Members not asked lose theirs. A
    /// member confirms its problems over two rounds itself, so a new one is open at once.
    /// Returns the problems that began, with the member that found them.
    fn update_remote(
        &mut self,
        answers: BTreeMap<String, Option<Vec<Problem>>>,
    ) -> Vec<(String, Problem)> {
        let mut began = Vec::new();
        let mut remote = BTreeMap::new();
        let known = std::mem::take(&mut self.remote);
        for (key, problem) in known.clone() {
            let device = key.device.as_deref().unwrap_or_default();
            if matches!(answers.get(device), Some(None)) {
                remote.insert(key, problem);
            }
        }
        for (device, problems) in answers {
            for problem in problems.into_iter().flatten() {
                let key = problem.found_on(&device);
                let since_ms = match known.get(&key) {
                    Some(known) => known.since_ms,
                    None => {
                        began.push((device.clone(), problem.clone()));
                        self.unhanded.insert(key.clone());
                        problem.since_ms
                    }
                };
                remote.insert(
                    key,
                    Problem {
                        since_ms,
                        ..problem
                    },
                );
            }
        }
        self.remote = remote;
        self.unhanded
            .retain(|key| self.problems.contains_key(key) || self.remote.contains_key(key));
        began
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

/// Scheduler jobs that stayed in one waiting state for `JOB_STALL_MS`, counted from when this
/// Node first saw them there and from `baseline_ms` at the earliest. A running turn has its
/// own timeout and its works their own check. While any turn runs, a queued job may wait its
/// turn behind it or for a slot, so a queued job counts only when nothing runs. A health
/// reminder's own turn and a job held by a pending check, which already waits for the user,
/// do not count.
fn stalled_jobs(
    state: &mut HealthState,
    jobs: &[crate::scheduling::SchedulerJob],
    now: i64,
) -> Vec<Problem> {
    use crate::scheduling::JobState;
    let running = jobs
        .iter()
        .any(|job| matches!(job.state, JobState::Running { .. }));
    let waiting: Vec<_> = jobs
        .iter()
        .filter(|job| match job.state {
            JobState::Running { .. } => false,
            JobState::Queued => !running,
            _ => true,
        })
        .filter(|job| !job.pending_check && !health_turn(&job.trigger))
        .map(|job| (format!("{}:{}", job.conversation_id, job.trigger), job))
        .collect();
    let seen = std::mem::take(&mut state.jobs);
    for (key, job) in &waiting {
        let name = job.state.name();
        let since = match seen.get(key) {
            Some((known, since)) if *known == name => *since,
            _ => now,
        };
        state.jobs.insert(key.clone(), (name, since));
    }
    waiting
        .into_iter()
        .filter_map(|(key, job)| {
            let (name, since) = state.jobs[&key];
            let since = since.max(state.baseline_ms);
            (now - since > JOB_STALL_MS).then(|| Problem {
                check: Check::JobStalled,
                subject: key,
                since_ms: since,
                detail: format!("{name} for over {} minutes", JOB_STALL_MS / 60_000),
                conversation_id: Some(job.conversation_id.clone()),
            })
        })
        .collect()
}

/// Runs of works that are past `WORK_STALL_MS` since their last progress.
fn stalled_works(
    state: &mut HealthState,
    records: &[crate::work::WorkRecord],
    now: i64,
) -> Vec<Problem> {
    let running: Vec<_> = records
        .iter()
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
                subject: record.work_id.clone(),
                since_ms: last,
                conversation_id: Some(record.conversation.clone()),
            })
        })
        .collect()
}

fn looping_works(
    state: &mut HealthState,
    records: &[crate::work::WorkRecord],
    now: i64,
) -> Vec<Problem> {
    state.tools.retain(|work_id, history| {
        history.prune(now);
        !history.completions.is_empty()
            && records.iter().any(|record| {
                &record.work_id == work_id
                    && matches!(record.state, WorkState::Running { .. })
                    && record.state.handle().is_some_and(|handle| {
                        crate::crypto::digest(handle.turn_id.as_bytes()) == history.turn
                    })
            })
    });
    records
        .iter()
        .filter_map(|record| {
            let (since_ms, detail) = state.tools.get(&record.work_id)?.repeated()?;
            Some(Problem {
                check: Check::WorkLooping,
                subject: record.work_id.clone(),
                since_ms,
                detail: format!(
                    "{TOOL_REPEAT_LIMIT} {detail} within {} minutes; native Codex evidence",
                    TOOL_WINDOW_MS / 60_000
                ),
                conversation_id: Some(record.conversation.clone()),
            })
        })
        .collect()
}

/// Peers this Node is connected to, or whose last sync finished, with no sync finished for
/// `SYNC_STALL_MS`. A peer whose syncs fail while its connection stays open counts too, and one
/// that never finished a sync since start. The time counts from the latest of the last sync,
/// `baseline_ms`, the round this Node first saw the peer connected and its last attachment
/// progress, so a reconnect or a long attachment download gets the full limit.
fn stalled_peers(
    state: &mut HealthState,
    peers: &[crate::maintenance::PeerHealth],
    now: i64,
) -> Vec<Problem> {
    let connected: Vec<_> = peers
        .iter()
        .filter(|peer| peer.path.is_some() || peer.reachable == Some(true))
        .collect();
    let seen = std::mem::take(&mut state.peer_connected);
    for peer in &connected {
        let open = peer.path.is_some();
        let since = match seen.get(&peer.device_id) {
            Some((since, had)) if *had || !open => *since,
            _ => now,
        };
        state
            .peer_connected
            .insert(peer.device_id.clone(), (since, open));
    }
    state
        .peer_progress
        .retain(|peer, _| state.peer_connected.contains_key(peer));
    connected
        .into_iter()
        .filter_map(|peer| {
            let since_ms = peer.synced_ms.unwrap_or(state.baseline_ms);
            let start = since_ms
                .max(state.baseline_ms)
                .max(state.peer_connected[&peer.device_id].0)
                .max(
                    state
                        .peer_progress
                        .get(&peer.device_id)
                        .copied()
                        .unwrap_or(0),
                );
            let detail = match (peer.synced_ms, peer.reachable) {
                (None, _) => format!(
                    "{} is connected, but no sync has finished since start",
                    peer.name
                ),
                (Some(_), Some(false)) => {
                    format!("{} is connected, but every sync since fails", peer.name)
                }
                _ => format!("{} answers, but no sync has finished since", peer.name),
            };
            (now - start > SYNC_STALL_MS).then(|| Problem {
                check: Check::SyncStalled,
                subject: peer.device_id.clone(),
                since_ms,
                detail,
                conversation_id: None,
            })
        })
        .collect()
}

fn record(problem: &Problem, began: bool) {
    let subject = problem.subject.as_str();
    let (work_id, peer, job) = match problem.check {
        Check::WorkStalled | Check::WorkLooping => (Some(subject), None, None),
        Check::SyncStalled => (None, Some(subject), None),
        Check::JobStalled => (None, None, Some(subject)),
        Check::AppServerUnresponsive => (None, None, None),
    };
    let conversation_id = problem.conversation_id.as_deref();
    if began {
        tracing::warn!(
            event = problem.check.began(),
            work_id,
            conversation_id,
            peer,
            job,
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
            job,
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
/// the user allows it. No work, wake or settings tool, and no `fetch_url`. This limits the
/// coordination tools only; the provider's own shell keeps host access.
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
        // One more than a page, so a page cut at `limit` still gets a cursor.
        limit: Some(
            limit
                .saturating_add(skip)
                .saturating_add(1)
                .min(crate::logging::MAX_READ),
        ),
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
    if records.len() > limit + 1 {
        records.drain(..records.len() - limit - 1);
    }
    // Room for the envelope, `truncated` and `cursor`.
    let mut room = super::agent_tools::PAGE_BYTES - 200;
    let mut page: Vec<crate::logging::Record> = Vec::new();
    while page.len() < limit
        && let Some(record) = records.pop()
    {
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

    pub(super) fn observe_work_tools(&self, work_id: &str, event: &Value) {
        self.health
            .lock()
            .expect("health")
            .observed_tool(work_id, event, now_ms());
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
                ran.extend([Check::WorkStalled, Check::WorkLooping]);
                let mut state = self.health.lock().expect("health");
                found.extend(stalled_works(&mut state, &records, now));
                found.extend(looping_works(&mut state, &records, now));
            }
            Err(error) => tracing::debug!(%error, "the work check waits for the next round"),
        }
        match self
            .store
            .read("health.jobs", |store| store.open_scheduler_jobs())
            .await
        {
            Ok(jobs) => {
                ran.push(Check::JobStalled);
                let mut state = self.health.lock().expect("health");
                found.extend(stalled_jobs(&mut state, &jobs, now));
            }
            Err(error) => tracing::debug!(%error, "the job check waits for the next round"),
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
        let peers = self.peer_health().await;
        found.extend(stalled_peers(
            &mut self.health.lock().expect("health"),
            &peers,
            now,
        ));
        let (began, ended) = self.health.lock().expect("health").update(&ran, found, now);
        for problem in &ended {
            record(problem, false);
        }
        for problem in &began {
            record(problem, true);
        }
        // A problem the primary bot did not get, for a limit, a missing bot or a failed write,
        // goes again each round while it stays open. Only its first try logs at info.
        // ponytail: one store transaction per open problem a minute, as on a Node that does not
        // schedule the primary bot; back off per key if that write lock ever shows up.
        let mut first_keys: BTreeSet<_> = began.iter().map(Problem::key).collect();
        for (device, problem) in self.member_problems(&peers).await {
            tracing::info!(event = "health.member_problem", device, check = ?problem.check,
                subject = problem.subject, since_ms = problem.since_ms, detail = problem.detail,
                "a member Node reports a problem");
            first_keys.insert(problem.found_on(&device));
        }
        let unhanded = self.health.lock().expect("health").unhanded();
        for (key, problem) in unhanded {
            let first = first_keys.contains(&key);
            match self.hand_to_primary_bot(&key, &problem, now, first).await {
                Err(error) if first => {
                    tracing::warn!(event = "health.handoff_failed", %error, check = ?problem.check,
                        subject = problem.subject, "an open problem did not reach the primary bot");
                }
                Err(error) => tracing::debug!(%error, subject = problem.subject,
                    "an open problem still does not reach the primary bot"),
                Ok(()) => {}
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
    async fn hand_to_primary_bot(
        &self,
        problem_key: &ProblemKey,
        problem: &Problem,
        now: i64,
        first: bool,
    ) -> Result<()> {
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
            .may_hand(problem_key, now)
        {
            if first {
                tracing::info!(event = "health.handoff_limited", check = ?problem.check,
                    subject = problem.subject, "the hourly or daily limit holds this problem back from the primary bot");
            }
            return Ok(());
        }
        let device = match &problem_key.device {
            Some(member) => member.clone(),
            None => self.identity.device_id()?,
        };
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
            PrimaryReminder::NoPrimaryBot if first => {
                tracing::info!(event = "health.no_primary_bot", check = ?problem.check,
                    "no primary bot to hand the problem to");
            }
            PrimaryReminder::NotOwner {
                conversation_id,
                owner,
            } if first => {
                tracing::info!(event = "health.not_owner", conversation_id, owner,
                    check = ?problem.check, subject = problem.subject,
                    "another Node schedules the primary bot, so only this log has the problem");
            }
            PrimaryReminder::NoPrimaryBot | PrimaryReminder::NotOwner { .. } => {}
            PrimaryReminder::Existing(change) => {
                self.health
                    .lock()
                    .expect("health")
                    .unhanded
                    .remove(problem_key);
                tracing::debug!(conversation_id = change.conversation_id, check = ?problem.check,
                    "the primary bot already has this problem");
            }
            PrimaryReminder::Written(change) => {
                self.health.lock().expect("health").handed(problem_key, now);
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

    /// On the Node that schedules the primary bot, asks each connected member Node for its open
    /// problems and keeps them as this Node's own, so they reach the primary bot the same way.
    /// Elsewhere, or with no primary bot, member problems stay with their Node.
    async fn member_problems(
        &self,
        peers: &[crate::maintenance::PeerHealth],
    ) -> Vec<(String, Problem)> {
        let owner = async {
            let network = self.store.call("network", |store| store.network()).await?;
            let Some(network) = network else {
                return Ok(None);
            };
            let local = self.identity.device_id()?;
            let read = network.clone();
            let owner = self
                .store
                .read("health.primary_owner", move |store| {
                    store.primary_bot_owner(&read)
                })
                .await?;
            Ok::<_, anyhow::Error>((owner.as_deref() == Some(local.as_str())).then_some(network))
        };
        let network = match owner.await {
            Ok(Some(network)) if self.manager().is_some() => network,
            Ok(_) => {
                self.health
                    .lock()
                    .expect("health")
                    .update_remote(BTreeMap::new());
                return Vec::new();
            }
            Err(error) => {
                tracing::debug!(%error, "member problems wait for the next round");
                return Vec::new();
            }
        };
        let asked = peers
            .iter()
            .filter(|peer| peer.path.is_some() || peer.reachable == Some(true))
            .map(|peer| {
                let network = network.clone();
                async move {
                    let request = self.peer_request(
                        &network,
                        &peer.device_id,
                        json!({"type":"health_problems"}),
                    );
                    let answer = tokio::time::timeout(PEER_HEALTH_TIMEOUT, request).await;
                    let problems = match answer {
                        Ok(Ok(answer)) => {
                            serde_json::from_value::<Vec<Problem>>(answer["problems"].clone())
                                .map(|problems| {
                                    problems
                                        .into_iter()
                                        .take(MAX_SHARED_PROBLEMS)
                                        .filter_map(Problem::bounded)
                                        .collect()
                                })
                                .ok()
                        }
                        Ok(Err(error)) => {
                            tracing::debug!(peer = peer.device_id, %error,
                                "a member Node did not share its problems");
                            None
                        }
                        Err(_) => None,
                    };
                    (peer.device_id.clone(), problems)
                }
            });
        let answers = futures_util::future::join_all(asked)
            .await
            .into_iter()
            .collect();
        self.health.lock().expect("health").update_remote(answers)
    }

    /// `health_problems` for a member Node: this Node's open problems, bounded.
    pub(super) fn shared_problems(&self) -> Value {
        let state = self.health.lock().expect("health");
        let problems: Vec<_> = state
            .problems
            .values()
            .take(MAX_SHARED_PROBLEMS)
            .cloned()
            .filter_map(Problem::bounded)
            .collect();
        json!({"problems":problems})
    }

    /// Open problems, when the checks last ran, and the last hour's warnings and errors by event.
    pub(crate) async fn health_status(&self) -> Value {
        let (checked, problems) = {
            let state = self.health.lock().expect("health");
            let mut problems: Vec<Value> = state.problems.values().map(|p| json!(p)).collect();
            problems.extend(state.remote.iter().map(|(key, problem)| {
                let mut problem = json!(problem);
                problem["device"] = json!(key.device);
                problem
            }));
            (state.checked_ms, problems)
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

    const ALL: [Check; 5] = [
        Check::WorkStalled,
        Check::WorkLooping,
        Check::AppServerUnresponsive,
        Check::SyncStalled,
        Check::JobStalled,
    ];

    fn tool_event(id: usize, command: &str, output: &str, exit: i64) -> Value {
        json!({"method":"item/completed","params":{"turnId":"turn","item":{
            "id":format!("tool-{id}"),"type":"commandExecution","status":"completed",
            "command":command,"cwd":"/private/fixture","aggregatedOutput":output,"exitCode":exit
        }}})
    }

    fn running_work() -> crate::work::WorkRecord {
        crate::work::WorkRecord {
            work_id: "work".into(),
            conversation: "conversation".into(),
            kind: WorkKind::Agent(Provider::Codex),
            state: WorkState::Running {
                handle: crate::work::Handle {
                    thread_id: "thread".into(),
                    turn_id: "turn".into(),
                    epoch: 0,
                },
            },
            text: String::new(),
            send_by_ms: 0,
            progress: String::new(),
            progress_at_ms: 0,
        }
    }

    #[test]
    fn repeated_tools_alert_despite_progress_and_reuse_confirmation_recovery_and_cooldown() {
        let mut state = HealthState::new(0);
        let records = [running_work()];
        for index in 0..TOOL_REPEAT_LIMIT {
            state.observed_tool(
                "work",
                &tool_event(index, "secret-command", "private-result", 0),
                index as i64,
            );
            state.progressed("work", index as i64);
        }
        let found = looping_works(&mut state, &records, 60_000);
        assert_eq!(found.len(), 1);
        assert!(stalled_works(&mut state, &records, 60_000).is_empty());
        assert!(state.update(&ALL, found, 60_000).0.is_empty());
        state.progressed("work", 120_000);
        let found = looping_works(&mut state, &records, 120_000);
        let (began, ended) = state.update(&ALL, found, 120_000);
        assert_eq!((began.len(), ended.len()), (1, 0));
        let problem = &began[0];
        assert_eq!((problem.check, problem.since_ms), (Check::WorkLooping, 0));
        assert_eq!(problem.conversation_id.as_deref(), Some("conversation"));
        let rendered = serde_json::to_string(problem).unwrap();
        assert!(!rendered.contains("secret-command") && !rendered.contains("private-result"));
        assert!(state.may_hand(&problem.key(), 120_000));
        state.handed(&problem.key(), 120_000);
        assert!(!state.may_hand(&problem.key(), 120_001));
        state.observed_tool(
            "work",
            &tool_event(TOOL_REPEAT_LIMIT, "changed", "new result", 0),
            120_001,
        );
        let found = looping_works(&mut state, &records, 120_001);
        assert!(found.is_empty());
        assert_eq!(state.update(&ALL, found, 120_001).1.len(), 1);
    }

    #[test]
    fn explicit_tool_failures_count_with_different_or_oversized_results() {
        let mut state = HealthState::new(0);
        for index in 0..TOOL_REPEAT_LIMIT {
            let event = tool_event(
                index,
                &format!("call-{index}"),
                &"x".repeat(MAX_TOOL_BYTES),
                1,
            );
            assert!(
                ToolCompletion::codex(&event, 0)
                    .unwrap()
                    .fingerprint
                    .is_none()
            );
            state.observed_tool("work", &event, 0);
        }
        let found = looping_works(&mut state, &[running_work()], 60_000);
        assert_eq!(found.len(), 1);
        assert!(found[0].detail.contains("explicit tool failures"));
        state.observed_tool(
            "work",
            &tool_event(TOOL_REPEAT_LIMIT, "next", "unknown", 0),
            60_001,
        );
        assert!(looping_works(&mut state, &[running_work()], 60_001).is_empty());
    }

    #[test]
    fn tool_labels_missing_evidence_duplicates_and_changing_results_do_not_prove_a_loop() {
        for mode in [
            "duplicate",
            "changed output",
            "changed call",
            "incomplete",
            "progress",
        ] {
            let mut state = HealthState::new(0);
            for index in 0..TOOL_REPEAT_LIMIT {
                let mut event = tool_event(
                    if mode == "duplicate" { 0 } else { index },
                    "call",
                    "result",
                    0,
                );
                match mode {
                    "changed output" => {
                        event["params"]["item"]["aggregatedOutput"] =
                            json!(format!("result-{index}"))
                    }
                    "changed call" => {
                        event["params"]["item"]["command"] = json!(format!("call-{index}"))
                    }
                    "incomplete" => {
                        event["params"]["item"]
                            .as_object_mut()
                            .unwrap()
                            .remove("aggregatedOutput");
                    }
                    "progress" => event["method"] = json!("item/started"),
                    _ => {}
                }
                state.observed_tool("work", &event, index as i64);
            }
            assert!(
                looping_works(&mut state, &[running_work()], 60_000).is_empty(),
                "{mode}"
            );
        }
    }

    #[test]
    fn tool_observations_are_bounded_and_reset_after_sleep_expiry_turn_change_or_work_end() {
        let mut state = HealthState::new(0);
        for work in 0..MAX_TOOL_WORKS + 1 {
            for index in 0..TOOL_REPEAT_LIMIT + 1 {
                state.observed_tool(
                    &format!("work-{work}"),
                    &tool_event(index, "call", "result", 0),
                    0,
                );
            }
        }
        assert_eq!(state.tools.len(), MAX_TOOL_WORKS);
        assert!(
            state
                .tools
                .values()
                .all(|history| history.completions.len() == TOOL_REPEAT_LIMIT)
        );
        state.observed_tool(
            "work",
            &tool_event(0, "call", "result", 0),
            TOOL_WINDOW_MS + 1,
        );
        assert_eq!(state.tools.len(), 1);
        for index in 1..TOOL_REPEAT_LIMIT {
            state.observed_tool(
                "work",
                &tool_event(index, "call", "result", 0),
                TOOL_WINDOW_MS + 1,
            );
        }
        let mut next = tool_event(TOOL_REPEAT_LIMIT, "call", "result", 0);
        next["params"]["turnId"] = json!("next-turn");
        state.observed_tool("work", &next, TOOL_WINDOW_MS + 2);
        assert_eq!(state.tools["work"].completions.len(), 1);
        assert!(looping_works(&mut state, &[running_work()], TOOL_WINDOW_MS + 2).is_empty());
        assert!(state.tools.is_empty(), "a previous turn is removed");
        state.observed_tool(
            "work",
            &tool_event(0, "call", "result", 0),
            TOOL_WINDOW_MS + 2,
        );
        assert!(looping_works(&mut state, &[], TOOL_WINDOW_MS + 2).is_empty());
        assert!(state.tools.is_empty(), "an ended work is removed");
        state.observed_tool(
            "work",
            &tool_event(0, "call", "result", 0),
            TOOL_WINDOW_MS + 2,
        );
        state.checked_ms = Some(TOOL_WINDOW_MS + 2);
        assert!(state.woke(TOOL_WINDOW_MS + 3 + ASLEEP_MS));
        assert!(state.tools.is_empty());
    }

    #[test]
    fn native_tool_evidence_ignores_identity_and_timing_but_keeps_inputs_and_results() {
        for item in [
            json!({"type":"mcpToolCall","id":"first","server":"fixture","tool":"lookup",
                "arguments":{"q":"private"},"result":{"content":[{"text":"result"}]}}),
            json!({"type":"dynamicToolCall","id":"first","tool":"lookup",
                "arguments":{"q":"private"},"success":true,"contentItems":[{"text":"result"}]}),
        ] {
            let mut event =
                json!({"method":"item/completed","params":{"turnId":"turn","item":item}});
            let first = ToolCompletion::codex(&event, 0).unwrap();
            assert!(first.fingerprint.is_some());
            event["params"]["item"]["id"] = json!("second");
            event["params"]["item"]["durationMs"] = json!(500);
            let next = ToolCompletion::codex(&event, 1).unwrap();
            assert_eq!(first.fingerprint, next.fingerprint);
            assert_ne!(first.id, next.id);
            event["params"]["item"]["arguments"]["q"] = json!("changed");
            assert_ne!(
                first.fingerprint,
                ToolCompletion::codex(&event, 1).unwrap().fingerprint
            );
        }
        for item in [
            json!({"type":"fileChange","status":"failed"}),
            json!({"type":"dynamicToolCall","success":false}),
            json!({"type":"mcpToolCall","result":{"isError":true}}),
            json!({"type":"mcpToolCall","error":{"message":"private error"}}),
        ] {
            let mut event = json!({"method":"item/completed","params":{"item":item}});
            event["params"]["item"]["id"] = json!("tool");
            assert!(ToolCompletion::codex(&event, 0).unwrap().failed);
        }
    }

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
        assert!(stalled_peers(&mut state, &peers, after_sleep).is_empty());
        assert!(stalled_peers(&mut state, &peers, after_sleep + SYNC_STALL_MS).is_empty());
        assert_eq!(
            stalled_peers(&mut state, &peers, after_sleep + SYNC_STALL_MS + 1)[0].since_ms,
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
        let mut state = HealthState::new(0);
        let peers = [peer];
        stalled_peers(&mut state, &peers, 0);
        let stalled = stalled_peers(&mut state, &peers, 1 + SYNC_STALL_MS + 1);
        assert_eq!(stalled.len(), 1);
        assert_eq!(
            (stalled[0].check, stalled[0].since_ms),
            (Check::SyncStalled, 1)
        );
    }

    #[test]
    fn read_logs_gives_a_cursor_when_more_records_match_than_the_limit() -> Result<()> {
        let dir = log_dir("health-log-count", 51, |index| index, |_| {})?;
        let first = read_logs(&dir, &json!({}))?;
        assert_eq!(first["records"].as_array().unwrap().len(), 50);
        assert_eq!(
            first["truncated"], true,
            "the 51st record is not lost silently"
        );
        let rest = read_logs(&dir, &json!({"cursor":first["cursor"]}))?;
        assert_eq!(rest["records"].as_array().unwrap().len(), 1);
        assert!(rest.get("cursor").is_none());
        std::fs::remove_dir_all(dir)?;
        Ok(())
    }

    #[test]
    fn a_connected_peer_whose_syncs_fail_or_never_finish_stalls() {
        let peer = |path, reachable, synced_ms| crate::maintenance::PeerHealth {
            device_id: "d".into(),
            path,
            name: "Mac".into(),
            reachable,
            synced_ms,
            rtt_ms: None,
            path_changes: 0,
            disconnects: 0,
        };
        let stalls = |peer: crate::maintenance::PeerHealth, baseline: i64, late: i64| {
            let mut state = HealthState::new(baseline);
            let peers = [peer];
            stalled_peers(&mut state, &peers, 0);
            !stalled_peers(&mut state, &peers, late).is_empty()
        };
        let late = SYNC_STALL_MS + 2;
        let direct = Some(crate::net::PathKind::Direct);
        assert!(stalls(peer(direct, Some(false), Some(1)), 0, late));
        assert!(stalls(peer(direct, None, None), 1, late));
        assert!(!stalls(peer(None, Some(false), Some(1)), 0, late));
        assert!(!stalls(peer(direct, None, None), 5, late));
        // A reconnect after an hour away gets the full limit, and so does attachment progress.
        let mut state = HealthState::new(0);
        let failing = [peer(direct, Some(false), Some(1))];
        assert!(stalled_peers(&mut state, &failing, HOUR_MS).is_empty());
        assert!(stalled_peers(&mut state, &failing, HOUR_MS + SYNC_STALL_MS).is_empty());
        state.blob_progressed("d", "h", HOUR_MS + SYNC_STALL_MS);
        assert!(stalled_peers(&mut state, &failing, HOUR_MS + 2 * SYNC_STALL_MS).is_empty());
        assert_eq!(
            stalled_peers(&mut state, &failing, HOUR_MS + 2 * SYNC_STALL_MS + 1).len(),
            1
        );
        assert!(state.blob_stalled("h") && !state.blob_stalled("h"));
    }

    #[test]
    fn a_job_stalls_after_waiting_in_one_state() {
        use crate::scheduling::JobState;
        let job = |trigger: &str, state| crate::scheduling::SchedulerJob {
            conversation_id: "c".into(),
            scope: crate::scheduling::SessionScope::Main,
            trigger: trigger.into(),
            kind: crate::scheduling::TriggerKind::Human,
            sequence: 1,
            basis: crate::model::VersionVector::new(),
            reply_to: trigger.into(),
            message_id: "reply".into(),
            runtime: None,
            pending_check: false,
            tool_calls: false,
            state,
        };
        let mut state = HealthState::new(0);
        let queued = [job("message:m", JobState::Queued)];
        assert!(stalled_jobs(&mut state, &queued, 1_000).is_empty());
        assert!(stalled_jobs(&mut state, &queued, 1_000 + JOB_STALL_MS).is_empty());
        let stalled = stalled_jobs(&mut state, &queued, 1_001 + JOB_STALL_MS);
        assert_eq!(
            (stalled[0].subject.as_str(), stalled[0].since_ms),
            ("c:message:m", 1_000)
        );
        assert!(stalled_jobs(&mut state, &[], 2_000 + JOB_STALL_MS).is_empty());
        assert!(state.jobs.is_empty(), "a finished job is forgotten");
        // Queued behind a running turn, held by a pending check, or a health reminder's own turn.
        let attempt = crate::scheduling::ManagerAttempt {
            id: "a".into(),
            boot: "b".into(),
            permission_version: 1,
        };
        let mut held = job("message:p", JobState::Queued);
        held.pending_check = true;
        let others = [
            job(
                "message:r",
                JobState::Running {
                    attempt: attempt.clone(),
                },
            ),
            job("message:q", JobState::Queued),
            held,
            job("schedule:health-x:1", JobState::Queued),
        ];
        let mut state = HealthState::new(0);
        stalled_jobs(&mut state, &others, 0);
        assert!(stalled_jobs(&mut state, &others, 2 * JOB_STALL_MS).is_empty());
    }

    #[test]
    fn an_open_problem_the_bot_did_not_get_is_tried_again_until_it_ends() {
        let mut state = HealthState::new(0);
        let found = || vec![problem(Check::WorkStalled, "w", 0)];
        state.update(&ALL, found(), 1);
        state.update(&ALL, found(), 2);
        assert_eq!(state.unhanded().len(), 1);
        state.update(&ALL, found(), 3);
        assert_eq!(state.unhanded().len(), 1, "still open, still not handed");
        state.handed(&problem(Check::WorkStalled, "w", 0).key(), 3);
        assert!(state.unhanded().is_empty());
        state
            .unhanded
            .insert(problem(Check::WorkStalled, "w", 0).key());
        state.update(&ALL, vec![], 4);
        assert!(
            state.unhanded().is_empty(),
            "an ended problem is not handed"
        );
    }

    #[test]
    fn member_problems_open_at_once_stay_while_unanswered_and_end_when_gone() {
        let mut state = HealthState::new(0);
        let found = |subject: &str| vec![problem(Check::WorkStalled, subject, 7)];
        let answers = |entries: Vec<(&str, Option<Vec<Problem>>)>| {
            entries
                .into_iter()
                .map(|(device, problems)| (device.to_owned(), problems))
                .collect::<BTreeMap<_, _>>()
        };
        let began = state.update_remote(answers(vec![("b", Some(found("w")))]));
        assert_eq!(
            began.len(),
            1,
            "a member confirmed it over two rounds itself"
        );
        let (key, _) = &state.unhanded()[0];
        assert_eq!(key.device.as_deref(), Some("b"));
        assert_ne!(
            *key,
            problem(Check::WorkStalled, "w", 7).key(),
            "not this Node's own"
        );
        assert!(
            state
                .update_remote(answers(vec![("b", Some(found("w")))]))
                .is_empty()
        );
        assert!(state.update_remote(answers(vec![("b", None)])).is_empty());
        assert_eq!(
            state.remote.len(),
            1,
            "a member that did not answer keeps its problems"
        );
        state.handed(&key.clone(), 1);
        assert!(state.unhanded().is_empty());
        state.update_remote(answers(vec![("b", Some(vec![]))]));
        assert!(
            state.remote.is_empty(),
            "a problem it no longer reports ended"
        );
        state.update_remote(answers(vec![("b", Some(found("w")))]));
        state.update_remote(BTreeMap::new());
        assert!(
            state.remote.is_empty() && state.unhanded().is_empty(),
            "not asked, not kept"
        );
    }

    #[test]
    fn a_member_problem_is_bounded() {
        let mut long = problem(Check::SyncStalled, "p", 0);
        long.detail = "x".repeat(MAX_DETAIL_CHARS + 10);
        long.conversation_id = Some("not an id!".into());
        let long = long.bounded().unwrap();
        assert_eq!(long.detail.chars().count(), MAX_DETAIL_CHARS);
        assert_eq!(long.conversation_id, None);
        assert!(problem(Check::SyncStalled, "", 0).bounded().is_none());
        assert!(
            problem(Check::SyncStalled, &"s".repeat(MAX_SUBJECT_BYTES + 1), 0)
                .bounded()
                .is_none()
        );
        let wire = json!([{"check":"job_stalled","subject":"c:t","since_ms":1,"detail":"d"}]);
        assert_eq!(
            serde_json::from_value::<Vec<Problem>>(wire).unwrap()[0].check,
            Check::JobStalled
        );
    }
}
