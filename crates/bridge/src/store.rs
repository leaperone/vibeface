use anyhow::{Context, Result, anyhow, bail};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::Value;
use std::path::Path;
use vibeface_protocol::{
    BridgeEvent, ChatMessage, Command, CommandRecord, CommandRequest, CommandStatus, TaskInput,
    TaskRecord, TaskStatus, TaskUpdate, now_ms,
};

pub struct Store {
    connection: Connection,
}

impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        let connection = Connection::open(path)
            .with_context(|| format!("open bridge store {}", path.display()))?;
        make_private(path)?;

        connection.pragma_update(None, "journal_mode", "WAL")?;
        connection.execute_batch(
            "CREATE TABLE IF NOT EXISTS metadata (
                 key TEXT PRIMARY KEY,
                 value TEXT NOT NULL
             );
             CREATE TABLE IF NOT EXISTS commands (
                 id TEXT PRIMARY KEY,
                 data TEXT NOT NULL
             );
             CREATE TABLE IF NOT EXISTS command_updates (
                 seq INTEGER PRIMARY KEY,
                 data TEXT NOT NULL
             );
             CREATE TABLE IF NOT EXISTS events (
                 seq INTEGER PRIMARY KEY,
                 data TEXT NOT NULL
             );
             CREATE TABLE IF NOT EXISTS agent_calls (
                 day INTEGER PRIMARY KEY,
                 calls INTEGER NOT NULL
             );
             CREATE TABLE IF NOT EXISTS tracked_threads (
                 thread_id TEXT PRIMARY KEY
             );
             CREATE TABLE IF NOT EXISTS tasks (id TEXT PRIMARY KEY, data TEXT NOT NULL);
             CREATE TABLE IF NOT EXISTS task_signals (task_id TEXT NOT NULL, signal TEXT NOT NULL, PRIMARY KEY(task_id,signal));
             CREATE TABLE IF NOT EXISTS chat_messages (seq INTEGER PRIMARY KEY, data TEXT NOT NULL);
             CREATE TABLE IF NOT EXISTS auto_turns (
                 thread_id TEXT NOT NULL,
                 turn_id TEXT NOT NULL,
                 PRIMARY KEY (thread_id, turn_id)
             );",
        )?;
        make_private_sidecars(path)?;
        Self { connection }.recover_pending()
    }

    fn recover_pending(mut self) -> Result<Self> {
        let transaction = self.connection.transaction()?;
        let mut query = transaction.prepare("SELECT id, data FROM commands")?;
        let mut records = Vec::new();
        {
            let rows = query.query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?;
            for row in rows {
                records.push(row?);
            }
        }
        drop(query);
        let mut uncertain = Vec::new();
        for (id, data) in records {
            let mut record: CommandRecord = serde_json::from_str(&data)
                .with_context(|| format!("invalid command record {id}"))?;
            if matches!(
                record.status,
                CommandStatus::Queued | CommandStatus::Running
            ) {
                record.status = CommandStatus::Unknown;
                record.updated_at_ms = now_ms();
                transaction.execute(
                    "UPDATE commands SET data=?2 WHERE id=?1",
                    params![id, serde_json::to_string(&record)?],
                )?;
                append_update(&transaction, &record)?;
            }
            if record.status == CommandStatus::Unknown {
                uncertain.push(record);
            }
        }
        let mut tasks = all_tasks(&transaction)?;
        for task in &mut tasks {
            if task.status == TaskStatus::Completed {
                continue;
            }
            if uncertain.iter().any(|record| {
                record.request.command.task_id() == Some(task.id.as_str())
                    || record
                        .request
                        .command
                        .thread_id()
                        .is_some_and(|thread| task.thread_ids.iter().any(|bound| bound == thread))
            }) {
                task.status = TaskStatus::Unknown;
                task.auto_continue = false;
                task.summary = Some("Bridge restarted during execution; inspect commands and Codex state before resuming".into());
                save_task(&transaction, task)?;
            }
        }
        transaction.commit()?;
        Ok(self)
    }

    pub fn accept(&mut self, request: &CommandRequest) -> Result<(CommandRecord, bool)> {
        request.validate().map_err(|error| anyhow!(error))?;
        if let Some(existing) = self.command(&request.id)? {
            if existing.request != *request {
                bail!("command id already exists with different content");
            }
            return Ok((existing, false));
        }

        let record = CommandRecord::new(request.clone(), CommandStatus::Queued);
        let transaction = self.connection.transaction()?;
        transaction.execute(
            "INSERT INTO commands (id, data) VALUES (?1, ?2)",
            params![record.request.id, serde_json::to_string(&record)?],
        )?;
        append_update(&transaction, &record)?;
        if let Command::Manage { prompt } = &request.command {
            append_chat(
                &transaction,
                &request.id,
                "user",
                Value::String(prompt.clone()),
            )?;
        }
        transaction.commit()?;
        Ok((record, true))
    }

    pub fn update(&mut self, record: &CommandRecord) -> Result<()> {
        let existing = self
            .command(&record.request.id)?
            .context("unknown command id")?;
        if existing.request != record.request {
            bail!("command content does not match dispatch");
        }
        if !existing.status.is_pending() && existing.status != record.status {
            bail!("terminal command cannot change status");
        }

        let mut saved = record.clone();
        saved.updated_at_ms = now_ms();
        let transaction = self.connection.transaction()?;
        transaction.execute(
            "UPDATE commands SET data=?2 WHERE id=?1",
            params![saved.request.id, serde_json::to_string(&saved)?],
        )?;
        append_update(&transaction, &saved)?;
        if saved.status == CommandStatus::Unknown && existing.status != CommandStatus::Unknown {
            append_event(
                &transaction,
                "notification",
                saved.request.command.thread_id().map(str::to_owned),
                None,
                serde_json::json!({"category":"command","reason":"unknown","command_id":saved.request.id,"task_id":saved.request.command.task_id(),"message":"Command outcome is unknown; inspect it before acknowledging or continuing"}),
            )?;
        }
        if saved.status == CommandStatus::Unknown && existing.status != CommandStatus::Unknown {
            for mut task in all_tasks(&transaction)? {
                if task.status == TaskStatus::Completed {
                    continue;
                }
                if saved.request.command.task_id() == Some(task.id.as_str())
                    || saved
                        .request
                        .command
                        .thread_id()
                        .is_some_and(|thread| task.thread_ids.iter().any(|bound| bound == thread))
                {
                    task.status = TaskStatus::Unknown;
                    task.auto_continue = false;
                    save_task(&transaction, &mut task)?;
                }
            }
        }
        if matches!(saved.request.command, Command::Manage { .. })
            && !matches!(saved.status, CommandStatus::Queued | CommandStatus::Running)
            && matches!(
                existing.status,
                CommandStatus::Queued | CommandStatus::Running
            )
        {
            let content = saved.output.clone().unwrap_or_else(
                || serde_json::json!({"error": saved.error, "status": saved.status}),
            );
            append_chat(&transaction, &saved.request.id, "assistant", content)?;
        }
        transaction.commit()?;
        Ok(())
    }

    pub fn command(&self, id: &str) -> Result<Option<CommandRecord>> {
        let data: Option<String> = self
            .connection
            .query_row("SELECT data FROM commands WHERE id=?1", [id], |row| {
                row.get(0)
            })
            .optional()?;
        data.map(|data| serde_json::from_str(&data).context("invalid command record"))
            .transpose()
    }

    pub fn updates_after(&self, seq: u64, limit: usize) -> Result<Vec<(u64, CommandRecord)>> {
        let seq = sqlite_integer(seq, "command update cursor")?;
        let limit = sqlite_limit(limit)?;
        let mut query = self
            .connection
            .prepare("SELECT seq, data FROM command_updates WHERE seq>?1 ORDER BY seq LIMIT ?2")?;
        let rows = query.query_map(params![seq, limit], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
        })?;
        let mut result = Vec::new();
        for row in rows {
            let (seq, data) = row?;
            result.push((
                u64::try_from(seq).context("negative command update sequence")?,
                serde_json::from_str(&data).context("invalid command update")?,
            ));
        }
        Ok(result)
    }

    pub fn push_event(
        &mut self,
        kind: &str,
        thread_id: Option<String>,
        turn_id: Option<String>,
        payload: Value,
    ) -> Result<BridgeEvent> {
        let transaction = self.connection.transaction()?;
        let event = append_event(
            &transaction,
            kind,
            thread_id.clone(),
            turn_id,
            payload.clone(),
        )?;
        if matches!(kind, "bridge.codex_connected" | "bridge.codex_disconnected") {
            let previous: Option<String> = transaction
                .query_row(
                    "SELECT value FROM metadata WHERE key='notified_codex_connection'",
                    [],
                    |row| row.get(0),
                )
                .optional()?;
            if previous.as_deref() != Some(kind) {
                transaction.execute("INSERT INTO metadata (key,value) VALUES ('notified_codex_connection',?1) ON CONFLICT(key) DO UPDATE SET value=excluded.value",[kind])?;
                append_event(
                    &transaction,
                    "notification",
                    None,
                    None,
                    serde_json::json!({"category":"connection","reason":kind,"source_event_seq":event.seq,"message":if kind=="bridge.codex_connected" {"Codex connected"} else {"Codex disconnected; task state requires reconciliation"}}),
                )?;
            }
        }
        if let Some(thread) = thread_id {
            let mut tasks = all_tasks(&transaction)?;
            for task in &mut tasks {
                if !task.thread_ids.contains(&thread) || task.status == TaskStatus::Completed {
                    continue;
                }
                let kind_lower = kind.to_ascii_lowercase();
                let status = if kind_lower.contains("request")
                    && (kind_lower.contains("approval")
                        || kind_lower.contains("userinput")
                        || kind_lower.contains("elicitation")
                        || kind == "bridge.codex_request")
                {
                    Some(TaskStatus::WaitingUser)
                } else if kind == "turn/started" {
                    Some(TaskStatus::Running)
                } else if kind == "turn/completed" {
                    match payload.pointer("/turn/status").and_then(Value::as_str) {
                        Some("failed") => Some(TaskStatus::Blocked),
                        Some("interrupted") => Some(TaskStatus::Paused),
                        Some("completed") => Some(TaskStatus::NeedsContinue),
                        _ => Some(TaskStatus::Unknown),
                    }
                } else if kind == "error"
                    && payload.get("willRetry").and_then(Value::as_bool) != Some(true)
                {
                    Some(TaskStatus::Blocked)
                } else if kind == "thread/goal/updated" {
                    match payload
                        .pointer("/goal/status")
                        .or_else(|| payload.get("status"))
                        .and_then(Value::as_str)
                    {
                        Some("complete") => Some(TaskStatus::AwaitingReview),
                        Some("blocked" | "usageLimited" | "budgetLimited") => {
                            Some(TaskStatus::Blocked)
                        }
                        Some("paused") => Some(TaskStatus::Paused),
                        _ => None,
                    }
                } else {
                    None
                };
                if let Some(status) = status {
                    let signal = if kind == "turn/completed" && event.turn_id.is_some() {
                        format!(
                            "{kind}:{thread}:{}",
                            event.turn_id.as_deref().unwrap_or_default()
                        )
                    } else {
                        format!("{kind}:{thread}:{}", serde_json::to_string(&payload)?)
                    };
                    let fresh = transaction.execute(
                        "INSERT OR IGNORE INTO task_signals (task_id,signal) VALUES (?1,?2)",
                        params![task.id, signal],
                    )? == 1;
                    if !fresh {
                        continue;
                    }
                    if status == TaskStatus::Unknown
                        || (task.status != TaskStatus::Paused
                            && task.status != TaskStatus::Unknown
                            && (!status.allows_auto() || task.status.allows_auto()))
                    {
                        task.status = status;
                    }
                    if matches!(
                        status,
                        TaskStatus::WaitingUser
                            | TaskStatus::Blocked
                            | TaskStatus::Paused
                            | TaskStatus::AwaitingReview
                            | TaskStatus::Unknown
                    ) {
                        task.auto_continue = false;
                    }
                    if kind != "turn/started" {
                        task.dirty_seq = event.seq;
                    }
                    save_task(&transaction, task)?;
                }
            }
        }
        transaction.commit()?;
        Ok(event)
    }

    pub fn events_after(&self, seq: u64, limit: usize) -> Result<Vec<BridgeEvent>> {
        let seq = sqlite_integer(seq, "event cursor")?;
        let limit = sqlite_limit(limit)?;
        let mut query = self
            .connection
            .prepare("SELECT data FROM events WHERE seq>?1 ORDER BY seq LIMIT ?2")?;
        let rows = query.query_map(params![seq, limit], |row| row.get::<_, String>(0))?;
        let mut events = Vec::new();
        for row in rows {
            events.push(serde_json::from_str(&row?).context("invalid bridge event")?);
        }
        Ok(events)
    }

    pub fn last_event_seq(&self) -> Result<u64> {
        let seq: i64 =
            self.connection
                .query_row("SELECT COALESCE(MAX(seq), 0) FROM events", [], |row| {
                    row.get(0)
                })?;
        u64::try_from(seq).context("negative persisted event sequence")
    }

    pub fn ack(&mut self, seq: u64) -> Result<()> {
        let last = self.last_event_seq()?;
        if seq > last {
            bail!("acknowledgement exceeds local event sequence");
        }
        let current = self.ack_cursor()?;
        if seq > current {
            self.set_metadata("ack_cursor", &seq.to_string())?;
        }
        Ok(())
    }

    pub fn ack_cursor(&self) -> Result<u64> {
        self.metadata_u64("ack_cursor")
    }

    pub fn align_ack(&mut self, seq: u64) -> Result<()> {
        if seq > self.last_event_seq()? {
            bail!("server acknowledgement exceeds local event sequence");
        }
        self.set_metadata("ack_cursor", &seq.to_string())
    }

    pub fn manager_session(&self) -> Result<Option<String>> {
        self.metadata("manager_session")
    }

    pub fn set_manager_session(&mut self, session: &str) -> Result<()> {
        self.set_metadata("manager_session", session)
    }

    pub fn auto_manager_session(&self) -> Result<Option<String>> {
        self.metadata("auto_manager_session")
    }

    pub fn set_auto_manager_session(&mut self, session: &str) -> Result<()> {
        self.set_metadata("auto_manager_session", session)
    }

    pub fn reserve_agent_call(&mut self, day: u64, limit: u32) -> Result<bool> {
        if limit == 0 {
            return Ok(false);
        }
        let day = sqlite_integer(day, "agent budget day")?;
        let transaction = self.connection.transaction()?;
        let changed = transaction.execute(
            "INSERT INTO agent_calls (day, calls) VALUES (?1, 1)
             ON CONFLICT(day) DO UPDATE SET calls=agent_calls.calls+1
             WHERE agent_calls.calls < ?2",
            params![day, i64::from(limit)],
        )? == 1;
        transaction.commit()?;
        Ok(changed)
    }

    pub fn agent_calls(&self, day: u64) -> Result<u32> {
        let day = sqlite_integer(day, "agent budget day")?;
        let calls: Option<i64> = self
            .connection
            .query_row("SELECT calls FROM agent_calls WHERE day=?1", [day], |row| {
                row.get(0)
            })
            .optional()?;
        calls
            .unwrap_or(0)
            .try_into()
            .context("agent call count is outside u32 range")
    }

    pub fn track_thread(&mut self, thread: &str) -> Result<()> {
        self.connection.execute(
            "INSERT OR IGNORE INTO tracked_threads (thread_id) VALUES (?1)",
            [thread],
        )?;
        Ok(())
    }

    pub fn tracked_thread(&self, thread: &str) -> Result<bool> {
        Ok(self
            .connection
            .query_row(
                "SELECT 1 FROM tracked_threads WHERE thread_id=?1",
                [thread],
                |_| Ok(()),
            )
            .optional()?
            .is_some())
    }

    pub fn tracked_threads(&self) -> Result<Vec<String>> {
        let mut query = self
            .connection
            .prepare("SELECT thread_id FROM tracked_threads ORDER BY thread_id")?;
        let rows = query.query_map([], |row| row.get::<_, String>(0))?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Into::into)
    }

    pub fn claim_auto_turn(&mut self, thread: &str, turn: &str) -> Result<bool> {
        self.connection.execute(
            "INSERT OR IGNORE INTO auto_turns (thread_id, turn_id) VALUES (?1, ?2)",
            params![thread, turn],
        )?;
        Ok(self.connection.changes() == 1)
    }

    pub fn task(&self, id: &str) -> Result<Option<TaskRecord>> {
        let data: Option<String> = self
            .connection
            .query_row("SELECT data FROM tasks WHERE id=?1", [id], |row| row.get(0))
            .optional()?;
        data.map(|data| serde_json::from_str(&data).context("invalid task record"))
            .transpose()
    }

    pub fn tasks(&self) -> Result<Vec<TaskRecord>> {
        all_tasks(&self.connection)
    }

    pub fn commands(&self) -> Result<Vec<CommandRecord>> {
        let mut query = self
            .connection
            .prepare("SELECT data FROM commands ORDER BY id")?;
        let rows = query.query_map([], |row| row.get::<_, String>(0))?;
        rows.map(|row| Ok(serde_json::from_str(&row?)?)).collect()
    }

    pub fn task_has_pending_commands(&self, task: &TaskRecord) -> Result<bool> {
        Ok(self.commands()?.iter().any(|record| {
            record.status.is_pending()
                && (record.request.command.task_id() == Some(task.id.as_str())
                    && !matches!(
                        record.request.command,
                        Command::ControlTask { .. }
                            | Command::UpdateTask { .. }
                            | Command::ReadTask { .. }
                    )
                    || record
                        .request
                        .command
                        .thread_id()
                        .is_some_and(|thread| task.thread_ids.iter().any(|id| id == thread)))
        }))
    }

    pub fn create_task(&mut self, input: TaskInput) -> Result<TaskRecord> {
        input.validate().map_err(anyhow::Error::msg)?;
        if self.task(&input.id)?.is_some() {
            bail!("task already exists");
        }
        let mut task = TaskRecord {
            id: input.id,
            goal: input.goal,
            cwd: input.cwd,
            acceptance_criteria: input.acceptance_criteria,
            thread_ids: vec![],
            status: TaskStatus::Queued,
            auto_continue: input.auto_continue,
            remaining_decisions: input.decision_limit,
            summary: None,
            evidence: None,
            dirty_seq: 0,
            handled_seq: 0,
            manager_session: None,
            updated_at_ms: now_ms(),
        };
        let transaction = self.connection.transaction()?;
        let event = append_event(
            &transaction,
            "task.created",
            None,
            None,
            serde_json::to_value(&task)?,
        )?;
        task.dirty_seq = event.seq;
        transaction.execute(
            "INSERT INTO tasks (id,data) VALUES (?1,?2)",
            params![task.id, serde_json::to_string(&task)?],
        )?;
        transaction.commit()?;
        Ok(task)
    }

    pub fn update_task(&mut self, id: &str, update: TaskUpdate) -> Result<TaskRecord> {
        update.validate().map_err(anyhow::Error::msg)?;
        let mut task = self.task(id)?.context("task not found")?;
        if task.status == TaskStatus::Completed {
            bail!("completed task is immutable");
        }
        if let Some(threads) = update.thread_ids {
            for other in self.tasks()? {
                if other.id != id
                    && threads
                        .iter()
                        .any(|thread| other.thread_ids.contains(thread))
                {
                    bail!("thread already belongs to another task");
                }
            }
            if task
                .thread_ids
                .iter()
                .any(|thread| !threads.contains(thread))
            {
                bail!("task thread bindings cannot be removed");
            }
            task.thread_ids = threads;
        }
        if let Some(status) = update.status {
            if !task.status.allows_auto() && status.allows_auto() {
                bail!("stopped task requires explicit user resume");
            }
            task.status = status;
            if !status.allows_auto() {
                task.auto_continue = false;
            }
        }
        if update.status == Some(TaskStatus::AwaitingReview)
            && update
                .evidence
                .as_deref()
                .or(task.evidence.as_deref())
                .is_none_or(|evidence| evidence.trim().is_empty())
        {
            bail!("awaiting_review requires acceptance evidence");
        }
        if update.summary.is_some() {
            task.summary = update.summary;
        }
        if update.evidence.is_some() {
            task.evidence = update.evidence;
        }
        self.save_task_event(task, "task.updated", false)
    }

    pub fn bind_task_thread(&mut self, id: &str, thread: &str) -> Result<()> {
        let mut task = self.task(id)?.context("task not found")?;
        if !task.thread_ids.iter().any(|bound| bound == thread) {
            for other in self.tasks()? {
                if other.id != id && other.thread_ids.iter().any(|bound| bound == thread) {
                    bail!("thread already belongs to another task");
                }
            }
            task.thread_ids.push(thread.into());
            self.save_task_event(task, "task.thread_bound", false)?;
        }
        self.track_thread(thread)
    }

    pub fn save_task_event(
        &mut self,
        mut task: TaskRecord,
        kind: &str,
        dirty: bool,
    ) -> Result<TaskRecord> {
        let transaction = self.connection.transaction()?;
        let event = append_event(&transaction, kind, None, None, serde_json::to_value(&task)?)?;
        if dirty {
            task.dirty_seq = event.seq;
        }
        save_task(&transaction, &mut task)?;
        transaction.commit()?;
        Ok(task)
    }

    pub fn set_task_session(&mut self, id: &str, session: &str) -> Result<()> {
        let mut task = self.task(id)?.context("task not found")?;
        task.manager_session = Some(session.into());
        self.connection.execute(
            "UPDATE tasks SET data=?2 WHERE id=?1",
            params![id, serde_json::to_string(&task)?],
        )?;
        Ok(())
    }

    pub fn claim_task(&mut self) -> Result<Option<(TaskRecord, CommandRecord)>> {
        if self.commands()?.iter().any(|record| {
            matches!(record.request.command, Command::Manage { .. })
                && matches!(
                    record.status,
                    CommandStatus::Queued | CommandStatus::Running
                )
        }) {
            return Ok(None);
        }
        let mut candidate = None;
        for task in self.tasks()? {
            if task.auto_continue
                && matches!(task.status, TaskStatus::Queued | TaskStatus::NeedsContinue)
                && task.dirty_seq > task.handled_seq
                && !self.task_has_pending_commands(&task)?
            {
                candidate = Some(task);
                break;
            }
        }
        let Some(mut task) = candidate else {
            return Ok(None);
        };
        if task.remaining_decisions == 0 {
            task.status = TaskStatus::Paused;
            task.auto_continue = false;
            task.summary =
                Some("automatic decision limit reached; explicitly resume with a new limit".into());
            self.save_task_event(task, "task.limit_reached", false)?;
            return Ok(None);
        }
        let request = CommandRequest {
            id: format!("task-{}", uuid::Uuid::new_v4()),
            command: Command::RunTask {
                task_id: task.id.clone(),
                captured_event_seq: task.dirty_seq,
            },
        };
        let record = CommandRecord::new(request, CommandStatus::Queued);
        task.remaining_decisions -= 1;
        task.status = TaskStatus::Running;
        let transaction = self.connection.transaction()?;
        save_task(&transaction, &mut task)?;
        transaction.execute(
            "INSERT INTO commands (id,data) VALUES (?1,?2)",
            params![record.request.id, serde_json::to_string(&record)?],
        )?;
        append_update(&transaction, &record)?;
        transaction.commit()?;
        Ok(Some((task, record)))
    }

    pub fn finish_task_run(
        &mut self,
        id: &str,
        captured: u64,
        status: CommandStatus,
        active: bool,
        summary: Option<String>,
    ) -> Result<()> {
        let mut task = self.task(id)?.context("task not found")?;
        if task.status == TaskStatus::Completed {
            return Ok(());
        }
        if status == CommandStatus::Succeeded {
            task.handled_seq = task.handled_seq.max(captured);
            if task.status == TaskStatus::Running {
                task.status = if active {
                    TaskStatus::Running
                } else {
                    TaskStatus::NeedsContinue
                };
            }
        } else {
            task.auto_continue = false;
            task.status = if status == CommandStatus::Unknown {
                TaskStatus::Unknown
            } else {
                TaskStatus::Blocked
            };
            task.summary = summary;
        }
        self.save_task_event(task, "task.decision_finished", false)?;
        Ok(())
    }

    pub fn chat_history(&self, after: u64, limit: usize) -> Result<Vec<ChatMessage>> {
        let mut query = self
            .connection
            .prepare("SELECT data FROM chat_messages WHERE seq>?1 ORDER BY seq LIMIT ?2")?;
        let rows = query.query_map(
            params![sqlite_integer(after, "chat cursor")?, sqlite_limit(limit)?],
            |row| row.get::<_, String>(0),
        )?;
        rows.map(|row| Ok(serde_json::from_str(&row?)?)).collect()
    }

    fn metadata(&self, key: &str) -> Result<Option<String>> {
        Ok(self
            .connection
            .query_row("SELECT value FROM metadata WHERE key=?1", [key], |row| {
                row.get(0)
            })
            .optional()?)
    }

    fn metadata_u64(&self, key: &str) -> Result<u64> {
        self.metadata(key)?
            .map(|value| value.parse().context("invalid metadata value"))
            .transpose()
            .map(|value| value.unwrap_or(0))
    }

    fn set_metadata(&mut self, key: &str, value: &str) -> Result<()> {
        self.connection.execute(
            "INSERT INTO metadata (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value=excluded.value",
            params![key, value],
        )?;
        Ok(())
    }
}

fn append_update(transaction: &rusqlite::Transaction<'_>, record: &CommandRecord) -> Result<()> {
    let previous: i64 = transaction.query_row(
        "SELECT COALESCE(MAX(seq), 0) FROM command_updates",
        [],
        |row| row.get(0),
    )?;
    let seq = previous
        .checked_add(1)
        .context("command update sequence exhausted")?;
    transaction.execute(
        "INSERT INTO command_updates (seq, data) VALUES (?1, ?2)",
        params![seq, serde_json::to_string(record)?],
    )?;
    append_event(
        transaction,
        "command.updated",
        record.request.command.thread_id().map(str::to_owned),
        None,
        serde_json::to_value(record)?,
    )?;
    Ok(())
}

fn append_event(
    connection: &Connection,
    kind: &str,
    thread_id: Option<String>,
    turn_id: Option<String>,
    payload: Value,
) -> Result<BridgeEvent> {
    let previous: i64 =
        connection.query_row("SELECT COALESCE(MAX(seq),0) FROM events", [], |row| {
            row.get(0)
        })?;
    let seq = previous
        .checked_add(1)
        .context("event sequence exhausted")?;
    let event = BridgeEvent {
        seq: u64::try_from(seq)?,
        at_ms: now_ms(),
        kind: kind.into(),
        thread_id,
        turn_id,
        payload,
    };
    connection.execute(
        "INSERT INTO events (seq,data) VALUES (?1,?2)",
        params![seq, serde_json::to_string(&event)?],
    )?;
    Ok(event)
}

fn all_tasks(connection: &Connection) -> Result<Vec<TaskRecord>> {
    let mut query = connection.prepare("SELECT data FROM tasks ORDER BY id")?;
    let rows = query.query_map([], |row| row.get::<_, String>(0))?;
    rows.map(|row| Ok(serde_json::from_str(&row?)?)).collect()
}

fn save_task(connection: &Connection, task: &mut TaskRecord) -> Result<()> {
    let previous: Option<String> = connection
        .query_row("SELECT data FROM tasks WHERE id=?1", [&task.id], |row| {
            row.get(0)
        })
        .optional()?;
    let previous = previous
        .map(|data| serde_json::from_str::<TaskRecord>(&data))
        .transpose()?;
    if previous
        .as_ref()
        .is_some_and(|previous| previous.status == TaskStatus::Completed)
        && task.status != TaskStatus::Completed
    {
        bail!("completed task is immutable");
    }
    task.updated_at_ms = now_ms();
    connection.execute(
        "UPDATE tasks SET data=?2 WHERE id=?1",
        params![task.id, serde_json::to_string(task)?],
    )?;
    if previous
        .as_ref()
        .is_none_or(|previous| previous.status != task.status)
        && matches!(
            task.status,
            TaskStatus::WaitingUser
                | TaskStatus::Blocked
                | TaskStatus::Paused
                | TaskStatus::Unknown
                | TaskStatus::AwaitingReview
                | TaskStatus::Completed
        )
    {
        let message = match task.status {
            TaskStatus::WaitingUser => "Task requires a human response",
            TaskStatus::Blocked => "Task is blocked; review the latest summary",
            TaskStatus::Paused => "Task is paused; no new automatic turns will be sent",
            TaskStatus::Unknown => {
                "Task execution is uncertain; inspect commands and session state"
            }
            TaskStatus::AwaitingReview => "Task is ready for acceptance review",
            TaskStatus::Completed => "Task was accepted as completed",
            _ => unreachable!(),
        };
        append_event(
            connection,
            "notification",
            None,
            None,
            serde_json::json!({"category":"task","task_id":task.id,"reason":task.status,"message":message}),
        )?;
    }
    Ok(())
}

fn append_chat(
    connection: &Connection,
    command_id: &str,
    role: &str,
    content: Value,
) -> Result<()> {
    let seq: i64 = connection.query_row(
        "SELECT COALESCE(MAX(seq),0)+1 FROM chat_messages",
        [],
        |row| row.get(0),
    )?;
    let message = ChatMessage {
        seq: u64::try_from(seq)?,
        command_id: command_id.into(),
        role: role.into(),
        content,
        at_ms: now_ms(),
    };
    connection.execute(
        "INSERT INTO chat_messages (seq,data) VALUES (?1,?2)",
        params![seq, serde_json::to_string(&message)?],
    )?;
    append_event(
        connection,
        "chat.message",
        None,
        None,
        serde_json::to_value(message)?,
    )?;
    Ok(())
}

fn sqlite_integer(value: u64, name: &str) -> Result<i64> {
    i64::try_from(value).with_context(|| format!("{name} is too large"))
}

fn sqlite_limit(value: usize) -> Result<i64> {
    i64::try_from(value).context("query limit is too large")
}

#[cfg(unix)]
fn make_private(path: &Path) -> Result<()> {
    if path != Path::new(":memory:") {
        use std::fs;
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

#[cfg(unix)]
fn make_private_sidecars(path: &Path) -> Result<()> {
    if path == Path::new(":memory:") {
        return Ok(());
    }
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    for suffix in ["-wal", "-shm"] {
        let sidecar = Path::new(&format!("{}{}", path.display(), suffix)).to_path_buf();
        if sidecar.exists() {
            fs::set_permissions(sidecar, fs::Permissions::from_mode(0o600))?;
        }
    }
    Ok(())
}

#[cfg(not(unix))]
fn make_private(_path: &Path) -> Result<()> {
    Ok(())
}

#[cfg(not(unix))]
fn make_private_sidecars(_path: &Path) -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};
    use vibeface_protocol::Command;

    fn request(id: &str, command: Command) -> CommandRequest {
        CommandRequest {
            id: id.into(),
            command,
        }
    }

    fn temp_path() -> PathBuf {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        std::env::temp_dir().join(format!(
            "vibeface-bridge-store-{}-{}.db",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ))
    }

    #[test]
    fn accept_is_idempotent_but_rejects_id_conflicts() {
        let mut store = Store::open(Path::new(":memory:")).unwrap();
        let first_request = request("cmd-1", Command::Snapshot);
        let (first, created) = store.accept(&first_request).unwrap();
        assert!(created);
        let (retry, created) = store.accept(&first_request).unwrap();
        assert!(!created);
        assert_eq!(retry, first);
        let changed = request("cmd-1", Command::StartThread { cwd: "/tmp".into() });
        assert!(store.accept(&changed).is_err());
    }

    #[test]
    fn queued_and_running_commands_recover_as_unknown() {
        let path = temp_path();
        {
            let mut store = Store::open(&path).unwrap();
            let queued = request("queued", Command::Snapshot);
            let running = request("running", Command::Snapshot);
            store.accept(&queued).unwrap();
            store.accept(&running).unwrap();
            let mut record = store.command("running").unwrap().unwrap();
            record.status = CommandStatus::Running;
            store.update(&record).unwrap();
        }
        let store = Store::open(&path).unwrap();
        assert_eq!(
            store.command("queued").unwrap().unwrap().status,
            CommandStatus::Unknown
        );
        assert_eq!(
            store.command("running").unwrap().unwrap().status,
            CommandStatus::Unknown
        );
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn event_ack_is_ordered_and_welcome_can_rewind_cursor() {
        let mut store = Store::open(Path::new(":memory:")).unwrap();
        let first = store
            .push_event("one", None, None, json!({"n": 1}))
            .unwrap();
        let second = store
            .push_event("two", None, None, json!({"n": 2}))
            .unwrap();
        assert_eq!(first.seq, 1);
        assert_eq!(second.seq, 2);
        store.ack(2).unwrap();
        store.ack(1).unwrap();
        assert_eq!(store.ack_cursor().unwrap(), 2);
        assert!(store.ack(3).is_err());
        store.align_ack(1).unwrap();
        assert_eq!(store.ack_cursor().unwrap(), 1);
        assert_eq!(store.events_after(0, 10).unwrap(), vec![first, second]);
    }

    #[test]
    fn budget_is_persistent_and_resets_by_day() {
        let path = temp_path();
        let day = 100;
        {
            let mut store = Store::open(&path).unwrap();
            assert!(store.reserve_agent_call(day, 2).unwrap());
            assert!(store.reserve_agent_call(day, 2).unwrap());
            assert!(!store.reserve_agent_call(day, 2).unwrap());
            assert_eq!(store.agent_calls(day).unwrap(), 2);
        }
        let mut store = Store::open(&path).unwrap();
        assert_eq!(store.agent_calls(day).unwrap(), 2);
        assert!(store.reserve_agent_call(day + 1, 2).unwrap());
        assert_eq!(store.agent_calls(day + 1).unwrap(), 1);
        std::fs::remove_file(path).unwrap();
    }
    fn task_input(id: &str) -> TaskInput {
        TaskInput {
            id: id.into(),
            goal: "finish isolated work".into(),
            cwd: "/tmp".into(),
            acceptance_criteria: vec!["result verified".into()],
            auto_continue: true,
            decision_limit: 3,
        }
    }

    #[test]
    fn task_capture_keeps_new_events_and_deduplicates_turn_signals() {
        let mut store = Store::open(Path::new(":memory:")).unwrap();
        store.create_task(task_input("task-a")).unwrap();
        store.bind_task_thread("task-a", "thread-a").unwrap();
        let (task, record) = store.claim_task().unwrap().unwrap();
        let captured = task.dirty_seq;
        let event = store
            .push_event(
                "turn/completed",
                Some("thread-a".into()),
                Some("turn-a".into()),
                json!({"turn":{"status":"completed"}}),
            )
            .unwrap();
        store
            .finish_task_run("task-a", captured, CommandStatus::Succeeded, false, None)
            .unwrap();
        store.update(&record.finish(Ok(json!({})))).unwrap();
        let task = store.task("task-a").unwrap().unwrap();
        assert_eq!(task.handled_seq, captured);
        assert_eq!(task.dirty_seq, event.seq);
        assert!(store.claim_task().unwrap().is_some());
        store
            .push_event(
                "turn/completed",
                Some("thread-a".into()),
                Some("turn-a".into()),
                json!({"turn":{"status":"completed"}}),
            )
            .unwrap();
        assert_eq!(store.task("task-a").unwrap().unwrap().dirty_seq, event.seq);
    }

    #[test]
    fn task_scope_is_exclusive_and_stopped_tasks_require_user_resume() {
        let mut store = Store::open(Path::new(":memory:")).unwrap();
        store.create_task(task_input("task-a")).unwrap();
        store.create_task(task_input("task-b")).unwrap();
        store.bind_task_thread("task-a", "thread-a").unwrap();
        assert!(store.bind_task_thread("task-b", "thread-a").is_err());
        store
            .update_task(
                "task-a",
                TaskUpdate {
                    status: Some(TaskStatus::Paused),
                    summary: None,
                    evidence: None,
                    thread_ids: None,
                },
            )
            .unwrap();
        assert!(
            store
                .update_task(
                    "task-a",
                    TaskUpdate {
                        status: Some(TaskStatus::Running),
                        summary: None,
                        evidence: None,
                        thread_ids: None
                    }
                )
                .is_err()
        );
        assert!(
            store
                .update_task(
                    "task-b",
                    TaskUpdate {
                        status: Some(TaskStatus::AwaitingReview),
                        summary: None,
                        evidence: None,
                        thread_ids: None
                    }
                )
                .is_err()
        );
    }

    #[test]
    fn recovered_task_claim_is_unknown_and_never_replayed() {
        let path = temp_path();
        {
            let mut store = Store::open(&path).unwrap();
            store.create_task(task_input("task-a")).unwrap();
            assert!(store.claim_task().unwrap().is_some());
        }
        let mut store = Store::open(&path).unwrap();
        let task = store.task("task-a").unwrap().unwrap();
        assert_eq!(task.status, TaskStatus::Unknown);
        assert!(!task.auto_continue);
        assert!(store.claim_task().unwrap().is_none());
        assert_eq!(store.commands().unwrap().len(), 1);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn rule_notifications_are_durable_and_do_not_repeat_unchanged_status() {
        let mut store = Store::open(Path::new(":memory:")).unwrap();
        store.create_task(task_input("task-a")).unwrap();
        let update = TaskUpdate {
            status: Some(TaskStatus::WaitingUser),
            summary: Some("approval required".into()),
            evidence: None,
            thread_ids: None,
        };
        store.update_task("task-a", update.clone()).unwrap();
        store.update_task("task-a", update).unwrap();
        store
            .push_event(
                "item/agentMessage/delta",
                None,
                None,
                json!({"delta":"hello"}),
            )
            .unwrap();
        let events = store.events_after(0, 100).unwrap();
        let notices: Vec<_> = events
            .iter()
            .filter(|event| event.kind == "notification")
            .collect();
        assert_eq!(notices.len(), 1);
        assert_eq!(notices[0].payload["reason"], "waiting_user");
    }
    #[test]
    fn active_turns_do_not_claim_more_decisions_and_ambiguous_completion_stops() {
        let mut store = Store::open(Path::new(":memory:")).unwrap();
        store.create_task(task_input("task-a")).unwrap();
        store.bind_task_thread("task-a", "thread-a").unwrap();
        let dirty = store.task("task-a").unwrap().unwrap().dirty_seq;
        store
            .push_event(
                "turn/started",
                Some("thread-a".into()),
                Some("turn-a".into()),
                json!({"turn":{"status":"inProgress"}}),
            )
            .unwrap();
        let task = store.task("task-a").unwrap().unwrap();
        assert_eq!(task.status, TaskStatus::Running);
        assert_eq!(task.dirty_seq, dirty);
        assert!(store.claim_task().unwrap().is_none());
        store
            .push_event(
                "turn/completed",
                Some("thread-a".into()),
                Some("turn-a".into()),
                json!({"turn":{}}),
            )
            .unwrap();
        let task = store.task("task-a").unwrap().unwrap();
        assert_eq!(task.status, TaskStatus::Unknown);
        assert!(!task.auto_continue);
        assert!(store.claim_task().unwrap().is_none());
    }

    #[test]
    fn elicitation_is_a_human_decision_and_never_auto_approved() {
        let mut store = Store::open(Path::new(":memory:")).unwrap();
        store.create_task(task_input("task-a")).unwrap();
        store.bind_task_thread("task-a", "thread-a").unwrap();
        store
            .push_event(
                "mcpServer/elicitation/request",
                Some("thread-a".into()),
                None,
                json!({"request_id":7}),
            )
            .unwrap();
        let task = store.task("task-a").unwrap().unwrap();
        assert_eq!(task.status, TaskStatus::WaitingUser);
        assert!(!task.auto_continue);
        assert!(store.claim_task().unwrap().is_none());
        assert!(
            store
                .events_after(0, 100)
                .unwrap()
                .iter()
                .any(|event| event.kind == "notification"
                    && event.payload["reason"] == "waiting_user")
        );
    }
    #[test]
    fn other_thread_progress_does_not_hide_pending_human_state() {
        let mut store = Store::open(Path::new(":memory:")).unwrap();
        store.create_task(task_input("task-a")).unwrap();
        store.bind_task_thread("task-a", "thread-a").unwrap();
        store.bind_task_thread("task-a", "thread-b").unwrap();
        store
            .push_event(
                "mcpServer/elicitation/request",
                Some("thread-a".into()),
                None,
                json!({"request_id":7}),
            )
            .unwrap();
        store
            .push_event(
                "turn/completed",
                Some("thread-b".into()),
                Some("turn-b".into()),
                json!({"turn":{"status":"completed"}}),
            )
            .unwrap();
        assert_eq!(
            store.task("task-a").unwrap().unwrap().status,
            TaskStatus::WaitingUser
        );
    }
    #[test]
    fn completed_tasks_never_regress_on_unknown_commands_or_restart() {
        let path = temp_path();
        {
            let mut store = Store::open(&path).unwrap();
            let mut task = store.create_task(task_input("task-a")).unwrap();
            task.status = TaskStatus::Completed;
            task.auto_continue = false;
            store
                .save_task_event(task, "task.controlled", false)
                .unwrap();
            let read = request(
                "read-a",
                Command::ReadTask {
                    task_id: "task-a".into(),
                },
            );
            store.accept(&read).unwrap();
        }
        let mut store = Store::open(&path).unwrap();
        assert_eq!(
            store.task("task-a").unwrap().unwrap().status,
            TaskStatus::Completed
        );
        assert_eq!(
            store.command("read-a").unwrap().unwrap().status,
            CommandStatus::Unknown
        );
        let read = request(
            "read-b",
            Command::ReadTask {
                task_id: "task-a".into(),
            },
        );
        let (mut record, _) = store.accept(&read).unwrap();
        record.status = CommandStatus::Unknown;
        store.update(&record).unwrap();
        assert_eq!(
            store.task("task-a").unwrap().unwrap().status,
            TaskStatus::Completed
        );
        let mut task = store.task("task-a").unwrap().unwrap();
        task.status = TaskStatus::Paused;
        assert!(
            store
                .save_task_event(task, "task.controlled", false)
                .is_err()
        );
        std::fs::remove_file(path).unwrap();
    }
}
