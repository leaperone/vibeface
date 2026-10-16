use anyhow::{Context, Result, anyhow, bail};
use argon2::{
    Argon2,
    password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString},
};
use rand::{RngCore, rngs::OsRng};
use rusqlite::{Connection, OpenFlags, OptionalExtension, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, OpenOptions},
    path::Path,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use uuid::Uuid;

pub const SESSION_MAX_AGE_SECONDS: i64 = 7 * 24 * 60 * 60;
const INVITE_TTL_MS: i64 = 10 * 60 * 1000;
const FAILURE_WINDOW_MS: i64 = 15 * 60 * 1000;
const MAX_ACCOUNT_FAILURES: i64 = 10;
const MAX_ACCOUNTS: i64 = 256;
const INVALID_LOGIN: &str = "invalid username or password";

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct Account {
    pub account_id: String,
    pub username: String,
    pub network_id: String,
}

pub struct Login {
    pub account: Account,
    pub token: String,
    pub csrf: String,
}

pub struct LoginSession {
    pub account: Account,
    pub csrf: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InviteGrant {
    pub network_id: String,
    pub roles: Vec<String>,
    pub claimed: bool,
}

pub struct AuthStore {
    connection: Connection,
    dummy_password_hash: String,
}

impl AuthStore {
    pub fn open(path: &Path) -> Result<Self> {
        private_file(path)?;
        let canonical_path = path.canonicalize().context("resolve auth database path")?;
        let path = canonical_path.as_path();
        for suffix in ["-wal", "-shm", "-journal"] {
            let mut sidecar = path.as_os_str().to_owned();
            sidecar.push(suffix);
            if Path::new(&sidecar).symlink_metadata().is_ok() {
                set_private(Path::new(&sidecar))?;
            }
        }
        let connection = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NOFOLLOW,
        )
        .context("open private auth database")?;
        connection.busy_timeout(Duration::from_secs(5))?;
        connection.pragma_update(None, "foreign_keys", true)?;
        connection.pragma_update(None, "journal_mode", "WAL")?;
        connection.execute_batch(
            "CREATE TABLE IF NOT EXISTS auth_accounts (
                 account_id TEXT PRIMARY KEY,
                 username TEXT NOT NULL UNIQUE,
                 network_id TEXT NOT NULL UNIQUE,
                 password_hash TEXT NOT NULL,
                 failed_attempts INTEGER NOT NULL DEFAULT 0,
                 failure_window_ms INTEGER NOT NULL DEFAULT 0
             );
             CREATE TABLE IF NOT EXISTS auth_owner_keys (
                 network_id TEXT PRIMARY KEY,
                 secret_key TEXT NOT NULL,
                 FOREIGN KEY (network_id) REFERENCES auth_accounts(network_id)
             );
             CREATE TABLE IF NOT EXISTS auth_sessions (
                 token_hash TEXT PRIMARY KEY,
                 account_id TEXT NOT NULL REFERENCES auth_accounts(account_id),
                 csrf TEXT NOT NULL,
                 created_at_ms INTEGER NOT NULL,
                 expires_at_ms INTEGER NOT NULL
             );
             CREATE INDEX IF NOT EXISTS auth_sessions_account ON auth_sessions(account_id);
             CREATE TABLE IF NOT EXISTS auth_invites (
                 code_hash TEXT PRIMARY KEY,
                 network_id TEXT NOT NULL REFERENCES auth_accounts(network_id),
                 roles_json TEXT NOT NULL,
                 expires_at_ms INTEGER NOT NULL,
                 claimed_node_id TEXT
             );
             CREATE TABLE IF NOT EXISTS auth_password_rate (
                 kind TEXT PRIMARY KEY CHECK (kind IN ('login', 'register')),
                 window_ms INTEGER NOT NULL,
                 attempts INTEGER NOT NULL
             );",
        )?;
        for suffix in ["-wal", "-shm"] {
            let mut sidecar = path.as_os_str().to_owned();
            sidecar.push(suffix);
            if Path::new(&sidecar).exists() {
                set_private(Path::new(&sidecar))?;
            }
        }
        Ok(Self {
            connection,
            dummy_password_hash: hash_password(&random_secret())?,
        })
    }

    pub fn register(
        &mut self,
        username: &str,
        password: &str,
        network_id: &str,
        owner_secret: String,
    ) -> Result<Account> {
        if owner_secret.len() != 64 || !owner_secret.bytes().all(|c| c.is_ascii_hexdigit()) {
            bail!("owner secret must be a 32-byte hex key");
        }
        self.create_account(username, password, network_id, Some(&owner_secret))
    }

    pub fn install_account(
        &mut self,
        username: &str,
        password: &str,
        network_id: &str,
    ) -> Result<Account> {
        self.create_account(username, password, network_id, None)
    }

    fn create_account(
        &mut self,
        username: &str,
        password: &str,
        network_id: &str,
        owner_secret: Option<&str>,
    ) -> Result<Account> {
        let username = normalize_username(username)?;
        validate_password(password)?;
        validate_network(network_id)?;
        self.reserve_password_attempt("register", now_ms())?;
        let password_hash = hash_password(password)?;
        let account = Account {
            account_id: Uuid::new_v4().to_string(),
            username,
            network_id: network_id.to_owned(),
        };
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let count: i64 =
            transaction.query_row("SELECT COUNT(*) FROM auth_accounts", [], |row| row.get(0))?;
        if count >= MAX_ACCOUNTS {
            bail!("account limit reached");
        }
        let inserted = transaction.execute(
            "INSERT INTO auth_accounts (account_id, username, network_id, password_hash)
             VALUES (?1, ?2, ?3, ?4) ON CONFLICT DO NOTHING",
            params![
                account.account_id,
                account.username,
                account.network_id,
                password_hash
            ],
        )?;
        if inserted != 1 {
            bail!("username or network already registered");
        }
        if let Some(owner_secret) = owner_secret {
            transaction.execute(
                "INSERT INTO auth_owner_keys (network_id, secret_key) VALUES (?1, ?2)",
                params![network_id, owner_secret],
            )?;
        }
        transaction.commit()?;
        Ok(account)
    }

    pub fn login(&mut self, username: &str, password: &str) -> Result<Login> {
        let username = normalize_username(username).map_err(|_| anyhow!(INVALID_LOGIN))?;
        validate_password(password).map_err(|_| anyhow!(INVALID_LOGIN))?;
        let now = now_ms();
        self.reserve_password_attempt("login", now)?;
        let record = self
            .connection
            .query_row(
                "SELECT account_id, username, network_id, password_hash, failed_attempts,
                        failure_window_ms FROM auth_accounts WHERE username = ?1",
                [&username],
                |row| {
                    Ok((
                        account_from_row(row)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, i64>(4)?,
                        row.get::<_, i64>(5)?,
                    ))
                },
            )
            .optional()?;
        let locked = record.as_ref().is_some_and(|(_, _, failures, window)| {
            *failures >= MAX_ACCOUNT_FAILURES && now < window.saturating_add(FAILURE_WINDOW_MS)
        });
        let password_hash = record
            .as_ref()
            .filter(|_| !locked)
            .map(|(_, hash, _, _)| hash.as_str())
            .unwrap_or(&self.dummy_password_hash);
        let valid = PasswordHash::new(password_hash).is_ok_and(|hash| {
            Argon2::default()
                .verify_password(password.as_bytes(), &hash)
                .is_ok()
        });
        let Some((account, _, _, _)) = record else {
            bail!(INVALID_LOGIN);
        };
        if !valid || locked {
            self.connection.execute(
                "UPDATE auth_accounts SET
                     failed_attempts = CASE WHEN failure_window_ms > ?3
                         THEN MIN(failed_attempts + 1, ?4) ELSE 1 END,
                     failure_window_ms = CASE WHEN failure_window_ms > ?3
                         THEN failure_window_ms ELSE ?2 END
                 WHERE account_id = ?1",
                params![
                    account.account_id,
                    now,
                    now - FAILURE_WINDOW_MS,
                    MAX_ACCOUNT_FAILURES
                ],
            )?;
            bail!(INVALID_LOGIN);
        }

        let token = random_secret();
        let csrf = random_secret();
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let locked: bool = transaction.query_row(
            "SELECT failed_attempts >= ?2 AND failure_window_ms > ?3
             FROM auth_accounts WHERE account_id = ?1",
            params![
                account.account_id,
                MAX_ACCOUNT_FAILURES,
                now - FAILURE_WINDOW_MS
            ],
            |row| row.get(0),
        )?;
        if locked {
            bail!(INVALID_LOGIN);
        }
        transaction.execute(
            "UPDATE auth_accounts SET failed_attempts = 0, failure_window_ms = 0
             WHERE account_id = ?1",
            [&account.account_id],
        )?;
        transaction.execute("DELETE FROM auth_sessions WHERE expires_at_ms <= ?1", [now])?;
        transaction.execute(
            "DELETE FROM auth_sessions WHERE account_id = ?1 AND token_hash NOT IN (
                 SELECT token_hash FROM auth_sessions WHERE account_id = ?1
                 ORDER BY created_at_ms DESC, rowid DESC LIMIT 15
             )",
            [&account.account_id],
        )?;
        transaction.execute(
            "INSERT INTO auth_sessions
             (token_hash, account_id, csrf, created_at_ms, expires_at_ms)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                secret_hash(&token),
                account.account_id,
                csrf,
                now,
                now + SESSION_MAX_AGE_SECONDS * 1000
            ],
        )?;
        transaction.commit()?;
        Ok(Login {
            account,
            token,
            csrf,
        })
    }

    fn reserve_password_attempt(&mut self, kind: &str, now: i64) -> Result<()> {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let previous: Option<(i64, i64)> = transaction
            .query_row(
                "SELECT window_ms, attempts FROM auth_password_rate WHERE kind = ?1",
                [kind],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let (window, attempts) = match previous {
            Some((window, attempts)) if now < window.saturating_add(60_000) => (window, attempts),
            _ => (now, 0),
        };
        if attempts >= 60 {
            bail!(INVALID_LOGIN);
        }
        transaction.execute(
            "INSERT INTO auth_password_rate (kind, window_ms, attempts) VALUES (?1, ?2, ?3)
             ON CONFLICT(kind) DO UPDATE SET window_ms = excluded.window_ms,
                                         attempts = excluded.attempts",
            params![kind, window, attempts + 1],
        )?;
        transaction.commit()?;
        Ok(())
    }

    pub fn authenticate(&self, token: &str) -> Result<Option<LoginSession>> {
        if !is_secret(token) {
            return Ok(None);
        }
        self.connection
            .query_row(
                "SELECT a.account_id, a.username, a.network_id, s.csrf
                 FROM auth_sessions s JOIN auth_accounts a ON a.account_id = s.account_id
                 WHERE s.token_hash = ?1 AND s.expires_at_ms > ?2",
                params![secret_hash(token), now_ms()],
                |row| {
                    Ok(LoginSession {
                        account: account_from_row(row)?,
                        csrf: row.get(3)?,
                    })
                },
            )
            .optional()
            .map_err(Into::into)
    }

    pub fn logout(&mut self, token: &str) -> Result<()> {
        if is_secret(token) {
            self.connection.execute(
                "DELETE FROM auth_sessions WHERE token_hash = ?1",
                [secret_hash(token)],
            )?;
        }
        Ok(())
    }

    pub fn owner(&self, network_id: &str) -> Result<Option<String>> {
        self.connection
            .query_row(
                "SELECT secret_key FROM auth_owner_keys WHERE network_id = ?1",
                [network_id],
                |row| row.get(0),
            )
            .optional()
            .map_err(Into::into)
    }

    pub fn accounts(&self) -> Result<Vec<Account>> {
        self.connection
            .prepare(
                "SELECT account_id, username, network_id FROM auth_accounts ORDER BY username",
            )?
            .query_map([], account_from_row)?
            .collect::<rusqlite::Result<_>>()
            .map_err(Into::into)
    }

    pub fn create_invite(&mut self, network_id: &str, mut roles: Vec<String>) -> Result<String> {
        validate_network(network_id)?;
        if roles.is_empty()
            || roles.len() > 16
            || roles.iter().any(|role| {
                role.is_empty()
                    || role.len() > 32
                    || !role.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_')
            })
        {
            bail!("invalid invitation roles");
        }
        roles.sort_unstable();
        roles.dedup();
        let roles_json = serde_json::to_string(&roles)?;
        let code = random_secret();
        let now = now_ms();
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        transaction.execute(
            "DELETE FROM auth_invites WHERE expires_at_ms <= ?1 AND claimed_node_id IS NULL",
            [now],
        )?;
        let pending: i64 = transaction.query_row(
            "SELECT COUNT(*) FROM auth_invites WHERE network_id = ?1 AND claimed_node_id IS NULL",
            [network_id],
            |row| row.get(0),
        )?;
        if pending >= 16 {
            bail!("invitation limit reached");
        }
        transaction.execute(
            "INSERT INTO auth_invites (code_hash, network_id, roles_json, expires_at_ms)
             VALUES (?1, ?2, ?3, ?4)",
            params![
                secret_hash(&code),
                network_id,
                roles_json,
                now + INVITE_TTL_MS
            ],
        )?;
        transaction.commit()?;
        Ok(code)
    }

    pub fn pairing_invite(&self, code: &str, node_id: &str) -> Result<InviteGrant> {
        if !is_secret(code) || !is_secret(node_id) {
            bail!("invitation is invalid or expired");
        }
        let grant: Option<(String, String, Option<String>)> = self
            .connection
            .query_row(
                "SELECT network_id, roles_json, claimed_node_id FROM auth_invites
                 WHERE code_hash = ?1 AND (expires_at_ms > ?2 OR claimed_node_id = ?3)",
                params![secret_hash(code), now_ms(), node_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?;
        let (network_id, roles, claimed_node_id) =
            grant.context("invitation is invalid or expired")?;
        if claimed_node_id
            .as_deref()
            .is_some_and(|claimed| claimed != node_id)
        {
            bail!("invitation is invalid or expired");
        }
        let roles = serde_json::from_str(&roles).context("invalid stored invitation")?;
        Ok(InviteGrant {
            network_id,
            roles,
            claimed: claimed_node_id.is_some(),
        })
    }

    pub fn consume_invite(&mut self, code: &str, node_id: &str) -> Result<()> {
        if !is_secret(code) || !is_secret(node_id) {
            bail!("invitation is invalid or expired");
        }
        let changed = self.connection.execute(
            "UPDATE auth_invites SET claimed_node_id = ?1
             WHERE code_hash = ?2 AND ((expires_at_ms > ?3 AND claimed_node_id IS NULL)
             OR claimed_node_id = ?1)",
            params![node_id, secret_hash(code), now_ms()],
        )?;
        if changed != 1 {
            bail!("invitation is invalid or expired");
        }
        Ok(())
    }

    pub fn complete_invite(&mut self, code: &str, node_id: &str) -> Result<()> {
        self.connection.execute(
            "DELETE FROM auth_invites WHERE code_hash = ?1 AND claimed_node_id = ?2",
            params![secret_hash(code), node_id],
        )?;
        Ok(())
    }
}

fn account_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Account> {
    Ok(Account {
        account_id: row.get(0)?,
        username: row.get(1)?,
        network_id: row.get(2)?,
    })
}

fn normalize_username(username: &str) -> Result<String> {
    if !(3..=64).contains(&username.len())
        || !username
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'_' | b'.' | b'-'))
    {
        bail!("username must contain 3 to 64 ASCII letters, digits, dots, dashes or underscores");
    }
    Ok(username.to_ascii_lowercase())
}

fn validate_password(password: &str) -> Result<()> {
    if !(12..=1024).contains(&password.len()) {
        bail!("password must contain 12 to 1024 bytes");
    }
    Ok(())
}

fn validate_network(network_id: &str) -> Result<()> {
    if network_id.is_empty()
        || network_id.len() > 256
        || !network_id.bytes().all(|c| c.is_ascii_graphic())
    {
        bail!("invalid personal network ID");
    }
    Ok(())
}

fn hash_password(password: &str) -> Result<String> {
    let salt = SaltString::generate(&mut OsRng);
    Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map(|hash| hash.to_string())
        .map_err(|_| anyhow!("password hashing failed"))
}

fn random_secret() -> String {
    let mut secret = [0_u8; 32];
    OsRng.fill_bytes(&mut secret);
    hex::encode(secret)
}

fn is_secret(secret: &str) -> bool {
    secret.len() == 64 && secret.bytes().all(|c| c.is_ascii_hexdigit())
}

fn secret_hash(secret: &str) -> String {
    hex::encode(Sha256::digest(secret.as_bytes()))
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(i64::MAX as u128) as i64
}

fn private_file(path: &Path) -> Result<()> {
    if let Ok(metadata) = fs::symlink_metadata(path)
        && (!metadata.is_file() || metadata.file_type().is_symlink())
    {
        bail!("auth database must be a regular file");
    }
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options.open(path).context("create private auth database")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

fn set_private(path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        bail!("auth database sidecar must be a regular file");
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const PASSWORD: &str = "a correct local password";

    struct Fixture {
        root: std::path::PathBuf,
        store: AuthStore,
    }

    impl Fixture {
        fn new() -> Self {
            let root = std::env::temp_dir().join(format!("vibeface-auth-{}", Uuid::new_v4()));
            fs::create_dir(&root).unwrap();
            let store = AuthStore::open(&root.join("auth.sqlite")).unwrap();
            Self { root, store }
        }

        fn register(&mut self) -> Account {
            self.store
                .register("Harry", PASSWORD, "network-a", "12".repeat(32))
                .unwrap()
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    #[test]
    fn login_is_private_expiring_and_logout_is_idempotent() {
        let mut fixture = Fixture::new();
        let account = fixture.register();
        let login = fixture.store.login("HARRY", PASSWORD).unwrap();
        assert_eq!(login.account, account);
        assert_eq!(login.token.len(), 64);
        assert_eq!(login.csrf.len(), 64);
        let session = fixture.store.authenticate(&login.token).unwrap().unwrap();
        assert_eq!(session.account, account);
        assert_eq!(session.csrf, login.csrf);
        let (hash, password_hash): (String, String) = fixture
            .store
            .connection
            .query_row(
                "SELECT s.token_hash, a.password_hash FROM auth_sessions s
                 JOIN auth_accounts a ON a.account_id = s.account_id",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(hash, secret_hash(&login.token));
        assert_ne!(hash, login.token);
        assert!(password_hash.starts_with("$argon2id$"));
        assert!(!password_hash.contains(PASSWORD));
        fixture
            .store
            .connection
            .execute("UPDATE auth_sessions SET expires_at_ms = 0", [])
            .unwrap();
        assert!(fixture.store.authenticate(&login.token).unwrap().is_none());
        fixture.store.logout(&login.token).unwrap();
        fixture.store.logout(&login.token).unwrap();
        assert!(fixture.store.authenticate("bad-token").unwrap().is_none());
    }

    #[test]
    fn account_uniqueness_owner_separation_and_validation() {
        let mut fixture = Fixture::new();
        fixture.register();
        assert_eq!(
            fixture.store.owner("network-a").unwrap().unwrap(),
            "12".repeat(32)
        );
        assert!(fixture.store.owner("network-b").unwrap().is_none());
        assert!(
            fixture
                .store
                .install_account("harry", PASSWORD, "network-b")
                .is_err()
        );
        assert!(
            fixture
                .store
                .install_account("someone", PASSWORD, "network-a")
                .is_err()
        );
        fixture
            .store
            .install_account("someone", PASSWORD, "network-b")
            .unwrap();
        assert!(fixture.store.owner("network-b").unwrap().is_none());
        assert_eq!(fixture.store.accounts().unwrap().len(), 2);
        for username in ["ab", "has space", "../", "名字"] {
            assert!(
                fixture
                    .store
                    .install_account(username, PASSWORD, "network-c")
                    .is_err()
            );
        }
        assert!(
            fixture
                .store
                .install_account("someone3", "too short", "network-c")
                .is_err()
        );
    }

    #[test]
    fn wrong_passwords_lock_out_and_do_not_create_sessions() {
        let mut fixture = Fixture::new();
        fixture.register();
        let absent = fixture.store.login("missing", PASSWORD).err().unwrap();
        let wrong = fixture
            .store
            .login("harry", "an incorrect password")
            .err()
            .unwrap();
        assert_eq!(absent.to_string(), wrong.to_string());
        fixture
            .store
            .connection
            .execute(
                "UPDATE auth_accounts SET failed_attempts = ?1, failure_window_ms = ?2",
                params![MAX_ACCOUNT_FAILURES, now_ms()],
            )
            .unwrap();
        assert!(fixture.store.login("harry", PASSWORD).is_err());
        assert_eq!(
            fixture
                .store
                .connection
                .query_row("SELECT COUNT(*) FROM auth_sessions", [], |row| row
                    .get::<_, i64>(0))
                .unwrap(),
            0
        );
        fixture
            .store
            .connection
            .execute("UPDATE auth_accounts SET failure_window_ms = 0", [])
            .unwrap();
        assert!(fixture.store.login("harry", PASSWORD).is_ok());
        fixture
            .store
            .connection
            .execute(
                "UPDATE auth_password_rate SET attempts = 60 WHERE kind = 'register'",
                [],
            )
            .unwrap();
        assert!(fixture.store.login("harry", PASSWORD).is_ok());
        fixture
            .store
            .connection
            .execute(
                "UPDATE auth_password_rate SET attempts = 60 WHERE kind = 'login'",
                [],
            )
            .unwrap();
        assert!(fixture.store.login("harry", PASSWORD).is_err());
    }

    #[test]
    fn pairing_invites_are_network_scoped_single_use_and_expire() {
        let mut fixture = Fixture::new();
        fixture.register();
        let roles = vec!["scheduler".to_owned(), "executor".to_owned()];
        let code = fixture
            .store
            .create_invite("network-a", roles.clone())
            .unwrap();
        let node_id = "11".repeat(32);
        let grant = fixture.store.pairing_invite(&code, &node_id).unwrap();
        assert_eq!(grant.network_id, "network-a");
        assert_eq!(grant.roles, vec!["executor", "scheduler"]);
        assert!(!grant.claimed);
        fixture.store.consume_invite(&code, &node_id).unwrap();
        assert!(
            fixture
                .store
                .consume_invite(&code, &"22".repeat(32))
                .is_err()
        );
        let expired = fixture.store.create_invite("network-a", roles).unwrap();
        fixture
            .store
            .connection
            .execute("UPDATE auth_invites SET expires_at_ms = 0", [])
            .unwrap();
        assert!(fixture.store.consume_invite(&expired, &node_id).is_err());
        assert!(
            fixture
                .store
                .create_invite("network-b", vec!["executor".to_owned()])
                .is_err()
        );

        let code = fixture
            .store
            .create_invite("network-a", vec!["executor".to_owned()])
            .unwrap();
        let barrier = std::sync::Barrier::new(2);
        let accepted = std::thread::scope(|scope| {
            let root = &fixture.root;
            let barrier = &barrier;
            let code = &code;
            let attempts: Vec<_> = (0..2)
                .map(|index| {
                    let node_id = format!("{index:064x}");
                    scope.spawn(move || {
                        let mut store = AuthStore::open(&root.join("auth.sqlite")).unwrap();
                        barrier.wait();
                        store.consume_invite(code, &node_id).is_ok()
                    })
                })
                .collect();
            attempts
                .into_iter()
                .map(|attempt| attempt.join().unwrap())
                .filter(|accepted| *accepted)
                .count()
        });
        assert_eq!(accepted, 1);
    }

    #[test]
    fn claimed_invite_survives_reopen_for_only_the_same_device() {
        let mut fixture = Fixture::new();
        fixture.register();
        let code = fixture
            .store
            .create_invite("network-a", vec!["executor".into()])
            .unwrap();
        let node_id = "33".repeat(32);
        fixture.store.consume_invite(&code, &node_id).unwrap();
        fixture
            .store
            .connection
            .execute("UPDATE auth_invites SET expires_at_ms = 0", [])
            .unwrap();
        fixture
            .store
            .create_invite("network-a", vec!["executor".into()])
            .unwrap();
        let mut reopened = AuthStore::open(&fixture.root.join("auth.sqlite")).unwrap();
        let grant = reopened.pairing_invite(&code, &node_id).unwrap();
        assert!(grant.claimed);
        assert_eq!(grant.network_id, "network-a");
        assert_eq!(grant.roles, vec!["executor"]);
        reopened.consume_invite(&code, &node_id).unwrap();
        assert!(reopened.pairing_invite(&code, &"44".repeat(32)).is_err());
        assert!(reopened.consume_invite(&code, &"44".repeat(32)).is_err());
        reopened.complete_invite(&code, &"44".repeat(32)).unwrap();
        assert!(reopened.pairing_invite(&code, &node_id).is_ok());
        reopened.complete_invite(&code, &node_id).unwrap();
        assert!(reopened.pairing_invite(&code, &node_id).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn database_and_wal_are_private_and_symlinks_are_rejected() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let mut fixture = Fixture::new();
        fixture.register();
        for suffix in ["", "-wal", "-shm"] {
            let path = fixture.root.join(format!("auth.sqlite{suffix}"));
            let mode = fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        let link = fixture.root.join("linked.sqlite");
        symlink(fixture.root.join("auth.sqlite"), &link).unwrap();
        assert!(AuthStore::open(&link).is_err());

        let db = fixture.root.join("fresh.sqlite");
        symlink(
            fixture.root.join("auth.sqlite"),
            db.with_extension("sqlite-wal"),
        )
        .unwrap();
        assert!(AuthStore::open(&db).is_err());
    }
}
