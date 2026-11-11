use super::{Local, Provider, ProviderRoots};
use crate::discovery::{Host, LocalSourceError};
use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{collections::BTreeMap, io::Read, path::Path};
use vibeface_bridge::platform::privatefs;
use zeroize::Zeroizing;

const LOGIN_LIMIT: u64 = 64 * 1024;
const IMPORT_MARKER: &str = "local-login.json";
const ACCESS_SNAPSHOT: &str = "local-login-access.json";

#[derive(Clone, PartialEq)]
pub struct ExternalCodexLogin {
    pub access_token: String,
    pub account_id: String,
    pub plan_type: Option<String>,
}

impl ExternalCodexLogin {
    pub(crate) fn fingerprint(&self) -> String {
        crate::crypto::digest(
            format!(
                "{}\0{}\0{}",
                self.access_token,
                self.account_id,
                self.plan_type.as_deref().unwrap_or_default()
            )
            .as_bytes(),
        )
    }
}

pub(super) fn external_codex_login(local: &Local) -> Option<ExternalCodexLogin> {
    if environment_authentication(Provider::Codex, local) {
        return None;
    }
    let root = Path::new(local.env.get(Provider::Codex).get("CODEX_HOME")?);
    owned_import(root, &root.join("auth.json"))?;
    let bytes = login_cache(&root.join(ACCESS_SNAPSHOT)).ok()??;
    let value: Value = serde_json::from_slice(&bytes).ok()?;
    let tokens = &value["tokens"];
    let access = tokens["access_token"].as_str().filter(|v| !v.is_empty())?;
    let claims = tokens["id_token"].as_str().and_then(jwt_claims);
    let auth = claims
        .as_ref()
        .map(|claims| &claims["https://api.openai.com/auth"]);
    let account = tokens["account_id"]
        .as_str()
        .or_else(|| auth.and_then(|auth| auth["chatgpt_account_id"].as_str()))?;
    if account.is_empty() || account.len() > 256 || account.chars().any(char::is_control) {
        return None;
    }
    Some(ExternalCodexLogin {
        access_token: access.into(),
        account_id: account.into(),
        plan_type: auth
            .and_then(|auth| auth["chatgpt_plan_type"].as_str())
            .map(str::to_owned),
    })
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ImportedLogin {
    credential_digest: String,
}

fn owned_import(root: &Path, target: &Path) -> Option<ImportedLogin> {
    let marker = login_cache(&root.join(IMPORT_MARKER)).ok()??;
    let marker: ImportedLogin = serde_json::from_slice(&marker).ok()?;
    let credential = login_cache(target).ok()??;
    (crate::crypto::digest(&credential) == marker.credential_digest).then_some(marker)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthenticationSource {
    Environment,
    ImportedCliLogin,
    PrivateLogin,
    LocalCliLogin,
    SystemServer,
    ProviderDefault,
}

fn environment_authentication(provider: Provider, local: &Local) -> bool {
    let names: &[&str] = match provider {
        Provider::Codex => &["OPENAI_API_KEY", "CODEX_API_KEY"],
        Provider::Claude => &[
            "ANTHROPIC_API_KEY",
            "ANTHROPIC_AUTH_TOKEN",
            "CLAUDE_CODE_OAUTH_TOKEN",
        ],
        Provider::Cursor | Provider::Pi | Provider::Opencode => &[],
    };
    names.iter().any(|name| {
        local.env.get(provider).contains_key(*name)
            || !cfg!(test) && std::env::var_os(name).is_some_and(|value| !value.is_empty())
    }) || provider == Provider::Codex
        && provider
            .key_variables(local.env.get(provider))
            .is_ok_and(|keys| !keys.is_empty())
}

impl ProviderRoots {
    pub fn authentication_source(&self, provider: Provider, local: &Local) -> AuthenticationSource {
        if provider == Provider::Codex && !local.launches(provider) {
            return AuthenticationSource::SystemServer;
        }
        if environment_authentication(provider, local) {
            return AuthenticationSource::Environment;
        }
        let root = match provider {
            Provider::Codex => &self.codex_home,
            Provider::Claude => {
                return if self.claude_borrowed() {
                    AuthenticationSource::ImportedCliLogin
                } else {
                    AuthenticationSource::PrivateLogin
                };
            }
            Provider::Cursor => return AuthenticationSource::LocalCliLogin,
            Provider::Pi | Provider::Opencode => return AuthenticationSource::ProviderDefault,
        };
        if owned_import(root, &root.join("auth.json")).is_some() {
            AuthenticationSource::ImportedCliLogin
        } else {
            AuthenticationSource::PrivateLogin
        }
    }

    /// Imported logins are access-token snapshots. The personal CLI alone keeps refresh
    /// ownership; explicit private logins are never overwritten.
    pub(crate) fn prepare_login(
        &self,
        provider: Provider,
        local: &Local,
        host: &Host,
    ) -> Result<bool> {
        if !local.launches(provider)
            || environment_authentication(provider, local)
            || !host.installed(&local.bin(provider))
        {
            return Ok(false);
        }
        let (source_root, target_root, name) = match provider {
            Provider::Codex => (
                host.codex_home
                    .clone()
                    .or_else(|| host.home.as_ref().map(|p| p.join(".codex"))),
                &self.codex_home,
                "auth.json",
            ),
            Provider::Claude => (
                host.claude_config
                    .clone()
                    .or_else(|| host.home.as_ref().map(|p| p.join(".claude"))),
                &self.claude_config,
                ".credentials.json",
            ),
            Provider::Cursor | Provider::Pi | Provider::Opencode => return Ok(false),
        };
        let target = target_root.join(name);
        let existing = match std::fs::symlink_metadata(&target) {
            Ok(_) if provider == Provider::Claude => return Ok(false),
            Ok(_) => match owned_import(target_root, &target) {
                Some(marker) => Some(marker),
                None => return Ok(false),
            },
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(_) => return Err(LocalSourceError.into()),
        };
        let Some(source) =
            source_root.filter(|source| source.is_absolute() && source != target_root)
        else {
            return Ok(false);
        };
        let bytes = if provider == Provider::Claude {
            match claude_keychain(host, &source)? {
                Some(bytes) => Some(bytes),
                None => login_cache(&source.join(name))?,
            }
        } else {
            login_cache(&source.join(name))?
        };
        let Some(bytes) = bytes else {
            return Ok(false);
        };
        let source: Value = serde_json::from_slice(&bytes).map_err(|_| LocalSourceError)?;
        let credential = match provider {
            Provider::Codex => {
                let Some(tokens) = source.get("tokens").filter(|tokens| tokens.is_object()) else {
                    return Ok(false);
                };
                if !token(tokens.get("access_token")) {
                    return Ok(false);
                }
                let mut tokens = tokens.as_object().ok_or(LocalSourceError)?.clone();
                tokens.insert("refresh_token".into(), json!(""));
                // The official external-auth loader requires this mode and a String field;
                // an empty refresh token in managed ChatGPT mode would be unsafe.
                let mut credential = json!({"auth_mode":"chatgptAuthTokens","tokens":tokens});
                if let Some(value) = source.get("last_refresh") {
                    credential["last_refresh"] = value.clone();
                }
                credential
            }
            Provider::Claude => {
                let Some(oauth) = source.get("claudeAiOauth").and_then(Value::as_object) else {
                    return Ok(false);
                };
                if !token(oauth.get("accessToken")) {
                    return Ok(false);
                }
                let mut oauth = oauth.clone();
                oauth.remove("refreshToken");
                json!({"claudeAiOauth":oauth})
            }
            _ => unreachable!(),
        };
        let bytes = Zeroizing::new(serde_json::to_vec(&credential).map_err(|_| LocalSourceError)?);
        for value in credential
            .get(if provider == Provider::Codex {
                "tokens"
            } else {
                "claudeAiOauth"
            })
            .and_then(Value::as_object)
            .into_iter()
            .flat_map(serde_json::Map::values)
            .filter_map(Value::as_str)
        {
            crate::logging::add_credential(value);
        }
        if provider == Provider::Claude {
            crate::config::private_dir(target_root).map_err(|_| LocalSourceError)?;
            let changed = login_cache(&target_root.join(ACCESS_SNAPSHOT))?
                .is_none_or(|current| *current != *bytes);
            if changed {
                crate::config::private_write(&target_root.join(ACCESS_SNAPSHOT), &bytes)
                    .map_err(|_| LocalSourceError)?;
            }
            let marker = ImportedLogin {
                credential_digest: crate::crypto::digest(&bytes),
            };
            crate::config::private_write(
                &target_root.join(IMPORT_MARKER),
                &serde_json::to_vec(&marker).map_err(|_| LocalSourceError)?,
            )
            .map_err(|_| LocalSourceError)?;
            return Ok(changed);
        }
        if existing.is_some()
            && login_cache(&target_root.join(ACCESS_SNAPSHOT))?
                .is_some_and(|current| *current == *bytes)
        {
            return Ok(false);
        }
        let marker = existing.unwrap_or_else(|| ImportedLogin {
            credential_digest: crate::crypto::digest(&bytes),
        });
        let published = if owned_import(target_root, &target).is_some() {
            true
        } else {
            publish_login(&target, &bytes)?
        };
        if published {
            // Only this Node writes this snapshot. An existing CLI credential is never
            // replaced, including when a person signs in concurrently from their terminal.
            crate::config::private_write(&target_root.join(ACCESS_SNAPSHOT), &bytes)
                .map_err(|_| LocalSourceError)?;
            crate::config::private_write(
                &target_root.join(IMPORT_MARKER),
                &serde_json::to_vec(&marker).map_err(|_| LocalSourceError)?,
            )
            .map_err(|_| LocalSourceError)?;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    pub(crate) fn borrowed_claude_environment(
        &self,
        local: &Local,
    ) -> Result<BTreeMap<String, String>> {
        let mut env = local.launch_env(Provider::Claude);
        if environment_authentication(Provider::Claude, local) || !self.claude_borrowed() {
            return Ok(env);
        }
        let bytes =
            login_cache(&self.claude_config.join(ACCESS_SNAPSHOT))?.ok_or(LocalSourceError)?;
        let value: Value = serde_json::from_slice(&bytes).map_err(|_| LocalSourceError)?;
        let token = value["claudeAiOauth"]["accessToken"]
            .as_str()
            .filter(|token| !token.is_empty())
            .ok_or(LocalSourceError)?;
        crate::logging::add_credential(token);
        env.insert("CLAUDE_CODE_OAUTH_TOKEN".into(), token.into());
        Ok(env)
    }

    fn claude_borrowed(&self) -> bool {
        std::fs::symlink_metadata(self.claude_config.join(".credentials.json"))
            .is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound)
            && owned_import(
                &self.claude_config,
                &self.claude_config.join(ACCESS_SNAPSHOT),
            )
            .is_some()
    }

    pub(crate) fn clear_claude_borrow(&self) -> Result<()> {
        match std::fs::remove_file(self.claude_config.join(IMPORT_MARKER)) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(_) => Err(LocalSourceError.into()),
        }
    }

    pub(crate) fn imported_login_expired(&self, provider: Provider, local: &Local) -> bool {
        let (root, imported) = match provider {
            Provider::Codex => (
                &self.codex_home,
                owned_import(&self.codex_home, &self.codex_home.join("auth.json")).is_some(),
            ),
            Provider::Claude => (&self.claude_config, self.claude_borrowed()),
            _ => return false,
        };
        if environment_authentication(provider, local) || !imported {
            return false;
        }
        let Some(bytes) = login_cache(&root.join(ACCESS_SNAPSHOT)).ok().flatten() else {
            return true;
        };
        let Ok(value) = serde_json::from_slice::<Value>(&bytes) else {
            return true;
        };
        let expires = if provider == Provider::Claude {
            value["claudeAiOauth"]["expiresAt"].as_u64()
        } else {
            value["tokens"]["access_token"]
                .as_str()
                .and_then(jwt_claims)
                .and_then(|payload| payload["exp"].as_u64())
                .and_then(|seconds| seconds.checked_mul(1000))
        };
        expires.is_some_and(|expires| expires <= vibeface_protocol::now_ms())
    }
}

fn jwt_claims(token: &str) -> Option<Value> {
    use base64::Engine;
    let payload = token.split('.').nth(1)?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload)
        .ok()?;
    serde_json::from_slice(&bytes).ok()
}

fn claude_keychain(host: &Host, source: &Path) -> Result<Option<Zeroizing<Vec<u8>>>> {
    #[cfg(all(target_os = "macos", not(test)))]
    {
        use security_framework::item::{ItemClass, ItemSearchOptions, SearchResult};
        use std::os::unix::ffi::OsStrExt;
        let Some(home) = host.home.as_ref() else {
            return Ok(None);
        };
        if host.claude_config.is_some() || source != home.join(".claude") {
            return Ok(None);
        }
        let mut entry = std::mem::MaybeUninit::<libc::passwd>::uninit();
        let mut found = std::ptr::null_mut();
        let mut buffer = vec![0u8; 16 * 1024];
        // Only the real OS account's default CLI store is eligible. A fixture's overridden
        // HOME or CLAUDE_CONFIG_DIR must never reach the operator's Keychain.
        let status = unsafe {
            libc::getpwuid_r(
                libc::geteuid(),
                entry.as_mut_ptr(),
                buffer.as_mut_ptr().cast(),
                buffer.len(),
                &mut found,
            )
        };
        if status != 0 || found.is_null() {
            return Ok(None);
        }
        // SAFETY: getpwuid_r succeeded and the NUL-terminated fields remain in `buffer`.
        let entry = unsafe { entry.assume_init() };
        let actual_home = unsafe { std::ffi::CStr::from_ptr(entry.pw_dir) };
        if home.as_os_str().as_bytes() != actual_home.to_bytes() {
            return Ok(None);
        }
        let account = unsafe { std::ffi::CStr::from_ptr(entry.pw_name) }
            .to_str()
            .map_err(|_| LocalSourceError)?;
        let mut search = ItemSearchOptions::new();
        search
            .class(ItemClass::generic_password())
            .service("Claude Code-credentials")
            .account(account)
            .load_data(true)
            .skip_authenticated_items(true)
            .limit(1);
        let results = match search.search() {
            Ok(results) => results,
            Err(error) if matches!(error.code(), -25300 | -25308 | -25293) => return Ok(None),
            Err(_) => return Err(LocalSourceError.into()),
        };
        for result in results {
            if let SearchResult::Data(bytes) = result {
                if bytes.len() as u64 > LOGIN_LIMIT {
                    return Err(LocalSourceError.into());
                }
                return Ok(Some(Zeroizing::new(bytes)));
            }
        }
        Ok(None)
    }
    #[cfg(any(not(target_os = "macos"), test))]
    {
        let _ = (host, source);
        Ok(None)
    }
}

fn token(value: Option<&Value>) -> bool {
    value.and_then(Value::as_str).is_some_and(|token| {
        !token.is_empty() && token.len() <= 16_384 && !token.chars().any(char::is_control)
    })
}

fn login_cache(path: &Path) -> Result<Option<Zeroizing<Vec<u8>>>> {
    let file = match privatefs::open_no_follow(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err(LocalSourceError.into()),
    };
    let metadata = file.metadata().map_err(|_| LocalSourceError)?;
    if !metadata.is_file()
        || metadata.len() > LOGIN_LIMIT
        || privatefs::owner_of(path).map_err(|_| LocalSourceError)?
            != privatefs::current_account().map_err(|_| LocalSourceError)?
    {
        return Err(LocalSourceError.into());
    }
    let mut bytes = Zeroizing::new(Vec::new());
    file.take(LOGIN_LIMIT + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| LocalSourceError)?;
    if bytes.len() as u64 > LOGIN_LIMIT {
        return Err(LocalSourceError.into());
    }
    Ok(Some(bytes))
}

fn publish_login(target: &Path, bytes: &[u8]) -> Result<bool> {
    let root = target.parent().ok_or(LocalSourceError)?;
    crate::config::private_dir(root).map_err(|_| LocalSourceError)?;
    let staging = root.join(format!(".login-{}", uuid::Uuid::new_v4()));
    crate::config::private_write(&staging, bytes).map_err(|_| LocalSourceError)?;
    let published = match std::fs::hard_link(&staging, target) {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Ok(false),
        Err(_) => Err(LocalSourceError),
    };
    let removed = std::fs::remove_file(&staging).map_err(|_| LocalSourceError);
    let published = published?;
    removed?;
    #[cfg(unix)]
    std::fs::File::open(root)
        .and_then(|file| file.sync_all())
        .map_err(|_| LocalSourceError)?;
    Ok(published)
}
