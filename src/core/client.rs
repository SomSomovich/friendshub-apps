use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::thread::{self, JoinHandle};

use friendshub_core::Client;
use friendshub_core::config::{Config, DiscoveryConfig};
use friendshub_core::error::Error as CoreError;

use super::CoreEventHandler;
use friendshub_core::events::Event;

/// A core failure safe to present to the application UI.
///
/// The core's server errors may contain response bodies. Those bodies are
/// deliberately discarded here so credentials or other private payloads can
/// never be surfaced by a generic UI error path.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CoreCallFailure {
    Configuration,
    Database,
    Runtime,
    Unsupported,
    NotAuthenticated,
    Network,
    Server { status: u16 },
    Internal,
}

impl CoreCallFailure {
    /// Stable category for application-level error handling.
    pub const fn code(self) -> &'static str {
        match self {
            Self::Configuration => "configuration",
            Self::Database => "database",
            Self::Runtime => "runtime",
            Self::Unsupported => "unsupported",
            Self::NotAuthenticated => "not_authenticated",
            Self::Network => "network",
            Self::Server { .. } => "server",
            Self::Internal => "internal",
        }
    }

    /// Non-sensitive Russian message suitable for the UI.
    pub const fn user_message(self) -> &'static str {
        match self {
            Self::Configuration => "Не удалось настроить клиент FriendsHub.",
            Self::Database => "Не удалось открыть защищённое хранилище.",
            Self::Runtime => "Не удалось запустить клиент FriendsHub.",
            Self::Unsupported => "Эта операция не поддерживается текущей версией ядра.",
            Self::NotAuthenticated => "Войдите в аккаунт, чтобы продолжить.",
            Self::Network => "Не удалось связаться с сервером. Проверьте подключение.",
            Self::Server { status: 401 } => "Сервер отклонил запрос. Проверьте данные входа.",
            Self::Server { status: 403 } => "У аккаунта нет доступа к этому действию.",
            Self::Server { status: 404 } => "Запрошенные данные не найдены.",
            Self::Server { status: 429 } => "Слишком много запросов. Повторите позже.",
            Self::Server { .. } => "Сервер не смог выполнить запрос.",
            Self::Internal => "Не удалось выполнить операцию в клиенте FriendsHub.",
        }
    }
}

impl From<CoreError> for CoreCallFailure {
    fn from(error: CoreError) -> Self {
        match error {
            CoreError::Config(_) => Self::Configuration,
            CoreError::Database(_) => Self::Database,
            CoreError::ActorClosed | CoreError::SpawnThread(_) | CoreError::RuntimeBuild(_) => {
                Self::Runtime
            }
            CoreError::UnknownMethod(_) | CoreError::NotImplemented => Self::Unsupported,
            CoreError::NotAuthenticated => Self::NotAuthenticated,
            CoreError::Network(_) => Self::Network,
            CoreError::Server { status, .. } => Self::Server { status },
            CoreError::InvalidPayload(_) | CoreError::Internal(_) => Self::Internal,
        }
    }
}

#[derive(Default)]
struct DatabaseLifecycle {
    opening: Mutex<()>,
    generation: AtomicU64,
    clients: Mutex<Vec<Weak<Client>>>,
}

fn database_lifecycle(path: &Path) -> Arc<DatabaseLifecycle> {
    static PATHS: OnceLock<Mutex<HashMap<PathBuf, Arc<DatabaseLifecycle>>>> = OnceLock::new();
    PATHS
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .entry(path.to_path_buf())
        .or_default()
        .clone()
}

fn removal_requested(path: &Path) -> Result<bool, CoreCallFailure> {
    match std::fs::symlink_metadata(CoreClient::removal_marker_path(path)) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(_) => Err(CoreCallFailure::Database),
    }
}

/// One native `friendshub-core` client instance.
///
/// A client is bound to one database path/account. Keep one instance per local
/// account; the core stores that account's session and crypto state in the
/// encrypted database and platform secure store.
pub struct CoreClient {
    inner: Arc<Client>,
}

impl CoreClient {
    /// Open the native core against an account database at `db_path`.
    ///
    /// `Client::open` performs discovery and database migrations and may block
    /// on disk or network I/O. Call this from a worker thread, never from the
    /// Slint event-loop thread.
    pub fn open(db_path: impl Into<PathBuf>) -> Result<Self, CoreCallFailure> {
        let db_path = db_path.into();
        let lifecycle = database_lifecycle(&db_path);
        let generation = lifecycle.generation.load(Ordering::SeqCst);
        if removal_requested(&db_path)? {
            return Err(CoreCallFailure::Database);
        }
        Self::open_with_generation(db_path, lifecycle, generation)
    }

    fn open_with_generation(
        db_path: PathBuf,
        lifecycle: Arc<DatabaseLifecycle>,
        generation: u64,
    ) -> Result<Self, CoreCallFailure> {
        let _guard = lifecycle
            .opening
            .lock()
            .map_err(|_| CoreCallFailure::Runtime)?;
        if lifecycle.generation.load(Ordering::SeqCst) != generation || removal_requested(&db_path)?
        {
            return Err(CoreCallFailure::Database);
        }
        if !db_path.is_absolute() {
            return Err(CoreCallFailure::Configuration);
        }
        if let Some(parent) = db_path.parent() {
            std::fs::create_dir_all(parent).map_err(|_| CoreCallFailure::Database)?;
        }
        let config = Config {
            db_path,
            api_base: String::new(),
            ws_url: String::new(),
            discovery: DiscoveryConfig::default(),
            log_level: "info".to_owned(),
            log_format: "text".to_owned(),
        };
        let client = Client::open(config).map_err(CoreCallFailure::from)?;
        if lifecycle.generation.load(Ordering::SeqCst) != generation {
            client.shutdown_local().map_err(CoreCallFailure::from)?;
            return Err(CoreCallFailure::Database);
        }
        let inner = Arc::new(client);
        let mut clients = lifecycle
            .clients
            .lock()
            .map_err(|_| CoreCallFailure::Runtime)?;
        clients.retain(|client| client.strong_count() > 0);
        clients.push(Arc::downgrade(&inner));
        Ok(Self { inner })
    }

    /// Open a native core client on a worker thread.
    ///
    /// The completion runs on that worker thread. The caller should use
    /// `slint::invoke_from_event_loop` with a weak component handle before
    /// updating UI state. If the operating system cannot create the worker,
    /// this method returns that `io::Error` and does not invoke the completion.
    pub fn open_async(
        db_path: impl Into<PathBuf>,
        completion: impl FnOnce(Result<Self, CoreCallFailure>) + Send + 'static,
    ) -> io::Result<JoinHandle<()>> {
        let db_path = db_path.into();
        let lifecycle = database_lifecycle(&db_path);
        let generation = lifecycle.generation.load(Ordering::SeqCst);
        if removal_requested(&db_path).unwrap_or(true) {
            return Err(io::Error::other("local account removal is pending"));
        }
        thread::Builder::new()
            .name("friendshub-core-open".to_owned())
            .spawn(move || completion(Self::open_with_generation(db_path, lifecycle, generation)))
    }

    /// Returns the database path expected by `Client::open` without changing
    /// the path or creating it. The platform layer is responsible for creating
    /// its writable application-data directory.
    pub fn database_path(data_dir: &Path, account_key: &str) -> PathBuf {
        let safe_key = if account_key.is_empty() {
            "pending".to_owned()
        } else {
            account_key
                .as_bytes()
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>()
        };
        data_dir.join("accounts").join(format!("{safe_key}.sqlite"))
    }

    /// Durable app-owned removal journal checked by core before database open.
    pub fn removal_marker_path(db_path: &Path) -> PathBuf {
        friendshub_core::db::pool::removal_marker_path(db_path)
    }

    /// Invalidate every previously queued open before cleanup begins.
    pub fn mark_removal(db_path: &Path) {
        database_lifecycle(db_path)
            .generation
            .fetch_add(1, Ordering::SeqCst);
    }

    /// Stop local calls, callback delivery and database handles. This does not
    /// revoke or otherwise change the server session. Run on a worker thread.
    pub fn shutdown_local(&self) -> Result<(), CoreCallFailure> {
        self.inner.shutdown_local().map_err(CoreCallFailure::from)
    }

    pub fn is_shutdown(&self) -> bool {
        self.inner.is_shutdown()
    }

    /// Quiesce all app clients for this database, including clients retained
    /// by a login completion, a previous account switch or an active call.
    /// The opening barrier and generation also cancel opens queued earlier.
    pub fn shutdown_database(db_path: &Path) -> Result<(), CoreCallFailure> {
        let lifecycle = database_lifecycle(db_path);
        let _guard = lifecycle
            .opening
            .lock()
            .map_err(|_| CoreCallFailure::Runtime)?;
        let clients: Vec<_> = lifecycle
            .clients
            .lock()
            .map_err(|_| CoreCallFailure::Runtime)?
            .iter()
            .filter_map(Weak::upgrade)
            .collect();
        for client in clients {
            client.shutdown_local().map_err(CoreCallFailure::from)?;
        }
        Ok(())
    }

    /// Retry cleanup only for a durably tombstoned, closed local database.
    pub fn remove_local_database(
        db_path: &Path,
    ) -> Result<(), friendshub_core::db::pool::LocalDatabaseRemovalError> {
        let lifecycle = database_lifecycle(db_path);
        let _guard = lifecycle
            .opening
            .lock()
            .map_err(|_| friendshub_core::db::pool::LocalDatabaseRemovalError::LockUnavailable)?;
        Client::remove_local_database(db_path)
    }

    /// Exposes a cloned handle for app-owned typed request closures.
    ///
    /// Prefer [`Self::spawn_call`] for operations initiated from UI callbacks.
    pub fn client(&self) -> Arc<Client> {
        Arc::clone(&self.inner)
    }

    /// Run one synchronous core call on a named worker thread.
    ///
    /// The closure receives the core client and returns the result of one of
    /// its typed native methods (for example `auth_login`,
    /// `conversations_list`, or `messages_send_message`). Its completion runs
    /// on the worker thread; use `slint::invoke_from_event_loop` with a weak UI
    /// handle inside the completion before changing Slint properties.
    pub fn call<T, F, C>(
        &self,
        thread_name: &'static str,
        call: F,
        completion: C,
    ) -> io::Result<JoinHandle<()>>
    where
        T: Send + 'static,
        F: FnOnce(&Client) -> Result<T, CoreError> + Send + 'static,
        C: FnOnce(Result<T, CoreCallFailure>) + Send + 'static,
    {
        let client = Arc::clone(&self.inner);
        thread::Builder::new()
            .name(thread_name.to_owned())
            .spawn(move || {
                completion(call(&client).map_err(CoreCallFailure::from));
            })
    }

    /// Alias for [`Self::call`] when the call site benefits from an explicit
    /// indication that the operation is dispatched asynchronously.
    pub fn spawn_call<T, F, C>(
        &self,
        thread_name: &'static str,
        call: F,
        completion: C,
    ) -> io::Result<JoinHandle<()>>
    where
        T: Send + 'static,
        F: FnOnce(&Client) -> Result<T, CoreError> + Send + 'static,
        C: FnOnce(Result<T, CoreCallFailure>) + Send + 'static,
    {
        self.call(thread_name, call, completion)
    }

    /// Install core event forwarding on a worker thread.
    ///
    /// The callback is invoked from the core event worker. It must copy the
    /// owned event into the Slint event loop and must not access a Slint
    /// component directly. Durable event IDs are intentionally not
    /// acknowledged here: the app should call `ack_event` only after it has
    /// successfully applied the event to its account state.
    pub fn set_event_handler(
        &self,
        callback: impl Fn(Event) + Send + Sync + 'static,
    ) -> Result<(), CoreCallFailure> {
        self.inner
            .set_event_handler(Arc::new(CoreEventHandler::new(callback)))
            .map_err(CoreCallFailure::from)
    }

    /// Install event forwarding asynchronously so `set_event_handler` cannot
    /// block the Slint event loop while the core activates queued event push.
    pub fn set_event_handler_async<C>(
        &self,
        callback: impl Fn(Event) + Send + Sync + 'static,
        completion: C,
    ) -> io::Result<JoinHandle<()>>
    where
        C: FnOnce(Result<(), CoreCallFailure>) + Send + 'static,
    {
        self.call(
            "friendshub-core-events",
            move |client| client.set_event_handler(Arc::new(CoreEventHandler::new(callback))),
            completion,
        )
    }

    /// Acknowledge a durable event after the application has successfully
    /// applied it. The acknowledgement runs off the UI thread.
    pub fn ack_event_async<C>(&self, event_id: u64, completion: C) -> io::Result<JoinHandle<()>>
    where
        C: FnOnce(Result<(), CoreCallFailure>) + Send + 'static,
    {
        self.call(
            "friendshub-core-event-ack",
            move |client| client.ack_event(event_id),
            completion,
        )
    }

    /// Stop event forwarding and wait for callbacks already in progress.
    /// Run this from a worker thread because the core waits for its event
    /// callback thread to finish.
    pub fn clear_event_handler(&self) {
        self.inner.clear_event_handler();
    }
}
