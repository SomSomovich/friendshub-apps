//! Typed application operations for bot conversations and owned-bot settings.
//!
//! Bot conversations use their own history and send operations; a bot is not a
//! regular account or a direct conversation. All I/O is delegated to the native
//! `friendshub_core::Client` methods. These synchronous calls belong on a worker
//! thread, not the Slint event loop.

use friendshub_core::{Client, error::Result as CoreResult};
use serde::{Deserialize, Serialize};

/// The account UI permits creating at most this many bots, matching WEB.
/// The server remains authoritative for enforcing the account limit.
pub const MAX_MANAGED_BOTS: usize = 20;
pub const MAX_HISTORY_PAGE_SIZE: i64 = 200;

/// An owned bot shown in the settings list. This response contains only a
/// token prefix; the full credential is never returned by a list operation.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct ManagedBot {
    pub id: String,
    pub token_prefix: String,
    pub created_at: i64,
    pub updated_at: i64,
    pub handle: Option<String>,
}

/// An owned bot available to a channel's bot picker.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct OwnedBot {
    pub bot_id: String,
    pub handle: Option<String>,
    pub token_prefix: String,
    pub created_at: i64,
    pub has_webhook: bool,
}

/// Public information used for a bot conversation's heading.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct BotInfo {
    pub bot_id: String,
    pub owner_account_id: String,
    pub handle: Option<String>,
    pub created_at: i64,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BotMessageDirection {
    ToBot,
    FromBot,
}

/// A stored bot message returned by history and send operations.
///
/// The core normalizes the backend's deletion timestamp into `is_deleted`.
/// Reply markup remains opaque JSON so the UI can display the same inline
/// keyboard labels as WEB without inventing button actions.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct BotMessage {
    pub id: String,
    pub bot_id: String,
    pub sender_account_id: String,
    pub direction: BotMessageDirection,
    pub text: String,
    pub reply_to_id: Option<String>,
    pub created_at: i64,
    pub reply_markup: Option<String>,
    #[serde(default)]
    pub is_deleted: bool,
    #[serde(default)]
    pub edited_at: Option<i64>,
    #[serde(default)]
    pub deleted_at: Option<i64>,
}

/// Pagination fields are flattened into the native core request. History is
/// newest first; continue with the last row's `created_at` and `id`. The core
/// validates the optional limit as `1..=200`.
#[derive(Clone, Debug, Default, Serialize)]
pub struct BotHistoryQuery {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub before: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub before_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit: Option<i64>,
}

/// Credential returned once after creating a bot. Keep it in transient UI
/// state for the copy dialog, then discard it; never log or persist it in
/// account settings. This type intentionally does not implement `Debug` or
/// `Serialize`.
#[derive(Deserialize)]
pub struct BotCredential {
    pub bot_id: String,
    pub token: String,
    pub token_prefix: String,
}

/// Replacement credential returned once after token rotation. It has the
/// same transient handling requirement as [`BotCredential`].
#[derive(Deserialize)]
pub struct RotatedBotToken {
    pub token: String,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct BotHandle {
    pub handle: String,
    pub handle_normalized: String,
}

/// Fetch the bots managed by the signed-in account for the settings panel.
pub fn list(client: &Client) -> CoreResult<Vec<ManagedBot>> {
    client.bots_list()
}

/// Fetch owned bots for displays such as the channel bot picker.
pub fn list_owned(client: &Client) -> CoreResult<Vec<OwnedBot>> {
    client.bots_list_my_bots()
}

/// Create a bot and return its one-time credential for the copy dialog.
pub fn create(client: &Client) -> CoreResult<BotCredential> {
    client.bots_create()
}

/// Change a managed bot's address. The caller supplies the normalized handle;
/// availability feedback and input validation follow the shared handle flow.
pub fn set_handle(client: &Client, bot_id: &str, handle: &str) -> CoreResult<BotHandle> {
    client.bots_set_profile(&SetBotHandleRequest { bot_id, handle })
}

/// Revoke the previous credential and return its replacement once.
pub fn rotate_token(client: &Client, bot_id: &str) -> CoreResult<RotatedBotToken> {
    client.bots_rotate_token(&BotIdRequest { id: bot_id })
}

/// Delete a managed bot. The UI should obtain confirmation before calling.
pub fn delete(client: &Client, bot_id: &str) -> CoreResult<()> {
    client
        .bots_delete::<_, DeletedBotResponse>(&BotIdRequest { id: bot_id })
        .map(|_| ())
}

pub fn info(client: &Client, bot_id: &str) -> CoreResult<BotInfo> {
    client.bots_get_bot_info(&BotIdRequest { id: bot_id })
}

pub fn history(
    client: &Client,
    bot_id: &str,
    query: BotHistoryQuery,
) -> CoreResult<Vec<BotMessage>> {
    client.bots_get_history(&BotHistoryRequest { id: bot_id, query })
}

/// Send to a bot and return the stored message for immediate insertion into
/// its transcript. A later bot reply arrives through the `bot_message` event.
pub fn send(
    client: &Client,
    bot_id: &str,
    text: &str,
    reply_to_message_id: Option<&str>,
) -> CoreResult<BotMessage> {
    client.bots_send_to_bot(&SendBotMessageRequest {
        bot_id,
        text,
        reply_to_message_id,
    })
}

/// Payload of the core's `bot_message` event. This frame carries the received
/// bot reply rather than an encrypted conversation envelope.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct IncomingBotMessage {
    pub message_id: String,
    pub bot_id: String,
    pub account_id: String,
    pub text: String,
    pub reply_to_message_id: Option<String>,
    pub reply_markup: Option<String>,
    pub created_at: i64,
}

impl IncomingBotMessage {
    /// Convert a reply into the transcript shape used by REST history. The
    /// event's account ID identifies the account that owns this bot thread.
    pub fn into_message(self) -> BotMessage {
        BotMessage {
            id: self.message_id,
            bot_id: self.bot_id,
            sender_account_id: self.account_id,
            direction: BotMessageDirection::FromBot,
            text: self.text,
            reply_to_id: self.reply_to_message_id,
            created_at: self.created_at,
            reply_markup: self.reply_markup,
            is_deleted: false,
            edited_at: None,
            deleted_at: None,
        }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct BotMessageEdited {
    pub message_id: String,
    pub text: String,
    pub reply_markup: Option<String>,
    pub edited_at: i64,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct BotMessageDeleted {
    pub message_id: String,
    pub deleted_at: i64,
}

#[derive(Serialize)]
struct BotIdRequest<'a> {
    id: &'a str,
}

#[derive(Serialize)]
struct SetBotHandleRequest<'a> {
    bot_id: &'a str,
    handle: &'a str,
}

#[derive(Serialize)]
struct BotHistoryRequest<'a> {
    id: &'a str,
    #[serde(flatten)]
    query: BotHistoryQuery,
}

#[derive(Serialize)]
struct SendBotMessageRequest<'a> {
    bot_id: &'a str,
    text: &'a str,
    reply_to_message_id: Option<&'a str>,
}

#[derive(Deserialize)]
struct DeletedBotResponse {
    #[serde(rename = "deleted")]
    _deleted: bool,
}
