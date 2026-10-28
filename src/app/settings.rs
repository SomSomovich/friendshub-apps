//! Native settings controller.
//!
//! Account-scoped settings use the typed core wrappers. Theme, language, and
//! notification sound preferences are app-local and stored under `data_dir`.
//! Account-scoped settings use typed core wrappers, while app-local interface
//! preferences use the local settings file. Slint state is updated only on its
//! event loop; core operations and account-store I/O run on workers.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::thread;
use std::time::{SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use friendshub_core::{Client, error::Error as CoreError};
use qrcode::{Color as QrColor, QrCode};
use serde::{Deserialize, Serialize};
use slint::{Color, ComponentHandle, Model, ModelRc, SharedString, VecModel};

use crate::app::{ActiveSession, AppContext, call_client};
use crate::core::{CoreCallFailure, CoreClient};
use crate::features::{auth, bots, conversations, discovery_dating, profile_settings as profile};
use crate::platform::accounts::AccountStore;
use crate::{
    MainWindow, OwnProfileField, PublicProfileField, SecurityDevice, SecurityEvent,
    SecuritySession, SettingsAccount, SettingsBot, SettingsContact, SettingsOption,
    SettingsPrivacyException, SettingsProfileField, Theme,
};

const PREFERENCES_FILE: &str = "settings.json";
const MAX_PREFERENCES_BYTES: u64 = 64 * 1024;
const MAX_USERNAME_CHARS: usize = 64;
const MAX_STATUS_TTL_SECONDS: i64 = 7 * 24 * 60 * 60;
const MAX_MANAGED_BOTS: usize = bots::MAX_MANAGED_BOTS;
const MAX_PROFILE_AVATAR_BYTES: usize = 5 * 1024 * 1024;
const MAX_PROFILE_AVATAR_BASE64_BYTES: usize = MAX_PROFILE_AVATAR_BYTES.div_ceil(3) * 4;
const MAX_OTPAUTH_URI_BYTES: usize = 2048;
const QR_QUIET_ZONE_MODULES: usize = 4;
const QR_MODULE_SCALE: usize = 4;
const WEB_REPOSITORY_URL: &str = "https://github.com/SomSomovich/friendshubWEB";
const CORE_REPOSITORY_URL: &str = "https://github.com/SomSomovich/friendshub-core";
const NATIVE_RELEASES_URL: &str = "https://github.com/SomSomovich/friendshub-apps/releases";
static PREFERENCES_LOCK: OnceLock<Mutex<()>> = OnceLock::new();
static USER_PROFILE_ID: OnceLock<Mutex<Option<String>>> = OnceLock::new();
static USER_PROFILE_ALIAS_GENERATION: AtomicU64 = AtomicU64::new(0);
static USER_PROFILE_LOAD_GENERATION: AtomicU64 = AtomicU64::new(0);
static USER_PROFILE_ALIAS_WRITE_GENERATION: AtomicU64 = AtomicU64::new(0);
static USER_PROFILE_ALIAS_WRITES_PENDING: AtomicU64 = AtomicU64::new(0);
static PRIVACY_LOAD_GENERATION: AtomicU64 = AtomicU64::new(0);
static TEMP_FILE_COUNTER: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default, rename_all = "camelCase")]
struct LocalPreferences {
    light_mode: bool,
    language: String,
    sound_enabled: bool,
    sound: String,
    has_custom_colors: bool,
    background_rgb: [u8; 3],
    button_rgb: [u8; 3],
    bubble_rgb: [u8; 3],
    text_rgb: [u8; 3],
}

impl Default for LocalPreferences {
    fn default() -> Self {
        Self {
            light_mode: false,
            language: "ru".to_owned(),
            sound_enabled: true,
            sound: "chime".to_owned(),
            has_custom_colors: false,
            background_rgb: [26, 26, 58],
            button_rgb: [252, 144, 3],
            bubble_rgb: [73, 57, 54],
            text_rgb: [244, 244, 251],
        }
    }
}

#[derive(Clone)]
enum PreferenceChange {
    Theme(bool),
    Language(String),
    SoundEnabled(bool),
    Sound(String),
    ResetColors,
    Colors {
        background: [u8; 3],
        button: [u8; 3],
        bubble: [u8; 3],
        text: [u8; 3],
        custom: bool,
    },
}

/// Load settings for the route name used by `MainWindow.screen`.
///
/// Load root properties for the settings screen identified by `route`.
/// Account settings are read from the active core client; app-local settings
/// are loaded from `data_dir`.
pub fn initialize(ctx: &AppContext) {
    load_preferences_into_window(ctx);
    if let Some(window) = ctx.ui.upgrade() {
        let ui = ctx.ui.clone();
        window.on_twofactor_copy_backup_codes_requested(move |_| {
            if let Some(window) = ui.upgrade() {
                window.set_twofactor_setup_error_text(
                    "Буфер обмена недоступен в нативном клиенте; сохраните коды вручную.".into(),
                );
            }
        });
    }
}

pub fn load(ctx: &AppContext, route: &str) {
    load_preferences_into_window(ctx);

    if let Some(id) = route
        .strip_prefix("/app/user/")
        .filter(|id| !id.trim().is_empty())
    {
        load_public_profile(ctx, id);
        return;
    }

    match settings_screen(route) {
        Some("profile") | Some("settings_account") | Some("settings_twofactor") => {
            load_own_profile(ctx)
        }
        Some("settings_security") => {
            load_own_profile(ctx);
            load_security(ctx);
        }
        Some("settings_privacy") => load_privacy(ctx),
        Some("settings_accounts") => load_accounts(ctx),
        Some("settings_bots") => load_bots(ctx),
        Some("settings_about") => load_about(ctx),
        _ => {}
    }
}

fn settings_screen(route: &str) -> Option<&'static str> {
    match route {
        "profile" | "/app/profile" => Some("profile"),
        "settings_account" | "account-settings" | "/app/settings/account" => {
            Some("settings_account")
        }
        "settings_security" | "/app/settings/security" => Some("settings_security"),
        "settings_privacy" | "/app/settings/privacy" => Some("settings_privacy"),
        "settings_twofactor" | "settings_two_factor" | "/app/settings/security/2fa" => {
            Some("settings_twofactor")
        }
        "settings_accounts" | "accounts" | "/app/settings/accounts" => Some("settings_accounts"),
        "settings_bots" | "/app/settings/bots" => Some("settings_bots"),
        "settings_about" | "/app/settings/about" => Some("settings_about"),
        "settings_appearance" | "/app/settings/appearance" => Some("settings_appearance"),
        "settings_notifications" | "/app/settings/notifications" => Some("settings_notifications"),
        _ => None,
    }
}

/// Handle a routed native UI action. Returns `true` when this controller
/// accepted the action (including when it scheduled asynchronous core work).
/// Returns `false` for actions that require a missing platform/UI adapter or
/// belong to another controller.
pub fn handle(
    ctx: &AppContext,
    namespace: &str,
    action: &str,
    arg1: &str,
    arg2: &str,
    arg3: &str,
) -> bool {
    match namespace {
        "profile" => handle_profile(ctx, action, arg1, arg2, arg3),
        "account-settings" => handle_account_settings(ctx, action, arg1, arg2, arg3),
        "appearance" => handle_appearance(ctx, action, arg1),
        "privacy" => handle_privacy(ctx, action, arg1, arg2),
        "security" => handle_security(ctx, action, arg1, arg2),
        "twofactor-setup" => handle_twofactor(ctx, action, arg1, arg2),
        "notifications" => handle_notifications(ctx, action, arg1),
        "accounts" => handle_accounts(ctx, action, arg1),
        "bots" => handle_bots(ctx, action, arg1, arg2),
        "about" => handle_about(ctx, action, arg1, arg2),
        _ => false,
    }
}

fn load_preferences_into_window(ctx: &AppContext) {
    match read_preferences(&ctx.data_dir) {
        Ok(preferences) => {
            ctx.apply_ui(move |window| apply_preferences(&window, &preferences));
        }
        Err(error) => report_local_failure("load app preferences", &error),
    }
}

fn apply_preferences(window: &MainWindow, preferences: &LocalPreferences) {
    window.set_light_mode(preferences.light_mode);
    window.set_language(preferences.language.clone().into());
    let theme = window.global::<Theme>();
    theme.set_light_mode(preferences.light_mode);
    theme.set_custom_colors_enabled(preferences.has_custom_colors);
    theme.set_custom_background(rgb_color(preferences.background_rgb));
    theme.set_custom_accent(rgb_color(preferences.button_rgb));
    theme.set_custom_bubble(rgb_color(preferences.bubble_rgb));
    theme.set_custom_foreground(rgb_color(preferences.text_rgb));
    theme.set_custom_accent_foreground(accent_foreground(preferences.button_rgb));
    window.set_appearance_has_custom_colors(preferences.has_custom_colors);
    set_rgb_inputs(window, preferences);
    apply_color_previews(window, preferences);
    window.set_notifications_sound_enabled(preferences.sound_enabled);
    window.set_notifications_sound_value(preferences.sound.clone().into());
    window.set_notifications_os_permission("unsupported".into());
    window.set_notifications_permission_request_supported(false);
    window
        .set_notifications_push_status("Push-подключение не настроено в нативном клиенте.".into());
    window.set_notifications_busy(false);
}

fn set_rgb_inputs(window: &MainWindow, preferences: &LocalPreferences) {
    window.set_appearance_background_r(preferences.background_rgb[0].to_string().into());
    window.set_appearance_background_g(preferences.background_rgb[1].to_string().into());
    window.set_appearance_background_b(preferences.background_rgb[2].to_string().into());
    window.set_appearance_button_r(preferences.button_rgb[0].to_string().into());
    window.set_appearance_button_g(preferences.button_rgb[1].to_string().into());
    window.set_appearance_button_b(preferences.button_rgb[2].to_string().into());
    window.set_appearance_bubble_r(preferences.bubble_rgb[0].to_string().into());
    window.set_appearance_bubble_g(preferences.bubble_rgb[1].to_string().into());
    window.set_appearance_bubble_b(preferences.bubble_rgb[2].to_string().into());
    window.set_appearance_text_r(preferences.text_rgb[0].to_string().into());
    window.set_appearance_text_g(preferences.text_rgb[1].to_string().into());
    window.set_appearance_text_b(preferences.text_rgb[2].to_string().into());
}

fn apply_color_previews(window: &MainWindow, preferences: &LocalPreferences) {
    window.set_appearance_background_preview(rgb_color(preferences.background_rgb));
    window.set_appearance_button_preview(rgb_color(preferences.button_rgb));
    window.set_appearance_bubble_preview(rgb_color(preferences.bubble_rgb));
    window.set_appearance_text_preview(rgb_color(preferences.text_rgb));
    window.set_appearance_contrast_score(contrast_score(
        preferences.background_rgb,
        preferences.text_rgb,
    ));
}

fn rgb_color(rgb: [u8; 3]) -> Color {
    Color::from_rgb_u8(rgb[0], rgb[1], rgb[2])
}

fn accent_foreground(rgb: [u8; 3]) -> Color {
    let brightness =
        (u32::from(rgb[0]) * 299 + u32::from(rgb[1]) * 587 + u32::from(rgb[2]) * 114) / 1000;
    if brightness >= 150 {
        Color::from_rgb_u8(20, 20, 28)
    } else {
        Color::from_rgb_u8(255, 255, 255)
    }
}

fn default_theme_colors(light_mode: bool) -> ([u8; 3], [u8; 3], [u8; 3], [u8; 3]) {
    if light_mode {
        ([255, 255, 255], [3, 11, 252], [204, 206, 244], [20, 20, 28])
    } else {
        ([26, 26, 58], [252, 144, 3], [73, 57, 54], [244, 244, 251])
    }
}

fn contrast_score(background: [u8; 3], text: [u8; 3]) -> i32 {
    background
        .iter()
        .zip(text.iter())
        .map(|(background, text)| i32::from(*background).abs_diff(i32::from(*text)) as i32)
        .sum::<i32>()
        .clamp(0, 765)
}

fn load_profile_avatar_bytes(client: &Client, account_id: &str) -> Option<Vec<u8>> {
    if account_id.trim().is_empty() {
        return None;
    }
    let avatar = discovery_dating::get_account_avatar(client, account_id).ok()?;
    if avatar.account_id != account_id
        || !avatar.content_type.eq_ignore_ascii_case("image/webp")
        || avatar.data_base64.len() > MAX_PROFILE_AVATAR_BASE64_BYTES
    {
        return None;
    }
    base64::engine::general_purpose::STANDARD
        .decode(avatar.data_base64)
        .ok()
        .filter(|bytes| !bytes.is_empty() && bytes.len() <= MAX_PROFILE_AVATAR_BYTES)
}

fn load_own_profile(ctx: &AppContext) {
    let Some(session) = ctx.active_session() else {
        return;
    };

    let accounts = ctx.accounts.clone();
    let context = ctx.clone();
    let account_id = session.account.id.clone();
    let expected_account_id = account_id.clone();
    let account_core = session.core.clone();
    let ui = ctx.ui.clone();
    let result = call_client(
        account_core,
        ui,
        "friendshub-settings-profile-load",
        move |client| {
            let current = profile::get_own_profile(client)?;
            let fields = profile::get_profile_fields(client)?;
            let avatar_bytes = current
                .avatar_url
                .as_deref()
                .filter(|url| !url.trim().is_empty())
                .and_then(|_| load_profile_avatar_bytes(client, &current.id));
            let persisted = refresh_local_account(&accounts, &current);
            Ok((current, fields, persisted, avatar_bytes))
        },
        move |window, result| match result {
            Ok((current, fields, persisted, avatar_bytes)) => {
                if !context
                    .active_session()
                    .is_some_and(|session| session.account.id == expected_account_id)
                {
                    return;
                }
                if let Err(error) = persisted {
                    report_account_failure("refresh profile metadata", &error);
                }
                let avatar_image = avatar_bytes
                    .as_deref()
                    .and_then(|bytes| slint::Image::load_from_data(bytes, Some("webp")).ok());
                let has_avatar = avatar_image.is_some();
                window.set_account_username(current.username.clone().into());
                window.set_account_fh_number(current.fh_number.clone().into());
                window.set_account_avatar(avatar_image.clone().unwrap_or_default());
                window.set_account_avatar_label(initials(&current.username).into());
                window.set_has_account_avatar(has_avatar);

                window.set_profile_status_text(
                    current
                        .custom_status_text
                        .clone()
                        .unwrap_or_default()
                        .into(),
                );
                window.set_profile_status_emoji(
                    current
                        .custom_status_emoji
                        .clone()
                        .unwrap_or_default()
                        .into(),
                );
                window.set_profile_has_avatar(has_avatar);
                window.set_profile_avatar_image(avatar_image.clone().unwrap_or_default());
                window.set_profile_avatar_initials(initials(&current.username).into());
                window.set_profile_fields(model(
                    fields
                        .fields
                        .iter()
                        .map(|field| OwnProfileField {
                            label: field.label.clone().into(),
                            value: field.value.clone().into(),
                        })
                        .collect(),
                ));

                window.set_settings_account_avatar_url(
                    current.avatar_url.clone().unwrap_or_default().into(),
                );
                window.set_settings_account_avatar_image(avatar_image.unwrap_or_default());
                window.set_settings_account_has_avatar(has_avatar);
                window.set_settings_account_status_text(
                    current
                        .custom_status_text
                        .clone()
                        .unwrap_or_default()
                        .into(),
                );
                window.set_settings_account_status_emoji(
                    current
                        .custom_status_emoji
                        .clone()
                        .unwrap_or_default()
                        .into(),
                );
                window.set_settings_account_status_ttl_seconds(status_ttl(
                    current.custom_status_expires_at,
                ));
                window.set_settings_account_status_custom_ttl_seconds(status_custom_ttl(
                    current.custom_status_expires_at,
                ));
                window.set_settings_account_profile_fields(model(
                    fields
                        .fields
                        .into_iter()
                        .map(|field| SettingsProfileField {
                            label: field.label.into(),
                            value: field.value.into(),
                        })
                        .collect(),
                ));
                window.set_settings_account_username_valid(true);
                window.set_settings_account_username_error(SharedString::default());
                window.set_settings_account_error_text(SharedString::default());
                window.set_settings_account_busy(false);

                window.set_security_totp_enabled(current.totp_enabled);
                window.set_twofactor_setup_enabled(current.totp_enabled);
                update_active_account_metadata(
                    &context,
                    &account_id,
                    &current.username,
                    current.avatar_url.clone(),
                    current.totp_enabled,
                );
            }
            Err(error) => report_core_failure("load own profile", error),
        },
    );
    if let Err(error) = result {
        report_dispatch_failure("load own profile", error);
    }
}

fn refresh_local_account(
    accounts: &AccountStore,
    current: &profile::OwnProfile,
) -> Result<(), crate::platform::accounts::AccountStoreError> {
    let manifest = accounts.load()?;
    let Some(mut account) = manifest
        .accounts
        .into_iter()
        .find(|account| account.id == current.id)
    else {
        return Ok(());
    };

    if account.user_id != current.user_id || account.fh_number != current.fh_number {
        return Ok(());
    }

    account.username.clone_from(&current.username);
    account.avatar_url.clone_from(&current.avatar_url);
    account.totp_enabled = current.totp_enabled;
    accounts.update(account).map(|_| ())
}

fn update_active_account_metadata(
    ctx: &AppContext,
    account_id: &str,
    username: &str,
    avatar_url: Option<String>,
    totp_enabled: bool,
) {
    let Some(mut active) = ctx.active_session() else {
        return;
    };
    if active.account.id != account_id {
        return;
    }
    active.account.username = username.to_owned();
    active.account.avatar_url = avatar_url;
    active.account.totp_enabled = totp_enabled;
    if let Err(error) = ctx.set_active_session(Some(active)) {
        eprintln!("[settings] refresh active account failed: {error}");
    }
}

fn model<T: Clone + 'static>(rows: Vec<T>) -> ModelRc<T> {
    ModelRc::new(VecModel::from(rows))
}

fn initials(name: &str) -> String {
    name.chars()
        .find(|character| !character.is_whitespace())
        .map(|character| character.to_uppercase().collect())
        .unwrap_or_else(|| "FH".to_owned())
}

fn status_ttl(expires_at: Option<i64>) -> i32 {
    let Some(expires_at) = expires_at else {
        return 0;
    };
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs().min(i64::MAX as u64) as i64)
        .unwrap_or(0);
    let remaining = expires_at.saturating_sub(now);
    if remaining <= 0 {
        return 0;
    }
    let remaining = remaining.clamp(60, MAX_STATUS_TTL_SECONDS);
    for preset in [3_600_i64, 8 * 3_600, MAX_STATUS_TTL_SECONDS] {
        if (remaining - preset).abs() <= 120 {
            return preset as i32;
        }
    }
    remaining as i32
}

fn status_custom_ttl(expires_at: Option<i64>) -> i32 {
    let Some(expires_at) = expires_at else {
        return 3_600;
    };
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs().min(i64::MAX as u64) as i64)
        .unwrap_or(0);
    expires_at
        .saturating_sub(now)
        .clamp(60, MAX_STATUS_TTL_SECONDS) as i32
}

fn load_accounts(ctx: &AppContext) {
    ctx.apply_ui(|window| {
        window.set_settings_accounts_busy(true);
        window.set_settings_accounts_error_text(SharedString::default());
    });
    let accounts = ctx.accounts.clone();
    let context = ctx.clone();
    let result = thread::Builder::new()
        .name("friendshub-settings-accounts-load".to_owned())
        .spawn(move || {
            let result = accounts.load();
            context.apply_ui(move |window| {
                match result {
                    Ok(manifest) => {
                        let active_id = manifest.active_id.unwrap_or_default();
                        let rows = manifest
                            .accounts
                            .into_iter()
                            .map(|account| SettingsAccount {
                                id: account.id.into(),
                                username: account.username.into(),
                                fh_number: account.fh_number.into(),
                                avatar: slint::Image::default(),
                                has_avatar: false,
                            })
                            .collect();
                        window.set_settings_accounts(model(rows));
                        window.set_settings_active_account_id(active_id.into());
                        window.set_settings_accounts_error_text(SharedString::default());
                    }
                    Err(error) => {
                        report_account_failure("load local accounts", &error);
                        window.set_settings_accounts_error_text(error.to_string().into());
                    }
                }
                window.set_settings_accounts_busy(false);
            });
        });
    if let Err(error) = result {
        report_dispatch_failure("load local accounts", error);
        ctx.apply_ui(|window| {
            window.set_settings_accounts_busy(false);
            window.set_settings_accounts_error_text(
                "Не удалось запустить загрузку списка аккаунтов.".into(),
            );
        });
    }
}

fn load_about(ctx: &AppContext) {
    ctx.apply_ui(|window| {
        window.set_about_app_version(env!("CARGO_PKG_VERSION").into());
        window.set_about_repository_url(WEB_REPOSITORY_URL.into());
        window.set_about_native_releases_url(NATIVE_RELEASES_URL.into());
        window.set_about_core_repository_url(CORE_REPOSITORY_URL.into());
        window.set_about_updater_supported(false);
        window.set_about_update_busy(false);
        window.set_about_busy(false);
        window.set_about_update_status(
            "Автоматическая проверка обновлений для нативного клиента не подключена.".into(),
        );
    });
}

fn load_privacy(ctx: &AppContext) {
    let load_generation = PRIVACY_LOAD_GENERATION
        .fetch_add(1, Ordering::Relaxed)
        .wrapping_add(1);
    let Some(session) = ctx.active_session() else {
        ctx.apply_ui(|window| window.set_privacy_busy(false));
        return;
    };
    let account_id = session.account.id.clone();
    let expected_core = session.core.clone();
    let completion_context = ctx.clone();
    ctx.apply_ui(|window| window.set_privacy_busy(true));
    dispatch_core_completion(
        ctx,
        "load privacy settings",
        |client| {
            Ok((
                profile::get_privacy(client),
                profile::get_invisible_mode(client),
                profile::get_allow_nudges(client),
                profile::get_dm_policy(client),
                profile::get_group_add_policy(client),
                profile::list_presence_exceptions(client),
                profile::list_profile_exceptions(client),
                profile::list_group_add_exceptions(client),
                profile::list_blocked_contacts(client),
                profile::list_contacts(client),
            ))
        },
        move |window, result| {
            if PRIVACY_LOAD_GENERATION.load(Ordering::Relaxed) != load_generation
                || !completion_context.active_session().is_some_and(|session| {
                    session.account.id == account_id
                        && std::sync::Arc::ptr_eq(&session.core, &expected_core)
                })
            {
                return;
            }
            window.set_privacy_busy(false);
            let (
                privacy,
                invisible,
                nudges,
                dm_policy,
                group_policy,
                presence,
                profile_exceptions,
                group_exceptions,
                blocked,
                contacts,
            ) = match result {
                Ok(results) => results,
                Err(error) => {
                    report_core_failure("load privacy settings", error);
                    return;
                }
            };

            apply_privacy_result(
                privacy,
                invisible,
                nudges,
                dm_policy,
                group_policy,
                presence,
                profile_exceptions,
                group_exceptions,
                blocked,
                contacts,
                &window,
            );
        },
    );
}

fn apply_privacy_result(
    privacy: Result<profile::PrivacySettings, CoreError>,
    invisible: Result<profile::InvisibleModeResponse, CoreError>,
    nudges: Result<profile::AllowNudgesResponse, CoreError>,
    dm_policy: Result<profile::PolicyResponse<profile::DirectMessagePolicy>, CoreError>,
    group_policy: Result<profile::PolicyResponse<profile::GroupAddPolicy>, CoreError>,
    presence: Result<Vec<profile::PresenceException>, CoreError>,
    profile_exceptions: Result<Vec<profile::ProfileException>, CoreError>,
    group_exceptions: Result<Vec<profile::GroupAddException>, CoreError>,
    blocked: Result<Vec<profile::BlockedContact>, CoreError>,
    contacts: Result<Vec<profile::ContactItem>, CoreError>,
    window: &MainWindow,
) {
    match privacy {
        Ok(settings) => {
            window.set_privacy_profile_hidden(settings.hide_profile);
            window.set_privacy_hidden_from_search(settings.hide_from_search);
        }
        Err(error) => report_core_failure("load privacy visibility", error.into()),
    }
    match invisible {
        Ok(settings) => window.set_privacy_invisible(settings.enabled),
        Err(error) => report_core_failure("load invisible mode", error.into()),
    }
    match nudges {
        Ok(settings) => window.set_privacy_allow_nudges(settings.allow),
        Err(error) => report_core_failure("load nudges preference", error.into()),
    }
    match dm_policy {
        Ok(settings) => window.set_privacy_dm_policy(dm_policy_name(settings.policy).into()),
        Err(error) => report_core_failure("load direct-message policy", error.into()),
    }
    match group_policy {
        Ok(settings) => {
            window.set_privacy_group_add_policy(group_policy_name(settings.policy).into())
        }
        Err(error) => report_core_failure("load group-add policy", error.into()),
    }
    match presence {
        Ok(items) => window.set_privacy_presence_exceptions(model(
            items
                .into_iter()
                .map(|item| SettingsPrivacyException {
                    id: item.target_account_id.clone().into(),
                    name: item.target_account_id.into(),
                    kind: match item.kind {
                        profile::PresenceExceptionKind::AlwaysVisible => "always_visible",
                        profile::PresenceExceptionKind::AlwaysInvisible => "always_invisible",
                    }
                    .into(),
                })
                .collect(),
        )),
        Err(error) => report_core_failure("load presence exceptions", error.into()),
    }
    match profile_exceptions {
        Ok(items) => window.set_privacy_profile_exceptions(model(
            items
                .into_iter()
                .map(|item| SettingsPrivacyException {
                    id: item.account_id.clone().into(),
                    name: item.account_id.into(),
                    kind: "profile".into(),
                })
                .collect(),
        )),
        Err(error) => report_core_failure("load profile exceptions", error.into()),
    }
    match group_exceptions {
        Ok(items) => window.set_privacy_group_add_exceptions(model(
            items
                .into_iter()
                .map(|item| SettingsPrivacyException {
                    id: item.account_id.clone().into(),
                    name: item.account_id.into(),
                    kind: "group_add".into(),
                })
                .collect(),
        )),
        Err(error) => report_core_failure("load group-add exceptions", error.into()),
    }
    match blocked {
        Ok(items) => window.set_privacy_blocked_accounts(model(
            items
                .into_iter()
                .map(|item| SettingsContact {
                    id: item.account_id.into(),
                    name: item.username.into(),
                })
                .collect(),
        )),
        Err(error) => report_core_failure("load blocked accounts", error.into()),
    }
    match contacts {
        Ok(items) => window.set_privacy_contacts(model(
            items
                .into_iter()
                .map(|item| SettingsContact {
                    id: item.target_account_id.into(),
                    name: if item.local_username.trim().is_empty() {
                        item.username.into()
                    } else {
                        item.local_username.into()
                    },
                })
                .collect(),
        )),
        Err(error) => {
            report_core_failure("load privacy contacts", error.into());
            window.set_privacy_contacts(model(Vec::new()));
        }
    }
}

fn dm_policy_name(policy: profile::DirectMessagePolicy) -> &'static str {
    match policy {
        profile::DirectMessagePolicy::Everyone => "everyone",
        profile::DirectMessagePolicy::ContactsOnly => "contacts_only",
    }
}

fn group_policy_name(policy: profile::GroupAddPolicy) -> &'static str {
    match policy {
        profile::GroupAddPolicy::Everyone => "everyone",
        profile::GroupAddPolicy::Nobody => "nobody",
    }
}

fn load_security(ctx: &AppContext) {
    let current_device_number = ctx
        .active_session()
        .map(|session| session.account.device_number)
        .unwrap_or_default();
    let (selected_source, selected_conversation) = ctx
        .ui
        .upgrade()
        .map(|window| {
            (
                window.get_security_history_source_device().to_string(),
                window.get_security_history_conversation_id().to_string(),
            )
        })
        .unwrap_or_default();
    ctx.apply_ui(|window| {
        window.set_security_busy(true);
        window.set_security_session_error(SharedString::default());
        window.set_security_device_error(SharedString::default());
        window.set_security_events_error(SharedString::default());
        window.set_security_history_error(
            "Перенос и загрузка истории пока не поддерживаются нативным клиентом.".into(),
        );
        window.set_security_history_progress(SharedString::default());
        window.set_security_history_page_result(SharedString::default());
        window.set_security_device_policy_hint(
            "Перенос истории с другого устройства пока недоступен в нативном клиенте.".into(),
        );
    });
    dispatch_core_completion(
        ctx,
        "load security settings",
        move |client| {
            Ok((
                profile::list_sessions(client),
                profile::list_devices(client),
                profile::list_security_events(client),
                conversations::list(client),
            ))
        },
        move |window, result| {
            window.set_security_busy(false);
            let (sessions, devices, events, conversations) = match result {
                Ok(results) => results,
                Err(error) => {
                    report_core_failure("load security settings", error);
                    return;
                }
            };

            match sessions {
                Ok(items) => window.set_security_sessions(model(
                    items
                        .into_iter()
                        .filter(|session| session.revoked_at.is_none())
                        .map(|session| SecuritySession {
                            id: session.id.into(),
                            ip_address: session.ip_address.unwrap_or_else(|| "—".to_owned()).into(),
                            user_agent: session.user_agent.unwrap_or_else(|| "—".to_owned()).into(),
                            signed_in_at: format_security_timestamp(session.created_at).into(),
                            is_current: session.is_current,
                        })
                        .collect(),
                )),
                Err(error) => {
                    report_core_failure("load sessions", error.into());
                    window.set_security_session_error("Не удалось загрузить список сессий.".into());
                }
            }

            let mut source_options = Vec::new();
            match devices {
                Ok(items) => {
                    let rows = items
                        .into_iter()
                        .map(|device| {
                            let is_current = device.device_number == current_device_number;
                            if !is_current {
                                source_options.push(SettingsOption {
                                    value: device.id.clone().into(),
                                    label: format!("{} · №{}", device.name, device.device_number)
                                        .into(),
                                });
                            }
                            SecurityDevice {
                                id: device.id.into(),
                                name: device.name.into(),
                                number: device.device_number.clamp(i32::MIN as i64, i32::MAX as i64)
                                    as i32,
                                added_at: format_security_timestamp(device.created_at).into(),
                                is_current,
                                can_revoke: true,
                            }
                        })
                        .collect();
                    window.set_security_devices(model(rows));
                    window.set_security_history_source_options(model(source_options.clone()));
                    let source = if source_options
                        .iter()
                        .any(|option| option.value == selected_source)
                    {
                        selected_source
                    } else {
                        source_options
                            .first()
                            .map(|option| option.value.to_string())
                            .unwrap_or_default()
                    };
                    window.set_security_history_source_device(source.into());
                }
                Err(error) => {
                    report_core_failure("load devices", error.into());
                    window
                        .set_security_device_error("Не удалось загрузить список устройств.".into());
                    window.set_security_history_source_options(model(Vec::new()));
                    window.set_security_history_source_device(SharedString::default());
                }
            }

            match events {
                Ok(items) => window.set_security_events(model(
                    items
                        .into_iter()
                        .map(|event| SecurityEvent {
                            id: event.id.into(),
                            event_type: event.event_type.into(),
                            created_at: format_security_timestamp(event.created_at).into(),
                            detail: [event.ip_address, event.user_agent, event.metadata]
                                .into_iter()
                                .flatten()
                                .filter(|value| !value.trim().is_empty())
                                .collect::<Vec<_>>()
                                .join(" · ")
                                .into(),
                        })
                        .collect(),
                )),
                Err(error) => {
                    report_core_failure("load security events", error.into());
                    window.set_security_events_error(
                        "Не удалось загрузить журнал безопасности.".into(),
                    );
                }
            }

            match conversations {
                Ok(items) => {
                    let options = items
                        .into_iter()
                        .map(|conversation| SettingsOption {
                            value: conversation.id.clone().into(),
                            label: conversation
                                .title
                                .unwrap_or_else(|| {
                                    format!("{} · {}", conversation.kind, conversation.id)
                                })
                                .into(),
                        })
                        .collect::<Vec<_>>();
                    let conversation_id = if options
                        .iter()
                        .any(|option| option.value == selected_conversation)
                    {
                        selected_conversation
                    } else {
                        options
                            .first()
                            .map(|option| option.value.to_string())
                            .unwrap_or_default()
                    };
                    window.set_security_history_conversation_options(model(options));
                    window.set_security_history_conversation_id(conversation_id.into());
                }
                Err(error) => {
                    report_core_failure("load security conversations", error.into());
                    window.set_security_history_error(
                        "Не удалось загрузить список бесед для истории.".into(),
                    );
                    window.set_security_history_conversation_options(model(Vec::new()));
                    window.set_security_history_conversation_id(SharedString::default());
                }
            }
        },
    );
}

fn format_security_timestamp(timestamp: i64) -> String {
    let days = timestamp.div_euclid(86_400);
    let seconds = timestamp.rem_euclid(86_400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let day_of_era = z - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let mut year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = month_prime + if month_prime < 10 { 3 } else { -9 };
    year += i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02} {:02}:{:02} UTC",
        seconds / 3_600,
        (seconds % 3_600) / 60
    )
}

fn load_bots(ctx: &AppContext) {
    ctx.apply_ui(|window| {
        window.set_settings_bots_loading(true);
        window.set_settings_bots_error_text(SharedString::default());
    });
    dispatch_core_completion(ctx, "load managed bots", bots::list, |window, result| {
        window.set_settings_bots_loading(false);
        match result {
            Ok(items) => {
                let count = items.len();
                window.set_settings_bots(model(
                    items
                        .into_iter()
                        .map(|bot| SettingsBot {
                            id: bot.id.into(),
                            handle: bot.handle.unwrap_or_default().into(),
                            token_prefix: bot.token_prefix.into(),
                        })
                        .collect(),
                ));
                window.set_settings_bots_can_create(count < MAX_MANAGED_BOTS);
                window.set_settings_bots_error_text(SharedString::default());
            }
            Err(error) => {
                report_core_failure("load managed bots", error);
                window.set_settings_bots_error_text(error.user_message().into());
            }
        }
    });
}

fn handle_profile(ctx: &AppContext, action: &str, arg1: &str, _arg2: &str, _arg3: &str) -> bool {
    match action {
        "load" => true,
        "open-public" if !arg1.trim().is_empty() => load_public_profile(ctx, arg1),
        "open-conversation" if !arg1.trim().is_empty() => open_conversation_profile(ctx, arg1),
        "close-modal" => {
            let deep_linked_profile = ctx
                .ui
                .upgrade()
                .is_some_and(|window| window.get_route().starts_with("/app/user/"));
            next_user_profile_alias_generation();
            next_user_profile_load_generation();
            set_user_profile_id(None);
            ctx.apply_ui(|window| {
                window.set_user_profile_open(false);
                window.set_user_profile_alias_editor_open(false);
                window.set_user_profile_alias_save_enabled(false);
                window.set_user_profile_error_text(SharedString::default());
            });
            if deep_linked_profile {
                crate::app::navigate(ctx, "/app");
                return true;
            }
            true
        }
        "block-toggle" => toggle_public_profile_block(ctx),
        "contact-toggle" => toggle_public_profile_contact(ctx),
        "alias-editor" => {
            let Some(window) = ctx.ui.upgrade() else {
                return false;
            };
            if window.get_user_profile_loading()
                || window.get_user_profile_busy()
                || window.get_user_profile_unavailable()
                || !window.get_user_profile_is_contact()
            {
                return true;
            }
            next_user_profile_alias_generation();
            ctx.apply_ui(|window| {
                let alias = window.get_user_profile_alias();
                let draft = if alias.trim().is_empty() {
                    window.get_user_profile_username()
                } else {
                    alias
                };
                let save_enabled =
                    !draft.trim().is_empty() && draft.chars().count() <= MAX_USERNAME_CHARS;
                window.set_user_profile_alias_draft(draft);
                window.set_user_profile_alias_save_enabled(save_enabled);
                window.set_user_profile_alias_editor_open(true);
                window.set_user_profile_error_text(SharedString::default());
            });
            true
        }
        "alias-draft-changed" => update_public_profile_alias_draft(ctx, arg1),
        "alias-save" => save_public_profile_alias(ctx, arg1),
        "alias-cancel" => {
            next_user_profile_alias_generation();
            ctx.apply_ui(|window| {
                window.set_user_profile_alias_editor_open(false);
                window.set_user_profile_alias_save_enabled(false);
            });
            true
        }
        // Avatar viewing/sharing require adapters that are not installed.
        "message" => open_profile_conversation(ctx),
        "share" | "avatar-open" => false,
        _ => false,
    }
}

fn open_conversation_profile(ctx: &AppContext, conversation_id: &str) -> bool {
    let Some(window) = ctx.ui.upgrade() else {
        return false;
    };
    if window.get_selected_conversation_id().as_str() != conversation_id
        || !matches!(window.get_screen().as_str(), "chat" | "channel")
    {
        return false;
    }
    let Some(peer_account_id) = crate::app::chat::peer_account_id(ctx, conversation_id) else {
        return false;
    };
    load_public_profile(ctx, &peer_account_id)
}

fn toggle_public_profile_contact(ctx: &AppContext) -> bool {
    let Some(window) = ctx.ui.upgrade() else {
        return false;
    };
    let Some(profile_id) = user_profile_id() else {
        return false;
    };
    if window.get_user_profile_busy()
        || window.get_user_profile_loading()
        || window.get_user_profile_unavailable()
    {
        return true;
    }

    let is_contact = window.get_user_profile_is_contact();
    let fh_number = window.get_user_profile_fh_number().to_string();
    let username = window.get_user_profile_username().to_string();
    if !is_contact
        && (fh_number.trim().is_empty()
            || username.trim().is_empty()
            || username.chars().count() > MAX_USERNAME_CHARS)
    {
        window.set_user_profile_error_text("Не удалось подготовить контакт.".into());
        return true;
    }

    let Some(session) = ctx.active_session() else {
        return false;
    };
    let account_id = session.account.id.clone();
    let completion_context = ctx.clone();
    let completion_profile_id = profile_id.clone();
    let completion_account_id = account_id.clone();
    let completion_peer_username = username.clone();
    window.set_user_profile_busy(true);
    window.set_user_profile_error_text(SharedString::default());

    let result = call_client(
        session.core,
        ctx.ui.clone(),
        "friendshub-settings-profile-contact-toggle",
        move |client| {
            if is_contact {
                profile::remove_contact(client, &profile::RemoveContactRequest { id: profile_id })
            } else {
                profile::add_contact(
                    client,
                    &profile::AddContactRequest {
                        target_fh_number: fh_number,
                        local_username: username,
                    },
                )
                .map(|_| ())
            }
        },
        move |window, result| {
            let current_account_matches = completion_context
                .active_session()
                .is_some_and(|session| session.account.id == completion_account_id);
            if !current_account_matches
                || user_profile_id().as_deref() != Some(completion_profile_id.as_str())
            {
                return;
            }

            window.set_user_profile_busy(false);
            match result {
                Ok(()) => {
                    if is_contact {
                        crate::app::chat::update_direct_chat_title(
                            &completion_context,
                            &completion_profile_id,
                            &completion_peer_username,
                        );
                    }
                    load_public_profile(&completion_context, &completion_profile_id);
                }
                Err(error) => {
                    window.set_user_profile_error_text(error.user_message().into());
                    report_core_failure("toggle profile contact", error);
                }
            }
        },
    );
    if let Err(error) = result {
        report_dispatch_failure("toggle profile contact", error);
        ctx.apply_ui(|window| {
            window.set_user_profile_busy(false);
            window.set_user_profile_error_text("Не удалось запустить операцию.".into());
        });
        false
    } else {
        true
    }
}

fn open_profile_conversation(ctx: &AppContext) -> bool {
    let Some(window) = ctx.ui.upgrade() else {
        return false;
    };
    let fh_number = window.get_user_profile_fh_number().to_string();
    if fh_number.trim().is_empty() {
        return false;
    }
    let context = ctx.clone();
    dispatch_core_completion(
        ctx,
        "open conversation with profile",
        move |client| conversations::create_direct(client, fh_number),
        move |_window, result| match result {
            Ok(conversation) => {
                crate::app::navigate(&context, &format!("/app/chat/{}", conversation.id))
            }
            Err(error) => report_core_failure("open conversation with profile", error),
        },
    )
}

fn update_public_profile_alias_draft(ctx: &AppContext, requested_alias: &str) -> bool {
    let Some(window) = ctx.ui.upgrade() else {
        return false;
    };
    if !window.get_user_profile_alias_editor_open()
        || window.get_user_profile_busy()
        || window.get_user_profile_loading()
        || window.get_user_profile_unavailable()
    {
        return true;
    }

    let draft = requested_alias
        .chars()
        .take(MAX_USERNAME_CHARS)
        .collect::<String>();
    let save_enabled = !draft.trim().is_empty();
    let alias_generation = USER_PROFILE_ALIAS_GENERATION.load(Ordering::Relaxed);
    ctx.apply_ui(move |window| {
        if USER_PROFILE_ALIAS_GENERATION.load(Ordering::Relaxed) == alias_generation
            && window.get_user_profile_alias_editor_open()
            && !window.get_user_profile_busy()
            && !window.get_user_profile_loading()
            && !window.get_user_profile_unavailable()
        {
            window.set_user_profile_alias_draft(draft.into());
            window.set_user_profile_alias_save_enabled(save_enabled);
            window.set_user_profile_error_text(SharedString::default());
        }
    });
    true
}

fn save_public_profile_alias(ctx: &AppContext, requested_alias: &str) -> bool {
    let Some(window) = ctx.ui.upgrade() else {
        return false;
    };
    let Some(profile_id) = user_profile_id() else {
        return false;
    };
    if window.get_user_profile_busy()
        || window.get_user_profile_loading()
        || window.get_user_profile_unavailable()
    {
        return true;
    }
    if !window.get_user_profile_is_contact() {
        window.set_user_profile_error_text("Сначала добавьте пользователя в контакты.".into());
        return true;
    }
    if !window.get_user_profile_alias_editor_open() {
        return true;
    }

    let local_username = requested_alias.trim();
    let username_length = local_username.chars().count();
    if username_length == 0 || username_length > MAX_USERNAME_CHARS {
        window
            .set_user_profile_error_text("Имя друга должно содержать от 1 до 64 символов.".into());
        return true;
    }
    let target_fh_number = window.get_user_profile_fh_number().to_string();
    if target_fh_number.trim().is_empty() {
        window.set_user_profile_error_text("Не удалось подготовить контакт.".into());
        return true;
    }
    let Some(session) = ctx.active_session() else {
        return false;
    };
    let account_id = session.account.id.clone();
    let completion_account_id = account_id.clone();
    let completion_profile_id = profile_id.clone();
    let completion_context = ctx.clone();
    let alias_generation = USER_PROFILE_ALIAS_GENERATION.load(Ordering::Relaxed);
    let alias_write_generation = next_user_profile_alias_write_generation();
    USER_PROFILE_ALIAS_WRITES_PENDING.fetch_add(1, Ordering::Relaxed);
    let request = profile::AddContactRequest {
        target_fh_number,
        local_username: local_username.to_owned(),
    };
    window.set_user_profile_busy(true);
    window.set_user_profile_error_text(SharedString::default());

    let result = call_client(
        session.core,
        ctx.ui.clone(),
        "friendshub-settings-profile-alias-save",
        move |client| profile::add_contact(client, &request),
        move |window, result| {
            let latest_alias_write = USER_PROFILE_ALIAS_WRITE_GENERATION.load(Ordering::Relaxed)
                == alias_write_generation;
            USER_PROFILE_ALIAS_WRITES_PENDING.fetch_sub(1, Ordering::Relaxed);
            if latest_alias_write {
                next_user_profile_alias_write_generation();
            }
            let current_account_matches = completion_context
                .active_session()
                .is_some_and(|session| session.account.id == completion_account_id);
            if !current_account_matches
                || user_profile_id().as_deref() != Some(completion_profile_id.as_str())
            {
                return;
            }

            let same_alias_editor =
                USER_PROFILE_ALIAS_GENERATION.load(Ordering::Relaxed) == alias_generation;
            match result {
                Ok(contact) => {
                    if latest_alias_write {
                        let profile_username = window.get_user_profile_username().to_string();
                        let displayed_title = contact.local_username.clone();
                        let alias = if contact.local_username == profile_username {
                            SharedString::default()
                        } else {
                            contact.local_username.clone().into()
                        };
                        window.set_user_profile_alias(alias);
                        crate::app::chat::update_direct_chat_title(
                            &completion_context,
                            &completion_profile_id,
                            &displayed_title,
                        );
                    }
                    if same_alias_editor {
                        window.set_user_profile_busy(false);
                        window.set_user_profile_alias_editor_open(false);
                        window.set_user_profile_alias_save_enabled(false);
                        window.set_user_profile_error_text(SharedString::default());
                    }
                }
                Err(error) => {
                    if same_alias_editor {
                        window.set_user_profile_busy(false);
                        window.set_user_profile_error_text(error.user_message().into());
                    }
                    report_core_failure("save profile contact alias", error);
                }
            }
        },
    );
    if let Err(error) = result {
        USER_PROFILE_ALIAS_WRITES_PENDING.fetch_sub(1, Ordering::Relaxed);
        next_user_profile_alias_write_generation();
        report_dispatch_failure("save profile contact alias", error);
        ctx.apply_ui(|window| {
            window.set_user_profile_busy(false);
            window.set_user_profile_error_text("Не удалось запустить операцию.".into());
        });
        false
    } else {
        true
    }
}

fn load_public_profile(ctx: &AppContext, id: &str) -> bool {
    let profile_generation = next_user_profile_load_generation();
    let alias_write_generation = USER_PROFILE_ALIAS_WRITE_GENERATION.load(Ordering::Relaxed);
    next_user_profile_alias_generation();
    let profile_changed = user_profile_id().as_deref() != Some(id);
    set_user_profile_id(Some(id.to_owned()));
    ctx.apply_ui(move |window| {
        window.set_user_profile_open(true);
        window.set_user_profile_loading(true);
        window.set_user_profile_unavailable(false);
        window.set_user_profile_busy(false);
        window.set_user_profile_error_text(SharedString::default());
        window.set_user_profile_alias_editor_open(false);
        window.set_user_profile_alias_save_enabled(false);
        if profile_changed {
            window.set_user_profile_alias(SharedString::default());
            window.set_user_profile_is_contact(false);
            window.set_user_profile_fh_number(SharedString::default());
        }
    });
    let Some(session) = ctx.active_session() else {
        ctx.apply_ui(|window| {
            window.set_user_profile_loading(false);
            window.set_user_profile_unavailable(true);
        });
        return false;
    };
    let profile_id = id.to_owned();
    let completion_profile_id = profile_id.clone();
    let completion_account_id = session.account.id.clone();
    let completion_context = ctx.clone();
    let result = call_client(
        session.core,
        ctx.ui.clone(),
        "friendshub-settings-public-profile",
        move |client| {
            let public_profile = profile::get_public_profile(
                client,
                &profile::GetPublicProfileRequest { id: profile_id },
            )?;
            let contact_alias = if public_profile.is_contact {
                profile::list_contacts(client).map(|contacts| {
                    contacts
                        .into_iter()
                        .find(|contact| contact.target_account_id == public_profile.id)
                        .map(|contact| contact.local_username)
                        .filter(|alias| alias != &public_profile.username)
                })
            } else {
                Ok(None)
            };
            let avatar_bytes = public_profile
                .avatar_url
                .as_deref()
                .filter(|url| !url.trim().is_empty())
                .and_then(|_| load_profile_avatar_bytes(client, &public_profile.id));
            Ok((public_profile, avatar_bytes, contact_alias))
        },
        move |window, result| {
            if user_profile_id().as_deref() != Some(completion_profile_id.as_str())
                || USER_PROFILE_LOAD_GENERATION.load(Ordering::Relaxed) != profile_generation
                || !completion_context
                    .active_session()
                    .is_some_and(|session| session.account.id == completion_account_id)
            {
                return;
            }
            window.set_user_profile_loading(false);
            match result {
                Ok((public_profile, avatar_bytes, contact_alias)) => {
                    let (contact_alias, alias_loaded) = match contact_alias {
                        Ok(alias) => (alias, true),
                        Err(error) => {
                            eprintln!("[settings] load profile contact alias failed: {error}");
                            (None, false)
                        }
                    };
                    let avatar_image = avatar_bytes
                        .as_deref()
                        .and_then(|bytes| slint::Image::load_from_data(bytes, Some("webp")).ok());
                    let has_avatar = avatar_image.is_some();
                    let fields = public_profile
                        .profile_fields
                        .into_iter()
                        .map(|field| PublicProfileField {
                            label: field.label.into(),
                            value: field.value.into(),
                        })
                        .collect();
                    window.set_user_profile_unavailable(false);
                    window.set_user_profile_error_text(SharedString::default());
                    window.set_user_profile_username(public_profile.username.clone().into());
                    if USER_PROFILE_ALIAS_WRITE_GENERATION.load(Ordering::Relaxed)
                        == alias_write_generation
                        && USER_PROFILE_ALIAS_WRITES_PENDING.load(Ordering::Relaxed) == 0
                        && alias_loaded
                    {
                        window.set_user_profile_alias(contact_alias.unwrap_or_default().into());
                    }
                    window.set_user_profile_fh_number(public_profile.fh_number.into());
                    window.set_user_profile_status_text(
                        public_profile.custom_status_text.unwrap_or_default().into(),
                    );
                    window.set_user_profile_status_emoji(
                        public_profile
                            .custom_status_emoji
                            .unwrap_or_default()
                            .into(),
                    );
                    window.set_user_profile_is_contact(public_profile.is_contact);
                    window.set_user_profile_is_blocked(public_profile.is_blocked_by_me);
                    window.set_user_profile_has_avatar(has_avatar);
                    window.set_user_profile_avatar_image(avatar_image.unwrap_or_default());
                    window.set_user_profile_avatar_initials(
                        initials(&public_profile.username).into(),
                    );
                    window.set_user_profile_fields(model(fields));
                }
                Err(error) => {
                    let message = error.user_message();
                    report_core_failure("load public profile", error);
                    window.set_user_profile_unavailable(true);
                    window.set_user_profile_error_text(message.into());
                }
            }
        },
    );
    if let Err(error) = result {
        report_dispatch_failure("load public profile", error);
        ctx.apply_ui(|window| {
            window.set_user_profile_loading(false);
            window.set_user_profile_unavailable(true);
        });
        false
    } else {
        true
    }
}

fn toggle_public_profile_block(ctx: &AppContext) -> bool {
    let Some(id) = user_profile_id() else {
        return false;
    };
    let Some(window) = ctx.ui.upgrade() else {
        return false;
    };
    let blocked = window.get_user_profile_is_blocked();
    let request = profile::IdRequest { id: id.clone() };
    let context = ctx.clone();
    let operation = if blocked {
        "unblock profile"
    } else {
        "block profile"
    };
    ctx.apply_ui(|window| window.set_user_profile_busy(true));
    let request_ctx = context.clone();
    let result = ctx.call(
        operation_thread_name(operation),
        move |client| {
            if blocked {
                profile::unblock_contact(client, &request).map(|_| ())
            } else {
                profile::block_contact(client, &request).map(|_| ())
            }
        },
        move |window, result| {
            window.set_user_profile_busy(false);
            match result {
                Ok(_) => {
                    load_public_profile(&request_ctx, &id);
                }
                Err(error) => report_core_failure(operation, error),
            }
        },
    );
    if let Err(error) = result {
        report_dispatch_failure(operation, error);
        ctx.apply_ui(|window| window.set_user_profile_busy(false));
        false
    } else {
        true
    }
}

fn user_profile_lock() -> &'static Mutex<Option<String>> {
    USER_PROFILE_ID.get_or_init(|| Mutex::new(None))
}

fn set_user_profile_id(id: Option<String>) {
    if let Ok(mut stored) = user_profile_lock().lock() {
        *stored = id;
    }
}

fn next_user_profile_alias_generation() -> u64 {
    USER_PROFILE_ALIAS_GENERATION
        .fetch_add(1, Ordering::Relaxed)
        .wrapping_add(1)
}

fn next_user_profile_load_generation() -> u64 {
    USER_PROFILE_LOAD_GENERATION
        .fetch_add(1, Ordering::Relaxed)
        .wrapping_add(1)
}

fn next_user_profile_alias_write_generation() -> u64 {
    USER_PROFILE_ALIAS_WRITE_GENERATION
        .fetch_add(1, Ordering::Relaxed)
        .wrapping_add(1)
}

fn user_profile_id() -> Option<String> {
    user_profile_lock().lock().ok()?.clone()
}

fn handle_account_settings(
    ctx: &AppContext,
    action: &str,
    arg1: &str,
    arg2: &str,
    arg3: &str,
) -> bool {
    match action {
        "save-username" => update_username(ctx, arg1),
        "save-status" => save_custom_status(ctx, arg1, arg2, arg3),
        "clear-status" => clear_custom_status(ctx),
        "remove-avatar" => remove_own_avatar(ctx),
        "field-label" | "field-value" | "field-add" | "field-remove" => {
            update_profile_field_draft(ctx, action, arg1, arg2)
        }
        "save-profile-fields" => save_profile_fields(ctx),
        "status-duration" => open_status_duration_editor(ctx),
        "status-duration-choice" => choose_status_duration(ctx, arg1),
        "status-hours-changed" => update_status_duration_fields(ctx, Some(arg1), None),
        "status-minutes-changed" => update_status_duration_fields(ctx, None, Some(arg1)),
        "status-duration-selected" => select_custom_status_duration(ctx, arg1, arg2),
        "status-duration-cancelled" => {
            if let Some(window) = ctx.ui.upgrade() {
                window.set_settings_account_status_duration_open(false);
            }
            true
        }
        // Avatar choosing and clipboard access require platform adapters that
        // are not wired in this application yet.
        "choose-avatar" | "copy-fh-number" => false,
        _ => false,
    }
}

fn open_status_duration_editor(ctx: &AppContext) -> bool {
    let Some(window) = ctx.ui.upgrade() else {
        return false;
    };
    if window.get_settings_account_busy() {
        return true;
    }
    let seconds = i64::from(window.get_settings_account_status_custom_ttl_seconds())
        .clamp(60, MAX_STATUS_TTL_SECONDS);
    window.set_settings_account_status_duration_hours((seconds / 3_600).to_string().into());
    window
        .set_settings_account_status_duration_minutes(((seconds % 3_600) / 60).to_string().into());
    window.set_settings_account_status_duration_valid(true);
    window.set_settings_account_status_duration_open(true);
    true
}

fn choose_status_duration(ctx: &AppContext, choice: &str) -> bool {
    let Some(window) = ctx.ui.upgrade() else {
        return false;
    };
    if window.get_settings_account_busy() {
        return true;
    }
    let seconds = match choice {
        "hour" => 3_600,
        "eight-hours" => 8 * 3_600,
        "week" => MAX_STATUS_TTL_SECONDS,
        "forever" => 0,
        "custom" => return open_status_duration_editor(ctx),
        _ => return false,
    };
    window.set_settings_account_status_ttl_seconds(seconds as i32);
    true
}

fn update_status_duration_fields(
    ctx: &AppContext,
    changed_hours: Option<&str>,
    changed_minutes: Option<&str>,
) -> bool {
    let Some(window) = ctx.ui.upgrade() else {
        return true;
    };
    let hours = changed_hours.map(str::to_owned).unwrap_or_else(|| {
        window
            .get_settings_account_status_duration_hours()
            .to_string()
    });
    let minutes = changed_minutes.map(str::to_owned).unwrap_or_else(|| {
        window
            .get_settings_account_status_duration_minutes()
            .to_string()
    });
    let valid = status_duration_seconds(&hours, &minutes).is_some();
    window.set_settings_account_status_duration_hours(hours.into());
    window.set_settings_account_status_duration_minutes(minutes.into());
    window.set_settings_account_status_duration_valid(valid);
    true
}

fn select_custom_status_duration(ctx: &AppContext, hours: &str, minutes: &str) -> bool {
    let Some(seconds) = status_duration_seconds(hours, minutes) else {
        update_status_duration_fields(ctx, Some(hours), Some(minutes));
        return true;
    };
    let Some(window) = ctx.ui.upgrade() else {
        return true;
    };
    window.set_settings_account_status_ttl_seconds(seconds);
    window.set_settings_account_status_custom_ttl_seconds(seconds);
    window.set_settings_account_status_duration_open(false);
    true
}

fn status_duration_seconds(hours: &str, minutes: &str) -> Option<i32> {
    let hours = hours.trim().parse::<i64>().ok()?;
    let minutes = minutes.trim().parse::<i64>().ok()?;
    if !(0..=59).contains(&minutes) {
        return None;
    }
    let seconds = hours
        .checked_mul(3_600)?
        .checked_add(minutes.checked_mul(60)?)?;
    (60..=MAX_STATUS_TTL_SECONDS)
        .contains(&seconds)
        .then_some(seconds as i32)
}

fn remove_own_avatar(ctx: &AppContext) -> bool {
    let Some(window) = ctx.ui.upgrade() else {
        return false;
    };
    if window.get_settings_account_busy() {
        return true;
    }
    if window.get_settings_account_avatar_url().trim().is_empty() {
        return true;
    }
    let Some(session) = ctx.active_session() else {
        return false;
    };

    let account_id = session.account.id.clone();
    let completion_account_id = account_id.clone();
    let context = ctx.clone();
    ctx.apply_ui(|window| {
        window.set_settings_account_busy(true);
        window.set_settings_account_error_text(SharedString::default());
    });
    dispatch_core_completion(
        ctx,
        "remove own avatar",
        profile::delete_own_avatar,
        move |window, result| {
            if !context
                .active_session()
                .is_some_and(|session| session.account.id == completion_account_id)
            {
                return;
            }
            window.set_settings_account_busy(false);
            match result {
                Ok(response) if response.deleted => {
                    window.set_settings_account_error_text(SharedString::default());
                    load_own_profile(&context);
                }
                Ok(_) => {
                    window.set_settings_account_error_text(
                        "Сервер не подтвердил удаление аватара.".into(),
                    );
                }
                Err(error) => {
                    let message = error.user_message();
                    window.set_settings_account_error_text(message.into());
                    report_core_failure("remove own avatar", error);
                }
            }
        },
    )
}

fn update_username(ctx: &AppContext, value: &str) -> bool {
    if !valid_username(value) {
        ctx.apply_ui(|window| {
            window.set_settings_account_username_valid(false);
            window.set_settings_account_username_error(
                "Имя не соответствует допустимому формату.".into(),
            );
        });
        return false;
    }
    let Some(session) = ctx.active_session() else {
        return false;
    };

    let accounts = ctx.accounts.clone();
    let context = ctx.clone();
    let username = value.to_owned();
    let completion_username = username.clone();
    let account_id = session.account.id.clone();
    let call_account_id = account_id.clone();
    let session_core = session.core.clone();
    ctx.apply_ui(|window| {
        window.set_settings_account_busy(true);
        window.set_settings_account_username_valid(true);
        window.set_settings_account_username_error(SharedString::default());
    });
    let result = call_client(
        session_core.clone(),
        ctx.ui.clone(),
        "friendshub-settings-username-update",
        move |client| {
            let updated = profile::update_username(
                client,
                &profile::UpdateUsernameRequest {
                    username: username.clone(),
                },
            )?;
            let persisted = if updated.updated {
                persist_username(&accounts, &call_account_id, &username)
            } else {
                Ok(())
            };
            Ok((updated, persisted))
        },
        move |window, result| match result {
            Ok((response, persisted)) => {
                if let Err(error) = persisted {
                    report_account_failure("save username metadata", &error);
                    window.set_settings_account_username_error(
                        "Имя обновлено на сервере, но локальный список аккаунтов не обновился."
                            .into(),
                    );
                }
                window.set_settings_account_busy(false);
                if response.updated {
                    window.set_settings_account_username_valid(true);
                    window.set_settings_account_username_error(SharedString::default());
                    window.set_account_username(completion_username.clone().into());
                    update_active_username(
                        &context,
                        &account_id,
                        session_core,
                        &completion_username,
                    );
                    load_own_profile(&context);
                }
            }
            Err(error) => {
                window.set_settings_account_busy(false);
                window.set_settings_account_username_valid(false);
                window.set_settings_account_username_error(error.user_message().into());
                report_core_failure("update username", error);
            }
        },
    );
    if let Err(error) = result {
        report_dispatch_failure("update username", error);
        ctx.apply_ui(|window| window.set_settings_account_busy(false));
        false
    } else {
        true
    }
}

fn update_profile_field_draft(ctx: &AppContext, action: &str, index: &str, value: &str) -> bool {
    let parsed_index = if matches!(action, "field-label" | "field-value" | "field-remove") {
        match index.parse::<usize>() {
            Ok(index) => Some(index),
            Err(_) => return false,
        }
    } else {
        None
    };
    let action = action.to_owned();
    let value = value.to_owned();
    ctx.apply_ui(move |window| {
        let current = window.get_settings_account_profile_fields();
        let mut rows = model_rows(&current);
        match action.as_str() {
            "field-label" => {
                let Some(field) = rows.get_mut(parsed_index.unwrap_or(usize::MAX)) else {
                    return;
                };
                field.label = value.into();
            }
            "field-value" => {
                let Some(field) = rows.get_mut(parsed_index.unwrap_or(usize::MAX)) else {
                    return;
                };
                field.value = value.into();
            }
            "field-add" if rows.len() < 10 => rows.push(SettingsProfileField {
                label: SharedString::default(),
                value: SharedString::default(),
            }),
            "field-remove" if parsed_index.is_some_and(|index| index < rows.len()) => {
                rows.remove(parsed_index.unwrap());
            }
            _ => return,
        }
        window.set_settings_account_profile_fields(model(rows));
    });
    true
}

fn model_rows<T: Clone + 'static>(model: &ModelRc<T>) -> Vec<T> {
    (0..model.row_count())
        .filter_map(|index| model.row_data(index))
        .collect()
}

fn save_profile_fields(ctx: &AppContext) -> bool {
    let Some(window) = ctx.ui.upgrade() else {
        return false;
    };
    let fields = model_rows(&window.get_settings_account_profile_fields())
        .into_iter()
        .map(|field| profile::ProfileField {
            label: field.label.to_string(),
            value: field.value.to_string(),
        })
        .collect::<Vec<_>>();
    if fields.len() > 10
        || fields
            .iter()
            .any(|field| field.label.chars().count() > 64 || field.value.chars().count() > 128)
    {
        return false;
    }
    let request = profile::SetProfileFieldsRequest { fields };
    ctx.apply_ui(|window| window.set_settings_account_busy(true));
    let context = ctx.clone();
    dispatch_core_completion(
        ctx,
        "save profile fields",
        move |client| profile::set_profile_fields(client, &request),
        move |window, result| {
            window.set_settings_account_busy(false);
            match result {
                Ok(response) if response.updated => load_own_profile(&context),
                Ok(_) => {}
                Err(error) => report_core_failure("save profile fields", error),
            }
        },
    )
}

fn clear_custom_status(ctx: &AppContext) -> bool {
    ctx.apply_ui(|window| window.set_settings_account_busy(true));
    let context = ctx.clone();
    dispatch_core_completion(
        ctx,
        "clear custom status",
        profile::clear_custom_status,
        move |window, result| {
            window.set_settings_account_busy(false);
            match result {
                Ok(_) => load_own_profile(&context),
                Err(error) => report_core_failure("clear custom status", error),
            }
        },
    )
}

fn update_active_username(
    ctx: &AppContext,
    account_id: &str,
    core: std::sync::Arc<crate::core::CoreClient>,
    username: &str,
) {
    let Some(mut current) = ctx.active_session() else {
        return;
    };
    if current.account.id != account_id {
        return;
    }
    current.account.username = username.to_owned();
    current.core = core;
    if let Err(error) = ctx.set_active_session(Some(current)) {
        eprintln!("[settings] save active username failed: {error}");
    }
}

fn persist_username(
    accounts: &AccountStore,
    account_id: &str,
    username: &str,
) -> Result<(), crate::platform::accounts::AccountStoreError> {
    let manifest = accounts.load()?;
    let Some(mut account) = manifest
        .accounts
        .into_iter()
        .find(|account| account.id == account_id)
    else {
        return Ok(());
    };
    account.username = username.to_owned();
    accounts.update(account).map(|_| ())
}

fn valid_username(username: &str) -> bool {
    let chars = username.chars().collect::<Vec<_>>();
    !chars.is_empty()
        && chars.len() <= MAX_USERNAME_CHARS
        && !username.starts_with(' ')
        && !username.ends_with(' ')
        && !username.contains("  ")
        && chars.iter().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '_' | '.' | ' ')
        })
}

fn save_custom_status(ctx: &AppContext, text: &str, emoji: &str, ttl_seconds: &str) -> bool {
    let Ok(ttl_seconds) = ttl_seconds.parse::<i64>() else {
        return false;
    };
    if ttl_seconds != 0 && !(60..=MAX_STATUS_TTL_SECONDS).contains(&ttl_seconds) {
        return false;
    }
    let request = profile::UpdateCustomStatusRequest {
        text: Some(text.to_owned()),
        emoji: Some(emoji.to_owned()),
        ttl_seconds: (ttl_seconds > 0).then_some(ttl_seconds),
    };
    ctx.apply_ui(|window| window.set_settings_account_busy(true));
    let context = ctx.clone();
    dispatch_core_completion(
        ctx,
        "update custom status",
        move |client| profile::update_custom_status(client, &request),
        move |window, result| {
            window.set_settings_account_busy(false);
            match result {
                Ok(response) if response.updated => load_own_profile(&context),
                Ok(_) => {}
                Err(error) => report_core_failure("update custom status", error),
            }
        },
    )
}

fn handle_appearance(ctx: &AppContext, action: &str, value: &str) -> bool {
    match action {
        "set-theme" => {
            let light_mode = match value {
                "light" | "true" => true,
                "dark" | "false" => false,
                _ => return false,
            };
            match update_preferences(&ctx.data_dir, PreferenceChange::Theme(light_mode)) {
                Ok(preferences) => {
                    ctx.apply_ui(move |window| apply_preferences(&window, &preferences));
                    true
                }
                Err(error) => {
                    report_local_failure("save theme preference", &error);
                    false
                }
            }
        }
        "set-language" => {
            if value != "ru" && value != "en" {
                return false;
            }
            match update_preferences(&ctx.data_dir, PreferenceChange::Language(value.to_owned())) {
                Ok(preferences) => {
                    ctx.apply_ui(move |window| apply_preferences(&window, &preferences));
                    true
                }
                Err(error) => {
                    report_local_failure("save language preference", &error);
                    false
                }
            }
        }
        "edit-color" => {
            if let Some(window) = ctx.ui.upgrade() {
                if let Some((background, _, _, text)) = read_rgb_draft(&window) {
                    window.set_appearance_contrast_score(contrast_score(background, text));
                    true
                } else {
                    window.set_appearance_contrast_score(0);
                    false
                }
            } else {
                false
            }
        }
        "apply-colors" => {
            let Some(window) = ctx.ui.upgrade() else {
                return false;
            };
            let Some((background, button, bubble, text)) = read_rgb_draft(&window) else {
                window.set_appearance_contrast_score(0);
                return false;
            };
            if contrast_score(background, text) < 20 {
                return false;
            }
            match update_preferences(
                &ctx.data_dir,
                PreferenceChange::Colors {
                    background,
                    button,
                    bubble,
                    text,
                    custom: true,
                },
            ) {
                Ok(preferences) => {
                    ctx.apply_ui(move |window| apply_preferences(&window, &preferences));
                    true
                }
                Err(error) => {
                    report_local_failure("save interface colors", &error);
                    false
                }
            }
        }
        "reset-colors" => match update_preferences(&ctx.data_dir, PreferenceChange::ResetColors) {
            Ok(preferences) => {
                ctx.apply_ui(move |window| apply_preferences(&window, &preferences));
                true
            }
            Err(error) => {
                report_local_failure("reset interface colors", &error);
                false
            }
        },
        // No native color-picker adapter is installed.
        "choose-color" => false,
        _ => false,
    }
}

fn read_rgb_draft(window: &MainWindow) -> Option<([u8; 3], [u8; 3], [u8; 3], [u8; 3])> {
    Some((
        [
            parse_rgb(&window.get_appearance_background_r())?,
            parse_rgb(&window.get_appearance_background_g())?,
            parse_rgb(&window.get_appearance_background_b())?,
        ],
        [
            parse_rgb(&window.get_appearance_button_r())?,
            parse_rgb(&window.get_appearance_button_g())?,
            parse_rgb(&window.get_appearance_button_b())?,
        ],
        [
            parse_rgb(&window.get_appearance_bubble_r())?,
            parse_rgb(&window.get_appearance_bubble_g())?,
            parse_rgb(&window.get_appearance_bubble_b())?,
        ],
        [
            parse_rgb(&window.get_appearance_text_r())?,
            parse_rgb(&window.get_appearance_text_g())?,
            parse_rgb(&window.get_appearance_text_b())?,
        ],
    ))
}

fn parse_rgb(value: &SharedString) -> Option<u8> {
    value.as_str().parse::<u8>().ok()
}

fn handle_privacy(ctx: &AppContext, action: &str, arg1: &str, arg2: &str) -> bool {
    match action {
        "dm-policy" => {
            let policy = match arg1 {
                "everyone" => profile::DirectMessagePolicy::Everyone,
                "contacts_only" => profile::DirectMessagePolicy::ContactsOnly,
                _ => return false,
            };
            dispatch_core(ctx, "set direct-message policy", move |client| {
                profile::set_dm_policy(client, &profile::PolicyRequest { policy })
            })
        }
        "group-add-policy" => {
            let policy = match arg1 {
                "everyone" => profile::GroupAddPolicy::Everyone,
                "nobody" => profile::GroupAddPolicy::Nobody,
                _ => return false,
            };
            dispatch_core(ctx, "set group-add policy", move |client| {
                profile::set_group_add_policy(client, &profile::PolicyRequest { policy })
            })
        }
        "invisible" | "allow-nudges" | "profile-hidden" | "hidden-from-search" => {
            let Some(enabled) = parse_bool(arg1) else {
                return false;
            };
            match action {
                "invisible" => dispatch_core(ctx, "set invisible mode", move |client| {
                    profile::set_invisible_mode(client, &profile::ToggleRequest { enabled })
                }),
                "allow-nudges" => dispatch_core(ctx, "set allow nudges", move |client| {
                    profile::set_allow_nudges(
                        client,
                        &profile::SetAllowNudgesRequest { allow: enabled },
                    )
                }),
                "profile-hidden" => dispatch_core(ctx, "set profile visibility", move |client| {
                    profile::set_profile_hidden(client, &profile::ToggleRequest { enabled })
                }),
                _ => dispatch_core(ctx, "set search visibility", move |client| {
                    profile::set_hidden_from_search(client, &profile::ToggleRequest { enabled })
                }),
            }
        }
        "presence-exception-add" => {
            if arg2.trim().is_empty() {
                return false;
            }
            let kind = match arg1 {
                "always_visible" => profile::PresenceExceptionKind::AlwaysVisible,
                "always_invisible" => profile::PresenceExceptionKind::AlwaysInvisible,
                _ => return false,
            };
            let request = profile::PresenceExceptionRequest {
                target_account_id: arg2.to_owned(),
                kind,
            };
            dispatch_core(ctx, "add presence exception", move |client| {
                profile::add_presence_exception(client, &request)
            })
        }
        "presence-exception-remove" => {
            if arg2.trim().is_empty() {
                return false;
            }
            let kind = match arg1 {
                "always_visible" => profile::PresenceExceptionKind::AlwaysVisible,
                "always_invisible" => profile::PresenceExceptionKind::AlwaysInvisible,
                _ => return false,
            };
            let request = profile::PresenceExceptionRequest {
                target_account_id: arg2.to_owned(),
                kind,
            };
            dispatch_core(ctx, "remove presence exception", move |client| {
                profile::remove_presence_exception(client, &request)
            })
        }
        "profile-exception-add"
        | "profile-exception-remove"
        | "group-add-exception-add"
        | "group-add-exception-remove" => {
            if arg1.trim().is_empty() {
                return false;
            }
            let request = profile::AccountIdRequest {
                account_id: arg1.to_owned(),
            };
            match action {
                "profile-exception-add" => {
                    dispatch_core(ctx, "add profile exception", move |client| {
                        profile::add_profile_exception(client, &request)
                    })
                }
                "profile-exception-remove" => {
                    dispatch_core(ctx, "remove profile exception", move |client| {
                        profile::remove_profile_exception(client, &request)
                    })
                }
                "group-add-exception-add" => {
                    dispatch_core(ctx, "add group exception", move |client| {
                        profile::add_group_add_exception(client, &request)
                    })
                }
                _ => dispatch_core(ctx, "remove group exception", move |client| {
                    profile::remove_group_add_exception(client, &request)
                }),
            }
        }
        "block-contact" | "unblock-contact" => {
            if arg1.trim().is_empty() {
                return false;
            }
            if action == "block-contact" {
                let request = profile::IdRequest {
                    id: arg1.to_owned(),
                };
                dispatch_core(ctx, "block contact", move |client| {
                    profile::block_contact(client, &request)
                })
            } else {
                let request = profile::IdRequest {
                    id: arg1.to_owned(),
                };
                dispatch_core(ctx, "unblock contact", move |client| {
                    profile::unblock_contact(client, &request)
                })
            }
        }
        _ => false,
    }
}

fn handle_security(ctx: &AppContext, action: &str, id: &str, _arg2: &str) -> bool {
    match action {
        "revoke-session" => {
            if id.trim().is_empty() {
                return false;
            }
            let request = profile::IdRequest { id: id.to_owned() };
            dispatch_core(ctx, "revoke session", move |client| {
                profile::revoke_session(client, &request)
            })
        }
        "revoke-all-sessions" => {
            dispatch_core(ctx, "revoke all sessions", profile::revoke_all_sessions)
        }
        "revoke-device" => {
            if id.trim().is_empty() {
                return false;
            }
            let request = profile::IdRequest { id: id.to_owned() };
            dispatch_core(ctx, "revoke device", move |client| {
                profile::revoke_device(client, &request)
            })
        }
        "history-sync" | "request-history-sync" | "load-history" | "load-device-history" => {
            ctx.apply_ui(|window| {
                window.set_security_history_error(
                    "Перенос и загрузка истории не реализованы в нативном клиенте.".into(),
                );
                window.set_security_history_progress(SharedString::default());
            });
            true
        }
        "select-history-source" => {
            let selected = id.to_owned();
            ctx.apply_ui(move |window| window.set_security_history_source_device(selected.into()));
            true
        }
        "select-history-conversation" => {
            let selected = id.to_owned();
            ctx.apply_ui(move |window| {
                window.set_security_history_conversation_id(selected.into());
                window.set_security_history_has_older_history(false);
            });
            true
        }
        "refresh-sessions" | "refresh-devices" | "refresh-events" => {
            load_security(ctx);
            true
        }
        _ => false,
    }
}

fn handle_twofactor(ctx: &AppContext, action: &str, arg1: &str, arg2: &str) -> bool {
    match action {
        "start" => start_twofactor_enrollment(ctx),
        "verify" => verify_twofactor_enrollment(ctx, arg1),
        "disable" => disable_twofactor(ctx, arg1, arg2),
        "dismiss-codes" => {
            ctx.apply_ui(|window| {
                window.set_twofactor_setup_show_backup_codes(false);
                window.set_twofactor_setup_backup_codes(model(Vec::new()));
                window.set_twofactor_setup_secret_base32(SharedString::default());
                window.set_twofactor_setup_otpauth_uri(SharedString::default());
                window.set_twofactor_setup_enrollment_started(false);
                window.set_twofactor_setup_error_text(SharedString::default());
            });
            true
        }
        "copy-uri" => {
            ctx.apply_ui(|window| {
                window.set_twofactor_setup_error_text(
                    "Буфер обмена недоступен в нативном клиенте; введите ключ вручную.".into(),
                );
            });
            true
        }
        // The backup-code UI emits a dedicated root callback handled during
        // initialize(); other copy actions reach this routed callback.
        "copy-otpauth-uri" | "copy-backup-codes" => false,
        _ => false,
    }
}

fn start_twofactor_enrollment(ctx: &AppContext) -> bool {
    ctx.apply_ui(|window| {
        window.set_twofactor_setup_busy(true);
        window.set_twofactor_setup_error_text(SharedString::default());
        window.set_twofactor_setup_secret_base32(SharedString::default());
        window.set_twofactor_setup_otpauth_uri(SharedString::default());
        window.set_twofactor_setup_has_qr_image(false);
        window.set_twofactor_setup_qr_image(slint::Image::default());
        window.set_twofactor_setup_enrollment_started(false);
        window.set_twofactor_setup_show_backup_codes(false);
        window.set_twofactor_setup_backup_codes(model(Vec::new()));
    });
    dispatch_core_completion(
        ctx,
        "start two-factor enrollment",
        auth::twofa_enroll,
        |window, result| {
            window.set_twofactor_setup_busy(false);
            match result {
                Ok(enrollment) => {
                    match qr_image_from_otpauth_uri(&enrollment.otpauth_uri) {
                        Ok(image) => {
                            window.set_twofactor_setup_qr_image(image);
                            window.set_twofactor_setup_has_qr_image(true);
                            window.set_twofactor_setup_error_text(SharedString::default());
                        }
                        Err(message) => {
                            window.set_twofactor_setup_qr_image(slint::Image::default());
                            window.set_twofactor_setup_has_qr_image(false);
                            window.set_twofactor_setup_error_text(message.into());
                        }
                    }
                    window.set_twofactor_setup_secret_base32(enrollment.secret_base32.into());
                    window.set_twofactor_setup_otpauth_uri(enrollment.otpauth_uri.into());
                    window.set_twofactor_setup_enrollment_started(true);
                }
                Err(error) => {
                    window.set_twofactor_setup_error_text(error.user_message().into());
                    report_core_failure("start two-factor enrollment", error);
                }
            }
        },
    )
}

fn qr_image_from_otpauth_uri(uri: &str) -> Result<slint::Image, &'static str> {
    if uri.is_empty() || uri.len() > MAX_OTPAUTH_URI_BYTES {
        return Err("QR-код слишком велик. Введите ключ вручную.");
    }

    let code = QrCode::new(uri.as_bytes())
        .map_err(|_| "Не удалось сформировать QR-код. Введите ключ вручную.")?;
    let modules_per_side = code.width();
    if modules_per_side == 0 {
        return Err("Не удалось сформировать QR-код. Введите ключ вручную.");
    }

    let rendered_modules = modules_per_side
        .checked_add(QR_QUIET_ZONE_MODULES * 2)
        .ok_or("Не удалось сформировать QR-код. Введите ключ вручную.")?;
    let rendered_side = rendered_modules
        .checked_mul(QR_MODULE_SCALE)
        .and_then(|side| u32::try_from(side).ok())
        .ok_or("Не удалось сформировать QR-код. Введите ключ вручную.")?;
    let mut pixels =
        slint::SharedPixelBuffer::<slint::Rgb8Pixel>::new(rendered_side, rendered_side);
    let bytes = pixels.make_mut_bytes();
    bytes.fill(255);

    for (index, color) in code.to_colors().into_iter().enumerate() {
        if color != QrColor::Dark {
            continue;
        }
        let module_x = index % modules_per_side + QR_QUIET_ZONE_MODULES;
        let module_y = index / modules_per_side + QR_QUIET_ZONE_MODULES;
        let pixel_x = module_x * QR_MODULE_SCALE;
        let pixel_y = module_y * QR_MODULE_SCALE;
        for y in pixel_y..pixel_y + QR_MODULE_SCALE {
            for x in pixel_x..pixel_x + QR_MODULE_SCALE {
                let offset = (y * rendered_side as usize + x) * 3;
                bytes[offset..offset + 3].fill(0);
            }
        }
    }

    Ok(slint::Image::from_rgb8(pixels))
}

fn verify_twofactor_enrollment(ctx: &AppContext, code: &str) -> bool {
    if code.trim().is_empty() {
        return false;
    }
    let Some(session) = ctx.active_session() else {
        return false;
    };
    let accounts = ctx.accounts.clone();
    let account_id = session.account.id.clone();
    let completion_id = account_id.clone();
    let account_core = session.core.clone();
    let context = ctx.clone();
    let code = code.to_owned();
    ctx.apply_ui(|window| {
        window.set_twofactor_setup_busy(true);
        window.set_twofactor_setup_error_text(SharedString::default());
    });
    dispatch_core_completion(
        ctx,
        "verify two-factor enrollment",
        move |client| {
            let verified = auth::twofa_enroll_verify(client, &code)?;
            let persisted = persist_totp_enabled(&accounts, &account_id, true);
            Ok((verified, persisted))
        },
        move |window, result| {
            window.set_twofactor_setup_busy(false);
            match result {
                Ok((verified, persisted)) => {
                    if let Err(error) = persisted {
                        report_account_failure("save two-factor metadata", &error);
                        window.set_twofactor_setup_error_text(
                            "2FA включена, но локальный список аккаунтов не обновился.".into(),
                        );
                    } else {
                        window.set_twofactor_setup_error_text(SharedString::default());
                    }
                    update_active_totp(&context, &completion_id, account_core, true);
                    window.set_twofactor_setup_enabled(true);
                    window.set_twofactor_setup_enrollment_started(false);
                    window.set_twofactor_setup_show_backup_codes(true);
                    window.set_twofactor_setup_backup_codes(model(
                        verified
                            .backup_codes
                            .into_iter()
                            .map(SharedString::from)
                            .collect(),
                    ));
                    window.set_twofactor_setup_verify_code(SharedString::default());
                    window.set_security_totp_enabled(true);
                }
                Err(error) => {
                    window.set_twofactor_setup_error_text(error.user_message().into());
                    report_core_failure("verify two-factor enrollment", error);
                }
            }
        },
    )
}

fn disable_twofactor(ctx: &AppContext, password: &str, code: &str) -> bool {
    if password.is_empty() || code.is_empty() {
        return false;
    }
    let Some(session) = ctx.active_session() else {
        return false;
    };
    let account_id = session.account.id.clone();
    let call_account_id = account_id.clone();
    let accounts = ctx.accounts.clone();
    let context = ctx.clone();
    let password = password.to_owned();
    let code = code.to_owned();
    let core = session.core.clone();
    ctx.apply_ui(|window| {
        window.set_twofactor_setup_busy(true);
        window.set_twofactor_setup_error_text(SharedString::default());
    });
    let result = call_client(
        core.clone(),
        ctx.ui.clone(),
        "friendshub-settings-twofactor-disable",
        move |client| {
            let response = auth::twofa_disable(client, &password, &code)?;
            let persisted = if response.disabled {
                persist_totp_enabled(&accounts, &call_account_id, false)
            } else {
                Ok(())
            };
            Ok((response, persisted))
        },
        move |window, result| {
            window.set_twofactor_setup_busy(false);
            match result {
                Ok((response, persisted)) => {
                    if let Err(error) = persisted {
                        report_account_failure("save two-factor metadata", &error);
                        window.set_twofactor_setup_error_text(
                            "2FA отключена, но локальный список аккаунтов не обновился.".into(),
                        );
                    }
                    if response.disabled {
                        update_active_totp(&context, &account_id, core, false);
                        window.set_twofactor_setup_enabled(false);
                        window.set_security_totp_enabled(false);
                        window.set_twofactor_setup_show_backup_codes(false);
                        window.set_twofactor_setup_backup_codes(model(Vec::new()));
                        window.set_twofactor_setup_enrollment_started(false);
                        window.set_twofactor_setup_secret_base32(SharedString::default());
                        window.set_twofactor_setup_otpauth_uri(SharedString::default());
                        window.set_twofactor_setup_verify_code(SharedString::default());
                        window.set_twofactor_setup_disable_password(SharedString::default());
                        window.set_twofactor_setup_disable_code(SharedString::default());
                    }
                }
                Err(error) => {
                    window.set_twofactor_setup_error_text(error.user_message().into());
                    report_core_failure("disable two-factor authentication", error);
                }
            }
        },
    );
    if let Err(error) = result {
        report_dispatch_failure("disable two-factor authentication", error);
        ctx.apply_ui(|window| window.set_twofactor_setup_busy(false));
        false
    } else {
        true
    }
}

fn persist_totp_enabled(
    accounts: &AccountStore,
    account_id: &str,
    enabled: bool,
) -> Result<(), crate::platform::accounts::AccountStoreError> {
    let manifest = accounts.load()?;
    let Some(mut account) = manifest
        .accounts
        .into_iter()
        .find(|account| account.id == account_id)
    else {
        return Ok(());
    };
    account.totp_enabled = enabled;
    accounts.update(account).map(|_| ())
}

fn update_active_totp(
    ctx: &AppContext,
    account_id: &str,
    core: std::sync::Arc<CoreClient>,
    enabled: bool,
) {
    let Some(mut active) = ctx.active_session() else {
        return;
    };
    if active.account.id != account_id {
        return;
    }
    active.account.totp_enabled = enabled;
    active.core = core;
    if let Err(error) = ctx.set_active_session(Some(active)) {
        eprintln!("[settings] refresh active two-factor state failed: {error}");
    }
}

fn handle_notifications(ctx: &AppContext, action: &str, value: &str) -> bool {
    match action {
        "sound-enabled" => {
            let Some(enabled) = parse_bool(value) else {
                return false;
            };
            persist_preference(ctx, PreferenceChange::SoundEnabled(enabled))
        }
        "sound" => {
            if !is_supported_sound(value) {
                return false;
            }
            persist_preference(ctx, PreferenceChange::Sound(value.to_owned()))
        }
        // System notification permission/settings and local sound preview do
        // not have native adapters in this checkout.
        "preview-sound" | "request-permission" | "open-system-settings" => false,
        _ => false,
    }
}

fn persist_preference(ctx: &AppContext, change: PreferenceChange) -> bool {
    match update_preferences(&ctx.data_dir, change) {
        Ok(preferences) => {
            ctx.apply_ui(move |window| apply_preferences(&window, &preferences));
            true
        }
        Err(error) => {
            report_local_failure("save notification preference", &error);
            false
        }
    }
}

fn handle_accounts(ctx: &AppContext, action: &str, id: &str) -> bool {
    match action {
        "switch" => switch_account(ctx, id),
        // AccountStore removes only registry metadata. The current core has no
        // account-database purge API, while the UI promises that local message
        // and key data are deleted. Avoid reporting a partial removal as done.
        "remove" => {
            ctx.apply_ui(|window| {
                window.set_settings_accounts_error_text(
                    "Удаление аккаунта и связанных локальных ключей не поддерживается этим клиентом."
                        .into(),
                );
            });
            true
        }
        "retry" | "load" | "open-switcher" => {
            load_accounts(ctx);
            true
        }
        _ => false,
    }
}

fn handle_bots(ctx: &AppContext, action: &str, bot_id: &str, value: &str) -> bool {
    let Some(window) = ctx.ui.upgrade() else {
        return false;
    };
    if window.get_screen() != "settings_bots" {
        return false;
    }

    match action {
        "retry" => {
            load_bots(ctx);
            true
        }
        "dismiss-token" => {
            ctx.apply_ui(|window| window.set_settings_bots_token_secret(SharedString::default()));
            true
        }
        "copy-token" => {
            ctx.apply_ui(|window| {
                window.set_settings_bots_error_text(
                    "Буфер обмена недоступен; скопируйте токен вручную и закройте это окно после сохранения.".into(),
                );
            });
            true
        }
        "create" => {
            let context = ctx.clone();
            set_bots_busy(ctx, true);
            dispatch_core_completion(
                ctx,
                "create managed bot",
                bots::create,
                move |window, result| {
                    window.set_settings_bots_busy(false);
                    match result {
                        Ok(credential) => {
                            window.set_settings_bots_token_secret(credential.token.into());
                            window.set_settings_bots_error_text(SharedString::default());
                            load_bots(&context);
                        }
                        Err(error) => {
                            window.set_settings_bots_error_text(error.user_message().into());
                            report_core_failure("create managed bot", error);
                        }
                    }
                },
            )
        }
        "set-handle" if !bot_id.trim().is_empty() => {
            let bot_id = bot_id.to_owned();
            let handle = value.trim().trim_start_matches('@').to_ascii_lowercase();
            let context = ctx.clone();
            set_bots_busy(ctx, true);
            dispatch_core_completion(
                ctx,
                "set managed bot handle",
                move |client| bots::set_handle(client, &bot_id, &handle),
                move |window, result| {
                    window.set_settings_bots_busy(false);
                    match result {
                        Ok(_) => {
                            window.set_settings_bots_error_text(SharedString::default());
                            load_bots(&context);
                        }
                        Err(error) => {
                            window.set_settings_bots_error_text(error.user_message().into());
                            report_core_failure("set managed bot handle", error);
                        }
                    }
                },
            )
        }
        "rotate-token" if !bot_id.trim().is_empty() => {
            let bot_id = bot_id.to_owned();
            let context = ctx.clone();
            set_bots_busy(ctx, true);
            dispatch_core_completion(
                ctx,
                "rotate managed bot token",
                move |client| bots::rotate_token(client, &bot_id),
                move |window, result| {
                    window.set_settings_bots_busy(false);
                    match result {
                        Ok(token) => {
                            window.set_settings_bots_token_secret(token.token.into());
                            window.set_settings_bots_error_text(SharedString::default());
                            load_bots(&context);
                        }
                        Err(error) => {
                            window.set_settings_bots_error_text(error.user_message().into());
                            report_core_failure("rotate managed bot token", error);
                        }
                    }
                },
            )
        }
        "delete" if !bot_id.trim().is_empty() => {
            let bot_id = bot_id.to_owned();
            let context = ctx.clone();
            set_bots_busy(ctx, true);
            dispatch_core_completion(
                ctx,
                "delete managed bot",
                move |client| bots::delete(client, &bot_id),
                move |window, result| {
                    window.set_settings_bots_busy(false);
                    match result {
                        Ok(()) => {
                            window.set_settings_bots_error_text(SharedString::default());
                            window.set_settings_bots_token_secret(SharedString::default());
                            load_bots(&context);
                        }
                        Err(error) => {
                            window.set_settings_bots_error_text(error.user_message().into());
                            report_core_failure("delete managed bot", error);
                        }
                    }
                },
            )
        }
        _ => false,
    }
}

fn set_bots_busy(ctx: &AppContext, busy: bool) {
    ctx.apply_ui(move |window| {
        window.set_settings_bots_busy(busy);
        window.set_settings_bots_error_text(SharedString::default());
    });
}

fn switch_account(ctx: &AppContext, id: &str) -> bool {
    if id.is_empty() {
        return false;
    }
    let accounts = ctx.accounts.clone();
    let data_dir = ctx.data_dir.clone();
    let context = ctx.clone();
    let requested_id = id.to_owned();
    ctx.apply_ui(move |window| {
        window.set_settings_accounts_busy(true);
        window.set_settings_busy_account_id(requested_id.into());
        window.set_settings_accounts_error_text(SharedString::default());
    });
    let requested_id = id.to_owned();
    let result = thread::Builder::new()
        .name("friendshub-settings-account-switch".to_owned())
        .spawn(move || {
            let result = (|| {
                let manifest = accounts.load().map_err(|error| error.to_string())?;
                let account = manifest
                    .accounts
                    .into_iter()
                    .find(|account| account.id == requested_id)
                    .ok_or_else(|| "account was not found".to_owned())?;
                let core = CoreClient::open(CoreClient::database_path(&data_dir, account.db_key()))
                    .map_err(|failure| failure.user_message().to_owned())?;
                let current = auth::me(&core.client())
                    .map_err(|error| CoreCallFailure::from(error).user_message().to_owned())?;
                if current.id != account.id
                    || current.user_id != account.user_id
                    || current.fh_number != account.fh_number
                {
                    return Err(
                        "stored account identity does not match its local database".to_owned()
                    );
                }
                let mut refreshed = account;
                refreshed.username = current.username;
                refreshed.avatar_url = current.avatar_url;
                refreshed.totp_enabled = current.totp_enabled;
                accounts
                    .update(refreshed.clone())
                    .map_err(|error| error.to_string())?;
                accounts
                    .set_active(&refreshed.id)
                    .map_err(|error| error.to_string())?;
                Ok(ActiveSession {
                    account: refreshed,
                    core: std::sync::Arc::new(core),
                })
            })();
            let completion_context = context.clone();
            context.apply_ui(move |window| match result {
                Ok(session) => {
                    let username = session.account.username.clone();
                    let fh_number = session.account.fh_number.clone();
                    let active_id = session.account.id.clone();
                    if let Err(error) = completion_context.set_active_session(Some(session)) {
                        eprintln!("[settings] activate selected account failed: {error}");
                        window.set_settings_accounts_error_text(
                            "Аккаунт открыт локально, но приложение не переключило активную сессию."
                                .into(),
                        );
                        window.set_settings_accounts_busy(false);
                        window.set_settings_busy_account_id(SharedString::default());
                        return;
                    }
                    window.set_account_username(username.into());
                    window.set_account_fh_number(fh_number.into());
                    window.set_settings_active_account_id(active_id.into());
                    window.set_settings_accounts_busy(false);
                    window.set_settings_busy_account_id(SharedString::default());
                }
                Err(error) => {
                    eprintln!("[settings] switch account failed: {error}");
                    window.set_settings_accounts_error_text(error.into());
                    window.set_settings_accounts_busy(false);
                    window.set_settings_busy_account_id(SharedString::default());
                }
            });
        });
    if let Err(error) = result {
        report_dispatch_failure("switch account", error);
        ctx.apply_ui(|window| {
            window.set_settings_accounts_busy(false);
            window.set_settings_busy_account_id(SharedString::default());
            window.set_settings_accounts_error_text(
                "Не удалось запустить переключение аккаунта.".into(),
            );
        });
        false
    } else {
        true
    }
}

fn handle_about(ctx: &AppContext, action: &str, value: &str, opened: &str) -> bool {
    match action {
        "open-url" => {
            let Some(window) = ctx.ui.upgrade() else {
                return false;
            };
            if value != WEB_REPOSITORY_URL
                && value != NATIVE_RELEASES_URL
                && value != CORE_REPOSITORY_URL
            {
                window.set_about_update_status(
                    "Ссылка не входит в список разрешённых адресов.".into(),
                );
                return true;
            }
            let status = if opened == "true" {
                format!("Открыта ссылка: {value}")
            } else {
                format!("Не удалось открыть ссылку: {value}")
            };
            window.set_about_update_status(status.into());
            true
        }
        "copy-text" => {
            ctx.apply_ui(|window| {
                window
                    .set_about_update_status("Буфер обмена недоступен в нативном клиенте.".into());
            });
            true
        }
        "check-updates" => {
            ctx.apply_ui(|window| {
                window.set_about_update_status(
                    "Автоматическая проверка обновлений не подключена.".into(),
                );
            });
            true
        }
        _ => false,
    }
}

fn dispatch_core<T, F>(ctx: &AppContext, operation: &'static str, call: F) -> bool
where
    T: Send + 'static,
    F: FnOnce(&Client) -> Result<T, CoreError> + Send + 'static,
{
    let refresh = match operation {
        "set direct-message policy"
        | "set group-add policy"
        | "set invisible mode"
        | "set allow nudges"
        | "set profile visibility"
        | "set search visibility"
        | "add presence exception"
        | "remove presence exception"
        | "add profile exception"
        | "remove profile exception"
        | "add group exception"
        | "remove group exception"
        | "block contact"
        | "unblock contact" => Some("privacy"),
        "revoke session" | "revoke all sessions" | "revoke device" => Some("security"),
        _ => None,
    };
    if let Some(refresh) = refresh {
        ctx.apply_ui(move |window| match refresh {
            "privacy" => window.set_privacy_busy(true),
            _ => window.set_security_busy(true),
        });
    }
    let context = ctx.clone();
    match ctx.call(
        operation_thread_name(operation),
        call,
        move |_window, result| match result {
            Ok(_) => match refresh {
                Some("privacy") => load_privacy(&context),
                Some("security") => load_security(&context),
                _ => {}
            },
            Err(error) => {
                if let Some(refresh) = refresh {
                    context.apply_ui(move |window| match refresh {
                        "privacy" => window.set_privacy_busy(false),
                        _ => window.set_security_busy(false),
                    });
                }
                report_core_failure(operation, error);
            }
        },
    ) {
        Ok(_) => true,
        Err(error) => {
            if let Some(refresh) = refresh {
                ctx.apply_ui(move |window| match refresh {
                    "privacy" => window.set_privacy_busy(false),
                    _ => window.set_security_busy(false),
                });
            }
            report_dispatch_failure(operation, error);
            false
        }
    }
}

fn dispatch_core_completion<T, F, C>(
    ctx: &AppContext,
    operation: &'static str,
    call: F,
    completion: C,
) -> bool
where
    T: Send + 'static,
    F: FnOnce(&Client) -> Result<T, CoreError> + Send + 'static,
    C: FnOnce(MainWindow, Result<T, CoreCallFailure>) + Send + 'static,
{
    match ctx.call(operation_thread_name(operation), call, completion) {
        Ok(_) => true,
        Err(error) => {
            let message = error.to_string();
            ctx.apply_ui(move |window| match operation {
                "load privacy settings" => window.set_privacy_busy(false),
                "load security settings" => {
                    window.set_security_busy(false);
                    window.set_security_session_error(message.clone().into());
                    window.set_security_device_error(message.clone().into());
                    window.set_security_events_error(message.clone().into());
                    window.set_security_history_error(message.into());
                }
                "load managed bots" => {
                    window.set_settings_bots_loading(false);
                    window.set_settings_bots_error_text(message.into());
                }
                "start two-factor enrollment" | "verify two-factor enrollment" => {
                    window.set_twofactor_setup_busy(false);
                    window.set_twofactor_setup_error_text(message.into());
                }
                "save profile fields" | "clear custom status" | "update custom status" => {
                    window.set_settings_account_busy(false);
                }
                "remove own avatar" => {
                    window.set_settings_account_busy(false);
                    window.set_settings_account_error_text(message.into());
                }
                "create managed bot"
                | "set managed bot handle"
                | "rotate managed bot token"
                | "delete managed bot" => {
                    window.set_settings_bots_busy(false);
                    window.set_settings_bots_error_text(message.into());
                }
                _ => {}
            });
            report_dispatch_failure(operation, error);
            false
        }
    }
}

fn operation_thread_name(operation: &str) -> &'static str {
    match operation {
        "set direct-message policy" => "friendshub-settings-dm-policy",
        "set group-add policy" => "friendshub-settings-group-policy",
        "set invisible mode" => "friendshub-settings-invisible",
        "set allow nudges" => "friendshub-settings-nudges",
        "set profile visibility" => "friendshub-settings-profile-visibility",
        "set search visibility" => "friendshub-settings-search-visibility",
        "add presence exception" => "friendshub-settings-presence-exception-add",
        "remove presence exception" => "friendshub-settings-presence-exception-remove",
        "add profile exception" => "friendshub-settings-profile-exception-add",
        "remove profile exception" => "friendshub-settings-profile-exception-remove",
        "add group exception" => "friendshub-settings-group-exception-add",
        "remove group exception" => "friendshub-settings-group-exception-remove",
        "block contact" => "friendshub-settings-block-contact",
        "unblock contact" => "friendshub-settings-unblock-contact",
        "revoke session" => "friendshub-settings-revoke-session",
        "revoke all sessions" => "friendshub-settings-revoke-all-sessions",
        "revoke device" => "friendshub-settings-revoke-device",
        "clear custom status" => "friendshub-settings-status-clear",
        "update custom status" => "friendshub-settings-status-update",
        "load privacy settings" => "friendshub-settings-privacy-load",
        "load security settings" => "friendshub-settings-security-load",
        "load managed bots" => "friendshub-settings-bots-load",
        "start two-factor enrollment" => "friendshub-settings-twofactor-enroll",
        "verify two-factor enrollment" => "friendshub-settings-twofactor-verify",
        "save profile fields" => "friendshub-settings-profile-fields-save",
        "open conversation with profile" => "friendshub-settings-profile-message",
        "create managed bot" => "friendshub-settings-bot-create",
        "set managed bot handle" => "friendshub-settings-bot-handle",
        "rotate managed bot token" => "friendshub-settings-bot-token-rotate",
        "delete managed bot" => "friendshub-settings-bot-delete",
        _ => "friendshub-settings-core-operation",
    }
}

fn parse_bool(value: &str) -> Option<bool> {
    match value {
        "true" => Some(true),
        "false" => Some(false),
        _ => None,
    }
}

fn is_supported_sound(value: &str) -> bool {
    matches!(value, "chime" | "ping" | "pop" | "none")
}

fn read_preferences(data_dir: &Path) -> io::Result<LocalPreferences> {
    let _guard = preferences_lock()
        .lock()
        .map_err(|_| io::Error::other("preferences lock is unavailable"))?;
    read_preferences_unlocked(data_dir)
}

fn update_preferences(data_dir: &Path, change: PreferenceChange) -> io::Result<LocalPreferences> {
    let _guard = preferences_lock()
        .lock()
        .map_err(|_| io::Error::other("preferences lock is unavailable"))?;
    let mut preferences = read_preferences_unlocked(data_dir)?;
    match change {
        PreferenceChange::Theme(value) => {
            preferences.light_mode = value;
            if !preferences.has_custom_colors {
                let (background, button, bubble, text) = default_theme_colors(value);
                preferences.background_rgb = background;
                preferences.button_rgb = button;
                preferences.bubble_rgb = bubble;
                preferences.text_rgb = text;
            }
        }
        PreferenceChange::Language(value) => preferences.language = value,
        PreferenceChange::SoundEnabled(value) => preferences.sound_enabled = value,
        PreferenceChange::Sound(value) => preferences.sound = value,
        PreferenceChange::ResetColors => {
            let (background, button, bubble, text) = default_theme_colors(preferences.light_mode);
            preferences.background_rgb = background;
            preferences.button_rgb = button;
            preferences.bubble_rgb = bubble;
            preferences.text_rgb = text;
            preferences.has_custom_colors = false;
        }
        PreferenceChange::Colors {
            background,
            button,
            bubble,
            text,
            custom,
        } => {
            preferences.background_rgb = background;
            preferences.button_rgb = button;
            preferences.bubble_rgb = bubble;
            preferences.text_rgb = text;
            preferences.has_custom_colors = custom;
        }
    }
    validate_preferences(&preferences)?;
    write_preferences_unlocked(data_dir, &preferences)?;
    Ok(preferences)
}

fn preferences_lock() -> &'static Mutex<()> {
    PREFERENCES_LOCK.get_or_init(|| Mutex::new(()))
}

fn read_preferences_unlocked(data_dir: &Path) -> io::Result<LocalPreferences> {
    let path = preferences_path(data_dir)?;
    let metadata = match fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(LocalPreferences::default());
        }
        Err(error) => return Err(error),
    };
    if !metadata.file_type().is_file() || metadata.len() > MAX_PREFERENCES_BYTES {
        return Err(invalid_preferences("file type or size is invalid"));
    }
    set_private_file_permissions(&path)?;
    let file = File::open(&path)?;
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.take(MAX_PREFERENCES_BYTES + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_PREFERENCES_BYTES {
        return Err(invalid_preferences("file is too large"));
    }
    let preferences: LocalPreferences =
        serde_json::from_slice(&bytes).map_err(|_| invalid_preferences("JSON is invalid"))?;
    validate_preferences(&preferences)?;
    Ok(preferences)
}

fn write_preferences_unlocked(data_dir: &Path, preferences: &LocalPreferences) -> io::Result<()> {
    let path = preferences_path(data_dir)?;
    if !path.parent().is_some_and(Path::is_dir) {
        fs::create_dir_all(data_dir)?;
    }
    let bytes = serde_json::to_vec_pretty(preferences)
        .map_err(|_| io::Error::other("could not encode app preferences"))?;
    let (mut file, temporary_path) = create_temporary_preferences(data_dir)?;
    let result = file.write_all(&bytes).and_then(|()| file.sync_all());
    drop(file);
    if let Err(error) = result {
        let _ = fs::remove_file(&temporary_path);
        return Err(error);
    }
    let result = fs::rename(&temporary_path, &path)
        .and_then(|()| set_private_file_permissions(&path))
        .and_then(|()| sync_directory(data_dir));
    if result.is_err() {
        let _ = fs::remove_file(&temporary_path);
    }
    result
}

fn preferences_path(data_dir: &Path) -> io::Result<PathBuf> {
    if !data_dir.is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "app data directory must be absolute",
        ));
    }
    Ok(data_dir.join(PREFERENCES_FILE))
}

fn validate_preferences(preferences: &LocalPreferences) -> io::Result<()> {
    if preferences.language != "ru" && preferences.language != "en" {
        return Err(invalid_preferences("language is unsupported"));
    }
    if !is_supported_sound(&preferences.sound) {
        return Err(invalid_preferences("sound value is unsupported"));
    }
    Ok(())
}

fn create_temporary_preferences(data_dir: &Path) -> io::Result<(File, PathBuf)> {
    for _ in 0..32 {
        let sequence = TEMP_FILE_COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = data_dir.join(format!(
            ".{PREFERENCES_FILE}.{}.{}.tmp",
            std::process::id(),
            sequence
        ));
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        set_create_mode(&mut options);
        match options.open(&path) {
            Ok(file) => return Ok((file, path)),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "could not reserve a temporary preferences path",
    ))
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

fn invalid_preferences(reason: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, reason)
}

fn report_core_failure(operation: &str, error: CoreCallFailure) {
    eprintln!(
        "[settings] {operation} failed ({}): {}",
        error.code(),
        error.user_message()
    );
}

fn report_dispatch_failure(operation: &str, error: io::Error) {
    eprintln!(
        "[settings] could not schedule {operation}: {}",
        error.kind()
    );
}

fn report_local_failure(operation: &str, error: &io::Error) {
    eprintln!("[settings] {operation} failed: {}", error.kind());
}

fn report_account_failure(operation: &str, error: &crate::platform::accounts::AccountStoreError) {
    eprintln!("[settings] {operation} failed: {error}");
}
