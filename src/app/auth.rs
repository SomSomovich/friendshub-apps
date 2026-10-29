//! Native authentication and first-device bootstrap.
//!
//! Passwords and short-lived 2FA challenges live only in this controller's
//! process memory. `friendshub-core` owns session, identity, and prekey state in
//! the account database.

use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use friendshub_core::{Client, error::Error as CoreError};

use crate::MainWindow;
use crate::core::{CoreCallFailure, CoreClient};
use crate::features::auth::{self, LoginResult, MeResponse};
use crate::platform::accounts::{AccountManifest, AccountStore, LocalAccount};

use super::{ActiveSession, AppContext, call_client, open_client};

const DEFAULT_DEVICE_NUMBER: i64 = 1;
const DEVICE_NAME: &str = "FriendsHub Desktop";
const REGISTRATION_DATABASE_KEY: &str = "native-auth-registration";

/// UI flow used to keep the right form busy and route errors to its field.
#[derive(Clone, Copy, Eq, PartialEq)]
enum Operation {
    Login,
    Register,
    TwoFactor,
    Logout,
    Bootstrap,
}

#[derive(Clone)]
struct Credentials {
    fh_number: String,
    password: String,
}

struct PendingChallenge {
    fh_number: String,
    challenge_token: String,
    device_number: i64,
    expires_at: Option<Instant>,
    device_setup: bool,
    db_key: String,
    core: Arc<CoreClient>,
}

#[derive(Default)]
struct ControllerState {
    operation: Option<Operation>,
    credentials: Option<Credentials>,
    challenge: Option<PendingChallenge>,
}

struct ControllerInner {
    ctx: AppContext,
    state: Mutex<ControllerState>,
}

/// Authentication callbacks for the native Slint window.
///
/// Clones share only short-lived in-memory authentication state, so one
/// controller can be moved into each Slint callback.
#[derive(Clone)]
pub struct AuthController {
    inner: Arc<ControllerInner>,
}

impl AuthController {
    pub fn new(ctx: AppContext) -> Self {
        Self {
            inner: Arc::new(ControllerInner {
                ctx,
                state: Mutex::new(ControllerState::default()),
            }),
        }
    }

    /// Start password login. Existing accounts keep their registered device
    /// number; a first login follows WEB's fallback of device number 1.
    pub fn login(&self, fh_number: String, password: String) {
        let fh_number = normalize_fh_number(&fh_number);
        if fh_number.is_empty() || password.is_empty() {
            self.show_error(Operation::Login, "Введите FH-номер и пароль.");
            return;
        }
        if !self.begin(
            Operation::Login,
            Some(Credentials {
                fh_number: fh_number.clone(),
                password: password.clone(),
            }),
        ) {
            return;
        }

        let binding = match login_binding(&self.inner.ctx.accounts, &fh_number) {
            Ok(binding) => binding,
            Err(message) => {
                self.fail(Operation::Login, message, true);
                return;
            }
        };
        self.open_account_client(
            Operation::Login,
            fh_number,
            password,
            binding.device_number,
            binding.db_key,
        );
    }

    /// Register an account and immediately begin the WEB-compatible login flow.
    pub fn register(&self, password: String) {
        if password.is_empty() {
            self.show_error(Operation::Register, "Введите пароль.");
            return;
        }
        if !self.begin(
            Operation::Register,
            Some(Credentials {
                fh_number: String::new(),
                password: password.clone(),
            }),
        ) {
            return;
        }

        let ctx = self.inner.ctx.clone();
        let controller = self.clone();
        let database_path = CoreClient::database_path(&ctx.data_dir, REGISTRATION_DATABASE_KEY);
        if let Err(error) = open_client(ctx.ui.clone(), database_path, move |_window, opened| {
            let core = match opened {
                Ok(core) => Arc::new(core),
                Err(error) => {
                    controller.fail(Operation::Register, error.user_message(), true);
                    return;
                }
            };

            let call_core = Arc::clone(&core);
            let completion_controller = controller.clone();
            let call_password = password.clone();
            let password_for_login = password.clone();
            if let Err(error) = call_client(
                call_core,
                ctx.ui.clone(),
                "friendshub-auth-register",
                move |client| auth::register(client, &call_password),
                move |_window, result| match result {
                    Ok(registered) => {
                        let fh_number = normalize_fh_number(&registered.fh_number);
                        if fh_number.is_empty() {
                            completion_controller.fail(
                                Operation::Register,
                                "Сервер вернул некорректный FH-номер.",
                                true,
                            );
                            return;
                        }
                        completion_controller.set_credential_fh_number(fh_number.clone());
                        completion_controller.inner.ctx.apply_ui(move |window| {
                            window.set_login_fh_number(fh_number.clone().into());
                            window.set_register_password("".into());
                        });
                        // Registration has no local account ID before the server
                        // response. The FH-derived alias is stable for later logins.
                        completion_controller.open_account_client(
                            Operation::Register,
                            normalize_fh_number(&registered.fh_number),
                            password_for_login,
                            DEFAULT_DEVICE_NUMBER,
                            database_key_for_fh(&registered.fh_number),
                        );
                    }
                    Err(error) => {
                        completion_controller.fail(Operation::Register, error.user_message(), true);
                    }
                },
            ) {
                controller.fail(
                    Operation::Register,
                    &format!("Не удалось запустить регистрацию ({})", error.kind()),
                    true,
                );
            }
        }) {
            self.fail(
                Operation::Register,
                &format!("Не удалось запустить регистрацию ({})", error.kind()),
                true,
            );
        }
    }

    /// Submit the current TOTP or backup code. The challenge is bound to the
    /// FH number, database, and exact device number that started the login.
    pub fn submit_twofactor(&self, code: String, backup_mode: bool) {
        let trimmed_code = code.trim().to_owned();
        let valid = if backup_mode {
            (6..=14).contains(&trimmed_code.len())
                && trimmed_code
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        } else {
            trimmed_code.len() == 6 && trimmed_code.bytes().all(|byte| byte.is_ascii_digit())
        };
        if !valid {
            self.inner.ctx.apply_ui(move |window| {
                window.set_twofactor_error(
                    if backup_mode {
                        "Введите резервный код длиной от 6 до 14 символов."
                    } else {
                        "Введите шестизначный код из приложения-аутентификатора."
                    }
                    .into(),
                );
            });
            return;
        }
        if !self.begin(Operation::TwoFactor, None) {
            return;
        }

        let pending = {
            let mut state = self.lock_state();
            let expired = state.challenge.as_ref().is_some_and(|challenge| {
                challenge.expires_at.is_some_and(|at| at <= Instant::now())
            });
            if expired {
                state.challenge = None;
                state.credentials = None;
                state.operation = None;
                None
            } else {
                state.challenge.as_ref().map(|challenge| PendingChallenge {
                    fh_number: challenge.fh_number.clone(),
                    challenge_token: challenge.challenge_token.clone(),
                    device_number: challenge.device_number,
                    expires_at: challenge.expires_at,
                    device_setup: challenge.device_setup,
                    db_key: challenge.db_key.clone(),
                    core: Arc::clone(&challenge.core),
                })
            }
        };
        let Some(pending) = pending else {
            self.expired_challenge();
            return;
        };

        let credentials = self.credentials_snapshot();
        let worker_credentials = credentials.clone();
        let call_core = Arc::clone(&pending.core);
        let completion_core = Arc::clone(&pending.core);
        let controller = self.clone();
        let store = self.inner.ctx.accounts.clone();
        let fh_number = pending.fh_number.clone();
        let db_key = pending.db_key.clone();
        let device_number = pending.device_number;
        let device_setup = pending.device_setup;
        let token = pending.challenge_token.clone();
        let code_for_call = trimmed_code.clone();

        if let Err(error) = call_client(
            call_core,
            self.inner.ctx.ui.clone(),
            "friendshub-auth-twofactor",
            move |client| match auth::login_2fa(client, &token, &code_for_call, device_number)? {
                LoginResult::Session {
                    expires_at,
                    account,
                } => complete_authenticated_session(
                    client,
                    &store,
                    &db_key,
                    &fh_number,
                    expires_at,
                    account,
                    device_number,
                    worker_credentials
                        .as_ref()
                        .map(|value| value.password.as_str()),
                    0,
                ),
                LoginResult::TotpRequired { .. } => Ok(AuthWorkResult::UnexpectedChallenge),
            },
            move |_window, result| {
                controller.finish_twofactor(
                    result,
                    completion_core,
                    device_setup,
                    pending.fh_number,
                    pending.device_number,
                    pending.db_key,
                    credentials,
                );
            },
        ) {
            self.fail(
                Operation::TwoFactor,
                &format!("Не удалось запустить проверку ({})", error.kind()),
                false,
            );
        }
    }

    /// Clear the in-memory password and challenge when the user leaves 2FA.
    /// Root may call this from the screen's `cancel-requested` callback.
    pub fn cancel_twofactor(&self) {
        let cancelled = {
            let mut state = self.lock_state();
            if state.operation.is_some() {
                false
            } else {
                state.challenge = None;
                state.credentials = None;
                true
            }
        };
        if cancelled {
            super::navigate(&self.inner.ctx, "/login");
        }
    }

    /// Reconcile a restored session's device and prekey state, then open `/app`.
    /// Without an active core session this returns to the landing screen.
    pub fn bootstrap(&self) {
        if !self.begin(Operation::Bootstrap, None) {
            return;
        }
        if let Some(session) = self.inner.ctx.active_session() {
            self.run_restored_bootstrap(session.core, session.account, true);
            return;
        }

        let manifest = match self.inner.ctx.accounts.load() {
            Ok(manifest) => manifest,
            Err(_) => {
                self.fail(
                    Operation::Bootstrap,
                    "Не удалось прочитать список аккаунтов.",
                    false,
                );
                super::navigate(&self.inner.ctx, "/login");
                return;
            }
        };
        let Some(account) = manifest.active_account().cloned() else {
            self.clear_operation();
            let route = if self
                .inner
                .ctx
                .accounts
                .pending_removals()
                .is_ok_and(|accounts| !accounts.is_empty())
            {
                "/app/settings/accounts"
            } else {
                "/"
            };
            super::navigate(&self.inner.ctx, route);
            return;
        };

        let path = CoreClient::database_path(&self.inner.ctx.data_dir, &account.db_key);
        let controller = self.clone();
        if let Err(error) =
            open_client(
                self.inner.ctx.ui.clone(),
                path,
                move |_window, opened| match opened {
                    Ok(core) => controller.run_restored_bootstrap(Arc::new(core), account, false),
                    Err(error) => {
                        controller.fail(Operation::Bootstrap, error.user_message(), false);
                        super::navigate(&controller.inner.ctx, "/login");
                    }
                },
            )
        {
            self.fail(
                Operation::Bootstrap,
                &format!("Не удалось открыть хранилище ({})", error.kind()),
                false,
            );
            super::navigate(&self.inner.ctx, "/login");
        }
    }

    /// Revoke the active server session and return to the login screen. Local
    /// account metadata and cryptographic state remain available for sign-in.
    pub fn logout(&self) {
        if !self.begin(Operation::Logout, None) {
            return;
        }
        let Some(session) = self.inner.ctx.active_session() else {
            self.fail(Operation::Logout, "Нет активной сессии.", false);
            return;
        };
        let controller = self.clone();
        if let Err(error) = call_client(
            Arc::clone(&session.core),
            self.inner.ctx.ui.clone(),
            "friendshub-auth-logout",
            auth::logout,
            move |window, result| {
                if !controller.inner.ctx.active_session().is_some_and(|active| {
                    active.account.id == session.account.id
                        && Arc::ptr_eq(&active.core, &session.core)
                }) {
                    controller.clear_operation();
                    return;
                }
                match result {
                    Ok(response) if response.ok => {
                        let clear_result = controller.inner.ctx.accounts.clear_active();
                        let active_result = controller.inner.ctx.set_active_session(None);
                        super::settings::clear_account_transients(&window);
                        controller.clear_transient_state();
                        controller.clear_operation();
                        if clear_result.is_err() || active_result.is_err() {
                            controller.show_error(
                                Operation::Logout,
                                "Сессия завершена, но локальный список аккаунтов не обновился.",
                            );
                        }
                        super::navigate(&controller.inner.ctx, "/login");
                    }
                    Ok(_) => controller.fail(
                        Operation::Logout,
                        "Не удалось завершить серверную сессию.",
                        false,
                    ),
                    Err(error) => controller.fail(Operation::Logout, error.user_message(), false),
                }
            },
        ) {
            self.fail(
                Operation::Logout,
                &format!("Не удалось запустить выход ({})", error.kind()),
                false,
            );
        }
    }

    fn open_account_client(
        &self,
        operation: Operation,
        fh_number: String,
        password: String,
        device_number: i64,
        db_key: String,
    ) {
        let ctx = self.inner.ctx.clone();
        let controller = self.clone();
        let path = CoreClient::database_path(&ctx.data_dir, &db_key);
        if let Err(error) = open_client(ctx.ui.clone(), path, move |_window, opened| {
            let core = match opened {
                Ok(core) => Arc::new(core),
                Err(error) => {
                    controller.fail(operation, error.user_message(), true);
                    return;
                }
            };
            controller.run_login_on_client(
                operation,
                core,
                fh_number,
                password,
                device_number,
                db_key,
            );
        }) {
            self.fail(
                operation,
                &format!("Не удалось открыть защищённое хранилище ({})", error.kind()),
                true,
            );
        }
    }

    fn run_login_on_client(
        &self,
        operation: Operation,
        core: Arc<CoreClient>,
        fh_number: String,
        password: String,
        device_number: i64,
        db_key: String,
    ) {
        let store = self.inner.ctx.accounts.clone();
        let call_core = Arc::clone(&core);
        let completion_core = Arc::clone(&core);
        let controller = self.clone();
        let worker_fh_number = fh_number.clone();
        let worker_db_key = db_key.clone();
        let worker_password = password.clone();
        if let Err(error) = call_client(
            call_core,
            self.inner.ctx.ui.clone(),
            "friendshub-auth-login",
            move |client| match auth::login(
                client,
                &worker_fh_number,
                &worker_password,
                device_number,
            )? {
                LoginResult::TotpRequired {
                    challenge_token,
                    expires_in_seconds,
                } => Ok(AuthWorkResult::Challenge {
                    fh_number: worker_fh_number,
                    challenge_token,
                    expires_in_seconds,
                    device_number,
                    device_setup: false,
                    db_key: worker_db_key,
                }),
                LoginResult::Session {
                    expires_at,
                    account,
                } => complete_authenticated_session(
                    client,
                    &store,
                    &worker_db_key,
                    &worker_fh_number,
                    expires_at,
                    account,
                    device_number,
                    Some(worker_password.as_str()),
                    0,
                ),
            },
            move |_window, result| {
                controller.finish_login_result(operation, result, completion_core);
            },
        ) {
            self.fail(
                operation,
                &format!("Не удалось запустить вход ({})", error.kind()),
                true,
            );
        }
    }

    fn finish_login_result(
        &self,
        operation: Operation,
        result: Result<AuthWorkResult, CoreCallFailure>,
        core: Arc<CoreClient>,
    ) {
        match result {
            Ok(AuthWorkResult::Challenge {
                fh_number,
                challenge_token,
                expires_in_seconds,
                device_number,
                device_setup,
                db_key,
            }) => self.enter_twofactor(
                operation,
                core,
                fh_number,
                challenge_token,
                expires_in_seconds,
                device_number,
                device_setup,
                db_key,
            ),
            Ok(AuthWorkResult::Ready(account)) => {
                self.activate_account(core, account);
            }
            Ok(AuthWorkResult::StorageFailure) => self.fail(
                operation,
                "Не удалось сохранить аккаунт в локальном списке.",
                true,
            ),
            Ok(AuthWorkResult::DeviceNumberChanged) => self.fail(
                operation,
                "Номер этого устройства изменился. Войдите ещё раз, чтобы завершить настройку.",
                true,
            ),
            Ok(AuthWorkResult::UnexpectedChallenge) => self.fail(
                operation,
                "Сервер вернул неожиданный ответ двухфакторной проверки.",
                true,
            ),
            Err(error) => self.fail(operation, error.user_message(), true),
        }
    }

    fn finish_twofactor(
        &self,
        result: Result<AuthWorkResult, CoreCallFailure>,
        core: Arc<CoreClient>,
        device_setup: bool,
        fh_number: String,
        device_number: i64,
        db_key: String,
        credentials: Option<Credentials>,
    ) {
        match result {
            Ok(AuthWorkResult::Ready(account)) => self.activate_account(core, account),
            Ok(AuthWorkResult::Challenge {
                fh_number,
                challenge_token,
                expires_in_seconds,
                device_number,
                device_setup,
                db_key,
            }) => self.enter_twofactor(
                Operation::TwoFactor,
                core,
                fh_number,
                challenge_token,
                expires_in_seconds,
                device_number,
                device_setup,
                db_key,
            ),
            Ok(AuthWorkResult::StorageFailure) => self.fail(
                Operation::TwoFactor,
                "Не удалось сохранить аккаунт в локальном списке.",
                true,
            ),
            Ok(AuthWorkResult::DeviceNumberChanged) => self.fail(
                Operation::TwoFactor,
                "Номер этого устройства изменился. Войдите ещё раз, чтобы завершить настройку.",
                true,
            ),
            Ok(AuthWorkResult::UnexpectedChallenge) => self.fail(
                Operation::TwoFactor,
                "Сервер вернул неожиданный ответ двухфакторной проверки.",
                true,
            ),
            Err(error) if should_refresh_challenge(error) => {
                self.refresh_challenge_after_failure(
                    core,
                    device_setup,
                    fh_number,
                    device_number,
                    db_key,
                    credentials,
                    error,
                );
            }
            Err(error) => {
                let message = match error {
                    CoreCallFailure::Server { status: 429 } => {
                        "Слишком много запросов. Повторите позже."
                    }
                    _ => error.user_message(),
                };
                self.fail_preserving_challenge(Operation::TwoFactor, message);
            }
        }
    }

    fn refresh_challenge_after_failure(
        &self,
        core: Arc<CoreClient>,
        device_setup: bool,
        fh_number: String,
        device_number: i64,
        db_key: String,
        credentials: Option<Credentials>,
        original_error: CoreCallFailure,
    ) {
        let Some(credentials) =
            credentials.filter(|credentials| credentials.fh_number == fh_number)
        else {
            self.fail(
                Operation::TwoFactor,
                "Проверка завершилась. Войдите снова, чтобы получить новый код.",
                true,
            );
            return;
        };

        let call_core = Arc::clone(&core);
        let completion_core = Arc::clone(&core);
        let store = self.inner.ctx.accounts.clone();
        let controller = self.clone();
        let worker_fh_number = fh_number.clone();
        let worker_db_key = db_key.clone();
        let password = credentials.password.clone();
        if let Err(error) = call_client(
            call_core,
            self.inner.ctx.ui.clone(),
            "friendshub-auth-refresh-challenge",
            move |client| match auth::login(client, &worker_fh_number, &password, device_number)? {
                LoginResult::TotpRequired {
                    challenge_token,
                    expires_in_seconds,
                } => Ok(AuthWorkResult::Challenge {
                    fh_number: worker_fh_number,
                    challenge_token,
                    expires_in_seconds,
                    device_number,
                    device_setup,
                    db_key: worker_db_key,
                }),
                LoginResult::Session {
                    expires_at,
                    account,
                } => complete_authenticated_session(
                    client,
                    &store,
                    &worker_db_key,
                    &worker_fh_number,
                    expires_at,
                    account,
                    device_number,
                    Some(password.as_str()),
                    0,
                ),
            },
            move |_window, result| match result {
                Ok(AuthWorkResult::Challenge {
                    fh_number,
                    challenge_token,
                    expires_in_seconds,
                    device_number,
                    device_setup,
                    db_key,
                }) => {
                    controller.enter_twofactor(
                        Operation::TwoFactor,
                        completion_core,
                        fh_number,
                        challenge_token,
                        expires_in_seconds,
                        device_number,
                        device_setup,
                        db_key,
                    );
                    controller.set_twofactor_error(match original_error {
                        CoreCallFailure::Network => "Связь прервалась. Введите новый код.",
                        _ => "Код неверен или истёк. Введите новый код.",
                    });
                }
                Ok(AuthWorkResult::Ready(account)) => {
                    controller.activate_account(completion_core, account);
                }
                Ok(_) => controller.fail(
                    Operation::TwoFactor,
                    "Не удалось обновить проверку входа.",
                    true,
                ),
                Err(error) => controller.fail(Operation::TwoFactor, error.user_message(), true),
            },
        ) {
            self.fail(
                Operation::TwoFactor,
                &format!("Не удалось обновить проверку ({})", error.kind()),
                true,
            );
        }
    }

    fn enter_twofactor(
        &self,
        operation: Operation,
        core: Arc<CoreClient>,
        fh_number: String,
        challenge_token: String,
        expires_in_seconds: u64,
        device_number: i64,
        device_setup: bool,
        db_key: String,
    ) {
        let expires_at = Instant::now().checked_add(Duration::from_secs(expires_in_seconds));
        {
            let mut state = self.lock_state();
            state.challenge = Some(PendingChallenge {
                fh_number: fh_number.clone(),
                challenge_token,
                device_number,
                expires_at,
                device_setup,
                db_key,
                core,
            });
            state.operation = None;
        }
        self.inner.ctx.apply_ui(move |window| {
            set_busy(&window, operation, false);
            window.set_login_fh_number(fh_number.clone().into());
            window.set_twofactor_code("".into());
            window.set_twofactor_backup_mode(false);
            window.set_twofactor_device_setup(device_setup);
            window.set_twofactor_error("".into());
        });
        super::navigate(&self.inner.ctx, "/login/2fa");
    }

    fn activate_account(&self, core: Arc<CoreClient>, account: LocalAccount) {
        let session = ActiveSession { account, core };
        if self
            .inner
            .ctx
            .set_active_session(Some(session.clone()))
            .is_err()
        {
            self.fail(
                Operation::Login,
                "Не удалось активировать аккаунт в приложении.",
                true,
            );
            return;
        }
        self.clear_transient_state();
        self.clear_operation();
        let username = session.account.username.clone();
        let fh_number = session.account.fh_number.clone();
        self.inner.ctx.apply_ui(move |window| {
            super::settings::clear_account_transients(&window);
            window.set_authenticated(true);
            window.set_account_username(username.into());
            window.set_account_fh_number(fh_number.into());
            window.set_login_password("".into());
            window.set_register_password("".into());
            window.set_twofactor_code("".into());
            window.set_login_error("".into());
            window.set_register_error("".into());
            window.set_twofactor_error("".into());
        });
        let route = self
            .inner
            .ctx
            .take_pending_route()
            .unwrap_or_else(|| "/app".to_owned());
        super::navigate(&self.inner.ctx, &route);
    }

    fn run_restored_bootstrap(
        &self,
        core: Arc<CoreClient>,
        account: LocalAccount,
        was_active: bool,
    ) {
        let completion_core = Arc::clone(&core);
        let store = self.inner.ctx.accounts.clone();
        let original_device_number = account.device_number;
        let completion_controller = self.clone();
        if let Err(error) = call_client(
            core,
            self.inner.ctx.ui.clone(),
            "friendshub-auth-bootstrap",
            move |client| {
                let profile = auth::me(client)?;
                let actual_device = bootstrap_core(client, original_device_number)?;
                let mut updated = local_account(profile, actual_device, &account.db_key);
                // The manifest and this database were selected by account id;
                // preserve its stable database alias while refreshing profile data.
                updated.db_key = account.db_key;
                if store_account(&store, &updated, true).is_err() {
                    return Ok(BootstrapWorkResult::StorageFailure);
                }
                if actual_device != original_device_number {
                    return Ok(BootstrapWorkResult::DeviceNumberChanged(updated));
                }
                Ok(BootstrapWorkResult::Ready(updated))
            },
            move |_window, result| {
                completion_controller.finish_restored_bootstrap(
                    completion_core,
                    result,
                    was_active,
                );
            },
        ) {
            self.fail(
                Operation::Bootstrap,
                &format!("Не удалось запустить инициализацию ({})", error.kind()),
                false,
            );
            if was_active {
                let route = self
                    .inner
                    .ctx
                    .take_pending_route()
                    .unwrap_or_else(|| "/app".to_owned());
                super::navigate(&self.inner.ctx, &route);
            } else {
                super::navigate(&self.inner.ctx, "/login");
            }
        }
    }

    fn finish_restored_bootstrap(
        &self,
        core: Arc<CoreClient>,
        result: Result<BootstrapWorkResult, CoreCallFailure>,
        was_active: bool,
    ) {
        match result {
            Ok(BootstrapWorkResult::Ready(account)) => {
                let session = ActiveSession {
                    account: account.clone(),
                    core,
                };
                if self.inner.ctx.set_active_session(Some(session)).is_err() {
                    self.fail(
                        Operation::Bootstrap,
                        "Не удалось восстановить активный аккаунт.",
                        false,
                    );
                    return;
                }
                self.clear_operation();
                self.inner.ctx.apply_ui(move |window| {
                    window.set_account_username(account.username.into());
                    window.set_account_fh_number(account.fh_number.into());
                });
                let route = self
                    .inner
                    .ctx
                    .take_pending_route()
                    .unwrap_or_else(|| "/app".to_owned());
                super::navigate(&self.inner.ctx, &route);
            }
            Ok(BootstrapWorkResult::DeviceNumberChanged(_account)) => {
                let _ = self.inner.ctx.accounts.clear_active();
                let _ = self.inner.ctx.set_active_session(None);
                self.fail(
                    Operation::Bootstrap,
                    "Номер устройства изменился. Повторно войдите в аккаунт, чтобы продолжить.",
                    false,
                );
                super::navigate(&self.inner.ctx, "/login");
            }
            Ok(BootstrapWorkResult::StorageFailure) => {
                self.fail(
                    Operation::Bootstrap,
                    "Не удалось сохранить настройки аккаунта.",
                    false,
                );
                if was_active {
                    let route = self
                        .inner
                        .ctx
                        .take_pending_route()
                        .unwrap_or_else(|| "/app".to_owned());
                    super::navigate(&self.inner.ctx, &route);
                } else {
                    super::navigate(&self.inner.ctx, "/login");
                }
            }
            Err(error)
                if matches!(
                    error,
                    CoreCallFailure::NotAuthenticated | CoreCallFailure::Server { status: 401 }
                ) =>
            {
                let _ = self.inner.ctx.accounts.clear_active();
                let _ = self.inner.ctx.set_active_session(None);
                self.fail(Operation::Bootstrap, error.user_message(), false);
                super::navigate(&self.inner.ctx, "/login");
            }
            Err(error) => {
                self.fail(Operation::Bootstrap, error.user_message(), false);
                // A restored session remains active when a network bootstrap
                // operation fails; the user can retry without signing in again.
                if was_active {
                    super::navigate(&self.inner.ctx, "/app");
                } else {
                    super::navigate(&self.inner.ctx, "/login");
                }
            }
        }
    }

    fn begin(&self, operation: Operation, credentials: Option<Credentials>) -> bool {
        let started = {
            let mut state = self.lock_state();
            if state.operation.is_some() {
                false
            } else {
                state.operation = Some(operation);
                if operation != Operation::TwoFactor {
                    state.challenge = None;
                }
                if credentials.is_some() {
                    state.credentials = credentials;
                }
                true
            }
        };
        if started {
            self.inner.ctx.apply_ui(move |window| {
                set_busy(&window, operation, true);
                set_error(&window, operation, "");
            });
        }
        started
    }

    fn fail(&self, operation: Operation, message: &str, clear_auth: bool) {
        if clear_auth {
            self.clear_transient_state();
        }
        self.clear_operation();
        self.show_error(operation, message);
    }

    fn fail_preserving_challenge(&self, operation: Operation, message: &str) {
        self.clear_operation();
        self.show_error(operation, message);
    }

    fn show_error(&self, operation: Operation, message: &str) {
        let message = message.to_owned();
        self.inner.ctx.apply_ui(move |window| {
            set_busy(&window, operation, false);
            set_error(&window, operation, &message);
        });
    }

    fn set_twofactor_error(&self, message: &str) {
        let message = message.to_owned();
        self.inner.ctx.apply_ui(move |window| {
            window.set_auth_busy(false);
            window.set_twofactor_code("".into());
            window.set_twofactor_error(message.into());
        });
    }

    fn expired_challenge(&self) {
        self.clear_transient_state();
        self.clear_operation();
        self.inner.ctx.apply_ui(|window| {
            window.set_auth_busy(false);
            window.set_twofactor_code("".into());
            window.set_twofactor_error("Проверка истекла. Войдите снова.".into());
        });
        super::navigate(&self.inner.ctx, "/login");
    }

    fn clear_operation(&self) {
        let operation = {
            let mut state = self.lock_state();
            state.operation.take()
        };
        if let Some(operation) = operation {
            self.inner
                .ctx
                .apply_ui(move |window| set_busy(&window, operation, false));
        }
    }

    fn clear_transient_state(&self) {
        let mut state = self.lock_state();
        state.credentials = None;
        state.challenge = None;
    }

    fn credentials_snapshot(&self) -> Option<Credentials> {
        let state = self.lock_state();
        state.credentials.as_ref().map(|credentials| Credentials {
            fh_number: credentials.fh_number.clone(),
            password: credentials.password.clone(),
        })
    }

    fn set_credential_fh_number(&self, fh_number: String) {
        if let Some(credentials) = self.lock_state().credentials.as_mut() {
            credentials.fh_number = fh_number;
        }
    }

    fn lock_state(&self) -> MutexGuard<'_, ControllerState> {
        self.inner
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

struct LoginBinding {
    db_key: String,
    device_number: i64,
}

enum AuthWorkResult {
    Challenge {
        fh_number: String,
        challenge_token: String,
        expires_in_seconds: u64,
        device_number: i64,
        device_setup: bool,
        db_key: String,
    },
    Ready(LocalAccount),
    StorageFailure,
    DeviceNumberChanged,
    UnexpectedChallenge,
}

enum BootstrapWorkResult {
    Ready(LocalAccount),
    DeviceNumberChanged(LocalAccount),
    StorageFailure,
}

fn normalize_fh_number(value: &str) -> String {
    value.trim().to_ascii_uppercase()
}

fn database_key_for_fh(fh_number: &str) -> String {
    format!("fh:{}", normalize_fh_number(fh_number))
}

fn login_binding(store: &AccountStore, fh_number: &str) -> Result<LoginBinding, &'static str> {
    let unavailable = "Не удалось прочитать список аккаунтов.";
    let manifest = store.load().map_err(|_| unavailable)?;
    if store
        .pending_removals()
        .map_err(|_| unavailable)?
        .iter()
        .any(|account| account.fh_number.eq_ignore_ascii_case(fh_number))
    {
        return Err(
            "Удаление локального аккаунта ещё не завершено. Откройте список аккаунтов и повторите очистку.",
        );
    }
    let existing = manifest
        .accounts
        .iter()
        .find(|account| account.fh_number.eq_ignore_ascii_case(fh_number));
    Ok(match existing {
        Some(account) => LoginBinding {
            db_key: account.db_key.clone(),
            device_number: account.device_number,
        },
        None => LoginBinding {
            db_key: database_key_for_fh(fh_number),
            // WEB uses 1 when this device has no saved routing number.
            device_number: DEFAULT_DEVICE_NUMBER,
        },
    })
}

fn complete_authenticated_session(
    client: &Client,
    store: &AccountStore,
    db_key: &str,
    fh_number: &str,
    _expires_at: i64,
    profile: MeResponse,
    requested_device_number: i64,
    password: Option<&str>,
    reauthentication_count: u8,
) -> Result<AuthWorkResult, CoreError> {
    let mut account = local_account(profile, requested_device_number, db_key);
    if store_account(store, &account, false).is_err() {
        return Ok(AuthWorkResult::StorageFailure);
    }

    let actual_device_number = bootstrap_core(client, requested_device_number)?;
    account.device_number = actual_device_number;
    if store_account(store, &account, false).is_err() {
        return Ok(AuthWorkResult::StorageFailure);
    }

    if actual_device_number != requested_device_number {
        let Some(password) = password.filter(|_| reauthentication_count == 0) else {
            return Ok(AuthWorkResult::DeviceNumberChanged);
        };
        return match auth::login(client, fh_number, password, actual_device_number)? {
            LoginResult::TotpRequired {
                challenge_token,
                expires_in_seconds,
            } => Ok(AuthWorkResult::Challenge {
                fh_number: fh_number.to_owned(),
                challenge_token,
                expires_in_seconds,
                device_number: actual_device_number,
                device_setup: true,
                db_key: db_key.to_owned(),
            }),
            LoginResult::Session {
                expires_at,
                account,
            } => complete_authenticated_session(
                client,
                store,
                db_key,
                fh_number,
                expires_at,
                account,
                actual_device_number,
                Some(password),
                reauthentication_count + 1,
            ),
        };
    }

    if store.set_active(&account.id).is_err() {
        return Ok(AuthWorkResult::StorageFailure);
    }
    Ok(AuthWorkResult::Ready(account))
}

fn bootstrap_core(client: &Client, current_device_number: i64) -> Result<i64, CoreError> {
    let status = auth::device_init_status(client)?;
    if status.initialized {
        auth::device_init_ensure_prekeys(client)?;
        return Ok(current_device_number);
    }
    let initialized = auth::device_init_initialize(client, DEVICE_NAME)?;
    Ok(initialized.device_number)
}

fn local_account(profile: MeResponse, device_number: i64, db_key: &str) -> LocalAccount {
    LocalAccount {
        id: profile.id,
        fh_number: profile.fh_number,
        user_id: profile.user_id,
        username: profile.username,
        avatar_url: profile.avatar_url,
        totp_enabled: profile.totp_enabled,
        device_number,
        db_key: db_key.to_owned(),
    }
}

fn store_account(store: &AccountStore, account: &LocalAccount, activate: bool) -> Result<(), ()> {
    let manifest: AccountManifest = store.load().map_err(|_| ())?;
    let existing = manifest.accounts.iter().find(|entry| {
        entry.id == account.id || entry.fh_number.eq_ignore_ascii_case(&account.fh_number)
    });
    if let Some(existing) = existing {
        if existing.id != account.id
            || !existing.fh_number.eq_ignore_ascii_case(&account.fh_number)
            || existing.db_key != account.db_key
        {
            return Err(());
        }
        store.update(account.clone()).map_err(|_| ())?;
    } else {
        store.add(account.clone()).map_err(|_| ())?;
    }
    if activate {
        store.set_active(&account.id).map_err(|_| ())?;
    }
    Ok(())
}

fn should_refresh_challenge(error: CoreCallFailure) -> bool {
    matches!(
        error,
        CoreCallFailure::Network | CoreCallFailure::Server { status: 401 }
    )
}

fn set_busy(window: &MainWindow, operation: Operation, busy: bool) {
    match operation {
        Operation::Login => window.set_login_busy(busy),
        Operation::Register => window.set_register_busy(busy),
        Operation::TwoFactor => window.set_auth_busy(busy),
        Operation::Logout => window.set_about_busy(busy),
        Operation::Bootstrap => {}
    }
}

fn set_error(window: &MainWindow, operation: Operation, message: &str) {
    let message = message.to_owned();
    match operation {
        Operation::Login => window.set_login_error(message.into()),
        Operation::Register => window.set_register_error(message.into()),
        Operation::TwoFactor => window.set_twofactor_error(message.into()),
        Operation::Logout => window.set_about_logout_error(message.into()),
        Operation::Bootstrap => window.set_login_error(message.into()),
    }
}
