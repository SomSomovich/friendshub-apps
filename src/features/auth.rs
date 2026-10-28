//! Typed application operations for authentication, device setup, and TOTP.
//!
//! These wrappers keep the core's Rust `snake_case` JSON contract at the
//! application boundary and delegate all persistence and server communication
//! to `friendshub_core::Client`. Core calls are synchronous; callers should run
//! them on a worker thread.

use friendshub_core::{Client, error::Result as CoreResult};
use serde::{Deserialize, Serialize};

#[derive(Serialize)]
struct RegisterRequest<'a> {
    password: &'a str,
}

/// Public account identity returned after registration.
#[derive(Deserialize)]
pub struct RegisterResponse {
    pub id: String,
    pub user_id: i64,
    pub fh_number: String,
    pub username: String,
}

/// Register an account. The password is borrowed for this call and is not
/// retained by this module.
pub fn register(client: &Client, password: &str) -> CoreResult<RegisterResponse> {
    client.auth_register(&RegisterRequest { password })
}

#[derive(Serialize)]
struct LoginRequest<'a> {
    fh_number: &'a str,
    password: &'a str,
    device_number: i64,
}

#[derive(Serialize)]
struct Login2faRequest<'a> {
    challenge_token: &'a str,
    code: &'a str,
    device_number: i64,
}

/// Account profile returned by login and `me`.
#[derive(Deserialize)]
pub struct MeResponse {
    pub id: String,
    pub user_id: i64,
    pub fh_number: String,
    pub username: String,
    pub avatar_url: Option<String>,
    pub totp_enabled: bool,
    pub custom_status_text: Option<String>,
    pub custom_status_emoji: Option<String>,
    pub custom_status_expires_at: Option<i64>,
}

/// Login outcomes. The core persists a successful session in its protected
/// database; the session token is deliberately not deserialized into the app.
#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum LoginResult {
    Session {
        expires_at: i64,
        account: MeResponse,
    },
    TotpRequired {
        challenge_token: String,
        expires_in_seconds: u64,
    },
}

/// Authenticate the selected account device. A successful session is stored
/// by the core; only a short-lived TOTP challenge is returned to the app.
pub fn login(
    client: &Client,
    fh_number: &str,
    password: &str,
    device_number: i64,
) -> CoreResult<LoginResult> {
    client.auth_login(&LoginRequest {
        fh_number,
        password,
        device_number,
    })
}

/// Finish login with a TOTP or backup code. The core persists the resulting
/// session and the response intentionally omits its session token.
pub fn login_2fa(
    client: &Client,
    challenge_token: &str,
    code: &str,
    device_number: i64,
) -> CoreResult<LoginResult> {
    client.auth_login_2fa(&Login2faRequest {
        challenge_token,
        code,
        device_number,
    })
}

/// Fetch the current account profile using the session held by the core.
pub fn me(client: &Client) -> CoreResult<MeResponse> {
    client.auth_me()
}

/// Revoke the current session and clear it from the core's local database.
#[derive(Deserialize)]
pub struct LogoutResponse {
    pub ok: bool,
}

pub fn logout(client: &Client) -> CoreResult<LogoutResponse> {
    client.auth_logout()
}

/// Device initialization state and current local one-time prekey counts.
#[derive(Deserialize)]
pub struct DeviceInitStatus {
    pub initialized: bool,
    pub one_time_prekeys: i64,
    pub kyber_one_time_prekeys: i64,
    pub min_threshold: usize,
}

pub fn device_init_status(client: &Client) -> CoreResult<DeviceInitStatus> {
    client.device_init_status()
}

#[derive(Serialize)]
struct DeviceInitRequest<'a> {
    name: &'a str,
}

/// Result of creating/registering this device and its initial prekeys.
#[derive(Deserialize)]
pub struct DeviceInitResult {
    pub device_id: String,
    pub device_number: i64,
    pub registration_id: i64,
    pub one_time_prekeys: usize,
    pub kyber_one_time_prekeys: usize,
}

pub fn device_init_initialize(
    client: &Client,
    name: &str,
) -> CoreResult<DeviceInitResult> {
    client.device_init_initialize(&DeviceInitRequest { name })
}

/// Number of one-time prekeys generated and uploaded by the core.
#[derive(Deserialize)]
pub struct EnsurePrekeysResult {
    pub generated: usize,
}

pub fn device_init_ensure_prekeys(client: &Client) -> CoreResult<EnsurePrekeysResult> {
    client.device_init_ensure_prekeys()
}

/// Enrollment material returned once while configuring an authenticator.
/// The app should display it for setup and must not log or persist it.
#[derive(Deserialize)]
pub struct TotpEnrollment {
    pub secret_base32: String,
    pub otpauth_uri: String,
}

pub fn twofa_enroll(client: &Client) -> CoreResult<TotpEnrollment> {
    client.twofa_enroll()
}

#[derive(Serialize)]
struct TotpVerifyRequest<'a> {
    code: &'a str,
}

/// One-time backup codes returned after successful TOTP enrollment.
/// Display them once and do not log or persist them in application settings.
#[derive(Deserialize)]
pub struct TotpEnrollmentVerified {
    pub backup_codes: Vec<String>,
}

pub fn twofa_enroll_verify(client: &Client, code: &str) -> CoreResult<TotpEnrollmentVerified> {
    client.twofa_enroll_verify(&TotpVerifyRequest { code })
}

#[derive(Serialize)]
struct TotpDisableRequest<'a> {
    password: &'a str,
    code: &'a str,
}

#[derive(Deserialize)]
pub struct TotpDisableResult {
    pub disabled: bool,
}

/// Disable TOTP using the account password and current TOTP or backup code.
pub fn twofa_disable(
    client: &Client,
    password: &str,
    code: &str,
) -> CoreResult<TotpDisableResult> {
    client.twofa_disable(&TotpDisableRequest { password, code })
}
