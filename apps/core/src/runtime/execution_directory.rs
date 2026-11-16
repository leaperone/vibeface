use crate::store::admission::AdmissionRejected;
use std::{
    io,
    path::{Path, PathBuf},
};

#[derive(Debug)]
pub(super) struct CanonicalExecutionPath(PathBuf);

#[derive(Debug)]
pub(super) enum DirectoryAuthorizationError {
    OutsideRoots,
    NotDirectory,
    UnresolvedRoot(io::Error),
    Metadata(io::Error),
}

impl From<DirectoryAuthorizationError> for anyhow::Error {
    fn from(error: DirectoryAuthorizationError) -> Self {
        match error {
            DirectoryAuthorizationError::OutsideRoots => AdmissionRejected::new(
                "execution path is outside configured roots; ask the user to allow its folder with propose_provider_config roots",
            )
            .into(),
            DirectoryAuthorizationError::NotDirectory => {
                AdmissionRejected::new("execution path must be a directory").into()
            }
            DirectoryAuthorizationError::UnresolvedRoot(error) => {
                Self::new(error).context("configured execution root cannot be resolved")
            }
            DirectoryAuthorizationError::Metadata(error) => error.into(),
        }
    }
}

impl CanonicalExecutionPath {
    pub(super) fn resolve(path: &Path) -> io::Result<Self> {
        path.canonicalize().map(Self)
    }

    pub(super) fn as_path(&self) -> &Path {
        &self.0
    }

    pub(super) fn authorize(
        self,
        roots: &[PathBuf],
    ) -> Result<PathBuf, DirectoryAuthorizationError> {
        let roots = roots.iter().map(std::fs::canonicalize).collect::<Vec<_>>();
        let allowed = roots
            .iter()
            .filter_map(|root| root.as_ref().ok())
            .any(|root| self.0.starts_with(root));
        if !allowed {
            for root in roots {
                root.map_err(DirectoryAuthorizationError::UnresolvedRoot)?;
            }
            return Err(DirectoryAuthorizationError::OutsideRoots);
        }
        if !std::fs::metadata(&self.0)
            .map_err(DirectoryAuthorizationError::Metadata)?
            .is_dir()
        {
            return Err(DirectoryAuthorizationError::NotDirectory);
        }
        Ok(self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::Result;
    use std::fs;

    fn directory() -> io::Result<PathBuf> {
        let path = std::env::temp_dir().join(format!(
            "vibeface-execution-directory-{}",
            uuid::Uuid::new_v4()
        ));
        fs::create_dir(&path)?;
        path.canonicalize()
    }

    #[test]
    fn root_matching_preserves_error_precedence_and_directory_type() -> Result<()> {
        let base = directory()?;
        let allowed = base.join("allowed");
        let child = allowed.join("child");
        let sibling = base.join("allowed-sibling");
        let missing = base.join("missing");
        let file = allowed.join("file");
        fs::create_dir_all(&child)?;
        fs::create_dir(&sibling)?;
        fs::write(&file, b"")?;

        for roots in [
            vec![allowed.clone()],
            vec![missing.clone(), allowed.clone()],
            vec![allowed.clone(), missing.clone()],
        ] {
            for path in [&allowed, &child] {
                assert_eq!(
                    CanonicalExecutionPath::resolve(path)?
                        .authorize(&roots)
                        .unwrap(),
                    *path
                );
            }
        }
        for roots in [vec![], vec![sibling.clone()]] {
            assert!(matches!(
                CanonicalExecutionPath::resolve(&child)?.authorize(&roots),
                Err(DirectoryAuthorizationError::OutsideRoots)
            ));
        }
        for path in [
            &sibling,
            &allowed.join(".."),
            &allowed.join("../allowed-sibling"),
        ] {
            assert!(matches!(
                CanonicalExecutionPath::resolve(path)?.authorize(std::slice::from_ref(&allowed)),
                Err(DirectoryAuthorizationError::OutsideRoots)
            ));
        }
        for roots in [vec![allowed.clone()], vec![file.clone()]] {
            assert!(matches!(
                CanonicalExecutionPath::resolve(&file)?.authorize(&roots),
                Err(DirectoryAuthorizationError::NotDirectory)
            ));
        }
        assert!(matches!(
            CanonicalExecutionPath::resolve(&file)?.authorize(&[sibling]),
            Err(DirectoryAuthorizationError::OutsideRoots)
        ));
        assert!(matches!(
            CanonicalExecutionPath::resolve(&allowed)?.authorize(std::slice::from_ref(&missing)),
            Err(DirectoryAuthorizationError::UnresolvedRoot(error))
                if error.kind() == io::ErrorKind::NotFound
        ));
        assert_eq!(
            CanonicalExecutionPath::resolve(&missing)
                .unwrap_err()
                .kind(),
            io::ErrorKind::NotFound
        );
        fs::remove_dir_all(base)?;
        Ok(())
    }

    #[test]
    fn symlinks_are_authorized_by_physical_destination() -> Result<()> {
        let base = directory()?;
        let allowed = base.join("allowed");
        let outside = base.join("outside");
        let alias = base.join("root-alias");
        fs::create_dir(&allowed)?;
        fs::create_dir(&outside)?;
        vibeface_bridge::platform::privatefs::link_dir(&allowed, &alias)?;
        vibeface_bridge::platform::privatefs::link_dir(&outside, &allowed.join("escape"))?;
        assert_eq!(
            CanonicalExecutionPath::resolve(&alias)?
                .authorize(std::slice::from_ref(&allowed))
                .unwrap(),
            allowed
        );
        assert_eq!(
            CanonicalExecutionPath::resolve(&allowed)?
                .authorize(&[alias])
                .unwrap(),
            allowed
        );
        assert!(matches!(
            CanonicalExecutionPath::resolve(&allowed.join("escape"))?.authorize(&[allowed]),
            Err(DirectoryAuthorizationError::OutsideRoots)
        ));
        fs::remove_dir_all(base)?;
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn unmatched_roots_report_the_first_resolution_failure() -> Result<()> {
        let base = directory()?;
        let missing = base.join("missing");
        let file = base.join("file");
        fs::write(&file, b"")?;
        let invalid = file.join("child");
        for (roots, expected) in [
            (
                vec![missing.clone(), invalid.clone()],
                io::ErrorKind::NotFound,
            ),
            (vec![invalid, missing], io::ErrorKind::NotADirectory),
        ] {
            assert!(matches!(
                CanonicalExecutionPath::resolve(&base)?.authorize(&roots),
                Err(DirectoryAuthorizationError::UnresolvedRoot(error)) if error.kind() == expected
            ));
        }
        fs::remove_dir_all(base)?;
        Ok(())
    }

    #[test]
    fn native_admission_rechecks_a_directory_replaced_after_identity_verification() -> Result<()> {
        let base = directory()?;
        let allowed = base.join("allowed");
        let child = allowed.join("child");
        let outside = base.join("outside");
        fs::create_dir_all(&child)?;
        fs::create_dir(&outside)?;
        let native = CanonicalExecutionPath::resolve(&child)?;
        assert_eq!(native.as_path(), child);
        assert_eq!(child.canonicalize()?, child);

        fs::remove_dir(&child)?;
        vibeface_bridge::platform::privatefs::link_dir(&outside, &child)?;
        let admission = CanonicalExecutionPath::resolve(native.as_path())?;
        assert_eq!(admission.as_path(), outside);
        let error = admission.authorize(&[allowed]).unwrap_err();
        assert!(matches!(error, DirectoryAuthorizationError::OutsideRoots));
        let error = anyhow::Error::from(error);
        assert!(error.is::<AdmissionRejected>());
        assert_eq!(
            error.to_string(),
            "execution path is outside configured roots; ask the user to allow its folder with propose_provider_config roots"
        );
        fs::remove_dir_all(base)?;
        Ok(())
    }

    #[test]
    fn metadata_is_checked_after_resolution_without_caching_authorization() -> Result<()> {
        let base = directory()?;
        let child = base.join("child");
        fs::create_dir(&child)?;
        let resolved = CanonicalExecutionPath::resolve(&child)?;
        fs::remove_dir(&child)?;
        let error = resolved.authorize(std::slice::from_ref(&base)).unwrap_err();
        assert!(matches!(error, DirectoryAuthorizationError::Metadata(_)));
        let error = anyhow::Error::from(error);
        assert!(!error.is::<AdmissionRejected>());
        assert!(error.is::<io::Error>());
        fs::create_dir(&child)?;
        assert_eq!(
            CanonicalExecutionPath::resolve(&child)?
                .authorize(std::slice::from_ref(&base))
                .unwrap(),
            child
        );
        assert!(matches!(
            CanonicalExecutionPath::resolve(&child)?.authorize(&[]),
            Err(DirectoryAuthorizationError::OutsideRoots)
        ));
        fs::remove_dir_all(base)?;
        Ok(())
    }
}
