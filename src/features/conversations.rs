//! Application-owned DTOs and operations for conversations.
//!
//! Requests and responses use the native core dispatcher's portable JSON
//! shape: field names are the exact snake_case wire names. All I/O is delegated
//! to the typed `friendshub_core::Client` methods; this module does not perform
//! HTTP requests or translate through the browser API.

use friendshub_core::Client;
use friendshub_core::error::Result;
use serde::{Deserialize, Serialize};

/// One conversation row returned by `GET /conversations`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Conversation {
    pub id: String,
    pub kind: String,
    pub title: Option<String>,
    pub member_count: i64,
    pub last_envelope_at: Option<i64>,
    pub created_at: i64,
    pub updated_at: i64,
    pub archived_at: Option<i64>,
    pub muted_until: Option<i64>,
}

/// The result of creating or resolving a direct conversation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DirectConversation {
    pub id: String,
    pub kind: String,
}

/// Full server-side metadata for a conversation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConversationDetail {
    pub id: String,
    pub kind: String,
    pub title: Option<String>,
    pub created_by: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
    pub members: Vec<String>,
}

/// An encrypted envelope in the current device's conversation history.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConversationEnvelope {
    pub envelope_id: String,
    pub sender_account_id: String,
    pub sender_device_number: i64,
    pub recipient_device_number: i64,
    pub envelope_type: i64,
    pub is_prekey_message: bool,
    pub ciphertext: String,
    pub client_timestamp: i64,
    pub server_timestamp: i64,
    pub conversation_id: Option<String>,
}

/// One account's read marker in a conversation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConversationRead {
    pub account_id: String,
    pub last_read_at: i64,
}

/// Request body for core chat-extra operations that use `conversation_id`.
#[derive(Debug, Clone, Serialize)]
pub struct ConversationIdRequest {
    pub conversation_id: String,
}

/// Fetch all conversations visible to this account.
pub fn list(client: &Client) -> Result<Vec<Conversation>> {
    client.conversations_list()
}

/// Resolve or create a direct conversation with the supplied FriendsHub number.
pub fn create_direct(
    client: &Client,
    peer_fh_number: impl Into<String>,
) -> Result<DirectConversation> {
    let request = CreateDirectRequest {
        peer_fh_number: peer_fh_number.into(),
    };
    client.conversations_create_direct(&request)
}

/// Fetch server-side metadata and current membership for a conversation.
pub fn get(client: &Client, conversation_id: impl Into<String>) -> Result<ConversationDetail> {
    let request = ConversationPathRequest {
        id: conversation_id.into(),
    };
    client.conversations_get_one(&request)
}

/// Leave a group or channel conversation.
pub fn leave(client: &Client, conversation_id: impl Into<String>) -> Result<()> {
    let request = ConversationPathRequest {
        id: conversation_id.into(),
    };
    client
        .conversations_leave::<_, LeftResponse>(&request)
        .map(|_| ())
}

/// Archive a conversation for the current account.
pub fn archive(client: &Client, conversation_id: impl Into<String>) -> Result<()> {
    let request = ConversationPathRequest {
        id: conversation_id.into(),
    };
    client
        .conversations_archive::<_, ArchivedResponse>(&request)
        .map(|_| ())
}

/// Remove a conversation from the current account's archive.
pub fn unarchive(client: &Client, conversation_id: impl Into<String>) -> Result<()> {
    let request = ConversationPathRequest {
        id: conversation_id.into(),
    };
    client
        .conversations_unarchive::<_, UnarchivedResponse>(&request)
        .map(|_| ())
}

/// Mute notifications for a duration in seconds; `None` means indefinitely.
pub fn mute(
    client: &Client,
    conversation_id: impl Into<String>,
    duration_seconds: Option<i64>,
) -> Result<()> {
    let request = MuteConversationRequest {
        id: conversation_id.into(),
        duration_seconds,
    };
    client
        .conversations_mute::<_, MutedResponse>(&request)
        .map(|_| ())
}

/// Remove the current account's mute from a conversation.
pub fn unmute(client: &Client, conversation_id: impl Into<String>) -> Result<()> {
    let request = ConversationPathRequest {
        id: conversation_id.into(),
    };
    client
        .conversations_unmute::<_, UnmutedResponse>(&request)
        .map(|_| ())
}

/// Fetch encrypted history for this conversation and device.
pub fn envelopes(
    client: &Client,
    conversation_id: impl Into<String>,
    cursor: HistoryCursor,
) -> Result<Vec<ConversationEnvelope>> {
    let request = ConversationHistoryRequest {
        conversation_id: conversation_id.into(),
        before: cursor.before,
        before_id: cursor.before_id,
        limit: cursor.limit,
    };
    client.chat_extras_conversation_envelopes(&request)
}

/// Fetch all account read markers, including the current account's marker.
pub fn reads(client: &Client, conversation_id: impl Into<String>) -> Result<Vec<ConversationRead>> {
    let request = ConversationIdRequest {
        conversation_id: conversation_id.into(),
    };
    client.chat_extras_conversation_reads(&request)
}

/// Update this account's read marker. `None` asks the server to use its current time.
pub fn mark_read(
    client: &Client,
    conversation_id: impl Into<String>,
    up_to: Option<i64>,
) -> Result<()> {
    let request = MarkConversationReadRequest {
        conversation_id: conversation_id.into(),
        up_to,
    };
    client.chat_extras_conversation_mark_read(&request)
}

/// List chat folders in their server-defined order.
pub fn list_folders(client: &Client) -> Result<Vec<ChatFolder>> {
    client.chat_extras_folders_list()
}

/// Create a chat folder.
pub fn create_folder(client: &Client, name: impl Into<String>) -> Result<ChatFolder> {
    let request = CreateFolderRequest { name: name.into() };
    client.chat_extras_folder_create(&request)
}

/// Change a folder's name and/or position.
pub fn update_folder(
    client: &Client,
    folder_id: impl Into<String>,
    changes: FolderChanges,
) -> Result<()> {
    let request = UpdateFolderRequest {
        folder_id: folder_id.into(),
        name: changes.name,
        position: changes.position,
    };
    client.chat_extras_folder_update(&request)
}

/// Delete a chat folder.
pub fn delete_folder(client: &Client, folder_id: impl Into<String>) -> Result<()> {
    let request = FolderIdRequest {
        folder_id: folder_id.into(),
    };
    client.chat_extras_folder_delete(&request)
}

/// Add a conversation to a folder.
pub fn add_conversation_to_folder(
    client: &Client,
    folder_id: impl Into<String>,
    conversation_id: impl Into<String>,
) -> Result<()> {
    let request = FolderConversationRequest {
        folder_id: folder_id.into(),
        conversation_id: conversation_id.into(),
    };
    client.chat_extras_folder_add_conversation(&request)
}

/// Remove a conversation from a folder.
pub fn remove_conversation_from_folder(
    client: &Client,
    folder_id: impl Into<String>,
    conversation_id: impl Into<String>,
) -> Result<()> {
    let request = FolderConversationRequest {
        folder_id: folder_id.into(),
        conversation_id: conversation_id.into(),
    };
    client.chat_extras_folder_remove_conversation(&request)
}

/// List encrypted messages scheduled for later delivery.
pub fn list_scheduled(client: &Client) -> Result<Vec<ScheduledEnvelope>> {
    client.chat_extras_scheduled_list()
}

/// Cancel a scheduled envelope by its identifier.
pub fn cancel_scheduled(client: &Client, envelope_id: impl Into<String>) -> Result<()> {
    let request = ScheduledEnvelopeIdRequest {
        envelope_id: envelope_id.into(),
    };
    client.chat_extras_scheduled_cancel(&request)
}

/// Search accounts, groups, channels and bots.
///
/// The core API accepts `q` (not the web API adapter's `query`), with optional
/// `scope` and `limit`; omission lets the core apply its documented defaults.
pub fn global_search(
    client: &Client,
    q: impl Into<String>,
    scope: Option<SearchScope>,
    limit: Option<i64>,
) -> Result<Vec<SearchResult>> {
    let request = GlobalSearchRequest {
        q: q.into(),
        scope,
        limit,
    };
    client.chat_extras_global_search(&request)
}

/// List pinned messages in a conversation, newest pin first.
pub fn list_pinned(
    client: &Client,
    conversation_id: impl Into<String>,
) -> Result<Vec<PinnedMessage>> {
    let request = ConversationIdRequest {
        conversation_id: conversation_id.into(),
    };
    client.chat_extras_conversation_pins_list(&request)
}

/// Pin a logical message identifier in a conversation.
pub fn pin(
    client: &Client,
    conversation_id: impl Into<String>,
    message_id: impl Into<String>,
) -> Result<()> {
    let request = ConversationMessageRequest {
        conversation_id: conversation_id.into(),
        message_id: message_id.into(),
    };
    client.chat_extras_conversation_pin(&request)
}

/// Remove a logical message identifier from a conversation's pinned messages.
pub fn unpin(
    client: &Client,
    conversation_id: impl Into<String>,
    message_id: impl Into<String>,
) -> Result<()> {
    let request = ConversationMessageRequest {
        conversation_id: conversation_id.into(),
        message_id: message_id.into(),
    };
    client.chat_extras_conversation_unpin(&request)
}

/// Cursor for fetching a page of conversation history.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HistoryCursor {
    pub before: Option<i64>,
    pub before_id: Option<String>,
    pub limit: Option<i64>,
}

/// Editable fields for a chat folder.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FolderChanges {
    pub name: Option<String>,
    pub position: Option<i64>,
}

/// Whether global search matches identifiers only or all supported fields.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SearchScope {
    Identifiers,
    All,
}

/// A chat folder and its conversation membership.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChatFolder {
    pub id: String,
    pub name: String,
    pub position: i64,
    pub conversation_ids: Vec<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

/// One envelope queued for scheduled delivery.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ScheduledEnvelope {
    pub envelope_id: String,
    pub conversation_id: Option<String>,
    pub recipient_account_id: String,
    pub recipient_device_number: i64,
    pub envelope_type: i64,
    pub client_timestamp: i64,
    pub send_at: i64,
}

/// A global-search result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SearchResult {
    pub entity_type: String,
    pub entity_id: String,
    pub display_name: String,
    pub identifier: Option<String>,
    pub avatar_url: Option<String>,
}

/// One server-pinned logical message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PinnedMessage {
    pub message_id: String,
    pub pinned_by_account_id: String,
    pub pinned_at: i64,
}

#[derive(Debug, Clone, Serialize)]
struct CreateDirectRequest {
    peer_fh_number: String,
}

#[derive(Debug, Clone, Serialize)]
struct MuteConversationRequest {
    id: String,
    duration_seconds: Option<i64>,
}

#[derive(Debug, Clone, Serialize)]
struct ConversationHistoryRequest {
    conversation_id: String,
    before: Option<i64>,
    before_id: Option<String>,
    limit: Option<i64>,
}

#[derive(Debug, Clone, Serialize)]
struct MarkConversationReadRequest {
    conversation_id: String,
    up_to: Option<i64>,
}

#[derive(Debug, Clone, Serialize)]
struct CreateFolderRequest {
    name: String,
}

#[derive(Debug, Clone, Serialize)]
struct UpdateFolderRequest {
    folder_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    position: Option<i64>,
}

#[derive(Debug, Clone, Serialize)]
struct FolderIdRequest {
    folder_id: String,
}

#[derive(Debug, Clone, Serialize)]
struct FolderConversationRequest {
    folder_id: String,
    conversation_id: String,
}

#[derive(Debug, Clone, Serialize)]
struct ScheduledEnvelopeIdRequest {
    envelope_id: String,
}

#[derive(Debug, Clone, Serialize)]
struct GlobalSearchRequest {
    q: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    scope: Option<SearchScope>,
    #[serde(skip_serializing_if = "Option::is_none")]
    limit: Option<i64>,
}

#[derive(Debug, Clone, Serialize)]
struct ConversationMessageRequest {
    conversation_id: String,
    message_id: String,
}

#[derive(Debug, Clone, Serialize)]
struct ConversationPathRequest {
    id: String,
}

#[derive(Debug, Deserialize)]
struct LeftResponse {
    #[allow(dead_code)]
    left: bool,
}

#[derive(Debug, Deserialize)]
struct ArchivedResponse {
    #[allow(dead_code)]
    archived: bool,
}

#[derive(Debug, Deserialize)]
struct UnarchivedResponse {
    #[allow(dead_code)]
    unarchived: bool,
}

#[derive(Debug, Deserialize)]
struct MutedResponse {
    #[allow(dead_code)]
    muted: bool,
}

#[derive(Debug, Deserialize)]
struct UnmutedResponse {
    #[allow(dead_code)]
    unmuted: bool,
}
