use super::*;
use std::collections::BTreeSet;

#[cfg(windows)]
pub(super) const GUI: &str = "Vibeface.exe";
#[cfg(target_os = "linux")]
pub(super) const GUI: &str = "vibeface";

const STAMP: &str = "resources/desktop-build.json";
const SEAL: &str = "resources/runtime/FILES.sha256";
#[cfg(windows)]
const PLATFORM_FILES: &[&str] = &[".vibeface-installed-files.json"];
#[cfg(target_os = "linux")]
const PLATFORM_FILES: &[&str] = &[
    "AppRun",
    "vibeface.desktop",
    "vibeface.png",
    "native-dependencies.json",
];

pub(super) fn check_layout(root: &Path) -> Result<()> {
    for name in [GUI, STAMP, SEAL, "resources/NATIVE-REQUIREMENTS.md"]
        .iter()
        .chain(PLATFORM_FILES)
    {
        let path = root.join(name);
        check_path(&path)?;
        ensure!(path.is_file(), "missing portable Desktop file {name}");
    }
    Ok(())
}

fn safe_relative(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 768
        && name.split('/').all(|part| {
            let stem = part.split('.').next().unwrap_or("").to_ascii_uppercase();
            let device = matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
                || (stem.len() == 4
                    && (stem.starts_with("COM") || stem.starts_with("LPT"))
                    && stem.as_bytes()[3].is_ascii_digit());
            !part.is_empty()
                && !part.ends_with('.')
                && !device
                && part
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
        })
}

/// The original archive has four fixed Linux integration files (one manifest on Windows),
/// known Desktop resources, and a checksum-sealed CLI payload. Unknown siblings are user data.
pub(super) fn owned_files(root: &Path) -> Result<BTreeSet<PathBuf>> {
    check_layout(root)?;
    let mut files: BTreeSet<_> = [GUI, STAMP, SEAL, "resources/NATIVE-REQUIREMENTS.md"]
        .into_iter()
        .chain(PLATFORM_FILES.iter().copied())
        .map(PathBuf::from)
        .collect();
    let mut names = BTreeSet::new();
    for line in std::str::from_utf8(&read_small(&root.join(SEAL), 4 << 20)?)?.lines() {
        let (digest, name) = line
            .split_once("  ")
            .context("invalid runtime checksum list")?;
        ensure!(
            digest.len() == 64
                && digest.bytes().all(|b| b.is_ascii_hexdigit())
                && safe_relative(name)
                && !name.eq_ignore_ascii_case("FILES.sha256")
                && names.insert(name.to_ascii_lowercase()),
            "invalid or duplicate runtime checksum entry"
        );
        let relative = Path::new("resources/runtime").join(name);
        ensure!(
            hash(&root.join(&relative))? == digest,
            "installed package file differs from its checksum: {name}"
        );
        files.insert(relative);
    }
    for name in [
        "BUILD-INFO.json",
        "web/index.html",
        if cfg!(windows) {
            "vibeface.exe"
        } else {
            "vibeface"
        },
        if cfg!(windows) {
            "vibeface-core.exe"
        } else {
            "vibeface-core"
        },
    ] {
        ensure!(
            files.contains(&Path::new("resources/runtime").join(name)),
            "missing packaged runtime file {name}"
        );
    }
    Ok(files)
}

fn list_files(root: &Path, dir: &Path, found: &mut BTreeSet<PathBuf>) -> Result<()> {
    for entry in fs::read_dir(dir)? {
        let path = entry?.path();
        check_path(&path)?;
        if path.is_dir() {
            list_files(root, &path, found)?;
        } else {
            found.insert(path.strip_prefix(root)?.into());
        }
    }
    Ok(())
}

pub(super) fn activate(
    current: &Installation,
    asset: &Path,
    candidate: &Build,
    signed: bool,
) -> Result<()> {
    let live = &current.path;
    current.check_current(&live.join("resources"), &live.join(GUI))?;
    #[cfg(windows)]
    windows::require_portable(live)?;
    let mut stage = Stage::new(live)?;
    let next = stage.path.join("payload");
    fs::create_dir(&next)?;
    #[cfg(target_os = "linux")]
    {
        let _ = signed;
        run(Command::new("/usr/bin/unzip")
            .env_remove("UNZIP")
            .env_remove("UNZIPOPT")
            .arg("-q")
            .arg(asset)
            .arg("-d")
            .arg(&next))?;
    }
    #[cfg(windows)]
    {
        run(Command::new(windows::system_tool("tar.exe")?)
            .arg("-xf")
            .arg(asset)
            .arg("-C")
            .arg(&next))?;
        windows::verify_candidate(&live.join(GUI), &next.join(GUI), signed)?;
    }
    ensure!(
        &read_build(&next.join("resources"))? == candidate,
        "portable Desktop update build identity mismatch"
    );
    let new = owned_files(&next)?;
    let mut extracted = BTreeSet::new();
    list_files(&next, &next, &mut extracted)?;
    ensure!(extracted == new, "portable archive has unexpected files");
    let old = owned_files(live)?;
    current.check_current(&live.join("resources"), &live.join(GUI))?;
    apply_files(live, &next, &old, &new, &mut stage)
}

/// Snapshot every old file before any mutation. Renaming each new file over the old one
/// keeps the GUI executable in place even when the filesystem refuses a later replacement.
fn apply_files(
    live: &Path,
    next: &Path,
    old: &BTreeSet<PathBuf>,
    new: &BTreeSet<PathBuf>,
    stage: &mut Stage,
) -> Result<()> {
    for rel in new {
        for ancestor in live.join(rel).ancestors() {
            match fs::symlink_metadata(ancestor) {
                Ok(_) => {
                    check_path(ancestor)?;
                    if ancestor == live.join(rel) {
                        ensure!(
                            old.contains(rel),
                            "a file already exists at the new package path {}",
                            ancestor.display()
                        );
                    } else {
                        ensure!(
                            ancestor.is_dir(),
                            "package directory is occupied: {}",
                            ancestor.display()
                        );
                    }
                    break;
                }
                Err(e) if e.kind() == io::ErrorKind::NotFound => (),
                Err(e) => return Err(e.into()),
            }
        }
    }
    let backup = stage.path.join("previous");
    backup_files(live, &backup, old)?;
    let mut changed = Vec::new();
    let result = (|| -> Result<()> {
        for rel in old.difference(new) {
            fs::remove_file(live.join(rel))?;
            changed.push(rel);
        }
        // Install the version stamp last, after every file named by it is present.
        for rel in new
            .iter()
            .filter(|p| p.as_path() != Path::new(STAMP))
            .chain(new.get(Path::new(STAMP)))
        {
            let dest = live.join(rel);
            fs::create_dir_all(parent(&dest)?)?;
            if old.contains(rel) {
                fs::rename(next.join(rel), &dest)?;
            } else {
                // A hard link creates a fresh name without clobbering an unowned file if it
                // appeared after the vacancy check. Both paths are on the same volume.
                fs::hard_link(next.join(rel), &dest)?;
            }
            changed.push(rel);
        }
        privatefs::sync_dir(live)
    })();
    if let Err(error) = result {
        for rel in changed.into_iter().rev() {
            let dest = live.join(rel);
            let restored = if old.contains(rel) {
                fs::rename(backup.join(rel), dest)
            } else {
                fs::remove_file(dest)
            };
            if let Err(rollback) = restored {
                stage.keep = true;
                bail!(
                    "{error:#}; rollback failed ({rollback}); previous release files at {}",
                    backup.display()
                );
            }
        }
        return Err(error);
    }
    Ok(())
}

pub(super) fn backup_files(live: &Path, backup: &Path, files: &BTreeSet<PathBuf>) -> Result<()> {
    for rel in files {
        let from = live.join(rel);
        let to = backup.join(rel);
        check_path(&from)?;
        fs::create_dir_all(parent(&to)?)?;
        fs::copy(from, &to)?;
        fs::OpenOptions::new().write(true).open(to)?.sync_all()?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths(names: &[&str]) -> BTreeSet<PathBuf> {
        names.iter().map(PathBuf::from).collect()
    }

    #[test]
    fn archive_name_boundaries() {
        for name in ["web/a-v2.js", "scripts/service.ps1"] {
            assert!(safe_relative(name));
        }
        for name in [
            "",
            "/usr/bin/x",
            "../x",
            "web/../x",
            "a//b",
            "a.",
            "x.lpt/aUx.txt",
            "x:y",
            "x\\a",
            "a/./b",
        ] {
            assert!(!safe_relative(name), "{name}");
        }
    }

    #[test]
    fn replaces_owned_files_and_preserves_neighbors_and_collision() -> Result<()> {
        let live = Stage::new(&fs::canonicalize(std::env::temp_dir())?)?;
        let mut stage = Stage::new(&live.path)?;
        let next = stage.path.join("new");
        fs::create_dir(&next)?;
        for name in ["owned", "obsolete", "notes"] {
            fs::write(live.path.join(name), name)?;
        }
        for name in ["owned", "fresh", "notes"] {
            fs::write(next.join(name), b"update")?;
        }
        let old = paths(&["owned", "obsolete"]);
        let new = paths(&["owned", "fresh", "notes"]);
        assert!(apply_files(&live.path, &next, &old, &new, &mut stage).is_err());
        assert_eq!(fs::read(live.path.join("obsolete"))?, b"obsolete");
        assert_eq!(fs::read(live.path.join("notes"))?, b"notes");
        apply_files(
            &live.path,
            &next,
            &old,
            &paths(&["owned", "fresh"]),
            &mut stage,
        )?;
        assert_eq!(fs::read(live.path.join("owned"))?, b"update");
        assert_eq!(fs::read(live.path.join("notes"))?, b"notes");
        assert!(!live.path.join("obsolete").try_exists()?);
        Ok(())
    }

    #[test]
    fn restores_prior_files_on_partial_copy_failure() -> Result<()> {
        let live = Stage::new(&fs::canonicalize(std::env::temp_dir())?)?;
        let mut stage = Stage::new(&live.path)?;
        let next = stage.path.join("new");
        fs::create_dir(&next)?;
        fs::write(next.join("a"), b"replacement")?;
        for name in ["a", "obsolete"] {
            fs::write(live.path.join(name), name)?;
        }
        let result = apply_files(
            &live.path,
            &next,
            &paths(&["a", "obsolete"]),
            &paths(&["a", "missing"]),
            &mut stage,
        );
        assert!(result.is_err());
        assert_eq!(fs::read(live.path.join("a"))?, b"a");
        assert_eq!(fs::read(live.path.join("obsolete"))?, b"obsolete");
        Ok(())
    }
}

#[cfg(all(test, target_os = "linux"))]
mod packaged_tests {
    use super::*;

    fn fixture(root: &Path, build: &Build) -> Result<()> {
        let runtime = root.join("resources/runtime");
        fs::create_dir_all(runtime.join("web"))?;
        fs::write(root.join(STAMP), serde_json::to_vec(build)?)?;
        fs::write(
            root.join("resources/NATIVE-REQUIREMENTS.md"),
            "requirements",
        )?;
        for name in PLATFORM_FILES {
            fs::write(root.join(name), name)?;
        }
        let mut seal = String::new();
        for name in [
            "vibeface",
            "vibeface-core",
            "BUILD-INFO.json",
            "web/index.html",
        ] {
            fs::write(runtime.join(name), &build.version)?;
            seal += &format!("{}  {name}\n", hash(&runtime.join(name))?);
        }
        fs::write(root.join(SEAL), seal)?;
        Ok(())
    }

    #[test]
    fn published_flat_zip_updates_the_detected_package() -> Result<()> {
        let temp = Stage::new(&fs::canonicalize(std::env::temp_dir())?)?;
        let old_root = temp.path.join("existing");
        let next_root = temp.path.join("release");
        let old = Build {
            version: "1.0.0".into(),
            source_commit: "a".repeat(40),
        };
        let new = Build {
            version: "1.1.0".into(),
            source_commit: "b".repeat(40),
        };
        fixture(&old_root, &old)?;
        fixture(&next_root, &new)?;
        fs::write(old_root.join(GUI), "existing GUI")?;
        fs::write(next_root.join(GUI), "candidate GUI")?;
        fs::write(old_root.join("resources/runtime/web/personal.txt"), "keep")?;
        let archive = temp.path.join("package.zip");
        run(Command::new("python3")
            .args(["-m", "zipfile", "-c"])
            .arg(&archive)
            .arg(GUI)
            .arg("resources")
            .args(PLATFORM_FILES)
            .current_dir(&next_root))?;
        let install =
            Installation::detect(&old_root.join(GUI))?.context("fixture package not detected")?;
        assert_eq!(install.asset_suffix(), ".zip");
        assert_eq!(install.key(), old_root);
        assert!(
            install
                .activate(
                    &archive,
                    &old.version,
                    &new.version,
                    &"c".repeat(40),
                    false,
                    None
                )
                .is_err()
        );
        assert_eq!(read_build(&old_root.join("resources"))?, old);
        install.activate(
            &archive,
            &old.version,
            &new.version,
            &new.source_commit,
            false,
            None,
        )?;
        assert_eq!(read_build(&old_root.join("resources"))?, new);
        assert_eq!(
            fs::read(old_root.join("resources/runtime/web/personal.txt"))?,
            b"keep"
        );
        assert!(
            install
                .activate(
                    &archive,
                    &old.version,
                    &new.version,
                    &new.source_commit,
                    false,
                    None
                )
                .is_err()
        );
        Ok(())
    }
}
