use super::*;
use std::os::unix::fs::{MetadataExt, PermissionsExt};

fn validate_header(image: &Path) -> Result<()> {
    check_path(image)?;
    let mut header = [0; 20];
    privatefs::open_no_follow(image)?.read_exact(&mut header)?;
    let machine = if cfg!(target_arch = "aarch64") {
        183
    } else {
        62
    };
    ensure!(
        header.starts_with(b"\x7fELF\x02\x01")
            && &header[8..11] == b"AI\x02"
            && u16::from_le_bytes([header[18], header[19]]) == machine,
        "expected a native type-2 AppImage"
    );
    Ok(())
}

pub(super) fn detect(executable: &Path) -> Result<Detected> {
    if executable.file_name().is_none_or(|name| name != "vibeface") {
        return Ok(None);
    }
    let root = parent(executable)?;
    if !root.join("resources/desktop-build.json").try_exists()? {
        return Ok(None);
    }
    check_path(executable)?;
    let build = read_build(&root.join("resources"))?;
    // APPDIR ties the running GUI to the image runtime's mount (or extract-and-run
    // directory). The exact previous release digest is checked again before replacement.
    if let Some(dir) = std::env::var_os("APPDIR")
        && fs::canonicalize(dir).ok().as_deref() == Some(root)
    {
        let Some(image) = std::env::var_os("APPIMAGE").map(PathBuf::from) else {
            return Ok(None);
        };
        validate_header(&image)?;
        return Ok(Some((Format::AppImage, image, build)));
    }
    if std::env::var_os("APPIMAGE").is_some() && root.starts_with(std::env::temp_dir()) {
        // Incomplete AppImage environment must not turn a transient mount into a ZIP install.
        return Ok(None);
    }
    portable::check_layout(root)?;
    Ok(Some((Format::Portable, root.into(), build)))
}

pub(super) fn activate(current: &Installation, asset: &Path, expected: Option<&str>) -> Result<()> {
    let expected = expected.context("the official installed AppImage checksum is required")?;
    let live = &current.path;
    ensure!(
        detect(&std::env::current_exe()?)?.as_ref()
            == Some(&(current.format, live.clone(), current.build.clone())),
        "running Desktop does not belong to the selected AppImage"
    );
    replace_image(live, asset, expected)
}

fn replace_image(live: &Path, asset: &Path, expected: &str) -> Result<()> {
    validate_header(asset)?;
    let metadata = fs::metadata(live)?;
    ensure!(
        metadata.mode() & 0o6000 == 0 && metadata.mode() & 0o111 != 0,
        "unexpected AppImage permissions"
    );
    let stage = Stage::new(parent(live)?)?;
    let candidate = stage.path.join("Vibeface.AppImage");
    fs::copy(asset, &candidate)?;
    fs::set_permissions(
        &candidate,
        fs::Permissions::from_mode(metadata.mode() & 0o777),
    )?;
    fs::File::open(&candidate)?.sync_all()?;
    ensure!(
        hash(live)? == expected,
        "installed AppImage differs from the official current release"
    );
    privatefs::durable_rename(&candidate, live)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retains_old_image_until_the_installed_release_digest_matches() -> Result<()> {
        let temp = Stage::new(&fs::canonicalize(std::env::temp_dir())?)?;
        let live = temp.path.join("renamed original.AppImage");
        let incoming = temp.path.join("package.AppImage");
        let mut header = [0; 20];
        header[..6].copy_from_slice(b"\x7fELF\x02\x01");
        header[8..11].copy_from_slice(b"AI\x02");
        header[18] = if cfg!(target_arch = "aarch64") {
            183
        } else {
            62
        };
        fs::write(&live, [header.as_slice(), b"old"].concat())?;
        fs::set_permissions(&live, fs::Permissions::from_mode(0o751))?;
        fs::write(&incoming, [header.as_slice(), b"new"].concat())?;
        let original = hash(&live)?;
        assert!(replace_image(&live, &incoming, &"0".repeat(64)).is_err());
        assert_eq!(hash(&live)?, original);
        replace_image(&live, &incoming, &original)?;
        assert_eq!(hash(&live)?, hash(&incoming)?);
        assert_eq!(fs::metadata(&live)?.mode() & 0o777, 0o751);
        Ok(())
    }
}
