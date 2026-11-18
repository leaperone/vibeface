//! Installation targets identified from the running packaged Desktop. Activation runs after
//! the GUI shuts down, with the verified release asset and the per-installation lock held.
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::{self, Read},
    path::{Path, PathBuf},
    process::Command,
};
use vibeface_bridge::platform::privatefs;

#[cfg(target_os = "linux")]
mod appimage;
#[cfg(target_os = "macos")]
mod mac;
#[cfg(any(target_os = "linux", windows))]
mod portable;
#[cfg(windows)]
mod windows;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Build {
    version: String,
    source_commit: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
enum Format {
    #[cfg(target_os = "linux")]
    AppImage,
    #[cfg(target_os = "macos")]
    AppBundle,
    #[cfg(any(target_os = "linux", windows))]
    Portable,
    #[cfg(windows)]
    Installer,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct Installation {
    format: Format,
    #[cfg(unix)]
    file_id: (u64, u64),
    path: PathBuf,
    build: Build,
}

impl Installation {
    pub(super) fn detect(executable: &Path) -> Result<Option<Self>> {
        #[cfg(target_os = "macos")]
        let target = mac::detect(executable)?;
        #[cfg(target_os = "linux")]
        let target = appimage::detect(executable)?;
        #[cfg(windows)]
        let target = windows::detect(executable)?;
        let Some((format, path, build)) = target else {
            return Ok(None);
        };
        Ok(Some(Self {
            #[cfg(unix)]
            file_id: file_id(executable)?,
            format,
            path,
            build,
        }))
    }

    pub(super) fn asset_suffix(&self) -> &'static str {
        match self.format {
            #[cfg(target_os = "linux")]
            Format::AppImage => ".AppImage",
            #[cfg(target_os = "macos")]
            Format::AppBundle => ".zip",
            #[cfg(any(target_os = "linux", windows))]
            Format::Portable => ".zip",
            #[cfg(windows)]
            Format::Installer => ".exe",
        }
    }

    pub(super) fn key(&self) -> &Path {
        &self.path
    }

    pub(super) fn activate(
        &self,
        asset: &Path,
        current_version: &str,
        new_version: &str,
        new_source_commit: &str,
        require_signature: bool,
        current_asset_sha256: Option<&str>,
    ) -> Result<()> {
        ensure!(
            self.build.version == current_version,
            "Desktop installation version changed"
        );
        ensure!(
            self.path.is_absolute() && asset.is_absolute(),
            "update paths must be absolute"
        );
        check_path(&self.path)?;
        check_path(asset)?;
        let candidate = Build {
            version: new_version.into(),
            source_commit: new_source_commit.into(),
        };
        #[cfg(not(target_os = "linux"))]
        let _ = current_asset_sha256;
        #[cfg(target_os = "linux")]
        let _ = require_signature;
        match self.format {
            #[cfg(target_os = "linux")]
            Format::AppImage => appimage::activate(self, asset, current_asset_sha256),
            #[cfg(target_os = "macos")]
            Format::AppBundle => mac::activate(self, asset, &candidate, require_signature),
            #[cfg(any(target_os = "linux", windows))]
            Format::Portable => portable::activate(self, asset, &candidate, require_signature),
            #[cfg(windows)]
            Format::Installer => windows::activate(self, asset, &candidate, require_signature),
        }
    }

    fn check_current(&self, resources: &Path, executable: &Path) -> Result<()> {
        ensure!(
            read_build(resources)? == self.build,
            "installed Desktop build changed"
        );
        check_path(executable)?;
        #[cfg(unix)]
        let original = file_id(executable)? == self.file_id;
        // The Windows helper is a byte-for-byte copy of the original GUI.
        #[cfg(windows)]
        let original = hash(executable)? == hash(&std::env::current_exe()?)?;
        ensure!(original, "the installed Desktop executable changed");
        Ok(())
    }
}

#[cfg(unix)]
fn file_id(path: &Path) -> Result<(u64, u64)> {
    use std::os::unix::fs::MetadataExt;
    let metadata = fs::metadata(path)?;
    Ok((metadata.dev(), metadata.ino()))
}

type Detected = Option<(Format, PathBuf, Build)>;

fn read_build(resources: &Path) -> Result<Build> {
    let build: Build =
        serde_json::from_slice(&read_small(&resources.join("desktop-build.json"), 4096)?)?;
    ensure!(
        !build.version.is_empty()
            && build.source_commit.len() == 40
            && build
                .source_commit
                .bytes()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()),
        "invalid packaged Desktop build identity"
    );
    Ok(build)
}

fn check_path(path: &Path) -> Result<()> {
    ensure!(
        path.is_absolute()
            && !path
                .components()
                .any(|part| matches!(part, std::path::Component::ParentDir)),
        "expected an absolute normalized package path"
    );
    for ancestor in path.ancestors() {
        let metadata = fs::symlink_metadata(ancestor)
            .with_context(|| format!("inspect {}", ancestor.display()))?;
        ensure!(
            !metadata.is_symlink(),
            "linked package path {}",
            ancestor.display()
        );
        #[cfg(windows)]
        {
            use std::os::windows::fs::MetadataExt;
            ensure!(
                metadata.file_attributes() & 0x400 == 0,
                "reparse point in package path {}",
                ancestor.display()
            );
        }
        ensure!(
            metadata.is_dir() || (ancestor == path && metadata.is_file()),
            "unexpected package path type {}",
            ancestor.display()
        );
    }
    Ok(())
}

fn read_small(path: &Path, limit: u64) -> Result<Vec<u8>> {
    check_path(path)?;
    let file = privatefs::open_no_follow(path)?;
    ensure!(
        file.metadata()?.is_file() && file.metadata()?.len() <= limit,
        "invalid size for {}",
        path.display()
    );
    let mut bytes = Vec::new();
    file.take(limit + 1).read_to_end(&mut bytes)?;
    ensure!(bytes.len() as u64 <= limit, "oversized {}", path.display());
    Ok(bytes)
}

fn hash(path: &Path) -> Result<String> {
    check_path(path)?;
    let mut file = privatefs::open_no_follow(path)?;
    let mut sha = Sha256::new();
    io::copy(&mut file, &mut sha)?;
    Ok(hex::encode(sha.finalize()))
}

fn run(command: &mut Command) -> Result<()> {
    let output = command
        .output()
        .with_context(|| format!("start {}", command.get_program().to_string_lossy()))?;
    ensure!(
        output.status.success(),
        "{} failed ({}): {}",
        command.get_program().to_string_lossy(),
        output.status,
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(())
}

/// Staging and backups are always on the target volume. Keep the private directory when
/// rollback fails so the prior release is still available for recovery.
struct Stage {
    path: PathBuf,
    keep: bool,
}

impl Stage {
    fn new(parent: &Path) -> Result<Self> {
        check_path(parent)?;
        let path = parent.join(format!(".vibeface-update-{}", uuid::Uuid::new_v4()));
        privatefs::create_dir_new(&path)
            .with_context(|| format!("create update staging directory in {}", parent.display()))?;
        Ok(Self { path, keep: false })
    }
}

impl Drop for Stage {
    fn drop(&mut self) {
        if !self.keep {
            let _ = fs::remove_dir_all(&self.path);
        }
    }
}

fn parent(path: &Path) -> Result<&Path> {
    path.parent()
        .context("package path has no parent directory")
}
