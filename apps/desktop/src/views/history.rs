//! Conversation history boundary and merge state. UI rendering never parses wire JSON.
use anyhow::{Context, Result, ensure};
use serde::Serialize;
use serde_json::{Map, Value};
use std::collections::HashSet;
use vibeface_protocol::history::{HistoryCoverage, HistoryPage, HistorySection, HISTORY_RESPONSE_BYTES};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HistoryScope {
    pub network_id: String,
    pub client_id: String,
    pub conversation_id: String,
    pub generation: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct RecordIdentity(pub Vec<String>);

/// Retains the complete server record, including attachment, provenance, and future fields.
#[derive(Clone, Debug)]
pub struct HistoryRecord {
    pub identity: RecordIdentity,
    pub rank: Option<String>,
    pub value: Value,
    revision: u64,
}

#[derive(Clone, Debug, Default)]
pub struct SectionState {
    pub rows: Vec<HistoryRecord>,
    pub coverage: Option<HistoryCoverage>,
    traversal: u64,
    replacement_revision: u64,
}

#[derive(Clone, Debug)]
pub struct SnapshotTicket {
    scope: HistoryScope,
    revision: u64,
    authority: Option<String>,
}

#[derive(Clone, Debug)]
pub struct PageTicket {
    scope: HistoryScope,
    authority: String,
    section: HistorySection,
    cursor: Option<String>,
    traversal: u64,
}
impl PageTicket {
    pub fn request(&self) -> vibeface_protocol::history::HistoryRequest {
        vibeface_protocol::history::HistoryRequest {
            conversation_id: self.scope.conversation_id.clone(),
            section: self.section,
            cursor: self.cursor.clone(),
            limit: vibeface_protocol::history::HISTORY_DEFAULT_LIMIT,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ApplyResult { Applied, AuthorityReset, Stale }

#[derive(Clone, Debug)]
pub struct HistoryState {
    scope: HistoryScope,
    authority: Option<String>,
    revision: u64,
    sections: Vec<(HistorySection, SectionState)>,
    base: Value,
}

fn text<'a>(value: &'a Value, key: &str) -> Result<&'a str> {
    let value = value.get(key).and_then(Value::as_str).context("历史记录缺少标识。")?;
    ensure!(!value.is_empty() && value.len() <= 16384, "历史记录标识无效。");
    Ok(value)
}
fn key(value: &Value, fields: &[&str]) -> Result<RecordIdentity> {
    Ok(RecordIdentity(fields.iter().map(|field| text(value, field).map(str::to_owned)).collect::<Result<_>>()?))
}
fn section_name(section: HistorySection) -> String {
    serde_json::to_value(section).expect("history section is a unit wire enum").as_str().unwrap().to_owned()
}
fn at_path<'a>(value: &'a Value, path: &[&str]) -> Option<&'a Value> {
    let mut current = value;
    for segment in path { current = current.as_object()?.get(*segment)?; }
    Some(current)
}
fn set_path(value: &mut Value, path: &[&str], replacement: Value) {
    let mut current = value;
    for segment in &path[..path.len() - 1] {
        if !current.is_object() { *current = Value::Object(Map::new()); }
        current = current.as_object_mut().unwrap().entry((*segment).to_owned()).or_insert_with(|| Value::Object(Map::new()));
    }
    if !current.is_object() { *current = Value::Object(Map::new()); }
    current.as_object_mut().unwrap().insert(path[path.len() - 1].to_owned(), replacement);
}
fn merge_object(target: &mut Value, incoming: &Value) {
    match (target, incoming) {
        (Value::Object(target), Value::Object(incoming)) => {
            for (key, value) in incoming {
                if let Some(old) = target.get_mut(key) { merge_object(old, value); }
                else { target.insert(key.clone(), value.clone()); }
            }
        }
        (target, incoming) => *target = incoming.clone(),
    }
}
fn validate_envelope(value: &Value, conversation: &str) -> Result<String> {
    ensure!(serde_json::to_vec(value)?.len() <= HISTORY_RESPONSE_BYTES, "历史响应超出协议范围。");
    ensure!(value.is_object(), "历史响应必须是对象。");
    let authority = text(value, "history_authority")?;
    let id = value.get("conversation_id").and_then(Value::as_str)
        .or_else(|| value.get("conversation").and_then(|value| value.get("conversation_id")).and_then(Value::as_str))
        .context("历史响应缺少对话标识。")?;
    ensure!(id == conversation, "历史响应属于另一个对话。");
    Ok(authority.to_owned())
}
fn validate_coverage(coverage: &HistoryCoverage) -> Result<()> {
    ensure!(coverage.complete == coverage.next_cursor.is_none(), "历史分页完成状态与游标不一致。");
    if let Some(cursor) = &coverage.next_cursor { ensure!(!cursor.is_empty() && cursor.len() <= 16384, "历史游标无效。"); }
    Ok(())
}
fn parse_rows(section: HistorySection, values: &[Value], conversation: &str) -> Result<Vec<HistoryRecord>> {
    let mut seen = HashSet::new();
    values.iter().map(|value| {
        if let Some(id) = value.get("conversation_id") { ensure!(id.as_str() == Some(conversation), "历史条目属于另一个对话。"); }
        let identity = match section {
            HistorySection::Messages => key(value, &["message_id"]),
            HistorySection::Works => key(value, &["work_id"]),
            HistorySection::Deliveries => {
                let mut id = key(value, &["work_id"])?;
                let state = text(value, "state")?;
                id.0.push(if state.starts_with("stop_") { "stop" } else { "dispatch" }.into());
                Ok(id)
            }
            HistorySection::Changes => key(value, &["change_id"]),
            HistorySection::Questions => key(value, &["question_id"]),
            HistorySection::Sessions => key(value, &["node", "session_id"]),
            HistorySection::Jobs | HistorySection::ScopeJobs | HistorySection::OwnerJobs | HistorySection::OwnerScopeJobs => key(value, &["trigger"]),
            HistorySection::Checks => key(value, &["check_id"]),
            HistorySection::Unanswered => {
                let id = value.as_str().context("未回复消息标识无效。")?;
                ensure!(!id.is_empty(), "未回复消息标识为空。");
                Ok(RecordIdentity(vec![id.to_owned()]))
            }
            HistorySection::Skills => key(value, &["skill_id"]),
            HistorySection::Schedules => key(value, &["schedule_id"]),
            HistorySection::Memory => key(value, &["memory_id"]),
            HistorySection::MemoryConflicts | HistorySection::OwnerMemoryConflicts => key(value, &["history_id"]),
            HistorySection::MemorySyncConflicts => key(value, &["memory_id", "losing_revision"]),
            HistorySection::Overwritten => {
                let mut id = key(value, &["change_id", "field"])?;
                id.0.push(value.get("provider").and_then(Value::as_str).unwrap_or("").to_owned());
                Ok(id)
            }
        }?;
        ensure!(seen.insert(identity.clone()), "历史页面包含重复标识，无法安全合并。");
        let rank = match value.get("rank") {
            None | Some(Value::Null) => None,
            Some(Value::String(rank)) if !rank.is_empty() => Some(rank.clone()),
            _ => anyhow::bail!("历史记录排序字段无效。"),
        };
        Ok(HistoryRecord { identity, rank, value: value.clone(), revision: 0 })
    }).collect()
}
fn sort_rows(section: HistorySection, rows: &mut [HistoryRecord]) {
    if matches!(section, HistorySection::Skills | HistorySection::Schedules) {
        rows.sort_by(|a, b| {
            let name = |row: &HistoryRecord| row.value.get("name").and_then(Value::as_str).unwrap_or("").to_lowercase();
            name(a).cmp(&name(b)).then_with(|| a.identity.0.cmp(&b.identity.0))
        });
    } else if matches!(section, HistorySection::Messages | HistorySection::Sessions) && rows.iter().all(|row| row.rank.is_some()) {
        rows.sort_by(|a, b| a.rank.cmp(&b.rank).then_with(|| a.identity.0.cmp(&b.identity.0)));
    }
}

impl HistoryState {
    pub fn new(scope: HistoryScope) -> Self {
        Self { scope, authority: None, revision: 0, sections: HistorySection::ALL.into_iter().map(|section| (section, SectionState::default())).collect(), base: Value::Object(Map::new()) }
    }
    /// Retain valid loaded rows across a transport generation while fencing its old requests.
    pub fn scope(&self) -> &HistoryScope { &self.scope }
    #[cfg(test)]
    pub fn authority(&self) -> Option<&str> { self.authority.as_deref() }
    pub fn section(&self, section: HistorySection) -> &SectionState {
        &self.sections.iter().find(|(candidate, _)| *candidate == section).expect("all protocol sections are initialized").1
    }
    fn section_mut(&mut self, section: HistorySection) -> &mut SectionState {
        &mut self.sections.iter_mut().find(|(candidate, _)| *candidate == section).expect("all protocol sections are initialized").1
    }
    pub fn begin_snapshot(&self) -> SnapshotTicket { SnapshotTicket { scope: self.scope.clone(), revision: self.revision, authority: self.authority.clone() } }
    pub fn begin_page(&self, section: HistorySection) -> Result<PageTicket> {
        let state = self.section(section);
        let coverage = state.coverage.as_ref().context("该历史分区尚未返回覆盖信息。")?;
        ensure!(!coverage.complete, "该历史分区已全部加载。");
        Ok(PageTicket { scope: self.scope.clone(), authority: self.authority.clone().context("对话尚未首读。")?, section, cursor: coverage.next_cursor.clone(), traversal: state.traversal })
    }
    fn reset_authority(&mut self, authority: String) -> bool {
        let changed = self.authority.as_ref().is_some_and(|old| old != &authority);
        if changed {
            for (_, section) in &mut self.sections { *section = SectionState::default(); }
            self.base = Value::Object(Map::new());
        }
        self.authority = Some(authority);
        changed
    }
    pub fn apply_snapshot(&mut self, ticket: &SnapshotTicket, value: Value) -> Result<ApplyResult> {
        if ticket.scope != self.scope || (ticket.authority != self.authority && ticket.revision != self.revision) { return Ok(ApplyResult::Stale); }
        let authority = validate_envelope(&value, &self.scope.conversation_id)?;
        let mut parsed = Vec::new();
        if let Some(history) = value.get("history") {
            let history = history.as_object().context("历史覆盖信息必须是对象。")?;
            for name in history.keys() {
                serde_json::from_value::<HistorySection>(Value::String(name.clone()))
                    .context("历史覆盖包含未知分区。")?;
            }
        }
        for section in HistorySection::ALL {
            let raw = at_path(&value, section.snapshot_path()).filter(|raw| !raw.is_null());
            let raw_coverage = value.get("history").and_then(|history| history.get(&section_name(section)));
            if raw.is_none() && raw_coverage.is_none() { continue; }
            let rows = if let Some(raw) = raw {
                parse_rows(section, raw.as_array().context("历史分区不是数组。")?, &self.scope.conversation_id)?
            } else { vec![] };
            let coverage: HistoryCoverage = serde_json::from_value(raw_coverage.cloned().context("历史分区缺少覆盖信息。")?)?;
            validate_coverage(&coverage)?;
            parsed.push((section, rows, coverage));
        }
        let reset = self.reset_authority(authority);
        self.revision += 1;
        let revision = self.revision;
        // A snapshot started before a push cannot replace newer scalar metadata either.
        if reset || ticket.revision == revision - 1 { self.base = value; }
        for (section, incoming, coverage) in parsed {
            let state = self.section_mut(section);
            if state.replacement_revision > ticket.revision { continue; }
            let replace = coverage.complete && state.rows.iter().all(|row| row.revision <= ticket.revision);
            let mut rows = if replace { vec![] } else { state.rows.clone() };
            for mut row in incoming {
                row.revision = revision;
                if let Some(old) = rows.iter_mut().find(|old| old.identity == row.identity) {
                    if old.revision <= ticket.revision { *old = row; }
                } else { rows.push(row); }
            }
            sort_rows(section, &mut rows);
            state.rows = rows;
            // A complete authoritative replacement also fences concurrent earlier snapshots.
            if replace { state.replacement_revision = revision; state.traversal += 1; }
            // Preserve a continuation already advanced by loading older pages.
            if replace || state.coverage.is_none() || state.traversal == 0 { state.coverage = Some(coverage); }
        }
        Ok(if reset { ApplyResult::AuthorityReset } else { ApplyResult::Applied })
    }
    pub fn apply_push(&mut self, value: Value) -> Result<ApplyResult> {
        let authority = validate_envelope(&value, &self.scope.conversation_id)?;
        let mut parsed = Vec::new();
        for section in HistorySection::ALL {
            let path = section.snapshot_path();
            // Exact records patches place scheduler and memory fields at the top level.
            let top_level_patch = match section {
                HistorySection::Jobs => value.get("jobs"),
                HistorySection::ScopeJobs => value.get("scope_jobs"),
                HistorySection::Memory => value.get("memory"),
                HistorySection::MemoryConflicts => value.get("memory_conflicts"),
                HistorySection::MemorySyncConflicts => value.get("memory_sync_conflicts").and_then(|value| value.get("items")),
                _ => None,
            };
            let raw = at_path(&value, path).or(top_level_patch);
            let Some(raw) = raw else { continue; };
            parsed.push((section, parse_rows(section, raw.as_array().context("推送历史分区不是数组。")?, &self.scope.conversation_id)?));
        }
        let reset = self.reset_authority(authority);
        self.revision += 1;
        let revision = self.revision;
        merge_object(&mut self.base, &value);
        for (section, incoming) in parsed {
            let state = self.section_mut(section);
            if incoming.is_empty() {
                state.rows.clear(); state.traversal += 1; state.coverage = None;
                state.replacement_revision = revision;
                continue;
            }
            for mut row in incoming {
                row.revision = revision;
                if let Some(old) = state.rows.iter_mut().find(|old| old.identity == row.identity) { *old = row; }
                else { state.rows.push(row); }
            }
            sort_rows(section, &mut state.rows);
        }
        Ok(if reset { ApplyResult::AuthorityReset } else { ApplyResult::Applied })
    }
    pub fn apply_page(&mut self, ticket: &PageTicket, value: Value) -> Result<ApplyResult> {
        if ticket.scope != self.scope || self.authority.as_ref() != Some(&ticket.authority) { return Ok(ApplyResult::Stale); }
        let authority = validate_envelope(&value, &self.scope.conversation_id)?;
        if authority != ticket.authority { return Ok(ApplyResult::Stale); }
        let page: HistoryPage<Value> = serde_json::from_value(value).context("历史分页响应无效。")?;
        ensure!(page.section == ticket.section, "历史分页属于另一个分区。");
        let coverage = page.coverage(); validate_coverage(&coverage)?;
        ensure!(coverage.next_cursor != ticket.cursor || coverage.complete, "历史分页游标没有推进。");
        let mut incoming = parse_rows(page.section, &page.items, &self.scope.conversation_id)?;
        let state = self.section_mut(page.section);
        if state.traversal != ticket.traversal || state.coverage.as_ref().and_then(|coverage| coverage.next_cursor.as_ref()) != ticket.cursor.as_ref() { return Ok(ApplyResult::Stale); }
        incoming.retain(|row| !state.rows.iter().any(|shown| shown.identity == row.identity));
        incoming.append(&mut state.rows);
        sort_rows(page.section, &mut incoming);
        state.rows = incoming;
        state.coverage = Some(coverage);
        state.traversal += 1;
        Ok(ApplyResult::Applied)
    }
    /// Explicit cursor invalidation clears projection rows before the caller refreshes.
    pub fn invalidate(&mut self) {
        self.authority = None; self.revision += 1; self.base = Value::Object(Map::new());
        for (_, section) in &mut self.sections { *section = SectionState::default(); }
    }
    pub fn materialize(&self) -> Value {
        let mut result = self.base.clone();
        for (section, state) in &self.sections {
            let values = Value::Array(state.rows.iter().map(|row| row.value.clone()).collect());
            let absent_owner = matches!(section,HistorySection::OwnerJobs|HistorySection::OwnerScopeJobs|HistorySection::OwnerMemoryConflicts)
                && state.rows.is_empty() && at_path(&result,&["scheduler","remote"]).is_none_or(Value::is_null);
            if *section == HistorySection::MemorySyncConflicts {
                set_path(&mut result, section.snapshot_path(), values);
                let path = section.snapshot_path();
                let mut more_path = path[..path.len() - 1].to_vec();
                more_path.push("has_more");
                set_path(&mut result, &more_path, Value::Bool(state.coverage.as_ref().is_some_and(|coverage| !coverage.complete)));
            } else if !absent_owner && (!state.rows.is_empty() || state.coverage.is_some() || at_path(&result, section.snapshot_path()).is_some_and(Value::is_array)) {
                set_path(&mut result, section.snapshot_path(), values);
            }
            if let Some(coverage) = &state.coverage {
                set_path(&mut result, &["history", &section_name(*section)], serde_json::to_value(coverage).expect("coverage contains only wire fields"));
            }
        }
        if let Some(authority) = &self.authority { result["history_authority"] = Value::String(authority.clone()); }
        result
    }
}

/// Complete typed Bot change envelope. Variant payloads remain intact for a dedicated native
/// detail view; a renderer must not substitute a generic "changed" card for these fields.
#[derive(Clone, Debug, serde::Deserialize, Serialize)]
pub struct BotChangeRecord {
    pub change_id: String,
    pub conversation_id: String,
    pub cause: Option<ChangeCause>,
    pub event_hash: String,
    pub created_at_ms: i64,
    pub change: BotChange,
}
#[derive(Clone, Debug, serde::Deserialize, Serialize)]
pub struct ChangeCause { pub conversation_id: String, pub message_id: String }
#[derive(Clone, Debug, serde::Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum BotChange {
    Profile {
        name: Option<FieldEdit<String>>,
        instructions: Option<FieldEdit<String>>,
        model: Option<ProviderEdit<String>>,
        reasoning_effort: Option<ProviderEdit<Option<String>>>,
        skills: Option<FieldEdit<Option<Vec<String>>>>,
        avatar: Option<FieldEdit<Option<ChangeAvatar>>>,
        #[serde(flatten)] fields: Map<String, Value>,
    },
    Skill { skill_id: String, before: Option<SkillBody>, after: Option<SkillBody>, #[serde(flatten)] fields: Map<String, Value> },
    Schedule { schedule_id: String, before: Option<ScheduleBody>, after: Option<ScheduleBody>, #[serde(flatten)] fields: Map<String, Value> },
    Memory { memory_id: String, before: Option<String>, after: Option<String>, base: Value, origin: String, essential: bool, #[serde(flatten)] fields: Map<String, Value> },
}
#[derive(Clone, Debug, serde::Deserialize, Serialize)]
pub struct FieldEdit<T> { pub before: T, pub after: T }
#[derive(Clone, Debug, serde::Deserialize, Serialize)]
pub struct ProviderEdit<T> { pub provider: ChangeProvider, pub before: T, pub after: T }
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ChangeProvider { Codex, Claude, Cursor, Opencode, Pi }
#[derive(Clone, Debug, serde::Deserialize, Serialize)]
pub struct ChangeAvatar { pub outline: String, pub eyes: String, pub hue: u16 }
#[derive(Clone, Debug, serde::Deserialize, Serialize)]
pub struct SkillBody { pub name: String, pub description: String, pub body: String, #[serde(flatten)] pub fields: Map<String, Value> }
#[derive(Clone, Debug, serde::Deserialize, Serialize)]
pub struct ScheduleBody {
    pub name: String, pub prompt: String, pub first_run_at_ms: i64,
    pub every_minutes: Option<u32>, pub enabled: bool, #[serde(default)] pub depth: u8,
    #[serde(flatten)] pub fields: Map<String, Value>,
}
#[derive(Clone, Debug, serde::Deserialize, Serialize)]
pub struct OverwrittenRecord {
    pub change_id: String, pub by: String,
    #[serde(flatten)] pub field: OverwrittenField,
}
#[derive(Clone, Debug, serde::Deserialize, Serialize)]
#[serde(tag = "field", rename_all = "snake_case")]
pub enum OverwrittenField {
    Name { value: String },
    Instructions { value: String },
    Model { provider: ChangeProvider, value: String },
    ReasoningEffort { provider: ChangeProvider, value: Option<String> },
    Skills { value: Option<Vec<String>> },
    Avatar { value: Option<ChangeAvatar> },
}
impl OverwrittenRecord {
    /// `value` is the current winner. Restoration uses the matching original change's after,
    /// never the current winner and never an inferred value when the old change is unloaded.
    pub fn original_after(&self, original: &BotChangeRecord) -> Result<Value> {
        ensure!(self.change_id == original.change_id, "尚未加载原始变更，请继续加载变更历史。");
        let BotChange::Profile { name, instructions, model, reasoning_effort, skills, avatar, .. } = &original.change else {
            anyhow::bail!("原始变更没有对应的配置字段。");
        };
        Ok(match &self.field {
            OverwrittenField::Name { .. } => serde_json::to_value(&name.as_ref().context("原始变更没有名称。")?.after)?,
            OverwrittenField::Instructions { .. } => serde_json::to_value(&instructions.as_ref().context("原始变更没有指令。")?.after)?,
            OverwrittenField::Model { provider, .. } => {
                let edit = model.as_ref().context("原始变更没有模型。")?;
                ensure!(&edit.provider == provider, "原始变更属于另一个 Provider。");
                serde_json::to_value(&edit.after)?
            }
            OverwrittenField::ReasoningEffort { provider, .. } => {
                let edit = reasoning_effort.as_ref().context("原始变更没有推理强度。")?;
                ensure!(&edit.provider == provider, "原始变更属于另一个 Provider。");
                serde_json::to_value(&edit.after)?
            }
            OverwrittenField::Skills { .. } => serde_json::to_value(&skills.as_ref().context("原始变更没有技能。")?.after)?,
            OverwrittenField::Avatar { .. } => serde_json::to_value(&avatar.as_ref().context("原始变更没有头像。")?.after)?,
        })
    }
}

#[derive(Clone, Debug)]
pub enum ProfileRestoration { Avatar(Option<ChangeAvatar>), Instruction(String) }
impl OverwrittenRecord {
    pub fn restoration(&self,original:&BotChangeRecord)->Result<ProfileRestoration>{
        let original=self.original_after(original)?;
        Ok(match &self.field{
            OverwrittenField::Avatar{..}=>ProfileRestoration::Avatar(serde_json::from_value(original)?),
            OverwrittenField::Name{..}=>ProfileRestoration::Instruction(format!("请仅把名称改回「{}」，保留其他配置。",serde_json::from_value::<String>(original)?)),
            OverwrittenField::Instructions{..}=>ProfileRestoration::Instruction(format!("请仅把说明改回下面这段，保留其他配置：\n\n{}",serde_json::from_value::<String>(original)?)),
            OverwrittenField::Model{provider,..}=>{let value=serde_json::from_value::<String>(original)?;ProfileRestoration::Instruction(format!("请仅把 {provider:?} 模型改回{}，保留其他配置。",if value.is_empty(){"设备默认".into()}else{format!("「{value}」")}))},
            OverwrittenField::ReasoningEffort{provider,..}=>{let value=serde_json::from_value::<Option<String>>(original)?;ProfileRestoration::Instruction(format!("请仅把 {provider:?} 推理强度改回{}，保留其他配置。",value.map(|value|format!("「{value}」")).unwrap_or_else(||"设备默认".into())))},
            OverwrittenField::Skills{..}=>{let value=serde_json::from_value::<Option<Vec<String>>>(original)?;ProfileRestoration::Instruction(format!("请仅把技能范围改回{}，保留其他配置。",value.map(|items|if items.is_empty(){"不启用技能".into()}else{items.join("、")}).unwrap_or_else(||"全部可用".into())))},
        })
    }
}
