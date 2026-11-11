//! Indexable facts derived from effective events, cleared with the other projections.
use super::*;
use crate::providers::Provider;
use serde::{Deserialize, Serialize};

#[derive(Deserialize, Serialize)]
enum Saw {
    Value(Value),
    Version(Option<String>),
}

fn provider_number(provider: Provider) -> i64 {
    match provider {
        Provider::Codex => 0,
        Provider::Cursor => 1,
        Provider::Claude => 2,
        Provider::Pi => 3,
        Provider::Opencode => 4,
    }
}

pub(super) fn setting(number: i64) -> Result<bot_settings::Setting> {
    use bot_settings::Setting;
    let provider = |number| match number {
        0 => Ok(Provider::Codex),
        1 => Ok(Provider::Cursor),
        2 => Ok(Provider::Claude),
        3 => Ok(Provider::Pi),
        4 => Ok(Provider::Opencode),
        _ => bail!("invalid setting provider"),
    };
    Ok(match number {
        0 => Setting::Name,
        1 => Setting::Instructions,
        2 => Setting::Skills,
        3 => Setting::Avatar,
        4..=8 => Setting::Model {
            provider: provider(number - 4)?,
        },
        9..=13 => Setting::ReasoningEffort {
            provider: provider(number - 9)?,
        },
        _ => bail!("invalid setting projection"),
    })
}

fn built_on(saw: &str, earlier_hash: &str, earlier_after: &str) -> Result<bool> {
    Ok(match serde_json::from_str::<Saw>(saw)? {
        Saw::Value(value) => value == serde_json::from_str::<Value>(earlier_after)?,
        Saw::Version(version) => version.as_deref() == Some(super::version(earlier_hash).as_str()),
    })
}

pub(super) fn clear_projection(connection: &Connection, network: &str) -> Result<()> {
    for table in [
        "conversation_setting_writes",
        "memory_history_operations",
        "memory_winner_chain",
    ] {
        connection.execute(
            &format!("DELETE FROM {table} WHERE network_id=?"),
            [network],
        )?;
    }
    Ok(())
}

/// Call after the ordinary reducer so the current memory winner is already available.
pub(super) fn apply_projection(
    connection: &Connection,
    network: &str,
    hash: &str,
    event: &Event,
    rank: &str,
) -> Result<()> {
    let mut writes: Vec<(i64, Option<&str>, Saw, Value)> = Vec::new();
    match &event.body {
        EventBody::BotChanged {
            change_id,
            change:
                BotChange::Profile {
                    name,
                    instructions,
                    model,
                    reasoning_effort,
                    skills,
                    avatar,
                },
            ..
        } => {
            for (number, edit) in [(0, name), (1, instructions)] {
                if let Some(edit) = edit {
                    writes.push((
                        number,
                        Some(change_id),
                        Saw::Value(serde_json::to_value(&edit.before)?),
                        serde_json::to_value(&edit.after)?,
                    ));
                }
            }
            if let Some(edit) = skills {
                writes.push((
                    2,
                    Some(change_id),
                    Saw::Value(serde_json::to_value(&edit.before)?),
                    serde_json::to_value(&edit.after)?,
                ));
            }
            if let Some(edit) = avatar {
                writes.push((
                    3,
                    Some(change_id),
                    Saw::Value(serde_json::to_value(edit.before)?),
                    serde_json::to_value(edit.after)?,
                ));
            }
            if let Some(edit) = model {
                writes.push((
                    4 + provider_number(edit.provider),
                    Some(change_id),
                    Saw::Value(serde_json::to_value(&edit.before)?),
                    serde_json::to_value(&edit.after)?,
                ));
            }
            if let Some(edit) = reasoning_effort {
                writes.push((
                    9 + provider_number(edit.provider),
                    Some(change_id),
                    Saw::Value(serde_json::to_value(&edit.before)?),
                    serde_json::to_value(&edit.after)?,
                ));
            }
        }
        EventBody::BotAvatarSet {
            version, avatar, ..
        } => {
            writes.push((
                3,
                None,
                Saw::Version(version.clone()),
                serde_json::to_value(avatar)?,
            ));
        }
        _ => {}
    }
    let by = event
        .body
        .attestation()
        .map_or(&event.author, |attestation| &attestation.device_id);
    for (number, change_id, saw, after) in writes {
        connection.execute(
            "INSERT INTO conversation_setting_writes(network_id,conversation_id,setting,rank,event_hash,change_id,by,saw,after_value,built_on_previous)
             VALUES(?,?,?,?,?,?,?,?,?,0) ON CONFLICT(network_id,conversation_id,setting,rank) DO NOTHING",
            params![network, event.scope_id, number, rank, hash, change_id, by,
                serde_json::to_string(&saw)?, serde_json::to_string(&after)?],
        )?;
        // A late insertion changes only its own adjacency and that of its immediate successor.
        for candidate in [Some(rank.to_owned()), connection.query_row(
            "SELECT rank FROM conversation_setting_writes WHERE network_id=? AND conversation_id=? AND setting=? AND rank>? ORDER BY rank LIMIT 1",
            params![network, event.scope_id, number, rank], |row| row.get(0),
        ).optional()?].into_iter().flatten() {
            let saw: String = connection.query_row(
                "SELECT saw FROM conversation_setting_writes WHERE network_id=? AND conversation_id=? AND setting=? AND rank=?",
                params![network, event.scope_id, number, candidate], |row| row.get(0),
            )?;
            let previous: Option<(String, String)> = connection.query_row(
                "SELECT event_hash,after_value FROM conversation_setting_writes WHERE network_id=? AND conversation_id=? AND setting=? AND rank<? ORDER BY rank DESC LIMIT 1",
                params![network, event.scope_id, number, candidate], |row| Ok((row.get(0)?, row.get(1)?)),
            ).optional()?;
            let observed = previous.map(|(hash, after)| built_on(&saw, &hash, &after)).transpose()?.unwrap_or(false);
            connection.execute(
                "UPDATE conversation_setting_writes SET built_on_previous=? WHERE network_id=? AND conversation_id=? AND setting=? AND rank=?",
                params![observed, network, event.scope_id, number, candidate],
            )?;
        }
    }
    if let Some(operation) = bot_settings::memory_operation(network, hash, event)? {
        connection.execute(
            "INSERT INTO memory_history_operations(network_id,conversation_id,memory_id,revision,base,user_controlled,source_event_hash,winning)
             VALUES(?1,?2,?3,?4,?5,?6,?7,EXISTS(SELECT 1 FROM memory_winner_chain WHERE network_id=?1 AND conversation_id=?2 AND memory_id=?3 AND revision=?4))
             ON CONFLICT(network_id,conversation_id,memory_id,revision)
             DO UPDATE SET source_event_hash=min(source_event_hash,excluded.source_event_hash)",
            params![network, event.scope_id, operation.memory_id, operation.version.revision,
                serde_json::to_string(&operation.base)?, operation.user_controlled, hash],
        )?;
        update_winner_chain(connection, network, &event.scope_id, &operation.memory_id)?;
    }
    Ok(())
}

fn update_winner_chain(
    connection: &Connection,
    network: &str,
    conversation: &str,
    memory: &str,
) -> Result<()> {
    let Some(winner) = bot_settings::memory_state_from(connection, network, conversation, memory)?
    else {
        return Ok(());
    };
    let head: Option<String> = connection.query_row(
        "SELECT revision FROM memory_winner_chain WHERE network_id=? AND conversation_id=? AND memory_id=? ORDER BY depth DESC LIMIT 1",
        params![network, conversation, memory], |row| row.get(0),
    ).optional()?;
    if head.as_deref() == Some(&winner.metadata.revision) {
        return Ok(());
    }
    let mut branch = Vec::new();
    let mut visited = std::collections::BTreeSet::new();
    let mut cursor = winner.metadata.base().version;
    let mut shared_depth = -1_i64;
    while let Some(version) = cursor {
        ensure!(
            visited.insert(version.revision.clone()),
            "memory operation ancestry has a cycle"
        );
        let depth: Option<i64> = connection.query_row(
            "SELECT depth FROM memory_winner_chain WHERE network_id=? AND conversation_id=? AND memory_id=? AND revision=?",
            params![network, conversation, memory, version.revision], |row| row.get(0),
        ).optional()?;
        if let Some(depth) = depth {
            shared_depth = depth;
            break;
        }
        let base: Option<String> = connection.query_row(
            "SELECT base FROM memory_history_operations WHERE network_id=? AND conversation_id=? AND memory_id=? AND revision=?",
            params![network, conversation, memory, version.revision], |row| row.get(0),
        ).optional()?;
        cursor = match base {
            Some(base) => serde_json::from_str::<MemoryBase>(&base)?.version,
            None => {
                let source = log::event_from(connection, network, &version.source_event_hash)?
                    .context("memory base is not stored here")?;
                bot_settings::memory_operation(network, &version.source_event_hash, &source)?
                    .context("memory base is not a memory operation")?
                    .base
                    .version
            }
        };
        branch.push(version.revision);
    }
    connection.execute(
        "UPDATE memory_history_operations SET winning=0 WHERE network_id=?1 AND conversation_id=?2 AND memory_id=?3
         AND revision IN (SELECT revision FROM memory_winner_chain WHERE network_id=?1 AND conversation_id=?2 AND memory_id=?3 AND depth>?4)",
        params![network, conversation, memory, shared_depth],
    )?;
    connection.execute(
        "DELETE FROM memory_winner_chain WHERE network_id=? AND conversation_id=? AND memory_id=? AND depth>?",
        params![network, conversation, memory, shared_depth],
    )?;
    for revision in branch.into_iter().rev() {
        shared_depth += 1;
        connection.execute(
            "INSERT INTO memory_winner_chain(network_id,conversation_id,memory_id,revision,depth) VALUES(?,?,?,?,?)",
            params![network, conversation, memory, revision, shared_depth],
        )?;
        connection.execute(
            "UPDATE memory_history_operations SET winning=1 WHERE network_id=? AND conversation_id=? AND memory_id=? AND revision=?",
            params![network, conversation, memory, revision],
        )?;
    }
    Ok(())
}

/// One transaction during installation of these derived tables, never on snapshot reads.
pub(super) fn rebuild_projection(connection: &Connection, network: &str) -> Result<()> {
    clear_projection(connection, network)?;
    let mut statement = connection.prepare(
        "SELECT hash,envelope,lamport,created_at_ms FROM events WHERE network_id=? AND effective=1
         AND kind IN ('bot_changed','bot_avatar_set','memory_controlled') ORDER BY lamport,created_at_ms,hash",
    )?;
    let mut rows = statement.query([network])?;
    while let Some(row) = rows.next()? {
        let hash: String = row.get(0)?;
        let signed: SignedEvent = serde_json::from_str(&row.get::<_, String>(1)?)?;
        let event: Event = serde_json::from_str(&signed.canonical_body)?;
        apply_projection(
            connection,
            network,
            &hash,
            &event,
            &super::rank(row.get(2)?, row.get(3)?, &hash),
        )?;
    }
    Ok(())
}

#[cfg(test)]
#[path = "conversation_settings_projection_tests.rs"]
mod tests;
