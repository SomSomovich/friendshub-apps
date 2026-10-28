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
/// confirmed by a core history reload or its feature controller applies it.
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

    schedule_event_refresh(context, account_id, generation, core, &event);
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
    // Attachment keys are decoded by the feature layer, but there is no
    // account-scoped encrypted app store or download flow that consumes them.
    // History replay events also require the begin/commit protocol. Both stay
    // pending until those application contracts exist.
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
