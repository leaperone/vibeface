use super::*;
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Serialize, de::DeserializeOwned};
use vibeface_secure::authorization::{MAX_PROOF_BYTES, PolicyPin};

pub(super) fn key(kind: &str, reference: &ObjectHash) -> String {
    format!("task-business-policy-v2:{kind}:{reference}")
}

pub(super) fn encoded<T: Serialize>(value: &T) -> Result<String> {
    let value = serde_json::to_string(value)?;
    ensure!(
        value.len() <= MAX_PROOF_BYTES * 2,
        "Task business metadata exceeds its storage bound"
    );
    Ok(value)
}

pub(super) fn read<T: DeserializeOwned>(connection: &Connection, name: &str) -> Result<Option<T>> {
    let value: Option<(usize, String)> = connection.query_row(
        "SELECT length(CAST(value AS BLOB)),CASE WHEN length(CAST(value AS BLOB))<=? THEN value ELSE '' END FROM identity_meta WHERE key=?",
        params![MAX_PROOF_BYTES * 2, name], |row| Ok((row.get(0)?,row.get(1)?)),
    ).optional()?;
    value
        .map(|(length, value)| {
            ensure!(
                length <= MAX_PROOF_BYTES * 2,
                "stored Task business metadata exceeds its bound"
            );
            serde_json::from_str(&value).context("invalid stored Task business metadata")
        })
        .transpose()
}

pub(super) fn immutable<T: Serialize + DeserializeOwned + PartialEq>(
    connection: &Connection,
    name: &str,
    value: &T,
) -> Result<()> {
    if let Some(saved) = read::<T>(connection, name)? {
        ensure!(saved == *value, "Task business immutable metadata changed");
        return Ok(());
    }
    connection.execute(
        "INSERT INTO identity_meta(key,value) VALUES(?,?)",
        params![name, encoded(value)?],
    )?;
    Ok(())
}

fn floor_key(pin: &PolicyPin) -> Result<String> {
    Ok(key(
        "source-floor",
        &ObjectHash::body(&(&pin.domain, &pin.genesis_root))?,
    ))
}

pub(super) fn floors(connection: &Connection, approved: &[PolicyPin]) -> Result<Vec<PolicyPin>> {
    let mut result = Vec::new();
    for pin in approved {
        if let Some(saved) = read::<PolicyPin>(connection, &floor_key(pin)?)? {
            ensure!(
                saved.domain == pin.domain && saved.genesis_root == pin.genesis_root,
                "Task business floor changed its grant-approved domain or genesis"
            );
            result.push(saved);
        }
    }
    Ok(result)
}

pub(super) fn advance_floors(
    connection: &Connection,
    source: &VerifiedTaskBusinessSource,
) -> Result<()> {
    source.check_saved_floors(&floors(connection, source.pins())?)?;
    for pin in source.pins() {
        let head = source
            .heads()
            .iter()
            .find(|head| head.domain == pin.domain)
            .context("completed Task business source has no exact domain head")?;
        let saved = PolicyPin {
            domain: pin.domain.clone(),
            genesis_root: pin.genesis_root.clone(),
            minimum_head: Some(head.clone()),
        };
        connection.execute("INSERT INTO identity_meta(key,value) VALUES(?,?) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
            params![floor_key(pin)?,encoded(&saved)?])?;
    }
    Ok(())
}
