//! Native call signaling, encoded media, and encrypted file transfers.
//!
//! All functions call the typed synchronous `friendshub_core::Client` API and
//! belong on a worker thread. The core routes encoded Opus/VP8 frames; capture,
//! encoding, decoding, playback, and track negotiation are host responsibilities.
//! Transfers accept native filesystem paths. The current core returns a final
//! transfer result and does not publish byte or chunk progress notifications.

use std::path::Path;

use friendshub_core::Client;
use friendshub_core::error::{Error, Result};
use friendshub_core::events::Event;
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CallDirection {
    Incoming,
    Outgoing,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CallStatus {
    Completed,
    Missed,
    Rejected,
    Cancelled,
    Failed,
}

#[derive(Clone, Debug, Deserialize)]
pub struct CallRecord {
    pub id: String,
    pub peer_account_id: String,
    pub direction: CallDirection,
    pub status: CallStatus,
    pub started_at: i64,
    pub ended_at: Option<i64>,
}

#[derive(Serialize)]
pub struct CallRecordInput<'a> {
    pub peer_account_id: &'a str,
    pub direction: CallDirection,
    pub status: CallStatus,
    pub started_at: i64,
    pub ended_at: Option<i64>,
}

#[derive(Serialize)]
struct CallListRequest<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    peer_account_id: Option<&'a str>,
    limit: i64,
}

pub fn list_calls(
    client: &Client,
    peer_account_id: Option<&str>,
    limit: i64,
) -> Result<Vec<CallRecord>> {
    client.calls_list(&CallListRequest {
        peer_account_id,
        limit,
    })
}

pub fn record_call(client: &Client, input: &CallRecordInput<'_>) -> Result<CallRecord> {
    client.calls_record(input)
}

#[derive(Serialize)]
struct IdRequest<'a> {
    id: &'a str,
}

pub fn delete_call(client: &Client, id: &str) -> Result<()> {
    #[derive(Deserialize)]
    struct Response {
        deleted: bool,
    }
    let response: Response = client.calls_delete_one(&IdRequest { id })?;
    confirmed(response.deleted, "call deletion")
}

pub fn clear_call_history(client: &Client) -> Result<()> {
    #[derive(Deserialize)]
    struct Response {
        cleared: bool,
    }
    let response: Response = client.calls_clear()?;
    confirmed(response.cleared, "call history clearing")
}

// ICE credentials and SDP may contain sensitive connection information. These
// DTOs intentionally do not implement Debug.
#[derive(Clone, Deserialize)]
pub struct IceServer {
    pub urls: Vec<String>,
    pub username: Option<String>,
    pub credential: Option<String>,
}

#[derive(Clone, Deserialize)]
pub struct IceServers {
    pub ice_servers: Vec<IceServer>,
    pub ttl_seconds: u64,
}

pub fn ice_servers(client: &Client) -> Result<IceServers> {
    client.webrtc_ice_servers()
}

/// W3C RTCIceCandidateInit shape consumed by the native core's RTC stack.
#[derive(Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct IceCandidate {
    pub candidate: String,
    pub sdp_mid: Option<String>,
    #[serde(rename = "sdpMLineIndex")]
    pub sdp_mline_index: Option<u16>,
    pub username_fragment: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
}

#[derive(Serialize)]
struct CallIdRequest<'a> {
    call_id: &'a str,
}

#[derive(Serialize)]
struct RemoteSdpRequest<'a> {
    call_id: &'a str,
    remote_sdp: &'a str,
}

#[derive(Deserialize)]
pub struct SessionDescription {
    pub call_id: String,
    pub sdp: String,
}

/// Creates or renegotiates a local offer without sending it to the peer.
pub fn create_offer(client: &Client, call_id: &str) -> Result<SessionDescription> {
    client.webrtc_create_offer(&CallIdRequest { call_id })
}

/// Applies the remote offer and creates a local answer without sending it.
pub fn accept_offer(
    client: &Client,
    call_id: &str,
    remote_sdp: &str,
) -> Result<SessionDescription> {
    client.webrtc_accept_offer(&RemoteSdpRequest {
        call_id,
        remote_sdp,
    })
}

#[derive(Deserialize)]
struct OkResponse {
    ok: bool,
}

pub fn apply_answer(client: &Client, call_id: &str, remote_sdp: &str) -> Result<()> {
    let response: OkResponse = client.webrtc_apply_answer(&RemoteSdpRequest {
        call_id,
        remote_sdp,
    })?;
    confirmed(response.ok, "remote answer application")
}

#[derive(Serialize)]
struct CandidateRequest<'a> {
    call_id: &'a str,
    candidate: &'a IceCandidate,
}

pub fn add_ice_candidate(client: &Client, call_id: &str, candidate: &IceCandidate) -> Result<()> {
    let response: OkResponse = client.webrtc_add_ice(&CandidateRequest { call_id, candidate })?;
    confirmed(response.ok, "remote ICE candidate application")
}

#[derive(Serialize)]
struct PeerCallRequest<'a> {
    recipient_account_id: &'a str,
    call_id: &'a str,
}

#[derive(Deserialize)]
pub struct SignaledSession {
    pub call_id: String,
    pub sdp: String,
    pub envelopes_sent: usize,
}

/// Creates an offer and sends it through Signal-encrypted envelopes.
/// This operation does not add audio/video tracks or capture devices.
pub fn initiate_call(
    client: &Client,
    recipient_account_id: &str,
    call_id: &str,
) -> Result<SignaledSession> {
    client.webrtc_initiate_call(&PeerCallRequest {
        recipient_account_id,
        call_id,
    })
}

#[derive(Serialize)]
struct AcceptIncomingRequest<'a> {
    recipient_account_id: &'a str,
    call_id: &'a str,
    remote_sdp: &'a str,
}

/// Applies an offer and sends the answer through encrypted envelopes.
pub fn accept_call(
    client: &Client,
    recipient_account_id: &str,
    call_id: &str,
    remote_sdp: &str,
) -> Result<SignaledSession> {
    client.webrtc_accept_call(&AcceptIncomingRequest {
        recipient_account_id,
        call_id,
        remote_sdp,
    })
}

#[derive(Deserialize)]
pub struct SignalDelivery {
    pub envelopes_sent: usize,
}

#[derive(Serialize)]
struct SendCandidateRequest<'a> {
    recipient_account_id: &'a str,
    call_id: &'a str,
    candidate: &'a IceCandidate,
}

pub fn send_ice_candidate(
    client: &Client,
    recipient_account_id: &str,
    call_id: &str,
    candidate: &IceCandidate,
) -> Result<SignalDelivery> {
    client.webrtc_send_ice(&SendCandidateRequest {
        recipient_account_id,
        call_id,
        candidate,
    })
}

#[derive(Serialize)]
struct EndCallRequest<'a> {
    recipient_account_id: &'a str,
    call_id: &'a str,
    reason: Option<&'a str>,
}

/// Closes the local peer connection and sends an encrypted hangup signal.
pub fn hang_up(
    client: &Client,
    recipient_account_id: &str,
    call_id: &str,
    reason: Option<&str>,
) -> Result<SignalDelivery> {
    client.webrtc_hangup(&EndCallRequest {
        recipient_account_id,
        call_id,
        reason,
    })
}

/// Sends an encrypted rejection for an offer not accepted by this device.
pub fn reject_call(
    client: &Client,
    recipient_account_id: &str,
    call_id: &str,
    reason: Option<&str>,
) -> Result<SignalDelivery> {
    client.webrtc_reject(&EndCallRequest {
        recipient_account_id,
        call_id,
        reason,
    })
}

/// Closes only the local peer connection; it sends no hangup signal.
pub fn close_call(client: &Client, call_id: &str) -> Result<()> {
    let response: OkResponse = client.webrtc_close_call(&CallIdRequest { call_id })?;
    confirmed(response.ok, "local peer connection closing")
}

pub fn active_calls(client: &Client) -> Result<Vec<String>> {
    client.webrtc_list_active()
}

#[derive(Serialize)]
struct TrackRequest<'a> {
    call_id: &'a str,
    label: &'a str,
}

#[derive(Deserialize)]
pub struct AddedTrack {
    pub added: bool,
    pub mime: String,
}

/// Adds an outgoing VP8 track. Its existence requires SDP renegotiation.
pub fn add_video_track(client: &Client, call_id: &str, label: &str) -> Result<AddedTrack> {
    client.webrtc_video_add_video_track(&TrackRequest { call_id, label })
}

/// Adds an outgoing Opus track. Its existence requires SDP renegotiation.
pub fn add_audio_track(client: &Client, call_id: &str, label: &str) -> Result<AddedTrack> {
    client.webrtc_video_add_audio_track(&TrackRequest { call_id, label })
}

#[derive(Serialize)]
struct WriteFrameRequest<'a> {
    call_id: &'a str,
    data_base64: &'a str,
    duration_ms: u64,
}

#[derive(Deserialize)]
struct WrittenResponse {
    written: bool,
}

/// Sends one already encoded VP8 frame, expressed as standard Base64.
pub fn write_video_frame(
    client: &Client,
    call_id: &str,
    data_base64: &str,
    duration_ms: u64,
) -> Result<()> {
    let response: WrittenResponse = client.webrtc_video_write_video_frame(&WriteFrameRequest {
        call_id,
        data_base64,
        duration_ms,
    })?;
    confirmed(response.written, "encoded video frame writing")
}

/// Sends one already encoded Opus frame, expressed as standard Base64.
pub fn write_audio_frame(
    client: &Client,
    call_id: &str,
    data_base64: &str,
    duration_ms: u64,
) -> Result<()> {
    let response: WrittenResponse = client.webrtc_video_write_audio_frame(&WriteFrameRequest {
        call_id,
        data_base64,
        duration_ms,
    })?;
    confirmed(response.written, "encoded audio frame writing")
}

#[derive(Serialize)]
struct ReadFrameRequest<'a> {
    call_id: &'a str,
    timeout_ms: u64,
}

#[derive(Deserialize)]
struct FrameResponse {
    timeout: bool,
    size: Option<usize>,
    data_base64: Option<String>,
}

/// Compressed data returned by core, awaiting the host codec/renderer.
pub struct EncodedFrame {
    pub size: usize,
    pub data_base64: String,
}

pub enum FrameRead {
    Timeout,
    Frame(EncodedFrame),
}

fn frame_read(response: FrameResponse) -> Result<FrameRead> {
    if response.timeout {
        return Ok(FrameRead::Timeout);
    }
    match (response.size, response.data_base64) {
        (Some(size), Some(data_base64)) => Ok(FrameRead::Frame(EncodedFrame { size, data_base64 })),
        _ => Err(Error::Internal(
            "core returned an incomplete encoded frame".into(),
        )),
    }
}

pub fn read_video_frame(client: &Client, call_id: &str, timeout_ms: u64) -> Result<FrameRead> {
    frame_read(client.webrtc_video_read_video_frame(&ReadFrameRequest {
        call_id,
        timeout_ms,
    })?)
}

pub fn read_audio_frame(client: &Client, call_id: &str, timeout_ms: u64) -> Result<FrameRead> {
    frame_read(client.webrtc_video_read_audio_frame(&ReadFrameRequest {
        call_id,
        timeout_ms,
    })?)
}

pub fn create_data_channel(client: &Client, call_id: &str, label: &str) -> Result<()> {
    let response: OkResponse =
        client.webrtc_create_data_channel(&TrackRequest { call_id, label })?;
    confirmed(response.ok, "data channel creation")
}

#[derive(Serialize)]
struct SendDataRequest<'a> {
    call_id: &'a str,
    label: &'a str,
    data_base64: &'a str,
    is_text: bool,
}

/// The channel must have emitted `data_channel_opened` before writing.
pub fn send_data_channel_data(
    client: &Client,
    call_id: &str,
    label: &str,
    data_base64: &str,
    is_text: bool,
) -> Result<()> {
    #[derive(Deserialize)]
    struct Response {
        sent: bool,
    }
    let response: Response = client.webrtc_data_send_channel_data(&SendDataRequest {
        call_id,
        label,
        data_base64,
        is_text,
    })?;
    confirmed(response.sent, "data channel writing")
}

#[derive(Clone, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CallSignal {
    Offer {
        call_id: String,
        sdp: String,
    },
    Answer {
        call_id: String,
        sdp: String,
    },
    Ice {
        call_id: String,
        candidate: IceCandidate,
    },
    Hangup {
        call_id: String,
        reason: Option<String>,
    },
    Reject {
        call_id: String,
        reason: Option<String>,
    },
}

#[derive(Clone, Deserialize)]
pub struct ReceivedCallSignal {
    pub envelope_id: String,
    pub sender_account_id: String,
    pub sender_device_number: i64,
    pub conversation_id: Option<String>,
    pub client_timestamp: i64,
    #[serde(rename = "decrypted_payload")]
    pub signal: CallSignal,
}

/// Decode an incoming semantic event. A durable event must only be acked by
/// its application owner after it has stored/applied this result.
pub fn received_call_signal(event: &Event) -> Result<Option<ReceivedCallSignal>> {
    if event.kind != "call_signal_received" {
        return Ok(None);
    }
    serde_json::from_value(event.payload.clone())
        .map(Some)
        .map_err(Error::from)
}

/// Events emitted by the native peer connection. Media track events identify
/// the track; compressed frames are read with `read_audio_frame`/`read_video_frame`.
#[derive(Clone, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum WebRtcEvent {
    IceCandidate {
        call_id: String,
        candidate: IceCandidate,
    },
    SignalingStateChanged {
        call_id: String,
        state: String,
    },
    IceConnectionStateChanged {
        call_id: String,
        state: String,
    },
    IceGatheringStateChanged {
        call_id: String,
        state: String,
    },
    ConnectionStateChanged {
        call_id: String,
        state: String,
    },
    DataChannelOpened {
        call_id: String,
        label: String,
    },
    DataChannelClosed {
        call_id: String,
        label: String,
    },
    DataChannelMessage {
        call_id: String,
        label: String,
        data_base64: String,
        is_text: bool,
    },
    TrackReceived {
        call_id: String,
        track_id: String,
        kind: String,
    },
    Closed {
        call_id: String,
    },
}

pub fn web_rtc_event(event: &Event) -> Result<Option<WebRtcEvent>> {
    if event.kind != "webrtc_event" {
        return Ok(None);
    }
    serde_json::from_value(event.payload.clone())
        .map(Some)
        .map_err(Error::from)
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AttachmentKind {
    Attachment,
    Thumbnail,
}

#[derive(Deserialize)]
pub struct ChunkPlan {
    pub chunk_size: u64,
    pub chunk_sizes: Vec<u64>,
    pub chunk_count: usize,
}

pub fn recommend_attachment_chunks(client: &Client, total_size: u64) -> Result<ChunkPlan> {
    #[derive(Serialize)]
    struct Request {
        total_size: u64,
    }
    client.attachments_recommend(&Request { total_size })
}

#[derive(Serialize)]
struct UploadRequest<'a> {
    file_path: &'a str,
    conversation_id: &'a str,
    kind: AttachmentKind,
}

/// Contains decryption material. Persist privately and forward through the
/// encrypted key envelope; never include this value in logs or server metadata.
#[derive(Clone, Deserialize, Serialize)]
pub struct AttachmentUpload {
    pub attachment_id: String,
    pub key_hex: String,
    pub base_nonce_hex: String,
    pub plaintext_size: u64,
    pub encrypted_size: u64,
    pub chunk_count: usize,
}

/// Encrypts and uploads an absolute filesystem file through the core.
/// Mobile content URIs must first be resolved/copied by the platform adapter.
pub fn upload_attachment(
    client: &Client,
    file_path: &Path,
    conversation_id: &str,
    kind: AttachmentKind,
) -> Result<AttachmentUpload> {
    client.attachments_upload(&UploadRequest {
        file_path: native_path(file_path)?,
        conversation_id,
        kind,
    })
}

#[derive(Serialize)]
struct DownloadRequest<'a> {
    attachment_id: &'a str,
    output_path: &'a str,
    key_hex: &'a str,
    base_nonce_hex: &'a str,
}

#[derive(Deserialize)]
pub struct AttachmentDownload {
    pub output_path: String,
    pub plaintext_size: u64,
    pub chunk_count: usize,
}

/// Decrypts to the requested file. The core creates/truncates that path; the
/// host must choose a private temporary destination when it needs an atomic
/// replacement and handle cleanup after a failed transfer.
pub fn download_attachment(
    client: &Client,
    attachment_id: &str,
    output_path: &Path,
    key_hex: &str,
    base_nonce_hex: &str,
) -> Result<AttachmentDownload> {
    client.attachments_download(&DownloadRequest {
        attachment_id,
        output_path: native_path(output_path)?,
        key_hex,
        base_nonce_hex,
    })
}

pub fn claim_attachment(client: &Client, id: &str) -> Result<()> {
    #[derive(Deserialize)]
    struct Response {
        claimed: bool,
    }
    let response: Response = client.attachments_claim(&IdRequest { id })?;
    confirmed(response.claimed, "attachment claim")
}

pub fn release_attachment(client: &Client, id: &str) -> Result<()> {
    #[derive(Deserialize)]
    struct Response {
        released: bool,
    }
    let response: Response = client.attachments_release(&IdRequest { id })?;
    confirmed(response.released, "attachment release")
}

#[derive(Serialize)]
struct AttachmentKeyRequest<'a> {
    recipient_account_id: &'a str,
    attachment_id: &'a str,
    key_hex: &'a str,
    base_nonce_hex: &'a str,
    conversation_id: Option<&'a str>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct KeyEnvelope {
    pub device_number: i64,
    pub envelope_id: String,
    pub is_prekey_message: bool,
    pub ciphertext_len: usize,
}

#[derive(Clone, Deserialize)]
pub struct KeyDeliveryError {
    pub device_number: i64,
    pub error: String,
}

#[derive(Clone, Deserialize)]
pub struct AttachmentKeyDelivery {
    pub envelopes: Vec<KeyEnvelope>,
    pub device_errors: Vec<KeyDeliveryError>,
}

pub fn send_attachment_key(
    client: &Client,
    recipient_account_id: &str,
    conversation_id: Option<&str>,
    upload: &AttachmentUpload,
) -> Result<AttachmentKeyDelivery> {
    client.attachments_send_key(&AttachmentKeyRequest {
        recipient_account_id,
        attachment_id: &upload.attachment_id,
        key_hex: &upload.key_hex,
        base_nonce_hex: &upload.base_nonce_hex,
        conversation_id,
    })
}

#[derive(Clone, Deserialize, Serialize)]
pub struct AttachmentKey {
    pub attachment_id: String,
    pub key_hex: String,
    pub base_nonce_hex: String,
    pub conversation_id: Option<String>,
}

#[derive(Clone, Deserialize)]
pub struct ReceivedAttachmentKey {
    pub envelope_id: String,
    pub sender_account_id: String,
    pub sender_device_number: i64,
    #[serde(rename = "decrypted_payload")]
    pub key: AttachmentKey,
}

/// Decode the key event without acknowledging it before the host saves it.
pub fn received_attachment_key(event: &Event) -> Result<Option<ReceivedAttachmentKey>> {
    if event.kind != "attachment_key_received" {
        return Ok(None);
    }
    serde_json::from_value(event.payload.clone())
        .map(Some)
        .map_err(Error::from)
}

fn native_path(path: &Path) -> Result<&str> {
    if !path.is_absolute() {
        return Err(Error::InvalidPayload(
            "expected an absolute native filesystem path".into(),
        ));
    }
    path.to_str().ok_or_else(|| {
        Error::InvalidPayload("native filesystem path cannot be represented as UTF-8".into())
    })
}

fn confirmed(value: bool, operation: &'static str) -> Result<()> {
    if value {
        Ok(())
    } else {
        Err(Error::Internal(format!("core did not confirm {operation}")))
    }
}
