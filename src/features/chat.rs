//! Typed native operations for conversation history, messages, and polls.
//!
//! Networking and cryptography stay inside `friendshub-core`. The DTOs here
//! use the dispatcher operation's snake_case JSON fields. Plaintext-bearing
//! DTOs are intentionally not `Debug` so they are not accidentally exposed by
//! routine diagnostic formatting.

use friendshub_core::{Client, error::Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

// -----------------------------------------------------------------------------
// Encrypted conversation history and messages
// -----------------------------------------------------------------------------

/// Cursor for the current device's conversation-envelope history.
///
/// The core returns newest-first pages. A new device must use the core's
/// history-replay operations instead of this device-bound endpoint.
#[derive(Clone, Serialize)]
pub struct HistoryPageRequest {
    pub conversation_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub before: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub before_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit: Option<i64>,
}

/// Wire shape returned by `chat_extras_conversation_envelopes`.
#[derive(Clone, Deserialize, Serialize)]
pub struct HistoryEnvelope {
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

#[derive(Deserialize, Serialize)]
pub struct DecryptResponse {
    /// Hex-encoded plaintext returned by the core; do not write it to logs.
    pub plaintext_hex: String,
}

/// A history row paired with the plaintext hex produced by the core.
#[derive(Deserialize, Serialize)]
pub struct DecryptedHistoryEnvelope {
    pub envelope: HistoryEnvelope,
    pub plaintext_hex: String,
}

pub fn fetch_history_page(
    client: &Client,
    request: &HistoryPageRequest,
) -> Result<Vec<HistoryEnvelope>> {
    client.chat_extras_conversation_envelopes(request)
}

/// Decrypt one history envelope using the account/device crypto state in core.
pub fn decrypt_envelope(client: &Client, envelope: &HistoryEnvelope) -> Result<DecryptResponse> {
    client.messages_decrypt(envelope)
}

/// Fetch one page and decrypt each envelope in the order returned by core.
///
/// Calls are kept sequential because each decrypt advances per-device crypto
/// state. The result contains plaintext hex for the app's message-payload
/// decoder; this operation does not log or persist plaintext.
pub fn fetch_decrypted_history_page(
    client: &Client,
    request: &HistoryPageRequest,
) -> Result<Vec<DecryptedHistoryEnvelope>> {
    fetch_history_page(client, request)?
        .into_iter()
        .map(|envelope| {
            let decrypted = decrypt_envelope(client, &envelope)?;
            Ok(DecryptedHistoryEnvelope {
                envelope,
                plaintext_hex: decrypted.plaintext_hex,
            })
        })
        .collect()
}

/// Request for core's high-level pairwise message fanout.
///
/// `plaintext_hex` is the already serialized message payload. Core owns
/// encryption, device fanout, envelope upload, and optional own-device sync.
#[derive(Serialize)]
pub struct SendMessageRequest {
    pub recipient_account_id: String,
    pub plaintext_hex: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub device_number: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub conversation_id: Option<String>,
    #[serde(default)]
    pub refresh_devices: bool,
    #[serde(default = "default_true")]
    pub sync: bool,
}

#[derive(Deserialize, Serialize)]
pub struct SentEnvelope {
    pub device_number: i64,
    pub envelope_id: String,
    pub is_prekey_message: bool,
    pub ciphertext_len: usize,
}

#[derive(Deserialize, Serialize)]
pub struct DeviceError {
    pub device_number: i64,
    pub error: String,
}

#[derive(Deserialize, Serialize)]
pub struct SendMessageResponse {
    pub envelopes: Vec<SentEnvelope>,
    pub device_errors: Vec<DeviceError>,
    pub sync_envelopes: Vec<SentEnvelope>,
    pub sync_errors: Vec<DeviceError>,
}

pub fn send_message(client: &Client, request: &SendMessageRequest) -> Result<SendMessageResponse> {
    client.messages_send_message(request)
}

/// Request for core's group SenderKey fanout.
#[derive(Serialize)]
pub struct SendGroupMessageRequest {
    pub conversation_id: String,
    pub plaintext_hex: String,
    #[serde(default = "default_true")]
    pub sync: bool,
}

#[derive(Deserialize, Serialize)]
pub struct GroupSendEnvelope {
    pub recipient_account_id: String,
    pub device_number: i64,
    pub envelope_id: String,
}

#[derive(Deserialize, Serialize)]
pub struct SendGroupMessageResponse {
    pub ciphertext_len: usize,
    pub envelopes: Vec<GroupSendEnvelope>,
    pub device_errors: Vec<Value>,
    pub sync_envelopes: Vec<GroupSendEnvelope>,
    pub sync_errors: Vec<Value>,
}

pub fn send_group_message(
    client: &Client,
    request: &SendGroupMessageRequest,
) -> Result<SendGroupMessageResponse> {
    client.groups_send_send_group_message(request)
}

// -----------------------------------------------------------------------------
// Encrypted message actions
// -----------------------------------------------------------------------------

#[derive(Serialize)]
pub struct EditMessageRequest {
    pub conversation_id: String,
    pub target_message_id: String,
    pub new_text: String,
}

#[derive(Clone, Serialize)]
pub struct DeleteMessageRequest {
    pub conversation_id: String,
    pub target_message_id: String,
}

#[derive(Clone, Serialize)]
pub struct SetMessageReactionRequest {
    pub conversation_id: String,
    pub target_message_id: String,
    /// `None` removes this device's reaction, matching the WEB envelope format.
    pub emoji: Option<String>,
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ActionEnvelope {
    pub recipient_account_id: String,
    pub device_number: i64,
    pub envelope_id: String,
    pub is_prekey_message: bool,
    pub ciphertext_len: usize,
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MessageActionResponse {
    pub envelopes: Vec<ActionEnvelope>,
    pub action_kind: String,
    pub target_message_id: String,
    pub skipped_recipient_devices: Vec<i64>,
    pub skipped_own_devices: Vec<i64>,
}

pub fn edit_message(
    client: &Client,
    request: &EditMessageRequest,
) -> Result<MessageActionResponse> {
    client.message_actions_send_edit(request)
}

pub fn delete_message(
    client: &Client,
    request: &DeleteMessageRequest,
) -> Result<MessageActionResponse> {
    client.message_actions_send_delete(request)
}

pub fn set_message_reaction(
    client: &Client,
    request: &SetMessageReactionRequest,
) -> Result<MessageActionResponse> {
    client.message_actions_send_reaction(request)
}

// -----------------------------------------------------------------------------
// Conversation pins
// -----------------------------------------------------------------------------

#[derive(Clone, Serialize)]
pub struct ConversationPinRequest {
    pub conversation_id: String,
    pub message_id: String,
}

#[derive(Deserialize, Serialize)]
pub struct ConversationPin {
    pub message_id: String,
    pub pinned_by_account_id: String,
    pub pinned_at: i64,
}

#[derive(Serialize)]
struct ConversationIdRequest<'a> {
    conversation_id: &'a str,
}

pub fn list_conversation_pins(
    client: &Client,
    conversation_id: &str,
) -> Result<Vec<ConversationPin>> {
    client.chat_extras_conversation_pins_list(&ConversationIdRequest { conversation_id })
}

pub fn pin_message(client: &Client, request: &ConversationPinRequest) -> Result<()> {
    client.chat_extras_conversation_pin(request)
}

pub fn unpin_message(client: &Client, request: &ConversationPinRequest) -> Result<()> {
    client.chat_extras_conversation_unpin(request)
}

#[derive(Clone, Serialize)]
pub struct MarkConversationReadRequest {
    pub conversation_id: String,
    /// Server timestamp through which the conversation has been read.
    /// Omit it to let the server use its current time.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub up_to: Option<i64>,
}

#[derive(Deserialize, Serialize)]
pub struct ConversationReadMarker {
    pub account_id: String,
    pub last_read_at: i64,
}

pub fn list_conversation_reads(
    client: &Client,
    conversation_id: &str,
) -> Result<Vec<ConversationReadMarker>> {
    client.chat_extras_conversation_reads(&ConversationIdRequest { conversation_id })
}

pub fn mark_conversation_read(
    client: &Client,
    request: &MarkConversationReadRequest,
) -> Result<()> {
    client.chat_extras_conversation_mark_read(request)
}

#[derive(Clone, Serialize)]
pub struct ContactSearchRequest {
    pub q: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit: Option<i64>,
}

#[derive(Deserialize, Serialize)]
pub struct ContactSearchResult {
    pub target_account_id: String,
    pub local_username: String,
    pub user_id: i64,
    pub fh_number: String,
    pub username: String,
    pub avatar_url: Option<String>,
}

pub fn search_contacts(
    client: &Client,
    request: &ContactSearchRequest,
) -> Result<Vec<ContactSearchResult>> {
    client.chat_extras_contact_search(request)
}

#[derive(Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SearchScope {
    All,
    Identifiers,
}

#[derive(Clone, Serialize)]
pub struct GlobalSearchRequest {
    pub q: String,
    pub scope: SearchScope,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit: Option<i64>,
}

#[derive(Deserialize, Serialize)]
pub struct GlobalSearchResult {
    pub entity_type: String,
    pub entity_id: String,
    pub display_name: String,
    pub identifier: Option<String>,
    pub avatar_url: Option<String>,
}

pub fn search_global(
    client: &Client,
    request: &GlobalSearchRequest,
) -> Result<Vec<GlobalSearchResult>> {
    client.chat_extras_global_search(request)
}

// -----------------------------------------------------------------------------
// Polls
// -----------------------------------------------------------------------------

#[derive(Serialize, Deserialize)]
pub struct PollMetadata {
    pub title: String,
    pub description: String,
    pub options: Vec<PollOption>,
}

#[derive(Serialize, Deserialize)]
pub struct PollOption {
    pub id: String,
    pub text: String,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PollType {
    Regular,
    Quiz,
}

#[derive(Serialize, Deserialize)]
pub struct PollSettings {
    pub poll_type: PollType,
    pub allows_multiple: bool,
    pub allows_revoting: bool,
    pub allows_user_options: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub open_period_seconds: Option<i64>,
    pub correct_option_id: Option<String>,
}

#[derive(Serialize)]
pub struct CreateConversationPollRequest {
    pub conversation_id: String,
    pub metadata: PollMetadata,
    pub settings: PollSettings,
}

#[derive(Deserialize, Serialize)]
pub struct PollCount {
    pub option_id: String,
    pub count: i64,
}

#[derive(Deserialize, Serialize)]
pub struct PollExtraOption {
    pub option_id: String,
    pub encrypted_meta: String,
    pub proposed_by_account_id: Option<String>,
    pub proposed_by_device_number: Option<i64>,
    pub created_at: i64,
}

#[derive(Deserialize, Serialize)]
pub struct PollItem {
    pub id: String,
    pub revision: i64,
    pub conversation_id: String,
    pub creator_account_id: Option<String>,
    pub creator_device_number: Option<i64>,
    pub poll_type: String,
    pub allows_multiple: bool,
    pub allows_revoting: bool,
    pub allows_user_options: bool,
    pub encrypted_meta: String,
    pub closes_at: Option<i64>,
    pub closed_at: Option<i64>,
    pub created_at: i64,
    pub counts: Vec<PollCount>,
    pub total_voters: i64,
    pub my_votes: Vec<String>,
    pub correct_option_id: Option<String>,
    pub extra_options: Vec<PollExtraOption>,
    pub deleted_option_ids: Vec<String>,
}

#[derive(Deserialize, Serialize)]
pub struct CreatedPoll {
    pub item: PollItem,
    pub metadata: PollMetadata,
    pub history_synced: bool,
}

pub fn create_conversation_poll(
    client: &Client,
    request: &CreateConversationPollRequest,
) -> Result<CreatedPoll> {
    client.poll_crypto_create_conversation_poll(request)
}

#[derive(Clone, Serialize)]
pub struct PollHistoryRequest {
    pub conversation_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub before: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub before_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit: Option<i64>,
}

#[derive(Clone, Serialize)]
pub struct PollIdRequest {
    pub poll_id: String,
}

#[derive(Clone, Serialize)]
pub struct PollVoteRequest {
    pub poll_id: String,
    pub option_ids: Vec<String>,
}

#[derive(Clone, Serialize)]
pub struct PollOptionMutationRequest {
    pub poll_id: String,
    pub option_id: String,
}

/// `encrypted_meta` is sender-key ciphertext produced by core's poll crypto.
#[derive(Clone, Serialize)]
pub struct AddPollOptionRequest {
    pub poll_id: String,
    pub option_id: String,
    pub encrypted_meta: String,
}

pub fn list_conversation_polls(
    client: &Client,
    request: &PollHistoryRequest,
) -> Result<Vec<PollItem>> {
    client.chat_extras_conversation_polls_list(request)
}

pub fn get_poll(client: &Client, request: &PollIdRequest) -> Result<PollItem> {
    client.chat_extras_poll_get(request)
}

pub fn vote_poll(client: &Client, request: &PollVoteRequest) -> Result<PollItem> {
    client.chat_extras_poll_vote(request)
}

pub fn stop_poll(client: &Client, request: &PollIdRequest) -> Result<()> {
    client.chat_extras_poll_stop(request)
}

pub fn delete_poll(client: &Client, request: &PollIdRequest) -> Result<()> {
    client.chat_extras_poll_delete(request)
}

pub fn add_poll_option(client: &Client, request: &AddPollOptionRequest) -> Result<()> {
    client.chat_extras_poll_add_option(request)
}

pub fn delete_poll_option(client: &Client, request: &PollOptionMutationRequest) -> Result<()> {
    client.chat_extras_poll_delete_option(request)
}

#[derive(Serialize)]
pub struct EncryptPollPayloadRequest {
    pub conversation_id: String,
    pub payload: Value,
}

#[derive(Serialize)]
pub struct EncryptPollOptionRequest {
    pub conversation_id: String,
    pub text: String,
}

#[derive(Serialize)]
pub struct DecryptPollPayloadRequest {
    pub sender_account_id: String,
    pub sender_device_number: i64,
    pub encrypted_meta: String,
}

pub fn encrypt_poll_payload(
    client: &Client,
    request: &EncryptPollPayloadRequest,
) -> Result<String> {
    client.poll_crypto_encrypt_poll_payload(request)
}

pub fn encrypt_poll_option(client: &Client, request: &EncryptPollOptionRequest) -> Result<String> {
    client.poll_crypto_encrypt_poll_option(request)
}

pub fn decrypt_poll_payload(client: &Client, request: &DecryptPollPayloadRequest) -> Result<Value> {
    client.poll_crypto_decrypt_poll_payload(request)
}

#[allow(dead_code)] // Referenced by serde attributes, which are not visible to dead-code analysis.
fn default_true() -> bool {
    true
}
