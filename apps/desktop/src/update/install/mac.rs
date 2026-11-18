use super::*;
use rustix::fs::{CWD, RenameFlags, renameat_with};

const GUI: &str = "Contents/MacOS/Vibeface";
const RESOURCES: &str = "Contents/Resources";
const CODESIGN: &str = "/usr/bin/codesign";
const BUNDLE_ID: &str = "one.leaper.vibeface.desktop";

pub(super) fn detect(executable: &Path) -> Result<Detected> {
    let Some(app) = executable.ancestors().nth(3) else {
        return Ok(None);
    };
    if app.extension().is_none_or(|s| s != "app") || executable != app.join(GUI) {
        return Ok(None);
    }
    check_path(executable)?;
    check_bundle_id(app)?;
    Ok(Some((
        Format::AppBundle,
        app.into(),
        read_build(&app.join(RESOURCES))?,
    )))
}

fn check_bundle_id(app: &Path) -> Result<()> {
    let plist = app.join("Contents/Info.plist");
    check_path(&plist)?;
    let output = Command::new("/usr/bin/plutil")
        .args(["-extract", "CFBundleIdentifier", "raw", "-o", "-"])
        .arg(plist)
        .output()?;
    ensure!(
        output.status.success() && String::from_utf8(output.stdout)?.trim() == BUNDLE_ID,
        "unexpected macOS bundle identifier"
    );
    Ok(())
}

/// Developer ID signed builds carry a designated requirement which survives certificate
/// renewal. Ad-hoc arm64 executables in unsigned Hourly bundles have no Developer ID.
struct DeveloperId {
    team: String,
    requirement: String,
}

fn developer_id(app: &Path) -> Result<Option<DeveloperId>> {
    let details = Command::new(CODESIGN).arg("-dvvv").arg(app).output()?;
    let details = String::from_utf8(details.stderr)?;
    if !details
        .lines()
        .any(|s| s.starts_with("Authority=Developer ID Application:"))
    {
        ensure!(
            details.contains("Signature=adhoc") || details.contains("not signed at all"),
            "unrecognized macOS signature"
        );
        return Ok(None);
    }
    run(Command::new(CODESIGN)
        .args(["--verify", "--deep", "--strict"])
        .arg(app))?;
    let team = details
        .lines()
        .find_map(|s| s.strip_prefix("TeamIdentifier="))
        .filter(|id| id.len() == 10 && id.bytes().all(|b| b.is_ascii_alphanumeric()))
        .context("missing Apple Developer team")?
        .to_owned();
    let output = Command::new(CODESIGN)
        .args(["-d", "-r-"])
        .arg(app)
        .output()?;
    ensure!(
        output.status.success(),
        "unable to read the current designated requirement"
    );
    let requirement = String::from_utf8([output.stdout, output.stderr].concat())?
        .lines()
        .find_map(|s| s.strip_prefix("designated => "))
        .context("missing designated requirement")?
        .to_owned();
    Ok(Some(DeveloperId { team, requirement }))
}

pub(super) fn activate(
    current: &Installation,
    asset: &Path,
    candidate: &Build,
    signed: bool,
) -> Result<()> {
    let app = &current.path;
    ensure!(
        app.extension().is_some_and(|s| s == "app"),
        "invalid app location"
    );
    current.check_current(&app.join(RESOURCES), &app.join(GUI))?;
    check_bundle_id(app)?;
    let publisher = developer_id(app)?;
    let mut stage = Stage::new(parent(app)?)?;
    run(Command::new("/usr/bin/ditto")
        .args(["-x", "-k"])
        .arg(asset)
        .arg(&stage.path))?;
    let next = stage.path.join("Vibeface.app");
    check_bundle_id(&next)?;
    ensure!(
        &read_build(&next.join(RESOURCES))? == candidate,
        "macOS update build identity mismatch"
    );
    let next_publisher = developer_id(&next)?;
    ensure!(
        next_publisher.is_some() == signed,
        "macOS update signature differs from the release receipt"
    );
    if let Some(publisher) = publisher {
        ensure!(
            next_publisher.is_some_and(|p| p.team == publisher.team),
            "the update requires the installed Apple Developer team"
        );
        run(Command::new(CODESIGN)
            .args(["--verify", "--deep", "--strict", "-R"])
            .arg(format!("={}", publisher.requirement))
            .arg(&next))?;
    }
    if signed {
        run(Command::new("/usr/sbin/spctl")
            .args(["--assess", "--type", "execute"])
            .arg(&next))?;
    }
    current.check_current(&app.join(RESOURCES), &app.join(GUI))?;
    let swap = || renameat_with(CWD, &next, CWD, app, RenameFlags::EXCHANGE);
    swap().context("atomically exchange the installed macOS bundle")?;
    let verify = || -> Result<()> {
        ensure!(
            &read_build(&app.join(RESOURCES))? == candidate,
            "installed macOS update identity mismatch"
        );
        privatefs::sync_dir(parent(app)?)
    };
    if let Err(error) = verify() {
        if let Err(rollback) = swap() {
            stage.keep = true;
            bail!(
                "{error:#}; macOS rollback failed ({rollback}); previous bundle at {}",
                next.display()
            );
        }
        return Err(error);
    }
    Ok(())
}
