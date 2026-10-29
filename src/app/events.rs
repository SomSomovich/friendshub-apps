//! Account-scoped bridge from native core events to Slint controllers.

use std::collections::{HashSet, VecDeque};
use std::sync::{Arc, Mutex, OnceLock, mpsc};
use std::thread;

use friendshub_core::events::Event;

use crate::core::CoreClient;

use super::{ActiveSession, AppContext};

const EVENT_CONFIRMATION_PAGE_SIZE: i64 = 200;
const APPLIED_EVENT_CACHE_CAPACITY: usize = 2048;

type DurableEventKey = (String, u64, u64);

#[derive(Default)]
struct DurableEventDedupe {
    in_flight: HashSet<DurableEventKey>,
    applied: HashSet<DurableEventKey>,
    applied_order: VecDeque<DurableEventKey>,
}

fn durable_event_dedupe() -> &'static Mutex<DurableEventDedupe> {
    static DEDUPE: OnceLock<Mutex<DurableEventDedupe>> = OnceLock::new();
    DEDUPE.get_or_init(|| Mutex::new(DurableEventDedupe::default()))
}

/// Replace the active core event handler away from the Slint event loop.
///
/// `clear_event_handler` waits for the old callback worker, so it must never
/// run on the UI thread. A generation prevents a slower older transition from
/// installing its handler after a newer account switch has begun.
pub(super) fn replace_session(
    context: AppContext,
    previous: Option<ActiveSession>,
    next: Option<ActiveSession>,
    generation: u64,
) {
    let result = thread::Builder::new()
        .name("friendshub-event-session".to_owned())
        .spawn(move || {
            if let Some(previous) = previous {
                previous.core.clear_event_handler();
            }

            if !is_current_generation(&context, generation) {
                return;
            }

            if let Some(next) = next {
                install_session_handler(context, next, generation);
            }
        });

    if let Err(error) = result {
        eprintln!(
            "[events] session transition worker failed: kind={:?}",
            error.kind()
        );
    }
}

fn install_session_handler(context: AppContext, session: ActiveSession, generation: u64) {
    let account_id = session.account.id.clone();
    if !is_current_generation(&context, generation) || !is_active_account(&context, &account_id) {
        return;
    }

    let callback_context = context.clone();
    let callback_core = session.core.clone();
    let callback_account_id = account_id.clone();
    let handler = move |event: Event| {
        receive_event(
            &callback_context,
            &callback_account_id,
            generation,
            &callback_core,
            event,
        );
    };

    // Wait on this non-UI worker so a later account transition cannot clear
    // the handler before this asynchronous registration finishes.
    let (completion_tx, completion_rx) = mpsc::sync_channel(1);
    if let Err(error) = session
        .core
        .set_event_handler_async(handler, move |result| {
            let _ = completion_tx.send(result);
        })
    {
        eprintln!("[events] handler start failed: kind={:?}", error.kind());
        return;
    }

    match completion_rx.recv() {
        Ok(Ok(())) => {}
        Ok(Err(error)) => {
            eprintln!("[events] handler activation failed: code={}", error.code());
            return;
        }
        Err(_) => {
            eprintln!("[events] handler activation failed: completion=unavailable");
            return;
        }
    }

    if !is_current_generation(&context, generation)
        && !context
            .active_session()
            .is_some_and(|active| Arc::ptr_eq(&active.core, &session.core))
    {
        session.core.clear_event_handler();
    }
}

/// Core owns the durable queue in its encrypted database. The callback must
/// not copy decrypted event payloads to a second, unencrypted journal. A
/// semantic envelope is acknowledged only after its source envelope is
/// confirmed by a core history reload, its feature controller applies it, or
/// an attachment key is saved in the SQLCipher database.
fn receive_event(
    context: &AppContext,
    account_id: &str,
    generation: u64,
    core: &Arc<CoreClient>,
    event: Event,
) {
    if event.kind == "call_signal_received" {
        schedule_call_signal(context, account_id, generation, core, event);
        return;
    }
    if event.kind == "webrtc_event" {
        schedule_webrtc_event(context, account_id, generation, event);
        return;
    }
    if event.kind == "attachment_key_received" {
        schedule_attachment_key_store(context, account_id, generation, core, event);
        return;
    }
    if matches!(
        event.kind.as_str(),
        "history_chunk_received" | "history_complete_received"
    ) {
        schedule_history_chunk(context, account_id, generation, core, event);
        return;
    }
    if event.kind == "history_request" {
        schedule_history_response(context, account_id, generation, core, event);
        return;
    }

    schedule_event_refresh(context, account_id, generation, core, &event);
}

/// Persist each decrypted history fragment in SQLCipher before acknowledging
/// its durable envelope event. The core validates the own-device identity,
/// assembles items and stores attachment keys atomically.
fn schedule_history_chunk(
    context: &AppContext,
    account_id: &str,
    generation: u64,
    core: &Arc<CoreClient>,
    event: Event,
) {
    if !is_current_handler(context, account_id, generation) {
        return;
    }
    let Some(event_id) = event.id else { return };
    let dedupe_key = (account_id.to_owned(), generation, event_id);
    if !begin_durable_event(&dedupe_key) {
        return;
    }
    let event_kind = event.kind.clone();
    let completion_context = context.clone();
    let completion_account_id = account_id.to_owned();
    let completion_core = core.clone();
    let completion_event_kind = event_kind.clone();
    let completion_key = dedupe_key.clone();
    let result = crate::app::call_client(
        core.clone(),
        context.ui.clone(),
        "friendshub-history-receive",
        move |client| crate::features::history_sync::receive_chunk(client, &event),
        move |window, result| {
            if !is_current_handler(&completion_context, &completion_account_id, generation) {
                finish_durable_event(&completion_key, false);
                return;
            }
            match result {
                Ok(result) => {
                    let status = if result.conversation_complete {
                        format!(
                            "История беседы загружена: {} сообщений.",
                            result.received_messages
                        )
                    } else {
                        format!("Получено сообщений: {}.", result.received_messages)
                    };
                    if window.get_screen() == "settings_security" {
                        window.set_security_history_progress(status.into());
                        window.set_security_history_error(slint::SharedString::default());
                    }
                    if result.item_kind == "message"
                        && result.item.is_some()
                        && window.get_selected_conversation_id().as_str() == result.conversation_id
                        && matches!(
                            window.get_screen().as_str(),
                            "chat_index" | "chat" | "channel"
                        )
                    {
                        super::chat::load(&completion_context, window.get_route().as_str());
                    }
                    acknowledge_confirmed_event(
                        &completion_core,
                        event_id,
                        &completion_event_kind,
                        completion_key,
                    );
                }
                Err(error) => {
                    eprintln!(
                        "[events] history fragment was not committed: code={}",
                        error.code()
                    );
                    finish_durable_event(&completion_key, false);
                }
            }
        },
    );
    if let Err(error) = result {
        eprintln!(
            "[events] history receiver could not start: error_kind={:?}",
            error.kind()
        );
        finish_durable_event(&dedupe_key, false);
    }
}

/// Respond to the core's ephemeral request from another active account device.
fn schedule_history_response(
    context: &AppContext,
    account_id: &str,
    generation: u64,
    core: &Arc<CoreClient>,
    event: Event,
) {
    if !is_current_handler(context, account_id, generation) {
        return;
    }
    let account_id = account_id.to_owned();
    let completion_context = context.clone();
    let payload = event.payload;
    let result = crate::app::call_client(
        core.clone(),
        context.ui.clone(),
        "friendshub-history-respond",
        move |client| crate::features::history_sync::respond_to_request(client, &payload),
        move |window, result| {
            if !is_current_handler(&completion_context, &account_id, generation) {
                return;
            }
            match result {
                Ok(response) => {
                    let complete = response
                        .get("complete")
                        .and_then(serde_json::Value::as_bool)
                        == Some(true);
                    let submitted = response
                        .get("submitted_chunks")
                        .and_then(serde_json::Value::as_u64)
                        .unwrap_or_default();
                    let chunks = response
                        .get("chunk_count")
                        .and_then(serde_json::Value::as_u64)
                        .unwrap_or_default();
                    let text = if complete {
                        format!("История отправлена: {submitted} зашифрованных фрагментов.")
                    } else {
                        format!(
                            "Передача истории прервана: {submitted} из {chunks} фрагментов. Запросите её повторно."
                        )
                    };
                    if window.get_screen() == "settings_security" {
                        window.set_security_history_progress(text.into());
                    }
                }
                Err(error) => {
                    eprintln!("[events] history relay failed: code={}", error.code());
                }
            }
        },
    );
    if let Err(error) = result {
        eprintln!(
            "[events] history response could not start: error_kind={:?}",
            error.kind()
        );
    }
}

/// Acknowledge an incoming key envelope only after the key is durable in the
/// account's encrypted core database. A failed save stays available for replay.
fn schedule_attachment_key_store(
    context: &AppContext,
    account_id: &str,
    generation: u64,
    core: &Arc<CoreClient>,
    event: Event,
) {
    if !is_current_handler(context, account_id, generation) {
        return;
    }
    let Some(event_id) = event.id else {
        return;
    };
    let dedupe_key = (account_id.to_owned(), generation, event_id);
    if !begin_durable_event(&dedupe_key) {
        return;
    }
    let key = match crate::features::calls_attachments::received_attachment_key(&event) {
        Ok(Some(received)) => received.key,
        Ok(None) | Err(_) => {
            finish_durable_event(&dedupe_key, false);
            return;
        }
    };
    let event_kind = event.kind;
    let completion_context = context.clone();
    let completion_account_id = account_id.to_owned();
    let completion_core = core.clone();
    let completion_key = dedupe_key.clone();
    let completion_kind = event_kind.clone();
    let result = crate::app::call_client(
        core.clone(),
        context.ui.clone(),
        "friendshub-attachment-key-store",
        move |client| crate::features::calls_attachments::store_attachment_key(client, &key),
        move |_, result| {
            if !is_current_handler(&completion_context, &completion_account_id, generation) {
                finish_durable_event(&completion_key, false);
                return;
            }
            match result {
                Ok(()) => acknowledge_confirmed_event(
                    &completion_core,
                    event_id,
                    &completion_kind,
                    completion_key,
                ),
                Err(error) => {
                    eprintln!("[events] attachment key save failed: code={}", error.code());
                    finish_durable_event(&completion_key, false);
                }
            }
        },
    );
    if let Err(error) = result {
        eprintln!(
            "[events] attachment key save could not start: error_kind={:?}",
            error.kind()
        );
        finish_durable_event(&dedupe_key, false);
    }
}

fn schedule_call_signal(
    context: &AppContext,
    account_id: &str,
    generation: u64,
    core: &Arc<CoreClient>,
    event: Event,
) {
    let ui_context = context.clone();
    let account_id = account_id.to_owned();
    let core = core.clone();
    context.apply_ui(move |_| {
        if !is_current_handler(&ui_context, &account_id, generation) {
            return;
        }

        let disposition = super::media::receive_core_event(&ui_context, &account_id, &event);
        if matches!(disposition, super::media::CoreEventDisposition::Acknowledge) {
            if let Some(event_id) = event.id {
                acknowledge_event(&core, event_id, &event.kind);
            }
        }
        // Deferred events are acknowledged by media only after its async core
        // action succeeds. Unapplied events remain in core for redelivery.
    });
}

/// Native WebRTC events are transient media state, so process them on Slint's
/// thread without acknowledging them as durable application events.
fn schedule_webrtc_event(context: &AppContext, account_id: &str, generation: u64, event: Event) {
    let ui_context = context.clone();
    let account_id = account_id.to_owned();
    context.apply_ui(move |_| {
        if is_current_handler(&ui_context, &account_id, generation) {
            super::media::receive_webrtc_event(&ui_context, &account_id, &event);
        }
    });
}

fn acknowledge_event(core: &Arc<CoreClient>, event_id: u64, event_kind: &str) {
    let event_kind = event_kind.to_owned();
    let completion_kind = event_kind.clone();
    if let Err(error) = core.ack_event_async(event_id, move |result| {
        if let Err(error) = result {
            eprintln!(
                "[events] durable event ack failed: kind={} code={}",
                completion_kind,
                error.code()
            );
        }
    }) {
        eprintln!(
            "[events] durable event ack could not start: kind={} error_kind={:?}",
            event_kind,
            error.kind()
        );
    }
}

fn schedule_event_refresh(
    context: &AppContext,
    account_id: &str,
    generation: u64,
    core: &Arc<CoreClient>,
    event: &Event,
) {
    let confirmation = durable_envelope_confirmation(event);
    let should_refresh =
        is_route_refresh_event(event) && (event.id.is_none() || confirmation.is_some());
    if !should_refresh && confirmation.is_none() {
        return;
    }

    let ui_context = context.clone();
    let account_id = account_id.to_owned();
    let event = event.clone();
    let core = core.clone();
    context.apply_ui(move |window| {
        if !is_current_handler(&ui_context, &account_id, generation) {
            return;
        }

        let confirmation_key = confirmation
            .as_ref()
            .map(|(event_id, _, _)| (account_id.clone(), generation, *event_id));
        if let Some(key) = &confirmation_key
            && !begin_durable_event(key)
        {
            return;
        }

        let route = window.get_route().to_string();
        let screen = window.get_screen().to_string();
        if should_refresh && event_matches_visible_screen(&event, &screen, &route) {
            refresh_screen(&ui_context, &route, &screen);
        }

        if let (Some((event_id, conversation_id, envelope_id)), Some(dedupe_key)) =
            (confirmation, confirmation_key)
        {
            confirm_durable_envelope(
                ui_context,
                account_id,
                generation,
                core,
                dedupe_key,
                event_id,
                event.kind,
                conversation_id,
                envelope_id,
            );
        }
    });
}

fn durable_envelope_confirmation(event: &Event) -> Option<(u64, String, String)> {
    let event_id = event.id?;
    if !is_confirmable_semantic_event(&event.kind) {
        return None;
    }
    let conversation_id = event
        .payload
        .get("conversation_id")
        .and_then(serde_json::Value::as_str)?;
    let envelope_id = event
        .payload
        .get("envelope_id")
        .and_then(serde_json::Value::as_str)?;
    if conversation_id.is_empty() || envelope_id.is_empty() {
        return None;
    }
    Some((event_id, conversation_id.to_owned(), envelope_id.to_owned()))
}

fn is_confirmable_semantic_event(kind: &str) -> bool {
    // Attachment keys use a dedicated save-before-ack path. History replay
    // events still require their own begin/commit protocol.
    matches!(
        kind,
        "message_received"
            | "poll_received"
            | "message_edited"
            | "message_deleted"
            | "message_reaction"
    )
}

fn begin_durable_event(key: &DurableEventKey) -> bool {
    let Ok(mut dedupe) = durable_event_dedupe().lock() else {
        return false;
    };
    if dedupe.applied.contains(key) {
        return false;
    }
    dedupe.in_flight.insert(key.clone())
}

fn finish_durable_event(key: &DurableEventKey, applied: bool) {
    let Ok(mut dedupe) = durable_event_dedupe().lock() else {
        return;
    };
    dedupe.in_flight.remove(key);
    if !applied || !dedupe.applied.insert(key.clone()) {
        return;
    }
    dedupe.applied_order.push_back(key.clone());
    while dedupe.applied_order.len() > APPLIED_EVENT_CACHE_CAPACITY {
        if let Some(expired) = dedupe.applied_order.pop_front() {
            dedupe.applied.remove(&expired);
        }
    }
}

fn confirm_durable_envelope(
    context: AppContext,
    account_id: String,
    generation: u64,
    core: Arc<CoreClient>,
    dedupe_key: DurableEventKey,
    event_id: u64,
    event_kind: String,
    conversation_id: String,
    envelope_id: String,
) {
    let completion_context = context.clone();
    let completion_account_id = account_id.clone();
    let completion_key = dedupe_key.clone();
    let completion_core = core.clone();
    let completion_kind = event_kind.clone();
    let result = context.call(
        "friendshub-event-envelope-confirmation",
        move |client| {
            let envelopes = crate::features::conversations::envelopes(
                client,
                conversation_id,
                crate::features::conversations::HistoryCursor {
                    before: None,
                    before_id: None,
                    limit: Some(EVENT_CONFIRMATION_PAGE_SIZE),
                },
            )?;
            Ok(envelopes
                .iter()
                .any(|envelope| envelope.envelope_id == envelope_id))
        },
        move |_, result| {
            if !is_current_handler(&completion_context, &completion_account_id, generation) {
                finish_durable_event(&completion_key, false);
                return;
            }

            match result {
                Ok(true) => acknowledge_confirmed_event(
                    &completion_core,
                    event_id,
                    &completion_kind,
                    completion_key,
                ),
                Ok(false) => finish_durable_event(&completion_key, false),
                Err(error) => {
                    eprintln!(
                        "[events] durable event reload failed: kind={} code={}",
                        completion_kind,
                        error.code()
                    );
                    finish_durable_event(&completion_key, false);
                }
            }
        },
    );
    if let Err(error) = result {
        eprintln!(
            "[events] durable event reload could not start: kind={} error_kind={:?}",
            event_kind,
            error.kind()
        );
        finish_durable_event(&dedupe_key, false);
    }
}

fn acknowledge_confirmed_event(
    core: &Arc<CoreClient>,
    event_id: u64,
    event_kind: &str,
    dedupe_key: DurableEventKey,
) {
    let event_kind = event_kind.to_owned();
    let completion_kind = event_kind.clone();
    let completion_key = dedupe_key.clone();
    if let Err(error) = core.ack_event_async(event_id, move |result| match result {
        Ok(()) => finish_durable_event(&completion_key, true),
        Err(error) => {
            eprintln!(
                "[events] durable event ack failed: kind={} code={}",
                completion_kind,
                error.code()
            );
            finish_durable_event(&completion_key, false);
        }
    }) {
        eprintln!(
            "[events] durable event ack could not start: kind={} error_kind={:?}",
            event_kind,
            error.kind()
        );
        finish_durable_event(&dedupe_key, false);
    }
}

fn refresh_screen(context: &AppContext, route: &str, screen: &str) {
    match screen {
        "chat_index" | "chat" => super::chat::load(context, route),
        "channel"
        | "conversation_settings"
        | "discover"
        | "dating"
        | "create_group"
        | "create_channel"
        | "invite" => super::community::load(context, route),
        "profile"
        | "user_profile"
        | "settings_index"
        | "settings_account"
        | "settings_appearance"
        | "settings_privacy"
        | "settings_security"
        | "settings_twofactor"
        | "settings_notifications"
        | "settings_accounts"
        | "settings_bots"
        | "settings_about" => super::settings::load(context, route),
        "calls" | "bot" => super::media::load(context, screen),
        _ => {}
    }
}

fn is_route_refresh_event(event: &Event) -> bool {
    matches!(
        event.kind.as_str(),
        "message_received"
            | "poll_received"
            | "message_edited"
            | "message_deleted"
            | "message_reaction"
            | "conversation_updated"
            | "folders_updated"
            | "poll_updated"
            | "poll_deleted"
            | "bot_message"
            | "bot_message_edited"
            | "bot_message_deleted"
            | "bot_callback_answered"
            | "channel_post"
    )
}

fn event_matches_visible_screen(event: &Event, screen: &str, route: &str) -> bool {
    match event.kind.as_str() {
        "message_received" | "message_edited" | "message_deleted" | "message_reaction" => {
            matches!(screen, "chat_index" | "chat")
        }
        "poll_received" | "poll_updated" | "poll_deleted" => {
            matches!(screen, "chat" | "channel")
        }
        "conversation_updated" => matches!(
            screen,
            "chat_index" | "chat" | "channel" | "conversation_settings" | "discover"
        ),
        "folders_updated" => matches!(screen, "chat_index" | "chat"),
        "bot_message" | "bot_message_edited" | "bot_message_deleted" | "bot_callback_answered" => {
            screen == "bot"
        }
        "channel_post" => {
            screen == "channel"
                && event
                    .payload
                    .get("channel_id")
                    .and_then(serde_json::Value::as_str)
                    .is_some_and(|channel_id| {
                        super::conversation_from_route(route)
                            .is_some_and(|(id, _)| id == channel_id)
                    })
        }
        _ => false,
    }
}

fn is_active_account(context: &AppContext, account_id: &str) -> bool {
    context
        .active_session()
        .is_some_and(|session| session.account.id == account_id)
}

fn is_current_handler(context: &AppContext, account_id: &str, generation: u64) -> bool {
    is_current_generation(context, generation) && is_active_account(context, account_id)
}

fn is_current_generation(context: &AppContext, generation: u64) -> bool {
    use std::sync::atomic::Ordering;

    context.event_generation.load(Ordering::SeqCst) == generation
}
