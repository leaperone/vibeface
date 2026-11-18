//! NSIS records absent paths from its compiled candidate plan before removing the prior
//! installation. Only a completed vacancy check publishes the record to the helper's stage.
use super::*;
use std::{collections::BTreeSet, io};

pub(super) const PLAN: &str = "vibeface-absent-paths.txt";
const HEADER: &str = "vibeface-absent-update-paths-v1\r\n";
const FOOTER: &str = "end-vibeface-absent-paths\r\n";

pub(super) fn remove_additions(root: &Path, stage: &Path, old: &BTreeSet<PathBuf>) -> Result<bool> {
    let plan = stage.join(PLAN);
    match fs::symlink_metadata(&plan) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(false),
        result => {
            result?;
        }
    }
    let bytes = read_small(&plan, 16 << 20)?;
    let records = std::str::from_utf8(&bytes)?
        .strip_prefix(HEADER)
        .and_then(|s| s.strip_suffix(FOOTER))
        .context("incomplete installer rollback record")?;
    ensure!(
        records.is_empty() || records.ends_with("\r\n"),
        "truncated installer rollback entry"
    );
    let protected: BTreeSet<_> = old
        .iter()
        .flat_map(|p| p.ancestors())
        .map(|p| p.to_string_lossy().replace('\\', "/").to_ascii_lowercase())
        .collect();
    let mut unique = BTreeSet::new();
    let mut files = Vec::new();
    let mut dirs = Vec::new();
    for line in records.split_terminator("\r\n") {
        let (kind, path) = line
            .split_once('|')
            .context("invalid installer rollback record")?;
        let name = path.replace('\\', "/");
        let key = name.to_ascii_lowercase();
        ensure!(
            unique.len() < 16384
                && portable::safe_relative(&name)
                && !key.starts_with(".vibeface-update-")
                && !protected.contains(&key)
                && unique.insert(key),
            "invalid, pre-existing or duplicate installer rollback path"
        );
        let dest = root.join(name);
        match kind {
            "F" => files.push(dest),
            "D" => dirs.push(dest),
            _ => bail!("unknown installer rollback path type"),
        }
    }
    dirs.sort_by_key(|path| std::cmp::Reverse(path.components().count()));
    for (paths, directory) in [(files, false), (dirs, true)] {
        for dest in paths {
            match fs::symlink_metadata(&dest) {
                Ok(metadata) => {
                    check_path(&dest)?;
                    ensure!(
                        metadata.is_dir() == directory,
                        "installer rollback path changed type: {}",
                        dest.display()
                    );
                    if directory {
                        // A concurrent user file inside this new directory must survive.
                        fs::remove_dir(&dest)?;
                    } else {
                        fs::remove_file(&dest)?;
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => (),
                Err(error) => return Err(error.into()),
            }
        }
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn removes_only_newly_created_candidate_paths_after_partial_copy() -> Result<()> {
        let root = Stage::new(&fs::canonicalize(std::env::temp_dir())?)?;
        let stage = Stage::new(&root.path)?;
        let resources = root.path.join("resources");
        fs::create_dir_all(resources.join("new/sub"))?;
        let old = BTreeSet::from([PathBuf::from("resources/keep")]);
        for name in ["keep", "notes", "new/sub/copied"] {
            fs::write(resources.join(name), name)?;
        }
        assert!(!remove_additions(&root.path, &stage.path, &old)?);
        let list = "D|resources\\new\r\nD|resources\\new\\sub\r\nF|resources\\new\\sub\\copied\r\nF|resources\\never-copied\r\n";
        fs::write(stage.path.join(PLAN), format!("{HEADER}{list}{FOOTER}"))?;
        assert!(remove_additions(&root.path, &stage.path, &old)?);
        assert!(!resources.join("new").exists());
        assert_eq!(fs::read(resources.join("keep"))?, b"keep");
        assert_eq!(fs::read(resources.join("notes"))?, b"notes");
        Ok(())
    }

    #[test]
    fn validates_whole_record_before_touching_payload() -> Result<()> {
        let root = Stage::new(&fs::canonicalize(std::env::temp_dir())?)?;
        let stage = Stage::new(&root.path)?;
        let old = BTreeSet::from([PathBuf::from("owned")]);
        fs::write(root.path.join("new"), b"candidate")?;
        for suffix in ["", "F|../elsewhere\r\n", "D|NEW\r\n", "F|OWNED\r\n"] {
            let contents = format!(
                "{HEADER}F|new\r\n{suffix}{}",
                if suffix.is_empty() { "" } else { FOOTER }
            );
            fs::write(stage.path.join(PLAN), contents)?;
            assert!(remove_additions(&root.path, &stage.path, &old).is_err());
            assert_eq!(fs::read(root.path.join("new"))?, b"candidate");
        }
        fs::create_dir(root.path.join("new-dir"))?;
        fs::write(root.path.join("new-dir/notes"), b"keep")?;
        fs::write(
            stage.path.join(PLAN),
            format!("{HEADER}D|new-dir\r\n{FOOTER}"),
        )?;
        assert!(remove_additions(&root.path, &stage.path, &old).is_err());
        assert_eq!(fs::read(root.path.join("new-dir/notes"))?, b"keep");
        Ok(())
    }
}
