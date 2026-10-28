//! Typed profile, privacy, and account-security operations backed by the
//! native `friendshub-core::Client` dispatcher.
//!
//! Field names intentionally match the native core's JSON wire contracts
//! (snake_case). Appearance, language, and other local interface preferences
//! do not belong in this module.

use friendshub_core::{Client, error::Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpdatedResponse {
    pub updated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClearedResponse {
    pub cleared: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AvatarDeletedResponse {
    pub deleted: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AddedResponse {
    pub added: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemovedResponse {
    pub removed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RevokedResponse {
    pub revoked: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlockedResponse {
    pub blocked: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UnblockedResponse {
    pub unblocked: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpdateUsernameRequest {
    pub username: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpdateCustomStatusRequest {
    pub text: Option<String>,
    pub emoji: Option<String>,
    pub ttl_seconds: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OwnProfile {
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProfileField {
    pub label: String,
    pub value: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProfileFieldsResponse {
    pub fields: Vec<ProfileField>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SetProfileFieldsRequest {
    pub fields: Vec<ProfileField>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GetPublicProfileRequest {
    /// Native `profile_get_public_profile` expects `id` in its JSON body.
    pub id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PublicProfile {
    pub id: String,
    pub user_id: i64,
    pub fh_number: String,
    pub username: String,
    pub avatar_url: Option<String>,
    pub custom_status_text: Option<String>,
    pub custom_status_emoji: Option<String>,
    pub custom_status_expires_at: Option<i64>,
    pub is_blocked_by_me: bool,
    pub is_contact: bool,
    #[serde(default)]
    pub profile_fields: Vec<ProfileField>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GetPresenceRequest {
    pub account_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PresenceResponse {
    pub account_id: String,
    pub is_online: bool,
    pub last_seen: Option<i64>,
    pub custom_status_text: Option<String>,
    pub custom_status_emoji: Option<String>,
    pub custom_status_expires_at: Option<i64>,
    pub visible: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrivacySettings {
    pub hide_profile: bool,
    pub hide_from_search: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToggleRequest {
    pub enabled: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProfileException {
    pub account_id: String,
    pub created_at: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccountIdRequest {
    pub account_id: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PresenceExceptionKind {
    AlwaysVisible,
    AlwaysInvisible,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PresenceException {
    pub target_account_id: String,
    pub kind: PresenceExceptionKind,
    pub created_at: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PresenceExceptionRequest {
    pub target_account_id: String,
    pub kind: PresenceExceptionKind,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InvisibleModeResponse {
    pub enabled: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DirectMessagePolicy {
    Everyone,
    ContactsOnly,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GroupAddPolicy {
    Everyone,
    Nobody,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PolicyResponse<P> {
    pub policy: P,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PolicyRequest<P> {
    pub policy: P,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GroupAddException {
    pub account_id: String,
    pub created_at: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AllowNudgesResponse {
    pub allow: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SetAllowNudgesRequest {
    pub allow: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HandleLookupRequest {
    pub handle: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HandleLookupResponse {
    pub entity_type: String,
    pub entity_id: String,
    pub handle_display: String,
    pub handle_normalized: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HandleAvailabilityRequest {
    pub handle: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HandleKind {
    Bot,
    Regular,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HandleAvailabilityResponse {
    pub handle: String,
    pub normalized: String,
    pub available: bool,
    pub handle_kind: HandleKind,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BatchHandleLookupRequest {
    pub handles: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BatchHandleLookupItem {
    pub handle_input: String,
    pub normalized: String,
    pub found: bool,
    pub entity_type: Option<String>,
    pub entity_id: Option<String>,
    pub handle_display: Option<String>,
    pub handle_normalized: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BatchHandleLookupResponse {
    pub items: Vec<BatchHandleLookupItem>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlockedContact {
    pub account_id: String,
    pub fh_number: String,
    pub username: String,
    pub avatar_url: Option<String>,
    pub blocked_at: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IdRequest {
    /// Native `contacts_block` / `contacts_unblock` operations read `id`.
    pub id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionItem {
    pub id: String,
    pub ip_address: Option<String>,
    pub user_agent: Option<String>,
    pub created_at: i64,
    pub expires_at: i64,
    pub revoked_at: Option<i64>,
    pub is_current: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RevokeAllSessionsResponse {
    pub revoked: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceItem {
    pub id: String,
    pub device_number: i64,
    pub name: String,
    pub registration_id: i64,
    pub identity_key_pub: String,
    pub created_at: i64,
    pub last_seen_at: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SecurityEventItem {
    pub id: String,
    pub event_type: String,
    pub ip_address: Option<String>,
    pub user_agent: Option<String>,
    pub metadata: Option<String>,
    pub created_at: i64,
}

/// Reads the current account profile from the authenticated `/me` operation.
pub fn get_own_profile(client: &Client) -> Result<OwnProfile> {
    client.auth_me()
}

pub fn update_username(
    client: &Client,
    request: &UpdateUsernameRequest,
) -> Result<UpdatedResponse> {
    client.profile_update_username(request)
}

pub fn update_custom_status(
    client: &Client,
    request: &UpdateCustomStatusRequest,
) -> Result<UpdatedResponse> {
    client.profile_update_custom_status(request)
}

pub fn clear_custom_status(client: &Client) -> Result<ClearedResponse> {
    client.profile_clear_custom_status()
}

pub fn delete_own_avatar(client: &Client) -> Result<AvatarDeletedResponse> {
    client.avatars_delete()
}

pub fn get_public_profile(
    client: &Client,
    request: &GetPublicProfileRequest,
) -> Result<PublicProfile> {
    client.profile_get_public_profile(request)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AddContactRequest {
    pub target_fh_number: String,
    pub local_username: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoveContactRequest {
    pub id: String,
}

pub fn add_contact(client: &Client, request: &AddContactRequest) -> Result<()> {
    let _: serde_json::Value = client.contacts_add(request)?;
    Ok(())
}

pub fn remove_contact(client: &Client, request: &RemoveContactRequest) -> Result<()> {
    let _: serde_json::Value = client.contacts_remove(request)?;
    Ok(())
}

pub fn get_presence(client: &Client, request: &GetPresenceRequest) -> Result<PresenceResponse> {
    client.profile_get_presence(request)
}

pub fn get_profile_fields(client: &Client) -> Result<ProfileFieldsResponse> {
    client.profile_extras_get_profile_fields()
}

pub fn set_profile_fields(
    client: &Client,
    request: &SetProfileFieldsRequest,
) -> Result<UpdatedResponse> {
    client.profile_extras_set_profile_fields(request)
}

pub fn get_privacy(client: &Client) -> Result<PrivacySettings> {
    client.profile_extras_get_privacy()
}

pub fn set_profile_hidden(client: &Client, request: &ToggleRequest) -> Result<UpdatedResponse> {
    client.profile_extras_set_profile_hidden(request)
}

pub fn set_hidden_from_search(client: &Client, request: &ToggleRequest) -> Result<UpdatedResponse> {
    client.profile_extras_set_hidden_from_search(request)
}

pub fn list_profile_exceptions(client: &Client) -> Result<Vec<ProfileException>> {
    client.profile_extras_list_profile_exceptions()
}

pub fn add_profile_exception(client: &Client, request: &AccountIdRequest) -> Result<AddedResponse> {
    client.profile_extras_add_profile_exception(request)
}

pub fn remove_profile_exception(
    client: &Client,
    request: &AccountIdRequest,
) -> Result<RemovedResponse> {
    client.profile_extras_remove_profile_exception(request)
}

pub fn get_invisible_mode(client: &Client) -> Result<InvisibleModeResponse> {
    client.profile_get_invisible()
}

pub fn set_invisible_mode(client: &Client, request: &ToggleRequest) -> Result<UpdatedResponse> {
    client.profile_set_invisible_mode(request)
}

pub fn list_presence_exceptions(client: &Client) -> Result<Vec<PresenceException>> {
    client.profile_list_exceptions()
}

pub fn add_presence_exception(
    client: &Client,
    request: &PresenceExceptionRequest,
) -> Result<AddedResponse> {
    client.profile_add_exception(request)
}

pub fn remove_presence_exception(
    client: &Client,
    request: &PresenceExceptionRequest,
) -> Result<RemovedResponse> {
    client.profile_remove_exception(request)
}

pub fn get_dm_policy(client: &Client) -> Result<PolicyResponse<DirectMessagePolicy>> {
    client.profile_extras_get_dm_policy()
}

pub fn set_dm_policy(
    client: &Client,
    request: &PolicyRequest<DirectMessagePolicy>,
) -> Result<UpdatedResponse> {
    client.profile_extras_set_dm_policy(request)
}

pub fn get_group_add_policy(client: &Client) -> Result<PolicyResponse<GroupAddPolicy>> {
    client.profile_extras_get_group_add_policy()
}

pub fn set_group_add_policy(
    client: &Client,
    request: &PolicyRequest<GroupAddPolicy>,
) -> Result<UpdatedResponse> {
    client.profile_extras_set_group_add_policy(request)
}

pub fn list_group_add_exceptions(client: &Client) -> Result<Vec<GroupAddException>> {
    client.profile_extras_list_group_add_exceptions()
}

pub fn add_group_add_exception(
    client: &Client,
    request: &AccountIdRequest,
) -> Result<AddedResponse> {
    client.profile_extras_add_group_add_exception(request)
}

pub fn remove_group_add_exception(
    client: &Client,
    request: &AccountIdRequest,
) -> Result<RemovedResponse> {
    client.profile_extras_remove_group_add_exception(request)
}

pub fn get_allow_nudges(client: &Client) -> Result<AllowNudgesResponse> {
    client.profile_extras_get_allow_nudges()
}

pub fn set_allow_nudges(
    client: &Client,
    request: &SetAllowNudgesRequest,
) -> Result<UpdatedResponse> {
    client.profile_extras_set_allow_nudges(request)
}

pub fn lookup_handle(
    client: &Client,
    request: &HandleLookupRequest,
) -> Result<HandleLookupResponse> {
    client.user_profiles_lookup(request)
}

pub fn check_handle_availability(
    client: &Client,
    request: &HandleAvailabilityRequest,
) -> Result<HandleAvailabilityResponse> {
    client.user_profiles_check_available(request)
}

pub fn batch_lookup_handles(
    client: &Client,
    request: &BatchHandleLookupRequest,
) -> Result<BatchHandleLookupResponse> {
    client.user_profiles_batch_lookup(request)
}

pub fn list_blocked_contacts(client: &Client) -> Result<Vec<BlockedContact>> {
    client.contacts_list_blocked()
}

pub fn block_contact(client: &Client, request: &IdRequest) -> Result<BlockedResponse> {
    client.contacts_block(request)
}

pub fn unblock_contact(client: &Client, request: &IdRequest) -> Result<UnblockedResponse> {
    client.contacts_unblock(request)
}

pub fn list_sessions(client: &Client) -> Result<Vec<SessionItem>> {
    client.sessions_list()
}

pub fn revoke_session(client: &Client, request: &IdRequest) -> Result<RevokedResponse> {
    client.sessions_revoke_one(request)
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EmptyRequest {}

pub fn revoke_all_sessions(client: &Client) -> Result<RevokeAllSessionsResponse> {
    client.sessions_revoke_all(&EmptyRequest {})
}

pub fn list_devices(client: &Client) -> Result<Vec<DeviceItem>> {
    client.devices_list()
}

pub fn revoke_device(client: &Client, request: &IdRequest) -> Result<RevokedResponse> {
    client.devices_revoke(request)
}

pub fn list_security_events(client: &Client) -> Result<Vec<SecurityEventItem>> {
    client.profile_extras_list_security_events()
}
