//! UI runtime shared by feature-specific native controllers.

use std::io;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use friendshub_core::{Client, error::Error as CoreError};

use crate::MainWindow;
use crate::core::{CoreCallFailure, CoreClient};
use crate::platform::accounts::{AccountStore, LocalAccount};

pub mod auth;
pub mod chat;
pub mod community;
pub mod events;
pub mod media;
pub mod settings;

/// The authenticated account and its account-scoped core client.
#[derive(Clone)]
pub struct ActiveSession {
    pub account: LocalAccount,
    pub core: Arc<CoreClient>,
}

/// Shared handles for native screen controllers.
///
/// Slint properties are only touched on the event-loop thread. Core work runs
/// on a worker via [`Self::call`] and its result returns through
/// [`Self::apply_ui`].
#[derive(Clone)]
pub struct AppContext {
    pub ui: slint::Weak<MainWindow>,
    pub data_dir: PathBuf,
    pub accounts: AccountStore,
    active: Arc<Mutex<Option<ActiveSession>>>,
    pending_route: Arc<Mutex<Option<String>>>,
    event_generation: Arc<AtomicU64>,
}

impl AppContext {
    pub fn new(
        ui: slint::Weak<MainWindow>,
        data_dir: PathBuf,
        accounts: AccountStore,
        active: Option<ActiveSession>,
    ) -> Self {
        let context = Self {
            ui,
            data_dir,
            accounts,
            active: Arc::new(Mutex::new(active)),
            pending_route: Arc::new(Mutex::new(None)),
            event_generation: Arc::new(AtomicU64::new(0)),
        };
        if let Some(session) = context.active_session() {
            context.schedule_event_session_change(None, Some(session));
        }
        context
    }

    pub fn active_session(&self) -> Option<ActiveSession> {
        self.active.lock().ok()?.clone()
    }

    pub fn set_active_session(&self, session: Option<ActiveSession>) -> Result<(), &'static str> {
        let previous = {
            let mut active = self
                .active
                .lock()
                .map_err(|_| "active session lock poisoned")?;
            std::mem::replace(&mut *active, session.clone())
        };
        if !same_event_session(previous.as_ref(), session.as_ref()) {
            self.schedule_event_session_change(previous, session);
        }
        Ok(())
    }

    fn schedule_event_session_change(
        &self,
        previous: Option<ActiveSession>,
        next: Option<ActiveSession>,
    ) {
        let generation = self.event_generation.fetch_add(1, Ordering::SeqCst) + 1;
        events::replace_session(self.clone(), previous, next, generation);
    }

    pub fn take_pending_route(&self) -> Option<String> {
        self.pending_route.lock().ok()?.take()
    }

    /// Schedule an owned update on Slint's event loop.
    pub fn apply_ui(&self, update: impl FnOnce(MainWindow) + Send + 'static) {
        let weak = self.ui.clone();
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(window) = weak.upgrade() {
                update(window);
            }
        });
    }

    /// Run one typed operation against the active account's core client.
    ///
    /// `completion` is invoked on Slint's event loop. It must not block or run
    /// another synchronous core operation there.
    pub fn call<T, F, C>(
        &self,
        thread_name: &'static str,
        call: F,
        completion: C,
    ) -> io::Result<JoinHandle<()>>
    where
        T: Send + 'static,
        F: FnOnce(&Client) -> Result<T, CoreError> + Send + 'static,
        C: FnOnce(MainWindow, Result<T, CoreCallFailure>) + Send + 'static,
    {
        let session = self
            .active_session()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotConnected, "no active account"))?;
        call_client(session.core, self.ui.clone(), thread_name, call, completion)
    }
}

fn same_event_session(previous: Option<&ActiveSession>, next: Option<&ActiveSession>) -> bool {
    match (previous, next) {
        (None, None) => true,
        (Some(previous), Some(next)) => {
            previous.account.id == next.account.id && Arc::ptr_eq(&previous.core, &next.core)
        }
        _ => false,
    }
}

/// Navigate to a normalized app route, apply its top-level Slint state, and
/// ask the feature controller to load data for the destination.
pub fn navigate(ctx: &AppContext, requested_path: &str) {
    let mut route = normalize_route(requested_path);
    let authenticated = ctx.active_session().is_some();
    if !authenticated && is_protected_route(&route) {
        if let Ok(mut pending) = ctx.pending_route.lock() {
            *pending = Some(route);
        }
        route = "/login".to_owned();
    }

    let screen = screen_for_route(&route);
    let conversation = conversation_from_route(&route);
    let invite_token = route
        .split('?')
        .next()
        .unwrap_or(&route)
        .strip_prefix("/invite/")
        .map(str::to_owned)
        .unwrap_or_default();
    let route_for_ui = route.clone();
    let screen_for_ui = screen.clone();
    let is_login_screen = screen == "login";
    let is_register_screen = screen == "register";
    ctx.apply_ui(move |window| {
        window.set_route(route_for_ui.into());
        window.set_screen(screen_for_ui.into());
        window.set_authenticated(authenticated);
        window.set_invite_token(invite_token.into());
        if let Some((id, kind)) = conversation {
            window.set_selected_conversation_id(id.into());
            window.set_conversation_kind(kind.into());
        }
        if is_login_screen {
            window.set_login_error("".into());
            window.set_login_busy(false);
        }
        if is_register_screen {
            window.set_register_error("".into());
            window.set_register_busy(false);
        }
    });

    if authenticated {
        match screen.as_str() {
            "chat_index" | "chat" => chat::load(ctx, &route),
            "channel"
            | "conversation_settings"
            | "discover"
            | "dating"
            | "create_group"
            | "create_channel"
            | "invite" => community::load(ctx, &route),
            "profile"
            | "user_profile"
            | "settings_index"
            | "settings_account"
            | "settings_appearance"
            | "settings_privacy"
            | "settings_security"
            | "settings_twofactor"
            | "settings_notifications"
            | "settings_accounts"
            | "settings_bots"
            | "settings_about" => settings::load(ctx, &route),
            "calls" | "bot" => media::load(ctx, &screen),
            _ => {}
        }
    }
}

fn normalize_route(path: &str) -> String {
    let (path_only, query) = match path.split_once('?') {
        Some((path_only, query)) => (path_only, Some(query)),
        None => (path, None),
    };

    let normalized = match path_only {
        "dating" => "/app/dating".to_owned(),
        "calls" => "/app/calls".to_owned(),
        "settings" => "/app/settings".to_owned(),
        "create-group" => "/app/create/group".to_owned(),
        "create-channel" => "/app/create/channel".to_owned(),
        "switch-account" => "/app/settings/accounts".to_owned(),
        "/app/create-group" => "/app/create/group".to_owned(),
        "/app/create-channel" => "/app/create/channel".to_owned(),
        "/app/2fa" => "/app/settings/security/2fa".to_owned(),
        "/2fa" => "/login/2fa".to_owned(),
        _ if path_only.starts_with("/join/") => {
            format!("/invite/{}", &path_only["/join/".len()..])
        }
        _ if path_only.starts_with('/') => path_only.to_owned(),
        _ => "/not-found".to_owned(),
    };

    match query {
        Some(query) => format!("{normalized}?{query}"),
        None => normalized,
    }
}

fn screen_for_route(route: &str) -> String {
    let route = route.split('?').next().unwrap_or(route);
    match route {
        "/" => "landing".to_owned(),
        "/login" => "login".to_owned(),
        "/register" => "register".to_owned(),
        "/login/2fa" | "/two-factor" => "two_factor".to_owned(),
        "/connect" => "connect".to_owned(),
        "/app" => "chat_index".to_owned(),
        "/app/profile" => "profile".to_owned(),
        "/app/discover" => "discover".to_owned(),
        "/app/dating" => "dating".to_owned(),
        "/app/calls" => "calls".to_owned(),
        "/app/create/group" => "create_group".to_owned(),
        "/app/create/channel" => "create_channel".to_owned(),
        "/app/settings" => "settings_index".to_owned(),
        "/app/settings/account" => "settings_account".to_owned(),
        "/app/settings/appearance" => "settings_appearance".to_owned(),
        "/app/settings/privacy" => "settings_privacy".to_owned(),
        "/app/settings/security" => "settings_security".to_owned(),
        "/app/settings/security/2fa" => "settings_twofactor".to_owned(),
        "/app/settings/notifications" => "settings_notifications".to_owned(),
        "/app/settings/accounts" => "settings_accounts".to_owned(),
        "/app/settings/bots" => "settings_bots".to_owned(),
        "/app/settings/about" => "settings_about".to_owned(),
        "/not-found" => "not_found".to_owned(),
        _ if route.starts_with("/invite/") => "invite".to_owned(),
        _ if route.starts_with("/app/chat/") && route.ends_with("/settings") => {
            "conversation_settings".to_owned()
        }
        _ if route.starts_with("/app/chat/") => "chat".to_owned(),
        _ if route.starts_with("/app/channel/") && route.ends_with("/settings") => {
            "conversation_settings".to_owned()
        }
        _ if route.starts_with("/app/channel/") => "channel".to_owned(),
        _ if route.starts_with("/app/bot/") => "bot".to_owned(),
        _ if route.starts_with("/app/user/") => "user_profile".to_owned(),
        _ => "not_found".to_owned(),
    }
}

fn is_protected_route(route: &str) -> bool {
    !matches!(
        screen_for_route(route).as_str(),
        "landing" | "login" | "register" | "two_factor" | "connect" | "invite"
    )
}

fn conversation_from_route(route: &str) -> Option<(String, String)> {
    let path = route.split('?').next().unwrap_or(route);
    let (kind, remainder) = if let Some(rest) = path.strip_prefix("/app/chat/") {
        ("direct", rest)
    } else if let Some(rest) = path.strip_prefix("/app/channel/") {
        ("channel", rest)
    } else {
        return None;
    };
    let id = remainder.strip_suffix("/settings").unwrap_or(remainder);
    let id = id.split('/').next().unwrap_or_default();
    (!id.is_empty()).then(|| (id.to_owned(), kind.to_owned()))
}

/// Run a typed core operation and return its completion to the Slint loop.
pub fn call_client<T, F, C>(
    core: Arc<CoreClient>,
    ui: slint::Weak<MainWindow>,
    thread_name: &'static str,
    call: F,
    completion: C,
) -> io::Result<JoinHandle<()>>
where
    T: Send + 'static,
    F: FnOnce(&Client) -> Result<T, CoreError> + Send + 'static,
    C: FnOnce(MainWindow, Result<T, CoreCallFailure>) + Send + 'static,
{
    core.call(thread_name, call, move |result| {
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(window) = ui.upgrade() {
                completion(window, result);
            }
        });
    })
}

/// Open an account-scoped core client on a worker and return it to Slint.
pub fn open_client(
    ui: slint::Weak<MainWindow>,
    db_path: PathBuf,
    completion: impl FnOnce(MainWindow, Result<CoreClient, CoreCallFailure>) + Send + 'static,
) -> io::Result<JoinHandle<()>> {
    CoreClient::open_async(db_path, move |result| {
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(window) = ui.upgrade() {
                completion(window, result);
            }
        });
    })
}
