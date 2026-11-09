//! `vibeface config set`: changes one config.json value after `init`, checked by the same
//! parser and validation that load config.json.

use crate::config::Config;
use anyhow::{Context, Result, bail, ensure};
use serde_json::Value;
use std::path::Path;

impl Config {
    /// Sets `key`, a dotted path into config.json, to `value` and saves the result, refusing
    /// one `Config::load` would refuse. `mesh.` stands for `network.mesh.`; `null` restores a
    /// field's default. Relay tokens come from files at `init`, never from a command line.
    pub fn set(dir: &Path, key: &str, value: Value) -> Result<()> {
        let path = config_path(key)?;
        ensure!(
            !path.iter().any(|segment| segment.contains("token")) && !names_token(&value),
            "relay tokens are read from files; pass them to vibeface init"
        );
        let mut document: Value = serde_json::from_slice(
            &std::fs::read(dir.join("config.json")).context("config.json is unreadable")?,
        )
        .context("config.json is not JSON")?;
        let (last, parents) = path.split_last().context("empty config key")?;
        let mut slot = &mut document;
        for segment in parents {
            let object = slot.as_object_mut().with_context(|| {
                format!("{key}: {segment} is inside a value that is not an object")
            })?;
            slot = object
                .entry(segment.clone())
                .or_insert_with(|| Value::Object(Default::default()));
        }
        let object = slot
            .as_object_mut()
            .with_context(|| format!("{key} is inside a value that is not an object"))?;
        if value.is_null() {
            object.remove(last);
        } else {
            object.insert(last.clone(), value);
        }
        let config: Config =
            serde_json::from_value(document).with_context(|| format!("invalid {key}"))?;
        config.save(dir).with_context(|| format!("invalid {key}"))
    }
}

fn config_path(key: &str) -> Result<Vec<String>> {
    let mut path: Vec<String> = key.split('.').map(str::to_owned).collect();
    if path.first().is_some_and(|first| first == "mesh") {
        path.insert(0, "network".into());
    }
    for segment in &path {
        if segment.is_empty()
            || !segment
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
        {
            bail!("a config key is a dotted path such as mesh.allow_private");
        }
    }
    Ok(path)
}

fn names_token(value: &Value) -> bool {
    match value {
        Value::Object(object) => object
            .iter()
            .any(|(name, value)| name.contains("token") || names_token(value)),
        Value::Array(items) => items.iter().any(names_token),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn node_dir() -> Result<std::path::PathBuf> {
        let dir =
            std::env::temp_dir().join(format!("vibeface-config-set-{}", uuid::Uuid::new_v4()));
        crate::config::private_dir(&dir)?;
        Config::local("127.0.0.1:4790".parse()?, vec![], false).save(&dir)?;
        Ok(dir)
    }

    #[test]
    fn set_changes_one_value_and_refuses_what_loading_would_refuse() -> Result<()> {
        let dir = node_dir()?;
        Config::set(&dir, "mesh.allow_private", json!(true))?;
        Config::set(&dir, "mesh.network", json!("lab-1"))?;
        Config::set(&dir, "network.mesh.enabled", json!(true))?;
        Config::set(&dir, "timeout_secs", json!(30))?;
        let loaded = Config::load(&dir)?;
        assert!(loaded.network.mesh.allow_private && loaded.network.mesh.enabled);
        assert_eq!(
            (loaded.network.mesh.network.as_str(), loaded.timeout_secs),
            ("lab-1", 30)
        );
        let before = std::fs::read(dir.join("config.json"))?;
        for (key, value, error) in [
            ("mesh.network", json!("Not Valid"), "invalid mesh.network"),
            ("mesh.no_such_field", json!(1), "invalid mesh.no_such_field"),
            ("timeout_secs", json!(0), "invalid timeout_secs"),
            ("network.relay_operator_token", json!("00"), "relay tokens"),
            (
                "network.relays",
                json!([{"url":"https://relay.example","token":"00"}]),
                "relay tokens",
            ),
            ("Mesh..x", json!(1), "dotted path"),
        ] {
            let refused = Config::set(&dir, key, value).unwrap_err();
            assert!(refused.to_string().contains(error), "{key}: {refused:#}");
        }
        assert_eq!(std::fs::read(dir.join("config.json"))?, before);
        Config::set(&dir, "mesh.network", Value::Null)?;
        assert_eq!(Config::load(&dir)?.network.mesh.network, "vibeface");
        std::fs::remove_dir_all(dir)?;
        Ok(())
    }
}
