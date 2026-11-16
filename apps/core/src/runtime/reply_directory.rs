use anyhow::Result;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// Collects directory hints; each provider still verifies its session and native records.
pub(super) fn read(
    replies: &Path,
    mut decode: impl FnMut(&[u8]) -> Result<Option<PathBuf>>,
) -> Result<BTreeSet<PathBuf>> {
    let entries = match std::fs::read_dir(replies) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(BTreeSet::new());
        }
        Err(error) => return Err(error.into()),
    };
    let mut directories = BTreeSet::new();
    for entry in entries {
        let path = entry?.path();
        if path
            .extension()
            .is_some_and(|extension| extension == "json")
            && let Some(directory) = decode(&std::fs::read(path)?)?
        {
            directories.insert(directory);
        }
    }
    Ok(directories)
}
