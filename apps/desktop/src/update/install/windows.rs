use super::*;
use base64::{Engine, engine::general_purpose::STANDARD};
use std::os::windows::process::CommandExt;

const MARKER: &str = ".vibeface-install-owned";
const UNINSTALL: &str = "Uninstall.exe";

pub(super) fn system_tool(relative: &str) -> Result<PathBuf> {
    let root =
        PathBuf::from(std::env::var_os("SystemRoot").context("missing Windows system directory")?);
    ensure!(root.is_absolute(), "invalid Windows system directory");
    Ok(root.join("System32").join(relative))
}

fn powershell(script: &str) -> Result<Command> {
    let mut cmd = Command::new(system_tool("WindowsPowerShell/v1.0/powershell.exe")?);
    cmd.creation_flags(0x08000000); // CREATE_NO_WINDOW
    let encoded = STANDARD.encode(
        script
            .encode_utf16()
            .flat_map(u16::to_le_bytes)
            .collect::<Vec<_>>(),
    );
    cmd.args([
        "-NoLogo",
        "-NoProfile",
        "-NonInteractive",
        "-EncodedCommand",
        &encoded,
    ]);
    Ok(cmd)
}

// NSIS accepts /D= only as the last, unquoted, DOS-style path. In particular Rust's normal
// Command::arg quoting would silently break a custom installation directory containing spaces.
fn dos_path(path: &Path) -> Result<&str> {
    let path = path
        .to_str()
        .context("NSIS needs a Unicode installation directory")?;
    let path = path.strip_prefix(r"\\?\").unwrap_or(path);
    let bytes = path.as_bytes();
    ensure!(
        bytes.len() >= 4
            && bytes[0].is_ascii_alphabetic()
            && &bytes[1..3] == b":\\"
            && !path.contains('"')
            && !path.chars().any(char::is_control)
            && path.encode_utf16().count() < 1024,
        "NSIS needs a local drive installation directory"
    );
    Ok(path)
}

fn present(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(_) => {
            check_path(path)?;
            Ok(true)
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e.into()),
    }
}

fn verify_registration(root: &Path, version: &str) -> Result<()> {
    let dir = dos_path(root)?;
    let bytes = read_small(&root.join(MARKER), 8192)?;
    ensure!(
        bytes.len().is_multiple_of(2),
        "invalid installer ownership marker"
    );
    let words: Vec<u16> = bytes
        .chunks_exact(2)
        .map(|v| u16::from_le_bytes([v[0], v[1]]))
        .collect();
    let marker = String::from_utf16(words.strip_prefix(&[0xfeff]).unwrap_or(&words))?;
    let expected = format!("vibeface-native-install-v2\r\n{dir}\r\n");
    ensure!(
        marker.eq_ignore_ascii_case(&expected),
        "expected v2 installer ownership for the selected directory"
    );
    check_path(&root.join(UNINSTALL))?;
    let script = r#"$ErrorActionPreference='Stop'
$r = Get-ItemProperty -LiteralPath 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Uninstall\Vibeface'
if ($r.InstallLocation -ne $env:VF_INSTALL_DIR -or $r.DisplayVersion -ne $env:VF_INSTALL_VERSION -or $r.UninstallString -ne ('"' + $env:VF_INSTALL_DIR + '\Uninstall.exe"')) { exit 1 }"#;
    run(powershell(script)?
        .env("VF_INSTALL_DIR", dir)
        .env("VF_INSTALL_VERSION", version))
}

pub(super) fn require_portable(root: &Path) -> Result<()> {
    ensure!(
        !present(&root.join(MARKER))? && !present(&root.join(UNINSTALL))?,
        "portable ZIP updates require an installation without NSIS ownership"
    );
    let script = r#"$ErrorActionPreference='Stop'
$k = 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Uninstall\Vibeface'
if ((Test-Path -LiteralPath $k) -and ((Get-ItemProperty -LiteralPath $k).InstallLocation -eq $env:VF_INSTALL_DIR)) { exit 1 }"#;
    let dir = root
        .to_str()
        .context("Windows install path is not Unicode")?;
    run(powershell(script)?.env("VF_INSTALL_DIR", dir.strip_prefix(r"\\?\").unwrap_or(dir)))
}

pub(super) fn detect(executable: &Path) -> Result<Detected> {
    if executable
        .file_name()
        .is_none_or(|name| !name.eq_ignore_ascii_case(portable::GUI))
    {
        return Ok(None);
    }
    let root = parent(executable)?;
    if !present(&root.join("resources/desktop-build.json"))? {
        return Ok(None);
    }
    let build = read_build(&root.join("resources"))?;
    portable::check_layout(root)?;
    let format = if present(&root.join(MARKER))? {
        verify_registration(root, &build.version)?;
        Format::Installer
    } else {
        require_portable(root)?;
        Format::Portable
    };
    Ok(Some((format, root.into(), build)))
}

pub(super) fn verify_candidate(current: &Path, next: &Path, signed: bool) -> Result<()> {
    let script = r#"$ErrorActionPreference='Stop'
$current = Get-AuthenticodeSignature -LiteralPath $env:VF_CURRENT
$next = Get-AuthenticodeSignature -LiteralPath $env:VF_CANDIDATE
if ($current.Status -notin 'Valid','NotSigned') { throw 'Invalid installed Authenticode signature' }
if ($env:VF_SIGNED -eq 'yes') {
    if ($next.Status -ne 'Valid' -or !$next.TimeStamperCertificate) { throw 'Invalid or untimestamped release signature' }
    if ($current.Status -eq 'Valid' -and $current.SignerCertificate.Subject -ne $next.SignerCertificate.Subject) { throw 'Unexpected Authenticode publisher' }
} elseif ($next.Status -ne 'NotSigned' -or $current.Status -ne 'NotSigned') { throw 'Unsigned update does not match the existing signature policy' }"#;
    let normal = |p: &Path| -> Result<String> {
        let s = p.to_str().context("signature path is not Unicode")?;
        Ok(s.strip_prefix(r"\\?\").unwrap_or(s).to_owned())
    };
    run(powershell(script)?
        .env("VF_CURRENT", normal(current)?)
        .env("VF_CANDIDATE", normal(next)?)
        .env("VF_SIGNED", if signed { "yes" } else { "no" }))
}

pub(super) fn activate(
    current: &Installation,
    asset: &Path,
    candidate: &Build,
    signed: bool,
) -> Result<()> {
    let root = &current.path;
    let gui = root.join(portable::GUI);
    current.check_current(&root.join("resources"), &gui)?;
    verify_registration(root, &current.build.version)?;
    verify_candidate(&gui, asset, signed)?;
    // The installer checks its old uninstaller's compiled ownership list before deletion.
    // Copy the validated prior payload as insurance against disk or sharing errors during
    // the subsequent copy. The registered path and shortcuts stay the same in upgrade mode.
    let mut files = portable::owned_files(root)?;
    files.extend([PathBuf::from(MARKER), PathBuf::from(UNINSTALL)]);
    let mut stage = Stage::new(root)?;
    let backup = stage.path.join("previous");
    portable::backup_files(root, &backup, &files)?;
    current.check_current(&root.join("resources"), &gui)?;
    verify_registration(root, &current.build.version)?;
    let status = Command::new(asset)
        .arg("/S")
        .raw_arg(format!("/D={}", dos_path(root)?))
        .current_dir(&stage.path)
        .status()?;
    let result = (|| -> Result<()> {
        ensure!(status.success(), "Windows installer failed ({status})");
        verify_registration(root, &candidate.version)?;
        ensure!(
            &read_build(&root.join("resources"))? == candidate,
            "installed Desktop differs from the selected release"
        );
        verify_candidate(&backup.join(portable::GUI), &gui, signed)?;
        Ok(())
    })();
    if let Err(error) = result {
        let rollback = (|| -> Result<()> {
            for relative in &files {
                let original = backup.join(relative);
                let dest = root.join(relative);
                if present(&dest)? && hash(&dest)? == hash(&original)? {
                    continue;
                }
                fs::create_dir_all(parent(&dest)?)?;
                check_path(parent(&dest)?)?;
                fs::rename(&original, &dest)?;
            }
            // The installer writes DisplayVersion towards the end. Reset only this value at
            // the same registered location should a post-install check have failed.
            let script = r#"$ErrorActionPreference='Stop'
$k='HKCU:\Software\Microsoft\Windows\CurrentVersion\Uninstall\Vibeface'
if ((Get-ItemProperty -LiteralPath $k).InstallLocation -ne $env:VF_INSTALL_DIR) { exit 1 }
Set-ItemProperty -LiteralPath $k -Name DisplayVersion -Value $env:VF_INSTALL_VERSION"#;
            run(powershell(script)?
                .env("VF_INSTALL_DIR", dos_path(root)?)
                .env("VF_INSTALL_VERSION", &current.build.version))?;
            verify_registration(root, &current.build.version)
        })();
        if let Err(rollback) = rollback {
            stage.keep = true;
            bail!(
                "{error:#}; installer rollback failed ({rollback:#}); backup at {}",
                backup.display()
            );
        }
        return Err(error);
    }
    Ok(())
}
