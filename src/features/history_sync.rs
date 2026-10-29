//! Native account history transfer through `friendshub-core`.
//!
//! Message bodies and attachment key material stay in the core's SQLCipher
//! database. This module only builds typed requests and shapes stored rows for
//! the existing chat controller.

use friendshub_core::{Client, error::Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Deserialize)]
pub struct NativeHistoryItem {
    pub conversation_id: String,
    pub item_kind: String,
    pub item_id: String,
    pub transfer_id: String,
    pub item: Value,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LocalHistoryPageResult {
    pub conversation_id: String,
    pub messages_imported: usize,
    pub has_older: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HistoryReceiveResult {
    pub conversation_id: String,
    pub transfer_id: String,
    pub item_kind: String,
    pub item_id: String,
    pub item: Option<Value>,
    pub conversation_complete: bool,
    pub received_messages: i64,
}

#[derive(Serialize)]
struct HistoryRequest<'a> {
    target_device_number: i64,
    conversation_ids: &'a [String],
}

#[derive(Serialize)]
struct ConversationRequest<'a> {
    conversation_id: &'a str,
    limit: i64,
}

#[derive(Serialize)]
struct LocalPageRequest<'a> {
    conversation_id: &'a str,
    older: bool,
}

#[derive(Serialize)]
struct ReceiveChunkRequest<'a> {
    sender_account_id: &'a str,
    sender_device_number: i64,
    recipient_account_id: &'a str,
    recipient_device_number: i64,
    conversation_id: &'a str,
    chunk: &'a Value,
}

/// Ask another active device to send encrypted history for at most 200
/// conversations in this WebSocket request.
pub fn request_from_device(
    client: &Client,
    target_device_number: i64,
    conversation_ids: &[String],
) -> Result<Value> {
    client.ws_request_history(&HistoryRequest {
        target_device_number,
        conversation_ids,
    })
}

/// Read encrypted-database history records for rendering into the current
/// conversation. The bounded page keeps a large transfer from freezing chat.
pub fn list_items(client: &Client, conversation_id: &str) -> Result<Vec<NativeHistoryItem>> {
    client.history_sync_list_items(&ConversationRequest {
        conversation_id,
        limit: 5_000,
    })
}

/// Import a server history page and commit its Signal replay stage only after
/// the messages and attachment keys have been saved in SQLCipher.
pub fn load_local_page(
    client: &Client,
    conversation_id: &str,
    older: bool,
) -> Result<LocalHistoryPageResult> {
    client.history_sync_load_local_page(&LocalPageRequest {
        conversation_id,
        older,
    })
}

/// Persist one parsed, authenticated history fragment before its durable
/// source envelope event is acknowledged.
pub fn receive_chunk(
    client: &Client,
    event: &friendshub_core::events::Event,
) -> Result<HistoryReceiveResult> {
    let payload = &event.payload;
    let chunk = payload.get("decrypted_payload").ok_or_else(|| {
        friendshub_core::error::Error::InvalidPayload("history chunk is missing".into())
    })?;
    let request = ReceiveChunkRequest {
        sender_account_id: string(payload, "sender_account_id")?,
        sender_device_number: integer(payload, "sender_device_number")?,
        recipient_account_id: string(payload, "recipient_account_id")?,
        recipient_device_number: integer(payload, "recipient_device_number")?,
        conversation_id: string(payload, "conversation_id")?,
        chunk,
    };
    client.history_sync_receive_chunk(&request)
}

/// Respond to the core's ephemeral relay request. Core fetches and replays
/// server pages, durably stores them, commits ratchets, then sends history.
pub fn respond_to_request(client: &Client, payload: &Value) -> Result<Value> {
    client.history_sync_respond_request(payload)
}

fn string<'a>(value: &'a Value, key: &str) -> Result<&'a str> {
    value
        .get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| friendshub_core::error::Error::InvalidPayload(format!("{key} is missing")))
}

fn integer(value: &Value, key: &str) -> Result<i64> {
    value
        .get(key)
        .and_then(Value::as_i64)
        .ok_or_else(|| friendshub_core::error::Error::InvalidPayload(format!("{key} is missing")))
}
