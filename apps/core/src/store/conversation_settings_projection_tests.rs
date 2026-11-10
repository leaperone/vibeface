use super::*;
use bot_settings::ChangeRequest;
use vibeface_protocol::history::HistoryPage;

struct Fixture {
    network: String,
    a: LocalIdentity,
    b: LocalIdentity,
    state: NetworkState,
}

impl Fixture {
    fn new() -> Result<Self> {
        let a = LocalIdentity::generate();
        let b = LocalIdentity::generate();
        let state = a.sign_network_fixture(1, vec![a.device_id()?, b.device_id()?])?;
        Ok(Self { network: a.fixture_network_id()?, a, b, state })
    }

    fn store(&self) -> Result<Store> {
        let mut store = Store::open(":memory:")?;
        store.install_trust_bundle(&self.network, &self.state)?;
        Ok(store)
    }

    fn seeded(&self) -> Result<Store> {
        let mut store = self.store()?;
        let owner = self.a.owner_fixture(&self.network, "c", &self.a.device_id()?)?;
        store.create_conversation(&self.a, &self.network, "c", "settings", owner, 1)?;
        for (body, at) in [
            (EventBody::BotCreated { bot_id: "bot".into(), name: "Helper".into(), instructions: "brief".into(), project: None, attestation: None, cause: None }, 2),
            (EventBody::InputPosted { message_id: "m1".into(), target: crate::workspace::InputTarget::BotMain { bot_id: "bot".into() },
                intent: crate::workspace::InputIntent::Continue, sequence: 1, expected_revision: None,
                text: "update".into(), attachments: vec![], report_results: Some(true), attestation: None }, 3),
        ] {
            let body = self.a.fixture_admin().attest(&self.network, "c", body)?;
            store.append_event(&self.a, &self.network, "c", body, vec![], at)?;
        }
        Ok(store)
    }

    fn sync(&self, from: &Store, to: &mut Store) -> Result<()> {
        for identity in [&self.a, &self.b] {
            let events = from.events_from(&self.network, &identity.device_id()?, 1, 500)?;
            let ingested = to.ingest_events(&self.network, &events)?;
            ensure!(ingested.statuses.iter().all(|status| matches!(status, IngestStatus::Accepted | IngestStatus::Duplicate)), "fixture sync rejected");
        }
        Ok(())
    }
}

fn pages<T>(mut read: impl FnMut(Option<&str>) -> Result<HistoryPage<T>>) -> Result<Vec<T>> {
    let mut all = Vec::new();
    let mut cursor = None;
    loop {
        let mut page = read(cursor.as_deref())?;
        page.items.append(&mut all);
        all = page.items;
        if page.complete { return Ok(all); }
        cursor = Some(page.next_cursor.context("cursor")?);
    }
}

fn assert_overwritten(store: &Store, network: &str) -> Result<Vec<bot_settings::Overwritten>> {
    let actual = pages(|cursor| store.conversation_overwritten_page(network, "c", cursor, 1))?;
    assert_eq!(actual, store.overwritten(network, "c")?);
    Ok(actual)
}

fn assert_memory_conflicts(store: &Store, network: &str) -> Result<Vec<MemorySyncConflict>> {
    let actual = pages(|cursor| store.conversation_memory_sync_conflicts_page(network, "c", cursor, 3))?;
    let old = store.memory_sync_conflicts(network, "c")?;
    assert_eq!(&actual[..actual.len().min(50)], old.items);
    assert_eq!(actual.len() > 50, old.has_more);
    Ok(actual)
}

#[test]
fn signed_setting_collision_late_insert_rebuild_and_fork_match_old_oracle() -> Result<()> {
    let f = Fixture::new()?;
    let mut a = f.seeded()?;
    let mut b = f.store()?;
    f.sync(&a, &mut b)?;
    let rename = |name: &str| ChangeRequest::UpdateProfile { name: Some(name.into()), instructions: None, avatar: None, default_avatar: None };
    a.change_bot(&f.a, &f.network, "c", "message:m1", "left", rename("Alpha"), Provider::Codex, 4)?;
    b.change_bot(&f.b, &f.network, "c", "message:m1", "right", rename("Beta"), Provider::Codex, 4)?;
    f.sync(&a, &mut b)?;
    f.sync(&b, &mut a)?;
    let old = assert_overwritten(&a, &f.network)?;
    assert_eq!(old.len(), 1);
    assert!(["Alpha", "Beta"].contains(&old[0].value.as_str().context("name")?));
    assert_eq!(assert_overwritten(&b, &f.network)?, old);
    // Reconstructing only the new derived indexes changes no semantic records.
    rebuild_projection(&a.connection, &f.network)?;
    assert_eq!(assert_overwritten(&a, &f.network)?, old);
    a.connection.execute("UPDATE events SET effective=0 WHERE network_id=?", [&f.network])?;
    reduce::clear(&a.connection, &f.network)?;
    log::recompute(&a.connection, &membership_from(&a.connection, &f.network)?)?;
    assert_eq!(assert_overwritten(&a, &f.network)?, old);
    let original = b.events_from(&f.network, &f.b.device_id()?, 1, 10)?.remove(0);
    let mut fork: Event = serde_json::from_str(&original.canonical_body)?;
    if let EventBody::BotChanged { change: BotChange::Profile { name: Some(name), .. }, .. } = &mut fork.body {
        name.after = "Forked name".into();
    } else { bail!("expected profile event"); }
    let ingested = a.ingest_events(&f.network, &[f.b.sign_event(fork)?])?;
    assert_eq!(ingested.statuses, [IngestStatus::Conflict]);
    assert!(ingested.rebuilt);
    assert_overwritten(&a, &f.network)?;
    rebuild_projection(&a.connection, &f.network)?;
    assert_overwritten(&a, &f.network)?;
    Ok(())
}

#[test]
fn settings_pages_keep_all_content_and_current_skill_selection() -> Result<()> {
    let f = Fixture::new()?;
    let mut store = f.seeded()?;
    for (i, name) in ["Zulu", "Äpfel", "Alpha"].into_iter().enumerate() {
        for (call, request) in [
            (format!("skill{i}"), ChangeRequest::SaveSkill {
                skill_id: None, name: name.into(), description: format!("{name} summary"), body: "instructions".into(),
            }),
            (format!("schedule{i}"), ChangeRequest::SaveSchedule {
                schedule_id: None, name: name.into(), prompt: "later".into(),
                first_run_at_ms: 100_000, every_minutes: None, enabled: Some(i != 1),
            }),
            (format!("memory{i}"), ChangeRequest::Remember {
                memory_id: Some(format!("memory{i}")), text: format!("note {i}"),
            }),
        ] {
            store.change_bot(&f.a, &f.network, "c", "message:m1", &call, request, Provider::Codex, 4 + i as i64)?;
        }
    }
    let mut skills = pages(|cursor| store.conversation_skills_page(&f.network, "c", cursor, 1))?;
    assert_eq!(skills.len(), 3);
    assert!(skills.windows(2).all(|pair| pair[0].skill_id < pair[1].skill_id));
    let selected_id = skills[1].skill_id.clone();
    skills.sort_by_cached_key(|entry| (entry.skill.name.to_lowercase(), entry.skill_id.clone()));
    assert_eq!(skills.iter().map(|s| s.skill.name.as_str()).collect::<Vec<_>>(), ["Alpha", "Zulu", "Äpfel"]);
    assert_eq!(serde_json::to_value(&skills)?, serde_json::to_value(store.skills(&f.network)?)?);
    let mut schedules = pages(|cursor| store.conversation_schedules_page(&f.network, "c", cursor, 1))?;
    schedules.sort_by_cached_key(|entry| (entry.schedule.name.to_lowercase(), entry.schedule_id.clone()));
    assert_eq!(serde_json::to_value(&schedules)?, serde_json::to_value(store.schedules(&f.network, "c")?)?);
    let memory = pages(|cursor| store.conversation_memory_page(&f.network, "c", cursor, 1))?;
    let old_memory = store.memory_records(&f.network, "c")?.into_iter().map(|record| serde_json::json!({
        "memory_id":record.entry.memory_id,"text":record.entry.text,"metadata":record.metadata(),
    })).collect::<Vec<_>>();
    assert_eq!(serde_json::to_value(memory)?, serde_json::to_value(old_memory)?);
    store.change_bot(&f.a, &f.network, "c", "message:m1", "select", ChangeRequest::ConfigureBot {
        bot: "bot".into(), name: None, instructions: None, model: None, reasoning_effort: None,
        skills: Some([selected_id.clone()].into_iter().collect()), all_skills: None, avatar: None, default_avatar: None,
    }, Provider::Codex, 20)?;
    let first = store.conversation_skills_page(&f.network, "c", None, 1)?;
    assert!(first.items.is_empty());
    assert!(!first.complete);
    let selected = pages(|cursor| store.conversation_skills_page(&f.network, "c", cursor, 1))?;
    assert_eq!(selected.iter().map(|s| s.skill_id.as_str()).collect::<Vec<_>>(), [selected_id.as_str()]);
    Ok(())
}

#[test]
fn local_conflict_pages_preserve_duplicate_scopes_and_reject_session_replacement() -> Result<()> {
    use crate::scheduling::{MemoryConflict, Shown, StagedMemory};
    let f = Fixture::new()?;
    let store = f.seeded()?;
    let conflict = MemoryConflict {
        call_ref: "call".into(), memory_id: "memory".into(), expected_epoch: 1, actual_epoch: 2,
        expected_revision: "old".into(), actual_revision: "new".into(), reason: "revision_changed".into(),
    };
    let mut shown = Shown { memory_conflicts: vec![conflict.clone(), conflict.clone()], ..Default::default() };
    shown.staged_memories.insert("stage".into(), StagedMemory {
        call_ref: "stage".into(), memory_id: "new-memory".into(), before: None, after: Some("note".into()),
        base: MemoryBase::absent(), origin: AutomaticMemoryOrigin::Agent, essential: true,
    });
    for (scope, active) in [("main", Some("running")), ("task:work", None)] {
        store.connection.execute(
            "INSERT INTO scheduler_sessions(conversation_id,scope,next_sequence,covers,owner_epoch,permission_version,shown,active)
             VALUES('c',?,1,'{}',1,1,?,?)",
            params![scope, serde_json::to_string(&shown)?, active],
        )?;
    }
    assert_eq!(store.conversation_memory_staged_count("c")?, 1);
    assert_eq!(store.conversation_memory_staged_count("c")?, store.scheduler_memory_staged_count("c")?);
    let all = pages(|cursor| store.conversation_memory_conflicts_page(&f.network, "c", cursor, 1))?;
    assert_eq!(all.len(), 4);
    assert_eq!(all.iter().map(|record| &record.history_id).collect::<std::collections::BTreeSet<_>>().len(), 4);
    assert_eq!(all.iter().map(|record| &record.conflict).collect::<Vec<_>>(), [&conflict, &conflict, &conflict, &conflict]);
    assert_eq!(serde_json::to_value(all.iter().map(|r| &r.conflict).collect::<Vec<_>>())?,
        serde_json::to_value(store.scheduler_memory_conflicts("c")?)?);
    let first = store.conversation_memory_conflicts_page(&f.network, "c", None, 1)?;
    let cursor = first.next_cursor.context("more conflicts")?;
    store.connection.execute(
        "UPDATE scheduler_sessions SET shown=json_set(shown,'$.memory_conflicts[0].reason','changed') WHERE conversation_id='c' AND scope='task:work'", [],
    )?;
    assert!(store.conversation_memory_conflicts_page(&f.network, "c", Some(&cursor), 1)
        .unwrap_err().to_string().contains("refresh"));
    let old_ids = all.iter().map(|record| &record.history_id).collect::<std::collections::BTreeSet<_>>();
    let mut moved = shown;
    let mut other = conflict;
    other.reason = "unrelated new conflict".into();
    moved.memory_conflicts.insert(0, other);
    store.connection.execute("UPDATE scheduler_sessions SET shown=? WHERE conversation_id='c' AND scope='task:work'",
        [serde_json::to_string(&moved)?])?;
    let new = pages(|cursor| store.conversation_memory_conflicts_page(&f.network, "c", cursor, 2))?;
    let new_ids = new.iter().map(|record| &record.history_id).collect::<std::collections::BTreeSet<_>>();
    assert!(old_ids.is_subset(&new_ids), "moving a conflict leaves its identity stable");
    Ok(())
}

#[test]
fn signed_memory_branches_user_control_and_all_conflict_pages_match_old_oracle() -> Result<()> {
    let f = Fixture::new()?;
    let mut a = f.seeded()?;
    let remember = |id: Option<String>, text: &str| ChangeRequest::Remember { memory_id: id, text: text.into() };
    a.change_bot(&f.a, &f.network, "c", "message:m1", "seed", remember(None, "seed"), Provider::Codex, 4)?;
    let memory = a.memory_records(&f.network, "c")?.remove(0).entry.memory_id;
    a.change_bot(&f.a, &f.network, "c", "message:m1", "linear", remember(Some(memory.clone()), "linear"), Provider::Codex, 5)?;
    assert!(assert_memory_conflicts(&a, &f.network)?.is_empty());
    let mut b = f.store()?;
    f.sync(&a, &mut b)?;
    for (store, author, call, text) in [(&mut a, &f.a, "left", "left branch"), (&mut b, &f.b, "right", "right branch")] {
        store.change_bot(author, &f.network, "c", "message:m1", call, remember(Some(memory.clone()), text), Provider::Codex, 6)?;
    }
    f.sync(&b, &mut a)?;
    let conflicts = assert_memory_conflicts(&a, &f.network)?;
    assert_eq!(conflicts.len(), 1);
    assert_eq!(conflicts[0].reason, MemorySyncConflictReason::ConcurrentAutomatic);
    let current = a.memory_state(&f.network, "c", &memory, None)?.current.context("memory")?;
    let body = f.a.fixture_admin().attest(&f.network, "c", EventBody::MemoryControlled {
        memory_id: memory, base: current.metadata.base(), after: current.text, attestation: None,
    })?;
    a.append_event(&f.a, &f.network, "c", body, vec![], 7)?;
    assert_eq!(assert_memory_conflicts(&a, &f.network)?[0].reason, MemorySyncConflictReason::UserProtected);
    for index in 0..55 {
        let body = f.a.fixture_admin().attest(&f.network, "c", EventBody::MemoryControlled {
            memory_id: "crowded".into(), base: MemoryBase::absent(),
            after: Some(format!("private candidate {index}")), attestation: None,
        })?;
        a.append_event(&f.a, &f.network, "c", body, vec![], 10 + index)?;
    }
    let expected = assert_memory_conflicts(&a, &f.network)?;
    assert_eq!(expected.len(), 55);
    assert!(!serde_json::to_string(&expected)?.contains("private candidate"));
    rebuild_projection(&a.connection, &f.network)?;
    assert_eq!(assert_memory_conflicts(&a, &f.network)?, expected);
    let mut late = f.store()?;
    f.sync(&a, &mut late)?;
    assert_eq!(assert_memory_conflicts(&late, &f.network)?, expected);
    a.connection.execute("UPDATE events SET effective=0 WHERE network_id=?", [&f.network])?;
    reduce::clear(&a.connection, &f.network)?;
    log::recompute(&a.connection, &membership_from(&a.connection, &f.network)?)?;
    assert_eq!(assert_memory_conflicts(&a, &f.network)?, expected);
    Ok(())
}
