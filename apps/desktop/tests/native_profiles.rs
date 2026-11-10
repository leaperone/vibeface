#[path = "../src/host/profiles.rs"]
mod profiles;

use anyhow::Result;
use profiles::{MAX_PROFILES, ProfileHints, ProfileId, ProfileRegistry};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use vibeface_bridge::platform::privatefs;
use vibeface_client::Entry;

struct Fixture {
    root: PathBuf,
}

impl Fixture {
    fn new() -> Result<Self> {
        let root = std::env::temp_dir().join(format!("vf-profiles-fixture-{}", uuid::Uuid::new_v4()));
        privatefs::create_dir_new(&root)?;
        Ok(Self { root: std::fs::canonicalize(root)? })
    }

    fn default_dir(&self) -> PathBuf {
        self.root.join("native-client")
    }

    fn metadata(&self) -> PathBuf {
        self.root.join("native-client-profiles.json")
    }

    fn open(&self) -> Result<ProfileRegistry> {
        ProfileRegistry::open(&self.default_dir())
    }

    fn seed_default(&self) -> Result<Vec<(PathBuf, Vec<u8>)>> {
        privatefs::create_dir_new(&self.default_dir())?;
        let mut files = vec![];
        for (name, bytes) in [
            ("client.json", b"synthetic sealed original identity".as_slice()),
            ("identity-operation.json", b"synthetic sealed unknown identity operation".as_slice()),
            ("outbox/original-network/original-client/outbox.sqlite3", b"synthetic unknown operation with original signature".as_slice()),
            ("input-drafts/original-network/original-client/draft.json", b"synthetic original input and attachment lease".as_slice()),
        ] {
            let path = self.default_dir().join(name);
            make_parents(&self.default_dir(), path.parent().unwrap())?;
            privatefs::create_durable(&path, bytes)?;
            files.push((path, bytes.to_vec()));
        }
        Ok(files)
    }

    fn assert_no_node(&self) -> Result<()> {
        // Fixtures never start a process or contain a runtime. Reject any Node-owned artifacts.
        fn inspect(path: &Path) -> Result<()> {
            for entry in std::fs::read_dir(path)? {
                let entry = entry?;
                let name = entry.file_name();
                assert!(!["node", "identity.json", "config.json", "local-access.json", "running"]
                    .iter().any(|unexpected| name == *unexpected));
                if entry.file_type()?.is_dir() {
                    inspect(&entry.path())?;
                }
            }
            Ok(())
        }
        inspect(&self.root)
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn make_parents(root: &Path, path: &Path) -> Result<()> {
    let mut current = root.to_owned();
    for part in path.strip_prefix(root)?.components() {
        current.push(part);
        if !current.exists() {
            privatefs::create_dir_new(&current)?;
        }
    }
    Ok(())
}

fn assert_bytes(files: &[(PathBuf, Vec<u8>)]) -> Result<()> {
    for (path, expected) in files {
        assert_eq!(&std::fs::read(path)?, expected);
    }
    Ok(())
}

#[test]
fn discovery_preserves_default_identity_pending_operations_plans_and_leases_byte_exact() -> Result<()> {
    let fixture = Fixture::new()?;
    let original = fixture.seed_default()?;
    let registry = fixture.open()?;
    assert_eq!(registry.profiles().len(), 1);
    assert_eq!(registry.selected_id().as_str(), "default");
    assert_eq!(registry.selected().label, "默认个人网络");
    let paths = registry.paths(registry.selected_id())?;
    assert_eq!(paths.client_dir, fixture.default_dir());
    assert_eq!(paths.plan_dir, fixture.default_dir());
    assert_eq!(paths.outbox_dir, fixture.default_dir().join("outbox"));
    assert_eq!(paths.draft_dir, fixture.default_dir().join("input-drafts"));
    let existing_base = format!("vibeface-native://{}", hex::encode(Sha256::digest(fixture.root.to_string_lossy().as_bytes())));
    assert_eq!(registry.credential_scope_from_base(registry.selected_id(), &existing_base)?, existing_base);
    assert_bytes(&original)?;
    drop(registry);
    let reopened = fixture.open()?;
    assert_eq!(reopened.selected_id().as_str(), "default");
    assert_bytes(&original)?;
    fixture.assert_no_node()
}

#[test]
fn explicit_create_select_switch_and_reopen_keep_both_profiles_and_credential_services() -> Result<()> {
    let fixture = Fixture::new()?;
    let original = fixture.seed_default()?;
    let mut registry = fixture.open()?;
    let default = registry.selected_id().clone();
    let default_scope = format!("vibeface-native://{}", hex::encode(Sha256::digest(fixture.root.to_string_lossy().as_bytes())));
    assert_eq!(registry.credential_scope_from_base(&default, &default_scope)?, default_scope);
    let second = registry.create("另一个个人网络")?;
    assert_eq!(registry.selected_id(), &default, "creation must not silently switch identities");
    let paths = registry.paths(&second)?;
    assert!(paths.client_dir.starts_with(fixture.root.join("native-client-profiles")));
    assert!(paths.client_dir.is_dir());
    assert!(paths.outbox_dir.is_dir());
    assert!(paths.draft_dir.is_dir());
    assert!(!paths.client_dir.join("client.json").exists(), "registry must not generate a key");
    for path in [&paths.client_dir, &paths.outbox_dir, &paths.draft_dir, &fixture.metadata()] {
        privatefs::ensure_private(path, "synthetic profile fixture")?;
    }
    let second_key = paths.client_dir.join("client.json");
    privatefs::create_durable(&second_key, b"synthetic independent sealed key")?;
    let second_scope = registry.credential_scope_from_base(&second, &default_scope)?;
    assert_eq!(second_scope, format!("{default_scope}/{}", second.as_str()));
    let original_spelling_scope = format!("vibeface-native://{}", "a".repeat(64));
    assert_eq!(registry.credential_scope_from_base(&default, &original_spelling_scope)?, original_spelling_scope);
    assert_eq!(registry.credential_scope_from_base(&second, &original_spelling_scope)?, format!("{original_spelling_scope}/{}", second.as_str()));
    assert!(registry.credential_scope_from_base(&second, "arbitrary-service").is_err());
    registry.set_hints(&second, ProfileHints {
        network: Some("untrusted-display-network".into()),
        entry: Some(Entry { origin: "https://display.invalid".into(), node_id: "untrusted-display-node".into() }),
    })?;
    registry.select(&second)?;
    assert_eq!(registry.selected().hints.network.as_deref(), Some("untrusted-display-network"));
    registry.select(&default)?;
    assert_bytes(&original)?;
    registry.select(&second)?;
    drop(registry);
    let reopened = fixture.open()?;
    assert_eq!(reopened.selected_id(), &second);
    assert_eq!(reopened.credential_scope_from_base(&second, &default_scope)?, second_scope);
    assert_eq!(reopened.credential_scope_from_base(&default, &default_scope)?, default_scope);
    assert_eq!(std::fs::read(second_key)?, b"synthetic independent sealed key");
    assert_bytes(&original)?;
    fixture.assert_no_node()
}

#[test]
fn empty_startup_registers_location_without_creating_identity_or_node() -> Result<()> {
    let fixture = Fixture::new()?;
    let registry = fixture.open()?;
    assert!(!registry.paths(registry.selected_id())?.client_dir.exists());
    assert_eq!(std::fs::read_dir(&fixture.root)?.count(), 2, "only metadata and its lock may appear");
    fixture.assert_no_node()
}

#[test]
fn invalid_ids_paths_and_labels_are_refused_without_mutating_metadata() -> Result<()> {
    let fixture = Fixture::new()?;
    let mut registry = fixture.open()?;
    let before = std::fs::read(fixture.metadata())?;
    for id in ["", ".", "..", "../native-client", "/absolute", "a/b", "default/child", "DEFAULT", "550e8400-e29b-41d4-a716-446655440000\n"] {
        assert!(ProfileId::parse(id).is_err());
    }
    for label in ["", " ", " leading", "trailing ", "line\nbreak"] {
        assert!(registry.create(label).is_err());
    }
    assert!(registry.create(&"x".repeat(121)).is_err());
    let missing = ProfileId::parse(&uuid::Uuid::new_v4().to_string())?;
    assert!(registry.select(&missing).is_err());
    assert!(registry.paths(&missing).is_err());
    assert!(ProfileRegistry::open(Path::new("relative/native-client")).is_err());
    assert!(ProfileRegistry::open(&fixture.root.join("../native-client")).is_err());
    assert!(ProfileRegistry::open(&fixture.root.join("node")).is_err());
    assert_eq!(std::fs::read(fixture.metadata())?, before);
    fixture.assert_no_node()
}

#[test]
fn corrupt_duplicate_unknown_and_injected_path_metadata_fail_closed_preserving_bytes() -> Result<()> {
    let fixture = Fixture::new()?;
    let original = fixture.seed_default()?;
    let registry = fixture.open()?;
    drop(registry);
    let valid: Value = serde_json::from_slice(&std::fs::read(fixture.metadata())?)?;
    let mut bad_values = vec![];
    let mut injected = valid.clone();
    injected["profiles"][0]["client_dir"] = json!("/untrusted/outside");
    bad_values.push(serde_json::to_vec(&injected)?);
    let mut duplicate = valid.clone();
    let profile = duplicate["profiles"][0].clone();
    duplicate["profiles"].as_array_mut().unwrap().push(profile);
    bad_values.push(serde_json::to_vec(&duplicate)?);
    let mut unknown = valid.clone();
    unknown["selected_id"] = json!(uuid::Uuid::new_v4().to_string());
    bad_values.push(serde_json::to_vec(&unknown)?);
    let mut bad_id = valid.clone();
    bad_id["profiles"][0]["id"] = json!("../../outside");
    bad_values.push(serde_json::to_vec(&bad_id)?);
    let mut bad_version = valid;
    bad_version["version"] = json!(2);
    bad_values.push(serde_json::to_vec(&bad_version)?);
    bad_values.push(b"{synthetic damaged metadata".to_vec());
    bad_values.push(vec![b'x'; 64 * 1024 + 1]);
    for bytes in bad_values {
        privatefs::write_atomic(&fixture.metadata(), &bytes)?;
        assert!(fixture.open().is_err());
        assert_eq!(std::fs::read(fixture.metadata())?, bytes);
        assert_bytes(&original)?;
    }
    fixture.assert_no_node()
}

#[test]
fn concurrent_registry_and_external_corruption_cannot_overwrite_metadata() -> Result<()> {
    let fixture = Fixture::new()?;
    let mut registry = fixture.open()?;
    assert!(fixture.open().is_err());
    let corrupted = b"synthetic external corruption";
    privatefs::write_atomic(&fixture.metadata(), corrupted)?;
    let id = registry.selected_id().clone();
    assert!(registry.select(&id).is_err());
    assert!(registry.create("must not write").is_err());
    assert!(registry.set_hints(&id, ProfileHints::default()).is_err());
    assert_eq!(std::fs::read(fixture.metadata())?, corrupted);
    assert!(!fixture.root.join("native-client-profiles").exists());
    fixture.assert_no_node()
}

#[test]
fn registry_cap_refuses_creation_without_discarding_any_profile() -> Result<()> {
    let fixture = Fixture::new()?;
    let mut registry = fixture.open()?;
    for index in 1..MAX_PROFILES {
        registry.create(&format!("synthetic network {index}"))?;
    }
    let before = std::fs::read(fixture.metadata())?;
    assert!(registry.create("one too many").is_err());
    assert_eq!(registry.profiles().len(), MAX_PROFILES);
    assert_eq!(std::fs::read(fixture.metadata())?, before);
    assert_eq!(std::fs::read_dir(fixture.root.join("native-client-profiles"))?.count(), MAX_PROFILES - 1);
    fixture.assert_no_node()
}

#[test]
fn missing_registered_profile_is_refused_instead_of_regenerating_an_identity() -> Result<()> {
    let fixture = Fixture::new()?;
    let mut registry = fixture.open()?;
    let id = registry.create("synthetic profile")?;
    let paths = registry.paths(&id)?;
    registry.select(&id)?;
    drop(registry);
    // Simulate files being unavailable. The registry must not replace them with a fresh identity.
    let saved = fixture.root.join("detached-profile");
    std::fs::rename(&paths.client_dir, &saved)?;
    let before = std::fs::read(fixture.metadata())?;
    assert!(fixture.open().is_err());
    assert!(!paths.client_dir.exists());
    assert!(saved.is_dir());
    assert_eq!(std::fs::read(fixture.metadata())?, before);
    fixture.assert_no_node()
}

#[test]
fn symlinked_registry_profile_container_key_and_draft_directory_are_refused() -> Result<()> {
    let fixture = Fixture::new()?;
    let original = fixture.seed_default()?;
    let registry = fixture.open()?;
    drop(registry);
    let before = std::fs::read(fixture.metadata())?;
    for target in [fixture.default_dir(), fixture.default_dir().join("input-drafts")] {
        let saved = fixture.root.join("held-directory");
        std::fs::rename(&target, &saved)?;
        privatefs::link_dir(&saved, &target)?;
        assert!(fixture.open().is_err());
        privatefs::remove_dir_link(&target)?;
        std::fs::rename(saved, target)?;
        assert_eq!(std::fs::read(fixture.metadata())?, before);
        assert_bytes(&original)?;
    }
    let container = fixture.root.join("native-client-profiles");
    let external = fixture.root.join("synthetic-external");
    privatefs::create_dir_new(&external)?;
    privatefs::link_dir(&external, &container)?;
    assert!(fixture.open().is_err());
    privatefs::remove_dir_link(&container)?;
    #[cfg(unix)]
    for target in [fixture.metadata(), fixture.default_dir().join("client.json")] {
        let saved = fixture.root.join("held-file");
        std::fs::rename(&target, &saved)?;
        std::os::unix::fs::symlink(&saved, &target)?;
        assert!(fixture.open().is_err());
        std::fs::remove_file(&target)?;
        std::fs::rename(saved, target)?;
        assert_eq!(std::fs::read(fixture.metadata())?, before);
        assert_bytes(&original)?;
    }
    fixture.assert_no_node()
}

#[cfg(unix)]
#[test]
fn permissive_registry_key_and_directory_are_refused_without_chmod() -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let fixture = Fixture::new()?;
    let original = fixture.seed_default()?;
    let registry = fixture.open()?;
    drop(registry);
    let before = std::fs::read(fixture.metadata())?;
    for (path, mode, private_mode) in [
        (fixture.metadata(), 0o644, 0o600),
        (fixture.default_dir().join("client.json"), 0o644, 0o600),
        (fixture.default_dir(), 0o755, 0o700),
    ] {
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode))?;
        assert!(fixture.open().is_err());
        assert_eq!(std::fs::symlink_metadata(&path)?.permissions().mode() & 0o777, mode);
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(private_mode))?;
        assert_eq!(std::fs::read(fixture.metadata())?, before);
        assert_bytes(&original)?;
    }
    fixture.assert_no_node()
}
