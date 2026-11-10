//! Bounded reads retain the authoritative scope resolver for every historical card.
use super::{conversation_history::{HistoryBoundary, history_page}, *};
use vibeface_protocol::history::{
    HISTORY_MAX_LIMIT, HISTORY_PAGE_BYTES, HistoryPage, HistorySection,
};

impl Store {
    /// Coverage counts do not need the outgoing message text or its attachment metadata.
    pub fn sent_bot_message_count(&self, network: &str, conversation: &str) -> Result<usize> {
        Ok(self.connection.query_row(
            "SELECT count(*) FROM messages WHERE network_id=?1 AND role='bot' AND json_extract(sender,'$.conversation_id')=?2",
            params![network, conversation], |row| row.get(0),
        )?)
    }

    pub fn conversation_works_page(
        &self,
        network: &str,
        conversation: &str,
        cursor: Option<&str>,
        limit: usize,
    ) -> Result<HistoryPage<WorkProjection>> {
        self.conversation_section_page(
            network, conversation, HistorySection::Works, cursor, limit,
            |id| {
                let work = self.work(network, id)?.context("work projection disappeared")?;
                Ok(scope_projection::work_is_main_from(&self.connection, network, conversation, id)?
                    .then_some(work))
            },
        )
    }

    pub fn conversation_changes_page(
        &self,
        network: &str,
        conversation: &str,
        cursor: Option<&str>,
        limit: usize,
    ) -> Result<HistoryPage<ChangeRecord>> {
        self.conversation_section_page(
            network, conversation, HistorySection::Changes, cursor, limit,
            |id| {
                let change = self.bot_change(network, id)?.context("change projection disappeared")?;
                let main = match &change.cause {
                    Some(cause) => scope_projection::message_is_main_from(
                        &self.connection, network, &cause.conversation_id, &cause.message_id,
                    )?,
                    None => false,
                };
                Ok(main.then_some(change))
            },
        )
    }

    pub fn conversation_questions_page(
        &self,
        network: &str,
        conversation: &str,
        cursor: Option<&str>,
        limit: usize,
    ) -> Result<HistoryPage<Value>> {
        let bot = self.bot_for_conversation(network, conversation)?;
        self.conversation_section_page(
            network, conversation, HistorySection::Questions, cursor, limit,
            |id| {
                let question = self.question(network, id)?.context("question projection disappeared")?;
                let main = question["target"]["kind"] == "bot_main" && bot.as_ref()
                    .is_some_and(|bot| question["target"]["bot_id"] == bot.bot_id);
                Ok(main.then_some(question))
            },
        )
    }

    pub fn conversation_sessions_page(
        &self,
        network: &str,
        conversation: &str,
        cursor: Option<&str>,
        limit: usize,
    ) -> Result<HistoryPage<Value>> {
        self.conversation_section_page(
            network, conversation, HistorySection::Sessions, cursor, limit,
            |id| {
                let (node, session) = id.split_once('\0').context("invalid session history boundary")?;
                let (attribution, muted, at, rank): (String, bool, i64, String) = self.connection.query_row(
                    "SELECT attribution,muted,created_at_ms,rank FROM session_owners WHERE network_id=? AND node=? AND session_id=? AND conversation_id=?",
                    params![network, node, session, conversation],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
                )?;
                let mut value: Value = serde_json::from_str(&attribution)?;
                value["conversation_id"] = serde_json::json!(conversation);
                value["node"] = serde_json::json!(node);
                value["session_id"] = serde_json::json!(session);
                value["muted"] = serde_json::json!(muted);
                value["created_at_ms"] = serde_json::json!(at);
                value["version"] = serde_json::json!(super::version(&rank));
                value["rank"] = serde_json::json!(rank);
                Ok(Some(value))
            },
        )
    }

    /// The candidate limit also bounds pages containing only task/routine/unknown rows.
    /// Such an empty page still advances, so older main records remain retrievable.
    fn conversation_section_page<T: Serialize>(
        &self,
        network: &str,
        conversation: &str,
        section: HistorySection,
        cursor: Option<&str>,
        limit: usize,
        mut project: impl FnMut(&str) -> Result<Option<T>>,
    ) -> Result<HistoryPage<T>> {
        ensure!((1..=HISTORY_MAX_LIMIT).contains(&limit), "history limit must be 1 to 100");
        let (authority, before) = self.history_cursor(network, conversation, section, cursor)?;
        let (selection, rank_column, id_columns) = match section {
            HistorySection::Works => (
                "SELECT dispatch_rank,work_id FROM works WHERE network_id=?1 AND conversation_id=?2 AND dispatch IS NOT NULL",
                "dispatch_rank", "work_id",
            ),
            HistorySection::Changes => (
                "SELECT rank,change_id FROM bot_changes WHERE network_id=?1 AND conversation_id=?2",
                "rank", "change_id",
            ),
            HistorySection::Questions => (
                "SELECT rank,question_id FROM questions WHERE network_id=?1 AND conversation_id=?2",
                "rank", "question_id",
            ),
            HistorySection::Sessions => (
                "SELECT rank,node,session_id FROM session_owners WHERE network_id=?1 AND conversation_id=?2 AND attribution IS NOT NULL",
                "rank", "node,session_id",
            ),
            _ => bail!("history section uses its own typed reader"),
        };
        let sessions = section == HistorySection::Sessions;
        let (id, second_id) = match before.as_ref() {
            Some(boundary) if sessions => {
                let (node, session) = boundary.id.split_once('\0').context("invalid session history boundary")?;
                (Some(node), Some(session))
            }
            boundary => (boundary.map(|b| b.id.as_str()), None),
        };
        // Separate statements keep the keyset comparison usable as an index seek.
        let seek = if before.is_some() {
            if sessions { " AND (rank,node,session_id)<(?3,?4,?5)".to_owned() }
            else { format!(" AND ({rank_column},{id_columns})<(?3,?4)") }
        } else { String::new() };
        let ordering = if sessions { "rank DESC,node DESC,session_id DESC".to_owned() }
            else { format!("{rank_column} DESC,{id_columns} DESC") };
        let sql = format!("{selection}{seek} ORDER BY {ordering} LIMIT ?6");
        let mut statement = self.connection.prepare(&sql)?;
        let mut rows = statement.query(params![network, conversation,
            before.as_ref().map(|b| b.rank.as_str()), id, second_id, limit + 1])?;
        let mut boundary = before.clone();
        let mut items = Vec::new();
        let mut bytes = 0;
        let mut scanned = 0;
        let mut has_more = false;
        while let Some(row) = rows.next()? {
            if scanned == limit {
                has_more = true;
                break;
            }
            let rank = row.get(0)?;
            let mut id: String = row.get(1)?;
            if sessions {
                id.push('\0');
                id.push_str(&row.get::<_, String>(2)?);
            }
            let item = project(&id)?;
            if let Some(item) = &item {
                let size = serde_json::to_vec(item)?.len() + 1;
                if !items.is_empty() && bytes + size > HISTORY_PAGE_BYTES {
                    has_more = true;
                    break;
                }
                bytes += size;
            }
            boundary = Some(HistoryBoundary { rank, id });
            scanned += 1;
            if let Some(item) = item { items.push(item); }
        }
        items.reverse();
        history_page(network, conversation, section, authority, boundary, items, has_more)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{providers::Provider, workspace::InputTarget};

    struct Fixture {
        store: Store,
        identity: LocalIdentity,
        network: String,
    }

    fn fixture() -> Result<Fixture> {
        let identity = LocalIdentity::generate();
        let network = identity.fixture_network_id()?;
        let node = identity.device_id()?;
        let state = identity.sign_network_fixture(1, vec![node.clone()])?;
        let mut store = Store::open(":memory:")?;
        store.install_trust_bundle(&network, &state)?;
        let owner = identity.owner_fixture(&network, "c", &node)?;
        store.create_conversation(&identity, &network, "c", "sections", owner, 1)?;
        let bot = identity.fixture_admin().attest(&network, "c", EventBody::BotCreated {
            bot_id: "bot".into(), name: "Helper".into(), instructions: "brief".into(),
            project: None, attestation: None, cause: None,
        })?;
        store.append_event(&identity, &network, "c", bot, vec![], 2)?;
        Ok(Fixture { store, identity, network })
    }

    fn add_session(f: &mut Fixture, id: &str, summary: &str, at: i64) -> Result<()> {
        f.store.append_event(&f.identity, &f.network, "c", EventBody::SessionAttributed {
            node: f.identity.device_id()?, agent: Provider::Codex, session_id: id.into(),
            cwd: "/w".into(), summary: summary.into(),
        }, vec![], at)?;
        Ok(())
    }

    fn all_pages<T>(mut read: impl FnMut(Option<&str>) -> Result<HistoryPage<T>>) -> Result<Vec<T>> {
        let mut cursor = None;
        let mut all = Vec::new();
        loop {
            let page = read(cursor.as_deref())?;
            let mut items = page.items;
            items.append(&mut all);
            all = items;
            if page.complete { return Ok(all); }
            cursor = Some(page.next_cursor.context("incomplete page needs cursor")?);
        }
    }

    #[test]
    fn signed_sections_reassemble_the_existing_projection_in_order() -> Result<()> {
        let mut f = fixture()?;
        let node = f.identity.device_id()?;
        for i in 0..6 {
            let message_id = format!("m{i}");
            let input = f.identity.fixture_admin().attest(&f.network, "c", EventBody::InputPosted {
                message_id: message_id.clone(), target: InputTarget::BotMain { bot_id: "bot".into() },
                intent: crate::workspace::InputIntent::Continue, sequence: 1, expected_revision: None,
                text: format!("request {i}"), attachments: vec![], report_results: Some(true), attestation: None,
            })?;
            f.store.append_event(&f.identity, &f.network, "c", input, vec![], 10 + i * 10)?;
            let trigger = crate::scheduling::message_trigger(&message_id);
            let spec = crate::work::WorkSpec {
                work_id: crate::work::WorkSpec::id("c", &trigger, "call"),
                conversation: "c".into(), trigger: trigger.clone(), target: node.clone(),
                dispatcher: node.clone(), call_ref: "call".into(), kind: WorkKind::Agent(Provider::Codex),
                model: None, resolved: ResolvedModel { provider: Provider::Codex, model: None },
                cwd: "/w".into(), title: format!("work {i}"), prompt: "do it".into(),
                continues: None, parent: None, thread: None,
            };
            f.store.accept_work_fixture(&f.identity, &f.network, spec, 11 + i * 10)?;
            f.store.change_bot(&f.identity, &f.network, "c", &trigger, "profile",
                bot_settings::ChangeRequest::UpdateProfile {
                    name: Some(format!("Helper {i}")), instructions: None, avatar: None, default_avatar: None,
                }, Provider::Codex, 12 + i * 10)?;
            add_session(&mut f, &format!("s{i}"), "session", 13 + i * 10)?;
            f.store.ask(&f.identity, &f.network, "c", EventBody::QuestionAsked {
                question_id: format!("q-{i}"), target: Some(InputTarget::BotMain { bot_id: "bot".into() }),
                attention: AttentionRequest { kind: AttentionKind::Question, device_id: Some(node.clone()) },
                severity: Severity::Info, text: format!("question {i}"), choices: vec![],
                about: Subject { node: node.clone(), agent: Some(Provider::Codex), session_id: format!("s{i}"),
                    cwd: "/w".into(), summary: String::new(), request_id: Some(format!("r{i}")) },
            }, 14 + i * 10)?;
        }
        let works = all_pages(|cursor| f.store.conversation_works_page(&f.network, "c", cursor, 2))?;
        assert_eq!(serde_json::to_value(&works)?, serde_json::to_value(f.store.works(&f.network, "c")?)?);
        assert_eq!(works.iter().map(|w| w.title.as_str()).collect::<Vec<_>>(),
            ["work 0", "work 1", "work 2", "work 3", "work 4", "work 5"]);
        let changes = all_pages(|cursor| f.store.conversation_changes_page(&f.network, "c", cursor, 2))?;
        assert_eq!(changes, f.store.bot_changes(&f.network, "c")?);
        assert_eq!(changes.len(), 6);
        let questions = all_pages(|cursor| f.store.conversation_questions_page(&f.network, "c", cursor, 2))?;
        assert_eq!(questions, f.store.conversation_questions(&f.network, "c")?);
        assert_eq!(questions.iter().map(|q| q["question_id"].as_str().unwrap()).collect::<Vec<_>>(),
            ["q-0", "q-1", "q-2", "q-3", "q-4", "q-5"]);
        let sessions = all_pages(|cursor| f.store.conversation_sessions_page(&f.network, "c", cursor, 2))?;
        assert_eq!(sessions, f.store.attributed_sessions(&f.network, "c")?);
        assert_eq!(sessions.iter().map(|s| s["session_id"].as_str().unwrap()).collect::<Vec<_>>(),
            ["s0", "s1", "s2", "s3", "s4", "s5"]);
        Ok(())
    }

    #[test]
    fn sessions_continue_after_boundary_deletion_and_ignore_newer_arrivals() -> Result<()> {
        let mut f = fixture()?;
        for i in 0..5 { add_session(&mut f, &format!("s{i}"), "session", 10 + i)?; }
        let first = f.store.conversation_sessions_page(&f.network, "c", None, 2)?;
        let cursor = first.next_cursor.context("more sessions")?;
        // Keyset windows do not depend on the boundary row still being present.
        f.store.connection.execute("DELETE FROM session_owners WHERE session_id='s3'", [])?;
        add_session(&mut f, "s5", "late", 100)?;
        let next = f.store.conversation_sessions_page(&f.network, "c", Some(&cursor), 2)?;
        assert_eq!(next.items.iter().map(|s| s["session_id"].as_str().unwrap()).collect::<Vec<_>>(), ["s1", "s2"]);
        assert!(f.store.conversation_questions_page(&f.network, "c", Some(&cursor), 2).is_err());
        assert!(f.store.conversation_sessions_page(&f.network, "other", Some(&cursor), 2).is_err());
        assert!(f.store.conversation_sessions_page(&f.network, "c", Some("invalid"), 2).is_err());
        assert!(f.store.conversation_sessions_page(&f.network, "c", None, 0).is_err());
        assert!(f.store.conversation_sessions_page(&f.network, "c", None, 101).is_err());
        Ok(())
    }

    #[test]
    fn byte_budget_keeps_the_next_full_session_retrievable() -> Result<()> {
        let mut f = fixture()?;
        // Session bodies normally obey event limits. Large local projections exercise the
        // serialized response boundary without changing signed-event admission rules.
        add_session(&mut f, "s0", "small", 10)?;
        add_session(&mut f, "s1", "small", 11)?;
        let large = "x".repeat(HISTORY_PAGE_BYTES / 2 + 32);
        f.store.connection.execute(
            "UPDATE session_owners SET attribution=json_set(attribution,'$.summary',?)", [&large],
        )?;
        let first = f.store.conversation_sessions_page(&f.network, "c", None, 100)?;
        assert_eq!(first.items.len(), 1);
        assert_eq!(first.items[0]["session_id"], "s1");
        assert!(!first.complete);
        let second = f.store.conversation_sessions_page(&f.network, "c", first.next_cursor.as_deref(), 100)?;
        assert_eq!(second.items[0]["session_id"], "s0");
        assert!(second.complete);
        let huge = "x".repeat(vibeface_protocol::history::HISTORY_RESPONSE_BYTES);
        f.store.connection.execute(
            "UPDATE session_owners SET attribution=json_set(attribution,'$.summary',?) WHERE session_id='s1'", [&huge],
        )?;
        assert!(f.store.conversation_sessions_page(&f.network, "c", None, 1).is_err());
        Ok(())
    }

    #[test]
    fn empty_filtered_question_page_advances_to_older_main_questions() -> Result<()> {
        let mut f = fixture()?;
        let node = f.identity.device_id()?;
        for (id, target, at) in [
            ("q-main", Some(InputTarget::BotMain { bot_id: "bot".into() }), 10),
            ("q-unknown", None, 11),
        ] {
            f.store.ask(&f.identity, &f.network, "c", EventBody::QuestionAsked {
                question_id: id.into(), target,
                attention: AttentionRequest { kind: AttentionKind::Question, device_id: Some(node.clone()) },
                severity: Severity::Info, text: id.into(), choices: vec![],
                about: Subject { node: node.clone(), agent: Some(Provider::Codex),
                    session_id: id.into(), cwd: "/w".into(), summary: String::new(), request_id: Some(id.into()) },
            }, at)?;
        }
        let first = f.store.conversation_questions_page(&f.network, "c", None, 1)?;
        assert!(first.items.is_empty());
        assert!(!first.complete);
        let next = f.store.conversation_questions_page(&f.network, "c", first.next_cursor.as_deref(), 1)?;
        assert_eq!(next.items[0]["question_id"], "q-main");
        assert!(next.complete);
        Ok(())
    }

    #[test]
    fn actual_store_session_page_does_not_scan_a_hundred_thousand_older_rows() -> Result<()> {
        use std::sync::{Arc, atomic::{AtomicUsize, Ordering}};
        let f = fixture()?;
        // This fixture measures projection reads, independently of event ingestion costs.
        f.store.connection.execute(
            "WITH RECURSIVE ids(i) AS (VALUES(0) UNION ALL SELECT i+1 FROM ids WHERE i<99999)
             INSERT INTO session_owners(network_id,conversation_id,node,session_id,rank,attribution,created_at_ms)
             SELECT ?1,'c','node',printf('s%06d',i),printf('%06d',i),'{}',i FROM ids",
            [&f.network],
        )?;
        let (authority, _) = f.store.history_cursor(&f.network, "c", HistorySection::Sessions, None)?;
        for rank in ["090000", "050000", "010000"] {
            let cursor = history_page::<Value>(&f.network, "c", HistorySection::Sessions,
                authority.clone(), Some(HistoryBoundary { rank: rank.into(), id: format!("node\0s{rank}") }),
                vec![], true)?.next_cursor.context("cursor")?;
            let steps = Arc::new(AtomicUsize::new(0));
            let observed = steps.clone();
            f.store.connection.progress_handler(1, Some(move || {
                observed.fetch_add(1, Ordering::Relaxed);
                false
            }));
            let page = f.store.conversation_sessions_page(&f.network, "c", Some(&cursor), 100);
            f.store.connection.progress_handler(0, None::<fn() -> bool>);
            assert_eq!(page?.items.len(), 100);
            assert!(steps.load(Ordering::Relaxed) < 10_000,
                "page at {rank} executed {} VM steps", steps.load(Ordering::Relaxed));
        }
        Ok(())
    }
}
