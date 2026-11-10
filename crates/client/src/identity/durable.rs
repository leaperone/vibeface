use super::*;

/// Publish a complete private file without replacing an existing destination.
/// Both names are in one directory, so the atomic hard link never crosses filesystems.
pub(super) fn publish_new(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .context("identity file has no parent directory")?;
    let name = path
        .file_name()
        .context("identity file has no name")?
        .to_string_lossy();
    let temporary = parent.join(format!(".{name}.{}.pending", uuid::Uuid::new_v4()));
    let result = (|| -> Result<()> {
        let mut file = privatefs::create_new(&temporary)?;
        std::io::Write::write_all(&mut file, bytes)?;
        file.sync_all()?;
        drop(file);
        std::fs::hard_link(&temporary, path)
            .context("publish private identity file without replacing an existing file")?;
        privatefs::sync_dir(parent)?;
        std::fs::remove_file(&temporary)?;
        privatefs::sync_dir(parent)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn interrupted_temporary_file_never_becomes_the_destination_and_existing_files_are_preserved()
    -> Result<()> {
        let dir =
            std::env::temp_dir().join(format!("native-identity-publish-{}", uuid::Uuid::new_v4()));
        privatefs::create_dir(&dir)?;
        let destination = dir.join("recovery.json");
        privatefs::create_durable(
            &dir.join(".recovery.json.interrupted.pending"),
            b"partial bytes",
        )?;
        assert!(!destination.exists());
        publish_new(&destination, b"complete sealed backup")?;
        assert_eq!(std::fs::read(&destination)?, b"complete sealed backup");
        assert!(publish_new(&destination, b"different backup").is_err());
        assert_eq!(std::fs::read(&destination)?, b"complete sealed backup");
        #[cfg(unix)]
        {
            let link = dir.join("symlink.json");
            std::os::unix::fs::symlink(&destination, &link)?;
            assert!(publish_new(&link, b"replacement").is_err());
            assert_eq!(std::fs::read(&destination)?, b"complete sealed backup");
        }
        std::fs::remove_dir_all(dir)?;
        Ok(())
    }
}
