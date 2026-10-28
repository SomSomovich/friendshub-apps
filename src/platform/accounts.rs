//! Private local account registry for native FriendsHub clients.
//!
//! This registry contains account identity and presentation metadata only. It
//! intentionally has no password, session-token, or cryptographic-key fields;
//! session and crypto state remain owned by `friendshub-core`.

use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

const MANIFEST_FILE: &str = "accounts.json";
pub const MAX_LOCAL_ACCOUNTS: usize = 5;
const MAX_MANIFEST_BYTES: u64 = 1024 * 1024;
const MAX_TEXT_BYTES: usize = 4096;
static TEMP_FILE_COUNTER: AtomicU64 = AtomicU64::new(0);

/// One account entry stored locally.
///
/// Only the fields below are serialized. Credentials, tokens, and keys are
/// deliberately excluded; `db_key` is an opaque non-secret database alias
/// passed to `CoreClient::database_path`.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct LocalAccount {
    pub id: String,
    pub fh_number: String,
    pub user_id: i64,
    pub username: String,
    pub avatar_url: Option<String>,
    pub totp_enabled: bool,
    pub device_number: i64,
    pub db_key: String,
}

impl LocalAccount {
    /// Opaque stable alias for `CoreClient::database_path`.
    pub fn db_key(&self) -> &str {
        &self.db_key
    }
}

/// A validated point-in-time snapshot of the local account registry.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct AccountManifest {
    pub accounts: Vec<LocalAccount>,
    pub active_id: Option<String>,
}

impl AccountManifest {
    pub fn active_account(&self) -> Option<&LocalAccount> {
        let active_id = self.active_id.as_deref()?;
        self.accounts.iter().find(|account| account.id == active_id)
    }
}

/// Errors returned by [`AccountStore`]. A corrupt manifest is reported and
/// left at its original path for inspection or recovery; it is never silently
/// replaced with an empty registry.
#[derive(Debug)]
pub enum AccountStoreError {
    Io(io::Error),
    CorruptManifest {
        reason: &'static str,
        line: usize,
        column: usize,
    },
    InvalidAccount(&'static str),
    AccountLimitReached,
    DuplicateAccount,
    AccountNotFound,
    DatabaseKeyChanged,
    LockPoisoned,
    Serialization,
}

impl fmt::Display for AccountStoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(_) => f.write_str("could not access the local account registry"),
            Self::CorruptManifest {
                reason,
                line,
                column,
            } => {
                write!(
                    f,
                    "local account registry is invalid ({reason} at {line}:{column})"
                )
            }
            Self::InvalidAccount(reason) => write!(f, "invalid account entry ({reason})"),
            Self::AccountLimitReached => {
                write!(f, "at most {MAX_LOCAL_ACCOUNTS} accounts can be stored")
            }
            Self::DuplicateAccount => f.write_str("this account is already stored"),
            Self::AccountNotFound => f.write_str("account was not found"),
            Self::DatabaseKeyChanged => f.write_str("an account database alias cannot be changed"),
            Self::LockPoisoned => f.write_str("local account registry is unavailable"),
            Self::Serialization => f.write_str("could not encode the local account registry"),
        }
    }
}

impl std::error::Error for AccountStoreError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            _ => None,
        }
    }
}

impl From<io::Error> for AccountStoreError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

/// Atomic local account registry stored in a private app-data directory.
///
/// Clones share a process-local mutation lock so concurrent UI operations
/// cannot overwrite one another. The directory must be an absolute
/// app-private path, such as the result of `core::storage::app_data_dir()`.
#[derive(Clone)]
pub struct AccountStore {
    directory: PathBuf,
    manifest_path: PathBuf,
    mutation_lock: Arc<Mutex<()>>,
}

impl AccountStore {
    /// Create an account store under an absolute app-data directory.
    ///
    /// On Unix the directory is restricted to mode `0700`, and the manifest
    /// itself is written with mode `0600`.
    pub fn new(app_data_dir: impl AsRef<Path>) -> Result<Self, AccountStoreError> {
        let directory = app_data_dir.as_ref();
        if !directory.is_absolute() {
            return Err(AccountStoreError::Io(io::Error::new(
                io::ErrorKind::InvalidInput,
                "app-data directory must be absolute",
            )));
        }
        fs::create_dir_all(directory)?;
        set_private_directory_permissions(directory)?;
        let directory = directory.to_path_buf();
        Ok(Self {
            manifest_path: directory.join(MANIFEST_FILE),
            directory,
            mutation_lock: Arc::new(Mutex::new(())),
        })
    }

    /// The absolute path of the JSON manifest.
    pub fn manifest_path(&self) -> &Path {
        &self.manifest_path
    }

    /// Read the manifest, returning an empty snapshot when it does not exist.
    ///
    /// Invalid JSON, unsupported data, and semantically inconsistent entries
    /// return `CorruptManifest`; the source file is preserved unchanged.
    pub fn load(&self) -> Result<AccountManifest, AccountStoreError> {
        let _guard = self
            .mutation_lock
            .lock()
            .map_err(|_| AccountStoreError::LockPoisoned)?;
        self.load_unlocked()
    }

    /// Add one account. The first account becomes active; adding later
    /// accounts preserves the current active account.
    pub fn add(&self, account: LocalAccount) -> Result<AccountManifest, AccountStoreError> {
        let _guard = self
            .mutation_lock
            .lock()
            .map_err(|_| AccountStoreError::LockPoisoned)?;
        validate_account(&account)?;
        let mut manifest = self.load_unlocked()?;
        if manifest.accounts.len() >= MAX_LOCAL_ACCOUNTS {
            return Err(AccountStoreError::AccountLimitReached);
        }
        if manifest.accounts.iter().any(|existing| {
            existing.id == account.id
                || existing.fh_number.eq_ignore_ascii_case(&account.fh_number)
                || existing.db_key == account.db_key
        }) {
            return Err(AccountStoreError::DuplicateAccount);
        }
        if manifest.active_id.is_none() {
            manifest.active_id = Some(account.id.clone());
        }
        manifest.accounts.push(account);
        self.save_unlocked(&manifest)?;
        Ok(manifest)
    }

    /// Update mutable presentation/session metadata for an existing account.
    /// Its account ID, FH number, user ID, and database alias are immutable.
    pub fn update(&self, account: LocalAccount) -> Result<AccountManifest, AccountStoreError> {
        let _guard = self
            .mutation_lock
            .lock()
            .map_err(|_| AccountStoreError::LockPoisoned)?;
        validate_account(&account)?;
        let mut manifest = self.load_unlocked()?;
        let index = manifest
            .accounts
            .iter()
            .position(|existing| existing.id == account.id)
            .ok_or(AccountStoreError::AccountNotFound)?;
        let existing = &manifest.accounts[index];
        if existing.db_key != account.db_key {
            return Err(AccountStoreError::DatabaseKeyChanged);
        }
        if existing.fh_number != account.fh_number || existing.user_id != account.user_id {
            return Err(AccountStoreError::InvalidAccount(
                "identity fields cannot change",
            ));
        }
        if manifest
            .accounts
            .iter()
            .enumerate()
            .any(|(other_index, other)| {
                other_index != index
                    && (other.fh_number.eq_ignore_ascii_case(&account.fh_number)
                        || other.db_key == account.db_key)
            })
        {
            return Err(AccountStoreError::DuplicateAccount);
        }
        manifest.accounts[index] = account;
        self.save_unlocked(&manifest)?;
        Ok(manifest)
    }

    /// Remove an account. When removing the active account, the first
    /// remaining account becomes active; an empty registry has no active ID.
    /// The caller remains responsible for revoking the server session through
    /// `friendshub-core` before forgetting local metadata.
    pub fn remove(&self, id: &str) -> Result<AccountManifest, AccountStoreError> {
        let _guard = self
            .mutation_lock
            .lock()
            .map_err(|_| AccountStoreError::LockPoisoned)?;
        let mut manifest = self.load_unlocked()?;
        let index = manifest
            .accounts
            .iter()
            .position(|account| account.id == id)
            .ok_or(AccountStoreError::AccountNotFound)?;
        manifest.accounts.remove(index);
        if manifest.active_id.as_deref() == Some(id) {
            manifest.active_id = manifest.accounts.first().map(|account| account.id.clone());
        }
        self.save_unlocked(&manifest)?;
        Ok(manifest)
    }

    /// Make a stored account active.
    pub fn set_active(&self, id: &str) -> Result<AccountManifest, AccountStoreError> {
        let _guard = self
            .mutation_lock
            .lock()
            .map_err(|_| AccountStoreError::LockPoisoned)?;
        let mut manifest = self.load_unlocked()?;
        if !manifest.accounts.iter().any(|account| account.id == id) {
            return Err(AccountStoreError::AccountNotFound);
        }
        manifest.active_id = Some(id.to_owned());
        self.save_unlocked(&manifest)?;
        Ok(manifest)
    }

    /// Clear the active account without deleting any stored account entry.
    pub fn clear_active(&self) -> Result<AccountManifest, AccountStoreError> {
        let _guard = self
            .mutation_lock
            .lock()
            .map_err(|_| AccountStoreError::LockPoisoned)?;
        let mut manifest = self.load_unlocked()?;
        manifest.active_id = None;
        self.save_unlocked(&manifest)?;
        Ok(manifest)
    }

    fn load_unlocked(&self) -> Result<AccountManifest, AccountStoreError> {
        let metadata = match fs::symlink_metadata(&self.manifest_path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Ok(AccountManifest::default());
            }
            Err(error) => return Err(AccountStoreError::Io(error)),
        };
        if !metadata.file_type().is_file() || metadata.len() > MAX_MANIFEST_BYTES {
            return Err(corrupt("manifest file type or size is invalid", 0, 0));
        }
        set_private_file_permissions(&self.manifest_path)?;
        let file = File::open(&self.manifest_path)?;
        let mut bytes = Vec::with_capacity(metadata.len() as usize);
        file.take(MAX_MANIFEST_BYTES + 1).read_to_end(&mut bytes)?;
        if bytes.len() as u64 > MAX_MANIFEST_BYTES {
            return Err(corrupt("manifest is too large", 0, 0));
        }
        let manifest: AccountManifest = serde_json::from_slice(&bytes)
            .map_err(|error| corrupt("JSON is invalid", error.line(), error.column()))?;
        validate_manifest(&manifest)?;
        Ok(manifest)
    }

    fn save_unlocked(&self, manifest: &AccountManifest) -> Result<(), AccountStoreError> {
        validate_manifest(manifest)?;
        let bytes =
            serde_json::to_vec_pretty(manifest).map_err(|_| AccountStoreError::Serialization)?;
        let (mut file, temporary_path) = create_temporary_manifest(&self.directory, MANIFEST_FILE)?;
        let result = file.write_all(&bytes).and_then(|()| file.sync_all());
        drop(file);
        if let Err(error) = result {
            let _ = fs::remove_file(&temporary_path);
            return Err(AccountStoreError::Io(error));
        }
        let result = fs::rename(&temporary_path, &self.manifest_path)
            .and_then(|()| set_private_file_permissions(&self.manifest_path))
            .and_then(|()| sync_directory(&self.directory));
        if result.is_err() {
            let _ = fs::remove_file(&temporary_path);
        }
        result.map_err(AccountStoreError::Io)
    }
}

fn validate_manifest(manifest: &AccountManifest) -> Result<(), AccountStoreError> {
    if manifest.accounts.len() > MAX_LOCAL_ACCOUNTS {
        return Err(corrupt("account limit is exceeded", 0, 0));
    }
    for (index, account) in manifest.accounts.iter().enumerate() {
        validate_account(account).map_err(|_| corrupt("account entry is invalid", 0, 0))?;
        if manifest.accounts[..index].iter().any(|previous| {
            previous.id == account.id
                || previous.fh_number.eq_ignore_ascii_case(&account.fh_number)
                || previous.db_key == account.db_key
        }) {
            return Err(corrupt("account entries are duplicated", 0, 0));
        }
    }
    if manifest.active_id.as_ref().is_some_and(|active_id| {
        !manifest
            .accounts
            .iter()
            .any(|account| &account.id == active_id)
    }) {
        return Err(corrupt("active account does not exist", 0, 0));
    }
    Ok(())
}

fn validate_account(account: &LocalAccount) -> Result<(), AccountStoreError> {
    if account.id.trim().is_empty() || account.id.len() > MAX_TEXT_BYTES {
        return Err(AccountStoreError::InvalidAccount("id"));
    }
    if account.fh_number.trim().is_empty() || account.fh_number.len() > MAX_TEXT_BYTES {
        return Err(AccountStoreError::InvalidAccount("fh_number"));
    }
    if account.user_id <= 0 {
        return Err(AccountStoreError::InvalidAccount("user_id"));
    }
    if account.username.len() > MAX_TEXT_BYTES {
        return Err(AccountStoreError::InvalidAccount("username"));
    }
    if account
        .avatar_url
        .as_ref()
        .is_some_and(|url| url.len() > MAX_TEXT_BYTES)
    {
        return Err(AccountStoreError::InvalidAccount("avatar_url"));
    }
    if !(1..=u32::MAX as i64).contains(&account.device_number) {
        return Err(AccountStoreError::InvalidAccount("device_number"));
    }
    if account.db_key.trim().is_empty() || account.db_key.len() > MAX_TEXT_BYTES {
        return Err(AccountStoreError::InvalidAccount("db_key"));
    }
    Ok(())
}

fn corrupt(reason: &'static str, line: usize, column: usize) -> AccountStoreError {
    AccountStoreError::CorruptManifest {
        reason,
        line,
        column,
    }
}

fn create_temporary_manifest(
    directory: &Path,
    file_name: &str,
) -> Result<(File, PathBuf), AccountStoreError> {
    for _ in 0..32 {
        let sequence = TEMP_FILE_COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = directory.join(format!(
            ".{file_name}.{}.{}.tmp",
            std::process::id(),
            sequence
        ));
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        set_create_mode(&mut options);
        match options.open(&path) {
            Ok(file) => return Ok((file, path)),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(AccountStoreError::Io(error)),
        }
    }
    Err(AccountStoreError::Io(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "could not reserve a temporary account-manifest path",
    )))
}

fn set_create_mode(options: &mut OpenOptions) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    #[cfg(not(unix))]
    let _ = options;
}

fn set_private_directory_permissions(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

fn set_private_file_permissions(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

fn sync_directory(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    File::open(path)?.sync_all()?;
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}
