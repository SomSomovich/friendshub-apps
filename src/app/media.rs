//! Native runtime for bot conversations, call signaling, and file attachments.
//!
//! Core work is dispatched through [`AppContext::call`]. Bot transcript and
//! call-history state are bound to the Slint root here. Managed-bot settings
//! state belongs to the settings controller. System media capture, clipboard,
//! and file picking stay outside this module until platform adapters exist.

use std::collections::{HashMap, HashSet};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::thread::JoinHandle;
use std::time::{SystemTime, UNIX_EPOCH};

use friendshub_core::error::Error as CoreError;
use friendshub_core::events::Event;
use slint::{ModelRc, SharedString, VecModel};

use crate::app::AppContext;
use crate::core::{CoreCallFailure, CoreClient};
use crate::features::{bots, calls_attachments, community, conversations};
use crate::{BotMessage as UiBotMessage, CallHistoryRow, MainWindow};

const BOT_HISTORY_PAGE_SIZE: i64 = 50;
const CALL_HISTORY_PAGE_SIZE: i64 = 200;

static CALL_SEQUENCE: AtomicU64 = AtomicU64::new(0);
static ACTIVE_CALL: OnceLock<Mutex<Option<ActiveCall>>> = OnceLock::new();
static BOT_TRANSCRIPTS: OnceLock<Mutex<HashMap<(String, String), BotTranscriptState>>> =
    OnceLock::new();
static CALL_CACHE: OnceLock<Mutex<HashMap<String, Vec<calls_attachments::CallRecord>>>> =
    OnceLock::new();
static APPLIED_CALL_EVENTS: OnceLock<Mutex<HashSet<(String, u64)>>> = OnceLock::new();
static IN_FLIGHT_CALL_EVENTS: OnceLock<Mutex<HashSet<(String, u64)>>> = OnceLock::new();

#[derive(Clone, Default)]
struct BotTranscriptState {
    messages: Vec<bots::BotMessage>,
    has_older: bool,
    loading: bool,
    loading_older: bool,
    busy: bool,
    error: String,
}

struct LoadedBotPage {
    info: Option<bots::BotInfo>,
    messages: Vec<bots::BotMessage>,
    older: bool,
    has_older: bool,
}

#[derive(Clone)]
struct ActiveCall {
    core: std::sync::Arc<CoreClient>,
    account_id: String,
    call_id: String,
    peer_account_id: String,
    peer_name: String,
    with_video: bool,
    started_at: i64,
    direction: calls_attachments::CallDirection,
    phase: ActiveCallPhase,
    remote_sdp: Option<String>,
    deferred_event_ids: Vec<u64>,
    action_pending: bool,
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum ActiveCallPhase {
    OutgoingStarting,
    Outgoing,
    IncomingRinging,
    Negotiating,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CoreEventDisposition {
    /// The event was applied synchronously and may be acknowledged by events.rs.
    Acknowledge,
    /// The media controller will acknowledge it after its core operation succeeds.
    Deferred,
    /// The event could not be applied; leave it unacknowledged for retry/replay.
    Unapplied,
}

fn active_call_slot() -> &'static Mutex<Option<ActiveCall>> {
    ACTIVE_CALL.get_or_init(|| Mutex::new(None))
}

fn bot_transcripts() -> &'static Mutex<HashMap<(String, String), BotTranscriptState>> {
    BOT_TRANSCRIPTS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn call_cache() -> &'static Mutex<HashMap<String, Vec<calls_attachments::CallRecord>>> {
    CALL_CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Snapshot loaded call history for one account.
pub fn call_history_snapshot(account_id: &str) -> Vec<calls_attachments::CallRecord> {
    call_cache()
        .lock()
        .ok()
        .and_then(|cache| cache.get(account_id).cloned())
        .unwrap_or_default()
}

/// Create a managed bot on a worker and return its one-time token on the
/// Slint event loop. The caller must display or copy the credential before
/// discarding it; this module never logs or stores the token.
pub fn create_bot(
    ctx: &AppContext,
    completion: impl FnOnce(Result<bots::BotCredential, CoreCallFailure>) + Send + 'static,
) -> io::Result<JoinHandle<()>> {
    ctx.call("friendshub-bot-create", bots::create, move |_, result| {
        completion(result)
    })
}

/// Rotate a managed bot token on a worker and return its replacement exactly
/// once to the Slint event loop. The caller is responsible for secure display
/// and for discarding the token when the transient dialog closes.
pub fn rotate_bot_token(
    ctx: &AppContext,
    bot_id: String,
    completion: impl FnOnce(Result<bots::RotatedBotToken, CoreCallFailure>) + Send + 'static,
) -> io::Result<JoinHandle<()>> {
    ctx.call(
        "friendshub-bot-token-rotate",
        move |client| bots::rotate_token(client, &bot_id),
        move |_, result| completion(result),
    )
}

/// Encrypt and upload a native file on a worker thread.
///
/// The completion runs on the Slint event loop and receives the one-time file
/// decryption key. The caller must keep that key out of logs and persist or
/// deliver it through the encrypted attachment-key workflow before discarding
/// it. This helper deliberately does not pretend that file picking or message
/// construction is implemented by the platform layer.
pub fn upload_attachment(
    ctx: &AppContext,
    file_path: PathBuf,
    conversation_id: String,
    kind: calls_attachments::AttachmentKind,
    completion: impl FnOnce(Result<calls_attachments::AttachmentUpload, CoreCallFailure>)
    + Send
    + 'static,
) -> io::Result<JoinHandle<()>> {
    validate_absolute_path(&file_path)?;
    ctx.call(
        "friendshub-attachment-upload",
        move |client| {
            calls_attachments::upload_attachment(client, &file_path, &conversation_id, kind)
        },
        move |_, result| completion(result),
    )
}

/// Decrypt an attachment to a caller-selected absolute path on a worker.
///
/// The core creates or truncates the destination directly. The caller must
/// choose an appropriate temporary destination when it needs atomic replace
/// semantics and remove partial output after a failed transfer.
pub fn download_attachment(
    ctx: &AppContext,
    attachment_id: String,
    output_path: PathBuf,
    key_hex: String,
    base_nonce_hex: String,
    completion: impl FnOnce(Result<calls_attachments::AttachmentDownload, CoreCallFailure>)
    + Send
    + 'static,
) -> io::Result<JoinHandle<()>> {
    validate_absolute_path(&output_path)?;
    ctx.call(
        "friendshub-attachment-download",
        move |client| {
            calls_attachments::download_attachment(
                client,
                &attachment_id,
                &output_path,
                &key_hex,
                &base_nonce_hex,
            )
        },
        move |_, result| completion(result),
    )
}

/// Load bot conversation or call history data without blocking Slint's event loop.
pub fn load(ctx: &AppContext, screen: &str) {
    match screen {
        "bot" => {
            let Some(bot_id) = current_bot_id(ctx) else {
                return;
            };
            load_bot_history(ctx, bot_id, false);
        }
        "calls" => load_call_history(ctx),
        _ => {}
    }
}

/// Retain a decrypted incoming call offer in memory until the user accepts or
/// declines it. The event's durable ID is acknowledged only after that action
/// succeeds, so core can redeliver the offer after an app restart.
pub fn receive_core_event(
    ctx: &AppContext,
    account_id: &str,
    event: &Event,
) -> CoreEventDisposition {
    if event.kind != "call_signal_received" {
        return CoreEventDisposition::Unapplied;
    }
    let Some(session) = ctx
        .active_session()
        .filter(|session| session.account.id == account_id)
    else {
        return CoreEventDisposition::Unapplied;
    };
    if event
        .id
        .is_some_and(|event_id| call_event_was_applied(account_id, event_id))
    {
        return CoreEventDisposition::Acknowledge;
    }
    let received = match calls_attachments::received_call_signal(event) {
        Ok(Some(received)) => received,
        Ok(None) => return CoreEventDisposition::Unapplied,
        Err(error) => {
            report_background_error("call signal decode", CoreCallFailure::from(error));
            return CoreEventDisposition::Unapplied;
        }
    };
    let peer_account_id = received.sender_account_id;
    match received.signal {
        calls_attachments::CallSignal::Offer { call_id, sdp } => {
            let peer_name = format!("Аккаунт {peer_account_id}");
            let with_video = sdp_offers_video(&sdp);
            let incoming = ActiveCall {
                core: session.core.clone(),
                account_id: account_id.to_owned(),
                call_id: call_id.clone(),
                peer_account_id: peer_account_id.clone(),
                peer_name: peer_name.clone(),
                with_video,
                started_at: received.client_timestamp,
                direction: calls_attachments::CallDirection::Incoming,
                phase: ActiveCallPhase::IncomingRinging,
                remote_sdp: Some(sdp),
                deferred_event_ids: event.id.into_iter().collect(),
                action_pending: false,
            };

            let mut already_handled = false;
            let should_reject = match active_call_slot().lock() {
                Ok(mut active) => match active.as_mut() {
                    Some(existing)
                        if existing.account_id == account_id
                            && existing.call_id == call_id
                            && existing.peer_account_id == peer_account_id =>
                    {
                        if existing.phase == ActiveCallPhase::IncomingRinging {
                            if let Some(event_id) = event.id {
                                if !existing.deferred_event_ids.contains(&event_id) {
                                    existing.deferred_event_ids.push(event_id);
                                }
                            }
                        } else {
                            already_handled = true;
                        }
                        false
                    }
                    Some(_) => true,
                    None => {
                        *active = Some(incoming);
                        false
                    }
                },
                Err(_) => true,
            };
            if should_reject {
                return dispatch_call_signal_event(account_id, event.id, || {
                    reject_busy_incoming_call(
                        ctx,
                        session.core,
                        account_id.to_owned(),
                        peer_account_id,
                        call_id,
                        received.client_timestamp,
                        event.id,
                    );
                    CoreEventDisposition::Deferred
                });
            }
            if already_handled {
                return CoreEventDisposition::Acknowledge;
            }

            let context = ctx.clone();
            let account_id = account_id.to_owned();
            ctx.apply_ui(move |window| {
                if !is_current_account(&context, &account_id) {
                    return;
                }
                let is_current_call = active_call_slot()
                    .lock()
                    .ok()
                    .and_then(|active| active.clone())
                    .is_some_and(|active| {
                        active.account_id == account_id
                            && active.call_id == call_id
                            && active.phase == ActiveCallPhase::IncomingRinging
                    });
                if !is_current_call {
                    return;
                }
                window.set_call_peer_name(peer_name.into());
                window.set_call_peer_initial(call_initial(&peer_account_id).into());
                window.set_call_with_video(with_video);
                window.set_call_state("incoming".into());
                window.set_call_status(
                    "Входящий звонок. Звук и видео недоступны без системного медиаподключателя."
                        .into(),
                );
                window.set_call_error(SharedString::default());
                window.set_call_has_remote_video(false);
                window.set_call_has_local_video(false);
                window.set_call_speaker_on(false);
                window.set_call_camera_off(false);
                window.set_call_overlay_visible(true);
            });
            CoreEventDisposition::Deferred
        }
        calls_attachments::CallSignal::Answer { call_id, sdp } => {
            dispatch_call_signal_event(account_id, event.id, || {
                apply_remote_answer(
                    ctx,
                    account_id,
                    session.core,
                    peer_account_id,
                    call_id,
                    sdp,
                    event.id,
                )
            })
        }
        calls_attachments::CallSignal::Ice { call_id, candidate } => {
            dispatch_call_signal_event(account_id, event.id, || {
                apply_remote_ice(
                    ctx,
                    account_id,
                    session.core,
                    peer_account_id,
                    call_id,
                    candidate,
                    event.id,
                )
            })
        }
        calls_attachments::CallSignal::Hangup { call_id, .. } => {
            dispatch_call_signal_event(account_id, event.id, || {
                finish_remote_call(
                    ctx,
                    account_id,
                    session.core,
                    peer_account_id,
                    call_id,
                    false,
                    event.id,
                )
            })
        }
        calls_attachments::CallSignal::Reject { call_id, .. } => {
            dispatch_call_signal_event(account_id, event.id, || {
                finish_remote_call(
                    ctx,
                    account_id,
                    session.core,
                    peer_account_id,
                    call_id,
                    true,
                    event.id,
                )
            })
        }
    }
}

fn active_call_for_signal(
    core: &std::sync::Arc<CoreClient>,
    account_id: &str,
    peer_account_id: &str,
    call_id: &str,
) -> Option<ActiveCall> {
    let mut slot = active_call_slot().lock().ok()?;
    let call = slot.as_mut()?;
    if call.account_id != account_id
        || call.call_id != call_id
        || !std::sync::Arc::ptr_eq(&call.core, core)
    {
        return None;
    }
    if call.peer_account_id.is_empty() && call.phase == ActiveCallPhase::OutgoingStarting {
        call.peer_account_id = peer_account_id.to_owned();
    } else if call.peer_account_id != peer_account_id {
        return None;
    }
    Some(call.clone())
}

fn dispatch_call_signal_event(
    account_id: &str,
    event_id: Option<u64>,
    apply: impl FnOnce() -> CoreEventDisposition,
) -> CoreEventDisposition {
    if let Some(event_id) = event_id {
        if call_event_was_applied(account_id, event_id) {
            return CoreEventDisposition::Acknowledge;
        }
        let Ok(mut in_flight) = IN_FLIGHT_CALL_EVENTS
            .get_or_init(|| Mutex::new(HashSet::new()))
            .lock()
        else {
            return CoreEventDisposition::Unapplied;
        };
        if !in_flight.insert((account_id.to_owned(), event_id)) {
            return CoreEventDisposition::Deferred;
        }
    }

    let disposition = apply();
    if disposition == CoreEventDisposition::Unapplied {
        if let Some(event_id) = event_id {
            clear_call_event_in_flight(account_id, event_id);
        }
    }
    disposition
}

fn apply_remote_answer(
    ctx: &AppContext,
    account_id: &str,
    core: std::sync::Arc<CoreClient>,
    peer_account_id: String,
    call_id: String,
    sdp: String,
    event_id: Option<u64>,
) -> CoreEventDisposition {
    let Some(call) = active_call_for_signal(&core, account_id, &peer_account_id, &call_id) else {
        return CoreEventDisposition::Unapplied;
    };
    if call.phase == ActiveCallPhase::IncomingRinging {
        return CoreEventDisposition::Unapplied;
    }
    let completion_core = core.clone();
    let completion_context = ctx.clone();
    let expected_account_id = account_id.to_owned();
    let completion_call = call.clone();
    let operation_call_id = call_id.clone();
    let result = core.call(
        "friendshub-call-answer",
        move |client| calls_attachments::apply_answer(client, &operation_call_id, &sdp),
        move |result| match result {
            Ok(()) => {
                if let Some(event_id) = event_id {
                    acknowledge_core_event(
                        &completion_core,
                        &expected_account_id,
                        event_id,
                        "call answer",
                    );
                }
                update_active_call_status(
                    &completion_context,
                    &completion_call,
                    &expected_account_id,
                    "Ответ собеседника получен. Звук и видео недоступны без системного медиаподключателя.",
                    None,
                );
            }
            Err(error) => {
                if let Some(event_id) = event_id {
                    clear_call_event_in_flight(&expected_account_id, event_id);
                }
                show_incoming_call_error(&completion_context, &completion_call, error.user_message());
                report_background_error("call answer", error);
            }
        },
    );
    match result {
        Ok(_) => CoreEventDisposition::Deferred,
        Err(error) => {
            report_spawn_error("call answer", error);
            CoreEventDisposition::Unapplied
        }
    }
}

fn apply_remote_ice(
    _ctx: &AppContext,
    account_id: &str,
    core: std::sync::Arc<CoreClient>,
    peer_account_id: String,
    call_id: String,
    candidate: calls_attachments::IceCandidate,
    event_id: Option<u64>,
) -> CoreEventDisposition {
    let Some(call) = active_call_for_signal(&core, account_id, &peer_account_id, &call_id) else {
        return CoreEventDisposition::Unapplied;
    };
    if call.phase == ActiveCallPhase::IncomingRinging {
        return CoreEventDisposition::Unapplied;
    }
    let completion_core = core.clone();
    let operation_call_id = call_id.clone();
    let result = core.call(
        "friendshub-call-ice",
        move |client| calls_attachments::add_ice_candidate(client, &operation_call_id, &candidate),
        move |result| match result {
            Ok(()) => {
                if let Some(event_id) = event_id {
                    acknowledge_core_event(
                        &completion_core,
                        &call.account_id,
                        event_id,
                        "call ICE",
                    );
                }
            }
            Err(error) => {
                if let Some(event_id) = event_id {
                    clear_call_event_in_flight(&call.account_id, event_id);
                }
                report_background_error("call ICE", error);
            }
        },
    );
    match result {
        Ok(_) => CoreEventDisposition::Deferred,
        Err(error) => {
            report_spawn_error("call ICE", error);
            CoreEventDisposition::Unapplied
        }
    }
}

fn finish_remote_call(
    ctx: &AppContext,
    account_id: &str,
    core: std::sync::Arc<CoreClient>,
    peer_account_id: String,
    call_id: String,
    rejected: bool,
    event_id: Option<u64>,
) -> CoreEventDisposition {
    let Some(call) = active_call_for_signal(&core, account_id, &peer_account_id, &call_id) else {
        return CoreEventDisposition::Unapplied;
    };
    let is_ringing_incoming = call.phase == ActiveCallPhase::IncomingRinging;
    let status = if is_ringing_incoming {
        calls_attachments::CallStatus::Missed
    } else if rejected {
        calls_attachments::CallStatus::Rejected
    } else {
        // Connection-state events are not wired, so the app cannot claim that
        // audio/video reached a connected state.
        calls_attachments::CallStatus::Cancelled
    };
    let completion_core = core.clone();
    let completion_context = ctx.clone();
    let refresh_context = ctx.clone();
    let completion_call = call.clone();
    let expected_account_id = account_id.to_owned();
    let result = core.call(
        "friendshub-call-remote-end",
        move |client| {
            if !is_ringing_incoming {
                let active_calls = calls_attachments::active_calls(client)?;
                if active_calls.iter().any(|active_id| active_id == &call_id) {
                    calls_attachments::close_call(client, &call_id)?;
                }
            }
            Ok(calls_attachments::record_call(
                client,
                &calls_attachments::CallRecordInput {
                    peer_account_id: &peer_account_id,
                    direction: call.direction,
                    status,
                    started_at: call.started_at,
                    ended_at: Some(unix_seconds()),
                },
            ))
        },
        move |result| match result {
            Ok(record_result) => {
                clear_active_call(&completion_call.call_id);
                acknowledge_call_events(&completion_call);
                if let Some(event_id) = event_id {
                    acknowledge_core_event(
                        &completion_core,
                        &completion_call.account_id,
                        event_id,
                        "remote call end",
                    );
                }
                if is_current_account(&completion_context, &expected_account_id) {
                    let account_id = completion_call.account_id.clone();
                    completion_context.apply_ui(move |window| {
                        if !account_has_active_call(&account_id) {
                            window.set_call_overlay_visible(false);
                            window.set_call_state("idle".into());
                            window.set_call_status(SharedString::default());
                            window.set_call_error(SharedString::default());
                            window.set_call_with_video(false);
                        }
                    });
                }
                if let Err(error) = record_result {
                    report_background_error("call history record", CoreCallFailure::from(error));
                }
                if is_current_account(&completion_context, &expected_account_id) {
                    load_call_history(&refresh_context);
                }
            }
            Err(error) => {
                if let Some(event_id) = event_id {
                    clear_call_event_in_flight(&completion_call.account_id, event_id);
                }
                show_incoming_call_error(
                    &completion_context,
                    &completion_call,
                    error.user_message(),
                );
                report_background_error("remote call end", error);
            }
        },
    );
    match result {
        Ok(_) => CoreEventDisposition::Deferred,
        Err(error) => {
            report_spawn_error("remote call end", error);
            CoreEventDisposition::Unapplied
        }
    }
}

fn update_active_call_status(
    ctx: &AppContext,
    call: &ActiveCall,
    account_id: &str,
    status: &'static str,
    error: Option<&'static str>,
) {
    let context = ctx.clone();
    let call = call.clone();
    let account_id = account_id.to_owned();
    ctx.apply_ui(move |window| {
        let matches_call = active_call_slot()
            .lock()
            .ok()
            .and_then(|active| active.clone())
            .is_some_and(|active| {
                active.account_id == call.account_id && active.call_id == call.call_id
            });
        if matches_call && is_current_account(&context, &account_id) {
            window.set_call_peer_name(call.peer_name.clone().into());
            window.set_call_peer_initial(call_initial(&call.peer_account_id).into());
            window.set_call_with_video(call.with_video);
            window.set_call_status(status.into());
            window.set_call_error(error.unwrap_or_default().into());
            if call.phase != ActiveCallPhase::IncomingRinging {
                window.set_call_state("active".into());
            }
        }
    });
}

fn accept_incoming_call(ctx: &AppContext) -> bool {
    let Some(session) = ctx.active_session() else {
        return false;
    };
    let call = match active_call_slot().lock() {
        Ok(mut active) => {
            let Some(call) = active.as_mut().filter(|call| {
                call.account_id == session.account.id
                    && std::sync::Arc::ptr_eq(&call.core, &session.core)
                    && call.direction == calls_attachments::CallDirection::Incoming
                    && call.phase == ActiveCallPhase::IncomingRinging
            }) else {
                return false;
            };
            if call.action_pending {
                return true;
            }
            call.action_pending = true;
            call.clone()
        }
        Err(_) => {
            report_call_message(
                ctx,
                "Не удалось прочитать состояние входящего звонка.",
                true,
            );
            return true;
        }
    };
    let Some(remote_sdp) = call.remote_sdp.clone() else {
        reset_incoming_action(&call);
        show_incoming_call_error(ctx, &call, "Не получено описание входящего звонка.");
        return true;
    };
    set_incoming_call_status(ctx, &call, "Принимаем вызов…");

    let call_for_worker = call.clone();
    let call_for_spawn_error = call.clone();
    let completion_context = ctx.clone();
    let worker_result = ctx.call(
        "friendshub-call-accept",
        move |client| {
            calls_attachments::accept_call(
                client,
                &call_for_worker.peer_account_id,
                &call_for_worker.call_id,
                &remote_sdp,
            )
        },
        move |window, result| match result {
            Ok(_signal) => {
                if let Ok(mut active) = active_call_slot().lock() {
                    if let Some(active) = active.as_mut().filter(|active| {
                        active.account_id == call.account_id && active.call_id == call.call_id
                    }) {
                        active.action_pending = false;
                        active.phase = ActiveCallPhase::Negotiating;
                        active.remote_sdp = None;
                        active.deferred_event_ids.clear();
                    }
                }
                acknowledge_call_events(&call);
                if is_current_account(&completion_context, &call.account_id)
                    && active_call_matches(&call.account_id, &call.call_id)
                    && window.get_call_state().as_str() == "incoming"
                {
                    window.set_call_peer_name(call.peer_name.clone().into());
                    window.set_call_peer_initial(call_initial(&call.peer_account_id).into());
                    window.set_call_with_video(call.with_video);
                    window.set_call_state("active".into());
                    window.set_call_status(
                        "Сигнал вызова принят. Голос и видео недоступны без системного медиаподключателя."
                            .into(),
                    );
                    window.set_call_error(SharedString::default());
                }
            }
            Err(error) => {
                reset_incoming_action(&call);
                show_incoming_call_error(
                    &completion_context,
                    &call,
                    error.user_message(),
                );
                report_background_error("call accept", error);
            }
        },
    );
    if let Err(error) = worker_result {
        reset_incoming_action(&call_for_spawn_error);
        show_incoming_call_error(
            ctx,
            &call_for_spawn_error,
            "Не удалось запустить принятие звонка.",
        );
        report_spawn_error("call accept", error);
    }
    true
}

fn decline_incoming_call(ctx: &AppContext) -> bool {
    let Some(session) = ctx.active_session() else {
        return false;
    };
    let call = match active_call_slot().lock() {
        Ok(mut active) => {
            let Some(call) = active.as_mut().filter(|call| {
                call.account_id == session.account.id
                    && std::sync::Arc::ptr_eq(&call.core, &session.core)
                    && call.direction == calls_attachments::CallDirection::Incoming
                    && call.phase == ActiveCallPhase::IncomingRinging
            }) else {
                return false;
            };
            if call.action_pending {
                return true;
            }
            call.action_pending = true;
            call.clone()
        }
        Err(_) => {
            report_call_message(
                ctx,
                "Не удалось прочитать состояние входящего звонка.",
                true,
            );
            return true;
        }
    };
    set_incoming_call_status(ctx, &call, "Отклоняем вызов…");

    let call_for_worker = call.clone();
    let call_for_spawn_error = call.clone();
    let completion_context = ctx.clone();
    let refresh_context = ctx.clone();
    let worker_result = ctx.call(
        "friendshub-call-decline",
        move |client| {
            calls_attachments::reject_call(
                client,
                &call_for_worker.peer_account_id,
                &call_for_worker.call_id,
                Some("user_reject"),
            )?;
            let record = calls_attachments::record_call(
                client,
                &calls_attachments::CallRecordInput {
                    peer_account_id: &call_for_worker.peer_account_id,
                    direction: calls_attachments::CallDirection::Incoming,
                    status: calls_attachments::CallStatus::Rejected,
                    started_at: call_for_worker.started_at,
                    ended_at: Some(unix_seconds()),
                },
            );
            Ok(record)
        },
        move |window, result| match result {
            Ok(record_result) => {
                clear_active_call(&call.call_id);
                acknowledge_call_events(&call);
                if is_current_account(&completion_context, &call.account_id)
                    && !account_has_active_call(&call.account_id)
                {
                    window.set_call_overlay_visible(false);
                    window.set_call_state("idle".into());
                    window.set_call_status(SharedString::default());
                    window.set_call_error(SharedString::default());
                    window.set_call_with_video(false);
                }
                if let Err(error) = record_result {
                    report_background_error("call history record", CoreCallFailure::from(error));
                }
                if is_current_account(&completion_context, &call.account_id) {
                    load_call_history(&refresh_context);
                }
            }
            Err(error) => {
                reset_incoming_action(&call);
                show_incoming_call_error(&completion_context, &call, error.user_message());
                report_background_error("call decline", error);
            }
        },
    );
    if let Err(error) = worker_result {
        reset_incoming_action(&call_for_spawn_error);
        show_incoming_call_error(
            ctx,
            &call_for_spawn_error,
            "Не удалось запустить отклонение звонка.",
        );
        report_spawn_error("call decline", error);
    }
    true
}

fn set_incoming_call_status(ctx: &AppContext, call: &ActiveCall, status: &'static str) {
    let context = ctx.clone();
    let account_id = call.account_id.clone();
    let call_id = call.call_id.clone();
    ctx.apply_ui(move |window| {
        if is_current_account(&context, &account_id)
            && window.get_call_state().as_str() == "incoming"
            && active_call_slot()
                .lock()
                .ok()
                .and_then(|active| active.clone())
                .is_some_and(|active| active.account_id == account_id && active.call_id == call_id)
        {
            window.set_call_status(status.into());
            window.set_call_error(SharedString::default());
        }
    });
}

fn show_incoming_call_error(ctx: &AppContext, call: &ActiveCall, message: &'static str) {
    let context = ctx.clone();
    let account_id = call.account_id.clone();
    let call_id = call.call_id.clone();
    ctx.apply_ui(move |window| {
        if is_current_account(&context, &account_id)
            && active_call_slot()
                .lock()
                .ok()
                .and_then(|active| active.clone())
                .is_some_and(|active| active.account_id == account_id && active.call_id == call_id)
        {
            window.set_call_error(message.into());
        }
    });
}

fn reset_incoming_action(call: &ActiveCall) {
    if let Ok(mut active) = active_call_slot().lock() {
        if let Some(active) = active
            .as_mut()
            .filter(|active| active.account_id == call.account_id && active.call_id == call.call_id)
        {
            active.action_pending = false;
        }
    }
}

fn acknowledge_call_events(call: &ActiveCall) {
    for event_id in &call.deferred_event_ids {
        let event_id = *event_id;
        let completion_call_id = call.call_id.clone();
        mark_call_event_applied(&call.account_id, event_id);
        if let Err(error) = call.core.ack_event_async(event_id, move |result| {
            if let Err(error) = result {
                eprintln!(
                    "[native media] call event acknowledgement failed: call={completion_call_id} code={}",
                    error.code()
                );
            }
        }) {
            eprintln!(
                "[native media] call event acknowledgement could not start: call={} kind={:?}",
                call.call_id,
                error.kind()
            );
        }
    }
}

fn reject_busy_incoming_call(
    ctx: &AppContext,
    core: std::sync::Arc<CoreClient>,
    account_id: String,
    peer_account_id: String,
    call_id: String,
    started_at: i64,
    event_id: Option<u64>,
) {
    let expected_account_id = account_id.clone();
    let spawn_account_id = expected_account_id.clone();
    let completion_context = ctx.clone();
    let completion_core = core.clone();
    let result = core.call(
        "friendshub-call-busy-reject",
        move |client| {
            calls_attachments::reject_call(client, &peer_account_id, &call_id, Some("busy"))?;
            let record = calls_attachments::record_call(
                client,
                &calls_attachments::CallRecordInput {
                    peer_account_id: &peer_account_id,
                    direction: calls_attachments::CallDirection::Incoming,
                    status: calls_attachments::CallStatus::Rejected,
                    started_at,
                    ended_at: Some(unix_seconds()),
                },
            );
            Ok(record)
        },
        move |result| match result {
            Ok(record_result) => {
                if let Some(event_id) = event_id {
                    acknowledge_core_event(
                        &completion_core,
                        &expected_account_id,
                        event_id,
                        "busy call reject",
                    );
                }
                if let Err(error) = record_result {
                    report_background_error("call history record", CoreCallFailure::from(error));
                }
                if is_current_account(&completion_context, &expected_account_id) {
                    load_call_history(&completion_context);
                }
            }
            Err(error) => {
                if let Some(event_id) = event_id {
                    clear_call_event_in_flight(&expected_account_id, event_id);
                }
                report_background_error("busy call reject", error);
            }
        },
    );
    if let Err(error) = result {
        if let Some(event_id) = event_id {
            clear_call_event_in_flight(&spawn_account_id, event_id);
        }
        report_spawn_error("busy call reject", error);
    }
}

fn acknowledge_core_event(
    core: &CoreClient,
    account_id: &str,
    event_id: u64,
    operation: &'static str,
) {
    mark_call_event_applied(account_id, event_id);
    if let Err(error) = core.ack_event_async(event_id, move |result| {
        if let Err(error) = result {
            eprintln!(
                "[native media] {operation} event acknowledgement failed: code={}",
                error.code()
            );
        }
    }) {
        eprintln!(
            "[native media] {operation} event acknowledgement could not start: kind={:?}",
            error.kind()
        );
    }
}

fn call_event_was_applied(account_id: &str, event_id: u64) -> bool {
    APPLIED_CALL_EVENTS
        .get_or_init(|| Mutex::new(HashSet::new()))
        .lock()
        .is_ok_and(|events| events.contains(&(account_id.to_owned(), event_id)))
}

fn mark_call_event_applied(account_id: &str, event_id: u64) {
    clear_call_event_in_flight(account_id, event_id);
    if let Ok(mut events) = APPLIED_CALL_EVENTS
        .get_or_init(|| Mutex::new(HashSet::new()))
        .lock()
    {
        events.insert((account_id.to_owned(), event_id));
    }
}

fn clear_call_event_in_flight(account_id: &str, event_id: u64) {
    if let Ok(mut events) = IN_FLIGHT_CALL_EVENTS
        .get_or_init(|| Mutex::new(HashSet::new()))
        .lock()
    {
        events.remove(&(account_id.to_owned(), event_id));
    }
}

fn sdp_offers_video(sdp: &str) -> bool {
    sdp.lines()
        .any(|line| line.trim_start().starts_with("m=video "))
}

fn call_initial(peer_account_id: &str) -> String {
    peer_account_id
        .chars()
        .next()
        .map(|initial| initial.to_uppercase().to_string())
        .unwrap_or_else(|| "?".to_owned())
}

/// Handle the `bots`, `calls`, or `attachments` namespace action.
///
/// `arg1..arg3` preserve the three string arguments from `MainWindow.ui-action`.
/// Return `false` when completing the action requires an unavailable platform
/// adapter or UI surface, allowing the root dispatcher to show its standard
/// unhandled-action feedback.
pub fn handle(
    ctx: &AppContext,
    namespace: &str,
    action: &str,
    arg1: &str,
    arg2: &str,
    arg3: &str,
) -> bool {
    match namespace {
        "bots" => handle_bots(ctx, action, arg1, arg2, arg3),
        "calls" => handle_calls(ctx, action, arg1, arg2, arg3),
        "attachments" => handle_attachments(ctx, action, arg1, arg2, arg3),
        _ => false,
    }
}

fn handle_bots(ctx: &AppContext, action: &str, arg1: &str, _arg2: &str, _arg3: &str) -> bool {
    match action {
        "refresh" | "retry" => match current_screen(ctx).as_deref() {
            Some("bot") => {
                let Some(bot_id) = current_bot_id(ctx) else {
                    return false;
                };
                load_bot_history(ctx, bot_id, false);
                true
            }
            _ => false,
        },
        "send" => {
            let Some(bot_id) = current_bot_id(ctx) else {
                return false;
            };
            let text = arg1.to_owned();
            if text.trim().is_empty() {
                return true;
            }
            let Some(key) = bot_key(ctx, &bot_id) else {
                return false;
            };
            {
                let Ok(mut transcripts) = bot_transcripts().lock() else {
                    show_bot_error(
                        ctx,
                        &bot_id,
                        "Не удалось прочитать состояние переписки с ботом.",
                    );
                    return true;
                };
                let state = transcripts.entry(key.clone()).or_default();
                if state.busy {
                    return true;
                }
                state.busy = true;
                state.error.clear();
                let snapshot = state.clone();
                drop(transcripts);
                apply_bot_state_to_context(ctx, &key, &snapshot);
            }
            let bot_id_for_call = bot_id.clone();
            let key_for_completion = key.clone();
            let sent_text = text.clone();
            let send_context = ctx.clone();
            if let Err(error) = ctx.call(
                "friendshub-bot-send",
                move |client| bots::send(client, &bot_id_for_call, &text, None),
                move |window, result| match result {
                    Ok(message) => {
                        let snapshot = match bot_transcripts().lock() {
                            Ok(mut transcripts) => {
                                let state =
                                    transcripts.entry(key_for_completion.clone()).or_default();
                                state.busy = false;
                                state.error.clear();
                                insert_bot_message(&mut state.messages, message);
                                Some(state.clone())
                            }
                            Err(_) => None,
                        };
                        if let Some(snapshot) = snapshot {
                            apply_bot_state_to_window(
                                &send_context,
                                &window,
                                &key_for_completion,
                                &snapshot,
                            );
                            if window.get_bot_draft().as_str() == sent_text.as_str() {
                                window.set_bot_draft(SharedString::default());
                            }
                        }
                    }
                    Err(error) => {
                        let snapshot = match bot_transcripts().lock() {
                            Ok(mut transcripts) => {
                                let state =
                                    transcripts.entry(key_for_completion.clone()).or_default();
                                state.busy = false;
                                state.error = error.user_message().to_owned();
                                Some(state.clone())
                            }
                            Err(_) => None,
                        };
                        if let Some(snapshot) = snapshot {
                            apply_bot_state_to_window(
                                &send_context,
                                &window,
                                &key_for_completion,
                                &snapshot,
                            );
                        }
                        report_background_error("bot send", error);
                    }
                },
            ) {
                set_bot_failure(&key, "Не удалось запустить отправку сообщения.");
                apply_bot_state_to_context(ctx, &key, &bot_state(&key));
                report_spawn_error("bot send", error);
            }
            true
        }
        // The text field is owned by BotChatScreen; no core operation is
        // needed until its explicit send callback.
        "draft" => true,
        "add-to-channel" => {
            let Some(conversation_id) = selected_conversation_id(ctx) else {
                return false;
            };
            let bot_id = arg1.to_owned();
            let conversation_id_for_call = conversation_id.clone();
            if let Err(error) = ctx.call(
                "friendshub-channel-bot-add",
                move |client| {
                    community::add_channel_bot(client, &conversation_id_for_call, &bot_id)
                },
                move |_, result| {
                    if let Err(error) = result {
                        report_background_error("channel bot add", error);
                    }
                },
            ) {
                report_spawn_error("channel bot add", error);
            }
            true
        }
        "load-older" => {
            let Some(bot_id) = current_bot_id(ctx) else {
                return false;
            };
            load_bot_history(ctx, bot_id, true);
            true
        }
        _ => false,
    }
}

fn handle_calls(ctx: &AppContext, action: &str, arg1: &str, _arg2: &str, _arg3: &str) -> bool {
    match action {
        "audio" | "video" => {
            let is_history_peer = current_screen(ctx).as_deref() == Some("calls");
            let target = if is_history_peer {
                arg1.to_owned()
            } else {
                let Some(conversation_id) = selected_conversation_id(ctx) else {
                    return false;
                };
                conversation_id
            };
            start_outgoing_call(ctx, target, is_history_peer, action == "video");
            true
        }
        "hang-up" => {
            end_active_call(ctx);
            true
        }
        "accept" => accept_incoming_call(ctx),
        "decline" => decline_incoming_call(ctx),
        "clear-history" => {
            set_calls_busy(ctx, true);
            let expected_account_id = ctx
                .active_session()
                .map(|session| session.account.id)
                .unwrap_or_default();
            let completion_account_id = expected_account_id.clone();
            let refresh_ctx = ctx.clone();
            let completion_ctx = ctx.clone();
            let error_ctx = ctx.clone();
            if let Err(error) = ctx.call(
                "friendshub-call-history-clear",
                calls_attachments::clear_call_history,
                move |window, result| match result {
                    Ok(()) => {
                        if !is_current_account(&completion_ctx, &completion_account_id) {
                            return;
                        }
                        window.set_calls_busy(false);
                        if window.get_screen().as_str() == "calls" {
                            clear_call_confirmation(&window);
                        }
                        load_call_history(&refresh_ctx);
                    }
                    Err(error) => {
                        if is_current_account(&completion_ctx, &completion_account_id) {
                            window.set_calls_busy(false);
                            if window.get_screen().as_str() == "calls" {
                                window.set_calls_error_text(error.user_message().into());
                            }
                        }
                        report_background_error("call history clear", error);
                    }
                },
            ) {
                set_calls_failure(
                    &error_ctx,
                    &expected_account_id,
                    "Не удалось запустить очистку истории звонков.",
                );
                report_spawn_error("call history clear", error);
            }
            true
        }
        "delete-history-item" => {
            if arg1.trim().is_empty() {
                return true;
            }
            let call_id = arg1.to_owned();
            set_calls_busy(ctx, true);
            let expected_account_id = ctx
                .active_session()
                .map(|session| session.account.id)
                .unwrap_or_default();
            let completion_account_id = expected_account_id.clone();
            let refresh_ctx = ctx.clone();
            let completion_ctx = ctx.clone();
            let error_ctx = ctx.clone();
            if let Err(error) = ctx.call(
                "friendshub-call-history-delete",
                move |client| calls_attachments::delete_call(client, &call_id),
                move |window, result| match result {
                    Ok(()) => {
                        if !is_current_account(&completion_ctx, &completion_account_id) {
                            return;
                        }
                        window.set_calls_busy(false);
                        if window.get_screen().as_str() == "calls" {
                            clear_call_confirmation(&window);
                        }
                        load_call_history(&refresh_ctx);
                    }
                    Err(error) => {
                        if is_current_account(&completion_ctx, &completion_account_id) {
                            window.set_calls_busy(false);
                            if window.get_screen().as_str() == "calls" {
                                window.set_calls_error_text(error.user_message().into());
                            }
                        }
                        report_background_error("call history deletion", error);
                    }
                },
            ) {
                set_calls_failure(
                    &error_ctx,
                    &expected_account_id,
                    "Не удалось запустить удаление записи звонка.",
                );
                report_spawn_error("call history deletion", error);
            }
            true
        }
        "refresh" | "retry" if current_screen(ctx).as_deref() == Some("calls") => {
            load_call_history(ctx);
            true
        }
        // Incoming call callbacks carry no sender, call id, or SDP. Speaker,
        // camera, and camera-switch actions are host-device operations.
        "profile-call" | "toggle-speaker" | "toggle-camera" | "switch-camera" => false,
        _ => false,
    }
}

fn handle_attachments(
    _ctx: &AppContext,
    action: &str,
    _arg1: &str,
    _arg2: &str,
    _arg3: &str,
) -> bool {
    match action {
        // The Slint callbacks currently provide no absolute file path. Open
        // callbacks provide only an attachment id, with no local key material.
        // Voice and video note callbacks have no capture/encoding adapter.
        "choose" | "open-message" | "open-post" | "voice-record" | "video-note" => false,
        // File transfer helpers are reachable once the platform/UI supplies a
        // path and the surrounding attachment message/key workflow.
        "upload" | "download" => false,
        _ => false,
    }
}

fn load_bot_history(ctx: &AppContext, bot_id: String, older: bool) {
    let Some(key) = bot_key(ctx, &bot_id) else {
        return;
    };
    let query = {
        let Ok(mut transcripts) = bot_transcripts().lock() else {
            show_bot_error(
                ctx,
                &bot_id,
                "Не удалось прочитать состояние переписки с ботом.",
            );
            return;
        };
        let state = transcripts.entry(key.clone()).or_default();
        let query = if older {
            if state.loading || state.loading_older || !state.has_older {
                return;
            }
            let Some(oldest) = state.messages.first() else {
                state.has_older = false;
                let snapshot = state.clone();
                drop(transcripts);
                apply_bot_state_to_context(ctx, &key, &snapshot);
                return;
            };
            state.loading_older = true;
            bots::BotHistoryQuery {
                before: Some(oldest.created_at),
                before_id: Some(oldest.id.clone()),
                limit: Some(BOT_HISTORY_PAGE_SIZE),
            }
        } else {
            state.loading = true;
            state.loading_older = false;
            state.error.clear();
            bots::BotHistoryQuery {
                limit: Some(BOT_HISTORY_PAGE_SIZE),
                ..Default::default()
            }
        };
        let snapshot = state.clone();
        drop(transcripts);
        apply_bot_state_to_context(ctx, &key, &snapshot);
        query
    };

    let request_bot_id = bot_id.clone();
    let expected_key = key.clone();
    let context = ctx.clone();
    if let Err(error) = ctx.call(
        "friendshub-bot-history",
        move |client| {
            let info = if older {
                None
            } else {
                Some(bots::info(client, &request_bot_id)?)
            };
            let messages = bots::history(client, &request_bot_id, query)?;
            let has_older = messages.len() as i64 >= BOT_HISTORY_PAGE_SIZE;
            Ok(LoadedBotPage {
                info,
                messages,
                older,
                has_older,
            })
        },
        move |window, result| {
            let snapshot = match bot_transcripts().lock() {
                Ok(mut transcripts) => {
                    let state = transcripts.entry(expected_key.clone()).or_default();
                    state.loading = false;
                    state.loading_older = false;
                    match result {
                        Ok(page) => {
                            state.error.clear();
                            merge_bot_page(state, page.messages, page.older, page.has_older);
                            let info = page.info;
                            let snapshot = state.clone();
                            Some((snapshot, info))
                        }
                        Err(error) => {
                            state.error = error.user_message().to_owned();
                            report_background_error("bot history load", error);
                            Some((state.clone(), None))
                        }
                    }
                }
                Err(_) => None,
            };
            let Some((snapshot, info)) = snapshot else {
                return;
            };
            let is_visible = window.get_screen().as_str() == "bot"
                && current_bot_id_from_route(&window.get_route()).as_deref()
                    == Some(expected_key.1.as_str());
            if is_visible {
                if let Some(info) = info {
                    window.set_conversation_title(info.handle.unwrap_or_default().into());
                }
                apply_bot_state_to_window(&context, &window, &expected_key, &snapshot);
            }
        },
    ) {
        set_bot_failure(&key, "Не удалось запустить загрузку переписки с ботом.");
        apply_bot_state_to_context(ctx, &key, &bot_state(&key));
        report_spawn_error("bot history load", error);
    }
}

fn bot_key(ctx: &AppContext, bot_id: &str) -> Option<(String, String)> {
    let account_id = ctx.active_session()?.account.id;
    (!bot_id.trim().is_empty()).then(|| (account_id, bot_id.to_owned()))
}

fn bot_state(key: &(String, String)) -> BotTranscriptState {
    bot_transcripts()
        .lock()
        .ok()
        .and_then(|transcripts| transcripts.get(key).cloned())
        .unwrap_or_default()
}

fn set_bot_failure(key: &(String, String), message: &str) {
    if let Ok(mut transcripts) = bot_transcripts().lock() {
        let state = transcripts.entry(key.clone()).or_default();
        state.loading = false;
        state.loading_older = false;
        state.busy = false;
        state.error = message.to_owned();
    }
}

fn show_bot_error(ctx: &AppContext, bot_id: &str, message: &'static str) {
    let bot_id = bot_id.to_owned();
    let expected_account = ctx.active_session().map(|session| session.account.id);
    let context = ctx.clone();
    ctx.apply_ui(move |window| {
        if window.get_screen().as_str() == "bot"
            && current_bot_id_from_route(&window.get_route()).as_deref() == Some(bot_id.as_str())
            && expected_account.is_some_and(|account_id| {
                context
                    .active_session()
                    .is_some_and(|session| session.account.id == account_id)
            })
        {
            window.set_bot_error_text(message.into());
        }
    });
}

fn apply_bot_state_to_context(
    ctx: &AppContext,
    key: &(String, String),
    state: &BotTranscriptState,
) {
    let context = ctx.clone();
    let key = key.clone();
    let state = state.clone();
    ctx.apply_ui(move |window| apply_bot_state_to_window(&context, &window, &key, &state));
}

fn apply_bot_state_to_window(
    ctx: &AppContext,
    window: &MainWindow,
    key: &(String, String),
    state: &BotTranscriptState,
) {
    let is_active_account = ctx
        .active_session()
        .is_some_and(|session| session.account.id == key.0);
    if !is_active_account
        || window.get_screen().as_str() != "bot"
        || current_bot_id_from_route(&window.get_route()).as_deref() != Some(key.1.as_str())
    {
        return;
    }
    window.set_bot_messages(ModelRc::new(VecModel::from(
        state
            .messages
            .iter()
            .cloned()
            .map(to_ui_bot_message)
            .collect::<Vec<_>>(),
    )));
    window.set_bot_error_text(state.error.clone().into());
    window.set_bot_loading(state.loading);
    window.set_bot_loading_older(state.loading_older);
    window.set_bot_has_older(state.has_older);
    window.set_bot_busy(state.busy);
}

fn merge_bot_page(
    state: &mut BotTranscriptState,
    mut page: Vec<bots::BotMessage>,
    older: bool,
    has_older: bool,
) {
    let page_len = page.len();
    page.reverse();
    if older {
        page.append(&mut state.messages);
        state.messages = page;
    } else {
        state.messages = page;
    }
    deduplicate_and_sort_bot_messages(&mut state.messages);
    state.has_older = has_older && page_len >= BOT_HISTORY_PAGE_SIZE as usize;
}

fn insert_bot_message(messages: &mut Vec<bots::BotMessage>, message: bots::BotMessage) {
    messages.push(message);
    deduplicate_and_sort_bot_messages(messages);
}

fn deduplicate_and_sort_bot_messages(messages: &mut Vec<bots::BotMessage>) {
    let mut seen = HashSet::new();
    messages.retain(|message| seen.insert(message.id.clone()));
    messages.sort_by(|left, right| {
        left.created_at
            .cmp(&right.created_at)
            .then_with(|| left.id.cmp(&right.id))
    });
}

fn load_call_history(ctx: &AppContext) {
    let Some(account_id) = ctx.active_session().map(|session| session.account.id) else {
        return;
    };
    let cached = call_history_snapshot(&account_id);
    let context = ctx.clone();
    let expected_account_id = account_id.clone();
    ctx.apply_ui(move |window| {
        let account_matches = context
            .active_session()
            .is_some_and(|session| session.account.id == expected_account_id);
        if account_matches && window.get_screen().as_str() == "calls" {
            window.set_calls_loading(true);
            window.set_calls_error_text(SharedString::default());
            if !cached.is_empty() {
                apply_call_records(&window, &cached);
            } else {
                window.set_calls(ModelRc::new(VecModel::from(Vec::<CallHistoryRow>::new())));
                window.set_calls_has_calls(false);
            }
        }
    });
    let completion_context = ctx.clone();
    let expected_account_id = account_id.clone();
    let spawn_error_context = ctx.clone();
    let spawn_error_account_id = account_id;
    if let Err(error) = ctx.call(
        "friendshub-call-history",
        |client| calls_attachments::list_calls(client, None, CALL_HISTORY_PAGE_SIZE),
        move |window, result| match result {
            Ok(mut calls) => {
                calls.sort_by(|left, right| {
                    right
                        .started_at
                        .cmp(&left.started_at)
                        .then_with(|| right.id.cmp(&left.id))
                });
                if let Ok(mut cache) = call_cache().lock() {
                    cache.insert(expected_account_id.clone(), calls.clone());
                }
                let account_matches = completion_context
                    .active_session()
                    .is_some_and(|session| session.account.id == expected_account_id);
                if account_matches && window.get_screen().as_str() == "calls" {
                    window.set_calls_loading(false);
                    window.set_calls_error_text(SharedString::default());
                    apply_call_records(&window, &calls);
                }
            }
            Err(error) => {
                let account_matches = completion_context
                    .active_session()
                    .is_some_and(|session| session.account.id == expected_account_id);
                if account_matches && window.get_screen().as_str() == "calls" {
                    window.set_calls_loading(false);
                    window.set_calls_error_text(error.user_message().into());
                }
                report_background_error("call history load", error);
            }
        },
    ) {
        ctx.apply_ui(move |window| {
            let account_matches = spawn_error_context
                .active_session()
                .is_some_and(|session| session.account.id == spawn_error_account_id);
            if account_matches && window.get_screen().as_str() == "calls" {
                window.set_calls_loading(false);
                window
                    .set_calls_error_text("Не удалось запустить загрузку истории звонков.".into());
            }
        });
        report_spawn_error("call history load", error);
    }
}

fn apply_call_records(window: &MainWindow, calls: &[calls_attachments::CallRecord]) {
    let rows = calls
        .iter()
        .map(|call| {
            let direction = match call.direction {
                calls_attachments::CallDirection::Incoming => "Входящий",
                calls_attachments::CallDirection::Outgoing => "Исходящий",
            };
            let status = match call.status {
                calls_attachments::CallStatus::Completed => "Завершён",
                calls_attachments::CallStatus::Missed => "Пропущен",
                calls_attachments::CallStatus::Rejected => "Отклонён",
                calls_attachments::CallStatus::Cancelled => "Отменён",
                calls_attachments::CallStatus::Failed => "Ошибка",
            };
            let completed = call.status == calls_attachments::CallStatus::Completed;
            let duration = call
                .ended_at
                .filter(|_| completed)
                .map(|ended_at| format_call_duration(call.started_at, ended_at))
                .unwrap_or_default();
            CallHistoryRow {
                id: call.id.clone().into(),
                peer_account_id: call.peer_account_id.clone().into(),
                peer_name: "Неизвестный абонент".into(),
                peer_initial: "?".into(),
                direction_status: format!("{direction} · {status}").into(),
                started_at: format_call_timestamp(call.started_at).into(),
                duration: duration.into(),
                completed,
            }
        })
        .collect::<Vec<_>>();
    window.set_calls(ModelRc::new(VecModel::from(rows)));
    window.set_calls_has_calls(!calls.is_empty());
}

fn format_call_duration(started_at: i64, ended_at: i64) -> String {
    let seconds = ended_at.saturating_sub(started_at).max(0);
    let hours = seconds / 3600;
    let minutes = (seconds % 3600) / 60;
    let seconds = seconds % 60;
    if hours > 0 {
        format!("{hours}:{minutes:02}:{seconds:02}")
    } else {
        format!("{minutes:02}:{seconds:02}")
    }
}

fn format_call_timestamp(timestamp: i64) -> String {
    let day_index = timestamp.div_euclid(86_400);
    let time_of_day = timestamp.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(day_index);
    let hours = time_of_day / 3600;
    let minutes = (time_of_day % 3600) / 60;
    format!("{day:02}.{month:02}.{year:04} {hours:02}:{minutes:02} UTC")
}

fn start_outgoing_call(
    ctx: &AppContext,
    target: String,
    target_is_account_id: bool,
    with_video: bool,
) {
    let Some(session) = ctx.active_session() else {
        report_call_message(ctx, "Войдите в аккаунт, чтобы начать звонок.", true);
        return;
    };
    let call_id = new_call_id();
    let started_at = unix_seconds();
    let peer_name = if target_is_account_id {
        "Неизвестный абонент".to_owned()
    } else {
        ctx.ui
            .upgrade()
            .map(|window| window.get_conversation_title().to_string())
            .filter(|name| !name.trim().is_empty())
            .unwrap_or_else(|| "Неизвестный абонент".to_owned())
    };

    {
        let Ok(mut active) = active_call_slot().lock() else {
            report_call_message(ctx, "Не удалось прочитать состояние звонка.", true);
            return;
        };
        if active.is_some() {
            report_call_message(ctx, "Уже выполняется другой звонок.", true);
            return;
        }
        *active = Some(ActiveCall {
            core: session.core.clone(),
            account_id: session.account.id.clone(),
            call_id: call_id.clone(),
            peer_account_id: String::new(),
            peer_name: peer_name.clone(),
            with_video,
            started_at,
            direction: calls_attachments::CallDirection::Outgoing,
            phase: ActiveCallPhase::OutgoingStarting,
            remote_sdp: None,
            deferred_event_ids: Vec::new(),
            action_pending: false,
        });
    }

    let local_account_id = session.account.id.clone();
    let call_id_for_call = call_id.clone();
    let call_id_for_completion = call_id.clone();
    let peer_initial = peer_name
        .chars()
        .next()
        .map(|initial| initial.to_uppercase().to_string())
        .unwrap_or_else(|| "?".to_owned());
    let completion_context = ctx.clone();
    let expected_account_id = session.account.id.clone();
    let result = ctx.call(
        "friendshub-call-initiate",
        move |client| {
            let peer_account_id = if target_is_account_id {
                target
            } else {
                let detail = conversations::get(client, target.clone())?;
                if detail.kind != "direct" {
                    return Err(CoreError::InvalidPayload(
                        "calls require a direct conversation".into(),
                    ));
                }
                detail
                    .members
                    .into_iter()
                    .find(|member| member.as_str() != local_account_id.as_str())
                    .ok_or_else(|| {
                        CoreError::InvalidPayload("direct conversation has no other member".into())
                    })?
            };
            let signal =
                calls_attachments::initiate_call(client, &peer_account_id, &call_id_for_call)?;
            Ok((peer_account_id, signal))
        },
        move |window, result| match result {
            Ok((peer_account_id, _signal)) => {
                if let Ok(mut active) = active_call_slot().lock() {
                    if let Some(active) = active
                        .as_mut()
                        .filter(|active| {
                            active.account_id == expected_account_id
                                && active.call_id == call_id_for_completion
                        })
                    {
                        active.peer_account_id = peer_account_id;
                        active.phase = ActiveCallPhase::Outgoing;
                    }
                }
                if is_current_account(&completion_context, &expected_account_id)
                    && active_call_matches(&expected_account_id, &call_id_for_completion)
                {
                    window.set_call_peer_name(SharedString::from(peer_name.clone()));
                    window.set_call_peer_initial(peer_initial.clone().into());
                    window.set_call_with_video(with_video);
                    window.set_call_state("outgoing".into());
                    window.set_call_status(
                        "Сигнал вызова отправлен. Звук и видео недоступны без системного медиаадаптера."
                            .into(),
                    );
                    window.set_call_error(SharedString::default());
                    window.set_call_has_remote_video(false);
                    window.set_call_has_local_video(false);
                    window.set_call_speaker_on(false);
                    window.set_call_camera_off(false);
                    window.set_call_overlay_visible(true);
                }
            }
            Err(error) => {
                clear_active_call(&call_id_for_completion);
                if is_current_account(&completion_context, &expected_account_id)
                    && !account_has_active_call(&expected_account_id)
                {
                    window.set_call_peer_name(peer_name.clone().into());
                    window.set_call_peer_initial(peer_initial.clone().into());
                    window.set_call_with_video(with_video);
                    window.set_call_state("failed".into());
                    window.set_call_overlay_visible(true);
                    window.set_call_error(error.user_message().into());
                    window.set_call_status(SharedString::default());
                    window.set_call_has_remote_video(false);
                    window.set_call_has_local_video(false);
                    window.set_call_speaker_on(false);
                    window.set_call_camera_off(false);
                }
                report_background_error("call signaling", error);
            }
        },
    );
    if let Err(error) = result {
        clear_active_call(&call_id);
        report_spawn_error("call signaling", error);
        report_call_message(ctx, "Не удалось запустить звонок.", true);
    }
}

fn validate_absolute_path(path: &Path) -> io::Result<()> {
    if !path.is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "attachment path must be absolute",
        ));
    }
    if path.to_str().is_none() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "attachment path must be valid UTF-8",
        ));
    }
    Ok(())
}

fn end_active_call(ctx: &AppContext) {
    let Some(session) = ctx.active_session() else {
        report_call_message(ctx, "Войдите в аккаунт, чтобы завершить звонок.", true);
        return;
    };
    let call = match active_call_slot().lock() {
        Ok(mut active) => active.take(),
        Err(_) => {
            report_call_message(ctx, "Не удалось прочитать состояние звонка.", true);
            return;
        }
    };
    let Some(call) = call else {
        ctx.apply_ui(|window| {
            if window.get_call_state().as_str() == "failed" {
                window.set_call_overlay_visible(false);
                window.set_call_state("idle".into());
                window.set_call_error(SharedString::default());
                window.set_call_status(SharedString::default());
            } else {
                window.set_call_error("Нет активного исходящего звонка.".into());
            }
        });
        return;
    };
    if call.account_id != session.account.id
        || !std::sync::Arc::ptr_eq(&call.core, &session.core)
        || call.peer_account_id.is_empty()
    {
        if let Ok(mut active) = active_call_slot().lock() {
            if active.is_none() {
                *active = Some(call);
            }
        }
        report_call_message(
            ctx,
            "Для этого звонка недоступны данные сигнализации.",
            true,
        );
        return;
    }
    if call.phase == ActiveCallPhase::IncomingRinging {
        if let Ok(mut active) = active_call_slot().lock() {
            if active.is_none() {
                *active = Some(call);
            }
        }
        report_call_message(
            ctx,
            "Отклоните входящий звонок или сначала примите его.",
            true,
        );
        return;
    }

    let call_for_worker = call.clone();
    let call_for_completion = call.clone();
    let refresh_ctx = ctx.clone();
    if let Err(error) = ctx.call(
        "friendshub-call-end",
        move |client| {
            let signal_result = calls_attachments::hang_up(
                client,
                &call_for_worker.peer_account_id,
                &call_for_worker.call_id,
                Some("user_hangup"),
            );
            let input = calls_attachments::CallRecordInput {
                peer_account_id: &call_for_worker.peer_account_id,
                direction: call_for_worker.direction,
                // The current runtime has no connection-state event wiring or
                // media adapter, so it can only record this as cancelled.
                status: calls_attachments::CallStatus::Cancelled,
                started_at: call_for_worker.started_at,
                ended_at: Some(unix_seconds()),
            };
            let record_result = calls_attachments::record_call(client, &input);
            signal_result?;
            Ok(record_result)
        },
        move |window, result| {
            match result {
                Ok(record_result) => {
                    window.set_call_overlay_visible(false);
                    window.set_call_state("idle".into());
                    window.set_call_status("Звонок завершён".into());
                    match record_result {
                        Ok(_) => window.set_call_error(SharedString::default()),
                        Err(error) => {
                            let error = CoreCallFailure::from(error);
                            window.set_call_error(error.user_message().into());
                            report_background_error("call history record", error);
                        }
                    }
                }
                Err(error) => {
                    if let Ok(mut active) = active_call_slot().lock() {
                        if active.is_none() {
                            *active = Some(call_for_completion.clone());
                        }
                    }
                    window.set_call_overlay_visible(true);
                    window.set_call_state(
                        if call_for_completion.direction
                            == calls_attachments::CallDirection::Incoming
                        {
                            "active"
                        } else {
                            "outgoing"
                        }
                        .into(),
                    );
                    window.set_call_status("Не удалось завершить звонок.".into());
                    window.set_call_error(error.user_message().into());
                    report_background_error("call ending", error);
                }
            }
            load_call_history(&refresh_ctx);
        },
    ) {
        if let Ok(mut active) = active_call_slot().lock() {
            if active.is_none() {
                *active = Some(call);
            }
        }
        report_spawn_error("call ending", error);
        report_call_message(ctx, "Не удалось запустить завершение звонка.", true);
    }
}

fn to_ui_bot_message(message: bots::BotMessage) -> UiBotMessage {
    let (has_reply_keyboard, reply_keyboard_labels) = if message.is_deleted {
        (false, String::new())
    } else {
        inline_keyboard_labels(message.reply_markup.as_deref())
    };
    UiBotMessage {
        id: message.id.into(),
        text: message.text.into(),
        time_label: message_time_label(message.created_at).into(),
        from_bot: message.direction == bots::BotMessageDirection::FromBot,
        deleted: message.is_deleted,
        has_reply_keyboard,
        reply_keyboard_labels: reply_keyboard_labels.into(),
    }
}

fn inline_keyboard_labels(raw: Option<&str>) -> (bool, String) {
    let Some(raw) = raw else {
        return (false, String::new());
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(raw) else {
        return (false, String::new());
    };
    let Some(rows) = value
        .get("inline_keyboard")
        .and_then(serde_json::Value::as_array)
    else {
        return (false, String::new());
    };
    let mut labels = Vec::new();
    for row in rows {
        let Some(buttons) = row.as_array() else {
            return (false, String::new());
        };
        for button in buttons {
            let Some(label) = button.get("text").and_then(serde_json::Value::as_str) else {
                return (false, String::new());
            };
            labels.push(label.to_owned());
        }
    }
    if labels.is_empty() {
        (false, String::new())
    } else {
        (true, labels.join(" · "))
    }
}

fn message_time_label(timestamp: i64) -> String {
    let now = unix_seconds();
    let age = now.saturating_sub(timestamp);
    if (0..60).contains(&age) {
        return "сейчас".to_owned();
    }
    if (60..3600).contains(&age) {
        return format!("{} мин. назад", age / 60);
    }
    if (3600..86_400).contains(&age) {
        return format!("{} ч. назад", age / 3600);
    }
    let day = timestamp.div_euclid(86_400);
    let current_day = now.div_euclid(86_400);
    let time_of_day = timestamp.rem_euclid(86_400);
    let time = format!("{:02}:{:02}", time_of_day / 3600, (time_of_day % 3600) / 60);
    if day == current_day {
        return format!("Сегодня {time} UTC");
    }
    if day == current_day - 1 {
        return format!("Вчера {time} UTC");
    }
    let (year, month, day) = civil_from_days(day);
    format!("{day:02}.{month:02}.{year:04}")
}

fn civil_from_days(days_since_epoch: i64) -> (i64, i64, i64) {
    let adjusted = days_since_epoch + 719_468;
    let era = adjusted.div_euclid(146_097);
    let day_of_era = adjusted - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let mut year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = month_prime + if month_prime < 10 { 3 } else { -9 };
    year += if month <= 2 { 1 } else { 0 };
    (year, month, day)
}

fn selected_conversation_id(ctx: &AppContext) -> Option<String> {
    ctx.ui
        .upgrade()
        .map(|window| window.get_selected_conversation_id().to_string())
        .filter(|id| !id.trim().is_empty())
}

fn current_bot_id(ctx: &AppContext) -> Option<String> {
    let route = ctx.ui.upgrade()?.get_route().to_string();
    current_bot_id_from_route(&route)
}

fn current_bot_id_from_route(route: &str) -> Option<String> {
    let path = route.split('?').next().unwrap_or(route);
    let remainder = path.strip_prefix("/app/bot/")?;
    let bot_id = remainder.split('/').next().unwrap_or_default();
    (!bot_id.trim().is_empty()).then(|| bot_id.to_owned())
}

fn current_screen(ctx: &AppContext) -> Option<String> {
    ctx.ui
        .upgrade()
        .map(|window| window.get_screen().to_string())
}

fn is_current_account(ctx: &AppContext, account_id: &str) -> bool {
    ctx.active_session()
        .is_some_and(|session| session.account.id == account_id)
}

fn active_call_matches(account_id: &str, call_id: &str) -> bool {
    active_call_slot()
        .lock()
        .ok()
        .and_then(|active| {
            active
                .as_ref()
                .map(|active| active.account_id == account_id && active.call_id == call_id)
        })
        .unwrap_or(false)
}

fn account_has_active_call(account_id: &str) -> bool {
    active_call_slot()
        .lock()
        .ok()
        .and_then(|active| {
            active
                .as_ref()
                .map(|active| active.account_id == account_id)
        })
        .unwrap_or(false)
}

fn report_call_message(ctx: &AppContext, message: &'static str, show_overlay: bool) {
    ctx.apply_ui(move |window| {
        window.set_call_error(message.into());
        if show_overlay {
            if window.get_call_state().as_str() == "idle" {
                window.set_call_state("failed".into());
            }
            window.set_call_overlay_visible(true);
        }
    });
}

fn set_calls_busy(ctx: &AppContext, busy: bool) {
    ctx.apply_ui(move |window| {
        if window.get_screen().as_str() == "calls" {
            window.set_calls_busy(busy);
            if busy {
                window.set_calls_error_text(SharedString::default());
            }
        }
    });
}

fn set_calls_failure(ctx: &AppContext, account_id: &str, message: &'static str) {
    let context = ctx.clone();
    let account_id = account_id.to_owned();
    ctx.apply_ui(move |window| {
        if is_current_account(&context, &account_id) && window.get_screen().as_str() == "calls" {
            window.set_calls_busy(false);
            window.set_calls_loading(false);
            window.set_calls_error_text(message.into());
        }
    });
}

fn clear_call_confirmation(window: &MainWindow) {
    window.set_calls_confirmation_open(false);
    window.set_calls_confirm_clear_all(false);
    window.set_calls_confirm_call_id(SharedString::default());
}

fn report_background_error(operation: &'static str, error: CoreCallFailure) {
    eprintln!(
        "[native media] {operation} failed: {}",
        error.user_message()
    );
}

fn report_spawn_error(operation: &'static str, error: std::io::Error) {
    eprintln!("[native media] could not start {operation}: {error}");
}

fn clear_active_call(call_id: &str) {
    if let Ok(mut active) = active_call_slot().lock() {
        if active
            .as_ref()
            .is_some_and(|active| active.call_id == call_id)
        {
            *active = None;
        }
    }
}

fn new_call_id() -> String {
    // Call IDs are protocol identifiers, not secrets. Generate UUIDv7-shaped
    // values from the current millisecond clock and a process-local counter.
    let milliseconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or_default()
        & 0x0000_FFFF_FFFF_FFFF;
    let sequence = CALL_SEQUENCE.fetch_add(1, Ordering::Relaxed) as u128;
    let value = (milliseconds << 80)
        | (0x7_u128 << 76)
        | ((sequence & 0x0fff) << 64)
        | (0b10_u128 << 62)
        | (sequence & 0x3fff_ffff_ffff_ffff);
    format!(
        "{:08x}-{:04x}-{:04x}-{:04x}-{:012x}",
        (value >> 96) as u32,
        (value >> 80) as u16,
        (value >> 64) as u16,
        (value >> 48) as u16,
        value & 0x0000_ffff_ffff_ffff,
    )
}

fn unix_seconds() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs().min(i64::MAX as u64) as i64)
        .unwrap_or_default()
}
