//! Typed operations for the public community directory and discover profiles.
//!
//! Calls use the native core's `snake_case` contract. They are synchronous and
//! should be dispatched by the application worker layer. The core performs
//! request validation, authenticated transport, and response decoding.

use friendshub_core::{Client, error::Result as CoreResult};
use serde::{Deserialize, Serialize};

#[derive(Serialize)]
struct PublicDirectoryRequest {
    limit: Option<i64>,
}

#[derive(Serialize)]
struct HandleRequest<'a> {
    handle: &'a str,
}

#[derive(Serialize)]
struct IdRequest<'a> {
    id: &'a str,
}

#[derive(Serialize)]
struct AccountIdRequest<'a> {
    account_id: &'a str,
}

/// One public group or channel returned by its directory operation.
///
/// The current native directory contract has no avatar field. Fetching a
/// conversation avatar is a separate operation.
#[derive(Clone, Debug, Deserialize)]
pub struct PublicDirectoryEntry {
    pub conversation_id: String,
    pub title: Option<String>,
    pub description: Option<String>,
    pub owner_account_id: String,
    pub created_at: i64,
    pub updated_at: i64,
    pub member_count: i64,
    pub handle: Option<String>,
    pub handle_normalized: Option<String>,
}

/// A single public community resolved by its handle.
#[derive(Clone, Debug, Deserialize)]
pub struct CommunityByHandle {
    pub conversation_id: String,
    pub title: Option<String>,
    pub description: Option<String>,
    pub member_count: i64,
    pub already_member: bool,
}

/// List public groups. `None` uses the native core's default limit of 50.
/// The WEB Discover screen requests 200 explicitly.
pub fn list_public_groups(
    client: &Client,
    limit: Option<i64>,
) -> CoreResult<Vec<PublicDirectoryEntry>> {
    client.groups_list_public(&PublicDirectoryRequest { limit })
}

/// List public channels. The core clamps an explicit limit to 200; `None`
/// uses its default limit of 50.
pub fn list_public_channels(
    client: &Client,
    limit: Option<i64>,
) -> CoreResult<Vec<PublicDirectoryEntry>> {
    client.community_extras_list_public_channels(&PublicDirectoryRequest { limit })
}

/// Resolve a group handle. Pass the handle without the display `@` prefix.
pub fn lookup_group_by_handle(client: &Client, handle: &str) -> CoreResult<CommunityByHandle> {
    client.groups_lookup_by_handle(&HandleRequest { handle })
}

/// Resolve a channel handle. Pass the handle without the display `@` prefix.
pub fn lookup_channel_by_handle(client: &Client, handle: &str) -> CoreResult<CommunityByHandle> {
    client.community_extras_lookup_channel_by_handle(&HandleRequest { handle })
}

#[derive(Clone, Copy, Debug, Deserialize)]
pub struct GroupJoined {
    pub joined: bool,
}

/// Join a public group. The server treats an existing membership as success.
pub fn join_public_group(client: &Client, conversation_id: &str) -> CoreResult<GroupJoined> {
    client.groups_join_public(&IdRequest {
        id: conversation_id,
    })
}

#[derive(Clone, Copy, Debug, Deserialize)]
pub struct GroupLeft {
    pub left: bool,
}

/// Leave a group through the common conversation operation. The server
/// enforces ownership and membership restrictions.
pub fn leave_group(client: &Client, conversation_id: &str) -> CoreResult<GroupLeft> {
    client.conversations_leave(&IdRequest {
        id: conversation_id,
    })
}

#[derive(Clone, Copy, Debug, Deserialize)]
pub struct ChannelSubscribed {
    pub subscribed: bool,
}

/// Subscribe to a public channel before refreshing the conversation list.
pub fn subscribe_channel(client: &Client, conversation_id: &str) -> CoreResult<ChannelSubscribed> {
    client.channels_subscribe(&IdRequest {
        id: conversation_id,
    })
}

#[derive(Clone, Copy, Debug, Deserialize)]
pub struct ChannelUnsubscribed {
    pub unsubscribed: bool,
}

/// Unsubscribe from a channel. A channel owner must first transfer ownership
/// or delete the channel; that restriction is enforced by the server.
pub fn unsubscribe_channel(
    client: &Client,
    conversation_id: &str,
) -> CoreResult<ChannelUnsubscribed> {
    client.channels_unsubscribe(&IdRequest {
        id: conversation_id,
    })
}

/// Profile gender values accepted by the discover profile API.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DiscoverGender {
    Male,
    Female,
    Other,
    Unspecified,
}

/// Directory filtering excludes the profile-only `unspecified` value.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DiscoverGenderFilter {
    Male,
    Female,
    Other,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DiscoverLookingFor {
    Friendship,
    Relationship,
    Chat,
    All,
}

/// The selected account's complete discover profile.
#[derive(Clone, Debug, Deserialize)]
pub struct DiscoverProfile {
    pub account_id: String,
    pub display_name: String,
    pub city: String,
    pub birth_year: i32,
    pub gender: DiscoverGender,
    pub looking_for: DiscoverLookingFor,
    pub bio: Option<String>,
    pub interests: Vec<String>,
    pub is_listed: bool,
    pub updated_at: i64,
}

/// Full replacement data for creating or editing a discover profile.
///
/// The core validates the profile, trims text, and normalizes empty optional
/// biography text. `is_listed` controls whether it appears in search results.
#[derive(Clone, Debug, Serialize)]
pub struct DiscoverProfileInput {
    pub display_name: String,
    pub city: String,
    pub birth_year: i32,
    pub gender: DiscoverGender,
    pub looking_for: DiscoverLookingFor,
    pub bio: Option<String>,
    pub interests: Vec<String>,
    pub is_listed: bool,
}

/// Load the selected account's profile. A missing profile remains a core
/// error so the caller can distinguish absence from transport failure.
pub fn get_discover_profile(client: &Client) -> CoreResult<DiscoverProfile> {
    client.profile_extras_get_discover_profile()
}

/// Create or fully replace the account's discover profile.
pub fn save_discover_profile(
    client: &Client,
    profile: &DiscoverProfileInput,
) -> CoreResult<DiscoverProfile> {
    client.profile_extras_save_discover_profile(profile)
}

#[derive(Clone, Copy, Debug, Deserialize)]
pub struct DiscoverProfileHidden {
    pub hidden: bool,
}

/// Hide the profile from search. The API preserves its profile data.
pub fn hide_discover_profile(client: &Client) -> CoreResult<DiscoverProfileHidden> {
    client.profile_extras_hide_discover_profile()
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct DiscoverFilters {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub city: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gender: Option<DiscoverGenderFilter>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub looking_for: Option<DiscoverLookingFor>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub min_age: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_age: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub interests: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub query: Option<String>,
}

/// Stable pagination boundary copied from the last entry of the prior page.
#[derive(Clone, Debug, Serialize)]
pub struct DiscoverCursor {
    pub before: i64,
    pub before_account_id: String,
}

/// Discover search parameters. An absent limit uses 20; the core accepts
/// limits from 1 through 50 and validates age ordering and all filters.
#[derive(Clone, Debug, Default, Serialize)]
pub struct DiscoverQuery {
    pub filters: DiscoverFilters,
    pub cursor: Option<DiscoverCursor>,
    pub limit: Option<i64>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct DiscoverCard {
    pub account_id: String,
    pub display_name: String,
    pub city: String,
    pub birth_year: i32,
    pub age: i32,
    pub gender: DiscoverGender,
    pub looking_for: DiscoverLookingFor,
    pub bio: Option<String>,
    pub interests: Vec<String>,
    pub updated_at: i64,
}

/// Search the discover directory. The API returns a page of cards, and the
/// application retains the active filters and constructs the next cursor.
pub fn list_discover_profiles(
    client: &Client,
    query: &DiscoverQuery,
) -> CoreResult<Vec<DiscoverCard>> {
    client.profile_extras_list_discover_profiles(query)
}

#[derive(Clone, Debug, Deserialize)]
pub struct DiscoverProfileDetails {
    pub account_id: String,
    pub display_name: String,
    pub city: String,
    pub birth_year: i32,
    pub age: i32,
    pub gender: DiscoverGender,
    pub looking_for: DiscoverLookingFor,
    pub bio: Option<String>,
    pub interests: Vec<String>,
    pub updated_at: i64,
    pub avatar_url: Option<String>,
    pub fh_number: String,
}

#[derive(Serialize)]
struct AccountAvatarRequest<'a> {
    account_id: &'a str,
}

#[derive(Clone, Debug, Deserialize)]
pub struct AccountAvatar {
    pub account_id: String,
    pub content_type: String,
    pub data_base64: String,
}

/// Fetch a public account avatar through the native core's typed operation.
/// The response is base64 encoded so the FFI remains JSON-only.
pub fn get_account_avatar(client: &Client, account_id: &str) -> CoreResult<AccountAvatar> {
    client.avatars_serve(&AccountAvatarRequest { account_id })
}

pub fn get_discover_profile_details(
    client: &Client,
    account_id: &str,
) -> CoreResult<DiscoverProfileDetails> {
    client.profile_extras_get_discover_profile_details(&AccountIdRequest { account_id })
}
