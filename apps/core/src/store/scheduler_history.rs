//! Bounded reads of local scheduler observations. Candidate selection precedes hydration.
use super::conversation_history::{HistoryBoundary, history_page};
use super::*;
use crate::scheduling::{PendingCheck, SchedulerJob};
use serde_json::json;
use vibeface_protocol::history::{
    HISTORY_MAX_LIMIT, HISTORY_PAGE_BYTES, HistoryPage, HistorySection,
};

#[derive(Clone, Copy)]
pub(crate) struct SchedulerHistoryQuery<'a> {
    pub conversation: &'a str,
    pub section: HistorySection,
    pub cursor: Option<&'a str>,
    pub limit: usize,
}

impl Store {
    pub(crate) fn scheduler_reply_is_present(
        &self,
        network: &str,
        conversation: &str,
        message: &str,
    ) -> Result<bool> {
        Ok(self.connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM messages WHERE network_id=? AND conversation_id=? AND message_id=?)",
            params![network, conversation, message], |row| row.get(0))?)
    }

    pub(crate) fn scheduler_trigger_has_check(
        &self,
        conversation: &str,
        trigger: &str,
    ) -> Result<bool> {
        Ok(self.connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM pending_checks WHERE conversation_id=? AND trigger=?)",
            params![conversation, trigger],
            |row| row.get(0),
        )?)
    }

    /// Each source consumes the remaining candidate quota. A full scan window may have no
    /// visible rows; its continuation still advances, including across source boundaries.
    pub(crate) fn conversation_scheduler_page(
        &self,
        network: &str,
        local: &str,
        query: SchedulerHistoryQuery<'_>,
        mut project: impl FnMut(&Store, &SchedulerJob) -> Result<Value>,
    ) -> Result<HistoryPage<Value>> {
        let SchedulerHistoryQuery {
            conversation,
            section,
            cursor,
            limit,
        } = query;
        ensure!(
            (1..=HISTORY_MAX_LIMIT).contains(&limit),
            "history limit must be 1 to 100"
        );
        ensure!(
            matches!(
                section,
                HistorySection::Jobs
                    | HistorySection::ScopeJobs
                    | HistorySection::Checks
                    | HistorySection::Deliveries
                    | HistorySection::Unanswered
            ),
            "not a scheduler history section"
        );
        let (authority, before) = self.history_cursor(network, conversation, section, cursor)?;
        if section == HistorySection::Unanswered
            && (self
                .conversation(network, conversation)?
                .and_then(|c| c.owner)
                .as_deref()
                != Some(local)
                || super::bots::bot_deleted_from(&self.connection, network, conversation)?)
        {
            return history_page(
                network,
                conversation,
                section,
                authority,
                before,
                vec![],
                false,
            );
        }
        let candidates = self.scheduler_history_candidates(
            network,
            conversation,
            section,
            before.as_ref(),
            limit,
        )?;
        let full = candidates.len() == limit;
        let mut boundary = before;
        let mut items = Vec::new();
        let mut bytes = 0;
        let mut has_more = full;
        for candidate in candidates {
            let value = match section {
                HistorySection::Jobs if candidate.rank == "outbox" => {
                    let owner = self
                        .conversation(network, conversation)?
                        .and_then(|c| c.owner);
                    Some(
                        json!({"trigger":candidate.id,"owner":owner,"conversation_id":conversation,
                        "state":"pending_delivery","message_id":candidate.id.strip_prefix("message:")}),
                    )
                }
                HistorySection::Jobs | HistorySection::ScopeJobs => self
                    .scheduler_job(conversation, &candidate.id)?
                    .map(|job| project(self, &job))
                    .transpose()?,
                HistorySection::Checks => {
                    self.history_check(network, conversation, &candidate.id)?
                }
                HistorySection::Deliveries => {
                    self.history_delivery(network, conversation, &candidate)?
                }
                HistorySection::Unanswered => self
                    .history_unanswered(network, conversation, &candidate.id)?
                    .then(|| json!(candidate.id)),
                _ => unreachable!("section checked at boundary"),
            };
            if let Some(value) = value {
                let size = serde_json::to_vec(&value)?.len() + 1;
                if !items.is_empty() && bytes + size > HISTORY_PAGE_BYTES {
                    has_more = true;
                    break;
                }
                bytes += size;
                items.push(value);
            }
            boundary = Some(candidate);
        }
        items.reverse();
        history_page(
            network,
            conversation,
            section,
            authority,
            boundary,
            items,
            has_more,
        )
    }

    fn scheduler_history_candidates(
        &self,
        network: &str,
        conversation: &str,
        section: HistorySection,
        before: Option<&HistoryBoundary>,
        limit: usize,
    ) -> Result<Vec<HistoryBoundary>> {
        let mut candidates = Vec::new();
        match section {
            HistorySection::Jobs | HistorySection::ScopeJobs => {
                // Outbox and admitted jobs have different order keys. Complete the outbox
                // phase first, then continue through jobs by their stored sequence.
                if section == HistorySection::Jobs && before.is_none_or(|b| b.rank == "outbox") {
                    let seek = if before.is_some() {
                        " AND trigger<?2"
                    } else {
                        ""
                    };
                    let mut statement =
                        self.connection
                            .prepare(&format!(
                        "SELECT trigger FROM scheduler_outbox WHERE conversation_id=?1{seek}
                         ORDER BY trigger DESC LIMIT ?3"))?;
                    candidates = statement
                        .query_map(params![conversation, before.map(|b| &b.id), limit], |row| {
                            Ok(HistoryBoundary {
                                rank: "outbox".into(),
                                id: row.get(0)?,
                            })
                        })?
                        .collect::<rusqlite::Result<_>>()?;
                }
                if candidates.len() < limit {
                    let seek = before.filter(|b| b.rank != "outbox");
                    let sequence = seek
                        .map(|b| b.rank.parse::<i64>())
                        .transpose()
                        .context("invalid job history boundary")?;
                    let scope = if section == HistorySection::Jobs {
                        " AND scope='main'"
                    } else {
                        ""
                    };
                    let seek_sql = if seek.is_some() {
                        " AND (sequence,trigger)<(?2,?3)"
                    } else {
                        ""
                    };
                    let mut statement = self.connection.prepare(&format!(
                        "SELECT sequence,trigger FROM scheduler_jobs WHERE conversation_id=?1{scope}{seek_sql}
                         ORDER BY sequence DESC,trigger DESC LIMIT ?4"))?;
                    candidates.extend(
                        statement
                            .query_map(
                                params![
                                    conversation,
                                    sequence,
                                    seek.map(|b| &b.id),
                                    limit - candidates.len()
                                ],
                                |row| {
                                    Ok(HistoryBoundary {
                                        rank: row.get::<_, i64>(0)?.to_string(),
                                        id: row.get(1)?,
                                    })
                                },
                            )?
                            .collect::<rusqlite::Result<Vec<_>>>()?,
                    );
                }
            }
            HistorySection::Checks => {
                let created = before
                    .map(|b| b.rank.parse::<i64>())
                    .transpose()
                    .context("invalid check history boundary")?;
                let seek = if before.is_some() {
                    " AND (created_at_ms,attempt_id)<(?2,?3)"
                } else {
                    ""
                };
                let mut statement = self.connection.prepare(&format!(
                    "SELECT created_at_ms,attempt_id FROM pending_checks WHERE conversation_id=?1{seek}
                     ORDER BY created_at_ms DESC,attempt_id DESC LIMIT ?4"))?;
                candidates = statement
                    .query_map(
                        params![conversation, created, before.map(|b| &b.id), limit],
                        |row| {
                            Ok(HistoryBoundary {
                                rank: row.get::<_, i64>(0)?.to_string(),
                                id: row.get(1)?,
                            })
                        },
                    )?
                    .collect::<rusqlite::Result<_>>()?;
            }
            HistorySection::Deliveries => {
                if before.is_none_or(|b| b.rank == "stop") {
                    let seek = if before.is_some() {
                        " AND work_id<?2"
                    } else {
                        ""
                    };
                    let mut statement = self.connection.prepare(&format!(
                        "SELECT work_id FROM work_stops WHERE conversation_id=?1{seek} ORDER BY work_id DESC LIMIT ?3"))?;
                    candidates = statement
                        .query_map(params![conversation, before.map(|b| &b.id), limit], |row| {
                            Ok(HistoryBoundary {
                                rank: "stop".into(),
                                id: row.get(0)?,
                            })
                        })?
                        .collect::<rusqlite::Result<_>>()?;
                }
                if candidates.len() < limit {
                    let seek = before.filter(|b| b.rank == "dispatch");
                    let seek_sql = if seek.is_some() {
                        " AND work_id<?2"
                    } else {
                        ""
                    };
                    let mut statement = self.connection.prepare(&format!(
                        "SELECT work_id FROM work_dispatches WHERE json_extract(spec,'$.conversation')=?1{seek_sql}
                         ORDER BY work_id DESC LIMIT ?3"))?;
                    candidates.extend(
                        statement
                            .query_map(
                                params![
                                    conversation,
                                    seek.map(|b| &b.id),
                                    limit - candidates.len()
                                ],
                                |row| {
                                    Ok(HistoryBoundary {
                                        rank: "dispatch".into(),
                                        id: row.get(0)?,
                                    })
                                },
                            )?
                            .collect::<rusqlite::Result<Vec<_>>>()?,
                    );
                }
            }
            HistorySection::Unanswered => {
                let seek = if before.is_some() {
                    " AND (rank,message_id)<(?3,?4)"
                } else {
                    ""
                };
                let mut statement = self.connection.prepare(&format!(
                    "SELECT rank,message_id FROM messages WHERE network_id=?1 AND conversation_id=?2{seek}
                     ORDER BY rank DESC,message_id DESC LIMIT ?5"))?;
                candidates = statement
                    .query_map(
                        params![
                            network,
                            conversation,
                            before.map(|b| &b.rank),
                            before.map(|b| &b.id),
                            limit
                        ],
                        |row| {
                            Ok(HistoryBoundary {
                                rank: row.get(0)?,
                                id: row.get(1)?,
                            })
                        },
                    )?
                    .collect::<rusqlite::Result<_>>()?;
            }
            _ => bail!("not a scheduler history section"),
        }
        Ok(candidates)
    }

    fn history_unanswered(&self, network: &str, conversation: &str, message: &str) -> Result<bool> {
        Ok(self.connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM messages m
             WHERE m.network_id=?1 AND m.conversation_id=?2 AND m.message_id=?3
             AND m.role='user' AND m.steered_into IS NULL
             AND (m.target IS NULL OR json_extract(m.target,'$.kind')='bot_main')
             AND NOT EXISTS(SELECT 1 FROM messages r WHERE r.network_id=?1 AND r.conversation_id=?2
                 AND r.in_reply_to=m.message_id AND r.status='completed')
             AND NOT EXISTS(SELECT 1 FROM scheduler_jobs j WHERE j.conversation_id=?2 AND j.trigger='message:'||m.message_id)
             AND NOT EXISTS(SELECT 1 FROM scheduler_outbox o WHERE o.conversation_id=?2 AND o.trigger='message:'||m.message_id))",
            params![network, conversation, message], |row| row.get(0))?)
    }

    fn history_check(
        &self,
        network: &str,
        conversation: &str,
        attempt: &str,
    ) -> Result<Option<Value>> {
        let mut check: PendingCheck = self.connection.query_row(
            "SELECT conversation_id,trigger,detail,created_at_ms FROM pending_checks WHERE attempt_id=?", [attempt], |row| {
                Ok(PendingCheck { check_id: PendingCheck::id(attempt), conversation_id: row.get(0)?,
                    trigger: row.get(1)?, attempt_id: attempt.into(), detail: row.get(2)?,
                    created_at_ms: row.get(3)?, process_groups: vec![], codex_turn: None })
            })?;
        if !scope_projection::trigger_is_main_from(
            &self.connection,
            network,
            conversation,
            &check.trigger,
        )? {
            return Ok(None);
        }
        if let Some(work_id) = check.trigger.strip_prefix("task:")
            && self.work(network, work_id)?.is_some_and(|work| {
                work.conversation_id == conversation && work.state == WorkStatus::Unverified
            })
        {
            return Ok(None);
        }
        check.process_groups = self.connection.prepare(
            "SELECT process_group FROM manager_processes WHERE attempt_id=? ORDER BY process_group")?
            .query_map([attempt], |row| row.get(0))?.collect::<rusqlite::Result<_>>()?;
        check.codex_turn = self
            .connection
            .query_row(
                "SELECT server,thread_id,turn_id FROM manager_turns WHERE attempt_id=?",
                [attempt],
                |row| {
                    Ok(crate::scheduling::ReplyTurn {
                        server: row.get(0)?,
                        thread: row.get(1)?,
                        turn: row.get(2)?,
                    })
                },
            )
            .optional()?;
        Ok(Some(serde_json::to_value(check)?))
    }

    fn history_delivery(
        &self,
        network: &str,
        conversation: &str,
        candidate: &HistoryBoundary,
    ) -> Result<Option<Value>> {
        if !scope_projection::work_is_main_from(
            &self.connection,
            network,
            conversation,
            &candidate.id,
        )? {
            return Ok(None);
        }
        if candidate.rank == "stop" {
            return Ok(self.stop_request(&candidate.id)?.map(|stop| json!({"work_id":stop.work_id,
                "target_node":stop.target,"state":if stop.timed_out {"stop_timed_out"} else {"stop_waiting"}})));
        }
        if self.connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM works WHERE network_id=?1 AND conversation_id=?2
             AND work_id=?3 AND dispatch IS NOT NULL)",
            params![network, conversation, candidate.id],
            |row| row.get::<_, bool>(0),
        )? {
            return Ok(None);
        }
        let Some(dispatch) = self.dispatch(&candidate.id)? else {
            return Ok(None);
        };
        if dispatch.state == crate::work::DispatchState::Accepted {
            return Ok(None);
        }
        let spec = &dispatch.spec;
        Ok(Some(
            json!({"work_id":spec.work_id,"target_node":spec.target,"state":dispatch.state.name(),
            "kind":spec.kind,"title":spec.title,"cwd":spec.cwd,"reason":dispatch.state.reason(),
            "failure":dispatch.state.failure(),"trigger":spec.trigger,"prompt":spec.prompt}),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::work::{Dispatch, DispatchState, RequestedWorkKind, StopRequest, WorkRequest};
    use vibeface_protocol::history::HISTORY_RESPONSE_BYTES;

    fn fixture() -> Result<(LocalIdentity, String, Store)> {
        let identity = LocalIdentity::generate();
        let network = identity.fixture_network_id()?;
        let state = identity.sign_network_fixture(1, vec![identity.device_id()?])?;
        let mut store = Store::open(":memory:")?;
        store.install_trust_bundle(&network, &state)?;
        let owner = identity.owner_fixture(&network, "c", &identity.device_id()?)?;
        store.create_conversation(&identity, &network, "c", "history", owner, 1)?;
        let bot = identity.fixture_admin().attest(
            &network,
            "c",
            EventBody::BotCreated {
                bot_id: "bot".into(),
                name: "history".into(),
                instructions: String::new(),
                project: None,
                attestation: None,
                cause: None,
            },
        )?;
        store.append_event(&identity, &network, "c", bot, vec![], 2)?;
        Ok((identity, network, store))
    }

    fn input(
        identity: &LocalIdentity,
        store: &mut Store,
        network: &str,
        id: &str,
        at: i64,
    ) -> Result<()> {
        let body = identity.fixture_admin().attest(
            network,
            "c",
            EventBody::InputPosted {
                message_id: id.into(),
                target: crate::workspace::InputTarget::BotMain {
                    bot_id: "bot".into(),
                },
                intent: crate::workspace::InputIntent::Continue,
                sequence: at as u64,
                expected_revision: None,
                text: format!("signed {id}"),
                attachments: vec![],
                report_results: Some(true),
                attestation: None,
            },
        )?;
        store.append_event(identity, network, "c", body, vec![], at)?;
        Ok(())
    }

    fn job(store: &Store, id: &str, sequence: u64, detail: &str) -> Result<()> {
        store.connection.execute(
            "INSERT INTO scheduler_jobs(conversation_id,trigger,kind,sequence,basis,reply_to,message_id,state,detail,tool_calls,scope)
             VALUES('c',?1,'human',?2,'{}',?3,?4,'cancelled',?5,0,'main')",
            params![format!("message:{id}"), sequence, id, format!("reply:{id}"), detail])?;
        Ok(())
    }

    fn all(
        store: &Store,
        network: &str,
        local: &str,
        section: HistorySection,
        limit: usize,
    ) -> Result<Vec<Value>> {
        let mut cursor = None;
        let mut collected = Vec::new();
        for _ in 0..1000 {
            let page = store.conversation_scheduler_page(
                network,
                local,
                SchedulerHistoryQuery {
                    conversation: "c",
                    section,
                    cursor: cursor.as_deref(),
                    limit,
                },
                |_, job| Ok(json!(job)),
            )?;
            assert!(serde_json::to_vec(&page)?.len() < HISTORY_RESPONSE_BYTES);
            collected.splice(0..0, page.items);
            cursor = page.next_cursor;
            if cursor.is_none() {
                return Ok(collected);
            }
        }
        bail!("scheduler history failed to terminate")
    }

    #[test]
    fn signed_scheduler_history_preserves_all_sources_and_append_boundary() -> Result<()> {
        let (identity, network, mut store) = fixture()?;
        for index in 0..12 {
            let id = format!("m{index:03}");
            input(&identity, &mut store, &network, &id, 3 + index)?;
            job(&store, &id, index as u64, &"完整记录\\\"\n".repeat(24_000))?;
        }
        store.connection.execute(
            "INSERT INTO scheduler_outbox VALUES('c','message:remote')",
            [],
        )?;
        let local = identity.device_id()?;
        let oracle = store
            .scheduler_jobs("c")?
            .iter()
            .map(|job| json!(job))
            .collect::<Vec<_>>();
        assert!(serde_json::to_vec(&oracle)?.len() > HISTORY_PAGE_BYTES);
        assert_eq!(
            all(&store, &network, &local, HistorySection::ScopeJobs, 100)?,
            oracle
        );
        let jobs = all(&store, &network, &local, HistorySection::Jobs, 3)?;
        assert_eq!(&jobs[..oracle.len()], oracle.as_slice());
        assert_eq!(jobs.last().context("outbox")?["trigger"], "message:remote");
        assert_eq!(jobs.last().context("outbox")?["state"], "pending_delivery");
        let first = store.conversation_scheduler_page(
            &network,
            &local,
            SchedulerHistoryQuery {
                conversation: "c",
                section: HistorySection::ScopeJobs,
                cursor: None,
                limit: 3,
            },
            |_, job| Ok(json!(job)),
        )?;
        let cursor = first.next_cursor.context("older jobs")?;
        input(&identity, &mut store, &network, "appended", 1000)?;
        job(&store, "appended", 999, "new")?;
        let next = store.conversation_scheduler_page(
            &network,
            &local,
            SchedulerHistoryQuery {
                conversation: "c",
                section: HistorySection::ScopeJobs,
                cursor: Some(&cursor),
                limit: 3,
            },
            |_, job| Ok(json!(job)),
        )?;
        assert!(
            next.items
                .iter()
                .all(|job| job["trigger"] != "message:appended")
        );
        assert!(
            store
                .conversation_scheduler_page(
                    &network,
                    &local,
                    SchedulerHistoryQuery {
                        conversation: "other",
                        section: HistorySection::ScopeJobs,
                        cursor: Some(&cursor),
                        limit: 3,
                    },
                    |_, job| Ok(json!(job))
                )
                .is_err()
        );
        assert!(
            store
                .conversation_scheduler_page(
                    &network,
                    &local,
                    SchedulerHistoryQuery {
                        conversation: "c",
                        section: HistorySection::Jobs,
                        cursor: Some(&cursor),
                        limit: 3,
                    },
                    |_, job| Ok(json!(job))
                )
                .is_err()
        );
        Ok(())
    }

    #[test]
    fn signed_scheduler_history_unanswered_matches_full_oracle_with_empty_continuations()
    -> Result<()> {
        let (identity, network, mut store) = fixture()?;
        for index in 0..125 {
            let id = format!("m{index:03}");
            input(&identity, &mut store, &network, &id, 3 + index)?;
            if index > 0 {
                job(&store, &id, index as u64, "settled")?;
            }
        }
        let local = identity.device_id()?;
        let first = store.conversation_scheduler_page(
            &network,
            &local,
            SchedulerHistoryQuery {
                conversation: "c",
                section: HistorySection::Unanswered,
                cursor: None,
                limit: 100,
            },
            |_, _| bail!("unanswered must not hydrate scheduler jobs"),
        )?;
        assert!(first.items.is_empty());
        assert!(first.next_cursor.is_some());
        let mut hydrated = 0;
        let jobs = store.conversation_scheduler_page(
            &network,
            &local,
            SchedulerHistoryQuery {
                conversation: "c",
                section: HistorySection::ScopeJobs,
                cursor: None,
                limit: 100,
            },
            |_, job| {
                hydrated += 1;
                Ok(json!(job))
            },
        )?;
        assert_eq!(hydrated, 100);
        assert_eq!(jobs.items.len(), 100);
        assert_eq!(
            all(&store, &network, &local, HistorySection::Unanswered, 7)?,
            store
                .unanswered_messages(&network, &local, "c")?
                .iter()
                .map(|id| json!(id))
                .collect::<Vec<_>>()
        );
        assert!(
            all(
                &store,
                &network,
                "former-owner",
                HistorySection::Unanswered,
                7
            )?
            .is_empty()
        );
        Ok(())
    }

    #[test]
    fn signed_scheduler_history_checks_deliveries_and_stops_keep_controls() -> Result<()> {
        let (identity, network, mut store) = fixture()?;
        input(&identity, &mut store, &network, "main", 3)?;
        let local = identity.device_id()?;
        for index in 0..9 {
            store.connection.execute(
                "INSERT INTO pending_checks VALUES(?1,'c','message:main',?2,?3)",
                params![
                    format!("attempt-{index}"),
                    format!("exact detail {index}"),
                    index
                ],
            )?;
            let id = format!("work-{index}");
            store.save_dispatch(&Dispatch {
                spec: WorkRequest {
                    work_id: id.clone(),
                    conversation: "c".into(),
                    trigger: "message:main".into(),
                    target: local.clone(),
                    dispatcher: local.clone(),
                    call_ref: id.clone(),
                    kind: RequestedWorkKind::Task,
                    model: None,
                    cwd: "/fixture".into(),
                    title: id.clone(),
                    prompt: format!("complete prompt {index}"),
                    continues: None,
                    parent: None,
                    thread: None,
                },
                state: if index == 8 {
                    DispatchState::Accepted
                } else {
                    DispatchState::Pending
                },
            })?;
            store.hold_stop_request(&StopRequest {
                work_id: id,
                conversation: "c".into(),
                target: local.clone(),
                requested_at_ms: index,
                deadline_ms: index + 100,
                timed_out: index % 2 == 0,
            })?;
        }
        store.connection.execute(
            "INSERT INTO pending_checks VALUES('other','other','message:main','private',99)",
            [],
        )?;
        let checks = all(&store, &network, &local, HistorySection::Checks, 2)?;
        assert_eq!(checks.len(), 9);
        assert_eq!(checks[0]["check_id"], "manager:attempt-0");
        assert_eq!(checks[8]["detail"], "exact detail 8");
        let deliveries = all(&store, &network, &local, HistorySection::Deliveries, 3)?;
        assert_eq!(
            deliveries.len(),
            17,
            "eight waiting dispatches and all nine stop controls"
        );
        assert_eq!(deliveries[0]["prompt"], "complete prompt 0");
        assert_eq!(deliveries[8]["state"], "stop_timed_out");
        assert_eq!(deliveries[16]["work_id"], "work-8");
        Ok(())
    }
}
